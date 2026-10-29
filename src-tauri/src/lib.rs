mod kvm;
mod network;
mod platform;
mod settings;
mod sim;
mod clip;
mod carry;

use network::{
    discovery::{self, DeviceInfo, DeviceType, Peer, Role},
    telemetry::{self, PacketType, PayloadKind, SpatialPacket},
    transfer::{self, Kind},
};
use platform::{Edge, Injected, Portal, Rect, SpatialPlatformEngine};
use settings::{CursorPrefs, PairedDevice, Settings};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{oneshot, watch};

const OVERLAY: &str = "edge-overlay";
const MAIN_WINDOW: &str = "main";

struct Shared {
    settings: Settings,
    peers: HashMap<String, Peer>,
    portal: Option<Portal>,
    /// fingerprint of the paired client the portal leads to
    target: Option<String>,
    last_y: f32,
    /// Client, while a Main controls us: its address and our band on the edge leading back to it
    session: Option<(IpAddr, Portal)>,
    /// files of an OS drag hovering the overlay right now
    hover: Option<Vec<std::path::PathBuf>>,
    /// the next Drop on the overlay is our own hand-off of a drag to the other device, not a transfer
    handoff: bool,
    /// file drags carried across the edge, waiting for their files or the held cursor
    carry: carry::Carry,
    /// pairing prompts waiting for the user's answer
    pending: HashMap<u64, oneshot::Sender<bool>>,
    next_request: u64,
}

type AppState = Arc<Mutex<Shared>>;

impl Shared {
    /// The device input/payloads go to: only a paired client, and only while we are Main.
    fn target(&self) -> Option<&Peer> {
        if self.settings.role != Some(Role::Main) {
            return None;
        }
        let f = self.target.as_ref().filter(|f| self.settings.is_paired(f))?;
        self.peers.get(f)
    }

    /// Where dropped payloads go: our selected Client (Main), or the Main controlling us (Client).
    fn drop_target(&self) -> Option<&Peer> {
        self.target().or_else(|| self.session.and_then(|(ip, _)| self.paired_peer_at(ip, Role::Main)))
    }

    fn live_peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values().filter(|p| p.seen.elapsed() < discovery::STALE)
    }

    /// Not only live peers: a few lost heartbeats must not lock out a paired device mid-use.
    fn paired_peer_at(&self, ip: IpAddr, role: Role) -> Option<&Peer> {
        self.peers.values().find(|p| p.addr == ip && p.info.role == Some(role) && self.settings.is_paired(&p.info.fingerprint))
    }
}

#[derive(serde::Serialize)]
struct Device {
    info: DeviceInfo,
    paired: Vec<PairedDevice>,
    /// this computer's LAN addresses, to type into another LOKA's "Add by IP"
    ips: Vec<std::net::Ipv4Addr>,
    hosts: Vec<IpAddr>,
    cursor: CursorPrefs,
}

#[tauri::command]
fn get_device(state: State<AppState>, me: State<watch::Sender<DeviceInfo>>) -> Device {
    let s = state.lock().unwrap();
    Device { info: me.borrow().clone(), paired: s.settings.paired.clone(), ips: discovery::local_ipv4(), hosts: s.settings.hosts.clone(), cursor: s.settings.cursor }
}

/// Applies from the next crossing on.
#[tauri::command]
fn set_cursor_prefs(state: State<AppState>, prefs: CursorPrefs) {
    let mut s = state.lock().unwrap();
    s.settings.cursor = prefs;
    s.settings.save();
}

#[tauri::command]
fn set_role(app: AppHandle, state: State<AppState>, me: State<watch::Sender<DeviceInfo>>, role: Role) {
    {
        let mut s = state.lock().unwrap();
        s.settings.role = Some(role);
        s.settings.save();
    }
    me.send_modify(|i| i.role = Some(role)); // re-announces immediately
    if role == Role::Client {
        if let Some(w) = app.get_webview_window(OVERLAY) {
            let _ = w.hide(); // clients have no portal
        }
    }
}

