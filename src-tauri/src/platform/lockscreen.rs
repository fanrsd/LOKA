//! Typing on the Windows lock screen. Its curtain (LockApp) is still on the Default desktop and takes plain
//! `SendInput`; the password screen behind it is on `winsta0\winlogon`, which only SYSTEM may send input to. So a
//! LocalSystem service (`loka --secure-service`, installed from About under Program Files: a SYSTEM service must never
//! run an exe the user can overwrite) starts `loka --secure-helper` as SYSTEM on that desktop in the active console
//! session. The helper reads `INPUT`s from a named pipe and `SendInput`s them, only while that session is locked (an
//! unlocked session's UAC prompt stays out of reach, so no user process can approve one through it).
//! While the winlogon desktop has the input, the app's `SendInput` calls go to the pipe instead (`windows.rs::send`).
use std::{
    fs::File,
    io::{Read, Write},
    mem::{size_of, zeroed},
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use windows_sys::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        Security::{
            Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT},
            DuplicateTokenEx, IsWellKnownSid, SecurityIdentification, SetTokenInformation, TokenPrimary, TokenSessionId, WinLocalSystemSid,
            OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
        },
        Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND},
        System::{
            Pipes::{ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientSessionId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT},
            RemoteDesktop::{ProcessIdToSessionId, WTSFreeMemory, WTSGetActiveConsoleSessionId, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW},
            Services::{
                RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
                SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
                SERVICE_WIN32_OWN_PROCESS,
            },
            Threading::{
                CreateProcessAsUserW, GetCurrentProcess, GetCurrentProcessId, OpenProcessToken, TerminateProcess, WaitForSingleObject,
                CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
        UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE},
    },
};

pub const SERVICE: &str = "LokaSecureInput";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Where About installs the service's exe (writable by administrators only).
pub fn installed_exe() -> PathBuf {
    PathBuf::from(std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into())).join("LOKA Secure Input").join("loka.exe")
}

fn own_session() -> u32 {
    let mut s = 0;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut s) };
    s
}

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\loka-secure-{session}")
}

fn locked(session: u32) -> bool {
    unsafe {
        let (mut buf, mut len) = (null_mut(), 0);
        if WTSQuerySessionInformationW(null_mut(), session, WTSSessionInfoEx, &mut buf, &mut len) == 0 {
            return false;
        }
        let info = &*(buf as *const WTSINFOEXW);
        let locked = info.Level == 1 && info.Data.WTSInfoExLevel1.SessionFlags == 0; // WTS_SESSIONSTATE_LOCK
        WTSFreeMemory(buf.cast());
        locked
    }
}

// ---- the app (user session) ----

static PIPE: Mutex<Option<File>> = Mutex::new(None);

/// The pipe, if the helper serves this session. Its owner must be SYSTEM: a user process could create the
/// name first and read what's typed (the password).
fn connect(pipe: &mut Option<File>) -> Option<&mut File> {
    if pipe.is_none() {
        let f = std::fs::OpenOptions::new().write(true).open(pipe_name(own_session())).ok()?;
        let system = unsafe {
            let (mut owner, mut sd): (PSID, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
            let ok = GetSecurityInfo(f.as_raw_handle() as HANDLE, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION, &mut owner, null_mut(), null_mut(), null_mut(), &mut sd) == 0
                && IsWellKnownSid(owner, WinLocalSystemSid) != 0;
            LocalFree(sd);
            ok
        };
        if !system {
            journal!("lock screen: pipe {} not owned by SYSTEM, ignored", pipe_name(own_session()));
            return None;
        }
        *pipe = Some(f);
    }
    pipe.as_mut()
}

/// This session is locked and the helper takes input for it.
pub fn available() -> bool {
    locked(own_session()) && connect(&mut PIPE.lock().unwrap()).is_some()
}

/// Hands one input to the helper; false when it isn't there.
pub fn send(input: &INPUT) -> bool {
    let mut pipe = PIPE.lock().unwrap();
    let Some(f) = connect(&mut pipe) else { return false };
    let bytes = unsafe { std::slice::from_raw_parts((input as *const INPUT).cast::<u8>(), size_of::<INPUT>()) };
    if f.write_all(bytes).is_ok() {
        return true;
    }
    *pipe = None;
    false
}

// ---- the helper (SYSTEM, winlogon desktop of the user's session) ----

pub fn helper() {
    let session = own_session();
    let name = wide(&pipe_name(session));
    // owner SYSTEM (checked by the app), SYSTEM full, interactive users read+write (opening for write alone is refused:
    // std's open also asks for attributes; an inbound pipe gives the client nothing to read); labelled medium, since an
    // object made by SYSTEM is System integrity by default and the app couldn't write up to it
    let sddl = wide("O:SYD:P(A;;GA;;;SY)(A;;GRGW;;;IU)S:(ML;;NW;;;ME)");
    let (mut lock, mut checked) = (false, Instant::now() - Duration::from_secs(1));
    loop {
        let pipe = unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut());
            let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
            let pipe = CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                0,
                4096,
                0,
                &sa,
            );
            LocalFree(sd);
            pipe
        };
        if pipe == INVALID_HANDLE_VALUE {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let mut client = 0;
        let ok = unsafe { (ConnectNamedPipe(pipe, null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED) && GetNamedPipeClientSessionId(pipe, &mut client) != 0 };
        let mut f = unsafe { File::from_raw_handle(pipe as _) };
        if !ok || client != session {
            continue;
        }
        let mut buf = [0u8; size_of::<INPUT>()];
        while f.read_exact(&mut buf).is_ok() {
            let input: INPUT = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
            if checked.elapsed() > Duration::from_millis(250) {
                (lock, checked) = (locked(session), Instant::now());
            }
            if lock && (input.r#type == INPUT_MOUSE || input.r#type == INPUT_KEYBOARD) {
                unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
            }
        }
    }
}

