//! Android: received input goes to `Injector.kt`, which LOKA starts as the shell user over its own
//! Wireless debugging link (apps can't inject input). It dials back to a localhost port and proves itself
//! with a one-time token; frames are 9 bytes: tag, i32, i32 (LE). Driving a computer = the touchpad screen.

use super::{CursorSample, Injected, SpatialPlatformEngine};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use tauri::WebviewWindow;
use tokio::sync::mpsc::UnboundedSender;

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
    injector: Arc<Mutex<Option<TcpStream>>>,
    /// latest cursor spot and whether the overlay still has to hear it
    cursor: Arc<Mutex<(Option<(f64, f64)>, bool)>>,
}

impl AndroidEngine {
    pub fn new(overlay: CursorOverlay) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("localhost port");
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let token = uuid::Uuid::new_v4().simple().to_string();
        let injector: Arc<Mutex<Option<TcpStream>>> = Arc::default();
        let (slot, want) = (injector.clone(), token.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                // any app may connect to localhost: only the injector we started knows the token
                let mut line = String::new();
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                if BufReader::new(&conn).read_line(&mut line).is_ok() && line.trim_end() == want {
                    let _ = conn.set_nodelay(true);
                    *slot.lock().unwrap() = Some(conn);
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
        Self { injector, cursor }
    }

    fn send(&self, tag: u8, a: i32, b: i32) {
        let mut slot = self.injector.lock().unwrap();
        let Some(s) = slot.as_mut() else { return };
        let mut f = [0u8; 9];
        f[0] = tag;
        f[1..5].copy_from_slice(&a.to_le_bytes());
        f[5..9].copy_from_slice(&b.to_le_bytes());
        if s.write_all(&f).is_err() {
            *slot = None; // injector died (Wireless debugging off, phone restarted): can_inject goes false
        }
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
        match e {
            Injected::MoveTo { x, y } => {
                self.send(1, x as i32, y as i32);
                self.overlay(Some((x, y)));
            }
            Injected::Button { button, down } => self.send(2, button as i32, down as i32),
            Injected::Wheel { dx, dy } => self.send(3, dx, dy),
            Injected::Key { scan, down } => self.send(4, scan as i32, down as i32),
        }
    }
    fn show_cursor(&self, on: bool) {
        if on {
            self.send(5, 0, 0); // a computer moved in: wake the screen
        } else {
            self.overlay(None);
        }
    }
    fn can_inject(&self) -> bool {
        self.injector.lock().unwrap().is_some()
    }
}
