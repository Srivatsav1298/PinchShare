use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tls/cert: {0}")]
    Tls(String),
    #[error("http: {0}")]
    Http(String),
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("unknown or unpaired device")]
    NotPaired,
    #[error("peer not found on network")]
    PeerNotFound,
    #[error("rejected by peer: {0}")]
    Rejected(String),
    #[error("transfer cancelled")]
    Cancelled,
    #[error("checksum mismatch: {0}")]
    Checksum(String),
    #[error("unsafe or invalid archive: {0}")]
    Archive(String),
    #[error("crypto: {0}")]
    Crypto(String),
}

pub type Result<T> = std::result::Result<T, Error>;
