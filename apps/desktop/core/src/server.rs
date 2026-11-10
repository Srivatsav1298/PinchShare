//! HTTPS server: unauthenticated pairing endpoints + signed-request API under /v1.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, OriginalUri, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::crypto::{self, PairKeys};
use crate::identity::Identity;
use crate::store::{PairedDevice, Store};
use crate::slot::ReadySlot;
use crate::transfer::{self, Transfers};

pub const MAX_BODY: usize = 8 * 1024 * 1024;
pub const MAX_SKEW_SECS: u64 = 60;
const MAX_PAIR_ATTEMPTS: u8 = 3;

/// Events delivered to the application. Contents are never logged.
#[derive(Debug, Clone)]
pub enum Incoming {
    Text { from_id: String, from_name: String, text: String },
    /// Receive progress for an in-flight file/folder transfer.
    Progress { transfer_id: String, from_id: String, name: String, received: u64, total: u64 },
    /// A file or folder was fully received, verified, and saved at `path`.
    File { from_id: String, from_name: String, path: std::path::PathBuf, size: u64, is_folder: bool },
}

struct Pending {
    session: String,
    keys: PairKeys,
}

pub struct PairingSession {
    code: String,
    expires: Instant,
    attempts_left: u8,
    pending: Option<Pending>,
}

pub struct Shared {
    pub identity: Identity,
    pub store: Arc<Store>,
    pub pairing: Mutex<Option<PairingSession>>,
    nonces: Mutex<HashMap<String, Instant>>,
    pub tx: UnboundedSender<Incoming>,
    pub save_dir: std::path::PathBuf,
    pub part_dir: std::path::PathBuf,
    pub transfers: Transfers,
    pub finalize_lock: tokio::sync::Mutex<()>,
    pub slot: Mutex<Option<ReadySlot>>,
    pub slot_gen: std::sync::atomic::AtomicU64,
}

impl Shared {
    pub fn new(identity: Identity, store: Arc<Store>, tx: UnboundedSender<Incoming>, save_dir: std::path::PathBuf) -> Shared {
        let part_dir = save_dir.join(".pinchshare-partial");
        Shared {
            identity, store, pairing: Mutex::new(None), nonces: Mutex::new(HashMap::new()), tx,
            save_dir, part_dir, transfers: Mutex::new(HashMap::new()), finalize_lock: tokio::sync::Mutex::new(()),
            slot: Mutex::new(None), slot_gen: Default::default(),
        }
    }

    pub fn begin_pairing(&self, ttl: Duration) -> String {
        let code = crypto::random_code();
        *self.pairing.lock().unwrap() = Some(PairingSession {
            code: code.clone(),
            expires: Instant::now() + ttl,
            attempts_left: MAX_PAIR_ATTEMPTS,
            pending: None,
        });
        code
    }

    pub fn cancel_pairing(&self) {
        *self.pairing.lock().unwrap() = None;
    }
}

#[derive(Serialize, Deserialize)]
pub struct StartReq { pub msg: String }
#[derive(Serialize, Deserialize)]
pub struct StartResp { pub session: String, pub msg: String }
#[derive(Serialize, Deserialize)]
pub struct ConfirmReq { pub session: String, pub id: String, pub name: String, pub fp: String, pub mac: String }
#[derive(Serialize, Deserialize)]
pub struct ConfirmResp { pub id: String, pub name: String, pub fp: String, pub mac: String }
#[derive(Serialize, Deserialize)]
pub struct TextReq { pub text: String }
#[derive(Serialize, Deserialize)]
pub struct PingResp { pub id: String, pub name: String }

#[derive(Clone)]
pub struct Authed(pub PairedDevice);

pub fn router(shared: Arc<Shared>) -> Router {
    let api = Router::new()
        .route("/ping", get(ping))
        .route("/text", post(text))
        .route("/slot", get(crate::pull::info))
        .route("/slot/pull", post(crate::pull::pull))
        .route("/transfer/init", post(transfer::init))
        .route("/transfer/{id}/chunk/{offset}", put(transfer::chunk))
        .route("/transfer/{id}/finish", post(transfer::finish))
        .route("/transfer/{id}", delete(transfer::cancel))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(shared.clone(), auth));
    Router::new()
        .route("/pair/start", post(pair_start))
        .route("/pair/confirm", post(pair_confirm))
        .nest("/v1", api)
        .with_state(shared)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn hdr<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok())
}

