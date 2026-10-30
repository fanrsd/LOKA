mod audio;
mod carry;
mod clip;
mod kvm;
mod network;
mod platform;
mod notify;
mod settings;
mod share;
mod shell;
mod sim;

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
    kvm::open(&tls, to, fingerprint, service).await.map_err(|e| e.to_string())
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
    let portal = app.state::<AppState>().lock().unwrap().settings.portals.get(&fingerprint).copied().ok_or("place that device next to a screen first")?;
    start_session(&app, portal_label(&fingerprint), fingerprint, portal, 0.5, None).map_err(String::from)
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
    let answer = kvm::request_pair(&tls, SocketAddr::new(peer.addr, peer.info.kvm_port), &fingerprint, show).await.map_err(|e| e.to_string())?;
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
fn unpair(app: AppHandle, state: State<AppState>, fingerprint: String) {
    state.lock().unwrap().settings.unpair(&fingerprint);
    if let Some(w) = app.get_webview_window(&portal_label(&fingerprint)) {
        let _ = w.close();
    }
    let _ = app.emit("paired", ());
        refresh_tray(&app);
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
/// prompt often only allows Public or Private, and Windows marks new networks Public.
#[tauri::command]
async fn allow_firewall() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let args = format!("advfirewall firewall add rule name=LOKA dir=in action=allow profile=any program=\"{}\"", exe.display());
        // Start-Process throws when the UAC prompt is declined → exit 1
        let script = format!(
            "$p = Start-Process netsh -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '{}'; exit $p.ExitCode",
            args.replace('\'', "''")
        );
        let status = tauri::async_runtime::spawn_blocking(move || {
            std::process::Command::new("powershell").args(["-NoProfile", "-Command", &script]).creation_flags(CREATE_NO_WINDOW).status()
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        return if status.success() { Ok(()) } else { Err("Windows Firewall was not changed".into()) };
    }
    #[allow(unreachable_code)]
    Err("Only needed on Windows".into())
}

/// Open a web page in the default browser.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https links".into());
    }
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(&url).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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
// async: building a window inside a sync command deadlocks WebView2 on Windows
#[tauri::command]
async fn set_portal(app: AppHandle, state: State<'_, AppState>, fingerprint: String, portal: Option<Portal>) -> Result<(), String> {
    {
        let mut s = state.lock().unwrap();
        if !s.settings.is_paired(&fingerprint) {
            return Err("not paired".into());
        }
        match portal {
            Some(p) => s.settings.portals.insert(fingerprint.clone(), p),
            None => s.settings.portals.remove(&fingerprint),
        };
        s.settings.save();
    }
    show_portal(&app, &fingerprint, portal)
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
    if app.get_webview_window(sim::LABEL).is_some() && app.state::<AppState>().lock().unwrap().peers.get(fingerprint).is_some_and(|p| p.info.os == "sim") {
        sim::place(app, &portal).map_err(|e| e.to_string())?;
    }
    open_overlay(app, &label, Some(fingerprint.to_string()), portal.edge).map_err(|e| e.to_string())?;
    let _ = app.emit_to(&label, "portal", portal.edge); // redraw for a new edge without a restart
    place_overlay(app, &label, &portal).map_err(|e| e.to_string())
}

/// The overlay window `label`, created if needed. `to`: the paired device a drop on it goes to
/// (`None`: the band, whose drops go back to whoever drives us).
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
        .resizable(false)
        .focused(false)
        .visible(false) // shown by place_overlay
        .build()?;
    let engine = app.state::<Arc<dyn SpatialPlatformEngine>>().inner().clone();
    let window = w.clone();
    // AppKit window setup belongs on the main thread
    app.run_on_main_thread(move || {
        if let Err(e) = engine.prepare_overlay(&window) {
            eprintln!("overlay: {e}");
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
            let to = s.session.take().and_then(|(fp, _)| s.addr(&fp, |i| i.transfer_port).map(|a| (a, fp)));
            to
        };
        if let Some((addr, fp)) = to {
            sync_clipboard(self.app.clone(), addr, fp);
        }
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
            Some(kvm::Hop { to, fp: fp.clone(), portal: *portal, y: along })
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
    let mut builder = tauri::Builder::default();
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
            );
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
            set_cursor_prefs,
            cancel_transfer,
            reveal,
            add_host,
            remove_host,
            allow_firewall,
            open_url,
            sim::start_sim,
            set_sharing,
            set_received,
            wake,
            send_paths,
            take_outbox,
            control,
            disconnect,
            clip::clip_history,
            clip::clip_restore,
            shell::term_open,
            shell::term_write,
            shell::term_resize,
            shell::term_close,
            share::open_share,
            audio::set_listening,
            audio::set_mic,
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
            let settings = Settings::load(&data);
            let tls = Arc::new(Tls::new(&Identity::load_or_create(&data)));
            let aspect_ratio = app
                .primary_monitor()?
                .map(|m| m.size().width as f32 / m.size().height as f32)
                .unwrap_or(16.0 / 9.0);
            let me = DeviceInfo {
                alias: std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "LOKA".into()),
                device_type: if cfg!(mobile) { DeviceType::Mobile } else { DeviceType::Desktop },
                os: std::env::consts::OS.into(),
                fingerprint: tls.me.clone(),
                aspect_ratio,
                telemetry_port: telemetry::PORT,
                transfer_port: transfer::PORT,
                kvm_port: kvm::PORT,
                proto: kvm::PROTOCOL,
                mac: mac_address::get_mac_address().ok().flatten().map(|m| m.to_string()),
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
            spawn_transfer_server(&handle, state.clone(), tls.clone())?;
            let h = handle.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = telemetry::listen(telemetry::PORT, |p| {
                    let _ = h.emit("spatial-remote", p);
                })
                .await
                {
                    eprintln!("telemetry listener stopped: {e}");
                }
            });

            #[cfg(desktop)]
            {
                setup_overlays(app, state.clone(), tls)?;
                setup_tray(app)?;
                clip::watch(handle.clone());
            }
            #[cfg(target_os = "windows")]
            install_send_to();
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
            // heartbeats are frequent; only tell the UI about arrivals and changes
            if previous.map_or(true, |p| p.info != peer.info || p.addr != peer.addr || p.seen.elapsed() >= discovery::STALE) {
                let _ = app.emit("peer", peer);
            }
        })
        .await;
        if let Err(e) = r {
            eprintln!("discovery stopped: {e}");
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
        let r = transfer::serve(tls, transfer::PORT, dir, clip_dir, allow, move |e| {
            match e {
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
            eprintln!("transfer server stopped: {e}");
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
                Err(e) => transfer::TransferEvent::Failed { id, outgoing: true, name, error: e.to_string() },
            });
        });
    }
}

