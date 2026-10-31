fn main() {
    // the Android side of "let computers control this phone" (LokaPlugin.kt); no Rust commands of its own
    let loka = tauri_build::InlinedPlugin::new()
        .commands(&["status", "openSettings", "askCode", "pairCode", "start", "cursor"])
        .default_permission(tauri_build::DefaultPermissionRule::AllowAllCommands);
    tauri_build::try_build(tauri_build::Attributes::new().plugin("loka", loka)).expect("tauri build");
}
