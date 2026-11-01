// the phone build has no capture, overlays or clipboard pictures yet: desktop-only paths stay unused there
#![cfg_attr(mobile, allow(dead_code, unused))]
#[macro_use]
mod journal;
#[cfg(desktop)]
mod audio;
mod carry;
#[cfg(desktop)]
mod clip;
mod kvm;
mod network;
#[cfg(target_os = "linux")]
mod headless;
mod platform;
mod notify;
mod settings;
#[cfg(desktop)]
mod share;
#[cfg(desktop)]
mod shell;
#[cfg(desktop)]
mod sim;
#[cfg(desktop)]
mod update;

use network::{
    discovery::{self, DeviceInfo, DeviceType, Peer},
    secure::{self, Identity, Tls},
    telemetry::{self, PacketType, PayloadKind, SpatialPacket},
    transfer,
};
use platform::{Edge, Injected, Portal, Rect, SpatialPlatformEngine};
use settings::{CursorPrefs, PairedDevice, Settings};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{oneshot, watch};

/// While another device drives this one: the band on the edge leading back to it.
const BAND: &str = "edge-overlay";
const MAIN_WINDOW: &str = "main";

/// Telemetry, transfer and kvm ports this device listens on. The Android emulator sits behind NAT on the
/// same computer as a desktop LOKA, so it takes its own, forwarded with `adb emu redir add udp:N:N`.
static PORTS: std::sync::LazyLock<(u16, u16, u16)> = std::sync::LazyLock::new(|| {
    #[cfg(target_os = "android")]
    if std::process::Command::new("getprop").arg("ro.hardware").output().is_ok_and(|o| matches!(o.stdout.trim_ascii(), b"ranchu" | b"goldfish")) {
        return (telemetry::PORT + 20, transfer::PORT + 20, kvm::PORT + 20);
    }
    (telemetry::PORT, transfer::PORT, kvm::PORT)
});

/// Overlay window of the portal leading to device `fp`.
fn portal_label(fp: &str) -> String {
    format!("portal-{}", &fp[..fp.len().min(24)])
}

struct Shared {
    settings: Settings,
    peers: HashMap<String, Peer>,
    last_y: f32,
    /// while another device drives this one: its fingerprint and our band on the edge leading back to it
    session: Option<(String, Portal)>,
    /// files of an OS drag hovering one of our overlays right now
    hover: Option<Vec<PathBuf>>,
    /// the next Drop on an overlay is our own hand-off of a drag to the other device, not a transfer
    handoff: bool,
    /// file drags carried across the edge, waiting for their files or the held cursor
    carry: carry::Carry,
    /// overlay the carried drag starts from (the one under the cursor)
    carry_window: String,
    /// pairing prompts waiting for the user's answer
    pending: HashMap<u64, oneshot::Sender<bool>>,
    next_request: u64,
    /// files that arrived in the last burst, for Ctrl+Alt+V
    recent: Vec<PathBuf>,
    /// files handed to LOKA by Send to / Open With, waiting for the user to pick a device
    outbox: Vec<PathBuf>,
}

type AppState = Arc<Mutex<Shared>>;

/// This computer's mouse & keyboard are on another device right now.
static DRIVING: AtomicBool = AtomicBool::new(false);

/// A stream to paired device `fingerprint`'s `service` (it must share it). Err = user-readable message.
pub(crate) async fn open_service(app: &AppHandle, fingerprint: &str, service: kvm::Service) -> Result<secure::Stream, String> {
    let to = app.state::<AppState>().lock().unwrap().addr(fingerprint, |i| i.kvm_port).ok_or("that device isn't paired or isn't on the network")?;
    let tls = app.state::<Arc<Tls>>().inner().clone();
    kvm::open(&tls, to, fingerprint, service).await.map_err(|e| explain(app, fingerprint, &e))
}

/// `e` from talking to device `fp`, as a message the user can act on (`kvm::explain`).
fn explain(app: &AppHandle, fp: &str, e: &std::io::Error) -> String {
    let (alias, os, heard) = app.state::<AppState>().lock().unwrap().who(fp);
    kvm::explain(e, &alias, &os, &kvm::hint(std::env::consts::OS, &os, &alias, heard))
}

/// Keeps `controllable` in what discovery announces current: an Android's injector starts and dies with
/// Wireless debugging, a Linux gets input access while LOKA runs. `on_change` runs when it flips.
pub(crate) async fn watch_controllable(engine: Arc<dyn SpatialPlatformEngine>, me: watch::Sender<DeviceInfo>, on_change: impl Fn() + Send) {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    loop {
        tick.tick().await;
        let e = engine.clone();
        let now = tokio::task::spawn_blocking(move || e.can_inject()).await.unwrap_or(false);
        if me.send_if_modified(|i| std::mem::replace(&mut i.controllable, now) != now) {
            journal!("controllable: {now}");
            on_change();
        }
    }
}

#[derive(serde::Serialize)]
struct Probe {
    ok: bool,
    /// round trip
    ms: Option<u32>,
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct Check {
    kvm: Probe,
    transfer: Probe,
    /// what to do about a failed probe
    hint: Option<String>,
}

/// Can this device reach `fingerprint` right now, on the input link and the file link; if not, what to do.
#[tauri::command]
async fn check_device(state: State<'_, AppState>, tls: State<'_, Arc<Tls>>, fingerprint: String) -> Result<Check, String> {
    let (peer, (alias, os, heard)) = {
        let s = state.lock().unwrap();
        if !s.settings.is_paired(&fingerprint) && !s.peers.contains_key(&fingerprint) {
            return Err("that device isn't paired or nearby".into());
        }
        (s.peers.get(&fingerprint).cloned(), s.who(&fingerprint))
    };
    let rtt = |port: fn(&DeviceInfo) -> u16| {
        let (tls, fp, to) = (tls.inner().clone(), fingerprint.clone(), peer.as_ref().map(|p| SocketAddr::new(p.addr, port(&p.info))));
        async move {
            let to = to.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "never heard on the network"))?;
            tokio::time::timeout(Duration::from_secs(5), tls.rtt(to, &fp)).await.unwrap_or_else(|_| Err(std::io::ErrorKind::HostUnreachable.into()))
        }
    };
    let (k, t) = tokio::join!(rtt(|i| i.kvm_port), rtt(|i| i.transfer_port));
    // another device answering says more than any network hint
    let hint = [&k, &t].into_iter().find_map(|r| r.as_ref().err()).map(|e| {
        if e.to_string() == secure::WRONG_DEVICE { kvm::explain(e, &alias, &os, "") } else { kvm::hint(std::env::consts::OS, &os, &alias, heard) }
    });
    let probe = |r: std::io::Result<Duration>| match r {
        Ok(d) => Probe { ok: true, ms: Some(d.as_millis() as u32), error: None },
        Err(e) if e.kind() == std::io::ErrorKind::HostUnreachable => Probe { ok: false, ms: None, error: Some("no answer".into()) },
        Err(e) => Probe { ok: false, ms: None, error: Some(e.to_string()) },
    };
    Ok(Check { kvm: probe(k), transfer: probe(t), hint })
}

/// Plain-text report for support: this device, what it hears, its pairings, layout, screens, state right now
/// and the log's tail. No keys.
fn report(app: &AppHandle) -> String {
    use std::fmt::Write;
    let me = app.state::<watch::Sender<DeviceInfo>>().borrow().clone();
    let screens = screens(app);
    let state = app.state::<AppState>();
    let s = state.lock().unwrap();
    let yes = |b: bool| if b { "yes" } else { "no" };
    let rect = |r: &Rect| format!("{},{} {}×{}", r.x, r.y, r.w, r.h);
    let mut r = format!("LOKA {} · {} {}\n", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH);
    let _ = writeln!(r, "This device: {} ({}…), controllable: {}", me.alias, &me.fingerprint[..me.fingerprint.len().min(12)], yes(me.controllable));
    let ips: Vec<String> = discovery::local_ipv4().iter().map(|ip| ip.to_string()).collect();
    let _ = writeln!(r, "IPs: {}", ips.join(", "));
    let _ = writeln!(r, "Ports (UDP): discovery {}, telemetry {}, transfer {}, kvm {}", discovery::PORT, PORTS.0, PORTS.1, PORTS.2);
    let _ = writeln!(r, "Heard on the network:");
    for p in s.peers.values() {
        let (i, paired) = (&p.info, s.settings.is_paired(&p.info.fingerprint));
        let heard = p.seen.elapsed().as_secs();
        let _ = writeln!(r, "  {} · {} · {} · proto {} · controllable {} · {heard} s ago · {}", i.alias, i.os, p.addr, i.proto, yes(i.controllable), if paired { "paired" } else { "not paired" });
    }
    let offline: Vec<&str> = s.settings.paired.iter().filter(|d| s.live_peers().all(|p| p.info.fingerprint != d.fingerprint)).map(|d| d.alias.as_str()).collect();
    let _ = writeln!(r, "Paired, offline: {}", if offline.is_empty() { "none".to_string() } else { offline.join(", ") });
    let _ = writeln!(r, "Screens: {}", screens.iter().map(rect).collect::<Vec<_>>().join(" | "));
    let _ = writeln!(r, "Layout:");
    for (fp, p) in &s.settings.portals {
        let window = app.get_webview_window(&portal_label(fp)).and_then(|w| Some((w.outer_position().ok()?, w.outer_size().ok()?)));
        let window = window.map_or("none".into(), |(at, size)| format!("{},{} {}×{}", at.x, at.y, size.width, size.height));
        let _ = writeln!(r, "  {}: {:?} edge of {}, {:.2}–{:.2}, overlay window {window}", s.who(fp).0, p.edge, rect(&p.monitor), p.start, p.end);
    }
    let driven = s.session.as_ref().map_or("no".into(), |(fp, _)| format!("by {} (session loop ran {} ms ago)", s.who(fp).0, kvm::beat_age().as_millis()));
    let _ = writeln!(r, "Being driven: {driven} · driving another device: {} · cursor lock: {}", yes(DRIVING.load(Ordering::SeqCst)), yes(kvm::LOCKED.load(Ordering::SeqCst)));
    if let Some(c) = app.state::<Arc<dyn SpatialPlatformEngine>>().cursor() {
        let _ = writeln!(r, "Cursor: {:.0}, {:.0}", c.x, c.y);
    }
    let _ = writeln!(r, "Start with computer: {}", yes(!s.settings.manual_start));
    if let Some((e, at)) = &*LAST_CROSSING.lock().unwrap() {
        let _ = writeln!(r, "Last crossing error ({} s ago): {e}", at.elapsed().as_secs());
    }
    let _ = write!(r, "Log (UTC):\n{}\n", journal::tail(300));
    r
}

/// This device's report plus every reachable paired device's, so one copy shows both sides of a problem.
#[tauri::command]
async fn diagnostics(app: AppHandle) -> String {
    let mut r = report(&app);
    if let Ok(fw) = firewall_status().await {
        r.insert_str(r.find("Log (UTC)").unwrap_or(r.len()), &format!("Windows Firewall: {fw}\n"));
    }
    let others: Vec<(String, String)> = {
        let state = app.state::<AppState>();
        let s = state.lock().unwrap();
        s.settings.paired.iter().filter(|d| s.live_peers().any(|p| p.info.fingerprint == d.fingerprint)).map(|d| (d.fingerprint.clone(), d.alias.clone())).collect()
    };
    for (fp, alias) in others {
        match peer_report(&app, &fp).await {
            Ok(text) => r.push_str(&format!("\n===== {alias} =====\n{text}")),
            Err(e) => r.push_str(&format!("\n===== {alias}: no report ({e}; LOKA before 0.9.4 doesn't send one) =====\n")),
        }
    }
    r
}

