use super::{Captured, CursorSample, Injected, InputEvent, SpatialPlatformEngine};
use std::{
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex,
    },
};
use tauri::WebviewWindow;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
            MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
            VK_CONTROL, VK_ESCAPE, VK_LBUTTON, VK_MENU, VK_SCROLL,
        },
        WindowsAndMessaging::{
            CallNextHookEx, CreateCursor, GetCursorPos, GetMessageW, GetSystemMetrics, GetWindowLongPtrW, PeekMessageW,
            PostThreadMessageW, SetCursorPos, SetSystemCursor, SetTimer, SetWindowLongPtrW, SetWindowsHookExW, SystemParametersInfoW,
            UnhookWindowsHookEx, GWL_EXSTYLE, SPI_SETCURSORS,
            HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, SM_CXVIRTUALSCREEN,
            SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
            WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
            WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_TIMER, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP,
            WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        },
    },
};

/// Marks events we inject so our own hooks let them through.
const TAG: usize = 0x4C4F_4B41; // "LOKA"

struct Capture {
    out: UnboundedSender<Captured>,
    park: POINT,
    ctrl: bool,
    alt: bool,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

pub struct WindowsEngine;

impl SpatialPlatformEngine for WindowsEngine {
    fn prepare_overlay(&self, window: &WebviewWindow) -> tauri::Result<()> {
        show_cursors(); // a crash while driving another computer would have left them blank
        self.set_click_through(window, true)
    }

    fn set_click_through(&self, window: &WebviewWindow, ignore: bool) -> tauri::Result<()> {
        // tao owns LAYERED/TRANSPARENT and rewrites the whole ex-style on every flag change
        // (show, ignore-cursor), so our bits are re-applied after it, not once.
        window.set_ignore_cursor_events(ignore)?;
        let hwnd = window.hwnd()?.0 as HWND;
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ((ex & !WS_EX_APPWINDOW) | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize);
        }
        Ok(())
    }

