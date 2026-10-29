//! Notification mirroring: the owner streams the notifications other apps show on it, the viewer shows
//! them again as its own native notifications.
//!
//! Wire (owner → viewer, after the service handshake): frames of `u32 LE len | JSON`, `len ≤ MAX_FRAME`.
//! JSON is `{"note":{"app","title","body"}}` or `{"error":"…"}` (the owner can't read its notifications;
//! the viewer stops). `len == 0` is a keepalive sent every `PING`, so a viewer notices an owner that went
//! to sleep. The viewer never writes; anything from it, or EOF, ends `serve`.

use crate::{kvm::Service, network::secure::ServerStream};
use serde::{Deserialize, Serialize};
use std::{io, sync::Mutex, time::Duration};
use tauri::{async_runtime::JoinHandle, AppHandle, Emitter};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

/// Fits the capped fields even when every char needs a 6-byte JSON escape.
const MAX_FRAME: usize = 16 * 1024;
const PING: Duration = Duration::from_secs(15);
const RETRY: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Note {
    app: String,
    title: String,
    body: String,
}

impl Note {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    fn new(app: &str, title: &str, body: &str) -> Self {
        let cap = |s: &str, n| s.chars().take(n).collect();
        Note { app: cap(app, 64), title: cap(title, 256), body: cap(body, 1024) }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Msg {
    Note(Note),
    Error(String),
}

async fn send(w: &mut (impl AsyncWrite + Unpin), m: &Msg) -> io::Result<()> {
    let json = serde_json::to_vec(m)?;
    w.write_all(&(json.len() as u32).to_le_bytes()).await?;
    w.write_all(&json).await?;
    w.flush().await
}

/// Next message, `None` for a keepalive.
async fn recv(r: &mut (impl AsyncRead + Unpin)) -> io::Result<Option<Msg>> {
    let len = r.read_u32_le().await? as usize;
    if len == 0 {
        return Ok(None);
    }
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "notification frame too large"));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

/// Owner side: streams this computer's new notifications until the viewer hangs up.
pub async fn serve(s: ServerStream) -> io::Result<()> {
    let (mut r, mut w) = tokio::io::split(s);
    let (tx, mut rx) = mpsc::channel(32);
    let watcher = tokio::task::spawn_blocking(move || watch(tx));
    let mut ping = tokio::time::interval(PING);
    let mut byte = [0u8; 1];
    loop {
        tokio::select! {
            note = rx.recv() => match note {
                Some(n) => send(&mut w, &Msg::Note(n)).await?,
                None => {
                    let e = match watcher.await {
                        Ok(Err(e)) => e,
                        _ => io::Error::other("Notification watcher stopped"),
                    };
                    let _ = send(&mut w, &Msg::Error(e.to_string())).await;
                    return Err(e);
                }
            },
            _ = ping.tick() => {
                w.write_all(&0u32.to_le_bytes()).await?;
                w.flush().await?;
            }
            _ = r.read(&mut byte) => return Ok(()),
        }
    }
}

/// Blocking: sends every notification that appears from now on until `tx` closes. Err = can't read them.
#[cfg(target_os = "windows")]
fn watch(tx: mpsc::Sender<Note>) -> io::Result<()> {
    use std::collections::HashSet;
    use windows::UI::Notifications::{
        KnownNotificationBindings,
        Management::{UserNotificationListener, UserNotificationListenerAccessStatus as Access},
        NotificationKinds, UserNotification,
    };
    let denied = || io::Error::other("Allow notification access for LOKA in Settings › Privacy & security › Notifications");
    let listener = UserNotificationListener::Current().map_err(|_| io::Error::other("Reading notifications needs a newer Windows"))?;
    let mut status = listener.GetAccessStatus().map_err(io::Error::other)?;
    if status == Access::Unspecified {
        status = listener.RequestAccessAsync().and_then(|op| op.join()).map_err(|_| denied())?;
    }
    if status != Access::Allowed {
        return Err(denied());
    }
    let generic = KnownNotificationBindings::ToastGeneric().map_err(io::Error::other)?;
    let read = |n: &UserNotification| -> Option<Note> {
        let info = n.AppInfo().ok();
        let aumid = info.as_ref().and_then(|i| i.AppUserModelId().ok()).map(|s| s.to_string()).unwrap_or_default();
        // our own (and, in dev builds, the PowerShell id tauri-plugin-notification falls back to): mirroring
        // both ways would bounce them forever
        if aumid.eq_ignore_ascii_case("dev.loka.app") || aumid.to_ascii_lowercase().ends_with("\\powershell.exe") {
            return None;
        }
        let app = info.and_then(|i| i.DisplayInfo().ok()).and_then(|d| d.DisplayName().ok()).map(|s| s.to_string());
        let texts = n.Notification().ok()?.Visual().ok()?.GetBinding(&generic).ok()?.GetTextElements().ok()?;
        let texts: Vec<String> = texts.into_iter().filter_map(|t| t.Text().ok()).map(|t| t.to_string()).collect();
        let (title, body) = texts.split_first()?;
        Some(Note::new(&app.unwrap_or_default(), title, &body.join("\n")))
    };
    // None until the first poll: what is already in Action Center is history, not news
    let mut seen: Option<HashSet<u32>> = None;
    while !tx.is_closed() {
        // polling, not NotificationChanged: that event needs package identity, LOKA runs unpackaged
        let list = listener.GetNotificationsAsync(NotificationKinds::Toast).and_then(|op| op.join()).map_err(io::Error::other)?;
        let mut now = HashSet::new();
        for n in &list {
            let Ok(id) = n.Id() else { continue };
            now.insert(id);
            if seen.as_ref().is_some_and(|s| !s.contains(&id)) {
                if let Some(note) = read(&n) {
                    if tx.blocking_send(note).is_err() {
                        return Ok(());
                    }
                }
            }
        }
        seen = Some(now);
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

/// macOS has no public API to read other apps' notifications; the only way is usernoted's private
/// SQLite database, which needs Full Disk Access and changes layout between releases.
#[cfg(not(target_os = "windows"))]
fn watch(_tx: mpsc::Sender<Note>) -> io::Result<()> {
    Err(io::Error::other("Reading notifications isn't possible on macOS"))
}

static MIRRORS: Mutex<Vec<(String, JoinHandle<()>)>> = Mutex::new(Vec::new());

/// Viewer side: show paired device `fingerprint`'s notifications here while `on`.
#[tauri::command]
pub async fn set_mirroring(app: AppHandle, fingerprint: String, on: bool) -> Result<(), String> {
    let mut mirrors = MIRRORS.lock().unwrap();
    if let Some(i) = mirrors.iter().position(|(f, _)| *f == fingerprint) {
        mirrors.swap_remove(i).1.abort();
    }
    if on {
        let task = tauri::async_runtime::spawn(mirror(app, fingerprint.clone()));
        mirrors.push((fingerprint, task));
    }
    Ok(())
}

/// Reconnects quietly while the device is unreachable; stops only when it refuses.
async fn mirror(app: AppHandle, device: String) {
    let error = loop {
        match crate::open_service(&app, &device, Service::Notifications).await {
            Ok(mut s) => {
                if let Some(e) = show_all(&app, &mut s).await {
                    break e;
                }
            }
            Err(e) if e.contains("isn't sharing") => break e,
            Err(_) => {}
        }
        tokio::time::sleep(RETRY).await;
    };
    let _ = app.emit("mirror", serde_json::json!({ "device": device, "on": false, "error": error }));
}

/// Shows notifications until the link drops (None) or the owner reports it can't read them (Some).
async fn show_all(app: &AppHandle, s: &mut (impl AsyncRead + Unpin)) -> Option<String> {
    use tauri_plugin_notification::NotificationExt;
    loop {
        match tokio::time::timeout(PING * 2, recv(s)).await {
            Ok(Ok(Some(Msg::Note(n)))) => {
                let title = if n.app.is_empty() { n.title } else { format!("{}: {}", n.app, n.title) };
                let _ = app.notification().builder().title(title).body(n.body).show();
            }
            Ok(Ok(Some(Msg::Error(e)))) => return Some(e),
            Ok(Ok(None)) => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wire_round_trip_and_rejects() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let long = "é\u{1}".repeat(5000);
        let note = Note::new(&long, &long, &long);
        assert_eq!(note.body.chars().count(), 1024);
        send(&mut a, &Msg::Note(note)).await.unwrap();
        send(&mut a, &Msg::Error("no".into())).await.unwrap();
        a.write_all(&0u32.to_le_bytes()).await.unwrap();
        let Some(Msg::Note(got)) = recv(&mut b).await.unwrap() else { panic!() };
        assert_eq!(got, Note::new(&long, &long, &long));
        assert_eq!(recv(&mut b).await.unwrap(), Some(Msg::Error("no".into())));
        assert_eq!(recv(&mut b).await.unwrap(), None);

        a.write_all(&(MAX_FRAME as u32 + 1).to_le_bytes()).await.unwrap();
        assert_eq!(recv(&mut b).await.unwrap_err().kind(), io::ErrorKind::InvalidData);

        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&4u32.to_le_bytes()).await.unwrap();
        a.write_all(b"nope").await.unwrap();
        assert_eq!(recv(&mut b).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
