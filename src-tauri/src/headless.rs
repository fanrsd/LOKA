//! `loka --headless`: a Linux computer without a desktop (a server on its text console) that another
//! device's mouse and keyboard can drive. No window: pairing requests are answered in the terminal.

use crate::{
    kvm,
    network::{
        discovery::{self, DeviceInfo, DeviceType, Peer},
        secure::{Identity, Tls},
        telemetry, transfer,
    },
    platform::{self, Edge, Injected, Rect, SpatialPlatformEngine},
    settings::{PairedDevice, Settings},
};
use std::{
    collections::HashMap,
    io::BufRead,
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, watch};

/// Same folder as the desktop app (Tauri's app_data_dir), so both share identity and pairings.
fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("dev.loka.app")
}

struct Sink {
    engine: Arc<dyn SpatialPlatformEngine>,
    alias: String,
}

impl kvm::Sink for Sink {
    fn screens(&self) -> Vec<Rect> {
        vec![platform::HEADLESS]
    }
    fn inject(&mut self, e: Injected) {
        self.engine.inject(e);
    }
}

/// Who drives this server right now, for its report (`troubleshoot` on the driver checks it).
static DRIVER: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

impl Drop for Sink {
    fn drop(&mut self) {
        println!("{} let go.", self.alias);
        *DRIVER.lock().unwrap() = None;
    }
}

pub fn run() {
    // no desktop to talk to: the engine's input lands on one console-sized screen (HEADLESS)
    std::env::remove_var("DISPLAY");
    std::env::remove_var("WAYLAND_DISPLAY");
    tauri::async_runtime::block_on(serve());
}

async fn serve() {
    let data = data_dir();
    let _ = std::fs::create_dir_all(&data);
    crate::journal::init(&data);
    journal!("LOKA {} starting headless", env!("CARGO_PKG_VERSION"));
    let settings = Arc::new(Mutex::new(Settings::load(&data)));
    let tls = Arc::new(Tls::new(&Identity::load_or_create(&data)));
    let me = DeviceInfo {
        alias: crate::alias(),
        device_type: DeviceType::Desktop,
        os: std::env::consts::OS.into(),
        fingerprint: tls.me.clone(),
        aspect_ratio: (platform::HEADLESS.w / platform::HEADLESS.h) as f32,
        telemetry_port: telemetry::PORT,
        transfer_port: transfer::PORT,
        kvm_port: kvm::PORT,
        proto: kvm::PROTOCOL,
        mac: mac_address::get_mac_address().ok().flatten().map(|m| m.to_string()),
        controllable: true, // corrected by `watch_controllable` right away
        text: true,
    };
    println!("LOKA {} without a desktop, as \"{}\". Ctrl+C ends it.", env!("CARGO_PKG_VERSION"), me.alias);
    println!("Pair from LOKA on another computer: pick \"{}\" there, then answer here.", me.alias);
    let engine: Arc<dyn SpatialPlatformEngine> = platform::engine().into();
    if !engine.can_inject() {
        println!("LOKA can't type on this computer yet: run `sudo loka --allow-input` (or run this as root).");
    }

    let peers = Arc::new(Mutex::new(HashMap::<String, Peer>::new()));
    let (me_tx, me_rx) = watch::channel(me);
    tauri::async_runtime::spawn(crate::watch_controllable(engine.clone(), me_tx, || {}));
    let (p, st) = (peers.clone(), settings.clone());
    tauri::async_runtime::spawn(async move {
        let hosts = move || st.lock().unwrap().hosts.clone();
        if let Err(e) = discovery::run(me_rx, hosts, move |peer| drop(p.lock().unwrap().insert(peer.info.fingerprint.clone(), peer))).await {
            journal!("discovery stopped: {e}");
        }
    });

    // one reader for the terminal: a prompt that timed out must not eat the answer to the next one
    let (line_tx, line_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    let lines = Arc::new(tokio::sync::Mutex::new(line_rx));

    let (st, e) = (settings.clone(), engine.clone());
    let allow = move |fp: &str| match (st.lock().unwrap().is_paired(fp), e.can_inject()) {
        (false, _) => Err(kvm::Refusal::NotPaired),
        (_, false) => Err(kvm::Refusal::NoInput),
        _ => Ok(()),
    };
    let (st, p) = (settings.clone(), peers.clone());
    let ask = move |_: IpAddr, fp: String, code: String| {
        let (settings, peers, lines) = (st.clone(), p.clone(), lines.clone());
        async move {
            let Some(from) = peers.lock().unwrap().get(&fp).cloned() else { return kvm::PairAnswer::Unknown };
            let mut lines = lines.lock().await;
            while lines.try_recv().is_ok() {} // typed before this question
            println!("\n{} wants to pair. Its screen shows {code}. Same code? Pair [y/N]:", from.info.alias);
            let answer = match tokio::time::timeout(kvm::PAIR_TIMEOUT, lines.recv()).await {
                Ok(Some(l)) if matches!(l.trim(), "y" | "Y" | "yes") => kvm::PairAnswer::Accepted,
                Ok(Some(_)) => kvm::PairAnswer::Declined,
                Ok(None) => {
                    println!("No terminal to answer in: run `loka --headless` in one to pair.");
                    kvm::PairAnswer::Declined
                }
                Err(_) => kvm::PairAnswer::NoAnswer,
            };
            if answer == kvm::PairAnswer::Accepted {
                settings.lock().unwrap().pair(PairedDevice { fingerprint: fp, alias: from.info.alias.clone() });
                println!("Paired with {}.", from.info.alias);
            } else {
                println!("Not paired.");
            }
            answer
        }
    };
    let make = move |fp: &str, _: Edge| {
        let alias = peers.lock().unwrap().get(fp).map_or_else(|| "The other device".into(), |p| p.info.alias.clone());
        println!("{alias} is using this computer.");
        *DRIVER.lock().unwrap() = Some(alias.clone());
        Some(Sink { engine: engine.clone(), alias })
    };
    // shortcut: no services (terminal, files, sound) and no file transfers without a desktop; add them if servers need them.
    // Only its log goes to a paired device, for that device's diagnostics.
    // Never drives, so a placement needs nothing here; an unpair is forgotten on this side too.
    let st = settings.clone();
    let on_place = move |fp: &str, l: kvm::Layout| {
        if l == kvm::Layout::Unpair {
            st.lock().unwrap().unpair(fp);
        }
        true
    };
    let st = settings.clone();
    let allow_service = move |s: kvm::Service, fp: &str| s == kvm::Service::Diagnostics && st.lock().unwrap().is_paired(fp);
    let on_service = |_: kvm::Service, _: String, mut stream: crate::network::secure::Stream| {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let driven = DRIVER.lock().unwrap().as_ref().map_or("no".into(), |a| format!("by {a}"));
            let text = format!("LOKA {} headless · {}\nBeing driven: {driven}\nLog (UTC):\n{}\n", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH, crate::journal::tail(300));
            if stream.write_all(text.as_bytes()).await.is_ok() {
                let _ = stream.shutdown().await;
            }
        });
    };
    if let Err(e) = kvm::serve(tls, kvm::PORT, allow, ask, allow_service, on_service, on_place, make).await {
        journal!("LOKA stopped: {e}");
    }
}
