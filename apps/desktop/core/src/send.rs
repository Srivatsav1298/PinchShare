//! Client side of file/folder transfer: zip folders, hash, chunked upload with resume, retry,
//! progress callbacks and cancellation.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Method, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::client::Session;
use crate::error::{Error, Result};
use crate::files;
use crate::transfer::{InitReq, Received};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Zipping,
    Hashing,
    Sending,
    Done,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub transfer_id: String,
    pub name: String,
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

#[derive(Clone)]
pub struct SendOptions {
    pub chunk_size: usize,
    pub max_retries: u32,
    pub backoff: Duration,
    pub cancel: Arc<AtomicBool>,
    pub progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,
}

impl Default for SendOptions {
    fn default() -> Self {
        SendOptions { chunk_size: 4 * 1024 * 1024, max_retries: 5, backoff: Duration::from_millis(250), cancel: Arc::default(), progress: None }
    }
}

#[derive(Debug, Clone)]
pub struct TransferStats {
    pub name: String,
    pub bytes: u64,
    pub duration: Duration,
    pub retries: u32,
    pub is_folder: bool,
}

struct Ctx<'a> {
    id: String,
    name: String,
    opts: &'a SendOptions,
}

impl Ctx<'_> {
    fn report(&self, phase: Phase, done: u64, total: u64) {
        if let Some(p) = &self.opts.progress {
            p(Progress { transfer_id: self.id.clone(), name: self.name.clone(), phase, done, total });
        }
    }
    fn cancelled(&self) -> bool {
        self.opts.cancel.load(Ordering::Relaxed)
    }
}

fn parse_received(b: &[u8]) -> Result<u64> {
    Ok(serde_json::from_slice::<Received>(b)?.received)
}

/// Send one file or folder. Folders are zipped to a temporary file first.
pub async fn send_path(session: &Session, path: &Path, opts: &SendOptions) -> Result<TransferStats> {
    let started = Instant::now();
    let meta = std::fs::metadata(path)?;
    let is_folder = meta.is_dir();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "folder".into());
    let ctx = Ctx { id: hex::encode(crate::crypto::random_bytes::<16>()), name: name.clone(), opts };

    // Keep the temp zip alive until the transfer ends.
    let (src, _tmp): (PathBuf, Option<tempfile::NamedTempFile>) = if is_folder {
        let tmp = tempfile::NamedTempFile::new()?;
        let (from, to) = (path.to_path_buf(), tmp.path().to_path_buf());
        let cb = opts.progress.clone();
        let (id, nm) = (ctx.id.clone(), name.clone());
        tokio::task::spawn_blocking(move || {
            let mut done = 0u64;
            files::zip_dir(&from, &to, |n| {
                done += n;
                if let Some(p) = &cb {
                    p(Progress { transfer_id: id.clone(), name: nm.clone(), phase: Phase::Zipping, done, total: 0 });
                }
            })
        })
        .await
        .map_err(|e| Error::Archive(e.to_string()))??;
        (tmp.path().to_path_buf(), Some(tmp))
    } else {
        (path.to_path_buf(), None)
    };
    if ctx.cancelled() {
        return Err(Error::Cancelled);
    }

    let size = std::fs::metadata(&src)?.len();
    let sha = {
        let (p, cb, id, nm) = (src.clone(), opts.progress.clone(), ctx.id.clone(), name.clone());
        tokio::task::spawn_blocking(move || {
            let mut done = 0u64;
            files::hash_file(&p, |n| {
                done += n;
                if let Some(cb) = &cb {
                    cb(Progress { transfer_id: id.clone(), name: nm.clone(), phase: Phase::Hashing, done, total: size });
                }
            })
        })
        .await
        .map_err(|e| Error::Archive(e.to_string()))??
    };

    let init_body = serde_json::to_vec(&InitReq { id: ctx.id.clone(), name: name.clone(), is_folder, size, sha256: sha })?;
    let result = upload(session, &ctx, &src, size, init_body).await;
    match &result {
        Ok(_) => ctx.report(Phase::Done, size, size),
        Err(_) => {
            // Best effort: tell the receiver to discard the partial file.
            let _ = session.call(Method::DELETE, &format!("/v1/transfer/{}", ctx.id), Vec::new()).await;
        }
    }
    let retries = result?;
    Ok(TransferStats { name, bytes: size, duration: started.elapsed(), retries, is_folder })
}

