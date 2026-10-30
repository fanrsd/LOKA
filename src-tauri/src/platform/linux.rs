//! Linux: input is read and written below the display server, so X11, Wayland and a text console behave
//! the same. Mice, keyboards and touchpads come from /dev/input (grabbed with EVIOCGRAB while driving
//! another device); input goes in through two uinput devices. Both need `allow_input` once.
//! X11 says where the pointer is; Wayland tells no client, so there it is followed from the devices' moves.

use super::{Captured, CursorSample, Injected, InputEvent, Rect, SpatialPlatformEngine};
use crate::kvm::{bounds, nearest};
use evdev::{
    uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, Device, EventType, InputId, KeyCode, PropType, RelativeAxisCode,
    SynchronizationCode, UinputAbsSetup,
};
use std::{
    io,
    os::fd::AsRawFd,
    path::PathBuf,
    process::Command,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::WebviewWindow;
use tokio::sync::mpsc::UnboundedSender;
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as _, xproto::ConnectionExt as _},
    rust_connection::RustConnection,
};

/// No display server (a server's text console): input goes to one screen of this size.
pub const HEADLESS: Rect = Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 };
const ACCESS: &str = "LOKA can't use this computer's mouse and keyboard yet: open About → Allow input access (or run `sudo loka --allow-input`)";
const ABS_MAX: i32 = 65535;
/// Touchpad while driving: pointer px per mm of finger travel, wheel units (120 = a notch) per mm of two-finger travel.
const PAD_PX: f64 = 12.0;
const PAD_WHEEL: f64 = 20.0;

/// Shared between the engine and the thread reading /dev/input.
#[derive(Default)]
struct State {
    /// pointer as followed from the devices (exact after our own moves); X11 asks the server instead
    pos: (f64, f64),
    left: bool,
    lock_key: bool,
    screens: Vec<Rect>,
    /// some input device could be opened
    reading: bool,
    /// driving: where captured input goes; `grabbed` once the devices are ours
    out: Option<UnboundedSender<Captured>>,
    grabbed: bool,
    /// `start_capture` waiting for the grab
    reply: Option<mpsc::Sender<Result<(), String>>>,
}

impl State {
    fn send(&self, c: Captured) {
        if let (true, Some(out)) = (self.grabbed, &self.out) {
            let _ = out.send(c);
        }
    }
}

pub struct LinuxEngine {
    state: Arc<Mutex<State>>,
    x: Option<Arc<(RustConnection, u32)>>,
    /// Wayland: no client may read the pointer, not even through XWayland
    wayland: bool,
    virt: Mutex<Option<Virtual>>,
}

impl LinuxEngine {
    pub fn new() -> Self {
        let x = x11rb::connect(None).ok().map(|(c, n)| {
            let root = c.setup().roots[n].root;
            Arc::new((c, root))
        });
        let state = Arc::new(Mutex::new(State { pos: (HEADLESS.w / 2.0, HEADLESS.h / 2.0), screens: vec![HEADLESS], ..Default::default() }));
        let (s, xc) = (state.clone(), x.clone());
        let _ = std::thread::Builder::new().name("loka-input".into()).spawn(move || read(s, xc));
        Self { state, x, wayland: std::env::var_os("WAYLAND_DISPLAY").is_some(), virt: Mutex::new(None) }
    }

    /// The uinput devices, created on first use.
    fn with_virtual<T>(&self, f: impl FnOnce(&mut Virtual) -> io::Result<T>) -> io::Result<T> {
        let mut v = self.virt.lock().unwrap();
        if v.is_none() {
            *v = Some(Virtual::new()?);
        }
        f(v.as_mut().unwrap())
    }
}

impl SpatialPlatformEngine for LinuxEngine {
    fn prepare_overlay(&self, _: &WebviewWindow) -> tauri::Result<()> {
        Ok(()) // click-through needs a realized GTK window (tao panics before): place_overlay sets it after show
    }