/// Paired device `fp`'s `report` (Diagnostics service, 0.9.4+), within 8 s.
async fn peer_report(app: &AppHandle, fp: &str) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let theirs = async {
        let s = open_service(app, fp, kvm::Service::Diagnostics).await?;
        let mut buf = Vec::new();
        s.take(4 << 20).read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    };
    tokio::time::timeout(Duration::from_secs(8), theirs).await.unwrap_or_else(|_| Err("no answer in 8 s".into()))
}

#[cfg(desktop)]
#[derive(serde::Serialize)]
struct Step {
    title: &'static str,
    /// "ok", "fixed" (was wrong, put right here) or "failed"
    result: &'static str,
    detail: String,
}

/// "Troubleshoot": every reason the cursor might not cross into `fingerprint`, putting right what can be
/// put right here, then a real crossing through its portal (tracker path), confirmed by the other side.
/// Stops at the first failure.
#[cfg(desktop)]
#[tauri::command]
async fn troubleshoot(app: AppHandle, fingerprint: String) -> Vec<Step> {
    use tauri::Listener;
    let fp = fingerprint;
    let state = app.state::<AppState>().inner().clone();
    let mut steps = Vec::new();
    let mut step = |title: &'static str, result: &'static str, detail: String| {
        journal!("troubleshoot: {title}: {result}, {detail}");
        steps.push(Step { title, result, detail });
        result != "failed"
    };
    let ((alias, os, heard), paired, peer, portal, delay) = {
        let s = state.lock().unwrap();
        let peer = s.live_peers().find(|p| p.info.fingerprint == fp).map(|p| (p.info.clone(), p.addr));
        (s.who(&fp), s.settings.is_paired(&fp), peer, s.settings.portals.get(&fp).copied(), s.settings.cursor.edge_delay_ms)
    };
    journal!("troubleshoot: {alias}");
    let ok = 'checks: {
        if !step("Paired", if paired { "ok" } else { "failed" }, if paired { format!("{alias} is paired with this computer") } else { "pair with it first".into() }) {
            break 'checks false;
        }
        let Some((info, addr)) = peer else {
            break 'checks step("On the network", "failed", format!("{alias} isn't heard on this network: {}", kvm::hint(std::env::consts::OS, &os, &alias, heard)));
        };
        step("On the network", "ok", format!("heard at {addr}"));
        if info.proto != kvm::PROTOCOL {
            break 'checks step("Same LOKA protocol", "failed", format!("{alias} runs a LOKA that doesn't match this one: update both to the newest"));
        }
        if !info.controllable {
            break 'checks step("Can be controlled", "failed", kvm::not_controllable(&alias, &os));
        }
        step("Can be controlled", "ok", format!("{alias} accepts mouse and keyboard"));
        match check_device(app.state(), app.state(), fp.clone()).await {
            Ok(c) if c.kvm.ok => step("Connection", "ok", format!("answers in {} ms", c.kvm.ms.unwrap_or(0))),
            Ok(c) => break 'checks step("Connection", "failed", format!("{} {}", c.kvm.error.unwrap_or_default(), c.hint.unwrap_or_default())),
            Err(e) => break 'checks step("Connection", "failed", e),
        };
        match free_driven(&app, &state) {
            Some(by) => step("This computer is free", "fixed", format!("it was still being controlled by {by}, which blocks every crossing from here: freed it")),
            None => step("This computer is free", "ok", "not being controlled by another device".into()),
        };
        if kvm::LOCKED.swap(false, Ordering::SeqCst) {
            let _ = app.emit("lock", false);
            step("Cursor lock", "fixed", "cursor lock was on (Scroll Lock / F14): turned it off".into());
        } else {
            step("Cursor lock", "ok", "off".into());
        }
        let Some(portal) = portal else {
            break 'checks step("Portal", "failed", format!("{alias} isn't placed next to a screen: drag it to an edge on the layout map"));
        };
        if !screens(&app).contains(&portal.monitor) {
            break 'checks step("Portal", "failed", format!("{alias}'s portal is on a screen that isn't connected now: place it again on the layout map"));
        }
        let _ = place_overlay(&app, &portal_label(&fp), &portal);
        let m = portal.monitor;
        step("Portal", "ok", format!("{:?} edge of the screen at {},{} ({}×{})", portal.edge, m.x, m.y, m.w, m.h));
        if DRIVING.load(Ordering::SeqCst) {
            break 'checks step("Crossing", "failed", "this computer is controlling a device right now: come back first".into());
        }
        // the real path: the tracker sees the cursor resting on the portal's edge pixel and crosses
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let id = app.listen("kvm", move |e| {
            let _ = tx.send(serde_json::from_str(e.payload()).unwrap_or_default());
        });
        let since = std::time::Instant::now();
        *PROBE.lock().unwrap() = Some(portal.point(0.5, 0.0));
        async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>, secs: u64) -> Option<serde_json::Value> {
            tokio::time::timeout(Duration::from_secs(secs), rx.recv()).await.ok().flatten()
        }
        let entered = next(&mut rx, u64::from(delay) / 1000 + 3).await;
        *PROBE.lock().unwrap() = None;
        let ok = match entered {
            Some(v) if v["active"] == true => {
                // ask the other side while the session runs; it may also end first (an error, or the mouse moved back)
                let (mut confirmed, mut ended) = (None::<Result<String, String>>, None::<serde_json::Value>);
                let until = std::time::Instant::now() + Duration::from_secs(3);
                while confirmed.is_none() && ended.is_none() && std::time::Instant::now() < until {
                    tokio::select! {
                        v = rx.recv() => ended = Some(v.unwrap_or_default()),
                        r = peer_report(&app, &fp) => match r {
                            Ok(t) => {
                                let line = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)).map(str::to_string);
                                if line("Being driven: ").is_some_and(|d| d.starts_with("by ")) {
                                    confirmed = Some(Ok(line("Cursor: ").map_or(String::new(), |c| format!(", its cursor at {c}"))));
                                } else {
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                }
                            }
                            Err(e) => confirmed = Some(Err(e)),
                        },
                    }
                }
                match ended {
                    Some(v) if v["error"].is_string() => step("Crossing", "failed", v["error"].as_str().unwrap_or_default().into()),
                    // only the other side sends the cursor back: it was controlled
                    Some(_) => step("Crossing", "ok", format!("the cursor went into {alias} and came back by itself (the mouse moved back)")),
                    None => {
                        let crossed = match confirmed {
                            Some(Ok(at)) => step("Crossing", "ok", format!("the cursor went into {alias}, which confirms this computer controls it{at}")),
                            Some(Err(e)) => step("Crossing", "ok", format!("the cursor went into {alias} ({alias} couldn't confirm: {e})")),
                            None => step("Crossing", "failed", format!("the cursor left this computer, but {alias} doesn't see itself controlled")),
                        };
                        kvm::end_sessions();
                        let back = match next(&mut rx, 3).await {
                            Some(v) if v["error"].is_string() => step("Coming back", "failed", v["error"].as_str().unwrap_or_default().into()),
                            Some(_) => step("Coming back", "ok", "the cursor came back to this computer".into()),
                            None => step("Coming back", "failed", "the session didn't end within 3 s: use the tray's Disconnect, or Ctrl+Alt+Esc".into()),
                        };
                        crossed && back
                    }
                }
            }
            _ => {
                let e = LAST_CROSSING.lock().unwrap().as_ref().filter(|(_, at)| at.elapsed() <= since.elapsed() + Duration::from_secs(30)).map(|(e, _)| e.clone());
                step("Crossing", "failed", e.unwrap_or_else(|| format!("the cursor at {alias}'s edge didn't cross: copy the diagnostics (About) and send them")))
            }
        };
        app.unlisten(id);
        ok
    };
    journal!("troubleshoot: {}", if ok { "everything works" } else { "stopped at a failure" });
    steps
}

/// Clears a driven session (a stuck one holds the cursor: only the way back counts). The driver's alias, if any.
fn free_driven(app: &AppHandle, state: &AppState) -> Option<String> {
    let (fp, _) = state.lock().unwrap().session.take()?;
    kvm::end_sessions();
    if let Some(engine) = app.try_state::<Arc<dyn SpatialPlatformEngine>>() {
        engine.show_cursor(false);
    }
    if let Some(w) = app.get_webview_window(BAND) {
        let _ = w.hide();
    }
    refresh_tray(app);
    Some(state.lock().unwrap().who(&fp).0)
}

/// Where received files go: the folder the user picked, or Downloads/LOKA.
pub(crate) fn received_dir(app: &AppHandle) -> PathBuf {
    let picked = app.state::<AppState>().lock().unwrap().settings.received.clone();
    picked.unwrap_or_else(|| app.path().download_dir().or_else(|_| app.path().app_data_dir()).unwrap_or_default().join("LOKA"))
}

#[tauri::command]
fn set_sharing(app: AppHandle, state: State<AppState>, sharing: settings::Sharing) {
    let mut s = state.lock().unwrap();
    s.settings.sharing = sharing;
    s.settings.save();
    drop(s);
    let _ = app.emit("device", ());
}

/// `None`: back to Downloads/LOKA.
#[tauri::command]
fn set_received(app: AppHandle, state: State<AppState>, folder: Option<PathBuf>) {
    let mut s = state.lock().unwrap();
    s.settings.received = folder;
    s.settings.save();
    drop(s);
    let _ = app.emit("device", ());
}

/// Wake-on-LAN: the magic packet (6×0xFF, then the MAC 16 times) to every local broadcast address.
#[tauri::command]
async fn wake(state: State<'_, AppState>, fingerprint: String) -> Result<(), String> {
    let mac = state.lock().unwrap().settings.macs.get(&fingerprint).cloned().ok_or("LOKA hasn't learned that computer's network address yet: open LOKA on it once")?;
    let bytes: Vec<u8> = mac.split([':', '-']).filter_map(|b| u8::from_str_radix(b, 16).ok()).collect();
    if bytes.len() != 6 {
        return Err("that computer announced an unusable network address".into());
    }
    let mut packet = vec![0xFF; 6];
    (0..16).for_each(|_| packet.extend_from_slice(&bytes));
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
    sock.set_broadcast(true).map_err(|e| e.to_string())?;
    let mut targets = vec![std::net::Ipv4Addr::BROADCAST];
    // per-subnet broadcasts reach it on routers that drop 255.255.255.255 (assumes /24, the home default)
    targets.extend(discovery::local_ipv4().into_iter().map(|ip| std::net::Ipv4Addr::from(u32::from(ip) | 0xFF)));
    for ip in targets {
        for port in [9, 7] {
            let _ = sock.send_to(&packet, (ip, port)).await;
        }
    }
    Ok(())
}

/// Send files (from Send to / Open With) to paired device `fingerprint`.
#[tauri::command]
async fn send_paths(app: AppHandle, state: State<'_, AppState>, fingerprint: String, paths: Vec<PathBuf>) -> Result<(), String> {
    let to = state.lock().unwrap().addr(&fingerprint, |i| i.transfer_port).ok_or("that device isn't on the network")?;
    let files = tokio::task::spawn_blocking(move || transfer::expand(&paths)).await.unwrap_or_default();
    if files.is_empty() {
        return Err("nothing to send".into());
    }
    send_files(app, to, fingerprint, files, None).await;
    Ok(())
}

/// A file picked on the phone, as base64: Android hands the web view file contents, not paths, and its IPC
/// bridge carries JSON only.
// shortcut: the whole file passes through memory (and base64); stream it in chunks if phones send multi-GB videos
#[tauri::command]
async fn send_upload(app: AppHandle, fingerprint: String, name: String, data: String) -> Result<(), String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| "unreadable file")?;
    let to = app.state::<AppState>().lock().unwrap().addr(&fingerprint, |i| i.transfer_port).ok_or("that device isn't on the network")?;
    let name = transfer::file_name(std::path::Path::new(&name)); // a plain file name, never a path
    let dir = app.path().app_cache_dir().map_err(|e| e.to_string())?.join("outgoing");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&name);
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    send_files(app, to, fingerprint, vec![(path, name)], None).await;
    Ok(())
}

