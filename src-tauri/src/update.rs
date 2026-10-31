//! In-app updates (desktop): `latest.json` on the newest GitHub release lists a signed installer per platform
//! and bundle type (`scripts/release.mjs` writes it); the updater checks the signature against the public key
//! in `tauri.conf.json` before installing anything.

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

#[derive(Serialize)]
pub struct Available {
    version: String,
    notes: Option<String>,
}

/// A newer version this install can update itself to; `None` when there is none, or none for this platform
/// and bundle type (e.g. a release without `latest.json`): the UI then offers the download page instead.
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<Available>, String> {
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| Available { version: u.version, notes: u.body }))
}

/// Downloads (progress as `update` events `{done, total}`), verifies and installs, then restarts.
/// Windows: the installer closes LOKA itself and starts the new one.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?.ok_or("LOKA is up to date")?;
    let mut done = 0u64;
    let a = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                done += chunk as u64;
                let _ = a.emit("update", serde_json::json!({ "done": done, "total": total }));
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}
