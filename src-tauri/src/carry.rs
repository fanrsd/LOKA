//! A file drag carried across the edge with the cursor. The files travel as a transfer batch while the
//! button stays held, announced first by a manifest (names + sizes). On this side a native drag starts under
//! the cursor once the held button (Drag frame / return) and the manifest are both here, in either order:
//! its files are virtual and fill in as the transfer arrives (`Growing`). Without a manifest it waits for
//! every file (batch complete) instead. Released before the drag could start: the files are saved to the
//! received folder.

use crate::network::transfer::{self, Promised};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    time::Duration,
};

#[cfg(target_os = "windows")]
#[path = "platform/vdrag_windows.rs"]
mod vdrag;
#[cfg(target_os = "macos")]
#[path = "platform/vdrag_macos.rs"]
mod vdrag;

#[derive(Debug, PartialEq)]
pub enum Step {
    /// start a native drag of these files now
    Start(Vec<PathBuf>),
    /// start a native drag of drag batch `.0`'s files, still arriving
    Stream(u64, Vec<Promised>),
    /// the button went up first: keep the files as a plain transfer
    Save(Vec<PathBuf>),
    Wait,
}

enum State {
    /// button held here, nothing to drag yet
    Waiting,
    /// manifest here, the cursor (and its held button) not yet
    Announced(Vec<Promised>),
    /// files here, the cursor not yet
    Ready(Vec<PathBuf>),
    /// button released before the drag could start
    Released,
    /// a streamed drag started; the files just finish landing
    Streaming,
}

#[derive(Default)]
pub struct Carry(HashMap<u64, State>);

impl Carry {
    /// The cursor arrived here with drag `batch` and its button held.
    pub fn held(&mut self, batch: u64) -> Step {
        match self.0.remove(&batch) {
            Some(State::Ready(files)) => Step::Start(files),
            Some(State::Announced(files)) => {
                self.0.insert(batch, State::Streaming);
                Step::Stream(batch, files)
            }
            _ => {
                self.0.insert(batch, State::Waiting);
                Step::Wait
            }
        }
    }

    /// Drag `batch` announced its files.
    pub fn manifest(&mut self, batch: u64, files: Vec<Promised>, button_down: bool) -> Step {
        match self.0.remove(&batch) {
            Some(State::Waiting) if button_down => {
                self.0.insert(batch, State::Streaming);
                Step::Stream(batch, files)
            }
            Some(State::Waiting | State::Released) => {
                self.0.insert(batch, State::Released);
                Step::Wait
            }
            // the files beat their own manifest: the complete path is better
            Some(s @ (State::Ready(_) | State::Streaming | State::Announced(_))) => {
                self.0.insert(batch, s);
                Step::Wait
            }
            None => {
                self.0.insert(batch, State::Announced(files));
                Step::Wait
            }
        }
    }

    /// Every file of drag `batch` arrived.
    pub fn files(&mut self, batch: u64, files: Vec<PathBuf>, button_down: bool) -> Step {
        match self.0.remove(&batch) {
            Some(State::Waiting) if button_down => Step::Start(files),
            Some(State::Waiting | State::Released) => Step::Save(files),
            Some(State::Streaming) => Step::Wait,
            Some(State::Ready(_) | State::Announced(_)) | None => {
                self.0.insert(batch, State::Ready(files));
                Step::Wait
            }
        }
    }

    /// The button is up: drags still waiting to start end up saved.
    pub fn released(&mut self) {
        for s in self.0.values_mut() {
            if matches!(s, State::Waiting) {
                *s = State::Released;
            }
        }
    }

    pub fn waiting(&self) -> bool {
        self.0.values().any(|s| matches!(s, State::Waiting))
    }
}

/// Start a native drag of `files` (drag `batch`, still arriving) under the cursor. Main thread only.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn stream_drag(window: &tauri::WebviewWindow, batch: u64, files: Vec<Promised>) -> Result<(), String> {
    vdrag::start(window, batch, files)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn stream_drag(_: &tauri::WebviewWindow, _: u64, _: Vec<Promised>) -> Result<(), String> {
    Err("streamed drags need Windows or macOS".into())
}

/// No byte of a streamed batch for this long: its sender is gone.
const STALL: Duration = Duration::from_secs(30);

/// A file of a streamed drag that is still being received: reads wait for bytes not written yet and end
/// at the announced size.
pub struct Growing {
    batch: u64,
    path: PathBuf,
    pub size: u64,
    pub pos: u64,
    file: Option<File>,
}

