//! This device's persistent identity: random id, display name, self-signed TLS certificate.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crypto::{random_bytes, sha256_hex};
use crate::error::{Error, Result};

#[derive(Clone)]
pub struct Identity {
    pub device_id: String,
    pub name: String,
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_der: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct IdFile {
    device_id: String,
    name: String,
}

impl Identity {
    pub fn fingerprint(&self) -> String {
        sha256_hex(&self.cert_der)
    }

    /// Load from `dir`, generating a new identity on first run.
    pub fn load_or_create(dir: &Path, default_name: &str) -> Result<Identity> {
        std::fs::create_dir_all(dir)?;
        let id_path = dir.join("identity.json");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        if id_path.exists() && cert_path.exists() && key_path.exists() {
            let f: IdFile = serde_json::from_slice(&std::fs::read(&id_path)?)?;
            let cert_pem = std::fs::read_to_string(&cert_path)?;
            let key_pem = std::fs::read_to_string(&key_path)?;
            let cert_der = pem_to_der(&cert_pem)?;
            return Ok(Identity { device_id: f.device_id, name: f.name, cert_pem, key_pem, cert_der });
        }
        let device_id = hex::encode(random_bytes::<16>());
        let ck = rcgen::generate_simple_self_signed(vec!["pinchshare.local".to_string()])
            .map_err(|e| Error::Tls(e.to_string()))?;
        let cert_pem = ck.cert.pem();
        let key_pem = ck.signing_key.serialize_pem();
        let cert_der = ck.cert.der().to_vec();
        write_private(&key_path, key_pem.as_bytes())?;
        std::fs::write(&cert_path, &cert_pem)?;
        write_private(&id_path, &serde_json::to_vec_pretty(&IdFile { device_id: device_id.clone(), name: default_name.to_string() })?)?;
        Ok(Identity { device_id, name: default_name.to_string(), cert_pem, key_pem, cert_der })
    }
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    base64_decode(&b64).ok_or_else(|| Error::Tls("bad certificate PEM".into()))
}

// Tiny base64 decoder to avoid another dependency for one call site.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Write a file readable only by the current user (0600 on unix).
pub fn write_private(path: &PathBuf, data: &[u8]) -> Result<()> {
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_loads() {
        let d = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(d.path(), "Mac").unwrap();
        let b = Identity::load_or_create(d.path(), "Other").unwrap();
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(b.name, "Mac");
        assert_eq!(a.device_id.len(), 32);
    }
}