    fn cursor(&self) -> Option<CursorSample> {
        let mut p = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut p) } == 0 {
            return None; // e.g. secure desktop / UAC prompt
        }
        // high bit = key currently down; reported even while OLE drag owns the mouse
        let button_down = unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } < 0;
        Some(CursorSample { x: p.x as f64, y: p.y as f64, button_down })
    }

    fn start_capture(&self, park: (f64, f64), out: UnboundedSender<Captured>) -> Result<(), String> {
        let park = POINT { x: park.0 as i32, y: park.1 as i32 };
        *CAPTURE.lock().unwrap() = Some(Capture { out, park, ctrl: false, alt: false });
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        // LL hooks are delivered through the installing thread's message loop.
        std::thread::spawn(move || unsafe {
            let mut msg: MSG = zeroed();
            PeekMessageW(&mut msg, null_mut(), WM_USER, WM_USER, PM_NOREMOVE); // create the queue before publishing
            // park first: once our LL hook is in, it would swallow this move like any other
            SetCursorPos(park.x, park.y);
            let module = GetModuleHandleW(null());
            let mut mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
            let mut keys = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), module, 0);
            if mouse.is_null() || keys.is_null() {
                for h in [mouse, keys] {
                    if !h.is_null() {
                        UnhookWindowsHookEx(h);
                    }
                }
                let _ = ready_tx.send(false);
                return;
            }
            HOOK_THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
            hide_cursors(); // parked in the middle of this screen, it would only distract
            let _ = ready_tx.send(true);
            // Another app's LL hook installed after ours runs first and may not pass input on (seen with
            // input utilities running), so keys and moves leak to this machine. Watch for leaks and
            // move our hooks back to the front; Ctrl+Alt+Esc also works from what leaked.
            let mut was_down = keys_down();
            SetTimer(null_mut(), 0, 15, None);
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                if msg.message != WM_TIMER {
                    continue;
                }
                let down = keys_down();
                if (0..256).any(|vk| down[vk] && !was_down[vk]) {
                    UnhookWindowsHookEx(keys);
                    keys = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), module, 0);
                    if let Some(c) = CAPTURE.lock().unwrap().as_ref() {
                        let held = |vk: u16| down[vk as usize];
                        if held(VK_ESCAPE) && (c.ctrl || held(VK_CONTROL)) && (c.alt || held(VK_MENU)) {
                            let _ = c.out.send(Captured::Escape);
                        }
                    }
                }
                was_down = down;
                let mut p = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut p) != 0 && (p.x != park.x || p.y != park.y) {
                    SetCursorPos(park.x, park.y);
                    UnhookWindowsHookEx(mouse);
                    mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0);
                }
            }
            UnhookWindowsHookEx(mouse);
            UnhookWindowsHookEx(keys);
        });
        if ready_rx.recv() == Ok(true) {
            Ok(())
        } else {
            *CAPTURE.lock().unwrap() = None;
            Err("SetWindowsHookExW failed".into())
        }
    }

    fn stop_capture(&self, warp_to: (f64, f64)) {
        *CAPTURE.lock().unwrap() = None; // hooks pass everything through from here on
        let thread = HOOK_THREAD.swap(0, Ordering::SeqCst);
        unsafe {
            if thread != 0 {
                PostThreadMessageW(thread, WM_QUIT, 0, 0);
            }
            // a key whose press leaked here but whose release was captured would stay stuck
            let down = keys_down();
            for vk in (0..256u16).filter(|&vk| down[vk as usize]) {
                let mut input: INPUT = zeroed();
                input.r#type = INPUT_KEYBOARD;
                input.Anonymous.ki = KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: KEYEVENTF_KEYUP, time: 0, dwExtraInfo: TAG };
                SendInput(1, &input, size_of::<INPUT>() as i32);
            }
            SetCursorPos(warp_to.0 as i32, warp_to.1 as i32);
            show_cursors();
        }
    }

    fn inject(&self, event: Injected) {
        let mut input: INPUT = unsafe { zeroed() };
        unsafe {
            match event {
                Injected::Key { scan, down } => {
                    input.r#type = INPUT_KEYBOARD;
                    let ki = &mut input.Anonymous.ki;
                    ki.wScan = scan & 0xFF;
                    ki.dwFlags = KEYEVENTF_SCANCODE
                        | if scan & 0xFF00 == 0xE000 { KEYEVENTF_EXTENDEDKEY } else { 0 }
                        | if down { 0 } else { KEYEVENTF_KEYUP };
                    ki.dwExtraInfo = TAG;
                }
                _ => {
                    input.r#type = INPUT_MOUSE;
                    let mi = &mut input.Anonymous.mi;
                    mi.dwExtraInfo = TAG;
                    match event {
                        Injected::MoveTo { x, y } => {
                            let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN) as f64, GetSystemMetrics(SM_YVIRTUALSCREEN) as f64);
                            let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64, GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64);
                            mi.dx = ((x - vx) * 65535.0 / (vw - 1.0)).round() as i32;
                            mi.dy = ((y - vy) * 65535.0 / (vh - 1.0)).round() as i32;
                            mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
                        }
                        Injected::Button { button, down } => {
                            let (flags, data) = match (button, down) {
                                (1, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                                (1, false) => (MOUSEEVENTF_LEFTUP, 0),
                                (2, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                                (2, false) => (MOUSEEVENTF_RIGHTUP, 0),
                                (3, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                                (3, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                                (4 | 5, true) => (MOUSEEVENTF_XDOWN, button as u32 - 3),
                                (4 | 5, false) => (MOUSEEVENTF_XUP, button as u32 - 3),
                                _ => return,
                            };
                            mi.dwFlags = flags;
                            mi.mouseData = data as _;
                        }
                        Injected::Wheel { dx, dy } => {
                            // one axis per INPUT
                            if dx != 0 {
                                mi.dwFlags = MOUSEEVENTF_HWHEEL;
                                mi.mouseData = dx as _;
                                SendInput(1, &input, size_of::<INPUT>() as i32);
                            }
                            if dy == 0 {
                                return;
                            }
                            let mi = &mut input.Anonymous.mi;
                            mi.dwFlags = MOUSEEVENTF_WHEEL;
                            mi.mouseData = dy as _;
                        }
                        Injected::Key { .. } => unreachable!(),
                    }
                }
            }
            SendInput(1, &input, size_of::<INPUT>() as i32);
        }
    }

    fn lock_key_down(&self) -> bool {
        unsafe { GetAsyncKeyState(VK_SCROLL as i32) < 0 }
    }
}

