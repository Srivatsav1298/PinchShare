//! Pairing (SPAKE2 over the 6-digit code), key derivation and request signing.
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::error::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

pub const SPAKE_ID: &[u8] = b"pinchshare-v1";

/// Keys derived from the SPAKE2 shared secret.
#[derive(Clone)]
pub struct PairKeys {
    /// Proves both sides know the code and saw the same TLS certificate.
    pub confirm_key: [u8; 32],
    /// Long-lived key stored for each paired device; signs every later request.
    pub auth_key: [u8; 32],
}

pub struct PakeState(Spake2<Ed25519Group>);

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS RNG unavailable");
    b
}

/// Random 6-digit code (uniform, rejection-sampled).
pub fn random_code() -> String {
    loop {
        let n = u32::from_le_bytes(random_bytes::<4>());
        if n < 4_000_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

pub fn pake_start(code: &str) -> (PakeState, Vec<u8>) {
    let (s, msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_bytes()),
        &Identity::new(SPAKE_ID),
    );
    (PakeState(s), msg)
}

pub fn pake_finish(state: PakeState, peer_msg: &[u8]) -> Result<PairKeys> {
    let k = state.0.finish(peer_msg).map_err(|e| Error::Crypto(format!("{e:?}")))?;
    let hk = Hkdf::<Sha256>::new(None, &k);
    let mut confirm_key = [0u8; 32];
    let mut auth_key = [0u8; 32];
    hk.expand(b"pinchshare confirm", &mut confirm_key).map_err(|e| Error::Crypto(e.to_string()))?;
    hk.expand(b"pinchshare auth", &mut auth_key).map_err(|e| Error::Crypto(e.to_string()))?;
    Ok(PairKeys { confirm_key, auth_key })
}

fn lp(out: &mut Vec<u8>, part: &[u8]) {
    out.extend_from_slice(&(part.len() as u32).to_be_bytes());
    out.extend_from_slice(part);
}

fn confirm_input(role: &str, id: &str, name: &str, own_fp: &str, peer_fp: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for p in [role, id, name, own_fp, peer_fp] {
        lp(&mut v, p.as_bytes());
    }
    v
}

/// MAC over identities and both TLS certificate fingerprints. A man-in-the-middle terminating
/// TLS shows each side a different certificate, so the MACs will not verify.
pub fn confirm_mac(key: &[u8; 32], role: &str, id: &str, name: &str, own_fp: &str, peer_fp: &str) -> String {
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).unwrap();
    m.update(&confirm_input(role, id, name, own_fp, peer_fp));
    hex::encode(m.finalize().into_bytes())
}

pub fn verify_confirm_mac(
    key: &[u8; 32], role: &str, id: &str, name: &str, own_fp: &str, peer_fp: &str, mac_hex: &str,
) -> bool {
    let Ok(tag) = hex::decode(mac_hex) else { return false };
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).unwrap();
    m.update(&confirm_input(role, id, name, own_fp, peer_fp));
    m.verify_slice(&tag).is_ok()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn sig_input(method: &str, path: &str, device: &str, ts: u64, nonce: &str, body_sha: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for p in [method, path, device, &ts.to_string(), nonce, body_sha] {
        lp(&mut v, p.as_bytes());
    }
    v
}

pub fn sign_request(key: &[u8], method: &str, path: &str, device: &str, ts: u64, nonce: &str, body_sha: &str) -> String {
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).unwrap();
    m.update(&sig_input(method, path, device, ts, nonce, body_sha));
    hex::encode(m.finalize().into_bytes())
}

#[allow(clippy::too_many_arguments)]
pub fn verify_request(key: &[u8], method: &str, path: &str, device: &str, ts: u64, nonce: &str, body_sha: &str, sig_hex: &str) -> bool {
    let Ok(tag) = hex::decode(sig_hex) else { return false };
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).unwrap();
    m.update(&sig_input(method, path, device, ts, nonce, body_sha));
    m.verify_slice(&tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_is_six_digits() {
        for _ in 0..200 {
            let c = random_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        }
    }

    #[test]
    fn same_code_same_keys() {
        let (a, ma) = pake_start("123456");
        let (b, mb) = pake_start("123456");
        let ka = pake_finish(a, &mb).unwrap();
        let kb = pake_finish(b, &ma).unwrap();
        assert_eq!(ka.auth_key, kb.auth_key);
        assert_eq!(ka.confirm_key, kb.confirm_key);
        assert_ne!(ka.auth_key, ka.confirm_key);
    }

    #[test]
    fn wrong_code_different_keys() {
        let (a, ma) = pake_start("123456");
        let (b, mb) = pake_start("654321");
        let ka = pake_finish(a, &mb).unwrap();
        let kb = pake_finish(b, &ma).unwrap();
        assert_ne!(ka.confirm_key, kb.confirm_key);
    }

    #[test]
    fn confirm_mac_binds_fingerprints() {
        let k = [7u8; 32];
        let mac = confirm_mac(&k, "client", "id1", "Laptop", "fpB", "fpA");
        assert!(verify_confirm_mac(&k, "client", "id1", "Laptop", "fpB", "fpA", &mac));
        assert!(!verify_confirm_mac(&k, "client", "id1", "Laptop", "fpB", "fpMITM", &mac));
        assert!(!verify_confirm_mac(&k, "host", "id1", "Laptop", "fpB", "fpA", &mac));
        assert!(!verify_confirm_mac(&[8u8; 32], "client", "id1", "Laptop", "fpB", "fpA", &mac));
    }

    #[test]
    fn request_signature_roundtrip_and_tamper() {
        let k = [1u8; 32];
        let s = sign_request(&k, "POST", "/v1/text", "dev", 100, "n1", "abc");
        assert!(verify_request(&k, "POST", "/v1/text", "dev", 100, "n1", "abc", &s));
        assert!(!verify_request(&k, "POST", "/v1/text", "dev", 101, "n1", "abc", &s));
        assert!(!verify_request(&k, "POST", "/v1/text", "dev", 100, "n1", "abd", &s));
        assert!(!verify_request(&k, "POST", "/v1/other", "dev", 100, "n1", "abc", &s));
        assert!(!verify_request(&k, "POST", "/v1/text", "dev", 100, "n1", "abc", "zz"));
    }
}