    fn cursor(&self) -> Option<CursorSample> {
        if let Some(x) = self.x.as_ref().filter(|_| !self.wayland) {
            let r = x.0.query_pointer(x.1).ok()?.reply().ok()?;
            let button_down = u16::from(r.mask) & u16::from(x11rb::protocol::xproto::KeyButMask::BUTTON1) != 0;
            return Some(CursorSample { x: r.root_x as f64, y: r.root_y as f64, button_down });
        }
        let s = self.state.lock().unwrap();
        s.reading.then_some(CursorSample { x: s.pos.0, y: s.pos.1, button_down: s.left })
    }

    fn start_capture(&self, _park: (f64, f64), out: UnboundedSender<Captured>) -> Result<(), String> {
        // the way back needs uinput for the warp
        self.with_virtual(|_| Ok(())).map_err(|_| ACCESS.to_string())?;
        let (tx, rx) = mpsc::channel();
        {
            let mut s = self.state.lock().unwrap();
            if !s.reading {
                return Err(ACCESS.into());
            }
            s.out = Some(out);
            s.reply = Some(tx);
        }
        rx.recv_timeout(Duration::from_secs(3)).unwrap_or_else(|_| {
            let mut s = self.state.lock().unwrap();
            (s.out, s.reply) = (None, None);
            Err("the mouse and keyboard couldn't be taken over".into())
        })
    }

    fn stop_capture(&self, warp_to: (f64, f64)) {
        {
            let mut s = self.state.lock().unwrap();
            (s.out, s.reply) = (None, None); // the reader lets go of the devices within a tick
        }
        self.inject(Injected::MoveTo { x: warp_to.0, y: warp_to.1 });
    }

    fn inject(&self, e: Injected) {
        let ev = |t: EventType, code: u16, value: i32| evdev::InputEvent::new(t.0, code, value);
        let r = self.with_virtual(|v| match e {
            Injected::MoveTo { x, y } => {
                let b = {
                    let mut s = self.state.lock().unwrap();
                    s.pos = (x, y);
                    bounds(&s.screens)
                };
                // the desktop maps an absolute pointer's range onto all of its screens
                let at = |p: f64, o: f64, len: f64| ((p - o) / (len - 1.0).max(1.0) * ABS_MAX as f64).round().clamp(0.0, ABS_MAX as f64) as i32;
                let (ax, ay) = (AbsoluteAxisCode::ABS_X.0, AbsoluteAxisCode::ABS_Y.0);
                v.pointer.emit(&[ev(EventType::ABSOLUTE, ax, at(x, b.x, b.w)), ev(EventType::ABSOLUTE, ay, at(y, b.y, b.h))])
            }
            Injected::Button { button: button @ 1..=5, down } => v.keys.emit(&[ev(EventType::KEY, 0x10F + button as u16, down as i32)]),
            Injected::Button { .. } => Ok(()),
            Injected::Wheel { dx, dy } => {
                // high-resolution units for libinput, whole notches for X11's own drivers
                v.rest = (v.rest.0 + dx, v.rest.1 + dy);
                let notches = (v.rest.0 / 120, v.rest.1 / 120);
                v.rest = (v.rest.0 - notches.0 * 120, v.rest.1 - notches.1 * 120);
                let rel = |c: RelativeAxisCode, val| ev(EventType::RELATIVE, c.0, val);
                v.keys.emit(&[
                    rel(RelativeAxisCode::REL_WHEEL_HI_RES, dy),
                    rel(RelativeAxisCode::REL_HWHEEL_HI_RES, dx),
                    rel(RelativeAxisCode::REL_WHEEL, notches.1),
                    rel(RelativeAxisCode::REL_HWHEEL, notches.0),
                ])
            }
            Injected::Key { scan, down } => match code_of(scan) {
                Some(code) => v.keys.emit(&[ev(EventType::KEY, code, down as i32)]),
                None => Ok(()),
            },
        });
        if let Err(e) = r {
            eprintln!("uinput: {e}");
        }
    }

