//! Clipboard pictures and files. Too big for a KVM frame, so they go over the transfer channel when the
//! cursor crosses, off the input path; text stays in the session (kvm.rs). An unchanged clipboard never
//! travels, and what just arrived is never sent back.

use crate::{network::transfer, platform};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::PathBuf,
    sync::Mutex,
};

/// shortcut: copied files are sent eagerly when the cursor crosses, so a big copy stays home; upgrade to
/// sending on paste (virtual files / file promises) if people copy more than this.
const MAX_FILES: u64 = 1 << 30;

static LAST_IMAGE: Mutex<Option<u64>> = Mutex::new(None);
static LAST_FILES: Mutex<Option<u64>> = Mutex::new(None);

pub enum Outgoing {
    Image(Vec<u8>),
    /// (path, name relative to the copy) as `transfer::expand` gives them
    Files(Vec<(PathBuf, String)>),
}

/// This computer's clipboard picture or files if they changed since the last exchange. Blocking: reads
/// the clipboard and encodes PNG.
pub fn outgoing() -> Option<Outgoing> {
    if platform::clipboard_concealed() {
        return None;
    }
    let files = platform::clipboard_files();
    if !files.is_empty() {
        if !replace(&LAST_FILES, files_hash(&files)) {
            return None;
        }
        let list = transfer::expand(&files);
        let total: u64 = list.iter().filter_map(|(p, _)| std::fs::metadata(p).ok()).map(|m| m.len()).sum();
        return (total <= MAX_FILES && !list.is_empty()).then_some(Outgoing::Files(list));
    }
    // text goes in the session; a picture arriving after it would replace it (Word copies both)
    if platform::clipboard_text().is_some() {
        return None;
    }
    let (w, h, rgba) = platform::clipboard_image()?;
    if !replace(&LAST_IMAGE, image_hash(w, h, &rgba)) {
        return None;
    }
    let png = encode_png(w, h, &rgba)?;
    (png.len() as u64 <= transfer::MAX_IMAGE).then_some(Outgoing::Image(png))
}

/// A picture from the other device: onto this clipboard, remembered so it doesn't go back.
pub fn apply_image(png: &[u8]) {
    let Ok(img) = image::load_from_memory_with_format(png, image::ImageFormat::Png) else { return };
    let img = img.into_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let rgba = img.into_raw();
    *LAST_IMAGE.lock().unwrap() = Some(image_hash(w, h, &rgba));
    platform::set_clipboard_image(w, h, rgba);
}

/// Files from the other device, already saved locally: onto this clipboard for a normal paste.
pub fn apply_files(paths: &[PathBuf]) {
    *LAST_FILES.lock().unwrap() = Some(files_hash(paths));
    platform::set_clipboard_files(paths);
}

/// `true` (and remember `h`) when `h` differs from the last exchanged value.
fn replace(slot: &Mutex<Option<u64>>, h: u64) -> bool {
    let mut last = slot.lock().unwrap();
    let changed = *last != Some(h);
    *last = Some(h);
    changed
}

/// Size plus a sample of the opaque pixels' colours: translucent ones come back premultiplied from
/// another OS's clipboard, which must not make the picture look new and bounce it back.
fn image_hash(w: usize, h: usize, rgba: &[u8]) -> u64 {
    let mut s = DefaultHasher::new();
    (w, h).hash(&mut s);
    for px in rgba.chunks_exact(4).step_by(7) {
        (px[3] == 255).then_some(&px[..3]).hash(&mut s);
    }
    s.finish()
}

/// Paths plus size and modification time: copying the same file again after editing it counts as new.
fn files_hash(paths: &[PathBuf]) -> u64 {
    let mut s = DefaultHasher::new();
    for p in paths {
        p.hash(&mut s);
        if let Ok(m) = std::fs::metadata(p) {
            m.len().hash(&mut s);
            m.modified().ok().hash(&mut s);
        }
    }
    s.finish()
}

