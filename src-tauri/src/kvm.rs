//! Shared mouse & keyboard. Pushing the cursor into a portal hands input to the paired device behind it;
//! pushing it out of that device's opposite edge (or Ctrl+Alt+Esc) hands it back. Any paired device can
//! drive any other; links are streams of pinned QUIC connections (`network::secure`).
//!
//! Wire: fixed 9-byte frames `tag | payload (8 bytes, LE)`. A Clipboard frame is followed by `len` bytes
//! of UTF-8: the clipboard travels with the cursor (driver → driven on Enter, back on Return). Pairing
//! frames are followed by 32 bytes: commit, nonce, reveal.

use crate::{
    network::secure::{self, Stream, Tls},
    platform::{clipboard_text, edge_point, set_clipboard_text, Captured, Edge, Gesture, Injected, InputEvent, Portal, Rect, SpatialPlatformEngine, LOCK_SCAN},
};
use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, WriteHalf},
    sync::{
        mpsc::{self, unbounded_channel, UnboundedReceiver},
        Notify,
    },
    task::JoinHandle,
    time::timeout,
};

pub const PORT: u16 = 53321;
/// Wire version, announced by discovery: devices with another one can't pair or connect.
pub const PROTOCOL: u32 = 2;
const FRAME: usize = 9;
/// A stalled peer must never keep this machine's input hostage.
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);
/// Where the cursor lands when control comes back, so it does not immediately re-enter.
const RETURN_INSET: f64 = 12.0;
/// Where the cursor appears on the receiver: far enough inside that hand jitter right after
/// crossing does not bounce control straight back.
const ENTER_INSET: f64 = 8.0;
/// Larger clipboard text stays home (also the receiver's limit).
const MAX_CLIPBOARD: u32 = 1 << 20;
/// Sender: clipboard text last exchanged with a Client, so only new copies travel and an
/// unchanged clipboard never overwrites the other side's (e.g. an image there).
static LAST_SHARED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Msg {
    /// Sender's portal edge and position along it.
    Enter { edge: Edge, y: f32 },
    Input(InputEvent),
    /// Receiver → sender: cursor left through the entry edge at `y` (fraction along that edge).
    Return { y: f32 },
    Leave,
    /// Ask the other device's user to pair with this one; followed by SHA-256 of the asker's nonce.
    PairRequest,
    /// Answerer → asker, right away: followed by the answerer's nonce.
    PairNonce,
    /// Asker → answerer: followed by the asker's nonce, which must match its commitment.
    PairReveal,
    PairResponse(PairAnswer),
    /// Clipboard text follows: `len` bytes of UTF-8.
    Clipboard { len: u32 },
    /// Driven → driver: session turned down, so the driver can say why instead of "closed".
    Refuse(Refusal),
    /// Driver → driven: cursor lock on/off; while on, the way back is not watched.
    Lock { on: bool },
    /// A file drag crosses with the cursor: its files come as transfer batch `batch` and become a
    /// native drag on the other side. Driver → driven right after Enter; back right before Return.
    Drag { batch: u64 },
    /// First frame of a connection for one of the device's services; it answers one byte (1 = shared)
    /// and from then on the stream belongs to that service.
    Open(Service),
}

/// What a paired device can open on this one, each only if this one's user shares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Service {
    Shell = 1,
    Files = 2,
    Sound = 3,
    Notifications = 4,
    Mic = 5,
}

impl Service {
    fn from_u8(b: u8) -> Option<Self> {
        [Service::Shell, Service::Files, Service::Sound, Service::Notifications, Service::Mic].into_iter().find(|s| *s as u8 == b)
    }

    fn not_shared(self) -> &'static str {
        match self {
            Service::Shell => "the other computer isn't sharing its terminal",
            Service::Files => "the other computer isn't sharing a folder",
            Service::Sound => "the other computer isn't sharing its sound",
            Service::Notifications => "the other computer isn't sharing its notifications",
            Service::Mic => "the other computer isn't sharing its microphone",
        }
    }
}

/// How the asked device answered a pairing request.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PairAnswer {
    Declined = 0,
    Accepted = 1,
    /// its user didn't answer in time
    NoAnswer = 2,
    /// it doesn't see the asking device on the network, so it can't tell who is asking
    Unknown = 3,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Refusal {
    NotPaired = 1,
    NoScreen = 2,
}

impl Refusal {
    fn text(self) -> &'static str {
        match self {
            Refusal::NotPaired => "the other device isn't paired with this one, pair them again",
            Refusal::NoScreen => "the other device has no screen to show the cursor on",
        }
    }
}

fn edge_byte(e: Edge) -> u8 {
    match e {
        Edge::Left => 0,
        Edge::Right => 1,
        Edge::Top => 2,
        Edge::Bottom => 3,
    }
}

impl Msg {
    pub fn encode(&self) -> [u8; FRAME] {
        let mut b = [0u8; FRAME];
        let mut put = |at: usize, bytes: &[u8]| b[at..at + bytes.len()].copy_from_slice(bytes);
        match *self {
            Msg::Enter { edge, y } => {
                put(0, &[1, edge_byte(edge)]);
                put(2, &y.to_le_bytes());
            }
            Msg::Input(InputEvent::Move { dx, dy }) => {
                put(0, &[2]);
                put(1, &dx.to_le_bytes());
                put(5, &dy.to_le_bytes());
            }
            Msg::Input(InputEvent::Button { button, down }) => put(0, &[3, button, down as u8]),
            Msg::Input(InputEvent::Wheel { dx, dy }) => {
                put(0, &[4]);
                put(1, &dx.to_le_bytes());
                put(5, &dy.to_le_bytes());
            }
            Msg::Input(InputEvent::Key { scan, down }) => {
                put(0, &[5]);
                put(1, &scan.to_le_bytes());
                put(3, &[down as u8]);
            }
            Msg::Input(InputEvent::Gesture { gesture }) => put(0, &[14, gesture as u8]),
            Msg::Return { y } => {
                put(0, &[6]);
                put(1, &y.to_le_bytes());
            }
            Msg::Leave => put(0, &[7]),
            Msg::PairRequest => put(0, &[8]),
            Msg::PairResponse(a) => put(0, &[9, a as u8]),
            Msg::Clipboard { len } => {
                put(0, &[10]);
                put(1, &len.to_le_bytes());
            }
            Msg::Refuse(r) => put(0, &[11, r as u8]),
            Msg::Lock { on } => put(0, &[12, on as u8]),
            Msg::Drag { batch } => {
                put(0, &[13]);
                put(1, &batch.to_le_bytes());
            }
            Msg::Open(s) => put(0, &[15, s as u8]),
            Msg::PairNonce => put(0, &[16]),
            Msg::PairReveal => put(0, &[17]),
        }
        b
    }

    pub fn decode(b: &[u8; FRAME]) -> Option<Msg> {
        let i32_at = |at: usize| i32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        let f32_at = |at: usize| Some(f32::from_le_bytes(b[at..at + 4].try_into().unwrap())).filter(|v| (0.0..=1.0).contains(v));
        Some(match b[0] {
            1 => Msg::Enter {
                edge: match b[1] {
                    0 => Edge::Left,
                    1 => Edge::Right,
                    2 => Edge::Top,
                    3 => Edge::Bottom,
                    _ => return None,
                },
                y: f32_at(2)?,
            },
            2 => Msg::Input(InputEvent::Move { dx: i32_at(1), dy: i32_at(5) }),
            3 if (1..=5).contains(&b[1]) => Msg::Input(InputEvent::Button { button: b[1], down: b[2] != 0 }),
            4 => Msg::Input(InputEvent::Wheel { dx: i32_at(1), dy: i32_at(5) }),
            5 => Msg::Input(InputEvent::Key { scan: u16::from_le_bytes([b[1], b[2]]), down: b[3] != 0 }),
            6 => Msg::Return { y: f32_at(1)? },
            7 => Msg::Leave,
            8 => Msg::PairRequest,
            9 => Msg::PairResponse(
                [PairAnswer::Declined, PairAnswer::Accepted, PairAnswer::NoAnswer, PairAnswer::Unknown].into_iter().find(|a| *a as u8 == b[1])?,
            ),
            10 => Msg::Clipboard { len: Some(u32::from_le_bytes(b[1..5].try_into().unwrap())).filter(|l| *l <= MAX_CLIPBOARD)? },
            11 => Msg::Refuse(match b[1] {
                1 => Refusal::NotPaired,
                2 => Refusal::NoScreen,
                _ => return None,
            }),
            12 => Msg::Lock { on: b[1] != 0 },
            13 => Msg::Drag { batch: u64::from_le_bytes(b[1..9].try_into().unwrap()) },
            14 => Msg::Input(InputEvent::Gesture { gesture: Gesture::from_u8(b[1])? }),
            15 => Msg::Open(Service::from_u8(b[1])?),
            16 => Msg::PairNonce,
            17 => Msg::PairReveal,
            _ => return None,
        })
    }
}

