//! Per-install state that must survive restarts: identity, chosen role, paired devices.

use crate::network::discovery::Role;
use serde::{Deserialize, Serialize};
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PairedDevice {
    pub fingerprint: String,
    pub alias: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// Stable identity; pairings are remembered against it.
    pub fingerprint: String,
    pub role: Option<Role>,
    pub paired: Vec<PairedDevice>,
    /// Addresses added by hand, for networks that drop discovery multicast.
    #[serde(default)]
    pub hosts: Vec<IpAddr>,
    #[serde(skip)]
    path: PathBuf,
}

impl Settings {
    /// Missing or corrupt file → fresh identity (pairings must be redone).
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("settings.json");
        let mut s = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_else(|| Settings { fingerprint: uuid::Uuid::new_v4().to_string(), role: None, paired: Vec::new(), hosts: Vec::new(), path: PathBuf::new() });
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

    pub fn unpair(&mut self, fingerprint: &str) {
        self.paired.retain(|p| p.fingerprint != fingerprint);
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_pairings_survive_reload() {
        let dir = std::env::temp_dir().join(format!("loka-settings-{}", uuid::Uuid::new_v4()));
        let mut s = Settings::load(&dir);
        s.role = Some(Role::Main);
        s.pair(PairedDevice { fingerprint: "a".into(), alias: "Laptop".into() });
        s.pair(PairedDevice { fingerprint: "a".into(), alias: "Laptop renamed".into() });
        s.save();

        let r = Settings::load(&dir);
        assert_eq!(r.fingerprint, s.fingerprint);
        assert_eq!(r.role, Some(Role::Main));
        assert_eq!(r.paired, vec![PairedDevice { fingerprint: "a".into(), alias: "Laptop renamed".into() }]);

        std::fs::write(dir.join("settings.json"), b"{broken").unwrap();
        assert_ne!(Settings::load(&dir).fingerprint, s.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
