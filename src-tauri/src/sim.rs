//! `--sim-mobile`: a phone window on the same host, reached through the real UDP/TLS stack on loopback.
//! It is a device of its own: its own certificate (kept in the cache dir), so it pairs like any other.

use crate::{
    ask_user, kvm,
    network::{
        discovery::{DeviceInfo, DeviceType, Peer},
        secure::{Identity, Tls},
        telemetry, transfer,
    },
    platform::{Edge, Injected, Portal, Rect},
    AppState,
};
use std::{
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, State, WebviewUrl, WebviewWindowBuilder};

pub const LABEL: &str = "mobile-view";
// offset so the simulated phone never collides with this host's own listeners
const TELEMETRY_PORT: u16 = telemetry::PORT + 10;
const TRANSFER_PORT: u16 = transfer::PORT + 10;
const KVM_PORT: u16 = kvm::PORT + 10;
/// Virtual screen of the simulated device, in the phone window's CSS px.
const SCREEN: Rect = Rect { x: 0.0, y: 0.0, w: 300.0, h: 620.0 };

/// Received input moves a virtual cursor in the phone window instead of this machine's real one
/// (injecting here would fight the capture that produced it).
struct WindowSink(AppHandle);

impl kvm::Sink for WindowSink {
    fn screens(&self) -> Vec<Rect> {
        vec![SCREEN]
    }
    fn inject(&mut self, e: Injected) {
        let _ = self.0.emit_to(LABEL, "kvm-input", e);
    }
}

// async: building a window inside a sync command deadlocks WebView2 on Windows (blank, unclosable window)
#[tauri::command]
pub async fn start_sim(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    start(&app, &state).map_err(|e| e.to_string())
}

pub fn start(app: &AppHandle, state: &AppState) -> tauri::Result<()> {
    if app.get_webview_window(LABEL).is_some() {
        return Ok(());
    }
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::default())
        .title("LOKA - simulated phone")
        .inner_size(SCREEN.w, SCREEN.h)
        .resizable(false)
        .always_on_top(true)
        .build()?;

    let tls = Arc::new(Tls::new(&Identity::load_or_create(&app.path().app_cache_dir()?.join("sim"))));
    let me = tls.me.clone();
    let host = app.state::<Arc<Tls>>().me.clone();
    let peer = Peer {
        info: DeviceInfo {
            alias: "Simulated phone".into(),
            device_type: DeviceType::Mobile,
            os: "sim".into(),
            fingerprint: me.clone(),
            aspect_ratio: 300.0 / 620.0,
            telemetry_port: TELEMETRY_PORT,
            transfer_port: TRANSFER_PORT,
            kvm_port: KVM_PORT,
            mac: None,
        },
        addr: Ipv4Addr::LOCALHOST.into(),
        seen: Instant::now(),
    };
    let portal = state.lock().unwrap().settings.portals.get(&me).copied();
    if let Some(p) = portal {
        place(app, &p)?;
    }
    // the simulated device "heartbeats" like a real one so it never goes stale
    let (h, st, fp) = (app.clone(), state.clone(), me.clone());
    tauri::async_runtime::spawn(async move {
        let mut beat = tokio::time::interval(Duration::from_secs(5));
        loop {
            beat.tick().await;
            let fresh = Peer { seen: Instant::now(), ..peer.clone() };
            if st.lock().unwrap().peers.insert(fp.clone(), fresh.clone()).is_none() {
                let _ = h.emit("peer", fresh);
            }
        }
    });

    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let r = telemetry::listen(TELEMETRY_PORT, |p| {
            let _ = h.emit_to(LABEL, "spatial-remote", p);
        })
        .await;
        if let Err(e) = r {
            eprintln!("sim telemetry stopped: {e}");
        }
    });

    // only this host may reach the simulated phone, and only once paired with it
    let paired = {
        let (st, me, host) = (state.clone(), me.clone(), host.clone());
        move |fp: &str| fp == host && st.lock().unwrap().settings.is_paired(&me)
    };
    let dir = app.path().app_cache_dir()?.join("sim-received");
    let (h, t, allow) = (app.clone(), tls.clone(), paired.clone());
    tauri::async_runtime::spawn(async move {
        // the simulated phone has no clipboard: copied pictures/files from the host just land in a folder
        let (clip, drops) = (dir.join("clipboard"), dir.clone());
        let r = transfer::serve(t, TRANSFER_PORT, move || drops.clone(), clip, allow, move |e| {
            let _ = h.emit_to(LABEL, "transfer", e);
        })
        .await;
        if let Err(e) = r {
            eprintln!("sim transfer server stopped: {e}");
        }
    });

    // Pairing prompts appear in the phone window; the pairing itself is stored by this host's side.
    let allow = move |fp: &str| if paired(fp) { Ok(()) } else { Err(kvm::Refusal::NotPaired) };
    let (a, st) = (app.clone(), state.clone());
    let ask = move |ip: std::net::IpAddr, fp: String| {
        let (app, state, host, me) = (a.clone(), st.clone(), host.clone(), me.clone());
        async move {
            if fp != host {
                return false;
            }
            let info = app.state::<tokio::sync::watch::Sender<DeviceInfo>>().borrow().clone();
            ask_user(&app, &state, LABEL, &Peer { info, addr: ip, seen: Instant::now() }, &me).await
        }
    };
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let sink = move |_: &str, _| Some(WindowSink(h.clone()));
        if let Err(e) = kvm::serve(tls, KVM_PORT, allow, ask, |_, _| false, |_, _, _| {}, sink).await {
            eprintln!("sim kvm receiver stopped: {e}");
        }
    });
    Ok(())
}

/// Stand-in for the desk: the opposite side of the portal's monitor, aligned with the portal segment,
/// so dragging toward the portal never crosses the simulated phone.
pub fn place(app: &AppHandle, portal: &Portal) -> tauri::Result<()> {
    let Some(w) = app.get_webview_window(LABEL) else { return Ok(()) };
    let _ = w.emit_to(LABEL, "portal", portal.edge);
    let s = w.outer_size()?;
    let (sw, sh) = (s.width as f64, s.height as f64);
    let (m, b) = (portal.monitor, portal.hit_box());
    let margin = 40.0;
    let (x, y) = match portal.edge {
        Edge::Right => (m.x + margin, b.y + b.h / 2.0 - sh / 2.0),
        Edge::Left => (m.x + m.w - sw - margin, b.y + b.h / 2.0 - sh / 2.0),
        Edge::Top => (b.x + b.w / 2.0 - sw / 2.0, m.y + m.h - sh - margin),
        Edge::Bottom => (b.x + b.w / 2.0 - sw / 2.0, m.y + margin),
    };
    let x = x.clamp(m.x, (m.x + m.w - sw).max(m.x));
    let y = y.clamp(m.y, (m.y + m.h - sh).max(m.y));
    w.set_position(PhysicalPosition::new(x as i32, y as i32))
}
