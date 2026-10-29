//! Shared mouse & keyboard. Pushing the cursor into the portal hands input to the paired device over TCP;
//! pushing it out of that device's opposite edge (or Ctrl+Alt+Esc) hands it back.
//!
//! Wire: fixed 9-byte frames `tag | payload (8 bytes, LE)`. A Clipboard frame is followed by `len` bytes
//! of UTF-8: the clipboard travels with the cursor (Main → Client on Enter, Client → Main on Return).

use crate::platform::{clipboard_text, edge_point, set_clipboard_text, Captured, Edge, Injected, InputEvent, Portal, Rect, SpatialPlatformEngine};
use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc::unbounded_channel,
    time::timeout,
};

pub const PORT: u16 = 53321;
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
    /// Main → client: ask the client's user to pair with this device.
    PairRequest,
    PairResponse { accepted: bool },
    /// Clipboard text follows: `len` bytes of UTF-8.
    Clipboard { len: u32 },
    /// Client → Main: session turned down, so the Main can say why instead of "closed".
    Refuse(Refusal),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Refusal {
    NotClient,
    NotPaired,
    NoScreen,
}

impl Refusal {
    fn text(self) -> &'static str {
        match self {
            Refusal::NotClient => "the other device is set as Main, switch it to Client",
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
            Msg::Return { y } => {
                put(0, &[6]);
                put(1, &y.to_le_bytes());
            }
            Msg::Leave => put(0, &[7]),
            Msg::PairRequest => put(0, &[8]),
            Msg::PairResponse { accepted } => put(0, &[9, accepted as u8]),
            Msg::Clipboard { len } => {
                put(0, &[10]);
                put(1, &len.to_le_bytes());
            }
            Msg::Refuse(r) => put(0, &[11, r as u8]),
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
            9 => Msg::PairResponse { accepted: b[1] != 0 },
            10 => Msg::Clipboard { len: Some(u32::from_le_bytes(b[1..5].try_into().unwrap())).filter(|l| *l <= MAX_CLIPBOARD)? },
            11 => Msg::Refuse(match b[1] {
                0 => Refusal::NotClient,
                1 => Refusal::NotPaired,
                2 => Refusal::NoScreen,
                _ => return None,
            }),
            _ => return None,
        })
    }
}

