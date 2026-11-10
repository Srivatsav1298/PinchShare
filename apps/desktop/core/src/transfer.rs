//! Server side of resumable, checksummed file/folder transfer.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::files;
use crate::server::{Authed, Incoming, Shared};

pub const MAX_TRANSFER_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ACTIVE: usize = 16;

pub struct Transfer {
    owner_id: String,
    owner_name: String,
    name: String,
    is_folder: bool,
    size: u64,
    sha: String,
    received: u64,
    hasher: Sha256,
    part: PathBuf,
}

pub type Transfers = std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Transfer>>>>;

#[derive(Serialize, Deserialize)]
pub struct InitReq {
    pub id: String,
    pub name: String,
    pub is_folder: bool,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize)]
pub struct Received {
    pub received: u64,
}

fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn get(s: &Shared, id: &str) -> Option<Arc<tokio::sync::Mutex<Transfer>>> {
    if !is_hex(id, 32) {
        return None;
    }
    s.transfers.lock().unwrap().get(id).cloned()
}

fn reply(status: StatusCode, received: u64) -> Response {
    (status, Json(Received { received })).into_response()
}

pub async fn init(State(s): State<Arc<Shared>>, Extension(Authed(peer)): Extension<Authed>, Json(r): Json<InitReq>) -> Response {
    if !is_hex(&r.id, 32) || !is_hex(&r.sha256, 64) || r.size > MAX_TRANSFER_BYTES {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let Some(t) = get(&s, &r.id) {
        let t = t.lock().await;
        return if t.owner_id == peer.id && t.sha == r.sha256 && t.size == r.size && t.is_folder == r.is_folder {
            reply(StatusCode::OK, t.received)
        } else {
            StatusCode::CONFLICT.into_response()
        };
    }
    if s.transfers.lock().unwrap().len() >= MAX_ACTIVE {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let part = s.part_dir.join(format!("{}.part", r.id));
    if tokio::fs::File::create(&part).await.is_err() {
        return StatusCode::INSUFFICIENT_STORAGE.into_response();
    }
    let t = Transfer {
        owner_id: peer.id,
        owner_name: peer.name,
        name: files::sanitize_name(&r.name),
        is_folder: r.is_folder,
        size: r.size,
        sha: r.sha256,
        received: 0,
        hasher: Sha256::new(),
        part,
    };
    s.transfers.lock().unwrap().insert(r.id, Arc::new(tokio::sync::Mutex::new(t)));
    reply(StatusCode::OK, 0)
}

pub async fn chunk(
    State(s): State<Arc<Shared>>, Extension(Authed(peer)): Extension<Authed>, Path((id, offset)): Path<(String, u64)>, body: Bytes,
) -> Response {
    let Some(t) = get(&s, &id) else { return StatusCode::NOT_FOUND.into_response() };
    let mut t = t.lock().await;
    if t.owner_id != peer.id {
        return StatusCode::NOT_FOUND.into_response();
    }
    if offset != t.received {
        return reply(StatusCode::CONFLICT, t.received);
    }
    if t.received + body.len() as u64 > t.size {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(mut f) = tokio::fs::OpenOptions::new().append(true).open(&t.part).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    if f.write_all(&body).await.is_err() || f.flush().await.is_err() {
        return StatusCode::INSUFFICIENT_STORAGE.into_response();
    }
    t.hasher.update(&body);
    t.received += body.len() as u64;
    let _ = s.tx.send(Incoming::Progress { transfer_id: id, from_id: t.owner_id.clone(), name: t.name.clone(), received: t.received, total: t.size });
    reply(StatusCode::OK, t.received)
}

async fn drop_transfer(s: &Shared, id: &str, part: &std::path::Path) {
    s.transfers.lock().unwrap().remove(id);
    let _ = tokio::fs::remove_file(part).await;
}

pub async fn finish(State(s): State<Arc<Shared>>, Extension(Authed(peer)): Extension<Authed>, Path(id): Path<String>) -> Response {
    let Some(t) = get(&s, &id) else { return StatusCode::NOT_FOUND.into_response() };
    let t = t.lock().await;
    if t.owner_id != peer.id {
        return StatusCode::NOT_FOUND.into_response();
    }
    if t.received != t.size {
        return reply(StatusCode::BAD_REQUEST, t.received);
    }
    if hex::encode(t.hasher.clone().finalize()) != t.sha {
        drop_transfer(&s, &id, &t.part).await;
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let (part, name, is_folder, save_dir) = (t.part.clone(), t.name.clone(), t.is_folder, s.save_dir.clone());
    let _guard = s.finalize_lock.lock().await; // keeps unique-name selection + rename atomic
    let res = tokio::task::spawn_blocking(move || -> crate::Result<PathBuf> {
        std::fs::create_dir_all(&save_dir)?;
        let dest = files::unique_path(&save_dir, &name);
        if is_folder {
            files::unzip_safe(&part, &dest).inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&dest);
            })?;
            let _ = std::fs::remove_file(&part);
        } else {
            std::fs::rename(&part, &dest)?;
        }
        Ok(dest)
    })
    .await;
    s.transfers.lock().unwrap().remove(&id);
    match res {
        Ok(Ok(path)) => {
            let _ = s.tx.send(Incoming::File { from_id: t.owner_id.clone(), from_name: t.owner_name.clone(), path, size: t.size, is_folder: t.is_folder });
            StatusCode::OK.into_response()
        }
        _ => {
            let _ = tokio::fs::remove_file(&t.part).await;
            StatusCode::UNPROCESSABLE_ENTITY.into_response()
        }
    }
}

pub async fn cancel(State(s): State<Arc<Shared>>, Extension(Authed(peer)): Extension<Authed>, Path(id): Path<String>) -> Response {
    let Some(t) = get(&s, &id) else { return StatusCode::OK.into_response() };
    let t = t.lock().await;
    if t.owner_id == peer.id {
        drop_transfer(&s, &id, &t.part).await;
    }
    StatusCode::OK.into_response()
}
