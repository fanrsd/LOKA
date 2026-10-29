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

#[cfg(test)]
mod tests {
    use super::*;

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
