use super::{Captured, CursorSample, Injected, InputEvent, SpatialPlatformEngine};
use objc2_app_kit::{NSMainMenuWindowLevel, NSWindow, NSWindowCollectionBehavior};
use std::{
    ffi::c_void,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicPtr, Ordering},
        Mutex,
    },
};
use tauri::WebviewWindow;
use tokio::sync::mpsc::UnboundedSender;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

type Ref = *mut c_void;
type TapCallback = unsafe extern "C" fn(proxy: Ref, ty: u32, event: Ref, info: Ref) -> Ref;

const COMBINED_SESSION_STATE: i32 = 0; // kCGEventSourceStateCombinedSessionState
const MOUSE_BUTTON_LEFT: u32 = 0; // kCGMouseButtonLeft

// CGEventType
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const MOVED: u32 = 5;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const KEY_DOWN: u32 = 10;
const KEY_UP: u32 = 11;
const FLAGS_CHANGED: u32 = 12;
const SCROLL: u32 = 22;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const TAP_DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
const TAP_DISABLED_USER: u32 = 0xFFFF_FFFF;

// CGEventField
const BUTTON_NUMBER: u32 = 3;
const DELTA_X: u32 = 4;
const DELTA_Y: u32 = 5;
const KEYCODE: u32 = 9;
const SCROLL_AXIS1: u32 = 11;
const SCROLL_AXIS2: u32 = 12;

// CGEventFlags
const FLAG_CAPS: u64 = 0x1_0000;
const FLAG_SHIFT: u64 = 0x2_0000;
const FLAG_CONTROL: u64 = 0x4_0000;
const FLAG_ALT: u64 = 0x8_0000;
const FLAG_COMMAND: u64 = 0x10_0000;

const SESSION_TAP: u32 = 1; // kCGSessionEventTap
const HID_TAP: u32 = 0; // kCGHIDEventTap
const HEAD_INSERT: u32 = 0;
const TAP_DEFAULT: u32 = 0; // active tap: may swallow events
const SCROLL_UNIT_LINE: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const c_void) -> Ref;
    fn CGEventGetLocation(event: Ref) -> CGPoint;
    fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
    fn CGGetDisplaysWithPoint(point: CGPoint, max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> Ref;
    fn CGDisplayModeGetPixelWidth(mode: Ref) -> usize;
    fn CGDisplayModeRelease(mode: Ref);
    fn CGMainDisplayID() -> u32;
    fn CGEventTapCreate(tap: u32, place: u32, options: u32, mask: u64, callback: TapCallback, info: Ref) -> Ref;
    fn CGEventTapEnable(tap: Ref, enable: bool);
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventGetFlags(event: Ref) -> u64;
    fn CGEventSetFlags(event: Ref, flags: u64);
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    fn CGWarpMouseCursorPosition(p: CGPoint) -> i32;
    fn CGEventCreateMouseEvent(source: *const c_void, ty: u32, p: CGPoint, button: u32) -> Ref;
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: u16, down: bool) -> Ref;
    fn CGEventCreateScrollWheelEvent(source: *const c_void, units: u32, count: u32, wheel1: i32, ...) -> Ref;
    fn CGEventPost(tap: u32, event: Ref);
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    // private, as Synergy/Barrier use them: hiding the cursor otherwise needs LOKA to be the active app
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(cid: i32, target: i32, key: *const c_void, value: *const c_void) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const std::ffi::c_char, encoding: u32) -> Ref;
    fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: Ref, order: isize) -> Ref;
    fn CFRunLoopGetCurrent() -> Ref;
    fn CFRunLoopAddSource(rl: Ref, source: Ref, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: Ref);
    static kCFRunLoopCommonModes: *const c_void;
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFDictionaryCreate(alloc: *const c_void, keys: *const *const c_void, values: *const *const c_void, n: isize, key_cb: *const c_void, value_cb: *const c_void) -> Ref;
}
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: *const c_void;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