/// Files waiting for a device to go to (Send to / Open With), emptied by reading.
#[tauri::command]
fn take_outbox(state: State<AppState>) -> Vec<PathBuf> {
    std::mem::take(&mut state.lock().unwrap().outbox)
}

/// Files handed to LOKA from the file manager: ask the user where to send them.
fn receive_outbox(app: &AppHandle, paths: Vec<PathBuf>) {
    let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
    if paths.is_empty() {
        return;
    }
    app.state::<AppState>().lock().unwrap().outbox.extend(paths);
    show_main(app);
    let _ = app.emit_to(MAIN_WINDOW, "outbox", ());
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
        #[cfg(desktop)]
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Hand mouse & keyboard to paired device `fingerprint` now, as if the cursor had crossed into its
/// portal (in the middle). The tray's "Control …".
#[tauri::command]
async fn control(app: AppHandle, fingerprint: String) -> Result<(), String> {
    if DRIVING.load(Ordering::SeqCst) {
        kvm::end_sessions();
        // let the current session give input back first
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if !DRIVING.load(Ordering::SeqCst) {
                break;
            }
        }
    }
    let portal = app.state::<AppState>().lock().unwrap().settings.portals.get(&fingerprint).copied();
    // a phone drives from its touchpad screen: the computer counts as sitting off its right edge
    #[cfg(mobile)]
    let portal = portal.or(Some(Portal { edge: Edge::Right, monitor: Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, start: 0.0, end: 1.0, reach: 0.0 }));
    let portal = portal.ok_or("place that device next to a screen first")?;
    start_session(&app, portal_label(&fingerprint), fingerprint, portal, 0.5, None)
}

/// The phone's touchpad: input for the computer it drives (`control`).
#[cfg(mobile)]
#[tauri::command]
fn touch(event: platform::Touch) {
    platform::touch(event);
}

/// Port and token `Injector.kt` dials back with (LokaPlugin `start`).
#[cfg(target_os = "android")]
#[tauri::command]
fn injector_ticket() -> Option<(u16, String)> {
    platform::android::ticket()
}

/// End whatever session runs: give this computer's input back, or hand the cursor back to whoever drives it.
#[tauri::command]
fn disconnect() {
    kvm::end_sessions();
}

/// Ctrl+Alt+V / ⌃⌥V: the files that arrived last go onto the clipboard and are pasted into the app in front.
#[cfg(desktop)]
fn paste_recent(app: &AppHandle) {
    let files = app.state::<AppState>().lock().unwrap().recent.clone();
    let files: Vec<PathBuf> = files.into_iter().filter(|p| p.exists()).collect();
    if files.is_empty() {
        return;
    }
    platform::set_clipboard_files(&files);
    let engine = app.state::<Arc<dyn SpatialPlatformEngine>>().inner().clone();
    // the shortcut's own modifiers are still held: lift them, then press the OS paste chord
    let (ctrl, alt, paste_mod) = (0x1D, 0x38, if cfg!(target_os = "macos") { 0xE05B } else { 0x1D });
    for scan in [alt, 0xE038, ctrl] {
        engine.inject(Injected::Key { scan, down: false });
    }
    for (scan, down) in [(paste_mod, true), (0x2F, true), (0x2F, false), (paste_mod, false)] {
        engine.inject(Injected::Key { scan, down });
    }
}

impl Shared {
    fn live_peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values().filter(|p| p.seen.elapsed() < discovery::STALE)
    }

    /// A paired device we know an address for (not only live ones: a few lost heartbeats must not
    /// lock it out mid-use).
    fn paired_peer(&self, fp: &str) -> Option<&Peer> {
        self.peers.get(fp).filter(|_| self.settings.is_paired(fp))
    }

    fn addr(&self, fp: &str, port: impl Fn(&DeviceInfo) -> u16) -> Option<SocketAddr> {
        self.paired_peer(fp).map(|p| SocketAddr::new(p.addr, port(&p.info)))
    }

    /// Name and OS of device `fp` for messages, and whether its announcements still arrive.
    fn who(&self, fp: &str) -> (String, String, bool) {
        if let Some(p) = self.peers.get(fp) {
            return (p.info.alias.clone(), p.info.os.clone(), p.seen.elapsed() < discovery::STALE);
        }
        let alias = self.settings.paired.iter().find(|d| d.fingerprint == fp).map_or_else(|| "The other device".into(), |d| d.alias.clone());
        (alias, String::new(), false)
    }
}

#[derive(serde::Serialize)]
struct Device {
    info: DeviceInfo,
    paired: Vec<PairedDevice>,
    portals: HashMap<String, Portal>,
    /// this computer's LAN addresses, to type into another LOKA's "Add by IP"
    ips: Vec<std::net::Ipv4Addr>,
    hosts: Vec<IpAddr>,
    cursor: CursorPrefs,
    sharing: settings::Sharing,
    /// the folder received files go to
    received: PathBuf,
    /// paired devices that can be woken up
    wakeable: Vec<String>,
}

#[tauri::command]
fn get_device(app: AppHandle, state: State<AppState>, me: State<watch::Sender<DeviceInfo>>) -> Device {
    let received = received_dir(&app);
    let s = state.lock().unwrap();
    Device {
        info: me.borrow().clone(),
        paired: s.settings.paired.clone(),
        portals: s.settings.portals.clone(),
        ips: discovery::local_ipv4(),
        hosts: s.settings.hosts.clone(),
        cursor: s.settings.cursor,
        sharing: s.settings.sharing.clone(),
        received,
        wakeable: s.settings.macs.keys().cloned().collect(),
    }
}

/// Applies from the next crossing on.
#[tauri::command]
fn set_cursor_prefs(state: State<AppState>, prefs: CursorPrefs) {
    let mut s = state.lock().unwrap();
    s.settings.cursor = prefs;
    s.settings.save();
}

#[tauri::command]
fn peers(state: State<AppState>) -> Vec<Peer> {
    state.lock().unwrap().live_peers().cloned().collect()
}

/// Ask a nearby device's user to pair. Emits `pair-code` `{fingerprint, code}` once the code both
/// screens show is known; resolves with their answer (`accepted`, `declined`, `noAnswer`, `unknown`).
#[tauri::command]
async fn pair(app: AppHandle, state: State<'_, AppState>, tls: State<'_, Arc<Tls>>, fingerprint: String) -> Result<kvm::PairAnswer, String> {
    let peer = state.lock().unwrap().live_peers().find(|p| p.info.fingerprint == fingerprint).cloned().ok_or("device is no longer on the network")?;
    // the connection only goes through if the device there holds the certificate discovery announced
    let show = |code: String| {
        let _ = app.emit_to(MAIN_WINDOW, "pair-code", serde_json::json!({ "fingerprint": fingerprint, "code": code }));
    };
    let answer = kvm::request_pair(&tls, SocketAddr::new(peer.addr, peer.info.kvm_port), &fingerprint, show).await.map_err(|e| explain(&app, &fingerprint, &e))?;
    if answer == kvm::PairAnswer::Accepted {
        state.lock().unwrap().settings.pair(PairedDevice { fingerprint, alias: peer.info.alias });
        let _ = app.emit("paired", ());
        refresh_tray(&app);
    }
    Ok(answer)
}

#[tauri::command]
fn answer_pair(state: State<AppState>, id: u64, accept: bool) {
    if let Some(tx) = state.lock().unwrap().pending.remove(&id) {
        let _ = tx.send(accept);
    }
}

#[tauri::command]
fn unpair(app: AppHandle, state: State<AppState>, tls: State<Arc<Tls>>, fingerprint: String) {
    // tell it first (its address goes with the pairing), so it drops this one and its glow too
    if let Some(to) = state.lock().unwrap().addr(&fingerprint, |i| i.kvm_port) {
        let (tls, fp) = (tls.inner().clone(), fingerprint.clone());
        tauri::async_runtime::spawn(async move { kvm::unpair(&tls, to, &fp).await });
    }
    forget(&app, &fingerprint);
}

/// Drop a paired device here: pairing, portal and its overlay.
fn forget(app: &AppHandle, fingerprint: &str) {
    app.state::<AppState>().lock().unwrap().settings.unpair(fingerprint);
    if let Some(w) = app.get_webview_window(&portal_label(fingerprint)) {
        let _ = w.close();
    }
    let _ = app.emit("paired", ());
    refresh_tray(app);
}

/// Remember a device by address (networks that drop multicast) and ask it to answer now.
#[tauri::command]
async fn add_host(state: State<'_, AppState>, me: State<'_, watch::Sender<DeviceInfo>>, ip: String) -> Result<(), String> {
    let ip: std::net::Ipv4Addr = ip.trim().parse().map_err(|_| "Type an address like 192.168.1.20")?;
    if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() || ip.is_loopback() {
        return Err("That address can't be another computer".into());
    }
    let ip = IpAddr::V4(ip);
    {
        let mut s = state.lock().unwrap();
        if !s.settings.hosts.contains(&ip) {
            s.settings.hosts.push(ip);
            s.settings.save();
        }
    }
    let info = me.borrow().clone();
    discovery::poke(ip, &info).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_host(state: State<AppState>, ip: IpAddr) {
    let mut s = state.lock().unwrap();
    s.settings.hosts.retain(|h| *h != ip);
    s.settings.save();
}

/// Windows: allow LOKA inbound on every network profile (one UAC prompt). The first-run firewall
/// prompt often only allows Public or Private, and Windows marks new networks Public. Earlier inbound
/// rules for this exe go first: a Block one (prompt cancelled) would win over the allow.
#[tauri::command]
async fn allow_firewall() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let program = format!("dir=in program=\"{}\"", exe.display());
        let args = format!("/c netsh advfirewall firewall delete rule name=all {program} & netsh advfirewall firewall add rule name=LOKA action=allow profile=any {program}");
        // Start-Process throws when the UAC prompt is declined → exit 1
        let script = format!("$p = Start-Process cmd -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '{}'; exit $p.ExitCode", args.replace('\'', "''"));
        let out = powershell(script).await?;
        return if out.status.success() { Ok(()) } else { Err("Windows Firewall was not changed".into()) };
    }
    #[allow(unreachable_code)]
    Err("Only needed on Windows".into())
}

/// Windows: this exe's inbound firewall rules: `blocked` (any Block rule), `allowed` (an Allow on every profile) or `none`.
#[tauri::command]
async fn firewall_status() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // the firewall COM API: Get-NetFirewall* takes ~8 s
        let script = format!(
            "$r = @((New-Object -ComObject HNetCfg.FwPolicy2).Rules | ? {{ $_.ApplicationName -eq '{}' -and $_.Direction -eq 1 -and $_.Enabled }}); \
             if ($r | ? Action -eq 0) {{ 'blocked' }} elseif ($r | ? {{ $_.Action -eq 1 -and ($_.Profiles -band 7) -eq 7 }}) {{ 'allowed' }} else {{ 'none' }}",
            exe.display().to_string().replace('\'', "''")
        );
        let out = powershell(script).await?;
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    #[allow(unreachable_code)]
    Err("Only needed on Windows".into())
}

#[cfg(target_os = "windows")]
async fn powershell(script: String) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    tauri::async_runtime::spawn_blocking(move || std::process::Command::new("powershell").args(["-NoProfile", "-Command", &script]).creation_flags(CREATE_NO_WINDOW).output())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Let LOKA send input: Linux `/dev/input` + uinput (one password prompt); macOS Accessibility (prompt + its pane).
#[tauri::command]
async fn allow_input() -> Result<(), String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return tauri::async_runtime::spawn_blocking(platform::allow_input).await.map_err(|e| e.to_string())?;
    #[allow(unreachable_code)]
    Err("Only needed on Linux and macOS".into())
}