fn encode_png(w: usize, h: usize, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    image::RgbaImage::from_raw(w as u32, h as u32, rgba.to_vec())?
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

// --- Recent: the last few things copied on this computer, memory only (pictures cached on disk) ---

const HISTORY_LEN: usize = 12;
const MAX_TEXT: usize = 1 << 20;

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    id: u64,
    kind: &'static str,
    /// text: the first ~300 chars; image: `data:image/png;base64,` thumbnail
    preview: String,
    time: u64,
}

enum Data {
    Text(String),
    Image(PathBuf),
}

struct Item {
    key: u64,
    entry: Entry,
    data: Data,
}

struct History {
    items: Vec<Item>,
    next: u64,
}

impl History {
    /// Move the entry with content `key` to the top; `false` if there is none.
    fn touch(&mut self, key: u64) -> bool {
        let Some(i) = self.items.iter().position(|it| it.key == key) else { return false };
        let it = self.items.remove(i);
        self.items.insert(0, it);
        true
    }

    /// Put a new item on top; returns what fell off the end.
    fn add(&mut self, item: Item) -> Vec<Item> {
        self.items.insert(0, item);
        self.items.split_off(self.items.len().min(HISTORY_LEN))
    }
}

static HISTORY: Mutex<History> = Mutex::new(History { items: Vec::new(), next: 1 });

/// Watch this computer's clipboard (including what arrives from the other device) into Recent.
pub fn watch(app: tauri::AppHandle) {
    use tauri::Manager;
    let Ok(dir) = app.path().app_cache_dir().map(|d| d.join("history")) else { return };
    let _ = std::fs::remove_dir_all(&dir); // history doesn't outlive the app
    let _ = std::fs::create_dir_all(&dir);
    std::thread::spawn(move || {
        let mut seen = None;
        loop {
            let seq = change_count();
            if seen != Some(seq) {
                seen = Some(seq);
                if !platform::clipboard_concealed() && record(&dir) {
                    notify(&app);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(700));
        }
    });
}

/// Reads the clipboard into the history; `true` if the list changed.
fn record(dir: &std::path::Path) -> bool {
    let time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    if let Some(mut text) = platform::clipboard_text() {
        if text.len() > MAX_TEXT {
            let mut end = MAX_TEXT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        let key = text_key(&text);
        let mut h = HISTORY.lock().unwrap();
        if h.items.first().is_some_and(|it| it.key == key) {
            return false;
        }
        if h.touch(key) {
            return true;
        }
        let id = h.next;
        h.next += 1;
        let preview = text.chars().take(300).collect();
        evict(h.add(Item { key, entry: Entry { id, kind: "text", preview, time }, data: Data::Text(text) }));
        return true;
    }
    let Some((w, ht, rgba)) = platform::clipboard_image() else { return false };
    let key = image_hash(w, ht, &rgba);
    let id = {
        let mut h = HISTORY.lock().unwrap();
        if h.items.first().is_some_and(|it| it.key == key) {
            return false;
        }
        if h.touch(key) {
            return true;
        }
        h.next += 1;
        h.next - 1
    };
    let Some(img) = image::RgbaImage::from_raw(w as u32, ht as u32, rgba) else { return false };
    let path = dir.join(format!("{id}.png"));
    if img.save_with_format(&path, image::ImageFormat::Png).is_err() {
        return false;
    }
    let s = 160.0 / w.max(ht).max(1) as f32;
    let thumb = if s < 1.0 { image::imageops::thumbnail(&img, ((w as f32 * s) as u32).max(1), ((ht as f32 * s) as u32).max(1)) } else { img };
    let mut png = Vec::new();
    if thumb.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).is_err() {
        return false;
    }
    use base64::Engine;
    let preview = format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(&png));
    let evicted = HISTORY.lock().unwrap().add(Item { key, entry: Entry { id, kind: "image", preview, time }, data: Data::Image(path) });
    evict(evicted);
    true
}