    fn show_cursor(&self, on: bool) {
        // our pointer device makes the desktop draw a cursor even with no mouse plugged in
        if on {
            let _ = self.with_virtual(|_| Ok(()));
        }
    }

    fn can_inject(&self) -> bool {
        self.with_virtual(|_| Ok(())).is_ok()
    }

    fn lock_key_down(&self) -> bool {
        self.state.lock().unwrap().lock_key
    }
}

/// Our input: an absolute pointer (moves land exactly, no second acceleration) and a keyboard with
/// the buttons and wheels.
struct Virtual {
    pointer: VirtualDevice,
    keys: VirtualDevice,
    /// wheel units short of a whole notch
    rest: (i32, i32),
}

impl Virtual {
    fn new() -> io::Result<Self> {
        let id = || InputId::new(BusType::BUS_VIRTUAL, 0x4C4F, 0x4B41, 1); // "LO" "KA"
        let buttons = || (0x110..=0x114).map(KeyCode::new);
        let abs = |c| UinputAbsSetup::new(c, AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0));
        let pointer = VirtualDevice::builder()?
            .name("LOKA pointer")
            .input_id(id())
            .with_keys(&buttons().collect::<AttributeSet<_>>())?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_X))?
            .with_absolute_axis(&abs(AbsoluteAxisCode::ABS_Y))?
            .build()?;
        let rel: AttributeSet<_> = [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ]
        .into_iter()
        .collect();
        let keys = VirtualDevice::builder()?
            .name("LOKA keyboard")
            .input_id(id())
            .with_keys(&(1..=248).map(KeyCode::new).chain(buttons()).collect::<AttributeSet<_>>())?
            .with_relative_axes(&rel)?
            .build()?;
        // the desktop needs a moment to pick new devices up; earlier events would be lost
        std::thread::sleep(Duration::from_millis(200));
        Ok(Self { pointer, keys, rest: (0, 0) })
    }
}

/// Ctrl and Alt held on any keyboard (for Ctrl+Alt+Esc).
#[derive(Default)]
struct Mods {
    ctrl: bool,
    alt: bool,
}

struct Pad {
    /// units per mm
    res: (f64, f64),
    at: (i32, i32),
    last: Option<(i32, i32)>,
    fingers: u8,
    /// most fingers of this touch, when it began, how far it went (mm), whether it clicked
    most: u8,
    since: Option<Instant>,
    travel: f64,
    clicked: bool,
    /// px and wheel units short of a whole one
    rest: (f64, f64),
    scroll: (f64, f64),
}

/// One mouse, keyboard or touchpad.
struct Input {
    dev: Device,
    path: PathBuf,
    /// REL_WHEEL_HI_RES comes along with REL_WHEEL: count only one
    hires: bool,
    pad: Option<Pad>,
    rel: (i32, i32),
    wheel: (i32, i32),
}

