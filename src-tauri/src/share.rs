//! Shared folder: the owner serves WebDAV for one folder over a paired `Service::Files` stream; the
//! viewer exposes it on `127.0.0.1:PORT` (one service stream per local connection) and mounts it as a
//! network drive. Wire format = plain HTTP/1.1 WebDAV inside the TLS stream.

use std::{convert::Infallible, io, path::PathBuf};

use dav_server::{localfs::LocalFs, memls::MemLs, DavHandler};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tauri::AppHandle;
use tokio::net::TcpListener;

use crate::network::secure::ServerStream;

/// Owner: WebDAV for `root` on this one stream until the viewer hangs up.
pub async fn serve(s: ServerStream, root: PathBuf) -> io::Result<()> {
    // dav-server maps request paths below `root` and refuses `..` past it
    let dav = DavHandler::builder().filesystem(LocalFs::new(root, false, false, false)).locksystem(MemLs::new()).build_handler();
    let svc = service_fn(move |req| {
        let dav = dav.clone();
        async move { Ok::<_, Infallible>(dav.handle(req).await) }
    });
    http1::Builder::new().serve_connection(TokioIo::new(s), svc).await.map_err(io::Error::other)
}

/// Local proxy port per device fingerprint.
/// shortcut: proxies live until the app quits (a few idle tasks); stop them on unpair if that ever matters.
static PROXIES: tokio::sync::Mutex<Vec<(String, u16)>> = tokio::sync::Mutex::const_new(Vec::new());

/// Viewer: open paired device `fingerprint`'s shared folder in Explorer/Finder; returns where it is.
#[tauri::command]
pub async fn open_share(app: AppHandle, fingerprint: String) -> Result<String, String> {
    // fail now with the owner's readable reason (not sharing, offline…) instead of an empty drive
    drop(crate::open_service(&app, &fingerprint, crate::kvm::Service::Files).await?);
    let port = {
        let mut proxies = PROXIES.lock().await;
        match proxies.iter().find(|(fp, _)| *fp == fingerprint) {
            Some(&(_, port)) => port,
            None => {
                let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| format!("Couldn't open the shared folder: {e}"))?;
                let port = listener.local_addr().map_err(|e| e.to_string())?.port();
                tauri::async_runtime::spawn(proxy(listener, app.clone(), fingerprint.clone()));
                proxies.push((fingerprint, port));
                port
            }
        }
    };
    mount(port).await
}

async fn proxy(listener: TcpListener, app: AppHandle, fingerprint: String) {
    while let Ok((mut local, _)) = listener.accept().await {
        let (app, fingerprint) = (app.clone(), fingerprint.clone());
        tauri::async_runtime::spawn(async move {
            match crate::open_service(&app, &fingerprint, crate::kvm::Service::Files).await {
                Ok(mut remote) => {
                    let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                }
                Err(e) => eprintln!("shared folder: {e}"),
            }
        });
    }
}

#[cfg(target_os = "windows")]
async fn mount(port: u16) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // WebClient redirector; an IP, not "localhost", since that may resolve to ::1 where nobody listens.
    // shortcut: WebClient refuses files over 50 MB (FileSizeLimitInBytes), raising it needs admin registry edits.
    let unc = format!(r"\\127.0.0.1@{port}\DavWWWRoot");
    // explorer's exit code means nothing, so only a failure to start counts
    std::process::Command::new("explorer").arg(&unc).creation_flags(CREATE_NO_WINDOW).spawn().map_err(|e| format!("Couldn't open Explorer: {e}"))?;
    Ok(unc)
}

#[cfg(target_os = "macos")]
async fn mount(port: u16) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let script = format!("POSIX path of (mount volume \"http://127.0.0.1:{port}/\")");
        let out = std::process::Command::new("osascript").args(["-e", &script]).output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("macOS couldn't connect to the shared folder".to_string());
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        std::process::Command::new("open").arg(&path).status().map_err(|e| e.to_string())?;
        Ok(path)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn mount(_port: u16) -> Result<String, String> {
    Err("Opening a shared folder isn't supported on this device".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::secure::{Identity, Tls};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One raw HTTP/1.1 request over a fresh pinned TLS pair; the whole response.
    async fn request(root: PathBuf, raw: Vec<u8>) -> Vec<u8> {
        let (owner, viewer) = (Arc::new(Tls::new(&Identity::generate())), Tls::new(&Identity::generate()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (srv, expect) = (owner.clone(), owner.me.clone());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let (s, _) = srv.accept(tcp).await.unwrap();
            let _ = serve(s, root).await;
        });
        let (mut s, _) = viewer.connect(addr, Some(&expect)).await.unwrap();
        s.write_all(&raw).await.unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        // a peer that closes without close_notify is an error here; the bytes before it still count
        while let Ok(n @ 1..) = s.read(&mut buf).await {
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[tokio::test]
    async fn files_round_trip_and_stay_inside_the_shared_folder() {
        let base = std::env::temp_dir().join(format!("loka-share-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let body = b"hello \x00\xff shared folder";

        let mut put = format!("PUT /note.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        put.extend_from_slice(body);
        let res = request(root.clone(), put).await;
        assert!(res.starts_with(b"HTTP/1.1 201"), "{}", String::from_utf8_lossy(&res));
        assert_eq!(std::fs::read(root.join("note.bin")).unwrap(), body);

        let res = request(root.clone(), b"GET /note.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_vec()).await;
        assert!(res.starts_with(b"HTTP/1.1 200") && res.ends_with(body), "{}", String::from_utf8_lossy(&res));

        let res = request(root.clone(), b"PUT /../escaped HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 3\r\n\r\nbad".to_vec()).await;
        assert!(!base.join("escaped").exists(), "wrote outside the shared folder: {}", String::from_utf8_lossy(&res));

        let _ = std::fs::remove_dir_all(&base);
    }
}