/// This computer's name as other devices list it.
fn alias() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        // macOS GUI apps get no HOSTNAME and have no /proc: ask for the name set in System Settings
        .or_else(|| cfg!(target_os = "macos").then(|| std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok()).flatten().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|h| !h.is_empty()))
        // Android: the name in Settings › About ("Galaxy S23 Ultra", or the one the user gave it), else the model
        // ("SM-S918B"); its hostname is just "localhost"
        .or_else(|| {
            #[cfg(target_os = "android")]
            return platform::android::device_name().or_else(|| std::process::Command::new("getprop").arg("ro.product.model").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|h| !h.is_empty()));
            #[cfg(not(target_os = "android"))]
            None
        })
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|h| h.trim().to_string()).filter(|h| !h.is_empty()))
        .unwrap_or_else(|| "LOKA".into())
}

/// Open a web page in the default browser.
#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https links".into());
    }
    #[cfg(target_os = "android")]
    let r = app.state::<tauri::plugin::PluginHandle<tauri::Wry>>().run_mobile_plugin::<serde_json::Value>("openUrl", serde_json::json!({ "url": url }));
    #[cfg(not(target_os = "android"))]
    let _ = app;
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(&url).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "android")))]
    let r = std::process::Command::new("xdg-open").arg(&url).spawn();
    r.map(drop).map_err(|e| e.to_string())
}

/// Show a pairing prompt (with the `code` both screens show) in `window` and wait for the user
/// (or time out → no answer).
async fn ask_user(app: &AppHandle, state: &AppState, window: &str, from: &Peer, code: String) -> kvm::PairAnswer {
    let (tx, rx) = oneshot::channel();
    let id = {
        let mut s = state.lock().unwrap();
        s.next_request += 1;
        let id = s.next_request;
        s.pending.insert(id, tx);
        id
    };
    let _ = app.emit_to(window, "pair-request", serde_json::json!({ "id": id, "peer": from, "code": code }));
    let answer = match tokio::time::timeout(kvm::PAIR_TIMEOUT, rx).await {
        Ok(Ok(true)) => kvm::PairAnswer::Accepted,
        Ok(Ok(false)) => kvm::PairAnswer::Declined,
        _ => kvm::PairAnswer::NoAnswer,
    };
    state.lock().unwrap().pending.remove(&id);
    let _ = app.emit_to(window, "pair-request-closed", id);
    answer
}

/// Store (or with `None`, remove) the portal to paired device `fingerprint` and put its overlay there.
/// `sync`: the side changed, so that device puts this one on its opposite edge (or takes it off); resolves
/// with why it didn't, if it couldn't.
// async: building a window inside a sync command deadlocks WebView2 on Windows
#[tauri::command]
async fn set_portal(app: AppHandle, state: State<'_, AppState>, tls: State<'_, Arc<Tls>>, fingerprint: String, portal: Option<Portal>, sync: bool) -> Result<Option<String>, String> {
    let (to, alias, phone) = {
        let mut s = state.lock().unwrap();
        if !s.settings.is_paired(&fingerprint) {
            return Err("not paired".into());
        }
        match portal {
            Some(p) => s.settings.portals.insert(fingerprint.clone(), p),
            None => s.settings.portals.remove(&fingerprint),
        };
        s.settings.save();
        let alias = s.settings.paired.iter().find(|d| d.fingerprint == fingerprint).map(|d| d.alias.clone()).unwrap_or_default();
        let phone = s.peers.get(&fingerprint).is_some_and(|p| p.info.os == "android");
        (s.addr(&fingerprint, |i| i.kvm_port), alias, phone)
    };
    show_portal(&app, &fingerprint, portal)?;
    if !sync {
        return Ok(None);
    }
    let Some(to) = to else { return Ok(Some(format!("{alias} is offline: place this computer on its screens there"))) };
    let edge = portal.map(|p| p.edge);
    let placed = tokio::time::timeout(Duration::from_secs(5), kvm::place(&tls, to, &fingerprint, edge)).await;
    Ok(match placed.unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into())) {
        Ok(true) => None,
        // phones before 0.9.3 refused every Place; newer ones always accept
        Ok(false) if phone => Some(format!("{alias} runs an older LOKA: update it, or set its side on the phone")),
        Ok(false) => Some(format!("{alias} has no free room on that side: place this computer on its screens there")),
        Err(e) => Some(explain(&app, &fingerprint, &e)),
    })
}

/// Tray, overlay window and its placement for the portal to `fingerprint` (already in settings).
fn show_portal(app: &AppHandle, fingerprint: &str, portal: Option<Portal>) -> Result<(), String> {
    refresh_tray(app);
    let label = portal_label(fingerprint);
    let Some(portal) = portal else {
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.close();
        }
        return Ok(());
    };
    // a phone has no overlay windows (yet)
    #[cfg(mobile)]
    return {
        let _ = portal;
        Ok(())
    };
    #[cfg(desktop)]
    {
        if app.get_webview_window(sim::LABEL).is_some() && app.state::<AppState>().lock().unwrap().peers.get(fingerprint).is_some_and(|p| p.info.os == "sim") {
            sim::place(app, &portal).map_err(|e| e.to_string())?;
        }
        open_overlay(app, &label, Some(fingerprint.to_string()), portal.edge).map_err(|e| e.to_string())?;
        let _ = app.emit_to(&label, "portal", portal.edge); // redraw for a new edge without a restart
        place_overlay(app, &label, &portal).map_err(|e| e.to_string())
    }
}

/// The overlay window `label`, created if needed. `to`: the paired device a drop on it goes to
/// (`None`: the band, whose drops go back to whoever drives us).
#[cfg(desktop)]
fn open_overlay(app: &AppHandle, label: &str, to: Option<String>, edge: Edge) -> tauri::Result<tauri::WebviewWindow> {
    use tauri::{DragDropEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
    if let Some(w) = app.get_webview_window(label) {
        return Ok(w);
    }
    let edge = serde_json::to_value(edge).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    let w = WebviewWindowBuilder::new(app, label, WebviewUrl::App(format!("index.html?edge={edge}").into()))
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(cfg!(target_os = "linux")) // GTK ignores set_size on a non-resizable window; no decorations = no handles anyway
        .focused(false)
        // WebView2 makes its input child window only once shown, so a hidden build leaves wry's file drop target off
        // it (drags got ⊘); built shown off-screen, then hidden until place_overlay shows it (the child stays)
        .visible(cfg!(target_os = "windows"))
        .position(-32000.0, -32000.0)
        .build()?;
    w.hide()?; // the band is only placed when a session starts
    let engine = app.state::<Arc<dyn SpatialPlatformEngine>>().inner().clone();
    let window = w.clone();
    // AppKit window setup belongs on the main thread
    app.run_on_main_thread(move || {
        if let Err(e) = engine.prepare_overlay(&window) {
            journal!("overlay: {e}");
        }
    })?;

    let (h, own, label) = (app.clone(), app.path().app_cache_dir()?.join("clipboard"), label.to_string());
    w.on_window_event(move |e| {
        let WindowEvent::DragDrop(e) = e else { return };
        let s = h.state::<AppState>();
        let paths = match e {
            DragDropEvent::Enter { paths, .. } => {
                s.lock().unwrap().hover = Some(paths.clone());
                return;
            }
            DragDropEvent::Leave => {
                s.lock().unwrap().hover = None;
                return;
            }
            DragDropEvent::Drop { paths, .. } => paths.clone(),
            _ => return,
        };
        let (target, tele, y) = {
            let mut st = s.lock().unwrap();
            st.hover = None;
            // our own hand-off, or a drag that just came over from the other device let go at the edge
            if std::mem::take(&mut st.handoff) || paths.iter().any(|p| p.starts_with(&own)) {
                return;
            }
            let Some(fp) = to.clone().or_else(|| st.session.as_ref().map(|(fp, _)| fp.clone())) else { return };
            let tele = to.as_ref().and_then(|_| st.addr(&fp, |i| i.telemetry_port)); // the driver has no glow for its band
            (st.addr(&fp, |i| i.transfer_port).map(|a| (a, fp)), tele, st.last_y)
        };
        let Some((addr, fp)) = target else { return };
        let (h, label) = (h.clone(), label.clone());
        tauri::async_runtime::spawn(async move {
            let packet = SpatialPacket { kind: PacketType::Drop, normalized_y: y, proximity: 1.0, payload: PayloadKind::File };
            if let (Ok(sock), Some(tele)) = (telemetry::sender().await, tele) {
                telemetry::send(&sock, tele, &packet).await;
            }
            let _ = h.emit_to(&label, "spatial", packet);
            let files = tokio::task::spawn_blocking(move || transfer::expand(&paths)).await.unwrap_or_default();
            send_files(h, addr, fp, files, None).await;
        });
    });
    Ok(w)
}

/// Move overlay `label` onto `portal`'s band and show it, click-through.
fn place_overlay(app: &AppHandle, label: &str, portal: &Portal) -> tauri::Result<()> {
    let Some(w) = app.get_webview_window(label) else { return Ok(()) };
    let b = portal.hit_box();
    // twice: landing on a monitor with another scale makes Windows rescale the window (WM_DPICHANGED)
    for _ in 0..2 {
        w.set_position(tauri::PhysicalPosition::new(b.x as i32, b.y as i32))?;
        w.set_size(tauri::PhysicalSize::new(b.w as u32, b.h as u32))?;
    }
    w.show()?;
    // show() rewrites window styles on Windows; restore the overlay ones
    match app.try_state::<Arc<dyn SpatialPlatformEngine>>() {
        Some(e) => e.set_click_through(&w, true),
        None => Ok(()),
    }
}

#[tauri::command]
fn cancel_transfer(id: u64) {
    transfer::cancel(id);
}

/// Open the file manager with `path` selected.
#[tauri::command]
fn reveal(path: PathBuf) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg("/select,").arg(&path).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(&path).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(&path)).spawn();
    r.map(drop).map_err(|e| e.to_string())
}

/// Received mouse/keyboard goes to this OS, across all of its monitors.
struct EngineSink {
    engine: Arc<dyn SpatialPlatformEngine>,
    screens: Vec<Rect>,
    app: AppHandle,
}

/// Session over: no band back to the driver any more; a picture or files copied here go to it.
impl Drop for EngineSink {
    fn drop(&mut self) {
        self.engine.show_cursor(false);
        let to = {
            let state = self.app.state::<AppState>();
            let mut s = state.lock().unwrap();
            let gone = s.session.take();
            if let Some((fp, _)) = &gone {
                journal!("no longer driven by {}", s.who(fp).0);
            }
            gone.and_then(|(fp, _)| s.addr(&fp, |i| i.transfer_port).map(|a| (a, fp)))
        };
        refresh_tray(&self.app);
        #[cfg(desktop)]
        if let Some((addr, fp)) = to {
            sync_clipboard(self.app.clone(), addr, fp);
        }
        #[cfg(mobile)]
        let _ = to;
        if let Some(w) = self.app.get_webview_window(BAND) {
            let _ = w.hide();
        }
    }
}

impl kvm::Sink for EngineSink {
    fn screens(&self) -> Vec<Rect> {
        self.screens.clone()
    }
    fn inject(&mut self, e: Injected) {
        self.engine.inject(e);
    }
    fn clipboard(&self) -> Option<String> {
        platform::clipboard_text()
    }
    fn set_clipboard(&mut self, text: String) {
        platform::set_clipboard_text(&text);
    }
    fn drag(&mut self, batch: u64) {
        hold_drag(&self.app, BAND, batch);
    }
    fn hop(&self, x: f64, y: f64) -> Option<kvm::Hop> {
        let state = self.app.state::<AppState>();
        let s = state.lock().unwrap();
        let driver = s.session.as_ref().map(|(fp, _)| fp.as_str());
        s.settings.portals.iter().filter(|(fp, _)| Some(fp.as_str()) != driver).find_map(|(fp, portal)| {
            let (along, p) = portal.sample(x, y)?;
            let to = s.addr(fp, |i| i.kvm_port).filter(|_| p >= 1.0)?;
            Some(kvm::Hop { to, fp: fp.clone(), portal: *portal, y: along, text: s.peers.get(fp).is_some_and(|p| p.info.text) })
        })
    }
}

