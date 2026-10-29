//! Payload transfer: one pinned TLS connection (`secure`) per payload.
//! `u32 LE header_len | JSON Header | size bytes`.
//! Also carries the clipboard when it is a picture or files (too big for a KVM frame): pictures as one
//! PNG payload, files as a batch the receiver puts on its clipboard once every file arrived.

use super::secure::Tls;

use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
};

pub const PORT: u16 = 53320;
const CHUNK: usize = 256 * 1024;
const MAX_HEADER: u32 = 64 * 1024;
const MAX_TEXT: u64 = 1024 * 1024;
/// Clipboard picture cap (PNG bytes).
pub const MAX_IMAGE: u64 = 10 << 20;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Text,
    Url,
    /// clipboard picture, PNG
    Image,
}

/// Files sent together: copied on the sender's clipboard, or (`drag`) carried by a file drag across the edge.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct ClipBatch {
    pub id: u64,
    pub files: u32,
    #[serde(default)]
    pub drag: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Header {
    pub kind: Kind,
    pub name: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<ClipBatch>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TransferEvent {
    Progress { id: u64, outgoing: bool, name: String, done: u64, total: u64 },
    /// received and saved
    File { id: u64, path: PathBuf },
    Sent { id: u64 },
    Failed { id: u64, outgoing: bool, name: String, error: String },
    Text { kind: Kind, text: String },
    /// clipboard picture arrived (handled by the app, not shown)
    ClipImage {
        #[serde(skip)]
        png: Vec<u8>,
    },
    /// every file of a batch arrived: the top-level files/folders it copied or dragged
    ClipFiles {
        #[serde(skip)]
        roots: Vec<PathBuf>,
        #[serde(skip)]
        batch: ClipBatch,
    },
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CANCELLED: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Stops transfer `id` at its next chunk; the peer sees the connection close.
pub fn cancel(id: u64) {
    CANCELLED.lock().unwrap().get_or_insert_with(HashSet::new).insert(id);
}

fn take_cancel(id: u64) -> io::Result<()> {
    match CANCELLED.lock().unwrap().as_mut().is_some_and(|s| s.remove(&id)) {
        true => Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled")),
        false => Ok(()),
    }
}

/// Progress at most every 100 ms (plus the final chunk) so a 4 GB file doesn't flood the UI.
struct Throttle(Option<Instant>);
impl Throttle {
    fn due(&mut self, done: u64, total: u64) -> bool {
        let due = done == total || self.0.is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
        if due {
            self.0 = Some(Instant::now());
        }
        due
    }
}

async fn write_header(s: &mut (impl AsyncWrite + Unpin), h: &Header) -> io::Result<()> {
    let json = serde_json::to_vec(h)?;
    s.write_all(&(json.len() as u32).to_le_bytes()).await?;
    s.write_all(&json).await
}

/// Send one file to the device at `to`, which must be `expect` (its fingerprint).
#[allow(clippy::too_many_arguments)]
pub async fn send_file(tls: &Tls, to: SocketAddr, expect: &str, path: &Path, name: String, clip: Option<ClipBatch>, id: u64, mut progress: impl FnMut(u64, u64)) -> io::Result<()> {
    let mut f = File::open(path).await?;
    let size = f.metadata().await?.len();
    let (mut s, _) = tls.connect(to, Some(expect)).await?;
    write_header(&mut s, &Header { kind: Kind::File, name, size, clip }).await?;
    let mut buf = vec![0u8; CHUNK];
    let (mut done, mut tick) = (0, Throttle(None));
    progress(0, size);
    while done < size {
        take_cancel(id)?;
        let n = f.read(&mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file shrank while sending"));
        }
        s.write_all(&buf[..n]).await?;
        done += n as u64;
        if tick.due(done, size) {
            progress(done, size);
        }
    }
    s.shutdown().await
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into())
}

/// Files to send for dropped `paths`: folders recursively, each named relative to the drop
/// ("Photos/2024/a.jpg") so the receiver rebuilds the tree. Symlinks are skipped (no loops).
pub fn expand(paths: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = paths.iter().map(|p| (p.clone(), file_name(p))).collect();
    while let Some((p, name)) = stack.pop() {
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => {
                for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                    stack.push((e.path(), format!("{name}/{}", e.file_name().to_string_lossy())));
                }
            }
            Ok(m) if m.is_file() => out.push((p, name)),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
pub async fn send_text(tls: &Tls, to: SocketAddr, expect: &str, kind: Kind, text: &str) -> io::Result<()> {
    send_bytes(tls, to, expect, kind, text.as_bytes()).await
}

pub async fn send_bytes(tls: &Tls, to: SocketAddr, expect: &str, kind: Kind, bytes: &[u8]) -> io::Result<()> {
    let (mut s, _) = tls.connect(to, Some(expect)).await?;
    write_header(&mut s, &Header { kind, name: String::new(), size: bytes.len() as u64, clip: None }).await?;
    s.write_all(bytes).await?;
    s.shutdown().await
}

/// Per clipboard batch: files arrived so far and the top-level entries they sit under.
type Batches = Arc<Mutex<HashMap<u64, (u32, BTreeSet<PathBuf>)>>>;

/// Accept payloads forever, only from devices whose fingerprint passes `allow`.
/// Dropped files land in `dir`, clipboard files in `clip_dir/<batch>`.
pub async fn serve(
    tls: Arc<Tls>,
    port: u16,
    dir: PathBuf,
    clip_dir: PathBuf,
    allow: impl Fn(&str) -> bool + Send + Sync + 'static,
    on_event: impl Fn(TransferEvent) + Send + Sync + 'static,
) -> io::Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let (on_event, allow) = (Arc::new(on_event), Arc::new(allow));
    let batches = Batches::default();
    loop {
        let (tcp, from) = listener.accept().await?;
        let (tls, allow, dir, clip_dir, on_event, batches) = (tls.clone(), allow.clone(), dir.clone(), clip_dir.clone(), on_event.clone(), batches.clone());
        tokio::spawn(async move {
            let Ok((s, who)) = tls.accept(tcp).await else { return };
            if !allow(&who) {
                return;
            }
            if let Err(e) = receive(s, &dir, &clip_dir, &batches, &*on_event).await {
                eprintln!("transfer from {from} failed: {e}");
            }
        });
    }
}

async fn receive(mut s: impl AsyncRead + Unpin, dir: &Path, clip_dir: &Path, batches: &Batches, on_event: &(impl Fn(TransferEvent) + ?Sized)) -> io::Result<()> {
    let len = s.read_u32_le().await?;
    if len > MAX_HEADER {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "header too large"));
    }
    let mut json = vec![0u8; len as usize];
    s.read_exact(&mut json).await?;
    let h: Header = serde_json::from_slice(&json)?;

    if h.kind == Kind::Image {
        if h.size > MAX_IMAGE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "picture too large"));
        }
        let mut png = vec![0u8; h.size as usize];
        s.read_exact(&mut png).await?;
        on_event(TransferEvent::ClipImage { png });
        return Ok(());
    }
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

    let id = next_id();
    let base = match h.clip {
        Some(b) if b.drag => clip_dir.join(format!("drag-{}", b.id)),
        Some(b) => clip_dir.join(b.id.to_string()),
        None => dir.to_path_buf(),
    };
    let (path, mut f) = create_unique(&base, &h.name).await?;
    let r = async {
        let mut buf = vec![0u8; CHUNK];
        let (mut done, mut tick) = (0, Throttle(None));
        on_event(TransferEvent::Progress { id, outgoing: false, name: h.name.clone(), done, total: h.size });
        while done < h.size {
            take_cancel(id)?;
            let want = CHUNK.min((h.size - done) as usize);
            let n = s.read(&mut buf[..want]).await?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the sender stopped"));
            }
            f.write_all(&buf[..n]).await?;
            done += n as u64;
            if tick.due(done, h.size) {
                on_event(TransferEvent::Progress { id, outgoing: false, name: h.name.clone(), done, total: h.size });
            }
        }
        f.flush().await
    }
    .await;
    match r {
        Ok(()) => {
            on_event(TransferEvent::File { id, path: path.clone() });
            if let Some(b) = h.clip {
                let done = {
                    let mut all = batches.lock().unwrap();
                    let (arrived, roots) = all.entry(b.id).or_default();
                    *arrived += 1;
                    if let Some(top) = path.strip_prefix(&base).ok().and_then(|r| r.components().next()) {
                        roots.insert(base.join(top));
                    }
                    (*arrived >= b.files).then(|| all.remove(&b.id)).flatten()
                };
                if let Some((_, roots)) = done {
                    on_event(TransferEvent::ClipFiles { roots: roots.into_iter().collect(), batch: b });
                }
            }
        }
        Err(e) => {
            drop(f);
            let _ = tokio::fs::remove_file(&path).await; // don't leave a truncated file behind
            on_event(TransferEvent::Failed { id, outgoing: false, name: h.name, error: e.to_string() });
            return Err(e);
        }
    }
    Ok(())
}

