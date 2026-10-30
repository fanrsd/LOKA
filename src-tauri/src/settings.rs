//! Per-install state that must survive restarts: paired devices, their portals, preferences.
//! The device's identity is its TLS certificate (`network::secure::Identity`), stored next to this file.

use crate::platform::Portal;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PairedDevice {
    /// certificate fingerprint
    pub fingerprint: String,
    pub alias: String,
}

/// How this computer's mouse and keyboard behave on the devices it drives.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct CursorPrefs {
    /// pointer speed on the other device, × this computer's own
    pub speed: f32,
    /// how long the cursor must rest against the portal before it crosses (0 = at once)
    pub edge_delay_ms: u32,
    /// Ctrl here acts as ⌘ on a Mac (and ⌘ as Ctrl on a PC), so shortcuts keep working
    pub swap_ctrl_cmd: bool,
}

impl Default for CursorPrefs {
    fn default() -> Self {
        Self { speed: 1.0, edge_delay_ms: 0, swap_ctrl_cmd: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Settings {
    /// Pairings made before devices had certificates have UUID fingerprints that never match: re-pair.
    pub paired: Vec<PairedDevice>,
    /// The device map: which stretch of which monitor edge leads to which paired device.
    pub portals: HashMap<String, Portal>,
    /// Addresses added by hand, for networks that drop discovery multicast.
    pub hosts: Vec<IpAddr>,
    pub cursor: CursorPrefs,
    /// What paired devices may open on this one (all off until the user turns them on).
    pub sharing: Sharing,
    /// Where received files go; `None` = Downloads/LOKA.
    pub received: Option<PathBuf>,
    /// Paired devices' hardware addresses as they announced them, to wake them up (Wake-on-LAN).
    pub macs: HashMap<String, String>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Sharing {
    pub terminal: bool,
    /// the folder paired devices can open as a network drive
    pub folder: Option<PathBuf>,
    pub sound: bool,
    pub notifications: bool,
    pub mic: bool,
}

impl Settings {
    /// Missing or corrupt file → nothing paired.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("settings.json");
        let mut s: Settings = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        s.path = path;
        s.save();
        s
    }

    pub fn save(&self) {
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(self.path.parent().unwrap_or(Path::new(".")))?;
            std::fs::write(&self.path, serde_json::to_vec_pretty(self)?)
        };
        if let Err(e) = write() {
            eprintln!("saving settings failed: {e}");
        }
    }

    pub fn is_paired(&self, fingerprint: &str) -> bool {
        self.paired.iter().any(|p| p.fingerprint == fingerprint)
    }

    pub fn pair(&mut self, device: PairedDevice) {
        self.paired.retain(|p| p.fingerprint != device.fingerprint);
        self.paired.push(device);
        self.save();
    }

    /// Forgets the device and its portal.
    pub fn unpair(&mut self, fingerprint: &str) {
        self.paired.retain(|p| p.fingerprint != fingerprint);
        self.portals.remove(fingerprint);
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Edge, Rect};

    #[test]
    fn pairings_and_portals_survive_reload_and_go_together() {
        let dir = std::env::temp_dir().join(format!("loka-settings-{}", uuid::Uuid::new_v4()));
        let mut s = Settings::load(&dir);
        s.pair(PairedDevice { fingerprint: "a".into(), alias: "Laptop".into() });
        s.pair(PairedDevice { fingerprint: "a".into(), alias: "Laptop renamed".into() });
        let portal = Portal { edge: Edge::Left, monitor: Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, start: 0.1, end: 0.5, reach: 120.0 };
        s.portals.insert("a".into(), portal);
        s.save();

        let mut r = Settings::load(&dir);
        assert_eq!(r.paired, vec![PairedDevice { fingerprint: "a".into(), alias: "Laptop renamed".into() }]);
        assert_eq!(r.portals.get("a"), Some(&portal));
        r.unpair("a");
        assert!(Settings::load(&dir).portals.is_empty(), "unpairing removes the door too");

        std::fs::write(dir.join("settings.json"), b"{broken").unwrap();
        assert!(Settings::load(&dir).paired.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
