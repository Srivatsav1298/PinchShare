//! mDNS discovery of other PinchShare devices on the local network.
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::error::{Error, Result};

pub const SERVICE_TYPE: &str = "_pinchshare._tcp.local.";

#[derive(Clone, Debug)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
}

impl Peer {
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        // Prefer IPv4: simplest on home networks.
        let ip = self.addrs.iter().find(|a| a.is_ipv4()).or_else(|| self.addrs.first())?;
        Some(SocketAddr::new(*ip, self.port))
    }
}

pub struct Discovery {
    daemon: ServiceDaemon,
    peers: Arc<Mutex<HashMap<String, Peer>>>,
    own_id: String,
}

impl Discovery {
    pub fn start(own_id: &str, own_name: &str, port: u16) -> Result<Discovery> {
        let m = |e: mdns_sd::Error| Error::Http(format!("mdns: {e}"));
        let daemon = ServiceDaemon::new().map_err(m)?;
        let host = format!("ps-{}.local.", &own_id[..12]);
        let props = [("id", own_id), ("name", own_name)];
        let info = ServiceInfo::new(SERVICE_TYPE, &own_id[..12], &host, "", port, &props[..]).map_err(m)?.enable_addr_auto();
        daemon.register(info).map_err(m)?;
        let rx = daemon.browse(SERVICE_TYPE).map_err(m)?;
        let peers: Arc<Mutex<HashMap<String, Peer>>> = Arc::default();
        let (p2, own) = (peers.clone(), own_id.to_string());
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                match ev {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(id) = info.get_property_val_str("id").map(str::to_string) else { continue };
                        if id == own { continue }
                        let name = info.get_property_val_str("name").unwrap_or("Unknown").to_string();
                        let addrs = info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                        p2.lock().unwrap().insert(id.clone(), Peer { id, name, addrs, port: info.get_port() });
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        p2.lock().unwrap().retain(|id, _| !fullname.starts_with(&id[..12]));
                    }
                    _ => {}
                }
            }
        });
        Ok(Discovery { daemon, peers, own_id: own_id.to_string() })
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.peers.lock().unwrap().values().cloned().collect()
    }

    pub fn find(&self, id: &str) -> Option<Peer> {
        self.peers.lock().unwrap().get(id).cloned()
    }

    pub fn own_id(&self) -> &str {
        &self.own_id
    }

    pub fn shutdown(&self) {
        let _ = self.daemon.shutdown();
    }
}