/// `loka --send <files…>` (Windows Send to): the files to send, if launched that way.
fn send_args(args: &[String]) -> Vec<PathBuf> {
    args.iter().skip_while(|a| *a != "--send").skip(1).map(PathBuf::from).collect()
}

/// Windows: a "LOKA" entry in Explorer's Send to menu (a shortcut to `loka --send`), refreshed each start
/// so it follows the installed exe.
#[cfg(target_os = "windows")]
fn install_send_to() {
    use std::os::windows::process::CommandExt;
    let (Ok(exe), Ok(appdata)) = (std::env::current_exe(), std::env::var("APPDATA")) else { return };
    let link = PathBuf::from(appdata).join(r"Microsoft\Windows\SendTo\LOKA.lnk");
    let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.Arguments='--send';$s.IconLocation='{},0';$s.Save()",
        q(&link),
        q(&exe),
        q(&exe)
    );
    // shortcut: the entry outlives an uninstall (harmless: it points at a missing exe)
    let _ = std::process::Command::new("powershell").args(["-NoProfile", "-Command", &script]).creation_flags(0x0800_0000).spawn();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.iter().any(|a| a == "--allow-input") {
            match platform::allow_input() {
                Ok(()) => println!("LOKA can use the mouse and keyboard now."),
                Err(e) => {
                    journal!("{e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        if args.iter().any(|a| a == "--headless") {
            return headless::run();
        }
        // overlays need to sit at exact screen positions, which Wayland gives no app: run through XWayland
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("GDK_BACKEND").is_none() {
            std::env::set_var("GDK_BACKEND", "x11");
        }
        // WebKitGTK's DMA-BUF renderer draws an empty window on NVIDIA's driver and in VMs (seen under WSLg)
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
    let mut builder = tauri::Builder::default();
    // Android side of being driven: Wireless debugging, the injector, the cursor overlay (LokaPlugin.kt)
    builder = builder.plugin(
        tauri::plugin::Builder::<tauri::Wry>::new("loka")
            .setup(|app, api| {
                #[cfg(target_os = "android")]
                app.manage(api.register_android_plugin("dev.loka.app", "LokaPlugin")?);
                let _ = (app, api);
                Ok(())
            })
            .build(),
    );
    #[cfg(desktop)]
    {
        use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};
        let paste = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyV);
        builder = builder
            // one LOKA per computer; a second launch (Send to) hands its files to the running one
            .plugin(tauri_plugin_single_instance::init(|app, args, _| {
                let files = send_args(&args);
                if files.is_empty() {
                    show_main(app);
                } else {
                    receive_outbox(app, files);
                }
            }))
            .plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_shortcut(paste)
                    .expect("Ctrl+Alt+V")
                    .with_handler(move |app, s, e| {
                        if *s == paste && e.state() == ShortcutState::Released {
                            paste_recent(app);
                        }
                    })
                    .build(),
            )
            .plugin(tauri_plugin_updater::Builder::new().build())
            // started with the computer: straight to the tray (`--autostart`)
            .plugin(tauri_plugin_autostart::Builder::new().arg("--autostart").build());
    }
    let app = builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_device,
            peers,
            pair,
            answer_pair,
            unpair,
            set_portal,
            check_device,
            diagnostics,
            #[cfg(desktop)]
            troubleshoot,
            set_cursor_prefs,
            cancel_transfer,
            reveal,
            add_host,
            remove_host,
            allow_firewall,
            firewall_status,
            allow_input,
            open_url,
            #[cfg(desktop)]
            sim::start_sim,
            set_sharing,
            set_received,
            wake,
            send_paths,
            take_outbox,
            control,
            disconnect,
            #[cfg(mobile)]
            touch,
            #[cfg(target_os = "android")]
            injector_ticket,
            send_upload,
            #[cfg(desktop)]
            clip::clip_history,
            #[cfg(desktop)]
            clip::clip_restore,
            #[cfg(desktop)]
            shell::term_open,
            #[cfg(desktop)]
            shell::term_write,
            #[cfg(desktop)]
            shell::term_resize,
            #[cfg(desktop)]
            shell::term_close,
            #[cfg(desktop)]
            share::open_share,
            #[cfg(desktop)]
            audio::set_listening,
            #[cfg(desktop)]
            audio::set_mic,
            #[cfg(desktop)]
            update::check_update,
            #[cfg(desktop)]
            update::install_update,
            notify::set_mirroring
        ])
        // closing the window keeps LOKA sharing from the tray; "Quit LOKA" there ends it
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == MAIN_WINDOW {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            let data = app.path().app_data_dir()?;
            journal::init(&data);
            let autostarted = std::env::args().any(|a| a == "--autostart");
            journal!("LOKA {} starting on {} {}{}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH, if autostarted { ", with the computer" } else { "" });
            let settings = Settings::load(&data);
            #[cfg(desktop)]
            {
                set_autostart(&handle, !settings.manual_start);
                if autostarted {
                    if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
                        let _ = w.hide();
                    }
                }
            }
            let tls = Arc::new(Tls::new(&Identity::load_or_create(&data)));
            let aspect_ratio = app
                .primary_monitor()?
                .map(|m| m.size().width as f32 / m.size().height as f32)
                .unwrap_or(16.0 / 9.0);
            let me = DeviceInfo {
                alias: alias(),
                device_type: if cfg!(mobile) { DeviceType::Mobile } else { DeviceType::Desktop },
                os: std::env::consts::OS.into(),
                fingerprint: tls.me.clone(),
                aspect_ratio,
                telemetry_port: PORTS.0,
                transfer_port: PORTS.1,
                kvm_port: PORTS.2,
                proto: kvm::PROTOCOL,
                mac: mac_address::get_mac_address().ok().flatten().map(|m| m.to_string()),
                // a phone's injector starts later; `watch_controllable` keeps it current
                controllable: !cfg!(target_os = "android"),
                text: true,
            };
            let state: AppState = Arc::new(Mutex::new(Shared {
                settings,
                peers: HashMap::new(),
                last_y: 0.0,
                session: None,
                hover: None,
                handoff: false,
                carry: Default::default(),
                carry_window: BAND.into(),
                pending: HashMap::new(),
                next_request: 0,
                recent: Vec::new(),
                outbox: send_args(&std::env::args().collect::<Vec<_>>()),
            }));
            app.manage(state.clone());
            app.manage(tls.clone());
            let (me_tx, me_rx) = watch::channel(me);
            app.manage(me_tx);

            spawn_discovery(handle.clone(), state.clone(), me_rx);
            spawn_watchdog(handle.clone(), state.clone());
            spawn_transfer_server(&handle, state.clone(), tls.clone())?;
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = telemetry::listen(PORTS.0, |p| {
                    let _ = h.emit("spatial-remote", p);
                })
                .await
                {
                    journal!("telemetry listener stopped: {e}");
                }
            });

            #[cfg(desktop)]
            {
                platform::keep_running();
                setup_overlays(app, state.clone(), tls)?;
                setup_tray(app)?;
                clip::watch(handle.clone());
            }
            // drives a computer from the touchpad; is driven through the injector once Wireless debugging is set up
            #[cfg(mobile)]
            {
                #[cfg(target_os = "android")]
                let engine: Arc<dyn SpatialPlatformEngine> = {
                    let plugin = app.state::<tauri::plugin::PluginHandle<tauri::Wry>>().inner().clone();
                    Arc::new(platform::android::AndroidEngine::new(Box::new(move |at| {
                        let (x, y, show) = at.map_or((0.0, 0.0, false), |(x, y)| (x, y, true));
                        let _ = plugin.run_mobile_plugin::<serde_json::Value>("cursor", serde_json::json!({ "x": x, "y": y, "show": show }));
                    })))
                };
                #[cfg(not(target_os = "android"))]
                let engine: Arc<dyn SpatialPlatformEngine> = platform::engine().into();
                app.manage(engine.clone());
                spawn_kvm_receiver(handle.clone(), engine, state.clone(), tls);
            }
            #[cfg(target_os = "windows")]
            install_send_to();
            #[cfg(desktop)]
            if std::env::args().any(|a| a == "--sim-mobile") {
                sim::start(&handle, &state)?;
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building LOKA");
    app.run(|app, e| {
        // macOS: files dropped on the Dock icon or opened with LOKA from Finder
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Opened { urls } = &e {
            receive_outbox(app, urls.iter().filter_map(|u| u.to_file_path().ok()).collect());
        }
        if let tauri::RunEvent::Exit = e {
            journal!("LOKA exiting");
        }
        let _ = (app, e);
    });
}

fn spawn_discovery(app: AppHandle, state: AppState, me: watch::Receiver<DeviceInfo>) {
    tauri::async_runtime::spawn(async move {
        let hosts_state = state.clone();
        let hosts = move || hosts_state.lock().unwrap().settings.hosts.clone();
        let r = discovery::run(me, hosts, |peer| {
            let previous = {
                let mut s = state.lock().unwrap();
                // remember paired devices' hardware addresses while they're awake, to wake them later
                if let (Some(mac), true) = (&peer.info.mac, s.settings.is_paired(&peer.info.fingerprint)) {
                    if s.settings.macs.get(&peer.info.fingerprint) != Some(mac) {
                        s.settings.macs.insert(peer.info.fingerprint.clone(), mac.clone());
                        s.settings.save();
                    }
                }
                s.peers.insert(peer.info.fingerprint.clone(), peer.clone())
            };
            // the tray's "Control …" follows whether it can be driven (first sight included), not every heartbeat
            if previous.as_ref().map(|p| p.info.controllable) != Some(peer.info.controllable) {
                refresh_tray(&app);
            }
            // heartbeats are frequent; only tell the UI about arrivals and changes
            if previous.map_or(true, |p| p.info != peer.info || p.addr != peer.addr || p.seen.elapsed() >= discovery::STALE) {
                let _ = app.emit("peer", peer);
            }
        })
        .await;
        if let Err(e) = r {
            journal!("discovery stopped: {e}");
        }
    });
}

fn spawn_transfer_server(app: &AppHandle, state: AppState, tls: Arc<Tls>) -> tauri::Result<()> {
    let a = app.clone();
    let dir = move || received_dir(&a);
    let clip_dir = app.path().app_cache_dir()?.join("clipboard");
    let _ = std::fs::remove_dir_all(&clip_dir); // last run's clipboard files are no longer on any clipboard
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        // paired devices only, recognised by certificate
        let allow = move |fp: &str| state.lock().unwrap().settings.is_paired(fp);
        let clip_root = clip_dir.clone();
        let r = transfer::serve(tls, PORTS.1, dir, clip_dir, allow, move |e| {
            match e {
                #[cfg(desktop)]
                transfer::TransferEvent::ClipImage { png } => {
                    tauri::async_runtime::spawn_blocking(move || clip::apply_image(&png));
                    return;
                }
                transfer::TransferEvent::ClipFiles { roots, batch } if batch.drag => {
                    let down = h.try_state::<Arc<dyn SpatialPlatformEngine>>().and_then(|e| e.cursor()).is_some_and(|c| c.button_down);
                    let step = h.state::<AppState>().lock().unwrap().carry.files(batch.id, roots, down);
                    carry_out(&h, step);
                    return;
                }
                transfer::TransferEvent::Manifest { batch, files } => {
                    let down = h.try_state::<Arc<dyn SpatialPlatformEngine>>().and_then(|e| e.cursor()).is_some_and(|c| c.button_down);
                    let step = h.state::<AppState>().lock().unwrap().carry.manifest(batch.id, files, down);
                    carry_out(&h, step);
                    return;
                }
                #[cfg(desktop)]
                transfer::TransferEvent::ClipFiles { roots, .. } => {
                    let root = clip_root.clone();
                    tauri::async_runtime::spawn_blocking(move || {
                        clip::apply_files(&roots);
                        // only the newest copy is on the clipboard; earlier batches can go (drags in flight stay)
                        let keep = roots.first().and_then(|p| p.strip_prefix(&root).ok()?.components().next().map(|c| root.join(c)));
                        for old in std::fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()) {
                            if Some(&old) != keep.as_ref() && !old.file_name().is_some_and(|n| n.to_string_lossy().starts_with("drag-")) {
                                let _ = std::fs::remove_dir_all(old);
                            }
                        }
                    });
                    return;
                }
                // one notification per burst, not one per file of a dropped folder; none for clipboard files
                transfer::TransferEvent::File { ref path, .. } if !path.starts_with(&clip_root) => {
                    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
                    let mut last = LAST.lock().unwrap();
                    let new_burst = last.is_none_or(|t| t.elapsed() > Duration::from_secs(3));
                    {
                        let state = h.state::<AppState>();
                        let mut s = state.lock().unwrap();
                        if new_burst {
                            s.recent.clear();
                        }
                        s.recent.push(path.clone());
                    }
                    if new_burst {
                        use tauri_plugin_notification::NotificationExt;
                        let (name, folder) = (transfer::file_name(path), transfer::file_name(&received_dir(&h)));
                        let _ = h.notification().builder().title("LOKA").body(format!("{name} saved to {folder} · Ctrl+Alt+V pastes it")).show();
                    }
                    *last = Some(std::time::Instant::now());
                }
                _ => {}
            }
            let _ = h.emit("transfer", e);
        })
        .await;
        if let Err(e) = r {
            journal!("transfer server stopped: {e}");
        }
    });
    Ok(())
}