impl Input {
    fn new(dev: Device, path: PathBuf) -> Option<Self> {
        if dev.name().is_some_and(|n| n.starts_with("LOKA ")) || dev.properties().contains(PropType::DIRECT) {
            return None; // our own devices; touchscreens and tablets
        }
        let has = |k: KeyCode| dev.supported_keys().is_some_and(|s| s.contains(k));
        let rel = dev.supported_relative_axes();
        let mouse = rel.is_some_and(|r| r.contains(RelativeAxisCode::REL_X)) && has(KeyCode::BTN_LEFT);
        let keyboard = has(KeyCode::KEY_ESC) && has(KeyCode::KEY_A);
        let touchpad = has(KeyCode::BTN_TOOL_FINGER) && has(KeyCode::BTN_TOUCH) && dev.supported_absolute_axes().is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_X));
        if !(mouse || keyboard || touchpad) {
            return None;
        }
        let pad = touchpad.then(|| {
            let abs = dev.get_abs_state().ok();
            let res = |c: AbsoluteAxisCode| {
                let i = abs.map(|a| a[c.0 as usize]);
                match i {
                    Some(i) if i.resolution > 0 => i.resolution as f64,
                    Some(i) => ((i.maximum - i.minimum) as f64 / 100.0).max(1.0), // unknown: about 10 cm wide
                    None => 10.0,
                }
            };
            Pad {
                res: (res(AbsoluteAxisCode::ABS_X), res(AbsoluteAxisCode::ABS_Y)),
                at: (0, 0),
                last: None,
                fingers: 0,
                most: 0,
                since: None,
                travel: 0.0,
                clicked: false,
                rest: (0.0, 0.0),
                scroll: (0.0, 0.0),
            }
        });
        let hires = rel.is_some_and(|r| r.contains(RelativeAxisCode::REL_WHEEL_HI_RES));
        dev.set_nonblocking(true).ok()?;
        Some(Self { dev, path, hires, pad, rel: (0, 0), wheel: (0, 0) })
    }

    /// A key or button is down (resting touchpad fingers don't count).
    fn held(&self) -> bool {
        self.dev.get_key_state().is_ok_and(|k| k.iter().any(|c| !(0x140..=0x14F).contains(&c.0)))
    }

    /// Everything the device has for us; `false` once it is gone.
    fn pump(&mut self, state: &Mutex<State>, mods: &mut Mods) -> bool {
        let events: Vec<_> = match self.dev.fetch_events() {
            Ok(e) => e.collect(),
            Err(e) => return e.kind() == io::ErrorKind::WouldBlock,
        };
        let mut s = state.lock().unwrap();
        for e in events {
            let (code, v) = (e.code(), e.value());
            match e.event_type() {
                EventType::RELATIVE => match RelativeAxisCode(code) {
                    RelativeAxisCode::REL_X => self.rel.0 += v,
                    RelativeAxisCode::REL_Y => self.rel.1 += v,
                    RelativeAxisCode::REL_WHEEL if !self.hires => self.wheel.1 += v * 120,
                    RelativeAxisCode::REL_HWHEEL if !self.hires => self.wheel.0 += v * 120,
                    RelativeAxisCode::REL_WHEEL_HI_RES => self.wheel.1 += v,
                    RelativeAxisCode::REL_HWHEEL_HI_RES => self.wheel.0 += v,
                    _ => {}
                },
                EventType::ABSOLUTE => match (&mut self.pad, AbsoluteAxisCode(code)) {
                    (Some(p), AbsoluteAxisCode::ABS_X) => p.at.0 = v,
                    (Some(p), AbsoluteAxisCode::ABS_Y) => p.at.1 = v,
                    _ => {}
                },
                EventType::KEY => self.key(code, v, &mut s, mods),
                EventType::SYNCHRONIZATION if code == SynchronizationCode::SYN_REPORT.0 => self.frame(&mut s),
                _ => {}
            }
        }
        true
    }

    fn key(&mut self, code: u16, v: i32, s: &mut State, mods: &mut Mods) {
        let down = v != 0; // 2 = autorepeat: another press, as Windows sends it
        match code {
            0x14A => {
                // BTN_TOUCH
                let Some(p) = &mut self.pad else { return };
                p.last = None;
                if down {
                    (p.since, p.travel, p.most, p.clicked) = (Some(Instant::now()), 0.0, p.fingers, false);
                } else if let Some(since) = p.since.take() {
                    // a quick touch that barely moved: tap to click (two fingers: right click)
                    if !p.clicked && since.elapsed() < Duration::from_millis(180) && p.travel < 1.5 {
                        let button = p.most.clamp(1, 3);
                        s.send(Captured::Input(InputEvent::Button { button, down: true }));
                        s.send(Captured::Input(InputEvent::Button { button, down: false }));
                    }
                }
            }
            0x145 | 0x14D | 0x14E | 0x14F | 0x148 => {
                // BTN_TOOL_FINGER, DOUBLETAP, TRIPLETAP, QUADTAP, QUINTTAP
                let Some(p) = &mut self.pad else { return };
                if down {
                    p.fingers = match code {
                        0x145 => 1,
                        0x14D => 2,
                        0x14E => 3,
                        0x14F => 4,
                        _ => 5,
                    };
                    p.most = p.most.max(p.fingers);
                    p.last = None; // another finger count: no jump from the old position
                }
            }
            0x110..=0x114 if v != 2 => {
                if code == 0x110 {
                    s.left = down;
                }
                if let Some(p) = &mut self.pad {
                    p.clicked = true;
                }
                s.send(Captured::Input(InputEvent::Button { button: (code - 0x10F) as u8, down }));
            }
            _ => {
                let Some(scan) = scan_of(code) else { return };
                match scan {
                    0x1D | 0xE01D => mods.ctrl = down,
                    0x38 | 0xE038 => mods.alt = down,
                    super::LOCK_SCAN => s.lock_key = down,
                    _ => {}
                }
                s.send(if down && scan == 0x01 && mods.ctrl && mods.alt { Captured::Escape } else { Captured::Input(InputEvent::Key { scan, down }) });
            }
        }
    }

    /// End of one report: its motion goes out (driving) or moves the followed pointer (at home).
    fn frame(&mut self, s: &mut State) {
        let (dx, dy) = std::mem::take(&mut self.rel);
        let mut moved = (dx as f64, dy as f64);
        let mut wheel = std::mem::take(&mut self.wheel);
        if let Some(p) = &mut self.pad {
            if let (Some(_), Some(last)) = (p.since, p.last) {
                let mm = ((p.at.0 - last.0) as f64 / p.res.0, (p.at.1 - last.1) as f64 / p.res.1);
                p.travel += mm.0.hypot(mm.1);
                match p.fingers {
                    1 if s.grabbed => {
                        p.rest = (p.rest.0 + mm.0 * PAD_PX, p.rest.1 + mm.1 * PAD_PX);
                        moved = (p.rest.0.trunc(), p.rest.1.trunc());
                        p.rest = (p.rest.0.fract(), p.rest.1.fract());
                    }
                    1 => moved = (mm.0 * PAD_PX, mm.1 * PAD_PX),
                    2 => {
                        // content follows the fingers (natural scrolling, the touchpad default)
                        p.scroll = (p.scroll.0 - mm.0 * PAD_WHEEL, p.scroll.1 + mm.1 * PAD_WHEEL);
                        wheel = (wheel.0 + p.scroll.0.trunc() as i32, wheel.1 + p.scroll.1.trunc() as i32);
                        p.scroll = (p.scroll.0.fract(), p.scroll.1.fract());
                    }
                    _ => {}
                }
            }
            p.last = p.since.map(|_| p.at);
        }
        if moved != (0.0, 0.0) {
            if s.grabbed {
                s.send(Captured::Input(InputEvent::Move { dx: moved.0 as i32, dy: moved.1 as i32 }));
            } else {
                // shortcut: followed 1:1, without the desktop's pointer acceleration, so on Wayland the spot along
                // an edge drifts until the pointer is pushed into a corner; model libinput's acceleration if it matters
                s.pos = nearest(&s.screens, s.pos.0 + moved.0, s.pos.1 + moved.1);
            }
        }
        if wheel != (0, 0) {
            s.send(Captured::Input(InputEvent::Wheel { dx: wheel.0, dy: wheel.1 }));
        }
    }
}