/// Hand this computer's clipboard picture or files to device `fp` (where the cursor is going), if they
/// changed. Runs beside the crossing, never delaying it.
fn sync_clipboard(app: AppHandle, to: SocketAddr, fp: String) {
    tauri::async_runtime::spawn(async move {
        match tokio::task::spawn_blocking(clip::outgoing).await.ok().flatten() {
            Some(clip::Outgoing::Image(png)) => {
                let tls = app.state::<Arc<Tls>>().inner().clone();
                if let Err(e) = transfer::send_bytes(&tls, to, &fp, transfer::Kind::Image, &png).await {
                    eprintln!("clipboard picture: {e}");
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
            eprintln!("drag manifest: {e}");
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
        carry::Step::Start(files) => {
            let label = app.state::<AppState>().lock().unwrap().carry_window.clone();
            let Some(w) = app.get_webview_window(&label) else { return };
            // AppKit/OLE drag sessions belong on the main thread
            let _ = app.run_on_main_thread(move || {
                let icon = drag::Image::Raw(include_bytes!("../icons/64x64.png").to_vec());
                if let Err(e) = drag::start_drag(&w, drag::DragItem::Files(files), icon, |_, _| {}, Default::default()) {
                    eprintln!("drag: {e}");
                }
            });
        }
        carry::Step::Stream(batch, files) => {
            let label = app.state::<AppState>().lock().unwrap().carry_window.clone();
            let Some(w) = app.get_webview_window(&label) else { return };
            let _ = app.run_on_main_thread(move || {
                if let Err(e) = carry::stream_drag(&w, batch, files) {
                    eprintln!("drag: {e}");
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
                    eprintln!("saving dragged {name}: {e}");
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
            "disconnect" => kvm::end_sessions(),
            "quit" => app.exit(0),
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
    let devices: Vec<(String, String)> = {
        let state = app.state::<AppState>();
        let s = state.lock().unwrap();
        s.settings.paired.iter().filter(|d| s.settings.portals.contains_key(&d.fingerprint)).map(|d| (d.fingerprint.clone(), d.alias.clone())).collect()
    };
    for (fp, alias) in devices {
        menu.append(&MenuItem::with_id(app, format!("control:{fp}"), format!("Control {alias}"), true, None::<&str>)?)?;
    }
    menu.append(&MenuItem::with_id(app, "disconnect", "Disconnect", true, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "about", "About LOKA", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit LOKA", true, None::<&str>)?)?;
    Ok(menu)
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

/// Answers pairing requests and accepts input sessions from paired devices.
#[cfg(desktop)]
fn spawn_kvm_receiver(app: AppHandle, engine: Arc<dyn SpatialPlatformEngine>, state: AppState, tls: Arc<Tls>) {
    tauri::async_runtime::spawn(async move {
        let st = state.clone();
        let allow = move |fp: &str| if st.lock().unwrap().settings.is_paired(fp) { Ok(()) } else { Err(kvm::Refusal::NotPaired) };
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
        let make = move |fp: &str, edge: Edge| {
            let screens: Vec<Rect> = app
                .available_monitors()
                .ok()?
                .iter()
                .map(|m| Rect { x: m.position().x as f64, y: m.position().y as f64, w: m.size().width as f64, h: m.size().height as f64 })
                .collect();
            if screens.is_empty() {
                return None;
            }
            // the whole entry edge leads back to the driver: files dragged there and dropped go to it (reach = Calibrate's REACH)
            let band = Portal { edge, monitor: kvm::bounds(&screens), start: 0.0, end: 1.0, reach: 120.0 };
            let mirror = {
                let state = app.state::<AppState>();
                let mut s = state.lock().unwrap();
                s.session = Some((fp.to_string(), band));
                // no way to the driver yet: mirror its portal (middle of the screen on the entry edge), editable in Calibrate
                let b = band.monitor;
                let screen = screens.iter().find(|m| match edge {
                    Edge::Left => m.x == b.x,
                    Edge::Right => m.x + m.w == b.x + b.w,
                    Edge::Top => m.y == b.y,
                    Edge::Bottom => m.y + m.h == b.y + b.h,
                });
                let mirror = screen.filter(|_| !s.settings.portals.contains_key(fp)).map(|m| Portal { edge, monitor: *m, start: 0.35, end: 0.65, reach: 120.0 });
                if let Some(p) = mirror {
                    s.settings.portals.insert(fp.to_string(), p);
                    s.settings.save();
                }
                mirror
            };
            if let Some(p) = mirror {
                let (app, fp) = (app.clone(), fp.to_string());
                tauri::async_runtime::spawn(async move { drop(show_portal(&app, &fp, Some(p))) });
            }
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
                    kvm::Service::Shell => sh.terminal,
                    kvm::Service::Files => sh.folder.is_some(),
                    kvm::Service::Sound => sh.sound,
                    kvm::Service::Notifications => sh.notifications,
                    kvm::Service::Mic => sh.mic,
                }
        };
        let st = state.clone();
        let on_service = move |service: kvm::Service, _fp: String, stream: secure::Stream| {
            let folder = st.lock().unwrap().settings.sharing.folder.clone();
            tauri::async_runtime::spawn(async move {
                let r = match service {
                    kvm::Service::Shell => shell::serve(stream).await,
                    kvm::Service::Files => match folder {
                        Some(root) => share::serve(stream, root).await,
                        None => Ok(()),
                    },
                    kvm::Service::Sound => audio::serve(stream, false).await,
                    kvm::Service::Mic => audio::serve(stream, true).await,
                    kvm::Service::Notifications => notify::serve(stream).await,
                };
                if let Err(e) = r {
                    eprintln!("{service:?}: {e}");
                }
            });
        };
        if let Err(e) = kvm::serve(tls, kvm::PORT, allow, ask, allow_service, on_service, make).await {
            eprintln!("kvm receiver stopped: {e}");
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
#[cfg(desktop)]
fn start_session(app: &AppHandle, label: String, fp: String, portal: Portal, y: f32, drag: Option<u64>) -> Result<(), &'static str> {
    let state = app.state::<AppState>();
    let (to, transfer_to, prefs, peer_is_mac, alias) = {
        let s = state.lock().unwrap();
        let to = s.addr(&fp, |i| i.kvm_port).ok_or("that device isn't on the network")?;
        let peer = s.peers.get(&fp);
        let alias = peer.map_or_else(|| "The other device".into(), |p| p.info.alias.clone());
        (to, s.addr(&fp, |i| i.transfer_port), s.settings.cursor, peer.is_some_and(|p| p.info.os == "macos"), alias)
    };
    if DRIVING.swap(true, Ordering::SeqCst) {
        return Err("already controlling another device");
    }
    if let Some(t) = transfer_to {
        sync_clipboard(app.clone(), t, fp.clone());
    }
    let tune = kvm::Tune::new(prefs.speed, prefs.swap_ctrl_cmd && peer_is_mac != cfg!(target_os = "macos"));
    let (app, engine) = (app.clone(), app.state::<Arc<dyn SpatialPlatformEngine>>().inner().clone());
    tauri::async_runtime::spawn(async move {
        let _ = app.emit("kvm", serde_json::json!({ "active": true, "y": y, "device": fp }));
        let tls = app.state::<Arc<Tls>>().inner().clone();
        let r = kvm::send_session(&tls, engine, to, &fp, portal, y, tune, drag).await;
        if let Ok(Some(batch)) = r {
            hold_drag(&app, &label, batch); // a drag came back with the cursor
        }
        // TimedOut: the link went silent (Wi-Fi drop, sleep, crash), not a refusal or a hang-up
        let error = r.as_ref().err().map(|e| if e.kind() == std::io::ErrorKind::TimedOut { format!("{alias} stopped answering") } else { e.to_string() });
        let _ = app.emit("kvm", serde_json::json!({ "active": false, "error": error }));
        if error.is_some() {
            tokio::time::sleep(Duration::from_secs(1)).await; // no reconnect storm at the edge
        }
        DRIVING.store(false, Ordering::SeqCst);
    });
    Ok(())
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
                    .filter(|(fp, _)| s.paired_peer(fp).is_some())
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
        let Some(c) = engine.cursor() else { continue };
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
        let on_edge = hit.is_some_and(|(d, _, p)| (1.0 - p as f64) * d.portal.reach < 1.5) && !locked;
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
                        eprintln!("crossing: {e}");
                    }
                }
                continue;
            }
        } else {
            resting = None;
        }
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