#[tauri::command]
fn peers(state: State<AppState>) -> Vec<Peer> {
    state.lock().unwrap().live_peers().cloned().collect()
}

/// Main → client: ask the client's user to pair. Resolves with their answer.
#[tauri::command]
async fn pair(app: AppHandle, state: State<'_, AppState>, fingerprint: String) -> Result<bool, String> {
    let peer = {
        let s = state.lock().unwrap();
        if s.settings.role != Some(Role::Main) {
            return Err("only the Main device can start pairing".into());
        }
        let found = s.live_peers().find(|p| p.info.fingerprint == fingerprint && p.info.role == Some(Role::Client)).cloned();
        found
    }
    .ok_or("device is no longer on the network")?;
    let accepted = kvm::request_pair(SocketAddr::new(peer.addr, peer.info.kvm_port)).await.map_err(|e| e.to_string())?;
    if accepted {
        state.lock().unwrap().settings.pair(PairedDevice { fingerprint, alias: peer.info.alias });
        let _ = app.emit("paired", ());
    }
    Ok(accepted)
}

#[tauri::command]
fn answer_pair(state: State<AppState>, id: u64, accept: bool) {
    if let Some(tx) = state.lock().unwrap().pending.remove(&id) {
        let _ = tx.send(accept);
    }
}

#[tauri::command]
fn unpair(app: AppHandle, state: State<AppState>, fingerprint: String) {
    let mut s = state.lock().unwrap();
    s.settings.unpair(&fingerprint);
    if s.target.as_deref() == Some(&fingerprint) {
        s.target = None;
    }
    let _ = app.emit("paired", ());
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

/// Show a pairing prompt in `window` and wait for the user (or time out → declined).
async fn ask_user(app: &AppHandle, state: &AppState, window: &str, from: &Peer) -> bool {
    let (tx, rx) = oneshot::channel();
    let id = {
        let mut s = state.lock().unwrap();
        s.next_request += 1;
        let id = s.next_request;
        s.pending.insert(id, tx);
        id
    };
    let _ = app.emit_to(window, "pair-request", serde_json::json!({ "id": id, "peer": from }));
    let accepted = matches!(tokio::time::timeout(kvm::PAIR_TIMEOUT, rx).await, Ok(Ok(true)));
    state.lock().unwrap().pending.remove(&id);
    let _ = app.emit_to(window, "pair-request-closed", id);
    accepted
}

/// Store the calibrated portal and move the overlay window onto its band.
#[tauri::command]
fn set_portal(app: AppHandle, state: State<AppState>, portal: Portal, target: Option<String>) -> Result<(), String> {
    {
        let mut s = state.lock().unwrap();
        s.portal = Some(portal);
        s.target = target;
    }
    let _ = app.emit("portal", portal.edge); // overlay + simulated phone redraw for a new edge
    sim::place(&app, &portal).map_err(|e| e.to_string())?;
    place_overlay(&app, &portal).map_err(|e| e.to_string())
}

/// Move the overlay window onto `portal`'s band and show it, click-through.
fn place_overlay(app: &AppHandle, portal: &Portal) -> tauri::Result<()> {
    let Some(w) = app.get_webview_window(OVERLAY) else { return Ok(()) };
    let b = portal.hit_box();
    w.set_position(tauri::PhysicalPosition::new(b.x as i32, b.y as i32))?;
    w.set_size(tauri::PhysicalSize::new(b.w as u32, b.h as u32))?;
    w.show()?;
    // show() rewrites window styles on Windows; restore the overlay ones
    match app.try_state::<Arc<dyn SpatialPlatformEngine>>() {
        Some(e) => e.set_click_through(&w, true),
        None => Ok(()),
    }
}

#[tauri::command]
async fn send_text(state: State<'_, AppState>, kind: Kind, text: String) -> Result<(), String> {
    let to = transfer_addr(&state).ok_or("no paired device")?;
    transfer::send_text(to, kind, &text).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn cancel_transfer(id: u64) {
    transfer::cancel(id);
}

/// Open the file manager with `path` selected.
#[tauri::command]
fn reveal(path: std::path::PathBuf) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg("/select,").arg(&path).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(&path).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(&path)).spawn();
    r.map(drop).map_err(|e| e.to_string())
}

