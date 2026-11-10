//! Outgoing requests: pairing and signed, certificate-pinned API calls.
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::crypto;
use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::server::{ConfirmReq, ConfirmResp, PingResp, StartReq, StartResp, TextReq};
use crate::store::{PairedDevice, Store};
use crate::tls::PinVerifier;

fn http(v: &std::sync::Arc<PinVerifier>) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_preconfigured_tls(v.client_config())
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Http(e.to_string()))
}

fn he(e: reqwest::Error) -> Error {
    Error::Http(e.to_string())
}

/// Join a device that is showing `code`. On success the host is stored in `store`.
pub async fn pair(addr: SocketAddr, code: &str, me: &Identity, store: &Store) -> Result<PairedDevice> {
    let verifier = PinVerifier::new(None);
    let client = http(&verifier)?;
    let base = format!("https://{addr}");
    let (state, my_msg) = crypto::pake_start(code);
    let resp = client.post(format!("{base}/pair/start")).json(&StartReq { msg: hex::encode(my_msg) }).send().await.map_err(he)?;
    if !resp.status().is_success() {
        return Err(Error::Pairing(format!("host refused ({})", resp.status())));
    }
    let sr: StartResp = resp.json().await.map_err(he)?;
    let keys = crypto::pake_finish(state, &hex::decode(&sr.msg).map_err(|e| Error::Pairing(e.to_string()))?)?;
    let host_fp = verifier.seen_single().ok_or_else(|| Error::Pairing("inconsistent host certificate".into()))?;
    let my_fp = me.fingerprint();
    let mac = crypto::confirm_mac(&keys.confirm_key, "client", &me.device_id, &me.name, &my_fp, &host_fp);
    let resp = client
        .post(format!("{base}/pair/confirm"))
        .json(&ConfirmReq { session: sr.session, id: me.device_id.clone(), name: me.name.clone(), fp: my_fp.clone(), mac })
        .send().await.map_err(he)?;
    if !resp.status().is_success() {
        return Err(Error::Pairing("wrong code or certificate mismatch".into()));
    }
    let cr: ConfirmResp = resp.json().await.map_err(he)?;
    if cr.fp != host_fp || !crypto::verify_confirm_mac(&keys.confirm_key, "host", &cr.id, &cr.name, &cr.fp, &my_fp, &cr.mac) {
        return Err(Error::Pairing("host failed to prove it knows the code".into()));
    }
    let dev = PairedDevice { id: cr.id, name: cr.name, auth_key_hex: hex::encode(keys.auth_key), cert_fp: host_fp };
    store.upsert(dev.clone())?;
    Ok(dev)
}

async fn signed(
    addr: SocketAddr, me: &Identity, peer: &PairedDevice, method: reqwest::Method, path: &str, body: Vec<u8>,
) -> Result<reqwest::Response> {
    let verifier = PinVerifier::new(Some(peer.cert_fp.clone()));
    let client = http(&verifier)?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let nonce = hex::encode(crypto::random_bytes::<16>());
    let body_sha = crypto::sha256_hex(&body);
    let key = hex::decode(&peer.auth_key_hex).map_err(|e| Error::Crypto(e.to_string()))?;
    let sig = crypto::sign_request(&key, method.as_str(), path, &me.device_id, ts, &nonce, &body_sha);
    let resp = client
        .request(method, format!("https://{addr}{path}"))
        .header("x-ps-device", &me.device_id)
        .header("x-ps-ts", ts.to_string())
        .header("x-ps-nonce", nonce)
        .header("x-ps-body-sha256", body_sha)
        .header("x-ps-sig", sig)
        .header("content-type", "application/json")
        .body(body)
        .send().await.map_err(he)?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(Error::Rejected("peer does not recognise this device (unpaired?)".into()));
    }
    if !resp.status().is_success() {
        return Err(Error::Rejected(resp.status().to_string()));
    }
    Ok(resp)
}

pub async fn ping(addr: SocketAddr, me: &Identity, peer: &PairedDevice) -> Result<PingResp> {
    signed(addr, me, peer, reqwest::Method::GET, "/v1/ping", Vec::new()).await?.json().await.map_err(he)
}

pub async fn send_text(addr: SocketAddr, me: &Identity, peer: &PairedDevice, text: &str) -> Result<()> {
    let body = serde_json::to_vec(&TextReq { text: text.to_string() })?;
    signed(addr, me, peer, reqwest::Method::POST, "/v1/text", body).await?;
    Ok(())
}
