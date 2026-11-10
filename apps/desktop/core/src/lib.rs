//! PinchShare core: discovery, pairing, and encrypted transfer between paired devices.
pub mod client;
pub mod controller;
pub mod crypto;
pub mod discovery;
pub mod error;
pub mod files;
pub mod identity;
pub mod metrics;
pub mod pull;
pub mod send;
pub mod slot;
pub mod server;
pub mod store;
pub mod tls;
pub mod transfer;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

pub use discovery::Peer;
pub use error::{Error, Result};
pub use send::{Phase, Progress, SendOptions, TransferStats};
pub use server::Incoming;
pub use store::PairedDevice;

pub struct NodeConfig {
    pub data_dir: PathBuf,
    pub device_name: String,
    pub bind: SocketAddr,
    /// Where received files are saved. `None` = ~/Downloads/PinchShare.
    pub save_dir: Option<PathBuf>,
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
    /// Manually known addresses (tests, "connect by IP"); take precedence over mDNS.
    static_peers: Mutex<HashMap<String, SocketAddr>>,
}

impl Node {
    pub async fn start(cfg: NodeConfig) -> Result<(Node, UnboundedReceiver<Incoming>)> {
        tls::init_crypto();
        let identity = identity::Identity::load_or_create(&cfg.data_dir, &cfg.device_name)?;
        let store = Arc::new(store::Store::open(&cfg.data_dir)?);
        let (tx, rx) = unbounded_channel();
        let save_dir = cfg.save_dir.clone().unwrap_or_else(files::default_save_dir);
        // Partial downloads do not survive a restart; clear any leftovers.
        let _ = std::fs::remove_dir_all(save_dir.join(".pinchshare-partial"));
        std::fs::create_dir_all(save_dir.join(".pinchshare-partial"))?;
        let shared = Arc::new(server::Shared::new(identity.clone(), store.clone(), tx, save_dir));

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
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await;
        });
        let discovery = if cfg.mdns { Some(discovery::Discovery::start(&identity.device_id, &identity.name, addr.port())?) } else { None };
        Ok((Node { identity, store, addr, shared, handle, discovery, static_peers: Mutex::default() }, rx))
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

    pub fn session(&self, addr: SocketAddr, device_id: &str) -> Result<client::Session> {
        let peer = self.store.get(device_id).ok_or(Error::NotPaired)?;
        client::Session::new(addr, &self.identity, &peer)
    }

    pub fn addr_of(&self, device_id: &str) -> Result<SocketAddr> {
        if let Some(a) = self.static_peers.lock().unwrap().get(device_id) {
            return Ok(*a);
        }
        self.discovery.as_ref().and_then(|d| d.find(device_id)).and_then(|p| p.socket_addr()).ok_or(Error::PeerNotFound)
    }

    pub async fn send_text_to(&self, addr: SocketAddr, device_id: &str, text: &str) -> Result<()> {
        self.session(addr, device_id)?.send_text(text).await
    }

    /// Send a file or folder to a paired device at a known address.
    pub async fn send_path_to(&self, addr: SocketAddr, device_id: &str, path: &std::path::Path, opts: &SendOptions) -> Result<TransferStats> {
        send::send_path(&self.session(addr, device_id)?, path, opts).await
    }

    /// Send several files/folders one after another (stops at the first error or cancel).
    pub async fn send_paths_to(&self, addr: SocketAddr, device_id: &str, paths: &[PathBuf], opts: &SendOptions) -> Result<Vec<TransferStats>> {
        let session = self.session(addr, device_id)?;
        let mut out = Vec::new();
        for p in paths {
            out.push(send::send_path(&session, p, opts).await?);
        }
        Ok(out)
    }

    /// Like `send_path_to`, locating the device via mDNS.
    pub async fn send_path(&self, device_id: &str, path: &std::path::Path, opts: &SendOptions) -> Result<TransferStats> {
        self.send_path_to(self.addr_of(device_id)?, device_id, path, opts).await
    }

    /// Send to a paired device found via mDNS.
    pub async fn send_text(&self, device_id: &str, text: &str) -> Result<()> {
        self.send_text_to(self.addr_of(device_id)?, device_id, text).await
    }

    pub fn set_peer_addr(&self, device_id: &str, addr: SocketAddr) {
        self.static_peers.lock().unwrap().insert(device_id.to_string(), addr);
    }

    /// Fill the ready slot; returns a generation number identifying this fill.
    pub fn set_slot(&self, content: slot::SlotContent, ttl: Duration, trigger: slot::Trigger) -> u64 {
        let gen = self.shared.slot_gen.fetch_add(1, Ordering::SeqCst) + 1;
        *self.shared.slot.lock().unwrap() = Some(slot::ReadySlot::new(content, ttl, trigger, gen));
        gen
    }

    pub fn slot_generation(&self) -> Option<u64> {
        self.shared.slot.lock().unwrap().as_ref().map(|s| s.generation)
    }

    pub fn clear_slot(&self) {
        *self.shared.slot.lock().unwrap() = None;
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