fn transfer_addr(state: &AppState) -> Option<SocketAddr> {
    state.lock().unwrap().drop_target().map(|p| SocketAddr::new(p.addr, p.info.transfer_port))
}

fn telemetry_addr(state: &AppState) -> Option<SocketAddr> {
    state.lock().unwrap().target().map(|p| SocketAddr::new(p.addr, p.info.telemetry_port))
}

fn kvm_addr(state: &AppState) -> Option<SocketAddr> {
    state.lock().unwrap().target().map(|p| SocketAddr::new(p.addr, p.info.kvm_port))
}

/// Received mouse/keyboard goes to this OS, inside the primary monitor.
// shortcut: receiver cursor is confined to its primary monitor; extend to the virtual desktop for multi-monitor receivers.
struct EngineSink {
    engine: Arc<dyn SpatialPlatformEngine>,
    area: Rect,
    app: AppHandle,
}

/// Session over: no band back to the Main any more; a picture or files copied here go to it.
impl Drop for EngineSink {
    fn drop(&mut self) {
        let to = {
            let state = self.app.state::<AppState>();
            let mut s = state.lock().unwrap();
            let to = s.drop_target().map(|p| SocketAddr::new(p.addr, p.info.transfer_port));
            s.session = None;
            to
        };
        if let Some(to) = to {
            sync_clipboard(self.app.clone(), to);
        }
        if let Some(w) = self.app.get_webview_window(OVERLAY) {
            let _ = w.hide();
        }
    }
}