fn evict(items: Vec<Item>) {
    for it in items {
        if let Data::Image(p) = it.data {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn text_key(text: &str) -> u64 {
    let mut s = DefaultHasher::new();
    text.hash(&mut s);
    s.finish()
}

fn notify(app: &tauri::AppHandle) {
    use tauri::Emitter;
    let _ = app.emit_to("main", "clip-history", ());
}

/// Cheap "did the clipboard change" counter, so the clipboard is only read when it did.
fn change_count() -> i64 {
    #[cfg(target_os = "windows")]
    return unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() } as i64;
    #[cfg(target_os = "macos")]
    return objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as i64;
    #[allow(unreachable_code)]
    0
}

#[tauri::command]
pub fn clip_history() -> Vec<Entry> {
    HISTORY.lock().unwrap().items.iter().map(|it| it.entry.clone()).collect()
}

/// Put entry `id` back on the clipboard and on top of Recent (the watcher then sees it as already there).
#[tauri::command]
pub fn clip_restore(app: tauri::AppHandle, id: u64) -> Result<(), String> {
    let (key, data) = {
        let h = HISTORY.lock().unwrap();
        let it = h.items.iter().find(|it| it.entry.id == id).ok_or("That item is gone.")?;
        (it.key, match &it.data { Data::Text(t) => Data::Text(t.clone()), Data::Image(p) => Data::Image(p.clone()) })
    };
    match data {
        Data::Text(t) => platform::set_clipboard_text(&t),
        Data::Image(p) => {
            let img = image::open(&p).map_err(|e| format!("Couldn't read the picture: {e}"))?.into_rgba8();
            platform::set_clipboard_image(img.width() as usize, img.height() as usize, img.into_raw());
        }
    }
    HISTORY.lock().unwrap().touch(key);
    notify(&app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(h: &mut History, key: u64) -> Vec<u64> {
        if h.touch(key) {
            return Vec::new();
        }
        let id = h.next;
        h.next += 1;
        h.add(Item { key, entry: Entry { id, kind: "text", preview: String::new(), time: 0 }, data: Data::Text(String::new()) })
            .iter()
            .map(|it| it.entry.id)
            .collect()
    }

    #[test]
    fn history_dedupes_moves_to_top_and_evicts_oldest() {
        let mut h = History { items: Vec::new(), next: 1 };
        for k in 0..HISTORY_LEN as u64 {
            assert!(text(&mut h, k).is_empty());
        }
        assert!(text(&mut h, 0).is_empty()); // re-copied: moved up, nothing new
        assert_eq!(h.items.len(), HISTORY_LEN);
        assert_eq!(h.items[0].entry.id, 1);
        assert_eq!(text(&mut h, 99), vec![2]); // key 1 (id 2) is now the oldest
        assert_eq!(h.items.len(), HISTORY_LEN);
        assert_eq!(h.items[0].key, 99);
    }

    #[test]
    fn picture_survives_png_and_premultiplying_does_not_make_it_new() {
        let mut rgba: Vec<u8> = (0..40 * 30).flat_map(|i| [i as u8, (i / 3) as u8, 200, 255]).collect();
        rgba[3] = 128;
        let png = encode_png(40, 30, &rgba).unwrap();
        let back = image::load_from_memory(&png).unwrap().into_rgba8().into_raw();
        assert_eq!(back, rgba);
        let mut premultiplied = rgba.clone();
        premultiplied[0] /= 2; // the translucent pixel after another clipboard's round trip
        assert_eq!(image_hash(40, 30, &premultiplied), image_hash(40, 30, &rgba));
        let mut edited = rgba.clone();
        edited[7 * 4] ^= 0xFF;
        assert_ne!(image_hash(40, 30, &edited), image_hash(40, 30, &rgba));
    }
}
