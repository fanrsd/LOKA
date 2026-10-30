//! OS abstraction for the Bezel Portal. All coordinates are global physical pixels,
//! top-left origin of the primary display (same space as `tauri::Monitor`).

use serde::{Deserialize, Serialize};
use tauri::WebviewWindow;
use tokio::sync::mpsc::UnboundedSender;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorSample {
    pub x: f64,
    pub y: f64,
    pub button_down: bool,
}

/// Platform-neutral input. Keys are PC/AT set-1 scancodes; extended keys are `0xE0xx`.
/// Buttons: 1 left, 2 right, 3 middle, 4 back, 5 forward. Wheel: 120 per notch.
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputEvent {
    Move { dx: i32, dy: i32 },
    Button { button: u8, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { scan: u16, down: bool },
    /// a Mac trackpad's system gesture, replayed as the receiving OS's shortcut
    Gesture { gesture: Gesture },
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
pub enum Gesture {
    /// the desktop (Space) to the left / right
    SpaceLeft = 1,
    SpaceRight = 2,
    /// Mission Control / Task View
    Overview = 3,
    /// the front app's windows (App Exposé)
    AppWindows = 4,
    ShowDesktop = 5,
    ZoomIn = 6,
    ZoomOut = 7,
}

impl Gesture {
    pub fn from_u8(b: u8) -> Option<Self> {
        use Gesture::*;
        [SpaceLeft, SpaceRight, Overview, AppWindows, ShowDesktop, ZoomIn, ZoomOut].into_iter().find(|g| *g as u8 == b)
    }
}

/// Scroll Lock (set-1), the cursor-lock key as in Synergy; a Mac's F14 maps to it.
pub const LOCK_SCAN: u16 = 0x46;

/// What the local capture hook produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Captured {
    Input(InputEvent),
    /// Ctrl+Alt+Esc: always hands control back to this machine.
    Escape,
}

static ORIGIN: std::sync::LazyLock<std::time::Instant> = std::sync::LazyLock::new(std::time::Instant::now);
static FED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> u64 {
    ORIGIN.elapsed().as_millis() as u64
}

/// Dead-man switch: the session task calls this while it is alive and draining captured input.
pub fn feed() {
    FED.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
}

/// No `feed` for 1.5 s: the session task panicked or hangs, so capture must give input back on its own.
pub fn starved() -> bool {
    now_ms().saturating_sub(FED.load(std::sync::atomic::Ordering::Relaxed)) > 1500
}

/// What a receiver asks its OS to perform. Positions are absolute physical px.
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Injected {
    MoveTo { x: f64, y: f64 },
    Button { button: u8, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { scan: u16, down: bool },
}

pub trait SpatialPlatformEngine: Send + Sync {
    /// Make the overlay non-activating, hidden from taskbar/app switcher, above full-screen apps,
    /// and click-through. Main thread only.
    fn prepare_overlay(&self, window: &WebviewWindow) -> tauri::Result<()>;
    /// Toggle click-through. Overridable because some OS layers reset window styles when it changes.
    fn set_click_through(&self, window: &WebviewWindow, ignore: bool) -> tauri::Result<()> {
        window.set_ignore_cursor_events(ignore)
    }
    /// Global cursor; polled because webviews get no mouse events during external OS drags.
    fn cursor(&self) -> Option<CursorSample>;
    /// Swallow all local mouse/keyboard input and forward it to `out`; the cursor is parked at `park`.
    fn start_capture(&self, _park: (f64, f64), _out: UnboundedSender<Captured>) -> Result<(), String> {
        Err("input capture is not supported on this platform".into())
    }
    /// Give input back to the local machine and put the cursor at `warp_to`. Idempotent.
    fn stop_capture(&self, _warp_to: (f64, f64)) {}
    fn inject(&self, _event: Injected) {}
    /// Being driven: keep the cursor drawn until `false` (Windows draws none when no mouse is attached). Idempotent.
    fn show_cursor(&self, _on: bool) {}
    /// Cursor-lock key held right now on this machine's own keyboard: Scroll Lock (F14 on a Mac).
    fn lock_key_down(&self) -> bool {
        false
    }
}

pub fn engine() -> Box<dyn SpatialPlatformEngine> {
    #[cfg(target_os = "windows")]
    return Box::new(windows::WindowsEngine);
    #[cfg(target_os = "macos")]
    return Box::new(macos::MacEngine);
    #[allow(unreachable_code)]
    Box::new(NoopEngine)
}

/// A password manager marked what's on the clipboard as secret: it never leaves this computer.
pub fn clipboard_concealed() -> bool {
    #[cfg(target_os = "windows")]
    return windows::clipboard_concealed();
    #[cfg(target_os = "macos")]
    return macos::clipboard_concealed();
    #[allow(unreachable_code)]
    false
}

/// Every clipboard read/write in this process, one at a time. Windows: the clipboard is opened for the
/// process (`OpenClipboard(NULL)`), not the thread, and two threads reading a picture at once corrupt the
/// heap (STATUS_HEAP_CORRUPTION; the history watcher and a crossing do exactly that).
#[cfg(desktop)]
fn with_clipboard<T>(f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Result<T, arboard::Error> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    f(&mut arboard::Clipboard::new()?)
}

/// Plain-text clipboard. `None` when empty, not text, secret, or held by another app.
pub fn clipboard_text() -> Option<String> {
    #[cfg(desktop)]
    return with_clipboard(|c| c.get_text()).ok().filter(|t| !t.is_empty() && !clipboard_concealed());
    #[allow(unreachable_code)]
    None
}

/// Replace the clipboard text, unless it already holds exactly that (keeps clipboard-history apps clean).
pub fn set_clipboard_text(text: &str) {
    #[cfg(desktop)]
    if let Err(e) = with_clipboard(|c| if c.get_text().ok().as_deref() == Some(text) { Ok(()) } else { c.set_text(text) }) {
        eprintln!("clipboard: {e}");
    }
}

/// Picture on the clipboard: `(width, height, RGBA)`.
pub fn clipboard_image() -> Option<(usize, usize, Vec<u8>)> {
    #[cfg(desktop)]
    return with_clipboard(|c| c.get_image()).ok().map(|i| (i.width, i.height, i.bytes.into_owned()));
    #[allow(unreachable_code)]
    None
}

pub fn set_clipboard_image(width: usize, height: usize, rgba: Vec<u8>) {
    #[cfg(desktop)]
    if let Err(e) = with_clipboard(|c| c.set_image(arboard::ImageData { width, height, bytes: rgba.into() })) {
        eprintln!("clipboard image: {e}");
    }
}

/// Files and folders copied in Explorer/Finder; empty when there are none.
pub fn clipboard_files() -> Vec<std::path::PathBuf> {
    #[cfg(desktop)]
    return with_clipboard(|c| c.get().file_list()).unwrap_or_default();
    #[allow(unreachable_code)]
    Vec::new()
}

/// Put `paths` on the clipboard as copied files, ready to paste in Explorer/Finder or an app.
pub fn set_clipboard_files(paths: &[std::path::PathBuf]) {
    #[cfg(desktop)]
    if let Err(e) = with_clipboard(|c| c.set().file_list(paths)) {
        eprintln!("clipboard files: {e}");
    }
}

/// Mobile (and unsupported desktops): the phone only renders received telemetry.
struct NoopEngine;
impl SpatialPlatformEngine for NoopEngine {
    fn prepare_overlay(&self, _: &WebviewWindow) -> tauri::Result<()> {
        Ok(())
    }
    fn cursor(&self) -> Option<CursorSample> {
        None
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// Pixel on `edge` of `r` at fraction `along` (top→bottom / left→right), `inset` px inside.
pub fn edge_point(r: Rect, edge: Edge, along: f64, inset: f64) -> (f64, f64) {
    let a = along.clamp(0.0, 1.0);
    let (ax, ay) = ((r.x + a * r.w).min(r.x + r.w - 1.0), (r.y + a * r.h).min(r.y + r.h - 1.0));
    match edge {
        Edge::Left => (r.x + inset, ay),
        Edge::Right => (r.x + r.w - 1.0 - inset, ay),
        Edge::Top => (ax, r.y + inset),
        Edge::Bottom => (ax, r.y + r.h - 1.0 - inset),
    }
}

/// Calibrated segment of one monitor edge. `start`/`end` are fractions along the edge
/// (top→bottom or left→right); `reach` is the activation depth in px.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Portal {
    pub edge: Edge,
    pub monitor: Rect,
    pub start: f64,
    pub end: f64,
    pub reach: f64,
}

impl Portal {
    /// `(normalized_y, proximity)` when the cursor is within reach of the segment.
    pub fn sample(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let m = self.monitor;
        let (along, dist) = match self.edge {
            Edge::Left => ((y - m.y) / m.h, x - m.x),
            Edge::Right => ((y - m.y) / m.h, m.x + m.w - 1.0 - x),
            Edge::Top => ((x - m.x) / m.w, y - m.y),
            Edge::Bottom => ((x - m.x) / m.w, m.y + m.h - 1.0 - y),
        };
        if dist < 0.0 || dist > self.reach || along < self.start || along > self.end {
            return None;
        }
        let ny = (along - self.start) / (self.end - self.start);
        Some((ny as f32, (1.0 - dist / self.reach) as f32))
    }

    /// Area the overlay window occupies (the band sampled by `sample`).
    pub fn hit_box(&self) -> Rect {
        let m = self.monitor;
        let r = self.reach.ceil() + 1.0;
        match self.edge {
            Edge::Left | Edge::Right => Rect {
                x: if self.edge == Edge::Left { m.x } else { m.x + m.w - r },
                y: m.y + self.start * m.h,
                w: r,
                h: (self.end - self.start) * m.h,
            },
            Edge::Top | Edge::Bottom => Rect {
                x: m.x + self.start * m.w,
                y: if self.edge == Edge::Top { m.y } else { m.y + m.h - r },
                w: (self.end - self.start) * m.w,
                h: r,
            },
        }
    }

    /// Point on the calibrated segment at `normalized_y`, `inset` px inside the monitor.
    pub fn point(&self, normalized_y: f32, inset: f64) -> (f64, f64) {
        let along = self.start + normalized_y as f64 * (self.end - self.start);
        edge_point(self.monitor, self.edge, along, inset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_pictures_can_be_read_from_several_threads_at_once() {
        // the history watcher and a crossing both read the picture; unserialised, that corrupted the heap
        set_clipboard_image(64, 64, vec![200; 64 * 64 * 4]);
        let readers: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(|| (0..25).for_each(|_| drop(clipboard_image()))))
            .collect();
        readers.into_iter().for_each(|r| r.join().unwrap());
    }

    // second monitor at x=1920, 2560x1440, phone on its right edge, middle half calibrated
    const P: Portal = Portal {
        edge: Edge::Right,
        monitor: Rect { x: 1920.0, y: 0.0, w: 2560.0, h: 1440.0 },
        start: 0.25,
        end: 0.75,
        reach: 100.0,
    };

    #[test]
    fn right_edge_maps_segment_and_proximity() {
        assert_eq!(P.sample(4479.0, 360.0), Some((0.0, 1.0)));
        assert_eq!(P.sample(4479.0, 1080.0), Some((1.0, 1.0)));
        assert_eq!(P.sample(4429.0, 720.0), Some((0.5, 0.5)));
    }

    #[test]
    fn outside_reach_segment_or_monitor_is_none() {
        assert_eq!(P.sample(4378.0, 720.0), None); // 101 px away
        assert_eq!(P.sample(4479.0, 300.0), None); // above segment
        assert_eq!(P.sample(4481.0, 720.0), None); // past the monitor
    }

    #[test]
    fn point_round_trips_through_sample() {
        let (x, y) = P.point(0.5, 10.0);
        assert_eq!((x, y), (4469.0, 720.0));
        assert_eq!(P.sample(x, y), Some((0.5, 0.9)));
    }

    #[test]
    fn top_edge_uses_x_as_along() {
        let p = Portal { edge: Edge::Top, start: 0.0, end: 1.0, ..P };
        assert_eq!(p.sample(1920.0 + 1280.0, 0.0), Some((0.5, 1.0)));
    }

    #[test]
    fn hit_box_covers_band() {
        let b = P.hit_box();
        assert_eq!((b.x, b.y, b.w, b.h), (4480.0 - 101.0, 360.0, 101.0, 720.0));
        let (cx, cy) = (b.x + 1.0, b.y + 1.0); // inner corner of the band samples
        assert!(P.sample(cx + 1.0, cy).is_some());
    }
}