async fn auth(State(s): State<Arc<Shared>>, req: Request, next: Next) -> Result<Response, StatusCode> {
    let (parts, body) = req.into_parts();
    let h = &parts.headers;
    let (Some(dev), Some(ts), Some(nonce), Some(body_sha), Some(sig)) = (
        hdr(h, "x-ps-device"), hdr(h, "x-ps-ts"), hdr(h, "x-ps-nonce"), hdr(h, "x-ps-body-sha256"), hdr(h, "x-ps-sig"),
    ) else { return Err(StatusCode::UNAUTHORIZED) };
    let ts: u64 = ts.parse().map_err(|_| StatusCode::UNAUTHORIZED)?;
    let peer = s.store.get(dev).ok_or(StatusCode::UNAUTHORIZED)?;
    let key = hex::decode(&peer.auth_key_hex).map_err(|_| StatusCode::UNAUTHORIZED)?;
    if now_secs().abs_diff(ts) > MAX_SKEW_SECS {
        return Err(StatusCode::UNAUTHORIZED);
    }
    // Nested routers strip the prefix from `uri`; the client signs the full path.
    let path = parts.extensions.get::<OriginalUri>().map(|o| o.0.path().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    if !crypto::verify_request(&key, parts.method.as_str(), &path, dev, ts, nonce, body_sha, sig) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    {
        // Replay protection (checked only after the signature is valid).
        let mut n = s.nonces.lock().unwrap();
        let cutoff = Duration::from_secs(MAX_SKEW_SECS * 2);
        n.retain(|_, t| t.elapsed() < cutoff);
        if n.insert(format!("{dev}:{nonce}"), Instant::now()).is_some() {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY).await.map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    if crypto::sha256_hex(&bytes) != body_sha {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(Authed(peer));
    Ok(next.run(req).await)
}

async fn ping(State(s): State<Arc<Shared>>) -> Json<PingResp> {
    Json(PingResp { id: s.identity.device_id.clone(), name: s.identity.name.clone() })
}

async fn text(State(s): State<Arc<Shared>>, Extension(Authed(peer)): Extension<Authed>, Json(r): Json<TextReq>) -> StatusCode {
    let _ = s.tx.send(Incoming::Text { from_id: peer.id, from_name: peer.name, text: r.text });
    StatusCode::OK
}

async fn pair_start(State(s): State<Arc<Shared>>, Json(r): Json<StartReq>) -> Result<Json<StartResp>, StatusCode> {
    let peer_msg = hex::decode(&r.msg).map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut g = s.pairing.lock().unwrap();
    let sess = g.as_mut().ok_or(StatusCode::FORBIDDEN)?;
    if Instant::now() > sess.expires || sess.attempts_left == 0 {
        *g = None;
        return Err(StatusCode::FORBIDDEN);
    }
    sess.attempts_left -= 1; // every attempt burns one of three guesses
    let (state, my_msg) = crypto::pake_start(&sess.code);
    let keys = crypto::pake_finish(state, &peer_msg).map_err(|_| StatusCode::BAD_REQUEST)?;
    let session = hex::encode(crypto::random_bytes::<16>());
    sess.pending = Some(Pending { session: session.clone(), keys });
    Ok(Json(StartResp { session, msg: hex::encode(my_msg) }))
}

async fn pair_confirm(State(s): State<Arc<Shared>>, Json(r): Json<ConfirmReq>) -> Result<Json<ConfirmResp>, StatusCode> {
    let mut g = s.pairing.lock().unwrap();
    let sess = g.as_mut().ok_or(StatusCode::FORBIDDEN)?;
    let pending = match sess.pending.take() {
        Some(p) if p.session == r.session => p,
        _ => return Err(StatusCode::FORBIDDEN),
    };
    let my_fp = s.identity.fingerprint();
    if !crypto::verify_confirm_mac(&pending.keys.confirm_key, "client", &r.id, &r.name, &r.fp, &my_fp, &r.mac) {
        if sess.attempts_left == 0 {
            *g = None;
        }
        return Err(StatusCode::FORBIDDEN);
    }
    s.store
        .upsert(PairedDevice { id: r.id.clone(), name: r.name.clone(), auth_key_hex: hex::encode(pending.keys.auth_key), cert_fp: r.fp.clone() })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    *g = None;
    let mac = crypto::confirm_mac(&pending.keys.confirm_key, "host", &s.identity.device_id, &s.identity.name, &my_fp, &r.fp);
    Ok(Json(ConfirmResp { id: s.identity.device_id.clone(), name: s.identity.name.clone(), fp: my_fp, mac }))
}