/// The reading thread: opens devices as they come, grabs them while driving, follows the pointer.
fn read(state: Arc<Mutex<State>>, x: Option<Arc<(RustConnection, u32)>>) {
    let mut inputs: Vec<Input> = Vec::new();
    let (mut scanned, mut asked) = (None::<Instant>, None::<Instant>);
    let mut mods = Mods::default();
    loop {
        if scanned.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            scanned = Some(Instant::now());
            let grabbed = state.lock().unwrap().grabbed;
            scan(&mut inputs, grabbed);
            let screens = x.as_ref().and_then(|x| monitors(&x.0, x.1)).unwrap_or_else(|| vec![HEADLESS]);
            let mut s = state.lock().unwrap();
            s.reading = !inputs.is_empty();
            s.pos = nearest(&screens, s.pos.0, s.pos.1);
            s.screens = screens;
        }
        {
            let mut s = state.lock().unwrap();
            if s.out.is_some() && !s.grabbed {
                let since = *asked.get_or_insert_with(Instant::now);
                // a key still down when grabbing stays down for the desktop, which never sees its release
                if since.elapsed() > Duration::from_secs(1) || !inputs.iter().any(Input::held) {
                    let r = grab(&mut inputs);
                    s.grabbed = r.is_ok();
                    if !s.grabbed {
                        s.out = None;
                    }
                    if let Some(reply) = s.reply.take() {
                        let _ = reply.send(r);
                    }
                    asked = None;
                }
            } else if s.out.is_none() {
                asked = None;
            }
            // the session task died or hangs: give input back without it
            if s.grabbed && (s.out.as_ref().is_none_or(|o| o.is_closed()) || super::starved()) {
                if let Some(out) = s.out.take() {
                    let _ = out.send(Captured::Escape);
                }
                s.grabbed = false;
                for i in &mut inputs {
                    let _ = i.dev.ungrab();
                }
            }
        }
        let mut fds: Vec<libc::pollfd> = inputs.iter().map(|i| libc::pollfd { fd: i.dev.as_raw_fd(), events: libc::POLLIN, revents: 0 }).collect();
        if fds.is_empty() {
            std::thread::sleep(Duration::from_millis(15));
        } else {
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 15) };
        }
        inputs.retain_mut(|i| i.pump(&state, &mut mods));
    }
}