async fn read_msg(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Msg> {
    let mut b = [0u8; FRAME];
    r.read_exact(&mut b).await?;
    Msg::decode(&b).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad kvm frame"))
}

/// Frame reader that can sit in a `select!`: a frame half-read when another branch wins stays buffered
/// for the next call (`read` is cancel-safe, `read_exact` is not).
#[derive(Default)]
struct Frames {
    buf: [u8; FRAME],
    filled: usize,
}

impl Frames {
    async fn next(&mut self, r: &mut (impl AsyncRead + Unpin)) -> io::Result<Msg> {
        while self.filled < FRAME {
            match r.read(&mut self.buf[self.filled..]).await? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => self.filled += n,
            }
        }
        self.filled = 0;
        Msg::decode(&self.buf).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad kvm frame"))
    }
}

/// Send `text` as a Clipboard frame; nothing for an empty or oversized clipboard.
async fn write_clipboard(w: &mut (impl AsyncWrite + Unpin), text: Option<String>) -> io::Result<()> {
    let Some(text) = text.filter(|t| t.len() <= MAX_CLIPBOARD as usize) else { return Ok(()) };
    let mut frame = Msg::Clipboard { len: text.len() as u32 }.encode().to_vec();
    frame.extend_from_slice(text.as_bytes());
    w.write_all(&frame).await?;
    w.flush().await
}

/// The bytes after a Clipboard frame; `None` if they are not UTF-8 (still consumed, so framing holds).
async fn read_clipboard(r: &mut (impl AsyncRead + Unpin), len: u32) -> io::Result<Option<String>> {
    let mut b = vec![0; len as usize];
    r.read_exact(&mut b).await?;
    Ok(String::from_utf8(b).ok())
}

/// Receiver-side cursor: absolute position on `screens` (all of the receiver's monitors), entered through
/// `edge` of their bounding box.
pub struct Receiver {
    screens: Vec<Rect>,
    /// bounding box of `screens`: its `edge` is the way back
    area: Rect,
    edge: Edge,
    x: f64,
    y: f64,
    buttons: u8,
    keys: Vec<u16>,
    /// cursor lock: the way back is not watched
    locked: bool,
    /// a file drag here is going back to the Main with the cursor (transfer batch)
    drag_out: Option<u64>,
}

#[derive(Debug, PartialEq)]
pub enum Applied {
    Inject(Injected),
    /// Hand control back; fraction along the entry edge.
    Return(f32),
    /// several events (a gesture's shortcut)
    Many(Vec<Injected>),
}

/// Client: a file drag here was handed to the Main (transfer batch id); the next push through the
/// entry edge returns even with the button held. 0 = none.
pub static DRAG_OUT: AtomicU64 = AtomicU64::new(0);

/// Bounding box of `screens`.
pub fn bounds(screens: &[Rect]) -> Rect {
    let x = screens.iter().map(|s| s.x).fold(f64::INFINITY, f64::min);
    let y = screens.iter().map(|s| s.y).fold(f64::INFINITY, f64::min);
    let r = screens.iter().map(|s| s.x + s.w).fold(f64::NEG_INFINITY, f64::max);
    let b = screens.iter().map(|s| s.y + s.h).fold(f64::NEG_INFINITY, f64::max);
    Rect { x, y, w: r - x, h: b - y }
}

/// The pixel on some screen closest to `(x, y)`: the cursor never sits in the gaps between monitors.
fn nearest(screens: &[Rect], x: f64, y: f64) -> (f64, f64) {
    screens
        .iter()
        .map(|s| (x.clamp(s.x, s.x + s.w - 1.0), y.clamp(s.y, s.y + s.h - 1.0)))
        .min_by(|a, b| ((a.0 - x).powi(2) + (a.1 - y).powi(2)).total_cmp(&((b.0 - x).powi(2) + (b.1 - y).powi(2))))
        .unwrap_or((x, y))
}

impl Receiver {
    /// The sender's right edge meets our left edge, etc.
    pub fn enter(screens: Vec<Rect>, sender_edge: Edge, y: f32) -> (Self, Injected) {
        let edge = sender_edge.opposite();
        let area = bounds(&screens);
        let (x, y) = edge_point(area, edge, y as f64, ENTER_INSET);
        let (x, y) = nearest(&screens, x, y);
        (Self { screens, area, edge, x, y, buttons: 0, keys: Vec::new(), locked: false, drag_out: None }, Injected::MoveTo { x, y })
    }

    pub fn apply(&mut self, e: InputEvent) -> Applied {
        match e {
            InputEvent::Move { dx, dy } => {
                let (nx, ny) = (self.x + dx as f64, self.y + dy as f64);
                let a = self.area;
                let crossed = match self.edge {
                    Edge::Left => nx < a.x,
                    Edge::Right => nx > a.x + a.w - 1.0,
                    Edge::Top => ny < a.y,
                    Edge::Bottom => ny > a.y + a.h - 1.0,
                };
                (self.x, self.y) = nearest(&self.screens, nx, ny);
                // never hand back mid-drag (the button-up would land on the wrong machine) unless the drag
                // itself goes along, and never while locked
                if crossed && (self.buttons == 0 || self.drag_out.is_some()) && !self.locked {
                    return Applied::Return(self.along());
                }
                Applied::Inject(Injected::MoveTo { x: self.x, y: self.y })
            }
            InputEvent::Button { button, down } => {
                let bit = 1 << (button - 1);
                if down { self.buttons |= bit } else { self.buttons &= !bit }
                Applied::Inject(Injected::Button { button, down })
            }
            InputEvent::Wheel { dx, dy } => Applied::Inject(Injected::Wheel { dx, dy }),
            InputEvent::Key { scan, down } => {
                self.keys.retain(|k| *k != scan);
                if down {
                    self.keys.push(scan);
                }
                Applied::Inject(Injected::Key { scan, down })
            }
            InputEvent::Gesture { gesture } => Applied::Many(gesture_keys(gesture, cfg!(target_os = "macos"))),
        }
    }

    /// Cursor position as a fraction along the entry edge.
    fn along(&self) -> f32 {
        let a = self.area;
        let f = match self.edge {
            Edge::Left | Edge::Right => (self.y - a.y) / a.h,
            Edge::Top | Edge::Bottom => (self.x - a.x) / a.w,
        };
        f.clamp(0.0, 1.0) as f32
    }

    /// Put the cursor at `(x, y)` (back from a device further along the chain).
    fn place(&mut self, x: f64, y: f64) -> Injected {
        (self.x, self.y) = nearest(&self.screens, x, y);
        Injected::MoveTo { x: self.x, y: self.y }
    }

    /// Key/button ups for everything still held, so nothing sticks when the session ends.
    pub fn release_all(&mut self) -> Vec<Injected> {
        let mut out: Vec<Injected> = self.keys.drain(..).rev().map(|scan| Injected::Key { scan, down: false }).collect();
        for b in 1..=5u8 {
            if self.buttons & (1 << (b - 1)) != 0 {
                out.push(Injected::Button { button: b, down: false });
            }
        }
        self.buttons = 0;
        out
    }
}

/// The shortcut the receiving OS uses for a trackpad gesture.
pub fn gesture_keys(g: Gesture, mac: bool) -> Vec<Injected> {
    const CTRL: u16 = 0x1D;
    const WIN: u16 = 0xE05B; // ⌘ on a Mac
    let chord = |mods: &[u16], key: u16| {
        let mut v: Vec<Injected> = mods.iter().map(|&scan| Injected::Key { scan, down: true }).collect();
        v.extend([Injected::Key { scan: key, down: true }, Injected::Key { scan: key, down: false }]);
        v.extend(mods.iter().rev().map(|&scan| Injected::Key { scan, down: false }));
        v
    };
    let ctrl_wheel = |dy| vec![Injected::Key { scan: CTRL, down: true }, Injected::Wheel { dx: 0, dy }, Injected::Key { scan: CTRL, down: false }];
    match (g, mac) {
        (Gesture::SpaceLeft, true) => chord(&[CTRL], 0xE04B),
        (Gesture::SpaceRight, true) => chord(&[CTRL], 0xE04D),
        (Gesture::Overview, true) => chord(&[CTRL], 0xE048),
        (Gesture::AppWindows, true) => chord(&[CTRL], 0xE050),
        (Gesture::ShowDesktop, true) => chord(&[], 0x57), // F11
        (Gesture::ZoomIn, true) => chord(&[WIN], 0x0D), // ⌘=
        (Gesture::ZoomOut, true) => chord(&[WIN], 0x0C), // ⌘-
        (Gesture::SpaceLeft, false) => chord(&[WIN, CTRL], 0xE04B),
        (Gesture::SpaceRight, false) => chord(&[WIN, CTRL], 0xE04D),
        (Gesture::Overview | Gesture::AppWindows, false) => chord(&[WIN], 0x0F), // Task View
        (Gesture::ShowDesktop, false) => chord(&[WIN], 0x20), // Win+D
        (Gesture::ZoomIn, false) => ctrl_wheel(120),
        (Gesture::ZoomOut, false) => ctrl_wheel(-120),
    }
}

