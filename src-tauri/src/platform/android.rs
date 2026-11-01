//! Android: received input goes to `Injector.kt`, which LOKA starts as the shell user over its own
//! Wireless debugging link (apps can't inject input). It dials back to a localhost port and proves itself
//! with a one-time token; frames are 9 bytes: tag, i32, i32 (LE); text frames (6 type, 7 set clipboard)
//! carry `len` (the first i32) bytes of UTF-8 after them, and 8 (get clipboard) is answered with u32 LE
//! `len` + UTF-8. Driving a computer = the touchpad screen.

use super::{CursorSample, Injected, SpatialPlatformEngine};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::WebviewWindow;
use tokio::sync::mpsc::UnboundedSender;

/// The injector's connection, once it dialed back (the clipboard functions need it outside the engine).
static INJECTOR: Mutex<Option<TcpStream>> = Mutex::new(None);

/// One frame to the injector, `extra` bytes after it; a failed write means it died (Wireless debugging off,
/// phone restarted): `can_inject` goes false.
fn send(tag: u8, a: i32, b: i32, extra: &[u8]) -> bool {
    let mut slot = INJECTOR.lock().unwrap();
    let Some(s) = slot.as_mut() else { return false };
    let mut f = vec![tag];
    f.extend_from_slice(&a.to_le_bytes());
    f.extend_from_slice(&b.to_le_bytes());
    f.extend_from_slice(extra);
    let ok = s.write_all(&f).is_ok();
    if !ok {
        *slot = None;
    }
    ok
}

/// Clipboard text: through the injector while it runs (the shell user may read it in the background, an
/// app only while in front), else as this app (LOKA in front: the phone driving a computer).
pub fn clipboard_text() -> Option<String> {
    let mut slot = INJECTOR.lock().unwrap();
    let Some(s) = slot.as_mut() else {
        drop(slot);
        return jni_clipboard(None).flatten();
    };
    let ask = |s: &mut TcpStream| -> std::io::Result<String> {
        // a late answer to an earlier ask that timed out must not pass for this one
        s.set_nonblocking(true)?;
        while s.read(&mut [0; 256]).is_ok_and(|n| n > 0) {}
        s.set_nonblocking(false)?;
        s.set_read_timeout(Some(Duration::from_millis(500)))?;
        let mut f = [0u8; 9];
        f[0] = 8;
        s.write_all(&f)?;
        let mut len = [0u8; 4];
        s.read_exact(&mut len)?;
        let mut text = vec![0; u32::from_le_bytes(len).min(1 << 20) as usize];
        s.read_exact(&mut text)?;
        Ok(String::from_utf8_lossy(&text).into_owned())
    };
    ask(s).inspect_err(|e| eprintln!("injector clipboard: {e}")).ok()
}

pub fn set_clipboard_text(text: &str) {
    if !send(7, text.len() as i32, 0, text.as_bytes()) {
        jni_clipboard(Some(text));
    }
}

/// `ClipboardManager` as this app: sets `text` if given, else reads it (`Some(None)`: empty).
fn jni_clipboard(text: Option<&str>) -> Option<Option<String>> {
    use jni::objects::{JObject, JString, JValue};
    let ctx = ndk_context::android_context();
    // SAFETY: as in `device_name`
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let r = (|| -> jni::errors::Result<Option<String>> {
        let name = env.new_string("clipboard")?;
        let cm = env.call_method(&context, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[(&name).into()])?.l()?;
        if let Some(text) = text {
            let (label, text) = (env.new_string("LOKA")?, env.new_string(text)?);
            let sig = "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;";
            let clip = env.call_static_method("android/content/ClipData", "newPlainText", sig, &[(&label).into(), (&text).into()])?.l()?;
            env.call_method(&cm, "setPrimaryClip", "(Landroid/content/ClipData;)V", &[(&clip).into()])?;
            return Ok(None);
        }
        let clip = env.call_method(&cm, "getPrimaryClip", "()Landroid/content/ClipData;", &[])?.l()?;
        if clip.is_null() || env.call_method(&clip, "getItemCount", "()I", &[])?.i()? == 0 {
            return Ok(None);
        }
        let item = env.call_method(&clip, "getItemAt", "(I)Landroid/content/ClipData$Item;", &[JValue::Int(0)])?.l()?;
        let seq = env.call_method(&item, "coerceToText", "(Landroid/content/Context;)Ljava/lang/CharSequence;", &[(&context).into()])?.l()?;
        if seq.is_null() {
            return Ok(None);
        }
        let s = env.call_method(&seq, "toString", "()Ljava/lang/String;", &[])?.l()?;
        Ok(Some(env.get_string(&JString::from(s))?.into()))
    })();
    if r.is_err() {
        let _ = env.exception_clear(); // a pending Java exception would poison the next JNI call
    }
    r.ok()
}