// ---- the service (LocalSystem, session 0) ----

static STOP: AtomicBool = AtomicBool::new(false);

pub fn service() {
    let mut name = wide(SERVICE);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: null_mut(), lpServiceProc: None },
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
}

unsafe extern "system" fn control(ctrl: u32, _: u32, _: *mut core::ffi::c_void, _: *mut core::ffi::c_void) -> u32 {
    if ctrl == SERVICE_CONTROL_STOP || ctrl == SERVICE_CONTROL_SHUTDOWN {
        STOP.store(true, Ordering::SeqCst);
    }
    0
}

unsafe extern "system" fn service_main(_: u32, _: *mut PWSTR) {
    let name = wide(SERVICE);
    let handle = RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), null());
    let report = |state| {
        let mut st: SERVICE_STATUS = zeroed();
        st.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
        st.dwCurrentState = state;
        st.dwControlsAccepted = if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 };
        SetServiceStatus(handle, &st);
    };
    report(SERVICE_RUNNING);
    // one helper, in the session at the physical console (fast user switching moves it)
    let mut helper: Option<(u32, HANDLE)> = None;
    let kill = |p: HANDLE| {
        TerminateProcess(p, 0);
        WaitForSingleObject(p, 2000); // its exe stays locked until it's gone (About replaces it)
        CloseHandle(p);
    };
    while !STOP.load(Ordering::SeqCst) {
        let session = WTSGetActiveConsoleSessionId();
        if let Some((s, p)) = helper {
            if s != session || WaitForSingleObject(p, 0) == WAIT_OBJECT_0 {
                kill(p);
                helper = None;
            }
        }
        if helper.is_none() && session != u32::MAX {
            helper = launch(session).map(|p| (session, p));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    if let Some((_, p)) = helper {
        kill(p);
    }
    report(SERVICE_STOPPED);
}

/// `loka --secure-helper` as SYSTEM (our own token moved to `session`) on that session's winlogon desktop.
unsafe fn launch(session: u32) -> Option<HANDLE> {
    let exe = std::env::current_exe().ok()?;
    let (mut token, mut dup) = (null_mut(), null_mut());
    if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut token) == 0 {
        return None;
    }
    let ok = DuplicateTokenEx(token, TOKEN_ALL_ACCESS, null(), SecurityIdentification, TokenPrimary, &mut dup) != 0;
    CloseHandle(token);
    if !ok {
        return None;
    }
    let mut cmd = wide(&format!("\"{}\" --secure-helper", exe.display()));
    let mut desktop = wide(r"winsta0\winlogon");
    let mut si: STARTUPINFOW = zeroed();
    si.cb = size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desktop.as_mut_ptr();
    let mut pi: PROCESS_INFORMATION = zeroed();
    let ok = SetTokenInformation(dup, TokenSessionId, (&session as *const u32).cast(), 4) != 0
        && CreateProcessAsUserW(dup, null(), cmd.as_mut_ptr(), null(), null(), 0, CREATE_NO_WINDOW, null(), null(), &si, &mut pi) != 0;
    CloseHandle(dup);
    if !ok {
        return None;
    }
    CloseHandle(pi.hThread);
    Some(pi.hProcess)
}