/// Cursor lock (Scroll Lock / F14): the pointer stays on the device it is on. Set at home by the
/// tracker, while driving by `send_session` from `Tune`; any session end clears it.
pub static LOCKED: AtomicBool = AtomicBool::new(false);

/// Sender-side shaping of captured input before it goes out.
pub struct Tune {
    speed: f32,
    swap_ctrl_cmd: bool,
    /// sub-pixel motion left over after scaling, so slow speeds still move
    rest: (f32, f32),
    locked: bool,
    /// double-tap Ctrl: when the Ctrl now held went down (and nothing else since), when the last clean tap ended
    ctrl_down: Option<(Instant, bool)>,
    tapped: Option<Instant>,
}

/// Both Ctrl taps of the "come home" double tap within this.
const TAP: Duration = Duration::from_millis(400);

impl Tune {
    /// A session never starts locked: the tracker does not cross while the lock is on.
    pub fn new(speed: f32, swap_ctrl_cmd: bool) -> Self {
        Self { speed: speed.clamp(0.25, 4.0), swap_ctrl_cmd, rest: (0.0, 0.0), locked: false, ctrl_down: None, tapped: None }
    }

    /// `true` when `e` completes a double tap of Ctrl (alone: no other key, click or scroll in between),
    /// which brings the cursor home. Auto-repeat of a held Ctrl is not a tap.
    pub fn home(&mut self, e: &InputEvent, now: Instant) -> bool {
        match *e {
            InputEvent::Move { .. } => {}
            InputEvent::Key { scan: 0x1D | 0xE01D, down: true } => match self.ctrl_down {
                Some(_) => {} // auto-repeat
                None if self.tapped.is_some_and(|t| now - t < TAP) => {
                    self.tapped = None;
                    return true;
                }
                None => self.ctrl_down = Some((now, true)),
            },
            InputEvent::Key { scan: 0x1D | 0xE01D, down: false } => {
                self.tapped = self.ctrl_down.take().filter(|(at, clean)| *clean && now - *at < TAP).map(|_| now);
            }
            _ => {
                self.tapped = None;
                if let Some((_, clean)) = &mut self.ctrl_down {
                    *clean = false;
                }
            }
        }
        false
    }

    /// The frame for `e`, if any: moves scaled, Ctrl↔⌘ swapped, the lock key toggling the lock.
    pub fn shape(&mut self, e: InputEvent) -> Option<Msg> {
        Some(match e {
            InputEvent::Move { dx, dy } => {
                let (fx, fy) = (dx as f32 * self.speed + self.rest.0, dy as f32 * self.speed + self.rest.1);
                let (mx, my) = (fx.trunc(), fy.trunc());
                self.rest = (fx - mx, fy - my);
                if mx == 0.0 && my == 0.0 {
                    return None;
                }
                Msg::Input(InputEvent::Move { dx: mx as i32, dy: my as i32 })
            }
            InputEvent::Key { scan: LOCK_SCAN, down } => {
                if !down {
                    return None;
                }
                self.locked = !self.locked;
                Msg::Lock { on: self.locked }
            }
            InputEvent::Key { scan, down } if self.swap_ctrl_cmd => {
                // Ctrl sits where ⌘ does in every shortcut; the Win key (⌘'s spot) takes Ctrl's place
                let scan = match scan {
                    0x1D => 0xE05B,
                    0xE01D => 0xE05C,
                    0xE05B => 0x1D,
                    0xE05C => 0xE01D,
                    s => s,
                };
                Msg::Input(InputEvent::Key { scan, down })
            }
            e => Msg::Input(e),
        })
    }
}

/// Sender side: capture local input and stream it until the peer returns control, Ctrl+Alt+Esc, or an error.
/// Local input is always restored before returning. `drag`: a file drag crossing along (its transfer batch).
/// Returns the batch of a file drag the peer sent back with the cursor, if any.
pub async fn send_session(
    tls: &Tls,
    engine: Arc<dyn SpatialPlatformEngine>,
    to: SocketAddr,
    expect: &str,
    portal: Portal,
    y: f32,
    mut tune: Tune,
    drag: Option<u64>,
) -> io::Result<Option<u64>> {
    let (s, _) = tls.connect(to, Some(expect)).await?;
    let (mut rd, mut wr) = tokio::io::split(s);
    let (tx, mut rx) = unbounded_channel();
    let m = portal.monitor;
    crate::platform::feed();
    // before Enter: when capture fails (a Mac without Accessibility), the peer's cursor must not jump to its edge
    engine.start_capture((m.x + m.w / 2.0, m.y + m.h / 2.0), tx).map_err(io::Error::other)?;
    let mut alive = tokio::time::interval(Duration::from_millis(500));

    // reading in its own task keeps frames intact when the select below picks the other branch
    // shortcut: the Client's clipboard only comes back on an edge return, not on Ctrl+Alt+Esc; add a Leave ack if needed.
    // a silently dead peer (QUIC idle timeout) stays TimedOut, so the user is told it stopped answering
    let closed = |e: io::Error| if e.kind() == io::ErrorKind::TimedOut { e } else { io::Error::new(io::ErrorKind::ConnectionAborted, "peer closed the session") };
    let mut returned = tokio::spawn(async move {
        let mut drag = None;
        loop {
            match read_msg(&mut rd).await {
                Ok(Msg::Return { y }) => return Ok((y, drag)),
                Ok(Msg::Drag { batch }) => drag = Some(batch),
                Ok(Msg::Refuse(r)) => return Err(io::Error::new(io::ErrorKind::PermissionDenied, r.text())),
                Ok(Msg::Clipboard { len }) => match read_clipboard(&mut rd, len).await {
                    Ok(Some(text)) => {
                        set_clipboard_text(&text);
                        *LAST_SHARED.lock().unwrap() = Some(text);
                    }
                    Ok(None) => {}
                    Err(e) => return Err(closed(e)),
                },
                Ok(_) => {}
                Err(e) => return Err(closed(e)),
            }
        }
    });
    let (mut back_y, mut back_drag) = (y, None);
    let ended = END.notified();
    tokio::pin!(ended);
    ended.as_mut().enable();
    let result: io::Result<()> = async {
        timeout(WRITE_TIMEOUT, send(&mut wr, Msg::Enter { edge: portal.edge, y })).await??;
        if let Some(batch) = drag {
            timeout(WRITE_TIMEOUT, send(&mut wr, Msg::Drag { batch })).await??;
        }
        // after capture is on: reading the clipboard (it may wait for a picture being read) never holds the cursor at the edge
        let fresh = clipboard_text().filter(|t| LAST_SHARED.lock().unwrap().as_ref() != Some(t));
        timeout(WRITE_TIMEOUT, write_clipboard(&mut wr, fresh.clone())).await??;
        if fresh.is_some() {
            *LAST_SHARED.lock().unwrap() = fresh;
        }
        // an event read past a run of moves, handled next turn
        let mut held: Option<Captured> = None;
        loop {
            let c = match held.take() {
                Some(c) => Some(c),
                None => tokio::select! {
                    c = rx.recv() => c,
                    r = &mut returned => {
                        (back_y, back_drag) = r.unwrap_or_else(|e| Err(closed(io::Error::other(e))))?;
                        return Ok(());
                    }
                    _ = &mut ended => Some(Captured::Escape),
                    _ = alive.tick() => {
                        crate::platform::feed();
                        continue;
                    }
                },
            };
            let e = match c {
                Some(Captured::Input(InputEvent::Move { mut dx, mut dy })) => {
                    // moves that piled up while a write was slow go as one: the pointer never replays old motion
                    while let Ok(next) = rx.try_recv() {
                        match next {
                            Captured::Input(InputEvent::Move { dx: x, dy: y }) => (dx, dy) = (dx.saturating_add(x), dy.saturating_add(y)),
                            other => {
                                held = Some(other);
                                break;
                            }
                        }
                    }
                    InputEvent::Move { dx, dy }
                }
                Some(Captured::Input(e)) => e,
                Some(Captured::Escape) | None => {
                    let _ = timeout(WRITE_TIMEOUT, send(&mut wr, Msg::Leave)).await;
                    return Ok(());
                }
            };
            if tune.home(&e, Instant::now()) {
                let _ = timeout(WRITE_TIMEOUT, send(&mut wr, Msg::Leave)).await;
                return Ok(());
            }
            if let Some(msg) = tune.shape(e) {
                if let Msg::Lock { on } = msg {
                    LOCKED.store(on, Ordering::SeqCst);
                }
                timeout(WRITE_TIMEOUT, send(&mut wr, msg)).await??;
            }
        }
    }
    .await;
    LOCKED.store(false, Ordering::SeqCst);
    returned.abort();
    engine.stop_capture(portal.point(back_y, RETURN_INSET));
    result.map(|()| back_drag)
}