/// Open the input devices that appeared since the last look.
fn scan(inputs: &mut Vec<Input>, grabbed: bool) {
    let Ok(dir) = std::fs::read_dir("/dev/input") else { return };
    for path in dir.flatten().map(|e| e.path()) {
        if !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event")) || inputs.iter().any(|i| i.path == path) {
            continue;
        }
        let Some(mut input) = Device::open(&path).ok().and_then(|d| Input::new(d, path)) else { continue };
        if grabbed {
            let _ = input.dev.grab();
        }
        inputs.push(input);
    }
}

/// Every device ours, or none.
fn grab(inputs: &mut [Input]) -> Result<(), String> {
    if inputs.is_empty() {
        return Err(ACCESS.into());
    }
    for k in 0..inputs.len() {
        if let Err(e) = inputs[k].dev.grab() {
            for i in &mut inputs[..k] {
                let _ = i.dev.ungrab();
            }
            let name = inputs[k].dev.name().unwrap_or("an input device").to_string();
            return Err(format!("another program holds {name} ({e})"));
        }
    }
    Ok(())
}

/// The X screen's monitors (XWayland's too: the same space as Tauri's monitors).
fn monitors(c: &RustConnection, root: u32) -> Option<Vec<Rect>> {
    let m = c.randr_get_monitors(root, true).ok()?.reply().ok()?;
    let v: Vec<Rect> = m.monitors.iter().map(|m| Rect { x: m.x as f64, y: m.y as f64, w: m.width as f64, h: m.height as f64 }).collect();
    (!v.is_empty()).then_some(v)
}

