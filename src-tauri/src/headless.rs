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

impl Drop for Sink {
    fn drop(&mut self) {
        println!("{} let go.", self.alias);
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
    };
    println!("LOKA {} without a desktop, as \"{}\". Ctrl+C ends it.", env!("CARGO_PKG_VERSION"), me.alias);
    println!("Pair from LOKA on another computer: pick \"{}\" there, then answer here.", me.alias);
    let engine: Arc<dyn SpatialPlatformEngine> = platform::engine().into();
    if !engine.can_inject() {
        println!("LOKA can't type on this computer yet: run `sudo loka --allow-input` (or run this as root).");
    }

    let peers = Arc::new(Mutex::new(HashMap::<String, Peer>::new()));
    let (_me, me_rx) = watch::channel(me);
    let (p, st) = (peers.clone(), settings.clone());
    tauri::async_runtime::spawn(async move {
        let hosts = move || st.lock().unwrap().hosts.clone();
        if let Err(e) = discovery::run(me_rx, hosts, move |peer| drop(p.lock().unwrap().insert(peer.info.fingerprint.clone(), peer))).await {
            eprintln!("discovery stopped: {e}");
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
        Some(Sink { engine: engine.clone(), alias })
    };
    // shortcut: no services (terminal, files, sound) and no file transfers without a desktop; add them if servers need them.
    // Never drives, so a layout change from a desktop needs nothing here.
    if let Err(e) = kvm::serve(tls, kvm::PORT, allow, ask, |_, _| false, |_, _, _| {}, |_, _| true, make).await {
        eprintln!("LOKA stopped: {e}");
    }
}