/// Where received input goes: the real OS, or the simulator window.
pub trait Sink: Send {
    /// Every monitor of this device, global physical px.
    fn screens(&self) -> Vec<Rect>;
    fn inject(&mut self, e: Injected);
    /// Clipboard text to hand back with control; the simulator has none of its own.
    fn clipboard(&self) -> Option<String> {
        None
    }
    fn set_clipboard(&mut self, _text: String) {}
    /// A file drag came along with the cursor; its files arrive as transfer batch `batch`.
    fn drag(&mut self, _batch: u64) {}
    /// One of this device's own portals the cursor is pushing against at `(x, y)`: the way on to a third
    /// device (a chain). Never back to the device driving this one.
    fn hop(&self, _x: f64, _y: f64) -> Option<Hop> {
        None
    }
}

/// The next device of a chain: where it is, and the portal (and fraction along it) the cursor leaves through.
pub struct Hop {
    pub to: SocketAddr,
    pub fp: String,
    pub portal: Portal,
    pub y: f32,
}

/// Running sessions end on this: driving ones like Ctrl+Alt+Esc, driven ones by handing the cursor back.
static END: Notify = Notify::const_new();

pub fn end_sessions() {
    END.notify_waiters();
}

/// One frame, pushed out now.
async fn send(w: &mut (impl AsyncWrite + Unpin), m: Msg) -> io::Result<()> {
    w.write_all(&m.encode()).await?;
    w.flush().await
}

/// A pairing frame and the 32 bytes that follow it.
async fn send_pairing(w: &mut (impl AsyncWrite + Unpin), m: Msg, bytes: &[u8; 32]) -> io::Result<()> {
    let mut b = m.encode().to_vec();
    b.extend_from_slice(bytes);
    w.write_all(&b).await?;
    w.flush().await
}

/// The 32 bytes after a `want` frame.
async fn read_pairing(r: &mut (impl AsyncRead + Unpin), want: Msg) -> io::Result<[u8; 32]> {
    if read_msg(r).await? != want {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected pairing frame"));
    }
    let mut b = [0; 32];
    r.read_exact(&mut b).await?;
    Ok(b)
}

/// Ask the device at `to` (which must be `expect`, its discovered fingerprint) to pair; `on_code` gets
/// the code both screens show as soon as it is known. Resolves once its user answers (or the request
/// times out).
pub async fn request_pair(tls: &Tls, to: SocketAddr, expect: &str, on_code: impl FnOnce(String)) -> io::Result<PairAnswer> {
    let (mut s, _) = tls.connect(to, Some(expect)).await?;
    let mine = secure::nonce();
    send_pairing(&mut s, Msg::PairRequest, &secure::commit(&mine)).await?;
    let theirs = timeout(Duration::from_secs(5), read_pairing(&mut s, Msg::PairNonce)).await??;
    send_pairing(&mut s, Msg::PairReveal, &mine).await?;
    on_code(secure::pairing_code(&tls.me, expect, &mine, &theirs));
    match timeout(PAIR_TIMEOUT + Duration::from_secs(5), read_msg(&mut s)).await? {
        Ok(Msg::PairResponse(answer)) => Ok(answer),
        Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply to pair request")),
        Err(e) => Err(e),
    }
}

/// Answering side, after the PairRequest frame from `asker`: the code to show, once the asker revealed
/// the nonce it committed to before seeing ours.
async fn answer_pair(s: &mut Stream, asker: &str, me: &str) -> io::Result<String> {
    let mut committed = [0; 32];
    s.read_exact(&mut committed).await?;
    let mine = secure::nonce();
    send_pairing(s, Msg::PairNonce, &mine).await?;
    let theirs = read_pairing(s, Msg::PairReveal).await?;
    if secure::commit(&theirs) != committed {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "pairing nonce doesn't match its commitment"));
    }
    Ok(secure::pairing_code(asker, me, &theirs, &mine))
}

/// How long the other device's user has to accept a pairing request.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(30);

/// A stream to paired device `expect`'s `service`, once it answered that it shares it.
pub async fn open(tls: &Tls, to: SocketAddr, expect: &str, service: Service) -> io::Result<Stream> {
    let (mut s, _) = tls.connect(to, Some(expect)).await?;
    send(&mut s, Msg::Open(service)).await?;
    match timeout(Duration::from_secs(5), s.read_u8()).await?? {
        1 => Ok(s),
        _ => Err(io::Error::new(io::ErrorKind::PermissionDenied, service.not_shared())),
    }
}

/// A driver's connection waiting for the screen: stream, its fingerprint, entry edge and position.
type Incoming = (Stream, String, Edge, f32);

/// Receiving side. Every stream is handled on its own: a pairing request (answered by `ask_pair` with
/// the address and fingerprint of the asking device and the code to show), a service (`allow_service`,
/// then `on_service` owns the stream), or an input session, which only devices passing `allow_session`
/// (paired fingerprints) may start. One session at a time: a new driver takes over, handing the cursor
/// back to the current one.
/// `make_sink` gets the driving device's fingerprint and the edge the cursor enters (and leaves) through.
pub async fn serve<S: Sink, F: Future<Output = PairAnswer> + Send + 'static>(
    tls: Arc<Tls>,
    port: u16,
    allow_session: impl Fn(&str) -> Result<(), Refusal> + Send + Sync + 'static,
    ask_pair: impl Fn(IpAddr, String, String) -> F + Send + Sync + 'static,
    allow_service: impl Fn(Service, &str) -> bool + Send + Sync + 'static,
    on_service: impl Fn(Service, String, Stream) + Send + Sync + 'static,
    mut make_sink: impl FnMut(&str, Edge) -> Option<S>,
) -> io::Result<()> {
    let mut listener = tls.listen(port)?;
    let (tx, mut rx) = mpsc::channel::<Incoming>(1);
    let host = Arc::new((allow_session, ask_pair, allow_service, on_service));
    let t = tls.clone();
    tokio::spawn(async move {
        while let Ok((mut s, who, from)) = listener.accept().await {
            let (t, tx, host) = (t.clone(), tx.clone(), host.clone());
            tokio::spawn(async move {
                let (allow_session, ask_pair, allow_service, on_service) = &*host;
                let Ok(Ok(first)) = timeout(Duration::from_secs(5), read_msg(&mut s)).await else { return };
                match first {
                    Msg::PairRequest => {
                        // a wrong reveal (or none) drops the request without a prompt
                        let Ok(Ok(code)) = timeout(Duration::from_secs(5), answer_pair(&mut s, &who, &t.me)).await else { return };
                        let answer = ask_pair(from.ip(), who, code).await;
                        let _ = send(&mut s, Msg::PairResponse(answer)).await;
                    }
                    Msg::Enter { edge, y } => match allow_session(&who) {
                        Ok(()) => {
                            let _ = tx.send((s, who, edge, y)).await;
                        }
                        Err(r) => refuse(s, r).await,
                    },
                    Msg::Open(service) => {
                        let ok = allow_service(service, &who);
                        if s.write_all(&[ok as u8]).await.is_ok() && s.flush().await.is_ok() && ok {
                            on_service(service, who, s);
                        }
                    }
                    _ => {}
                }
            });
        }
    });
    let mut next = None;
    loop {
        let (s, who, edge, y) = match next.take() {
            Some(i) => i,
            None => rx.recv().await.ok_or_else(|| io::Error::other("kvm listener stopped"))?,
        };
        let Some(mut sink) = make_sink(&who, edge.opposite()) else {
            tokio::spawn(refuse(s, Refusal::NoScreen));
            continue;
        };
        match session(s, &mut sink, edge, y, &tls, &mut rx).await {
            Ok(taken) => next = taken,
            Err(e) => eprintln!("kvm session ended: {e}"),
        }
    }
}

/// Turn a session down; `shutdown` waits until the driver has the Refuse.
async fn refuse(mut s: Stream, r: Refusal) {
    let _ = timeout(WRITE_TIMEOUT, async {
        send(&mut s, Msg::Refuse(r)).await?;
        s.shutdown().await
    })
    .await;
}