async fn init(session: &Session, body: &[u8]) -> Result<u64> {
    let (st, b) = session.call(Method::POST, "/v1/transfer/init", body.to_vec()).await?;
    if !st.is_success() {
        return Err(Error::Rejected(format!("transfer refused ({st})")));
    }
    parse_received(&b)
}

/// Returns the number of retries that were needed.
async fn upload(session: &Session, ctx: &Ctx<'_>, src: &Path, size: u64, init_body: Vec<u8>) -> Result<u32> {
    let o = ctx.opts;
    let (mut retries, mut failures, mut conflicts) = (0u32, 0u32, 0u32);
    let mut offset = loop {
        match init(session, &init_body).await {
            Ok(r) => break r,
            Err(Error::Http(e)) if failures < o.max_retries => {
                failures += 1;
                retries += 1;
                tracing::debug!("init retry: {e}");
                tokio::time::sleep(backoff(o, failures)).await;
            }
            Err(e) => return Err(e),
        }
    };
    failures = 0;
    let mut file = tokio::fs::File::open(src).await?;
    while offset < size {
        if ctx.cancelled() {
            return Err(Error::Cancelled);
        }
        let len = (o.chunk_size as u64).min(size - offset) as usize;
        let mut buf = vec![0u8; len];
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_exact(&mut buf).await?;
        let path = format!("/v1/transfer/{}/chunk/{offset}", ctx.id);
        let retryable = match session.call(Method::PUT, &path, buf).await {
            Ok((StatusCode::OK, b)) => {
                offset = parse_received(&b)?;
                failures = 0;
                ctx.report(Phase::Sending, offset, size);
                false
            }
            Ok((StatusCode::CONFLICT, b)) => {
                // Our previous attempt landed but the reply was lost: skip ahead.
                conflicts += 1;
                if conflicts > 1000 {
                    return Err(Error::Rejected("receiver and sender disagree on offset".into()));
                }
                offset = parse_received(&b)?;
                false
            }
            Ok((st, _)) if st.is_server_error() => true,
            Ok((st, _)) => return Err(Error::Rejected(format!("chunk refused ({st})"))),
            Err(Error::Http(e)) => {
                tracing::debug!("chunk error: {e}");
                true
            }
            Err(e) => return Err(e),
        };
        if retryable {
            failures += 1;
            retries += 1;
            if failures > o.max_retries {
                return Err(Error::Http(format!("connection lost; gave up after {} retries", o.max_retries)));
            }
            tokio::time::sleep(backoff(o, failures)).await;
            // Resync with the receiver's view of how much it has.
            if let Ok(r) = init(session, &init_body).await {
                offset = r;
            }
        }
    }
    // Finish (verification happens on the receiver). A retried finish that gets 404 means an
    // earlier attempt completed and only its reply was lost.
    let path = format!("/v1/transfer/{}/finish", ctx.id);
    let mut attempt = 0u32;
    loop {
        match session.call(Method::POST, &path, Vec::new()).await {
            Ok((st, _)) if st.is_success() => return Ok(retries),
            Ok((StatusCode::NOT_FOUND, _)) if attempt > 0 => return Ok(retries),
            Ok((StatusCode::UNPROCESSABLE_ENTITY, _)) => return Err(Error::Checksum("receiver rejected the file (checksum or archive invalid)".into())),
            Ok((st, _)) if !st.is_server_error() => return Err(Error::Rejected(format!("finish refused ({st})"))),
            Ok(_) | Err(Error::Http(_)) => {
                attempt += 1;
                retries += 1;
                if attempt > o.max_retries {
                    return Err(Error::Http("could not confirm completion".into()));
                }
                tokio::time::sleep(backoff(o, attempt)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn backoff(o: &SendOptions, n: u32) -> Duration {
    (o.backoff * 2u32.saturating_pow(n.saturating_sub(1))).min(Duration::from_secs(5))
}