impl kvm::Sink for EngineSink {
    fn area(&self) -> Rect {
        self.area
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
        hold_drag(&self.app, batch);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .invoke_handler(tauri::generate_handler![
            get_device,
            set_role,
            peers,
            pair,
            answer_pair,
            unpair,
            set_portal,
            set_cursor_prefs,
            send_text,
            cancel_transfer,
            reveal,
            add_host,
            remove_host,
            allow_firewall,
            open_url,
            sim::start_sim
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
            let settings = Settings::load(&app.path().app_data_dir()?);
            let aspect_ratio = app
                .primary_monitor()?
                .map(|m| m.size().width as f32 / m.size().height as f32)
                .unwrap_or(16.0 / 9.0);
            let me = DeviceInfo {
                alias: std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "LOKA".into()),
                device_type: if cfg!(mobile) { DeviceType::Mobile } else { DeviceType::Desktop },
                os: std::env::consts::OS.into(),
                fingerprint: settings.fingerprint.clone(),
                aspect_ratio,
                telemetry_port: telemetry::PORT,
                transfer_port: transfer::PORT,
                kvm_port: kvm::PORT,
                role: settings.role,
            };
            let state: AppState = Arc::new(Mutex::new(Shared {
                settings,
                peers: HashMap::new(),
                portal: None,
                target: None,
                last_y: 0.0,
                session: None,
                hover: None,
                handoff: false,
                carry: Default::default(),
                pending: HashMap::new(),
                next_request: 0,
            }));
            app.manage(state.clone());
            let (me_tx, me_rx) = watch::channel(me);
            app.manage(me_tx);

            spawn_discovery(handle.clone(), state.clone(), me_rx);
            spawn_transfer_server(&handle, state.clone())?;
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
                setup_overlay(app, state.clone())?;
                setup_tray(app)?;
            }
            if std::env::args().any(|a| a == "--sim-mobile") {
                sim::start(&handle, &state)?;
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running LOKA");
}

fn spawn_discovery(app: AppHandle, state: AppState, me: watch::Receiver<DeviceInfo>) {
    tauri::async_runtime::spawn(async move {
        let hosts_state = state.clone();
        let hosts = move || hosts_state.lock().unwrap().settings.hosts.clone();
        let r = discovery::run(me, hosts, |peer| {
            let previous = state.lock().unwrap().peers.insert(peer.info.fingerprint.clone(), peer.clone());
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

fn spawn_transfer_server(app: &AppHandle, state: AppState) -> tauri::Result<()> {
    let dir = app.path().download_dir().or_else(|_| app.path().app_data_dir())?.join("LOKA");
    let clip_dir = app.path().app_cache_dir()?.join("clipboard");
    let _ = std::fs::remove_dir_all(&clip_dir); // last run's clipboard files are no longer on any clipboard
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        // paired devices only: a Main pushes to its Clients, a Client drops back to its Main
        let allow = move |ip| {
            let s = state.lock().unwrap();
            s.paired_peer_at(ip, Role::Main).or(s.paired_peer_at(ip, Role::Client)).is_some()
        };
        let clip_root = clip_dir.clone();
        let r = transfer::serve(transfer::PORT, dir, clip_dir, allow, move |e| {
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
                    if last.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(3)) {
                        use tauri_plugin_notification::NotificationExt;
                        let name = transfer::file_name(path);
                        let _ = h.notification().builder().title("LOKA").body(format!("{name} saved to Downloads/LOKA")).show();
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

/// Send files 4 at a time like LocalSend's parallel uploads (small files don't queue behind each other's
/// handshakes), reporting progress as `transfer` events.
async fn send_files(app: AppHandle, to: SocketAddr, files: Vec<(std::path::PathBuf, String)>, clip: Option<transfer::ClipBatch>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    for (path, name) in files {
        let (h, slot) = (app.clone(), slots.clone().acquire_owned().await.unwrap());
        tauri::async_runtime::spawn(async move {
            let id = transfer::next_id();
            let r = transfer::send_file(to, &path, name.clone(), clip, id, |done, total| {
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

/// Hand this computer's clipboard picture or files to `to` (the device the cursor is moving to), if they
/// changed. Runs beside the crossing, never delaying it.
fn sync_clipboard(app: AppHandle, to: SocketAddr) {
    tauri::async_runtime::spawn(async move {
        match tokio::task::spawn_blocking(clip::outgoing).await.ok().flatten() {
            Some(clip::Outgoing::Image(png)) => {
                if let Err(e) = transfer::send_bytes(to, transfer::Kind::Image, &png).await {
                    eprintln!("clipboard picture: {e}");
                }
            }
            Some(clip::Outgoing::Files(files)) => {
                let batch = transfer::ClipBatch { id: uuid::Uuid::new_v4().as_u128() as u64, files: files.len() as u32, drag: false };
                send_files(app, to, files, Some(batch)).await;
            }
            None => {}
        }
    });
}

/// Hand an OS file drag hovering the portal edge to the other device: end it here (a marked button-up
/// drops it on our own overlay, ignored as `handoff`), send its files as drag batch, and return the batch
/// for the cursor to carry along.
fn hand_off_drag(app: &AppHandle, engine: &Arc<dyn SpatialPlatformEngine>, state: &AppState, to: SocketAddr) -> Option<u64> {
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
        send_files(app, to, files, Some(batch)).await;
    });
    Some(id)
}

/// The cursor arrived here with drag `batch`, its button still held: hold it on this OS too (pressed on our
/// own overlay, which sits under the cursor at the edge) until the files are here.
fn hold_drag(app: &AppHandle, batch: u64) {
    let engine = app.state::<Arc<dyn SpatialPlatformEngine>>().inner().clone();
    if let Some(w) = app.get_webview_window(OVERLAY) {
        let _ = engine.set_click_through(&w, false);
    }
    engine.inject(Injected::Button { button: 1, down: true });
    let step = app.state::<AppState>().lock().unwrap().carry.held(batch);
    carry_out(app, step);
}

fn carry_out(app: &AppHandle, step: carry::Step) {
    match step {
        carry::Step::Start(files) => {
            let Some(w) = app.get_webview_window(OVERLAY) else { return };
            // AppKit/OLE drag sessions belong on the main thread
            let _ = app.run_on_main_thread(move || {
                let icon = drag::Image::Raw(include_bytes!("../icons/64x64.png").to_vec());
                if let Err(e) = drag::start_drag(&w, drag::DragItem::Files(files), icon, |_, _| {}, Default::default()) {
                    eprintln!("drag: {e}");
                }
            });
        }
        carry::Step::Save(files) => {
            let Ok(dir) = app.path().download_dir().map(|d| d.join("LOKA")) else { return };
            let _ = std::fs::create_dir_all(&dir);
            for f in &files {
                let name = transfer::file_name(f);
                let dest = (0..1000).map(|i| if i == 0 { dir.join(&name) } else { dir.join(format!("{name} ({i})")) }).find(|p| !p.exists());
                // shortcut: rename only; cache and Downloads on different volumes leave the files in the cache
                if let Some(Err(e)) = dest.map(|d| std::fs::rename(f, d)) {
                    eprintln!("saving dragged {name}: {e}");
                }
            }
            use tauri_plugin_notification::NotificationExt;
            let _ = app.notification().builder().title("LOKA").body("Let go before the files arrived: saved to Downloads/LOKA").show();
        }
        carry::Step::Wait => {}
    }
}

#[cfg(desktop)]
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::{
        menu::{Menu, MenuItem},
        tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    };
    let open = MenuItem::with_id(app, "open", "Open LOKA", true, None::<&str>)?;
    let about = MenuItem::with_id(app, "about", "About LOKA", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit LOKA", true, None::<&str>)?;
    let show = |app: &AppHandle| {
        if let Some(w) = app.get_webview_window(MAIN_WINDOW) {
            let _ = w.unminimize();
            let _ = w.show();
            let _ = w.set_focus();
        }
    };
    let mut tray = TrayIconBuilder::new()
        .tooltip("LOKA")
        .menu(&Menu::with_items(app, &[&open, &about, &quit])?)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, e| match e.id.as_ref() {
            "open" => show(app),
            "about" => {
                show(app);
                let _ = app.emit_to(MAIN_WINDOW, "about", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, e| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                show(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg(desktop)]
fn setup_overlay(app: &mut tauri::App, state: AppState) -> tauri::Result<()> {
    use tauri::{DragDropEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};

    let overlay = WebviewWindowBuilder::new(app, OVERLAY, WebviewUrl::default())
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false) // shown by set_portal after calibration
        .build()?;
    let engine: Arc<dyn SpatialPlatformEngine> = platform::engine().into();
    engine.prepare_overlay(&overlay)?;

    let (h, s) = (app.handle().clone(), state.clone());
    let own = app.path().app_cache_dir()?.join("clipboard");
    overlay.on_window_event(move |e| {
        let WindowEvent::DragDrop(e) = e else { return };
        let paths = match e {
            DragDropEvent::Enter { paths, .. } => {
                s.lock().unwrap().hover = Some(paths.clone());
                return;
            }
            DragDropEvent::Leave => {
                s.lock().unwrap().hover = None;
                return;
            }
            DragDropEvent::Drop { paths, .. } => paths,
            _ => return,
        };
        {
            let mut st = s.lock().unwrap();
            st.hover = None;
            // our own hand-off, or a drag that just came over from the other device let go at the edge
            if std::mem::take(&mut st.handoff) || paths.iter().any(|p| p.starts_with(&own)) {
                return;
            }
        }
        let Some(to) = transfer_addr(&s) else { return };
        let tele = telemetry_addr(&s); // none on a Client: the Main has no glow for us
        let y = s.lock().unwrap().last_y;
        let (h, paths) = (h.clone(), paths.clone());
        tauri::async_runtime::spawn(async move {
            let packet = SpatialPacket { kind: PacketType::Drop, normalized_y: y, proximity: 1.0, payload: PayloadKind::File };
            if let (Ok(sock), Some(tele)) = (telemetry::sender().await, tele) {
                telemetry::send(&sock, tele, &packet).await;
            }
            let _ = h.emit_to(OVERLAY, "spatial", packet);
            let files = tokio::task::spawn_blocking(move || transfer::expand(&paths)).await.unwrap_or_default();
            send_files(h, to, files, None).await;
        });
    });

    app.manage(engine.clone());
    spawn_kvm_receiver(app.handle().clone(), engine.clone(), state.clone());
    tauri::async_runtime::spawn(track(app.handle().clone(), overlay, engine, state));
    Ok(())
}

/// Client side: answers pairing requests from Mains and accepts input sessions from paired ones.
#[cfg(desktop)]
fn spawn_kvm_receiver(app: AppHandle, engine: Arc<dyn SpatialPlatformEngine>, state: AppState) {
    tauri::async_runtime::spawn(async move {
        let is_client = |s: &Shared| s.settings.role == Some(Role::Client);
        let st = state.clone();
        let allow = move |ip| {
            let s = st.lock().unwrap();
            match (is_client(&s), s.paired_peer_at(ip, Role::Main)) {
                (false, _) => Err(kvm::Refusal::NotClient),
                (true, None) => Err(kvm::Refusal::NotPaired),
                (true, Some(_)) => Ok(()),
            }
        };
        let (a, st) = (app.clone(), state.clone());
        let ask = move |ip: IpAddr| {
            let (app, state) = (a.clone(), st.clone());
            async move {
                let from = {
                    let s = state.lock().unwrap();
                    if !is_client(&s) {
                        return false;
                    }
                    let found = s.live_peers().find(|p| p.addr == ip && p.info.role == Some(Role::Main)).cloned();
                    found
                };
                let Some(from) = from else { return false }; // not a Main we have discovered
                let accepted = ask_user(&app, &state, MAIN_WINDOW, &from).await;
                if accepted {
                    state.lock().unwrap().settings.pair(PairedDevice { fingerprint: from.info.fingerprint.clone(), alias: from.info.alias.clone() });
                    let _ = app.emit("paired", ());
                }
                accepted
            }
        };
        let make = move |ip: IpAddr, edge: Edge| {
            let m = app.primary_monitor().ok().flatten()?;
            let area = Rect { x: m.position().x as f64, y: m.position().y as f64, w: m.size().width as f64, h: m.size().height as f64 };
            // the whole entry edge leads back to the Main: files dragged there and dropped go to it (reach = Calibrate's REACH)
            let band = Portal { edge, monitor: area, start: 0.0, end: 1.0, reach: 120.0 };
            app.state::<AppState>().lock().unwrap().session = Some((ip, band));
            let _ = app.emit("portal", edge);
            let _ = place_overlay(&app, &band);
            Some(EngineSink { engine: engine.clone(), area, app: app.clone() })
        };
        if let Err(e) = kvm::serve(kvm::PORT, allow, ask, make).await {
            eprintln!("kvm receiver stopped: {e}");
        }
    });
}

/// 60 Hz loop: cursor → portal sample → click-through toggle, local glow event, UDP telemetry.
#[cfg(desktop)]
async fn track(app: AppHandle, overlay: tauri::WebviewWindow, engine: Arc<dyn SpatialPlatformEngine>, state: AppState) {
    let Ok(sock) = telemetry::sender().await else { return };
    let mut tick = tokio::time::interval(Duration::from_micros(16_667));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (mut interactive, mut streaming) = (false, false);
    let remote = Arc::new(AtomicBool::new(false));
    // cursor lock key edge, last lock state shown, since when the cursor rests on the portal edge
    let (mut lock_key, mut shown_lock, mut resting) = (false, false, None::<std::time::Instant>);
    loop {
        tick.tick().await;
        let (portal, prefs, peer_is_mac) = {
            let s = state.lock().unwrap();
            let portal = match s.settings.role {
                Some(Role::Main) => s.portal,
                Some(Role::Client) => s.session.map(|(_, band)| band),
                None => None,
            };
            (portal, s.settings.cursor, s.target().is_some_and(|p| p.info.os == "macos"))
        };
        let Some(portal) = portal else { continue };
        let driving = remote.load(Ordering::SeqCst);
        if kvm_addr(&state).is_some() {
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
        let sample = portal.sample(c.x, c.y);
        let on_edge = matches!(sample, Some((_, p)) if p >= 1.0) && !locked;
        {
            // the button of a carried drag went up before its files came: they get saved instead
            let mut s = state.lock().unwrap();
            if !c.button_down && s.carry.waiting() {
                s.carry.released();
            }
        }
        // a file drag pushed against the edge goes over with the cursor (Main), or back with it (Client)
        let mut drag = None;
        if on_edge && c.button_down && state.lock().unwrap().hover.is_some() {
            if let Some(t) = transfer_addr(&state) {
                drag = hand_off_drag(&app, &engine, &state, t);
                if let (Some(batch), None) = (drag, kvm_addr(&state)) {
                    kvm::DRAG_OUT.store(batch, Ordering::SeqCst); // rides back with the next push through the edge
                    continue;
                }
            }
        }
        // resting against the portal edge (no drag, not locked) hands mouse & keyboard to the paired device
        let at_edge = on_edge && (!c.button_down || drag.is_some());
        if !at_edge {
            resting = None;
        }
        if let (true, Some((y, _)), Some(to)) = (at_edge, sample, kvm_addr(&state)) {
            let since = *resting.get_or_insert_with(std::time::Instant::now);
            if drag.is_some() || since.elapsed() >= Duration::from_millis(prefs.edge_delay_ms.into()) {
                resting = None;
                remote.store(true, Ordering::SeqCst);
                let (engine, remote, app) = (engine.clone(), remote.clone(), app.clone());
                if let Some(t) = transfer_addr(&state) {
                    sync_clipboard(app.clone(), t);
                }
                let tune = kvm::Tune::new(prefs.speed, prefs.swap_ctrl_cmd && peer_is_mac != cfg!(target_os = "macos"));
                tauri::async_runtime::spawn(async move {
                    let _ = app.emit("kvm", serde_json::json!({ "active": true, "y": y }));
                    let r = kvm::send_session(engine, to, portal, y, tune, drag).await;
                    if let Ok(Some(batch)) = r {
                        hold_drag(&app, batch); // a drag came back with the cursor
                    }
                    let error = r.as_ref().err().map(|e| e.to_string());
                    let _ = app.emit("kvm", serde_json::json!({ "active": false, "error": error }));
                    if error.is_some() {
                        tokio::time::sleep(Duration::from_secs(1)).await; // no reconnect storm at the edge
                    }
                    remote.store(false, Ordering::SeqCst);
                });
            }
            continue;
        }
        // only a drag (button held) inside the portal may make the overlay catch input
        let hit = sample.filter(|_| c.button_down);
        if hit.is_some() != interactive {
            interactive = !interactive;
            let _ = engine.set_click_through(&overlay, !interactive);
        }
        let packet = match hit {
            Some((y, p)) => {
                streaming = true;
                state.lock().unwrap().last_y = y;
                SpatialPacket { kind: PacketType::Proximity, normalized_y: y, proximity: p, payload: PayloadKind::None }
            }
            None if streaming => {
                streaming = false;
                SpatialPacket { kind: PacketType::Leave, normalized_y: state.lock().unwrap().last_y, proximity: 0.0, payload: PayloadKind::None }
            }
            None => continue,
        };
        let _ = app.emit_to(OVERLAY, "spatial", packet);
        if let Some(to) = telemetry_addr(&state) {
            telemetry::send(&sock, to, &packet).await;
        }
    }
}