/// Main (event tap) and Client (CGEventPost, silently dropped otherwise) both need Accessibility.
/// Untrusted → macOS adds LOKA to the list and shows its "open System Settings" prompt.
fn ask_accessibility() {
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let opts = CFDictionaryCreate(null(), keys.as_ptr(), values.as_ptr(), 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        AXIsProcessTrustedWithOptions(opts);
        CFRelease(opts);
    }
}

/// (set-1 scancode, macOS virtual keycode). Positional, so layouts behave like a physical keyboard.
const KEYMAP: &[(u16, u16)] = &[
    (0x1E, 0x00), (0x1F, 0x01), (0x20, 0x02), (0x21, 0x03), (0x23, 0x04), (0x22, 0x05), (0x2C, 0x06),
    (0x2D, 0x07), (0x2E, 0x08), (0x2F, 0x09), (0x56, 0x0A), (0x30, 0x0B), (0x10, 0x0C), (0x11, 0x0D),
    (0x12, 0x0E), (0x13, 0x0F), (0x15, 0x10), (0x14, 0x11), (0x02, 0x12), (0x03, 0x13), (0x04, 0x14),
    (0x05, 0x15), (0x07, 0x16), (0x06, 0x17), (0x0D, 0x18), (0x0A, 0x19), (0x08, 0x1A), (0x0C, 0x1B),
    (0x09, 0x1C), (0x0B, 0x1D), (0x1B, 0x1E), (0x18, 0x1F), (0x16, 0x20), (0x1A, 0x21), (0x17, 0x22),
    (0x19, 0x23), (0x1C, 0x24), (0x26, 0x25), (0x24, 0x26), (0x28, 0x27), (0x25, 0x28), (0x27, 0x29),
    (0x2B, 0x2A), (0x33, 0x2B), (0x35, 0x2C), (0x31, 0x2D), (0x32, 0x2E), (0x34, 0x2F), (0x0F, 0x30),
    (0x39, 0x31), (0x29, 0x32), (0x0E, 0x33), (0x01, 0x35),
    // modifiers: Win/Cmd, Shift, Caps, Alt/Option, Ctrl
    (0xE05C, 0x36), (0xE05B, 0x37), (0x2A, 0x38), (0x3A, 0x39), (0x38, 0x3A), (0x1D, 0x3B),
    (0x36, 0x3C), (0xE038, 0x3D), (0xE01D, 0x3E),
    // F1..F12
    (0x3B, 0x7A), (0x3C, 0x78), (0x3D, 0x63), (0x3E, 0x76), (0x3F, 0x60), (0x40, 0x61), (0x41, 0x62),
    (0x42, 0x64), (0x43, 0x65), (0x44, 0x6D), (0x57, 0x67), (0x58, 0x6F),
    // navigation
    (0xE052, 0x72), (0xE047, 0x73), (0xE049, 0x74), (0xE053, 0x75), (0xE04F, 0x77), (0xE051, 0x79),
    (0xE04B, 0x7B), (0xE04D, 0x7C), (0xE050, 0x7D), (0xE048, 0x7E),
    // keypad
    (0x52, 0x52), (0x4F, 0x53), (0x50, 0x54), (0x51, 0x55), (0x4B, 0x56), (0x4C, 0x57), (0x4D, 0x58),
    (0x47, 0x59), (0x48, 0x5B), (0x49, 0x5C), (0x53, 0x41), (0x37, 0x43), (0x4E, 0x45), (0x4A, 0x4E),
    (0xE035, 0x4B), (0xE01C, 0x4C), (0x45, 0x47),
    // a Mac has no Scroll Lock: F14 is the cursor-lock key
    (super::LOCK_SCAN, 0x6B),
];

