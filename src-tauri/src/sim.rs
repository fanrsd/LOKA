//! `--sim-mobile`: a phone window on the same host, reached through the real UDP/TCP stack on loopback.

use crate::{
    ask_user, kvm,
    network::{
        discovery::{DeviceInfo, DeviceType, Peer, Role},
        telemetry, transfer,
    },
    platform::{Edge, Injected, Portal, Rect},
    AppState,
};
use std::{
    net::{IpAddr, Ipv4Addr},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, State, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::watch;

pub const LABEL: &str = "mobile-view";
const FINGERPRINT: &str = "sim-mobile";
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
    fn area(&self) -> Rect {
        SCREEN
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

    let peer = Peer {
        info: DeviceInfo {
            alias: "Simulated phone".into(),
            device_type: DeviceType::Mobile,
            os: "sim".into(),
            fingerprint: FINGERPRINT.into(),
            aspect_ratio: 300.0 / 620.0,
            telemetry_port: TELEMETRY_PORT,
            transfer_port: TRANSFER_PORT,
            kvm_port: KVM_PORT,
            role: Some(Role::Client),
        },
        addr: Ipv4Addr::LOCALHOST.into(),
        seen: Instant::now(),
    };
    let portal = state.lock().unwrap().portal;
    if let Some(p) = portal {
        place(app, &p)?;
    }
    // the simulated device "heartbeats" like a real one so it never goes stale
    let (h, st) = (app.clone(), state.clone());
    tauri::async_runtime::spawn(async move {
        let mut beat = tokio::time::interval(Duration::from_secs(5));
        loop {
            beat.tick().await;
            let fresh = Peer { seen: Instant::now(), ..peer.clone() };
            if st.lock().unwrap().peers.insert(FINGERPRINT.into(), fresh.clone()).is_none() {
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

    let dir = app.path().app_cache_dir()?.join("sim-received");
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        // the simulated phone has no clipboard: copied pictures/files from the host just land in a folder
        let r = transfer::serve(TRANSFER_PORT, dir.clone(), dir.join("clipboard"), |ip| ip.is_loopback(), move |e| {
            let _ = h.emit_to(LABEL, "transfer", e);
        })
        .await;
        if let Err(e) = r {
            eprintln!("sim transfer server stopped: {e}");
        }
    });

    // Pairing prompts appear in the phone window; the pairing itself is stored by this host's Main side.
    let (h, st) = (app.clone(), state.clone());
    let allow = move |ip: IpAddr| match ip.is_loopback() && st.lock().unwrap().settings.is_paired(FINGERPRINT) {
        true => Ok(()),
        false => Err(kvm::Refusal::NotPaired),
    };
    let (a, st) = (app.clone(), state.clone());
    let ask = move |ip: IpAddr| {
        let (app, state) = (a.clone(), st.clone());
        async move {
            if !ip.is_loopback() {
                return false;
            }
            let host = Peer { info: app.state::<watch::Sender<DeviceInfo>>().borrow().clone(), addr: ip, seen: Instant::now() };
            ask_user(&app, &state, LABEL, &host).await
        }
    };
    tauri::async_runtime::spawn(async move {
        if let Err(e) = kvm::serve(KVM_PORT, allow, ask, move |_, _| Some(WindowSink(h.clone()))).await {
            eprintln!("sim kvm receiver stopped: {e}");
        }
    });
    Ok(())
}

/// Stand-in for the desk: the opposite side of the portal's monitor, aligned with the portal segment,
/// so dragging toward the portal never crosses the simulated phone.
pub fn place(app: &AppHandle, portal: &Portal) -> tauri::Result<()> {
    let Some(w) = app.get_webview_window(LABEL) else { return Ok(()) };
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
