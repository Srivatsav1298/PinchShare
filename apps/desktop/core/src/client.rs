//! Outgoing requests: pairing and signed, certificate-pinned API calls.
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::crypto;
use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::slot::{PullReq, SlotInfo};
use crate::server::{ConfirmReq, ConfirmResp, PingResp, StartReq, StartResp, TextReq};
use crate::store::{PairedDevice, Store};
use crate::tls::PinVerifier;

fn http(v: &std::sync::Arc<PinVerifier>, timeout: Duration, pooled: bool) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().use_preconfigured_tls(v.client_config()).connect_timeout(Duration::from_secs(5)).timeout(timeout);
    if !pooled {
        b = b.pool_max_idle_per_host(0);
    }
    b.build().map_err(|e| Error::Http(e.to_string()))
}

fn he(e: reqwest::Error) -> Error {
    Error::Http(e.to_string())
}

/// Join a device that is showing `code`. On success the host is stored in `store`.
pub async fn pair(addr: SocketAddr, code: &str, me: &Identity, store: &Store) -> Result<PairedDevice> {
    let verifier = PinVerifier::new(None);
    let client = http(&verifier, Duration::from_secs(30), false)?;
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

/// A connection-pooled, certificate-pinned, request-signing client for one paired peer.
pub struct Session {
    client: reqwest::Client,
    addr: SocketAddr,
    me: Identity,
    peer: PairedDevice,
    key: Vec<u8>,
}

impl Session {
    pub fn new(addr: SocketAddr, me: &Identity, peer: &PairedDevice) -> Result<Session> {
        let verifier = PinVerifier::new(Some(peer.cert_fp.clone()));
        let key = hex::decode(&peer.auth_key_hex).map_err(|e| Error::Crypto(e.to_string()))?;
        Ok(Session { client: http(&verifier, Duration::from_secs(60), true)?, addr, me: me.clone(), peer: peer.clone(), key })
    }

    pub fn peer(&self) -> &PairedDevice {
        &self.peer
    }

    /// Signed request. Returns (status, body) for any HTTP response; 401 becomes `Rejected`.
    /// Network-level failures are `Error::Http` (retryable by callers).
    pub async fn call(&self, method: reqwest::Method, path: &str, body: Vec<u8>) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let nonce = hex::encode(crypto::random_bytes::<16>());
        let body_sha = crypto::sha256_hex(&body);
        let sig = crypto::sign_request(&self.key, method.as_str(), path, &self.me.device_id, ts, &nonce, &body_sha);
        let resp = self
            .client
            .request(method, format!("https://{}{path}", self.addr))
            .header("x-ps-device", &self.me.device_id)
            .header("x-ps-ts", ts.to_string())
            .header("x-ps-nonce", nonce)
            .header("x-ps-body-sha256", body_sha)
            .header("x-ps-sig", sig)
            .header("content-type", "application/json")
            .body(body)
            .send().await.map_err(he)?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(he)?.to_vec();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Rejected("peer does not recognise this device (unpaired?)".into()));
        }
        Ok((status, bytes))
    }

    async fn ok(&self, method: reqwest::Method, path: &str, body: Vec<u8>) -> Result<Vec<u8>> {
        let (st, b) = self.call(method, path, body).await?;
        if !st.is_success() {
            return Err(Error::Rejected(st.to_string()));
        }
        Ok(b)
    }

    pub async fn ping(&self) -> Result<PingResp> {
        Ok(serde_json::from_slice(&self.ok(reqwest::Method::GET, "/v1/ping", Vec::new()).await?)?)
    }

    pub async fn send_text(&self, text: &str) -> Result<()> {
        self.ok(reqwest::Method::POST, "/v1/text", serde_json::to_vec(&TextReq { text: text.to_string() })?).await?;
        Ok(())
    }

    pub async fn slot_info(&self) -> Result<SlotInfo> {
        Ok(serde_json::from_slice(&self.ok(reqwest::Method::GET, "/v1/slot", Vec::new()).await?)?)
    }

    /// Ask the peer to push its ready slot to our server on `my_port`.
    pub async fn pull(&self, my_port: u16) -> Result<()> {
        self.ok(reqwest::Method::POST, "/v1/slot/pull", serde_json::to_vec(&PullReq { port: my_port })?).await?;
        Ok(())
    }
}