fn mac_key(scan: u16) -> Option<u16> {
    KEYMAP.iter().find(|(s, _)| *s == scan).map(|(_, m)| *m)
}
fn scan_of(mac: u16) -> Option<u16> {
    KEYMAP.iter().find(|(_, m)| *m == mac).map(|(s, _)| *s)
}
fn modifier_flag(mac: u16) -> Option<u64> {
    Some(match mac {
        0x36 | 0x37 => FLAG_COMMAND,
        0x38 | 0x3C => FLAG_SHIFT,
        0x39 => FLAG_CAPS,
        0x3A | 0x3D => FLAG_ALT,
        0x3B | 0x3E => FLAG_CONTROL,
        _ => return None,
    })
}

struct Capture {
    out: UnboundedSender<Captured>,
}
static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
static TAP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static RUN_LOOP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
/// CGDisplayHide/ShowCursor are counted; keep them balanced.
static HIDDEN: AtomicBool = AtomicBool::new(false);

fn set_cursor_hidden(hide: bool) {
    if HIDDEN.swap(hide, Ordering::SeqCst) == hide {
        return;
    }
    unsafe {
        if hide {
            let key = CFStringCreateWithCString(null(), c"SetsCursorInBackground".as_ptr(), 0x0800_0100);
            let cid = _CGSDefaultConnection();
            CGSSetConnectionProperty(cid, cid, key, kCFBooleanTrue);
            CFRelease(key);
            CGDisplayHideCursor(CGMainDisplayID());
        } else {
            CGDisplayShowCursor(CGMainDisplayID());
        }
    }
}

/// Password managers mark secrets with this (nspasteboard.org convention).
pub fn clipboard_concealed() -> bool {
    let pb = objc2_app_kit::NSPasteboard::generalPasteboard();
    pb.types().is_some_and(|types| types.iter().any(|t| t.to_string() == "org.nspasteboard.ConcealedType"))
}

/// Receiver-side state CoreGraphics does not track for synthetic events.
struct Injection {
    at: CGPoint,
    buttons: u8, // bit n = button n+1
    flags: u64,
}
static INJECTION: Mutex<Injection> = Mutex::new(Injection { at: CGPoint { x: 0.0, y: 0.0 }, buttons: 0, flags: 0 });

pub struct MacEngine;

impl SpatialPlatformEngine for MacEngine {
    fn prepare_overlay(&self, window: &WebviewWindow) -> tauri::Result<()> {
        let ns = window.ns_window()? as *const NSWindow;
        // SAFETY: called from the setup hook (main thread); tauri keeps the NSWindow alive.
        let ns = unsafe { &*ns };
        ns.setLevel(NSMainMenuWindowLevel + 1);
        ns.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        ask_accessibility(); // runs once at launch, main thread
        window.set_ignore_cursor_events(true)
    }

    fn cursor(&self) -> Option<CursorSample> {
        unsafe {
            let ev = CGEventCreate(null());
            if ev.is_null() {
                return None;
            }
            // Global points, top-left origin of the main display (already flipped, unlike NSEvent).
            let p = CGEventGetLocation(ev);
            CFRelease(ev);
            let scale = scale_at(p);
            Some(CursorSample {
                x: p.x * scale,
                y: p.y * scale,
                button_down: CGEventSourceButtonState(COMBINED_SESSION_STATE, MOUSE_BUTTON_LEFT),
            })
        }
    }

