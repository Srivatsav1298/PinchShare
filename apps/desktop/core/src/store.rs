//! Paired-device storage (JSON, 0600). Keys are stored in plaintext for the MVP; see KNOWN_LIMITATIONS.
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::identity::write_private;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    pub auth_key_hex: String,
    /// SHA-256 of the peer's TLS certificate; pinned on every connection.
    pub cert_fp: String,
}

pub struct Store {
    path: PathBuf,
    devices: Mutex<Vec<PairedDevice>>,
}

impl Store {
    pub fn open(dir: &std::path::Path) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("paired.json");
        let devices = if path.exists() { serde_json::from_slice(&std::fs::read(&path)?)? } else { Vec::new() };
        Ok(Store { path, devices: Mutex::new(devices) })
    }

    fn persist(&self, d: &[PairedDevice]) -> Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        write_private(&tmp, &serde_json::to_vec_pretty(d)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<PairedDevice> {
        self.devices.lock().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<PairedDevice> {
        self.devices.lock().unwrap().iter().find(|d| d.id == id).cloned()
    }

    pub fn upsert(&self, dev: PairedDevice) -> Result<()> {
        let mut g = self.devices.lock().unwrap();
        g.retain(|d| d.id != dev.id);
        g.push(dev);
        self.persist(&g)
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        let mut g = self.devices.lock().unwrap();
        g.retain(|d| d.id != id);
        self.persist(&g)
    }

    /// One-click "unpair all".
    pub fn clear(&self) -> Result<()> {
        let mut g = self.devices.lock().unwrap();
        g.clear();
        self.persist(&g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str) -> PairedDevice {
        PairedDevice { id: id.into(), name: "n".into(), auth_key_hex: "00".into(), cert_fp: "fp".into() }
    }

    #[test]
    fn persists_and_reloads() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        s.upsert(dev("a")).unwrap();
        s.upsert(dev("b")).unwrap();
        s.upsert(dev("a")).unwrap();
        assert_eq!(s.list().len(), 2);
        let s2 = Store::open(d.path()).unwrap();
        assert_eq!(s2.list().len(), 2);
        assert!(s2.get("b").is_some());
        s2.remove("b").unwrap();
        s2.clear().unwrap();
        assert!(Store::open(d.path()).unwrap().list().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path()).unwrap();
        s.upsert(dev("a")).unwrap();
        let m = std::fs::metadata(d.path().join("paired.json")).unwrap().permissions().mode();
        assert_eq!(m & 0o077, 0);
    }
}
