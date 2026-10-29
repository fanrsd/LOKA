//! Remote terminal: the paired device's default shell (in its home directory) in a PTY, over the pinned
//! TLS link. Pairing plus the owner's opt-in is the auth; there are no passwords.
//!
//! Wire format after the service handshake (little-endian):
//! - client → server: first `cols: u16, rows: u16`, then frames `1 | len: u16 | len bytes` (keyboard
//!   input) or `2 | cols: u16 | rows: u16` (resize). Any other tag or a zero size ends the session.
//! - server → client: raw PTY output; the stream closes when the shell exits.
//!
//! The shell is killed as soon as the client goes away.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::AppHandle;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::network::secure::ServerStream;

const DATA: u8 = 1;
const RESIZE: u8 = 2;

type Master = Mutex<Option<Box<dyn MasterPty + Send>>>;

fn other(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

async fn read_size(r: &mut (impl AsyncRead + Unpin)) -> io::Result<PtySize> {
    let (cols, rows) = (r.read_u16_le().await?, r.read_u16_le().await?);
    if cols == 0 || rows == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty terminal size"));
    }
    Ok(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
}

/// Runs one shell for an authenticated, permitted peer until either side ends.
pub async fn serve(s: ServerStream) -> io::Result<()> {
    let (mut rd, mut wr) = tokio::io::split(s);
    let size = read_size(&mut rd).await?;
    let pair = native_pty_system().openpty(size).map_err(other)?;
    let mut child = pair.slave.spawn_command(CommandBuilder::new_default_prog()).map_err(other)?;
    drop(pair.slave);
    let mut killer = child.clone_killer();
    let mut reader = pair.master.try_clone_reader().map_err(other)?;
    let mut writer = pair.master.take_writer().map_err(other)?;
    let master: Arc<Master> = Arc::new(Mutex::new(Some(pair.master)));

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let m = master.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        // ConPTY's output only reaches EOF once the pseudo console is closed
        m.lock().unwrap().take();
    });
    let (in_tx, mut in_rx) = mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        while let Some(d) = in_rx.blocking_recv() {
            if writer.write_all(&d).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    });

    let output = async {
        while let Some(d) = out_rx.recv().await {
            wr.write_all(&d).await?;
        }
        wr.shutdown().await
    };
    let r = tokio::select! {
        r = input(&mut rd, &in_tx, &master) => r,
        r = output => r,
    };
    let _ = killer.kill();
    r
}

