//! PinchShare core: discovery, pairing, and encrypted transfer between paired devices.
pub mod client;
pub mod crypto;
pub mod discovery;
pub mod error;
pub mod identity;
pub mod server;
pub mod store;
pub mod tls;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

pub use discovery::Peer;
pub use error::{Error, Result};
pub use server::Incoming;
pub use store::PairedDevice;

pub struct NodeConfig {
    pub data_dir: PathBuf,
    pub device_name: String,
    pub bind: SocketAddr,
    /// Advertise and browse over mDNS (tests on loopback turn this off).
    pub mdns: bool,
}

pub struct Node {
    pub identity: identity::Identity,
    pub store: Arc<store::Store>,
    pub addr: SocketAddr,
    shared: Arc<server::Shared>,
    handle: axum_server::Handle<SocketAddr>,
    discovery: Option<discovery::Discovery>,
}

impl Node {
    pub async fn start(cfg: NodeConfig) -> Result<(Node, UnboundedReceiver<Incoming>)> {
        tls::init_crypto();
        let identity = identity::Identity::load_or_create(&cfg.data_dir, &cfg.device_name)?;
        let store = Arc::new(store::Store::open(&cfg.data_dir)?);
        let (tx, rx) = unbounded_channel();
        let shared = Arc::new(server::Shared::new(identity.clone(), store.clone(), tx));

        let rustls_cfg = axum_server::tls_rustls::RustlsConfig::from_pem(
            identity.cert_pem.clone().into_bytes(),
            identity.key_pem.clone().into_bytes(),
        )
        .await
        .map_err(|e| Error::Tls(e.to_string()))?;
        let listener = std::net::TcpListener::bind(cfg.bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let handle = axum_server::Handle::new();
        let app = server::router(shared.clone());
        let h2 = handle.clone();
        tokio::spawn(async move {
            let _ = axum_server::from_tcp_rustls(listener, rustls_cfg)
                .expect("listener")
                .handle(h2)
                .serve(app.into_make_service())
                .await;
        });
        let discovery = if cfg.mdns { Some(discovery::Discovery::start(&identity.device_id, &identity.name, addr.port())?) } else { None };
        Ok((Node { identity, store, addr, shared, handle, discovery }, rx))
    }

    /// Start showing a 6-digit code; another device can pair with it until it expires.
    pub fn begin_pairing(&self, ttl: Duration) -> String {
        self.shared.begin_pairing(ttl)
    }

    pub fn cancel_pairing(&self) {
        self.shared.cancel_pairing()
    }

    pub async fn pair_with(&self, addr: SocketAddr, code: &str) -> Result<PairedDevice> {
        client::pair(addr, code, &self.identity, &self.store).await
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.discovery.as_ref().map(|d| d.peers()).unwrap_or_default()
    }

    pub async fn send_text_to(&self, addr: SocketAddr, device_id: &str, text: &str) -> Result<()> {
        let peer = self.store.get(device_id).ok_or(Error::NotPaired)?;
        client::send_text(addr, &self.identity, &peer, text).await
    }

    /// Send to a paired device found via mDNS.
    pub async fn send_text(&self, device_id: &str, text: &str) -> Result<()> {
        let addr = self
            .discovery
            .as_ref()
            .and_then(|d| d.find(device_id))
            .and_then(|p| p.socket_addr())
            .ok_or(Error::PeerNotFound)?;
        self.send_text_to(addr, device_id, text).await
    }

    pub fn unpair_all(&self) -> Result<()> {
        self.store.clear()
    }

    pub fn shutdown(&self) {
        self.handle.shutdown();
        if let Some(d) = &self.discovery {
            d.shutdown();
        }
    }
}