/// Every system cursor (OCR_NORMAL … OCR_PERSON) swapped for a blank one.
fn hide_cursors() {
    const OCR: [u32; 16] = [32512, 32513, 32514, 32515, 32516, 32642, 32643, 32644, 32645, 32646, 32648, 32649, 32650, 32651, 32671, 32672];
    let (and, xor) = ([0xFFu8; 128], [0u8; 128]); // 32×32: AND 1 + XOR 0 = screen untouched
    unsafe {
        let module = GetModuleHandleW(null());
        for id in OCR {
            let blank = CreateCursor(module, 0, 0, 32, 32, and.as_ptr().cast(), xor.as_ptr().cast());
            if !blank.is_null() {
                SetSystemCursor(blank, id); // takes ownership of `blank`
            }
        }
    }
}

/// The user's own cursors, reloaded from their settings.
fn show_cursors() {
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, null_mut(), 0);
    }
}

/// Password managers (KeePass, 1Password, Bitwarden…) tag secrets with these formats.
pub fn clipboard_concealed() -> bool {
    use windows_sys::Win32::System::DataExchange::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
    ["ExcludeClipboardContentFromMonitorProcessing", "Clipboard Viewer Ignore"].iter().any(|name| {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        unsafe { IsClipboardFormatAvailable(RegisterClipboardFormatW(wide.as_ptr())) != 0 }
    })
}

fn hiword(v: u32) -> i32 {
    (v >> 16) as u16 as i16 as i32
}

unsafe extern "system" fn mouse_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(l as *const MSLLHOOKSTRUCT);
        if info.dwExtraInfo != TAG {
            if let Some(c) = CAPTURE.lock().unwrap().as_mut() {
                let data = info.mouseData as u32;
                let event = match w as u32 {
                    WM_MOUSEMOVE => {
                        let (dx, dy) = (info.pt.x - c.park.x, info.pt.y - c.park.y);
                        (dx != 0 || dy != 0).then_some(InputEvent::Move { dx, dy })
                    }
                    WM_LBUTTONDOWN => Some(InputEvent::Button { button: 1, down: true }),
                    WM_LBUTTONUP => Some(InputEvent::Button { button: 1, down: false }),
                    WM_RBUTTONDOWN => Some(InputEvent::Button { button: 2, down: true }),
                    WM_RBUTTONUP => Some(InputEvent::Button { button: 2, down: false }),
                    WM_MBUTTONDOWN => Some(InputEvent::Button { button: 3, down: true }),
                    WM_MBUTTONUP => Some(InputEvent::Button { button: 3, down: false }),
                    WM_XBUTTONDOWN => Some(InputEvent::Button { button: 3 + hiword(data) as u8, down: true }),
                    WM_XBUTTONUP => Some(InputEvent::Button { button: 3 + hiword(data) as u8, down: false }),
                    WM_MOUSEWHEEL => Some(InputEvent::Wheel { dx: 0, dy: hiword(data) }),
                    WM_MOUSEHWHEEL => Some(InputEvent::Wheel { dx: hiword(data), dy: 0 }),
                    _ => None,
                };
                if let Some(e) = event {
                    let _ = c.out.send(Captured::Input(e));
                }
                return 1; // swallow: the cursor stays parked, local apps see nothing
            }
        }
    }
    CallNextHookEx(null_mut(), code, w, l)
}

/// Keys this machine currently sees as held (mouse buttons, VK 1-6, excluded).
fn keys_down() -> [bool; 256] {
    let mut down = [false; 256];
    for vk in 8..256 {
        down[vk] = unsafe { GetAsyncKeyState(vk as i32) } < 0;
    }
    down
}

unsafe extern "system" fn key_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(l as *const KBDLLHOOKSTRUCT);
        if info.dwExtraInfo != TAG {
            if let Some(c) = CAPTURE.lock().unwrap().as_mut() {
                let down = matches!(w as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                let scan = (info.scanCode as u16 & 0xFF) | if info.flags & LLKHF_EXTENDED != 0 { 0xE000 } else { 0 };
                match scan {
                    0x1D | 0xE01D => c.ctrl = down,
                    0x38 | 0xE038 => c.alt = down,
                    _ => {}
                }
                let msg = if down && scan == 0x01 && c.ctrl && c.alt {
                    Captured::Escape
                } else {
                    Captured::Input(InputEvent::Key { scan, down })
                };
                let _ = c.out.send(msg);
                return 1;
            }
        }
    }
    CallNextHookEx(null_mut(), code, w, l)
}