    fn start_capture(&self, park: (f64, f64), out: UnboundedSender<Captured>) -> Result<(), String> {
        *CAPTURE.lock().unwrap() = Some(Capture { out });
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || unsafe {
            let mask = [LEFT_DOWN, LEFT_UP, RIGHT_DOWN, RIGHT_UP, MOVED, LEFT_DRAGGED, RIGHT_DRAGGED, KEY_DOWN, KEY_UP,
                FLAGS_CHANGED, SCROLL, OTHER_DOWN, OTHER_UP, OTHER_DRAGGED]
                .iter()
                .fold(0u64, |m, t| m | 1 << t);
            let tap = CGEventTapCreate(SESSION_TAP, HEAD_INSERT, TAP_DEFAULT, mask, tap_callback, null_mut());
            if tap.is_null() {
                let _ = ready_tx.send(false); // no Accessibility permission
                return;
            }
            let source = CFMachPortCreateRunLoopSource(null(), tap, 0);
            let rl = CFRunLoopGetCurrent();
            CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            TAP.store(tap, Ordering::SeqCst);
            RUN_LOOP.store(rl, Ordering::SeqCst);
            let s = main_scale();
            CGWarpMouseCursorPosition(CGPoint { x: park.0 / s, y: park.1 / s });
            CGAssociateMouseAndMouseCursorPosition(0); // freeze the cursor; deltas still arrive
            set_cursor_hidden(true); // parked in the middle of this screen, it would only distract
            let _ = ready_tx.send(true);
            CFRunLoopRun();
            CGEventTapEnable(tap, false);
            CFRelease(source);
            CFRelease(tap);
        });
        if ready_rx.recv() == Ok(true) {
            Ok(())
        } else {
            *CAPTURE.lock().unwrap() = None;
            Err("CGEventTapCreate failed: grant LOKA Accessibility permission in System Settings".into())
        }
    }

    fn stop_capture(&self, warp_to: (f64, f64)) {
        *CAPTURE.lock().unwrap() = None;
        TAP.store(null_mut(), Ordering::SeqCst);
        let rl = RUN_LOOP.swap(null_mut(), Ordering::SeqCst);
        unsafe {
            if !rl.is_null() {
                CFRunLoopStop(rl);
            }
            CGAssociateMouseAndMouseCursorPosition(1);
            let s = main_scale();
            CGWarpMouseCursorPosition(CGPoint { x: warp_to.0 / s, y: warp_to.1 / s });
        }
        set_cursor_hidden(false);
    }

    fn inject(&self, event: Injected) {
        let mut st = INJECTION.lock().unwrap();
        unsafe {
            let ev = match event {
                Injected::MoveTo { x, y } => {
                    let s = main_scale();
                    st.at = CGPoint { x: x / s, y: y / s };
                    // macOS needs *Dragged* while a button is held or apps never see the drag
                    let (ty, button) = match st.buttons {
                        b if b & 1 != 0 => (LEFT_DRAGGED, 0),
                        b if b & 2 != 0 => (RIGHT_DRAGGED, 1),
                        b if b != 0 => (OTHER_DRAGGED, 2),
                        _ => (MOVED, 0),
                    };
                    CGEventCreateMouseEvent(null(), ty, st.at, button)
                }
                Injected::Button { button, down } => {
                    let bit = 1u8 << (button.saturating_sub(1).min(7));
                    if down { st.buttons |= bit } else { st.buttons &= !bit }
                    let (ty, cg_button) = match (button, down) {
                        (1, true) => (LEFT_DOWN, 0),
                        (1, false) => (LEFT_UP, 0),
                        (2, true) => (RIGHT_DOWN, 1),
                        (2, false) => (RIGHT_UP, 1),
                        (n, true) => (OTHER_DOWN, n as u32 - 1),
                        (n, false) => (OTHER_UP, n as u32 - 1),
                    };
                    // where the cursor really is: a carried drag presses or lets go on a cursor we never moved
                    let probe = CGEventCreate(null());
                    if !probe.is_null() {
                        st.at = CGEventGetLocation(probe);
                        CFRelease(probe);
                    }
                    CGEventCreateMouseEvent(null(), ty, st.at, cg_button)
                }
                Injected::Wheel { dx, dy } => {
                    // 120 = one notch = one line; never drop a partial (high-resolution) tick
                    let lines = |v: i32| if v != 0 && v / 120 == 0 { v.signum() } else { v / 120 };
                    CGEventCreateScrollWheelEvent(null(), SCROLL_UNIT_LINE, 2, lines(dy), lines(dx))
                }
                Injected::Key { scan, down } => {
                    let Some(code) = mac_key(scan) else { return };
                    if let Some(f) = modifier_flag(code) {
                        if down { st.flags |= f } else { st.flags &= !f }
                    }
                    CGEventCreateKeyboardEvent(null(), code, down)
                }
            };
            if ev.is_null() {
                return;
            }
            CGEventSetFlags(ev, st.flags);
            CGEventPost(HID_TAP, ev);
            CFRelease(ev);
        }
    }

    fn lock_key_down(&self) -> bool {
        unsafe { CGEventSourceKeyState(COMBINED_SESSION_STATE, 0x6B) } // F14
    }
}