/// Keep a relative path ("Photos/a.jpg") but nothing that climbs out of `dir`, and never overwrite.
async fn create_unique(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty() && *p != "." && *p != ".." && !p.contains(':')).collect();
    let (base, folders) = parts.split_last().map(|(b, f)| (*b, f)).unwrap_or(("file", &[]));
    let dir = &folders.iter().fold(dir.to_path_buf(), |d, p| d.join(p));
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
    use crate::network::secure::Identity;
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
        let (them, me, stranger) = (Arc::new(Tls::new(&Identity::generate())), Tls::new(&Identity::generate()), Tls::new(&Identity::generate()));
        let (fp, mine) = (them.me.clone(), me.me.clone());
        tokio::spawn(serve(them, port, dir.clone(), dir.join("clip"), move |who| who == mine, move |e| ev.lock().unwrap().push(e)));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let to: SocketAddr = ([127, 0, 0, 1], port).into();

        let mut last = 0;
        send_file(&me, to, &fp, &src, file_name(&src), None, next_id(), |d, _| last = d).await.unwrap();
        send_file(&me, to, &fp, &src, file_name(&src), None, next_id(), |_, _| {}).await.unwrap();
        // a cancelled send stops and the receiver deletes its partial copy
        let id = next_id();
        cancel(id);
        assert!(send_file(&me, to, &fp, &src, file_name(&src), None, id, |_, _| {}).await.is_err());
        // hostile name must stay inside `dir`
        for name in ["../../evil.txt", "C:/Photos/./../2024/a.jpg"] {
            let (mut s, _) = me.connect(to, Some(&fp)).await.unwrap();
            write_header(&mut s, &Header { kind: Kind::File, name: name.into(), size: 2, clip: None }).await.unwrap();
            s.write_all(b"hi").await.unwrap();
            s.shutdown().await.unwrap();
        }
        send_text(&me, to, &fp, Kind::Url, "https://loka.dev").await.unwrap();
        // an unpaired device's file never lands
        let _ = send_text(&stranger, to, &fp, Kind::Text, "spam").await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        assert_eq!(last, data.len() as u64);
        let src_name = src.file_name().unwrap().to_str().unwrap();
        assert_eq!(std::fs::read(dir.join(src_name)).unwrap(), data);
        let (stem, _) = src_name.rsplit_once('.').unwrap();
        assert_eq!(std::fs::read(dir.join(format!("{stem} (1).bin"))).unwrap(), data);
        assert_eq!(std::fs::read(dir.join("evil.txt")).unwrap(), b"hi");
        assert_eq!(std::fs::read(dir.join("Photos").join("2024").join("a.jpg")).unwrap(), b"hi"); // folder drops keep their tree
        assert!(!dir.join(format!("{stem} (2).bin")).exists());
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, TransferEvent::Failed { outgoing: false, .. })));
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, TransferEvent::Text { kind: Kind::Url, text } if text == "https://loka.dev")));
        assert!(!events.lock().unwrap().iter().any(|e| matches!(e, TransferEvent::Text { text, .. } if text == "spam")));

        // a dropped folder expands to its files, named relative to the drop
        let tree = std::env::temp_dir().join(format!("loka-tree-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub").join("b.txt"), b"b").unwrap();
        let mut names: Vec<String> = expand(&[tree.clone(), src.clone()]).into_iter().map(|(_, n)| n).collect();
        names.sort();
        let top = tree.file_name().unwrap().to_str().unwrap();
        let mut want = vec![format!("{top}/sub/b.txt"), src_name.to_string()];
        want.sort();
        assert_eq!(names, want);
        let _ = std::fs::remove_dir_all(&tree);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&src);
    }

    #[tokio::test]
    async fn clipboard_files_land_together_and_pictures_pass_through() {
        let dir = std::env::temp_dir().join(format!("loka-clip-{}", uuid::Uuid::new_v4()));
        let src = std::env::temp_dir().join(format!("loka-clipsrc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(src.join("Folder")).unwrap();
        std::fs::write(src.join("Folder").join("in.txt"), b"in").unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let ev = events.clone();
        let port = 53393;
        let (them, me) = (Arc::new(Tls::new(&Identity::generate())), Tls::new(&Identity::generate()));
        let fp = them.me.clone();
        tokio::spawn(serve(them, port, dir.join("drops"), dir.join("clip"), |_| true, move |e| ev.lock().unwrap().push(e)));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let to: SocketAddr = ([127, 0, 0, 1], port).into();
        let roots = |events: &Vec<TransferEvent>| events.iter().find_map(|e| match e { TransferEvent::ClipFiles { roots, .. } => Some(roots.clone()), _ => None });

        let files = expand(&[src.join("Folder"), src.join("a.txt")]);
        let batch = ClipBatch { id: 7, files: files.len() as u32, drag: false };
        send_file(&me, to, &fp, &files[0].0, files[0].1.clone(), Some(batch), next_id(), |_, _| {}).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(roots(&events.lock().unwrap()), None, "not on the clipboard until every file arrived");
        send_file(&me, to, &fp, &files[1].0, files[1].1.clone(), Some(batch), next_id(), |_, _| {}).await.unwrap();
        send_bytes(&me, to, &fp, Kind::Image, b"\x89PNG fake").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let got = events.lock().unwrap();
        let b = dir.join("clip").join("7");
        assert_eq!(roots(&got), Some(vec![b.join("Folder"), b.join("a.txt")]));
        assert_eq!(std::fs::read(b.join("Folder").join("in.txt")).unwrap(), b"in");
        assert!(!dir.join("drops").exists(), "clipboard files never land with dropped ones");
        assert!(got.iter().any(|e| matches!(e, TransferEvent::ClipImage { png } if png == b"\x89PNG fake")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src);
    }
}