/// Linux key codes that aren't their set-1 scancode (codes 1-88 are, but Num Lock and Pause, which
/// Windows swaps).
const KEYS: &[(u16, u16)] = &[
    (69, 0xE045), (119, 0x45), (96, 0xE01C), (97, 0xE01D), (98, 0xE035), (99, 0xE037), (100, 0xE038), (102, 0xE047),
    (103, 0xE048), (104, 0xE049), (105, 0xE04B), (106, 0xE04D), (107, 0xE04F), (108, 0xE050), (109, 0xE051), (110, 0xE052),
    (111, 0xE053), (113, 0xE020), (114, 0xE02E), (115, 0xE030), (116, 0xE05E), (117, 0x59), (121, 0x7E), (124, 0x7D),
    (125, 0xE05B), (126, 0xE05C), (127, 0xE05D), (89, 0x73), (92, 0x79), (93, 0x70), (94, 0x7B), (142, 0xE05F),
    (143, 0xE063), (140, 0xE021), (155, 0xE06C), (156, 0xE066), (158, 0xE06A), (159, 0xE069), (163, 0xE019), (164, 0xE022),
    (165, 0xE010), (166, 0xE024), (172, 0xE032), (173, 0xE067), (217, 0xE065), (183, 0x64), (184, 0x65), (185, 0x66),
    (186, 0x67), (187, 0x68), (188, 0x69), (189, 0x6A), (190, 0x6B), (191, 0x6C), (192, 0x6D), (193, 0x6E), (194, 0x76),
];

fn scan_of(code: u16) -> Option<u16> {
    KEYS.iter().find(|(c, _)| *c == code).map(|(_, s)| *s).or(matches!(code, 1..=88).then_some(code).filter(|&c| !(84..=85).contains(&c)))
}

fn code_of(scan: u16) -> Option<u16> {
    KEYS.iter().find(|(_, s)| *s == scan).map(|(c, _)| *c).or(matches!(scan, 1..=88).then_some(scan).filter(|&s| !(84..=85).contains(&s)))
}

/// Give this user's apps the input devices and uinput: an `input` group rule, the user in it, and
/// right away (no new login) an ACL. Asks for the password with pkexec unless already root.
pub fn allow_input() -> Result<(), String> {
    const SCRIPT: &str = r#"set -e
u="$1"
modprobe uinput 2>/dev/null || true
echo uinput > /etc/modules-load.d/loka.conf
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' > /etc/udev/rules.d/60-loka.rules
getent group input >/dev/null || groupadd -r input
usermod -aG input "$u"
udevadm control --reload-rules 2>/dev/null || true
chgrp input /dev/uinput && chmod 0660 /dev/uinput
setfacl -m "u:$u:rw" /dev/uinput /dev/input/event* 2>/dev/null || true
"#;
    let user = std::env::var("SUDO_USER").or_else(|_| std::env::var("USER")).map_err(|_| "no user name (USER isn't set)".to_string())?;
    let mut cmd = if unsafe { libc::geteuid() } == 0 {
        Command::new("sh")
    } else {
        let mut c = Command::new("pkexec");
        c.arg("sh");
        c
    };
    let status = cmd.args(["-c", SCRIPT, "sh", &user]).status().map_err(|e| format!("couldn't ask for the password ({e})"))?;
    if status.success() {
        Ok(())
    } else {
        Err("not allowed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_go_both_ways() {
        for code in (1..=88).chain(KEYS.iter().map(|(c, _)| *c)).filter(|c| !(84..=85).contains(c)) {
            let scan = scan_of(code).unwrap();
            assert_eq!(code_of(scan), Some(code), "code {code} → scan {scan:#x}");
        }
        assert_eq!(scan_of(1), Some(0x01)); // Esc
        assert_eq!(scan_of(70), Some(super::super::LOCK_SCAN)); // Scroll Lock
        assert_eq!(scan_of(125), Some(0xE05B)); // left Super = Win/⌘
        assert_eq!(code_of(0x45), Some(119)); // Windows' 0x45 is Pause
        assert_eq!(scan_of(85), None);
    }
}
