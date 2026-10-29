//! Payload transfer: one TCP connection per payload.
//! `u32 LE header_len | JSON Header | size bytes`.

use serde::{Deserialize, Serialize};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub const PORT: u16 = 53320;
const CHUNK: usize = 256 * 1024;
const MAX_HEADER: u32 = 64 * 1024;
const MAX_TEXT: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Text,
    Url,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Header {
    pub kind: Kind,
    pub name: String,
    pub size: u64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TransferEvent {
    Progress { name: String, done: u64, total: u64 },
    File { path: PathBuf },
    Text { kind: Kind, text: String },
}

async fn write_header(s: &mut TcpStream, h: &Header) -> io::Result<()> {
    let json = serde_json::to_vec(h)?;
    s.write_all(&(json.len() as u32).to_le_bytes()).await?;
    s.write_all(&json).await
}

pub async fn send_file(to: SocketAddr, path: &Path, mut progress: impl FnMut(u64, u64)) -> io::Result<()> {
    let mut f = File::open(path).await?;
    let size = f.metadata().await?.len();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let mut s = TcpStream::connect(to).await?;
    s.set_nodelay(true)?;
    write_header(&mut s, &Header { kind: Kind::File, name, size }).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0;
    while done < size {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file shrank while sending"));
        }
        s.write_all(&buf[..n]).await?;
        done += n as u64;
        progress(done, size);
    }
    s.shutdown().await
}

pub async fn send_text(to: SocketAddr, kind: Kind, text: &str) -> io::Result<()> {
    let mut s = TcpStream::connect(to).await?;
    write_header(&mut s, &Header { kind, name: String::new(), size: text.len() as u64 }).await?;
    s.write_all(text.as_bytes()).await?;
    s.shutdown().await
}

/// Accept payloads forever. Connections from IPs where `allow` is false are dropped unread.
pub async fn serve(
    port: u16,
    dir: PathBuf,
    allow: impl Fn(IpAddr) -> bool + Send + Sync + 'static,
    on_event: impl Fn(TransferEvent) + Send + Sync + 'static,
) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let on_event = Arc::new(on_event);
    loop {
        let (s, from) = listener.accept().await?;
        if !allow(from.ip()) {
            continue;
        }
        let (dir, on_event) = (dir.clone(), on_event.clone());
        tokio::spawn(async move {
            if let Err(e) = receive(s, &dir, &*on_event).await {
                eprintln!("transfer from {from} failed: {e}");
            }
        });
    }
}

async fn receive(mut s: TcpStream, dir: &Path, on_event: &(impl Fn(TransferEvent) + ?Sized)) -> io::Result<()> {
    let len = s.read_u32_le().await?;
    if len > MAX_HEADER {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "header too large"));
    }
    let mut json = vec![0u8; len as usize];
    s.read_exact(&mut json).await?;
    let h: Header = serde_json::from_slice(&json)?;

    if h.kind != Kind::File {
        if h.size > MAX_TEXT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "text too large"));
        }
        let mut text = vec![0u8; h.size as usize];
        s.read_exact(&mut text).await?;
        let text = String::from_utf8(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        on_event(TransferEvent::Text { kind: h.kind, text });
        return Ok(());
    }

    let (path, mut f) = create_unique(dir, &h.name).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0;
    while done < h.size {
        let want = CHUNK.min((h.size - done) as usize);
        let n = s.read(&mut buf[..want]).await?;
        if n == 0 {
            drop(f);
            let _ = tokio::fs::remove_file(&path).await; // don't leave a truncated file behind
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "sender closed early"));
        }
        f.write_all(&buf[..n]).await?;
        done += n as u64;
        on_event(TransferEvent::Progress { name: h.name.clone(), done, total: h.size });
    }
    f.flush().await?;
    on_event(TransferEvent::File { path });
    Ok(())
}

/// Keep only the last path component (no `..`, no absolute paths) and never overwrite.
async fn create_unique(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let base = Path::new(name).file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).unwrap_or("file");
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (base, String::new()),
    };
    tokio::fs::create_dir_all(dir).await?;
    for i in 0..1000 {
        let candidate = if i == 0 { dir.join(base) } else { dir.join(format!("{stem} ({i}){ext}")) };
        match OpenOptions::new().write(true).create_new(true).open(&candidate).await {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "too many files with this name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn file_and_text_roundtrip_with_safe_names() {
        let dir = std::env::temp_dir().join(format!("loka-test-{}", uuid::Uuid::new_v4()));
        let src = std::env::temp_dir().join(format!("loka-src-{}.bin", uuid::Uuid::new_v4()));
        let data: Vec<u8> = (0..CHUNK * 2 + 17).map(|i| i as u8).collect();
        std::fs::write(&src, &data).unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let port = 53399;
        tokio::spawn(serve(port, dir.clone(), |_| true, move |e| ev.lock().unwrap().push(e)));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let to: SocketAddr = ([127, 0, 0, 1], port).into();

        let mut last = 0;
        send_file(to, &src, |d, _| last = d).await.unwrap();
        send_file(to, &src, |_, _| {}).await.unwrap();
        // hostile name must stay inside `dir`
        let mut s = TcpStream::connect(to).await.unwrap();
        write_header(&mut s, &Header { kind: Kind::File, name: "../../evil.txt".into(), size: 2 }).await.unwrap();
        s.write_all(b"hi").await.unwrap();
        s.shutdown().await.unwrap();
        send_text(to, Kind::Url, "https://loka.dev").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(last, data.len() as u64);
        let src_name = src.file_name().unwrap().to_str().unwrap();
        assert_eq!(std::fs::read(dir.join(src_name)).unwrap(), data);
        let (stem, _) = src_name.rsplit_once('.').unwrap();
        assert_eq!(std::fs::read(dir.join(format!("{stem} (1).bin"))).unwrap(), data);
        assert_eq!(std::fs::read(dir.join("evil.txt")).unwrap(), b"hi");
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, TransferEvent::Text { kind: Kind::Url, text } if text == "https://loka.dev")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&src);
    }
}