/// Send files to device `fp` at `to` 4 at a time like LocalSend's parallel uploads (small files don't
/// queue behind each other's handshakes), reporting progress as `transfer` events.
async fn send_files(app: AppHandle, to: SocketAddr, fp: String, files: Vec<(PathBuf, String)>, clip: Option<transfer::ClipBatch>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    for (path, name) in files {
        let (h, fp, slot) = (app.clone(), fp.clone(), slots.clone().acquire_owned().await.unwrap());
        tauri::async_runtime::spawn(async move {
            let id = transfer::next_id();
            let tls = h.state::<Arc<Tls>>().inner().clone();
            let r = transfer::send_file(&tls, to, &fp, &path, name.clone(), clip, id, |done, total| {
                let _ = h.emit("transfer", transfer::TransferEvent::Progress { id, outgoing: true, name: name.clone(), done, total });
            })
            .await;
            drop(slot);
            let _ = h.emit("transfer", match r {
                Ok(()) => transfer::TransferEvent::Sent { id },
                Err(e) => transfer::TransferEvent::Failed { id, outgoing: true, name, error: explain(&h, &fp, &e) },
            });
        });
    }
}

/// Hand this computer's clipboard picture or files to device `fp` (where the cursor is going), if they
/// changed. Runs beside the crossing, never delaying it.
#[cfg(desktop)]
fn sync_clipboard(app: AppHandle, to: SocketAddr, fp: String) {
    tauri::async_runtime::spawn(async move {
        match tokio::task::spawn_blocking(clip::outgoing).await.ok().flatten() {
            Some(clip::Outgoing::Image(png)) => {
                let tls = app.state::<Arc<Tls>>().inner().clone();
                if let Err(e) = transfer::send_bytes(&tls, to, &fp, transfer::Kind::Image, &png).await {
                    journal!("clipboard picture: {e}");
                }
            }
            Some(clip::Outgoing::Files(files)) => {
                let batch = transfer::ClipBatch { id: uuid::Uuid::new_v4().as_u128() as u64, files: files.len() as u32, drag: false };
                send_files(app, to, fp, files, Some(batch)).await;
            }
            None => {}
        }
    });
}

/// Hand an OS file drag hovering the edge to device `fp`: end it here (a marked button-up drops it on
/// our own overlay, ignored as `handoff`), announce its files (manifest) and send them as drag batch,
/// and return the batch for the cursor to carry along.
fn hand_off_drag(app: &AppHandle, engine: &Arc<dyn SpatialPlatformEngine>, state: &AppState, to: SocketAddr, fp: String) -> Option<u64> {
    let paths = {
        let mut s = state.lock().unwrap();
        let paths = s.hover.take()?;
        s.handoff = true;
        paths
    };
    engine.inject(Injected::Button { button: 1, down: false });
    let id = uuid::Uuid::new_v4().as_u128() as u64;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let files = tokio::task::spawn_blocking(move || transfer::expand(&paths)).await.unwrap_or_default();
        let batch = transfer::ClipBatch { id, files: files.len() as u32, drag: true };
        let tls = app.state::<Arc<Tls>>().inner().clone();
        // without it the other side just waits for every file
        if let Err(e) = transfer::send_manifest(&tls, to, &fp, batch, &files).await {
            journal!("drag manifest: {e}");
        }
        send_files(app, to, fp, files, Some(batch)).await;
    });
    Some(id)
}

/// The cursor arrived with drag `batch`, its button still held: hold it on this OS too (pressed on our own
/// overlay `window`, which sits under the cursor at the edge) until the drag can start.
fn hold_drag(app: &AppHandle, window: &str, batch: u64) {
    let engine = app.state::<Arc<dyn SpatialPlatformEngine>>().inner().clone();
    if let Some(w) = app.get_webview_window(window) {
        let _ = engine.set_click_through(&w, false);
    }
    engine.inject(Injected::Button { button: 1, down: true });
    let step = {
        let state = app.state::<AppState>();
        let mut s = state.lock().unwrap();
        s.carry_window = window.into();
        s.carry.held(batch)
    };
    carry_out(app, step);
}

fn carry_out(app: &AppHandle, step: carry::Step) {
    match step {
        #[cfg(mobile)]
        carry::Step::Start(files) => carry_out(app, carry::Step::Save(files)),
        #[cfg(desktop)]
        carry::Step::Start(files) => {
            let label = app.state::<AppState>().lock().unwrap().carry_window.clone();
            let Some(w) = app.get_webview_window(&label) else { return };
            // AppKit/OLE drag sessions belong on the main thread
            let _ = app.run_on_main_thread(move || {
                let icon = drag::Image::Raw(include_bytes!("../icons/64x64.png").to_vec());
                #[cfg(target_os = "linux")]
                let Ok(w) = w.gtk_window() else { return };
                if let Err(e) = drag::start_drag(&w, drag::DragItem::Files(files), icon, |_, _| {}, Default::default()) {
                    journal!("drag: {e}");
                }
            });
        }
        carry::Step::Stream(batch, files) => {
            let label = app.state::<AppState>().lock().unwrap().carry_window.clone();
            let Some(w) = app.get_webview_window(&label) else { return };
            let _ = app.run_on_main_thread(move || {
                if let Err(e) = carry::stream_drag(&w, batch, files) {
                    journal!("drag: {e}");
                }
            });
        }
        carry::Step::Save(files) => {
            let dir = received_dir(app);
            let _ = std::fs::create_dir_all(&dir);
            for f in &files {
                let name = transfer::file_name(f);
                let dest = (0..1000).map(|i| if i == 0 { dir.join(&name) } else { dir.join(format!("{name} ({i})")) }).find(|p| !p.exists());
                // shortcut: rename only; cache and received folder on different volumes leave the files in the cache
                if let Some(Err(e)) = dest.map(|d| std::fs::rename(f, d)) {
                    journal!("saving dragged {name}: {e}");
                }
            }
            use tauri_plugin_notification::NotificationExt;
            let folder = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
            let _ = app.notification().builder().title("LOKA").body(format!("Let go before the drag could start: saved to {folder}")).show();
        }
        carry::Step::Wait => {}
    }
}

#[cfg(desktop)]
const TRAY: &str = "loka";