/// Draws (`Some`) or hides (`None`) the computer's cursor over every app (LokaPlugin `cursor`).
pub type CursorOverlay = Box<dyn Fn(Option<(f64, f64)>) + Send + Sync>;

/// Set once the engine listens (the port is only known then).
static TICKET: std::sync::OnceLock<(u16, String)> = std::sync::OnceLock::new();

/// Where `Injector.kt` must dial back, and how it proves itself.
pub fn ticket() -> Option<(u16, String)> {
    TICKET.get().cloned()
}

/// The name set in Settings › About phone › Device name (`Settings.Global.device_name`); not readable from a
/// shell as the app user, hence JNI.
pub fn device_name() -> Option<String> {
    use jni::objects::{JObject, JString};
    let ctx = ndk_context::android_context();
    // SAFETY: tao hands ndk-context a live JavaVM and the activity for as long as the app runs
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let name = (|| -> jni::errors::Result<Option<String>> {
        let resolver = env.call_method(&context, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?.l()?;
        let key = env.new_string("device_name")?;
        let sig = "(Landroid/content/ContentResolver;Ljava/lang/String;)Ljava/lang/String;";
        let v = env.call_static_method("android/provider/Settings$Global", "getString", sig, &[(&resolver).into(), (&key).into()])?.l()?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(env.get_string(&JString::from(v))?.into()))
    })();
    if name.is_err() {
        let _ = env.exception_clear(); // a pending Java exception would poison the next JNI call
    }
    name.ok().flatten().map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
}

pub struct AndroidEngine {
    /// latest cursor spot and whether the overlay still has to hear it
    cursor: Arc<Mutex<(Option<(f64, f64)>, bool)>>,
}

impl AndroidEngine {
    pub fn new(overlay: CursorOverlay) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("localhost port");
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let want = token.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                // any app may connect to localhost: only the injector we started knows the token
                let mut line = String::new();
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                if BufReader::new(&conn).read_line(&mut line).is_ok() && line.trim_end() == want {
                    let _ = conn.set_nodelay(true);
                    *INJECTOR.lock().unwrap() = Some(conn);
                }
            }
        });
        let cursor: Arc<Mutex<(Option<(f64, f64)>, bool)>> = Arc::new(Mutex::new((None, false)));
        let c = cursor.clone();
        // the overlay is a JNI round trip: at most 60 a second, latest spot only
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(16));
            let pending = {
                let mut c = c.lock().unwrap();
                std::mem::replace(&mut c.1, false).then_some(c.0)
            };
            if let Some(at) = pending {
                overlay(at);
            }
        });
        let _ = TICKET.set((port, token));
        Self { cursor }
    }

    fn overlay(&self, at: Option<(f64, f64)>) {
        *self.cursor.lock().unwrap() = (at, true);
    }
}

impl SpatialPlatformEngine for AndroidEngine {
    fn prepare_overlay(&self, _: &WebviewWindow) -> tauri::Result<()> {
        Ok(())
    }
    fn cursor(&self) -> Option<CursorSample> {
        None
    }
    fn start_capture(&self, park: (f64, f64), out: UnboundedSender<super::Captured>) -> Result<(), String> {
        super::TouchEngine.start_capture(park, out)
    }
    fn stop_capture(&self, warp_to: (f64, f64)) {
        super::TouchEngine.stop_capture(warp_to)
    }
    fn inject(&self, e: Injected) {
        let _ = match e {
            Injected::MoveTo { x, y } => {
                self.overlay(Some((x, y)));
                send(1, x as i32, y as i32, &[])
            }
            Injected::Button { button, down } => send(2, button as i32, down as i32, &[]),
            Injected::Wheel { dx, dy } => send(3, dx, dy, &[]),
            Injected::Key { scan, down } => send(4, scan as i32, down as i32, &[]),
            Injected::Text { text } => send(6, text.len() as i32, 0, text.as_bytes()),
        };
    }
    fn show_cursor(&self, on: bool) {
        if on {
            send(5, 0, 0, &[]); // a computer moved in: wake the screen
        } else {
            self.overlay(None);
        }
    }
    fn can_inject(&self) -> bool {
        send(0, 0, 0, &[]) // a dead injector only shows on a write; Injector.kt ignores tag 0
    }
}