async fn read_msg(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Msg> {
    let mut b = [0u8; FRAME];
    r.read_exact(&mut b).await?;
    Msg::decode(&b).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad kvm frame"))
}

/// Send `text` as a Clipboard frame; nothing for an empty or oversized clipboard.
async fn write_clipboard(w: &mut (impl AsyncWrite + Unpin), text: Option<String>) -> io::Result<()> {
    let Some(text) = text.filter(|t| t.len() <= MAX_CLIPBOARD as usize) else { return Ok(()) };
    let mut frame = Msg::Clipboard { len: text.len() as u32 }.encode().to_vec();
    frame.extend_from_slice(text.as_bytes());
    w.write_all(&frame).await
}

/// The bytes after a Clipboard frame; `None` if they are not UTF-8 (still consumed, so framing holds).
async fn read_clipboard(r: &mut (impl AsyncRead + Unpin), len: u32) -> io::Result<Option<String>> {
    let mut b = vec![0; len as usize];
    r.read_exact(&mut b).await?;
    Ok(String::from_utf8(b).ok())
}

/// Receiver-side cursor: absolute position inside `area`, entered through `edge`.
pub struct Receiver {
    area: Rect,
    edge: Edge,
    x: f64,
    y: f64,
    buttons: u8,
    keys: Vec<u16>,
}

#[derive(Debug, PartialEq)]
pub enum Applied {
    Inject(Injected),
    /// Hand control back; fraction along the entry edge.
    Return(f32),
}

impl Receiver {
    /// The sender's right edge meets our left edge, etc.
    pub fn enter(area: Rect, sender_edge: Edge, y: f32) -> (Self, Injected) {
        let edge = sender_edge.opposite();
        let (x, y) = edge_point(area, edge, y as f64, ENTER_INSET);
        (Self { area, edge, x, y, buttons: 0, keys: Vec::new() }, Injected::MoveTo { x, y })
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
                self.x = nx.clamp(a.x, a.x + a.w - 1.0);
                self.y = ny.clamp(a.y, a.y + a.h - 1.0);
                // never hand back mid-drag: the button-up would land on the wrong machine
                if crossed && self.buttons == 0 {
                    let along = match self.edge {
                        Edge::Left | Edge::Right => (self.y - a.y) / a.h,
                        Edge::Top | Edge::Bottom => (self.x - a.x) / a.w,
                    };
                    return Applied::Return(along as f32);
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
        }
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

/// Sender side: capture local input and stream it until the peer returns control, Ctrl+Alt+Esc, or an error.
/// Local input is always restored before returning.
pub async fn send_session(engine: Arc<dyn SpatialPlatformEngine>, to: SocketAddr, portal: Portal, y: f32) -> io::Result<()> {
    let s = timeout(Duration::from_secs(2), TcpStream::connect(to)).await??;
    s.set_nodelay(true)?;
    let (mut rd, mut wr) = s.into_split();
    wr.write_all(&Msg::Enter { edge: portal.edge, y }.encode()).await?;
    let fresh = clipboard_text().filter(|t| LAST_SHARED.lock().unwrap().as_ref() != Some(t));
    timeout(WRITE_TIMEOUT, write_clipboard(&mut wr, fresh.clone())).await??;
    if fresh.is_some() {
        *LAST_SHARED.lock().unwrap() = fresh;
    }

    let (tx, mut rx) = unbounded_channel();
    let m = portal.monitor;
    engine.start_capture((m.x + m.w / 2.0, m.y + m.h / 2.0), tx).map_err(io::Error::other)?;

    // reading in its own task keeps frames intact when the select below picks the other branch
    // shortcut: the Client's clipboard only comes back on an edge return, not on Ctrl+Alt+Esc; add a Leave ack if needed.
    let closed = || io::Error::new(io::ErrorKind::ConnectionAborted, "peer closed the session");
    let mut returned = tokio::spawn(async move {
        loop {
            match read_msg(&mut rd).await {
                Ok(Msg::Return { y }) => return Ok(y),
                Ok(Msg::Refuse(r)) => return Err(io::Error::new(io::ErrorKind::PermissionDenied, r.text())),
                Ok(Msg::Clipboard { len }) => match read_clipboard(&mut rd, len).await {
                    Ok(Some(text)) => {
                        set_clipboard_text(&text);
                        *LAST_SHARED.lock().unwrap() = Some(text);
                    }
                    Ok(None) => {}
                    Err(_) => return Err(closed()),
                },
                Ok(_) => {}
                Err(_) => return Err(closed()),
            }
        }
    });
    let mut back_y = y;
    let result: io::Result<()> = async {
        loop {
            tokio::select! {
                c = rx.recv() => match c {
                    Some(Captured::Input(e)) => timeout(WRITE_TIMEOUT, wr.write_all(&Msg::Input(e).encode())).await??,
                    Some(Captured::Escape) | None => {
                        let _ = timeout(WRITE_TIMEOUT, wr.write_all(&Msg::Leave.encode())).await;
                        return Ok(());
                    }
                },
                r = &mut returned => {
                    back_y = r.unwrap_or_else(|_| Err(closed()))?;
                    return Ok(());
                }
            }
        }
    }
    .await;
    returned.abort();
    engine.stop_capture(portal.point(back_y, RETURN_INSET));
    result
}

/// Where received input goes: the real OS, or the simulator window.
pub trait Sink: Send {
    fn area(&self) -> Rect;
    fn inject(&mut self, e: Injected);
    /// Clipboard text to hand back with control; the simulator has none of its own.
    fn clipboard(&self) -> Option<String> {
        None
    }
    fn set_clipboard(&mut self, _text: String) {}
}

/// Main side of pairing: resolves once the client's user answers (or the request times out).
pub async fn request_pair(to: SocketAddr) -> io::Result<bool> {
    let mut s = timeout(Duration::from_secs(2), TcpStream::connect(to)).await??;
    s.write_all(&Msg::PairRequest.encode()).await?;
    match timeout(PAIR_TIMEOUT + Duration::from_secs(5), read_msg(&mut s)).await? {
        Ok(Msg::PairResponse { accepted }) => Ok(accepted),
        Ok(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected reply to pair request")),
        Err(e) => Err(e),
    }
}

/// How long a client's user has to accept a pairing request.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(30);

/// Client side. One connection at a time: a pairing request (answered by `ask_pair`) or an input
/// session, which only peers passing `allow_session` (paired mains) may start. `make_sink` gets the
/// Main's address and the edge the cursor enters (and leaves) through.
pub async fn serve<S: Sink, F: Future<Output = bool>>(
    port: u16,
    allow_session: impl Fn(IpAddr) -> Result<(), Refusal>,
    ask_pair: impl Fn(IpAddr) -> F,
    mut make_sink: impl FnMut(IpAddr, Edge) -> Option<S>,
) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    loop {
        let (mut s, from) = listener.accept().await?;
        let ip = from.ip();
        let _ = s.set_nodelay(true);
        // a silent connection must not block the (sequential) accept loop
        let Ok(Ok(first)) = timeout(Duration::from_secs(5), read_msg(&mut s)).await else { continue };
        match first {
            Msg::PairRequest => {
                let accepted = ask_pair(ip).await;
                let _ = s.write_all(&Msg::PairResponse { accepted }.encode()).await;
            }
            Msg::Enter { edge, y } => match allow_session(ip).and_then(|()| make_sink(ip, edge.opposite()).ok_or(Refusal::NoScreen)) {
                Ok(mut sink) => {
                    if let Err(e) = session(s, &mut sink, edge, y).await {
                        eprintln!("kvm session from {from} ended: {e}");
                    }
                }
                Err(r) => {
                    // let the Main hang up first: closing with its clipboard frame unread resets the link and loses the Refuse
                    let _ = timeout(WRITE_TIMEOUT, async {
                        s.write_all(&Msg::Refuse(r).encode()).await?;
                        tokio::io::copy(&mut s, &mut tokio::io::sink()).await
                    })
                    .await;
                }
            },
            _ => {}
        }
    }
}

async fn session(mut s: TcpStream, sink: &mut impl Sink, edge: Edge, y: f32) -> io::Result<()> {
    let (mut rcv, first) = Receiver::enter(sink.area(), edge, y);
    sink.inject(first);
    // hand the clipboard back only if it changed here, so the Main's own (maybe an image) survives
    let mut seen = sink.clipboard();
    let result = loop {
        match read_msg(&mut s).await {
            Ok(Msg::Input(e)) => match rcv.apply(e) {
                Applied::Inject(i) => sink.inject(i),
                Applied::Return(y) => {
                    // clipboard first: the Main stops reading once it sees Return
                    let now = sink.clipboard();
                    if now != seen {
                        if let Err(e) = write_clipboard(&mut s, now).await {
                            break Err(e);
                        }
                    }
                    break s.write_all(&Msg::Return { y }.encode()).await;
                }
            },
            Ok(Msg::Clipboard { len }) => match read_clipboard(&mut s, len).await {
                Ok(Some(text)) => {
                    sink.set_clipboard(text.clone());
                    seen = Some(text);
                }
                Ok(None) => {}
                Err(e) => break Err(e),
            },
            Ok(Msg::Leave) => break Ok(()),
            Ok(_) => {}
            Err(e) => break Err(e),
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
            Msg::PairRequest,
            Msg::PairResponse { accepted: true },
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
        let (mut r, first) = Receiver::enter(AREA, Edge::Right, 0.5);
        assert_eq!(first, Injected::MoveTo { x: 8.0, y: 250.0 });
        // jitter back toward the edge right after entering must not hand control back
        assert_eq!(r.apply(InputEvent::Move { dx: -3, dy: 0 }), Applied::Inject(Injected::MoveTo { x: 5.0, y: 250.0 }));
        assert_eq!(r.apply(InputEvent::Move { dx: 95, dy: -50 }), Applied::Inject(Injected::MoveTo { x: 100.0, y: 200.0 }));
        // pushing past the right edge just clamps
        assert_eq!(r.apply(InputEvent::Move { dx: 5000, dy: 0 }), Applied::Inject(Injected::MoveTo { x: 999.0, y: 200.0 }));
        assert_eq!(r.apply(InputEvent::Move { dx: -5000, dy: 0 }), Applied::Return(0.4));
    }

    #[test]
    fn no_return_while_dragging_and_release_all_unsticks() {
        let (mut r, _) = Receiver::enter(AREA, Edge::Right, 0.5);
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

    struct Recorder(Arc<Mutex<Vec<Injected>>>);
    impl Sink for Recorder {
        fn area(&self) -> Rect {
            AREA
        }
        fn inject(&mut self, e: Injected) {
            self.0.lock().unwrap().push(e);
        }
    }

    #[tokio::test]
    async fn leave_releases_held_keys_on_receiver() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let port = 53398;
        tokio::spawn(serve(port, |_| Ok(()), |_| async { false }, move |_, _| Some(Recorder(g.clone()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        for m in [
            Msg::Enter { edge: Edge::Left, y: 0.0 },
            Msg::Input(InputEvent::Key { scan: 0x1D, down: true }),
            Msg::Leave,
        ] {
            s.write_all(&m.encode()).await.unwrap();
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
    async fn unpaired_main_is_ignored_until_the_user_accepts_pairing() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let paired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (g, p1, p2) = (got.clone(), paired.clone(), paired.clone());
        let port = 53397;
        let allow = move |_| if p1.load(std::sync::atomic::Ordering::SeqCst) { Ok(()) } else { Err(Refusal::NotPaired) };
        let ask = move |_| {
            let p = p2.clone();
            async move {
                p.store(true, std::sync::atomic::Ordering::SeqCst); // user taps "Accept"
                true
            }
        };
        tokio::spawn(serve(port, allow, ask, move |_, _| Some(Recorder(g.clone()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let to: SocketAddr = ([127, 0, 0, 1], port).into();
        let enter = Msg::Enter { edge: Edge::Left, y: 0.0 }.encode();

        let mut s = TcpStream::connect(to).await.unwrap();
        s.write_all(&enter).await.unwrap();
        assert_eq!(read_msg(&mut s).await.unwrap(), Msg::Refuse(Refusal::NotPaired), "the Main is told why");
        assert!(got.lock().unwrap().is_empty(), "session before pairing must be dropped");

        assert!(request_pair(to).await.unwrap());
        let mut s = TcpStream::connect(to).await.unwrap();
        s.write_all(&enter).await.unwrap();
        s.write_all(&Msg::Leave.encode()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*got.lock().unwrap(), vec![Injected::MoveTo { x: 991.0, y: 0.0 }]);
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
        tokio::spawn(serve(port, |_| Err(Refusal::NotClient), |_| async { false }, |_, _| None::<Recorder>));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let engine = Arc::new(Captor::default());
        let portal = Portal { edge: Edge::Right, monitor: AREA, start: 0.0, end: 1.0, reach: 100.0 };
        let e = send_session(engine.clone(), ([127, 0, 0, 1], port).into(), portal, 0.5).await.unwrap_err();
        assert_eq!(e.to_string(), Refusal::NotClient.text());
        assert_eq!(*engine.warped.lock().unwrap(), Some(portal.point(0.5, RETURN_INSET)));
    }

    /// Receiver's clipboard; records what the Main sent.
    struct Clip {
        own: Arc<Mutex<Option<String>>>,
    }
    impl Sink for Clip {
        fn area(&self) -> Rect {
            AREA
        }
        fn inject(&mut self, _: Injected) {}
        fn clipboard(&self) -> Option<String> {
            self.own.lock().unwrap().clone()
        }
        fn set_clipboard(&mut self, text: String) {
            *self.own.lock().unwrap() = Some(text);
        }
    }

    async fn crossing(port: u16, own: Arc<Mutex<Option<String>>>, send: Option<&str>, copy_on_client: Option<&str>) -> Option<String> {
        let o = own.clone();
        tokio::spawn(serve(port, |_| Ok(()), |_| async { false }, move |_, _| Some(Clip { own: o.clone() })));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(&Msg::Enter { edge: Edge::Left, y: 0.5 }.encode()).await.unwrap();
        write_clipboard(&mut s, send.map(String::from)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(c) = copy_on_client {
            *own.lock().unwrap() = Some(c.into()); // user copies something on the Client
        }
        // entered on the receiver's right edge; pushing right crosses back
        s.write_all(&Msg::Input(InputEvent::Move { dx: 5000, dy: 0 }).encode()).await.unwrap();
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
}