#[cfg(desktop)]
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let mut tray = TrayIconBuilder::with_id(TRAY)
        .tooltip("LOKA")
        .menu(&tray_menu(app.handle())?)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, e| match e.id.as_ref() {
            "open" => show_main(app),
            "about" => {
                show_main(app);
                let _ = app.emit_to(MAIN_WINDOW, "about", ());
            }
            "disconnect" => {
                journal!("Disconnect from the tray");
                kvm::end_sessions()
            }
            "quit" => {
                journal!("quit from the tray");
                app.exit(0)
            }
            "autostart" => {
                let on = {
                    let state = app.state::<AppState>();
                    let mut s = state.lock().unwrap();
                    s.settings.manual_start = !s.settings.manual_start;
                    s.settings.save();
                    !s.settings.manual_start
                };
                set_autostart(app, on);
                refresh_tray(app);
            }
            id => {
                if let Some(fp) = id.strip_prefix("control:") {
                    let (app, fp) = (app.clone(), fp.to_string());
                    tauri::async_runtime::spawn(async move {
                        if let Err(error) = control(app.clone(), fp).await {
                            let _ = app.emit("kvm", serde_json::json!({ "active": false, "error": error }));
                        }
                    });
                }
            }
        })
        .on_tray_icon_event(move |tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// Open, "Control <device>" for every placed paired device (switch to it without the edge), Disconnect,
/// About, Quit.
#[cfg(desktop)]
fn tray_menu(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(app, "open", "Open LOKA", true, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let (devices, busy): (Vec<(String, String, bool)>, bool) = {
        let state = app.state::<AppState>();
        let s = state.lock().unwrap();
        let devices = s
            .settings
            .paired
            .iter()
            .filter(|d| s.settings.portals.contains_key(&d.fingerprint))
            .map(|d| (d.fingerprint.clone(), d.alias.clone(), s.peers.get(&d.fingerprint).map_or(true, |p| p.info.controllable)))
            .collect();
        (devices, s.session.is_some())
    };
    for (fp, alias, controllable) in devices {
        let text = if controllable { format!("Control {alias}") } else { format!("Control {alias} (finish setup there)") };
        menu.append(&MenuItem::with_id(app, format!("control:{fp}"), text, controllable, None::<&str>)?)?;
    }
    // only while another device drives this one: while driving, this computer's own mouse is over there,
    // so the tray can't be reached and Ctrl+Alt+Esc is the way out
    menu.append(&MenuItem::with_id(app, "disconnect", "Disconnect", busy, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let autostart = !app.state::<AppState>().lock().unwrap().settings.manual_start;
    menu.append(&tauri::menu::CheckMenuItem::with_id(app, "autostart", "Start with computer", true, autostart, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "about", "About LOKA", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit LOKA", true, None::<&str>)?)?;
    Ok(menu)
}

/// LOKA starts (hidden, in the tray) when the user logs in: on unless they turned it off in the tray.
#[cfg(desktop)]
fn set_autostart(app: &AppHandle, on: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let launch = app.autolaunch();
    if launch.is_enabled().ok() == Some(on) {
        return;
    }
    match if on { launch.enable() } else { launch.disable() } {
        Ok(()) => journal!("start with computer: {on}"),
        Err(e) => journal!("start with computer ({on}) failed: {e}"),
    }
}

/// Pairings or portals changed: the tray's device list follows.
fn refresh_tray(app: &AppHandle) {
    #[cfg(desktop)]
    if let (Some(tray), Ok(menu)) = (app.tray_by_id(TRAY), tray_menu(app)) {
        let _ = tray.set_menu(Some(menu));
    }
    let _ = app;
}

/// The band (for being driven) and one overlay per saved portal; then the receiver and the tracker.
#[cfg(desktop)]
fn setup_overlays(app: &mut tauri::App, state: AppState, tls: Arc<Tls>) -> tauri::Result<()> {
    let engine: Arc<dyn SpatialPlatformEngine> = platform::engine().into();
    app.manage(engine.clone());
    let h = app.handle().clone();
    open_overlay(&h, BAND, None, Edge::Left)?;
    let portals: Vec<(String, Portal)> = {
        let s = state.lock().unwrap();
        s.settings.portals.iter().filter(|(fp, _)| s.settings.is_paired(fp)).map(|(f, p)| (f.clone(), *p)).collect()
    };
    for (fp, p) in portals {
        let label = portal_label(&fp);
        open_overlay(&h, &label, Some(fp), p.edge)?;
        place_overlay(&h, &label, &p)?;
    }
    spawn_kvm_receiver(h.clone(), engine.clone(), state.clone(), tls);
    tauri::async_runtime::spawn(track(h, engine, state));
    Ok(())
}

/// This computer's screens, in the canonical space.
fn screens(app: &AppHandle) -> Vec<Rect> {
    let monitors = app.available_monitors().unwrap_or_default();
    monitors.iter().map(|m| Rect { x: m.position().x as f64, y: m.position().y as f64, w: m.size().width as f64, h: m.size().height as f64 }).collect()
}

/// Put paired device `fp` on `edge` here (`None`: off this computer's screens), where that edge still has
/// room; one already on that edge stays where it is. `false`: not paired, or no room.
#[cfg(desktop)]
fn mirror_portal(app: &AppHandle, fp: &str, edge: Option<Edge>) -> bool {
    let screens = screens(app);
    let portal = {
        let state = app.state::<AppState>();
        let mut s = state.lock().unwrap();
        if !s.settings.is_paired(fp) {
            return false;
        }
        let portal = match edge {
            Some(e) if s.settings.portals.get(fp).is_some_and(|p| p.edge == e) => return true,
            Some(e) => {
                let taken: Vec<Portal> = s.settings.portals.iter().filter(|(f, _)| *f != fp).map(|(_, p)| *p).collect();
                let Some(p) = platform::fit(&screens, e, &taken) else { return false };
                s.settings.portals.insert(fp.to_string(), p);
                Some(p)
            }
            None => {
                s.settings.portals.remove(fp);
                None
            }
        };
        s.settings.save();
        portal
    };
    let (app, fp) = (app.clone(), fp.to_string());
    tauri::async_runtime::spawn(async move {
        drop(show_portal(&app, &fp, portal));
        let _ = app.emit("device", ()); // the layout map follows
    });
    true
}

/// A phone: paired device `fp` sits along the whole `edge` of its screen (`None`: taken off). `false`: not paired.
#[cfg(mobile)]
fn mirror_portal(app: &AppHandle, fp: &str, edge: Option<Edge>) -> bool {
    let monitor = screens(app).first().copied().unwrap_or(Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 });
    {
        let state = app.state::<AppState>();
        let mut s = state.lock().unwrap();
        if !s.settings.is_paired(fp) {
            return false;
        }
        match edge {
            Some(edge) => s.settings.portals.insert(fp.to_string(), Portal { edge, monitor, start: 0.0, end: 1.0, reach: 120.0 }),
            None => s.settings.portals.remove(fp),
        };
        s.settings.save();
    }
    let _ = app.emit("device", ());
    true
}

/// Answers pairing requests and accepts input sessions from paired devices.
fn spawn_kvm_receiver(app: AppHandle, engine: Arc<dyn SpatialPlatformEngine>, state: AppState, tls: Arc<Tls>) {
    tauri::async_runtime::spawn(async move {
        let (st, e) = (state.clone(), engine.clone());
        let allow = move |fp: &str| match (st.lock().unwrap().settings.is_paired(fp), e.can_inject()) {
            (false, _) => Err(kvm::Refusal::NotPaired),
            (_, false) => Err(kvm::Refusal::NoInput),
            _ => Ok(()),
        };
        let (a, me) = (app.clone(), app.state::<watch::Sender<DeviceInfo>>().inner().clone());
        tauri::async_runtime::spawn(watch_controllable(engine.clone(), me, move || drop(a.emit("device", ()))));
        let a = app.clone();
        // the other device moved this one to its `edge` of its screens: it goes on the opposite edge here;
        // or it unpaired: forget it here too
        let on_place = move |fp: &str, l: kvm::Layout| match l {
            kvm::Layout::Place(edge) => mirror_portal(&a, fp, edge.map(Edge::opposite)),
            kvm::Layout::Unpair => {
                forget(&a, fp);
                true
            }
        };
        let (a, st) = (app.clone(), state.clone());
        let ask = move |_: IpAddr, fp: String, code: String| {
            let (app, state) = (a.clone(), st.clone());
            async move {
                // the fingerprint is proven by the connection; its address may differ from the one discovery
                // heard (several interfaces, VPN, vEthernet), and a peer a few heartbeats stale still counts
                let from = state.lock().unwrap().peers.get(&fp).cloned();
                let Some(from) = from else { return kvm::PairAnswer::Unknown };
                // a prompt hidden in the tray would just time out
                if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
                    #[cfg(desktop)]
                    let _ = w.unminimize();
                    let _ = w.show();
                }
                {
                    use tauri_plugin_notification::NotificationExt;
                    let _ = app.notification().builder().title("LOKA").body(format!("{} wants to pair · open LOKA to answer", from.info.alias)).show();
                }
                let answer = ask_user(&app, &state, MAIN_WINDOW, &from, code).await;
                if answer == kvm::PairAnswer::Accepted {
                    state.lock().unwrap().settings.pair(PairedDevice { fingerprint: fp, alias: from.info.alias.clone() });
                    let _ = app.emit("paired", ());
                    refresh_tray(&app);
                }
                answer
            }
        };
        let reporter = app.clone();
        // the phone's whole screen; no band or portal windows
        #[cfg(mobile)]
        let make = move |fp: &str, edge: Edge| {
            let screens = screens(&app);
            let monitor = *screens.first()?;
            let band = Portal { edge, monitor, start: 0.0, end: 1.0, reach: 120.0 };
            let state = app.state::<AppState>();
            let mut s = state.lock().unwrap();
            journal!("driven by {}, entering at the {edge:?} edge", s.who(fp).0);
            s.session = Some((fp.to_string(), band));
            drop(s);
            engine.show_cursor(true);
            Some(EngineSink { engine: engine.clone(), screens, app: app.clone() })
        };
        #[cfg(desktop)]
        let make = move |fp: &str, edge: Edge| {
            let screens = screens(&app);
            if screens.is_empty() {
                return None;
            }
            // the whole entry edge leads back to the driver: files dragged there and dropped go to it (reach = Calibrate's REACH)
            let band = Portal { edge, monitor: kvm::bounds(&screens), start: 0.0, end: 1.0, reach: 120.0 };
            let placed = {
                let state = app.state::<AppState>();
                let mut s = state.lock().unwrap();
                s.session = Some((fp.to_string(), band));
                journal!("driven by {}, entering at the {edge:?} edge of {screens:?}", s.who(fp).0);
                s.settings.portals.contains_key(fp)
            };
            // no way to the driver yet: it goes on the entry edge, editable in the layout
            if !placed {
                mirror_portal(&app, fp, Some(edge));
            }
            refresh_tray(&app);
            let _ = app.emit_to(BAND, "portal", edge);
            let _ = place_overlay(&app, BAND, &band);
            engine.show_cursor(true);
            Some(EngineSink { engine: engine.clone(), screens, app: app.clone() })
        };
        let st = state.clone();
        let allow_service = move |service: kvm::Service, fp: &str| {
            let s = st.lock().unwrap();
            let sh = &s.settings.sharing;
            s.settings.is_paired(fp)
                && match service {
                    kvm::Service::Shell => cfg!(desktop) && sh.terminal,
                    kvm::Service::Files => cfg!(desktop) && sh.folder.is_some(),
                    kvm::Service::Sound => cfg!(desktop) && sh.sound,
                    kvm::Service::Notifications => sh.notifications,
                    kvm::Service::Mic => cfg!(desktop) && sh.mic,
                    kvm::Service::Diagnostics => true,
                }
        };
        let st = state.clone();
        let on_service = move |service: kvm::Service, _fp: String, mut stream: secure::Stream| {
            let folder = st.lock().unwrap().settings.sharing.folder.clone();
            let reporter = reporter.clone();
            tauri::async_runtime::spawn(async move {
                let r = match service {
                    kvm::Service::Diagnostics => {
                        use tokio::io::AsyncWriteExt;
                        let text = report(&reporter);
                        match stream.write_all(text.as_bytes()).await {
                            Ok(()) => stream.shutdown().await,
                            Err(e) => Err(e),
                        }
                    }
                    #[cfg(desktop)]
                    kvm::Service::Shell => shell::serve(stream).await,
                    #[cfg(desktop)]
                    kvm::Service::Files => match folder {
                        Some(root) => share::serve(stream, root).await,
                        None => Ok(()),
                    },
                    #[cfg(desktop)]
                    kvm::Service::Sound => audio::serve(stream, false).await,
                    #[cfg(desktop)]
                    kvm::Service::Mic => audio::serve(stream, true).await,
                    #[cfg(mobile)]
                    kvm::Service::Shell | kvm::Service::Files | kvm::Service::Sound | kvm::Service::Mic => Ok(drop((stream, folder))),
                    kvm::Service::Notifications => notify::serve(stream).await,
                };
                if let Err(e) = r {
                    journal!("{service:?}: {e}");
                }
            });
        };
        if let Err(e) = kvm::serve(tls, PORTS.2, allow, ask, allow_service, on_service, on_place, make).await {
            journal!("kvm receiver stopped: {e}");
        }
    });
}

/// A portal (or the band) the cursor can reach this tick.
struct Door {
    label: String,
    /// device behind it
    fp: String,
    portal: Portal,
    /// the band only takes drops and dragged files back; crossing out of it is the receiver's job
    band: bool,
}

/// Hand mouse & keyboard to device `fp` through `portal` (overlay `label`) at `y` along it, with a file
/// drag going along if `drag`. Runs the session in the background.
fn start_session(app: &AppHandle, label: String, fp: String, portal: Portal, y: f32, drag: Option<u64>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let (to, transfer_to, prefs, peer) = {
        let s = state.lock().unwrap();
        let to = s.addr(&fp, |i| i.kvm_port).ok_or("that device isn't on the network")?;
        let peer = s.peers.get(&fp);
        // it says it can't be driven (phone not set up, Linux without input access): don't even try
        if let Some(p) = peer.filter(|p| !p.info.controllable) {
            return Err(kvm::not_controllable(&p.info.alias, &p.info.os));
        }
        (to, s.addr(&fp, |i| i.transfer_port), s.settings.cursor, peer.map(|p| (p.info.os.clone(), p.info.text)))
    };
    if DRIVING.swap(true, Ordering::SeqCst) {
        return Err("already controlling another device".into());
    }
    #[cfg(desktop)]
    if let Some(t) = transfer_to {
        sync_clipboard(app.clone(), t, fp.clone());
    }
    let (os, text) = peer.unwrap_or_default();
    // an Android keymap is US only: characters go as text in this keyboard's layout; computers map keys themselves
    let layout = platform::key_text().filter(|_| os == "android");
    let tune = kvm::Tune::new(prefs.speed, prefs.swap_ctrl_cmd && (os == "macos") != cfg!(target_os = "macos")).text(text, layout);
    let (app, engine) = (app.clone(), app.state::<Arc<dyn SpatialPlatformEngine>>().inner().clone());
    tauri::async_runtime::spawn(async move {
        let _ = app.emit("kvm", serde_json::json!({ "active": true, "y": y, "device": fp }));
        let alias = app.state::<AppState>().lock().unwrap().who(&fp).0;
        journal!("crossing to {alias} ({to}) at {y:.2} along its {:?} portal{}", portal.edge, if drag.is_some() { ", carrying a file drag" } else { "" });
        let tls = app.state::<Arc<Tls>>().inner().clone();
        let r = kvm::send_session(&tls, engine, to, &fp, portal, y, tune, drag).await;
        match &r {
            Ok(_) => journal!("back from {alias}"),
            Err(e) => journal!("session with {alias} ended: {e} ({:?})", e.kind()),
        }
        if let Ok(Some(batch)) = r {
            hold_drag(&app, &label, batch); // a drag came back with the cursor
        }
        let error = r.as_ref().err().map(|e| explain(&app, &fp, e));
        let _ = app.emit("kvm", serde_json::json!({ "active": false, "error": error }));
        // it unpaired while this one was away (its certificate is pinned, so it really is that device)
        if r.as_ref().err().and_then(|e| e.get_ref()?.downcast_ref::<kvm::Refusal>()) == Some(&kvm::Refusal::NotPaired) {
            forget(&app, &fp);
        }
        if let Some(e) = &error {
            crossing_failed(&app, e.clone());
        }
        if error.is_some() {
            tokio::time::sleep(Duration::from_secs(1)).await; // no reconnect storm at the edge
        }
        DRIVING.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// The last crossing that failed and when, for `diagnostics`.
static LAST_CROSSING: Mutex<Option<(String, std::time::Instant)>> = Mutex::new(None);

/// `troubleshoot`'s live test: while set, the tracker sees the cursor at this point (button up) instead of the real one.
#[cfg(desktop)]
static PROBE: Mutex<Option<(f64, f64)>> = Mutex::new(None);

/// Every 2 s: logs what changed (devices heard, screens, cursor lock), follows screen changes with the
/// portals, and clears a driven session whose loop stopped running, so a stuck one can't hold the cursor.
fn spawn_watchdog(app: AppHandle, state: AppState) {
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let (mut heard, mut shown, mut lock) = (HashMap::<String, (String, IpAddr)>::new(), Vec::<Rect>::new(), false);
        loop {
            tick.tick().await;
            if kvm::beat_age() > Duration::from_secs(10) && state.lock().unwrap().session.is_some() {
                let age = kvm::beat_age().as_secs();
                if let Some(by) = free_driven(&app, &state) {
                    journal!("watchdog: cleared the session driven by {by}: its loop hasn't run for {age} s");
                }
            }
            let now: HashMap<String, (String, IpAddr)> = state.lock().unwrap().live_peers().map(|p| (p.info.fingerprint.clone(), (p.info.alias.clone(), p.addr))).collect();
            for (fp, (alias, addr)) in &now {
                if heard.get(fp).is_none_or(|(_, a)| a != addr) {
                    journal!("heard {alias} at {addr}");
                }
            }
            for (fp, (alias, _)) in &heard {
                if !now.contains_key(fp) {
                    journal!("lost {alias}: not heard for {} s", discovery::STALE.as_secs());
                }
            }
            heard = now;
            if kvm::LOCKED.load(Ordering::SeqCst) != lock {
                lock = !lock;
                journal!("cursor lock {}", if lock { "on" } else { "off" });
            }
            #[cfg(desktop)]
            {
                let screens = screens(&app);
                if screens.is_empty() || screens == shown {
                    continue;
                }
                journal!("screens: {screens:?}");
                shown = screens.clone();
                let (moved, lost): (Vec<(String, String, Portal)>, Vec<String>) = {
                    let mut s = state.lock().unwrap();
                    let moved: Vec<(String, Portal)> = s
                        .settings
                        .portals
                        .iter_mut()
                        .filter_map(|(fp, p)| {
                            p.monitor = platform::refit(p.monitor, &screens)?;
                            Some((fp.clone(), *p))
                        })
                        .collect();
                    if !moved.is_empty() {
                        s.settings.save();
                    }
                    let lost = s.settings.portals.iter().filter(|(_, p)| !screens.contains(&p.monitor)).map(|(fp, _)| s.who(fp).0).collect();
                    (moved.into_iter().map(|(fp, p)| (s.who(&fp).0, fp, p)).collect(), lost)
                };
                for (alias, fp, p) in &moved {
                    journal!("portal to {alias} follows its screen: now {:?}", p.monitor);
                    let _ = place_overlay(&app, &portal_label(fp), p);
                }
                for alias in lost {
                    journal!("portal to {alias} is on a screen that isn't there now: it works again once that screen is back, or place it again");
                }
                if !moved.is_empty() {
                    let _ = app.emit("device", ());
                }
            }
            #[cfg(mobile)]
            let _ = &mut shown;
        }
    });
}

/// The reason a crossing failed, as a notification: the main window showing it is usually hidden.
/// The same reason again within 30 s stays quiet (the cursor keeps pushing the edge).
fn crossing_failed(app: &AppHandle, error: String) {
    use tauri_plugin_notification::NotificationExt;
    let mut last = LAST_CROSSING.lock().unwrap();
    if last.as_ref().is_some_and(|(e, at)| *e == error && at.elapsed() < Duration::from_secs(30)) {
        return;
    }
    journal!("couldn't cross: {error}");
    let _ = app.notification().builder().title("LOKA couldn't move over").body(&error).show();
    *last = Some((error, std::time::Instant::now()));
}

/// 60 Hz loop: cursor → portal samples → click-through toggle, local glow event, UDP telemetry, crossings.
#[cfg(desktop)]
async fn track(app: AppHandle, engine: Arc<dyn SpatialPlatformEngine>, state: AppState) {
    let Ok(sock) = telemetry::sender().await else { return };
    let mut tick = tokio::time::interval(Duration::from_micros(16_667));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // overlay currently catching input; overlay + device whose glow is lit
    let (mut interactive, mut streaming) = (None::<String>, None::<(String, String)>);
    let remote = &DRIVING;
    // cursor lock key edge, last lock state shown, since when the cursor rests on a portal edge
    let (mut lock_key, mut shown_lock, mut resting) = (false, false, None::<std::time::Instant>);
    // overlay the cursor is near (its `hover` events)
    let mut hovering = None::<String>;
    // why the cursor at an edge doesn't cross (device, reason), logged once each time it changes
    let mut stuck = None::<(String, &str)>;
    loop {
        tick.tick().await;
        let (doors, prefs) = {
            let s = state.lock().unwrap();
            let doors: Vec<Door> = match &s.session {
                // being driven: only the way back, never on to a third device
                Some((fp, band)) => vec![Door { label: BAND.into(), fp: fp.clone(), portal: *band, band: true }],
                None => s
                    .settings
                    .portals
                    .iter()
                    .filter(|(fp, _)| s.settings.is_paired(fp))
                    .map(|(fp, p)| Door { label: portal_label(fp), fp: fp.clone(), portal: *p, band: false })
                    .collect(),
            };
            (doors, s.settings.cursor)
        };
        if doors.is_empty() {
            continue;
        }
        let driving = remote.load(Ordering::SeqCst);
        if !doors[0].band {
            // at home the lock key is read here; while driving, the capture hook swallows it and Tune toggles
            let down = engine.lock_key_down();
            if down && !lock_key && !driving {
                kvm::LOCKED.fetch_xor(true, Ordering::SeqCst);
            }
            lock_key = down;
        }
        let locked = kvm::LOCKED.load(Ordering::SeqCst);
        if locked != shown_lock {
            shown_lock = locked;
            let _ = app.emit("lock", locked);
        }
        let probe = PROBE.lock().unwrap().map(|(x, y)| platform::CursorSample { x, y, button_down: false });
        let Some(c) = probe.or_else(|| engine.cursor()) else { continue };
        if driving {
            continue;
        }
        {
            // the button of a carried drag went up before its files came: they get saved instead
            let mut s = state.lock().unwrap();
            if !c.button_down && s.carry.waiting() {
                s.carry.released();
            }
        }
        let hit = doors.iter().find_map(|d| d.portal.sample(c.x, c.y).map(|(y, p)| (d, y, p)));
        // within a pixel: a Mac reports points, so ×2 on Retina its cursor can stop a pixel short of the right/bottom edge
        let at_edge = hit.is_some_and(|(d, _, p)| (1.0 - p as f64) * d.portal.reach < 1.5);
        let on_edge = at_edge && !locked;
        let why = hit.filter(|_| at_edge).and_then(|(d, ..)| {
            let reason = if d.band {
                "the way back while it drives this computer: not crossing from here"
            } else if locked {
                "not crossing: cursor lock is on (Scroll Lock / F14)"
            } else if c.button_down && state.lock().unwrap().hover.is_none() {
                "not crossing: a button is held (crosses with the button up, or with a file drag)"
            } else {
                return None;
            };
            Some((d.fp.clone(), reason))
        });
        if why != stuck {
            if let Some((fp, reason)) = &why {
                let (alias, driven) = {
                    let s = state.lock().unwrap();
                    (s.who(fp).0, s.session.is_some())
                };
                let beat = if driven { format!(" (session loop ran {} ms ago)", kvm::beat_age().as_millis()) } else { String::new() };
                journal!("cursor at {alias}'s edge, {reason}{beat}");
            }
            stuck = why;
        }
        if let (true, Some((door, y, _))) = (on_edge, hit) {
            // a file drag pushed against the edge goes over with the cursor, or back with it (band)
            let mut drag = None;
            if c.button_down && state.lock().unwrap().hover.is_some() {
                let to = state.lock().unwrap().addr(&door.fp, |i| i.transfer_port);
                if let Some(to) = to {
                    drag = hand_off_drag(&app, &engine, &state, to, door.fp.clone());
                    if let (Some(batch), true) = (drag, door.band) {
                        kvm::DRAG_OUT.store(batch, Ordering::SeqCst); // rides back with the next push through the edge
                        continue;
                    }
                }
            }
            // resting against the edge (no drag, not locked) hands mouse & keyboard to the device behind it
            if let (false, true) = (door.band, !c.button_down || drag.is_some()) {
                let since = *resting.get_or_insert_with(std::time::Instant::now);
                if drag.is_some() || since.elapsed() >= Duration::from_millis(prefs.edge_delay_ms.into()) {
                    resting = None;
                    if let Err(e) = start_session(&app, door.label.clone(), door.fp.clone(), door.portal, y, drag) {
                        if !DRIVING.load(Ordering::SeqCst) {
                            crossing_failed(&app, e.to_string());
                        }
                    }
                }
                continue;
            }
        } else {
            resting = None;
        }
        // near a portal, button or not: its overlay may be hidden until then (glow auto-hide); local only
        let near = hit.map(|(d, y, p)| (d.label.clone(), y, p));
        if let Some((label, y, p)) = &near {
            let _ = app.emit_to(label, "hover", (y, p));
        }
        if let Some(gone) = hovering.take().filter(|l| near.as_ref().is_none_or(|(n, ..)| n != l)) {
            let _ = app.emit_to(&gone, "hover", (0.5, 0.0));
        }
        hovering = near.map(|(l, ..)| l);
        // only a drag (button held) inside a portal may make its overlay catch input
        let catching = hit.filter(|_| c.button_down);
        let want = catching.map(|(d, _, _)| d.label.clone());
        if want != interactive {
            for (label, ignore) in [(interactive.as_ref(), true), (want.as_ref(), false)] {
                if let Some(w) = label.and_then(|l| app.get_webview_window(l)) {
                    let _ = engine.set_click_through(&w, ignore);
                }
            }
            interactive = want;
        }
        let (packet, lit) = match catching {
            Some((d, y, p)) => {
                state.lock().unwrap().last_y = y;
                (SpatialPacket { kind: PacketType::Proximity, normalized_y: y, proximity: p, payload: PayloadKind::None }, Some((d.label.clone(), d.fp.clone(), d.band)))
            }
            None => match streaming.take() {
                Some((label, fp)) => (
                    SpatialPacket { kind: PacketType::Leave, normalized_y: state.lock().unwrap().last_y, proximity: 0.0, payload: PayloadKind::None },
                    Some((label, fp, true)), // `true`: no telemetry needed past this Leave's own send below
                ),
                None => continue,
            },
        };
        let Some((label, fp, band)) = lit else { continue };
        if packet.kind == PacketType::Proximity {
            streaming = Some((label.clone(), fp.clone()));
        }
        let _ = app.emit_to(&label, "spatial", packet);
        // the device behind a portal glows too; a band's driver has no glow for it
        let to = if band && packet.kind == PacketType::Proximity { None } else { state.lock().unwrap().addr(&fp, |i| i.telemetry_port) };
        if let Some(to) = to {
            telemetry::send(&sock, to, &packet).await;
        }
    }
}
