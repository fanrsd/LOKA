//! Shared mouse & keyboard. Pushing the cursor into the portal hands input to the paired device over TCP;
//! pushing it out of that device's opposite edge (or Ctrl+Alt+Esc) hands it back.
//!
//! Wire: fixed 9-byte frames `tag | payload (8 bytes, LE)`.

use crate::platform::{edge_point, Captured, Edge, Injected, InputEvent, Portal, Rect, SpatialPlatformEngine};
use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
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
            _ => return None,
        })
    }
}

async fn read_msg(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Msg> {
    let mut b = [0u8; FRAME];
    r.read_exact(&mut b).await?;
    Msg::decode(&b).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad kvm frame"))
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

    let (tx, mut rx) = unbounded_channel();
    let m = portal.monitor;
    engine.start_capture((m.x + m.w / 2.0, m.y + m.h / 2.0), tx).map_err(io::Error::other)?;

    // reading in its own task keeps frames intact when the select below picks the other branch
    let mut returned = tokio::spawn(async move {
        loop {
            match read_msg(&mut rd).await {
                Ok(Msg::Return { y }) => return Some(y),
                Ok(_) => {}
                Err(_) => return None,
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
                    return match r.ok().flatten() {
                        Some(y) => { back_y = y; Ok(()) }
                        None => Err(io::Error::new(io::ErrorKind::ConnectionAborted, "peer closed the session")),
                    };
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
/// session, which only peers passing `allow_session` (paired mains) may start.
pub async fn serve<S: Sink, F: Future<Output = bool>>(
    port: u16,
    allow_session: impl Fn(IpAddr) -> bool,
    ask_pair: impl Fn(IpAddr) -> F,
    mut make_sink: impl FnMut() -> Option<S>,
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
            Msg::Enter { edge, y } if allow_session(ip) => {
                let Some(mut sink) = make_sink() else { continue };
                if let Err(e) = session(s, &mut sink, edge, y).await {
                    eprintln!("kvm session from {from} ended: {e}");
                }
            }
            _ => {}
        }
    }
}

async fn session(mut s: TcpStream, sink: &mut impl Sink, edge: Edge, y: f32) -> io::Result<()> {
    let (mut rcv, first) = Receiver::enter(sink.area(), edge, y);
    sink.inject(first);
    let result = loop {
        match read_msg(&mut s).await {
            Ok(Msg::Input(e)) => match rcv.apply(e) {
                Applied::Inject(i) => sink.inject(i),
                Applied::Return(y) => break s.write_all(&Msg::Return { y }.encode()).await,
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
            Msg::PairRequest,
            Msg::PairResponse { accepted: true },
        ];
        for m in all {
            assert_eq!(Msg::decode(&m.encode()), Some(m));
        }
        let mut bad = Msg::Return { y: 0.5 }.encode();
        bad[1..5].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(Msg::decode(&bad), None);
        assert_eq!(Msg::decode(&[3, 9, 1, 0, 0, 0, 0, 0, 0]), None); // button 9
        assert_eq!(Msg::decode(&[0; FRAME]), None);
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
        tokio::spawn(serve(port, |_| true, |_| async { false }, move || Some(Recorder(g.clone()))));
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
        let allow = move |_| p1.load(std::sync::atomic::Ordering::SeqCst);
        let ask = move |_| {
            let p = p2.clone();
            async move {
                p.store(true, std::sync::atomic::Ordering::SeqCst); // user taps "Accept"
                true
            }
        };
        tokio::spawn(serve(port, allow, ask, move || Some(Recorder(g.clone()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let to: SocketAddr = ([127, 0, 0, 1], port).into();
        let enter = Msg::Enter { edge: Edge::Left, y: 0.0 }.encode();

        TcpStream::connect(to).await.unwrap().write_all(&enter).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(got.lock().unwrap().is_empty(), "session before pairing must be dropped");

        assert!(request_pair(to).await.unwrap());
        let mut s = TcpStream::connect(to).await.unwrap();
        s.write_all(&enter).await.unwrap();
        s.write_all(&Msg::Leave.encode()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*got.lock().unwrap(), vec![Injected::MoveTo { x: 991.0, y: 0.0 }]);
    }
}