/// The driver's input forwarded on to the next device of a chain until that one hands the cursor back.
struct Relay {
    wr: WriteHalf<Stream>,
    back: UnboundedReceiver<Back>,
    reader: JoinHandle<()>,
    portal: Portal,
    y: f32,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

enum Back {
    Clipboard(String),
    Return(f32),
    Gone,
}

async fn start_relay(tls: &Tls, hop: Hop, clipboard: Option<String>) -> io::Result<Relay> {
    let (c, _) = tls.connect(hop.to, Some(&hop.fp)).await?;
    let (mut rd, mut wr) = tokio::io::split(c);
    timeout(WRITE_TIMEOUT, async {
        send(&mut wr, Msg::Enter { edge: hop.portal.edge, y: hop.y }).await?;
        write_clipboard(&mut wr, clipboard).await
    })
    .await??;
    let (tx, back) = unbounded_channel();
    let reader = tokio::spawn(async move {
        loop {
            let b = match read_msg(&mut rd).await {
                Ok(Msg::Return { y }) => Back::Return(y),
                Ok(Msg::Clipboard { len }) => match read_clipboard(&mut rd, len).await {
                    Ok(Some(text)) => Back::Clipboard(text),
                    Ok(None) => continue,
                    Err(_) => Back::Gone,
                },
                Ok(Msg::Refuse(_)) | Err(_) => Back::Gone,
                Ok(_) => continue,
            };
            let last = !matches!(b, Back::Clipboard(_));
            if tx.send(b).is_err() || last {
                return;
            }
        }
    });
    Ok(Relay { wr, back, reader, portal: hop.portal, y: hop.y })
}

/// Give the cursor back to the driver at `y` along the entry edge: changed clipboard first (it stops
/// reading once it sees Return), then a drag going along, then Return.
async fn hand_back(s: &mut (impl AsyncWrite + Unpin), sink: &impl Sink, seen: &Option<String>, drag: Option<u64>, y: f32) -> io::Result<()> {
    let now = sink.clipboard();
    if now != *seen {
        write_clipboard(s, now).await?;
    }
    if let Some(batch) = drag {
        DRAG_OUT.store(0, Ordering::SeqCst);
        send(s, Msg::Drag { batch }).await?;
    }
    send(s, Msg::Return { y }).await
}

/// One driven session. Ends with `Some` when another driver took over (its connection, to start next).
async fn session(
    mut s: impl AsyncRead + AsyncWrite + Unpin,
    sink: &mut impl Sink,
    edge: Edge,
    y: f32,
    tls: &Tls,
    waiting: &mut mpsc::Receiver<Incoming>,
) -> io::Result<Option<Incoming>> {
    enum Ev {
        Frame(io::Result<Msg>),
        Back(Back),
        Waiting(Option<Incoming>),
        End,
    }
    let (mut rcv, first) = Receiver::enter(sink.screens(), edge, y);
    sink.inject(first);
    DRAG_OUT.store(0, Ordering::SeqCst);
    // hand the clipboard back only if it changed here, so the driver's own (maybe an image) survives
    let mut seen = sink.clipboard();
    let (mut frames, mut relay, mut retry_at) = (Frames::default(), None::<Relay>, Instant::now());
    let ended = END.notified();
    tokio::pin!(ended);
    ended.as_mut().enable();
    let result = loop {
        let ev = tokio::select! {
            m = frames.next(&mut s) => Ev::Frame(m),
            b = async {
                match relay.as_mut() {
                    Some(r) => r.back.recv().await.unwrap_or(Back::Gone),
                    None => std::future::pending().await,
                }
            } => Ev::Back(b),
            i = waiting.recv() => Ev::Waiting(i),
            _ = &mut ended => Ev::End,
        };
        let m = match ev {
            Ev::Frame(Ok(m)) => m,
            Ev::Frame(Err(e)) => break Err(e),
            Ev::Waiting(None) => break Err(io::Error::other("kvm listener stopped")),
            // another device takes over, or the user ended it here: the cursor goes back to its driver
            Ev::Waiting(Some(i)) => break hand_back(&mut s, sink, &seen, None, rcv.along()).await.map(|()| Some(i)),
            Ev::End => break hand_back(&mut s, sink, &seen, None, rcv.along()).await.map(|()| None),
            Ev::Back(Back::Clipboard(text)) => {
                sink.set_clipboard(text);
                continue;
            }
            Ev::Back(b) => {
                let r = relay.take().expect("Back only while relaying");
                let (x, y) = r.portal.point(if let Back::Return(y) = b { y } else { r.y }, RETURN_INSET);
                sink.inject(rcv.place(x, y));
                continue;
            }
        };
        if let Some(r) = relay.as_mut() {
            let sent = match m {
                Msg::Input(_) | Msg::Lock { .. } => {
                    if let Msg::Lock { on } = m {
                        rcv.locked = on;
                    }
                    matches!(timeout(WRITE_TIMEOUT, send(&mut r.wr, m)).await, Ok(Ok(())))
                }
                Msg::Clipboard { len } => match read_clipboard(&mut s, len).await {
                    Ok(Some(text)) => {
                        sink.set_clipboard(text.clone());
                        seen = Some(text.clone());
                        matches!(timeout(WRITE_TIMEOUT, write_clipboard(&mut r.wr, Some(text))).await, Ok(Ok(())))
                    }
                    Ok(None) => true,
                    Err(e) => break Err(e),
                },
                Msg::Leave => {
                    let _ = timeout(WRITE_TIMEOUT, send(&mut r.wr, Msg::Leave)).await;
                    break Ok(None);
                }
                _ => true,
            };
            if !sent {
                // the next device stopped answering: the cursor comes back out of its portal here
                let r = relay.take().expect("relaying");
                let (x, y) = r.portal.point(r.y, RETURN_INSET);
                sink.inject(rcv.place(x, y));
            }
            continue;
        }
        match m {
            Msg::Input(e) => {
                let out = DRAG_OUT.load(Ordering::SeqCst);
                if out != 0 {
                    rcv.drag_out = Some(out);
                }
                match rcv.apply(e) {
                    Applied::Inject(i) => {
                        sink.inject(i);
                        let Injected::MoveTo { x, y } = i else { continue };
                        if rcv.buttons != 0 || rcv.locked || Instant::now() < retry_at {
                            continue;
                        }
                        if let Some(hop) = sink.hop(x, y) {
                            match start_relay(tls, hop, sink.clipboard()).await {
                                Ok(r) => relay = Some(r),
                                Err(e) => {
                                    eprintln!("kvm chain: {e}");
                                    retry_at = Instant::now() + Duration::from_secs(1); // no reconnect storm at that edge
                                }
                            }
                        }
                    }
                    Applied::Many(all) => all.into_iter().for_each(|i| sink.inject(i)),
                    Applied::Return(y) => break hand_back(&mut s, sink, &seen, rcv.drag_out, y).await.map(|()| None),
                }
            }
            Msg::Clipboard { len } => match read_clipboard(&mut s, len).await {
                Ok(Some(text)) => {
                    sink.set_clipboard(text.clone());
                    seen = Some(text);
                }
                Ok(None) => {}
                Err(e) => break Err(e),
            },
            Msg::Leave => break Ok(None),
            Msg::Lock { on } => rcv.locked = on,
            Msg::Drag { batch } => {
                rcv.buttons |= 1; // the driver's button is still held: no return until it's released
                sink.drag(batch);
            }
            _ => {}
        }
    };
    for i in rcv.release_all() {
        sink.inject(i);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const AREA: Rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 500.0 };

    #[test]
    fn frames_round_trip_and_reject_garbage() {
        let all = [
            Msg::Enter { edge: Edge::Bottom, y: 0.25 },
            Msg::Input(InputEvent::Move { dx: -7, dy: 300 }),
            Msg::Input(InputEvent::Button { button: 5, down: true }),
            Msg::Input(InputEvent::Wheel { dx: 0, dy: -240 }),
            Msg::Input(InputEvent::Key { scan: 0xE05B, down: false }),
            Msg::Return { y: 1.0 },
            Msg::Leave,
            Msg::Refuse(Refusal::NoScreen),
            Msg::Lock { on: true },
            Msg::Drag { batch: u64::MAX - 1 },
            Msg::PairRequest,
            Msg::PairResponse(PairAnswer::NoAnswer),
            Msg::PairNonce,
            Msg::PairReveal,
            Msg::Clipboard { len: MAX_CLIPBOARD },
        ];
        for m in all {
            assert_eq!(Msg::decode(&m.encode()), Some(m));
        }
        let mut bad = Msg::Return { y: 0.5 }.encode();
        bad[1..5].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(Msg::decode(&bad), None);
        assert_eq!(Msg::decode(&[3, 9, 1, 0, 0, 0, 0, 0, 0]), None); // button 9
        assert_eq!(Msg::decode(&[0; FRAME]), None);
        assert_eq!(Msg::decode(&Msg::Clipboard { len: MAX_CLIPBOARD + 1 }.encode()), None);
    }

    #[test]
    fn enters_on_opposite_edge_and_returns_through_it() {
        // sender's portal is its right edge → we enter on our left edge
        let (mut r, first) = Receiver::enter(vec![AREA], Edge::Right, 0.5);
        assert_eq!(first, Injected::MoveTo { x: 8.0, y: 250.0 });
        // jitter back toward the edge right after entering must not hand control back
        assert_eq!(r.apply(InputEvent::Move { dx: -3, dy: 0 }), Applied::Inject(Injected::MoveTo { x: 5.0, y: 250.0 }));
        assert_eq!(r.apply(InputEvent::Move { dx: 95, dy: -50 }), Applied::Inject(Injected::MoveTo { x: 100.0, y: 200.0 }));
        // pushing past the right edge just clamps
        assert_eq!(r.apply(InputEvent::Move { dx: 5000, dy: 0 }), Applied::Inject(Injected::MoveTo { x: 999.0, y: 200.0 }));
        assert_eq!(r.apply(InputEvent::Move { dx: -5000, dy: 0 }), Applied::Return(0.4));
    }

    #[test]
    fn receiver_spans_every_monitor_but_never_their_gaps() {
        // a wide screen and a taller one to its right; the sender sits to our right
        let screens = vec![AREA, Rect { x: 1000.0, y: 0.0, w: 800.0, h: 1000.0 }];
        let (mut r, first) = Receiver::enter(screens, Edge::Left, 0.75);
        assert_eq!(first, Injected::MoveTo { x: 1791.0, y: 750.0 });
        // moving left below the short screen stops at the tall one's edge, not in the empty corner
        assert_eq!(r.apply(InputEvent::Move { dx: -900, dy: 0 }), Applied::Inject(Injected::MoveTo { x: 1000.0, y: 750.0 }));
        assert_eq!(r.apply(InputEvent::Move { dx: -500, dy: -600 }), Applied::Inject(Injected::MoveTo { x: 500.0, y: 150.0 }));
        // the way back is the right edge of the whole desk
        assert_eq!(r.apply(InputEvent::Move { dx: 2000, dy: 0 }), Applied::Return(0.15));
    }

    #[test]
    fn no_return_while_dragging_and_release_all_unsticks() {
        let (mut r, _) = Receiver::enter(vec![AREA], Edge::Right, 0.5);
        r.apply(InputEvent::Key { scan: 0x1D, down: true });
        r.apply(InputEvent::Key { scan: 0x2E, down: true });
        r.apply(InputEvent::Button { button: 1, down: true });
        assert_eq!(r.apply(InputEvent::Move { dx: -10, dy: 0 }), Applied::Inject(Injected::MoveTo { x: 0.0, y: 250.0 }));
        assert_eq!(
            r.release_all(),
            vec![
                Injected::Key { scan: 0x2E, down: false },
                Injected::Key { scan: 0x1D, down: false },
                Injected::Button { button: 1, down: false },
            ]
        );
    }

    #[test]
    fn locked_receiver_keeps_the_cursor_until_unlocked() {
        let (mut r, _) = Receiver::enter(vec![AREA], Edge::Right, 0.5);
        r.locked = true;
        assert_eq!(r.apply(InputEvent::Move { dx: -50, dy: 0 }), Applied::Inject(Injected::MoveTo { x: 0.0, y: 250.0 }));
        r.locked = false;
        assert_eq!(r.apply(InputEvent::Move { dx: -1, dy: 0 }), Applied::Return(0.5));
    }

    #[test]
    fn tune_scales_without_losing_slow_motion() {
        let mut t = Tune::new(0.5, false);
        assert_eq!(t.shape(InputEvent::Move { dx: 1, dy: -1 }), None, "half a pixel is kept, not sent");
        assert_eq!(t.shape(InputEvent::Move { dx: 1, dy: -1 }), Some(Msg::Input(InputEvent::Move { dx: 1, dy: -1 })));
        let mut fast = Tune::new(1.5, false);
        assert_eq!(fast.shape(InputEvent::Move { dx: 3, dy: 0 }), Some(Msg::Input(InputEvent::Move { dx: 4, dy: 0 })));
        assert_eq!(fast.shape(InputEvent::Move { dx: 1, dy: 0 }), Some(Msg::Input(InputEvent::Move { dx: 2, dy: 0 })));
    }

    #[test]
    fn tune_swaps_ctrl_and_cmd_and_toggles_the_lock() {
        let key = |scan, down| InputEvent::Key { scan, down };
        let mut t = Tune::new(1.0, true);
        assert_eq!(t.shape(key(0x1D, true)), Some(Msg::Input(key(0xE05B, true))), "Ctrl+C on the PC is ⌘C on the Mac");
        assert_eq!(t.shape(key(0xE05C, false)), Some(Msg::Input(key(0xE01D, false))));
        assert_eq!(t.shape(key(0x2E, true)), Some(Msg::Input(key(0x2E, true))));
        assert_eq!(t.shape(key(LOCK_SCAN, true)), Some(Msg::Lock { on: true }));
        assert_eq!(t.shape(key(LOCK_SCAN, false)), None, "the lock key itself never reaches the other device");
        assert_eq!(t.shape(key(LOCK_SCAN, true)), Some(Msg::Lock { on: false }));
        assert_eq!(Tune::new(1.0, false).shape(key(0x1D, true)), Some(Msg::Input(key(0x1D, true))));
    }

    struct Recorder(Arc<Mutex<Vec<Injected>>>);
    impl Sink for Recorder {
        fn screens(&self) -> Vec<Rect> {
            vec![AREA]
        }
        fn inject(&mut self, e: Injected) {
            self.0.lock().unwrap().push(e);
        }
    }

    /// The device listening (behind `serve`) and the one connecting to it.
    fn devices() -> (Arc<Tls>, Tls) {
        use crate::network::secure::Identity;
        (Arc::new(Tls::new(&Identity::generate())), Tls::new(&Identity::generate()))
    }

    async fn dial(t: &Tls, port: u16) -> Stream {
        t.connect(([127, 0, 0, 1], port).into(), None).await.unwrap().0
    }

    #[tokio::test]
    async fn leave_releases_held_keys_on_receiver() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let port = 53398;
        let (them, me) = devices();
        tokio::spawn(serve(them, port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| Some(Recorder(g.clone()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = dial(&me, port).await;
        for m in [
            Msg::Enter { edge: Edge::Left, y: 0.0 },
            Msg::Input(InputEvent::Key { scan: 0x1D, down: true }),
            Msg::Leave,
        ] {
            send(&mut s, m).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            *got.lock().unwrap(),
            vec![
                Injected::MoveTo { x: 991.0, y: 0.0 },
                Injected::Key { scan: 0x1D, down: true },
                Injected::Key { scan: 0x1D, down: false },
            ]
        );
    }

    #[tokio::test]
    async fn unpaired_device_is_refused_until_the_user_accepts_pairing() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let paired = Arc::new(Mutex::new(None::<String>));
        let (g, p1, p2) = (got.clone(), paired.clone(), paired.clone());
        let port = 53397;
        let (them, me) = devices();
        let them_fp = them.me.clone();
        let allow = move |fp: &str| if p1.lock().unwrap().as_deref() == Some(fp) { Ok(()) } else { Err(Refusal::NotPaired) };
        let ask = move |_, fp: String, _| {
            let p = p2.clone();
            async move {
                *p.lock().unwrap() = Some(fp); // user taps "Pair": remembers who asked, by certificate
                PairAnswer::Accepted
            }
        };
        tokio::spawn(serve(them, port, allow, ask, |_, _| false, |_, _, _| {}, move |_, _| Some(Recorder(g.clone()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let to: SocketAddr = ([127, 0, 0, 1], port).into();
        let enter = Msg::Enter { edge: Edge::Left, y: 0.0 };

        let mut s = dial(&me, port).await;
        send(&mut s, enter).await.unwrap();
        assert_eq!(read_msg(&mut s).await.unwrap(), Msg::Refuse(Refusal::NotPaired), "the driver is told why");
        assert!(got.lock().unwrap().is_empty(), "session before pairing must be dropped");

        assert_eq!(request_pair(&me, to, &them_fp, |_| {}).await.unwrap(), PairAnswer::Accepted);
        assert_eq!(paired.lock().unwrap().as_deref(), Some(me.me.as_str()));
        let mut s = dial(&me, port).await;
        send(&mut s, enter).await.unwrap();
        send(&mut s, Msg::Leave).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*got.lock().unwrap(), vec![Injected::MoveTo { x: 991.0, y: 0.0 }]);
        // a third device with its own certificate is still a stranger
        let (_, other) = devices();
        let mut s = dial(&other, port).await;
        send(&mut s, enter).await.unwrap();
        assert_eq!(read_msg(&mut s).await.unwrap(), Msg::Refuse(Refusal::NotPaired));
    }

    /// Main-side engine: capture "works" (holds the input channel open, like a real hook) and
    /// records where the cursor is warped back to.
    #[derive(Default)]
    struct Captor {
        out: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Captured>>>,
        warped: Mutex<Option<(f64, f64)>>,
    }
    impl SpatialPlatformEngine for Captor {
        fn prepare_overlay(&self, _: &tauri::WebviewWindow) -> tauri::Result<()> {
            Ok(())
        }
        fn cursor(&self) -> Option<crate::platform::CursorSample> {
            None
        }
        fn start_capture(&self, _: (f64, f64), out: tokio::sync::mpsc::UnboundedSender<Captured>) -> Result<(), String> {
            *self.out.lock().unwrap() = Some(out);
            Ok(())
        }
        fn stop_capture(&self, warp_to: (f64, f64)) {
            *self.warped.lock().unwrap() = Some(warp_to);
        }
    }

    #[tokio::test]
    async fn refused_session_says_why_and_gives_input_back() {
        let port = 53394;
        let (them, me) = devices();
        let them_fp = them.me.clone();
        tokio::spawn(serve(them, port, |_| Err(Refusal::NotPaired), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, |_, _| None::<Recorder>));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let engine = Arc::new(Captor::default());
        let portal = Portal { edge: Edge::Right, monitor: AREA, start: 0.0, end: 1.0, reach: 100.0 };
        let e = send_session(&me, engine.clone(), ([127, 0, 0, 1], port).into(), &them_fp, portal, 0.5, Tune::new(1.0, false), None).await.unwrap_err();
        assert_eq!(e.to_string(), Refusal::NotPaired.text());
        assert_eq!(*engine.warped.lock().unwrap(), Some(portal.point(0.5, RETURN_INSET)));
    }

    /// A Mac without Accessibility: the hook can't be installed.
    struct NoCapture;
    impl SpatialPlatformEngine for NoCapture {
        fn prepare_overlay(&self, _: &tauri::WebviewWindow) -> tauri::Result<()> {
            Ok(())
        }
        fn cursor(&self) -> Option<crate::platform::CursorSample> {
            None
        }
        fn start_capture(&self, _: (f64, f64), _: tokio::sync::mpsc::UnboundedSender<Captured>) -> Result<(), String> {
            Err("no Accessibility".into())
        }
    }

    #[tokio::test]
    async fn failed_capture_never_enters_the_peer() {
        let port = 53378;
        let (them, me) = devices();
        let them_fp = them.me.clone();
        let entered = Arc::new(AtomicBool::new(false));
        let seen = entered.clone();
        tokio::spawn(serve(them, port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| {
            seen.store(true, Ordering::SeqCst);
            None::<Recorder>
        }));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let portal = Portal { edge: Edge::Right, monitor: AREA, start: 0.0, end: 1.0, reach: 100.0 };
        let e = send_session(&me, Arc::new(NoCapture), ([127, 0, 0, 1], port).into(), &them_fp, portal, 0.5, Tune::new(1.0, false), None).await.unwrap_err();
        assert_eq!(e.to_string(), "no Accessibility");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!entered.load(Ordering::SeqCst), "the peer's cursor jumped to its edge for a session that never ran");
    }

    struct Drags(Arc<Mutex<Vec<u64>>>);
    impl Sink for Drags {
        fn screens(&self) -> Vec<Rect> {
            vec![AREA]
        }
        fn inject(&mut self, _: Injected) {}
        fn drag(&mut self, batch: u64) {
            self.0.lock().unwrap().push(batch);
        }
    }

    #[tokio::test]
    async fn file_drags_cross_with_the_cursor_both_ways() {
        let port = 53392;
        let drags = Arc::new(Mutex::new(Vec::new()));
        let d = drags.clone();
        let (them, me) = devices();
        let them_fp = them.me.clone();
        tokio::spawn(serve(them, port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| Some(Drags(d.clone()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let engine = Arc::new(Captor::default());
        let portal = Portal { edge: Edge::Right, monitor: AREA, start: 0.0, end: 1.0, reach: 100.0 };
        let to: SocketAddr = ([127, 0, 0, 1], port).into();
        let e = engine.clone();
        let main = tokio::spawn(async move { send_session(&me, e, to, &them_fp, portal, 0.5, Tune::new(1.0, false), Some(5)).await });
        // other sessions in this test binary reset DRAG_OUT when they start; let them settle first
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(*drags.lock().unwrap(), vec![5], "the Client was told a drag came along");
        let push = |dx| engine.out.lock().unwrap().as_ref().unwrap().send(Captured::Input(InputEvent::Move { dx, dy: 0 })).unwrap();
        push(-50);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!main.is_finished(), "the dragged button is still held: no way back yet");
        DRAG_OUT.store(9, Ordering::SeqCst); // the Client hands its own drag back
        push(-50);
        assert_eq!(timeout(Duration::from_secs(2), main).await.unwrap().unwrap().unwrap(), Some(9));
    }

    /// Receiver's clipboard; records what the Main sent.
    struct Clip {
        own: Arc<Mutex<Option<String>>>,
    }
    impl Sink for Clip {
        fn screens(&self) -> Vec<Rect> {
            vec![AREA]
        }
        fn inject(&mut self, _: Injected) {}
        fn clipboard(&self) -> Option<String> {
            self.own.lock().unwrap().clone()
        }
        fn set_clipboard(&mut self, text: String) {
            *self.own.lock().unwrap() = Some(text);
        }
    }

    async fn crossing(port: u16, own: Arc<Mutex<Option<String>>>, text: Option<&str>, copy_on_client: Option<&str>) -> Option<String> {
        let o = own.clone();
        let (them, me) = devices();
        tokio::spawn(serve(them, port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| Some(Clip { own: o.clone() })));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = dial(&me, port).await;
        send(&mut s, Msg::Enter { edge: Edge::Left, y: 0.5 }).await.unwrap();
        write_clipboard(&mut s, text.map(String::from)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(c) = copy_on_client {
            *own.lock().unwrap() = Some(c.into()); // user copies something on the Client
        }
        // entered on the receiver's right edge; pushing right crosses back
        send(&mut s, Msg::Input(InputEvent::Move { dx: 5000, dy: 0 })).await.unwrap();
        let mut back = None;
        loop {
            match read_msg(&mut s).await.unwrap() {
                Msg::Clipboard { len } => back = read_clipboard(&mut s, len).await.unwrap(),
                Msg::Return { .. } => return back,
                m => panic!("unexpected {m:?}"),
            }
        }
    }

    #[tokio::test]
    async fn clipboard_travels_with_the_cursor_and_only_when_changed() {
        // Main's text lands on the Client; the Client's new copy comes back before Return
        let own = Arc::new(Mutex::new(Some("old client text".into())));
        let back = crossing(53396, own.clone(), Some("from main"), Some("from client")).await;
        assert_eq!(back.as_deref(), Some("from client"));

        // nothing copied on the Client: nothing is sent back, so the Main's clipboard (maybe an image) survives
        let own = Arc::new(Mutex::new(Some("client text".into())));
        let back = crossing(53395, own.clone(), Some("from main"), None).await;
        assert_eq!(back, None);
        assert_eq!(own.lock().unwrap().as_deref(), Some("from main"));
    }

    #[test]
    fn double_tap_ctrl_comes_home_but_not_shortcuts_or_repeats() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let key = |scan, down| InputEvent::Key { scan, down };
        let mut t = Tune::new(1.0, false);
        assert!(!t.home(&key(0x1D, true), at(0)));
        assert!(!t.home(&key(0x1D, false), at(80)));
        assert!(!t.home(&InputEvent::Move { dx: 3, dy: 0 }, at(100)), "the hand on the mouse doesn't break it");
        assert!(t.home(&key(0xE01D, true), at(200)), "second tap, either Ctrl");
        // Ctrl+C then Ctrl+V is not a double tap
        let mut t = Tune::new(1.0, false);
        for (e, ms) in [(key(0x1D, true), 0), (key(0x2E, true), 50), (key(0x2E, false), 90), (key(0x1D, false), 120)] {
            assert!(!t.home(&e, at(ms)));
        }
        assert!(!t.home(&key(0x1D, true), at(200)));
        // auto-repeat of a held Ctrl, Ctrl+click, and a slow second tap
        let mut t = Tune::new(1.0, false);
        assert!(!t.home(&key(0x1D, true), at(0)));
        assert!(!t.home(&key(0x1D, true), at(300)));
        let mut t = Tune::new(1.0, false);
        t.home(&key(0x1D, true), at(0));
        t.home(&InputEvent::Button { button: 1, down: true }, at(20));
        t.home(&key(0x1D, false), at(60));
        assert!(!t.home(&key(0x1D, true), at(120)));
        let mut t = Tune::new(1.0, false);
        t.home(&key(0x1D, true), at(0));
        t.home(&key(0x1D, false), at(50));
        assert!(!t.home(&key(0x1D, true), at(900)));
    }

    /// Device B: forwards to C once its cursor reaches x = 999 (its right edge).
    struct Hopper {
        rec: Arc<Mutex<Vec<Injected>>>,
        next: (SocketAddr, String),
    }
    const RIGHT: Portal = Portal { edge: Edge::Right, monitor: AREA, start: 0.0, end: 1.0, reach: 120.0 };
    impl Sink for Hopper {
        fn screens(&self) -> Vec<Rect> {
            vec![AREA]
        }
        fn inject(&mut self, e: Injected) {
            self.rec.lock().unwrap().push(e);
        }
        fn hop(&self, x: f64, y: f64) -> Option<Hop> {
            (x >= 999.0).then(|| Hop { to: self.next.0, fp: self.next.1.clone(), portal: RIGHT, y: (y / 500.0) as f32 })
        }
    }

    #[tokio::test]
    async fn cursor_travels_on_through_a_chain_and_back() {
        let (b_rec, c_rec) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
        let (b, me) = devices();
        let c = Arc::new(Tls::new(&crate::network::secure::Identity::generate()));
        let (c_port, b_port) = (53391, 53390);
        let cr = c_rec.clone();
        tokio::spawn(serve(c.clone(), c_port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| Some(Recorder(cr.clone()))));
        let (br, next) = (b_rec.clone(), (SocketAddr::from(([127, 0, 0, 1], c_port)), c.me.clone()));
        tokio::spawn(serve(b, b_port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| Some(Hopper { rec: br.clone(), next: next.clone() })));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = dial(&me, b_port).await;
        let step = |ms| tokio::time::sleep(Duration::from_millis(ms));
        send(&mut s, Msg::Enter { edge: Edge::Right, y: 0.5 }).await.unwrap(); // A's right edge → B's left
        send(&mut s, Msg::Input(InputEvent::Move { dx: 5000, dy: 0 })).await.unwrap(); // across B into C
        step(300).await;
        send(&mut s, Msg::Input(InputEvent::Key { scan: 0x1E, down: true })).await.unwrap();
        send(&mut s, Msg::Input(InputEvent::Key { scan: 0x1E, down: false })).await.unwrap();
        send(&mut s, Msg::Input(InputEvent::Move { dx: -5000, dy: 0 })).await.unwrap(); // out of C's entry edge
        step(300).await;
        send(&mut s, Msg::Input(InputEvent::Key { scan: 0x30, down: true })).await.unwrap(); // typed on B again
        step(200).await;
        let c_got = c_rec.lock().unwrap().clone();
        assert_eq!(c_got[0], Injected::MoveTo { x: 8.0, y: 250.0 }, "C entered on its left edge, level with B's cursor");
        assert!(c_got.contains(&Injected::Key { scan: 0x1E, down: true }) && !c_got.contains(&Injected::Key { scan: 0x30, down: true }));
        let b_got = b_rec.lock().unwrap().clone();
        assert!(!b_got.contains(&Injected::Key { scan: 0x1E, down: true }), "keys typed on C never reach B");
        let back = b_got.iter().position(|e| *e == Injected::MoveTo { x: 987.0, y: 250.0 }).expect("cursor back out of B's portal");
        assert_eq!(b_got[back + 1], Injected::Key { scan: 0x30, down: true });
    }

    #[tokio::test]
    async fn a_new_driver_takes_over_and_the_old_one_gets_its_cursor_back() {
        let rec = Arc::new(Mutex::new(Vec::new()));
        let (b, first) = devices();
        let second = Tls::new(&crate::network::secure::Identity::generate());
        let port = 53379; // UDP 53389 is held by a Windows service on some machines
        let r = rec.clone();
        tokio::spawn(serve(b, port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| Some(Recorder(r.clone()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut one = dial(&first, port).await;
        send(&mut one, Msg::Enter { edge: Edge::Right, y: 0.5 }).await.unwrap();
        send(&mut one, Msg::Input(InputEvent::Key { scan: 0x2A, down: true })).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut two = dial(&second, port).await;
        send(&mut two, Msg::Enter { edge: Edge::Bottom, y: 0.25 }).await.unwrap();
        let back = timeout(Duration::from_secs(2), read_msg(&mut one)).await.unwrap().unwrap();
        assert_eq!(back, Msg::Return { y: 0.5 });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let got = rec.lock().unwrap().clone();
        // the first driver's held Shift is let go before the second one's cursor appears at our top edge
        let up = got.iter().position(|e| *e == Injected::Key { scan: 0x2A, down: false }).unwrap();
        assert_eq!(got[up + 1], Injected::MoveTo { x: 250.0, y: 8.0 });
    }

    #[tokio::test]
    async fn pairing_codes_match_answers_say_why_and_a_wrong_reveal_gets_no_prompt() {
        let port = 53386;
        let (them, me) = devices();
        let them_fp = them.me.clone();
        let shown = Arc::new(Mutex::new(Vec::new()));
        let sh = shown.clone();
        // the first request is accepted; the user never answers the second
        let ask = move |_, _, code: String| {
            let mut sh = sh.lock().unwrap();
            sh.push(code);
            let answer = if sh.len() == 1 { PairAnswer::Accepted } else { PairAnswer::NoAnswer };
            async move { answer }
        };
        tokio::spawn(serve(them, port, |_| Ok(()), ask, |_, _| false, |_, _, _| {}, |_, _| None::<Recorder>));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let to: SocketAddr = ([127, 0, 0, 1], port).into();
        let mut mine = None;
        assert_eq!(request_pair(&me, to, &them_fp, |c| mine = Some(c)).await.unwrap(), PairAnswer::Accepted);
        assert_eq!(*shown.lock().unwrap(), vec![mine.unwrap()], "both screens show the same code");
        assert_eq!(request_pair(&me, to, &them_fp, |_| {}).await.unwrap(), PairAnswer::NoAnswer);
        assert_eq!(Msg::decode(&[9, 4, 0, 0, 0, 0, 0, 0, 0]), None, "unknown answer");

        // revealing another nonce than the one committed to: dropped before any prompt
        let mut s = dial(&me, port).await;
        send_pairing(&mut s, Msg::PairRequest, &secure::commit(&secure::nonce())).await.unwrap();
        read_pairing(&mut s, Msg::PairNonce).await.unwrap();
        send_pairing(&mut s, Msg::PairReveal, &secure::nonce()).await.unwrap();
        assert!(timeout(Duration::from_secs(2), read_msg(&mut s)).await.unwrap().is_err(), "no answer");
        assert_eq!(shown.lock().unwrap().len(), 2, "no prompt");
    }

    /// Forwards UDP between a client and `to` until `cut`, then drops everything both ways.
    async fn black_hole(listen: u16, to: u16, cut: Arc<AtomicBool>) {
        let front = tokio::net::UdpSocket::bind(("127.0.0.1", listen)).await.unwrap();
        let back = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        back.connect(("127.0.0.1", to)).await.unwrap();
        let (mut a, mut b, mut client) = ([0u8; 65536], [0u8; 65536], None);
        loop {
            tokio::select! {
                Ok((n, from)) = front.recv_from(&mut a) => {
                    client = Some(from);
                    if !cut.load(Ordering::SeqCst) {
                        let _ = back.send(&a[..n]).await;
                    }
                }
                Ok(n) = back.recv(&mut b) => {
                    if let (Some(c), false) = (client, cut.load(Ordering::SeqCst)) {
                        let _ = front.send_to(&b[..n], c).await;
                    }
                }
                else => {}
            }
        }
    }

    #[tokio::test]
    async fn a_silently_dead_link_releases_held_keys_and_ends_both_sides() {
        let (port, proxy) = (53385, 53384);
        let rec = Arc::new(Mutex::new(Vec::new()));
        let (them, me) = devices();
        let them_fp = them.me.clone();
        let r = rec.clone();
        tokio::spawn(serve(them, port, |_| Ok(()), |_, _, _| async { PairAnswer::Declined }, |_, _| false, |_, _, _| {}, move |_, _| Some(Recorder(r.clone()))));
        let cut = Arc::new(AtomicBool::new(false));
        tokio::spawn(black_hole(proxy, port, cut.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let engine = Arc::new(Captor::default());
        let portal = Portal { edge: Edge::Right, monitor: AREA, start: 0.0, end: 1.0, reach: 100.0 };
        let e = engine.clone();
        let driver = tokio::spawn(async move { send_session(&me, e, ([127, 0, 0, 1], proxy).into(), &them_fp, portal, 0.5, Tune::new(1.0, false), None).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let down = Injected::Key { scan: 0x1E, down: true };
        engine.out.lock().unwrap().as_ref().unwrap().send(Captured::Input(InputEvent::Key { scan: 0x1E, down: true })).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(rec.lock().unwrap().contains(&down));

        cut.store(true, Ordering::SeqCst); // Wi-Fi gone, peer asleep: nothing more arrives either way
        let e = timeout(Duration::from_secs(5), driver).await.expect("driver gave up in time").unwrap().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "the driver is told the other side stopped answering: {e}");
        assert!(engine.warped.lock().unwrap().is_some(), "local input given back");
        let up = Injected::Key { scan: 0x1E, down: false };
        for _ in 0..20 {
            if rec.lock().unwrap().contains(&up) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the held key was never released on the driven side");
    }
}