impl Growing {
    pub fn new(batch: u64, file: &Promised) -> Self {
        Self { batch, path: file.path.clone(), size: file.size, pos: 0, file: None }
    }

    /// Bytes at `pos` into `buf` (0 at the end), or `None` while the next ones are still on their way.
    pub fn try_read(&mut self, buf: &mut [u8]) -> Option<io::Result<usize>> {
        let want = (self.size.saturating_sub(self.pos)).min(buf.len() as u64);
        if want == 0 {
            return Some(Ok(0));
        }
        // before the length: once the batch is complete, the length read after it is final
        let batch = transfer::streamed(self.batch);
        if self.file.is_none() {
            self.file = File::open(&self.path).ok();
        }
        if let Some(f) = &mut self.file {
            let r = f.metadata().and_then(|m| {
                let ready = m.len().saturating_sub(self.pos).min(want) as usize;
                if ready == 0 {
                    return Ok(0);
                }
                f.seek(SeekFrom::Start(self.pos))?;
                f.read(&mut buf[..ready])
            });
            match r {
                Ok(0) => {}
                Ok(n) => {
                    self.pos += n as u64;
                    return Some(Ok(n));
                }
                Err(e) => return Some(Err(e)),
            }
        }
        match batch {
            None => Some(Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the file arrived shorter than announced"))),
            Some((_, true)) => Some(Err(io::Error::other("the transfer failed"))),
            Some((last, _)) if last.elapsed() > STALL => Some(Err(io::Error::new(io::ErrorKind::TimedOut, "the sender stopped"))),
            Some(_) => None,
        }
    }
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.try_read(buf) {
                Some(r) => return r,
                None => std::thread::sleep(Duration::from_millis(15)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_starts_whichever_comes_last_and_saves_after_an_early_release() {
        let f = || vec![PathBuf::from("a.txt")];
        let m = || vec![Promised { name: "a.txt".into(), path: PathBuf::from("a.txt"), size: 1 }];
        let mut c = Carry::default();
        // manifest first, then the held cursor: the drag starts before any file arrived
        assert_eq!(c.manifest(1, m(), false), Step::Wait);
        assert_eq!(c.held(1), Step::Stream(1, m()));
        assert!(!c.waiting());
        assert_eq!(c.files(1, f(), true), Step::Wait, "a streamed drag is already running");
        // held cursor first, then the manifest with the button still down
        assert_eq!(c.held(2), Step::Wait);
        assert!(c.waiting());
        assert_eq!(c.manifest(2, m(), true), Step::Stream(2, m()));
        assert_eq!(c.files(2, f(), false), Step::Wait);
        // released before the manifest came: the files are saved once complete
        assert_eq!(c.held(3), Step::Wait);
        c.released();
        assert!(!c.waiting());
        assert_eq!(c.manifest(3, m(), false), Step::Wait);
        assert_eq!(c.files(3, f(), false), Step::Save(f()));
        // tiny files beat the cursor (and the manifest): the complete files are dragged
        assert_eq!(c.files(4, f(), false), Step::Wait);
        assert_eq!(c.manifest(4, m(), false), Step::Wait);
        assert_eq!(c.held(4), Step::Start(f()));
        // no manifest (older sender): wait for the files with the button held
        assert_eq!(c.held(5), Step::Wait);
        assert_eq!(c.files(5, f(), true), Step::Start(f()));
        assert!(c.0.is_empty());
    }

    #[test]
    fn growing_file_reads_as_it_arrives_and_ends_at_its_size() {
        let path = std::env::temp_dir().join(format!("loka-growing-{}", uuid::Uuid::new_v4()));
        let batch = uuid::Uuid::new_v4().as_u128() as u64;
        transfer::announce(batch);
        let mut g = Growing::new(batch, &Promised { name: "g".into(), path: path.clone(), size: 4 });
        let mut buf = [0u8; 8];
        assert!(g.try_read(&mut buf).is_none(), "not created yet");
        std::fs::write(&path, b"ab").unwrap();
        assert_eq!(g.try_read(&mut buf).unwrap().unwrap(), 2);
        assert!(g.try_read(&mut buf).is_none());
        std::fs::write(&path, b"abcd").unwrap();
        assert_eq!(g.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], b"cd");
        assert_eq!(g.read(&mut buf).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }
}