unsafe extern "C" fn tap_callback(_proxy: Ref, ty: u32, event: Ref, _info: Ref) -> Ref {
    if ty == TAP_DISABLED_TIMEOUT || ty == TAP_DISABLED_USER {
        let tap = TAP.load(Ordering::SeqCst);
        if !tap.is_null() {
            CGEventTapEnable(tap, true); // macOS disables slow taps; keep capturing
        }
        return event;
    }
    let guard = CAPTURE.lock().unwrap();
    let Some(c) = guard.as_ref() else { return event };
    let field = |f| CGEventGetIntegerValueField(event, f);
    let flags = CGEventGetFlags(event);
    let input = match ty {
        MOVED | LEFT_DRAGGED | RIGHT_DRAGGED | OTHER_DRAGGED => {
            let (dx, dy) = (field(DELTA_X) as i32, field(DELTA_Y) as i32);
            (dx != 0 || dy != 0).then_some(InputEvent::Move { dx, dy })
        }
        LEFT_DOWN => Some(InputEvent::Button { button: 1, down: true }),
        LEFT_UP => Some(InputEvent::Button { button: 1, down: false }),
        RIGHT_DOWN => Some(InputEvent::Button { button: 2, down: true }),
        RIGHT_UP => Some(InputEvent::Button { button: 2, down: false }),
        OTHER_DOWN => Some(InputEvent::Button { button: field(BUTTON_NUMBER) as u8 + 1, down: true }),
        OTHER_UP => Some(InputEvent::Button { button: field(BUTTON_NUMBER) as u8 + 1, down: false }),
        SCROLL => Some(InputEvent::Wheel { dx: field(SCROLL_AXIS2) as i32 * 120, dy: field(SCROLL_AXIS1) as i32 * 120 }),
        KEY_DOWN | KEY_UP => {
            let code = field(KEYCODE) as u16;
            if ty == KEY_DOWN && code == 0x35 && flags & FLAG_CONTROL != 0 && flags & FLAG_ALT != 0 {
                let _ = c.out.send(Captured::Escape);
                return null_mut();
            }
            scan_of(code).map(|scan| InputEvent::Key { scan, down: ty == KEY_DOWN })
        }
        FLAGS_CHANGED => {
            let code = field(KEYCODE) as u16;
            match (scan_of(code), modifier_flag(code)) {
                (Some(scan), Some(f)) => Some(InputEvent::Key { scan, down: flags & f != 0 }),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(e) = input {
        let _ = c.out.send(Captured::Input(e));
    }
    null_mut() // swallow
}

fn main_scale() -> f64 {
    unsafe { display_scale(CGMainDisplayID()) }
}

unsafe fn display_scale(id: u32) -> f64 {
    let mode = CGDisplayCopyDisplayMode(id);
    if mode.is_null() {
        return 1.0;
    }
    let px = CGDisplayModeGetPixelWidth(mode) as f64;
    CGDisplayModeRelease(mode);
    let pts = CGDisplayBounds(id).size.width;
    if pts > 0.0 { px / pts } else { 1.0 }
}

/// Backing scale of the display under `p`; matches tao's `position = points * scale`.
unsafe fn scale_at(p: CGPoint) -> f64 {
    let (mut id, mut n) = (0u32, 0u32);
    if CGGetDisplaysWithPoint(p, 1, &mut id, &mut n) != 0 || n == 0 {
        return 1.0;
    }
    display_scale(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_is_a_bijection() {
        for (i, (s, m)) in KEYMAP.iter().enumerate() {
            assert!(KEYMAP[i + 1..].iter().all(|(s2, m2)| s2 != s && m2 != m), "duplicate {s:#x}/{m:#x}");
        }
    }
}