async fn input(rd: &mut (impl AsyncRead + Unpin), to_pty: &mpsc::Sender<Vec<u8>>, master: &Master) -> io::Result<()> {
    loop {
        match rd.read_u8().await? {
            DATA => {
                let mut d = vec![0; rd.read_u16_le().await? as usize];
                rd.read_exact(&mut d).await?;
                if to_pty.send(d).await.is_err() {
                    return Ok(());
                }
            }
            RESIZE => {
                let size = read_size(rd).await?;
                if let Some(m) = master.lock().unwrap().as_ref() {
                    m.resize(size).map_err(other)?;
                }
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown terminal frame")),
        }
    }
}

/// Open sessions: id → encoded frames for that session's writer task.
static SESSIONS: Mutex<BTreeMap<u64, mpsc::UnboundedSender<Vec<u8>>>> = Mutex::new(BTreeMap::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn size_bytes(cols: u16, rows: u16) -> [u8; 4] {
    let (c, r) = (cols.max(1).to_le_bytes(), rows.max(1).to_le_bytes());
    [c[0], c[1], r[0], r[1]]
}

/// Opens a shell on paired device `fingerprint`. Output arrives on `on_data` as `ArrayBuffer`s; an empty
/// one means the session ended.
#[tauri::command]
pub async fn term_open(app: AppHandle, fingerprint: String, cols: u16, rows: u16, on_data: Channel<InvokeResponseBody>) -> Result<u64, String> {
    let s = crate::open_service(&app, &fingerprint, crate::kvm::Service::Shell).await?;
    let (mut rd, mut wr) = tokio::io::split(s);
    wr.write_all(&size_bytes(cols, rows)).await.map_err(|e| e.to_string())?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    SESSIONS.lock().unwrap().insert(id, tx);
    tauri::async_runtime::spawn(async move {
        while let Some(f) = rx.recv().await {
            if wr.write_all(&f).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    tauri::async_runtime::spawn(async move {
        let mut buf = vec![0u8; 16384];
        while let Ok(n @ 1..) = rd.read(&mut buf).await {
            if on_data.send(InvokeResponseBody::Raw(buf[..n].to_vec())).is_err() {
                break;
            }
        }
        SESSIONS.lock().unwrap().remove(&id);
        let _ = on_data.send(InvokeResponseBody::Raw(Vec::new()));
    });
    Ok(id)
}

fn push(id: u64, frames: impl IntoIterator<Item = Vec<u8>>) {
    // an unknown id is a session that already ended: the UI says so already
    if let Some(tx) = SESSIONS.lock().unwrap().get(&id) {
        frames.into_iter().for_each(|f| {
            let _ = tx.send(f);
        });
    }
}

#[tauri::command]
pub fn term_write(id: u64, data: String) {
    push(id, data.as_bytes().chunks(u16::MAX as usize).map(|c| [&[DATA][..], &(c.len() as u16).to_le_bytes(), c].concat()));
}

#[tauri::command]
pub fn term_resize(id: u64, cols: u16, rows: u16) {
    push(id, [[&[RESIZE][..], &size_bytes(cols, rows)].concat()]);
}

#[tauri::command]
pub fn term_close(id: u64) {
    SESSIONS.lock().unwrap().remove(&id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::secure::{Identity, Tls};
    use std::time::Duration;
    use tokio::time::timeout;

    async fn read_until(s: &mut (impl AsyncRead + Unpin), seen: &mut Vec<u8>, marker: &str) {
        let mut buf = [0u8; 4096];
        while !String::from_utf8_lossy(seen).contains(marker) {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "stream ended before {marker:?}: {}", String::from_utf8_lossy(seen));
            seen.extend_from_slice(&buf[..n]);
        }
    }

    fn data(s: &str) -> Vec<u8> {
        [&[DATA][..], &(s.len() as u16).to_le_bytes(), s.as_bytes()].concat()
    }

    #[tokio::test]
    async fn remote_shell_runs_commands_and_ends_when_the_shell_exits() {
        let (srv, cli) = (Tls::new(&Identity::generate()), Tls::new(&Identity::generate()));
        let expect = srv.me.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            serve(srv.accept(tcp).await.unwrap().0).await
        });
        let (mut s, _) = cli.connect(addr, Some(&expect)).await.unwrap();
        s.write_all(&size_bytes(100, 30)).await.unwrap();
        // the PTY echoes what was typed, so the marker must only appear once the shell has expanded it
        #[cfg(windows)]
        let (cmd, marker) = ("echo LOKA_%OS%\r", "LOKA_Windows_NT");
        #[cfg(not(windows))]
        let (cmd, marker) = ("echo LOKA_$((6*7))\r", "LOKA_42");
        let mut seen = Vec::new();
        // ConPTY asks for the cursor position first and waits; xterm.js answers that by itself
        #[cfg(windows)]
        {
            timeout(Duration::from_secs(20), read_until(&mut s, &mut seen, "\x1b[6n")).await.expect("cursor query");
            s.write_all(&data("\x1b[1;1R")).await.unwrap();
        }
        s.write_all(&[data(cmd), [&[RESIZE][..], &size_bytes(120, 40)].concat()].concat()).await.unwrap();
        let got = timeout(Duration::from_secs(20), read_until(&mut s, &mut seen, marker)).await;
        assert!(got.is_ok(), "marker never came back: {:?}", String::from_utf8_lossy(&seen));

        s.write_all(&data("exit\r")).await.unwrap();
        timeout(Duration::from_secs(20), s.read_to_end(&mut seen)).await.expect("stream ends after exit").unwrap();
        server.await.unwrap().unwrap();
    }
}
