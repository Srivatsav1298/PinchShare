//! Server side of the paste flow: report slot metadata, and push the slot to a requesting peer.
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::{Extension, Json};

use crate::client::Session;
use crate::send::{self, SendOptions};
use crate::server::{Authed, Shared};
use crate::slot::{PullReq, SlotContent, SlotInfo};
use crate::store::PairedDevice;

pub async fn info(State(s): State<Arc<Shared>>) -> Json<SlotInfo> {
    let g = s.slot.lock().unwrap();
    Json(g.as_ref().filter(|sl| sl.fresh()).map(SlotInfo::of).unwrap_or_else(SlotInfo::none))
}

pub async fn pull(
    State(s): State<Arc<Shared>>, ConnectInfo(remote): ConnectInfo<SocketAddr>, Extension(Authed(peer)): Extension<Authed>, Json(r): Json<PullReq>,
) -> StatusCode {
    let content = {
        let g = s.slot.lock().unwrap();
        match g.as_ref().filter(|sl| sl.fresh()) {
            Some(sl) => sl.content.clone(),
            None => return StatusCode::NOT_FOUND,
        }
    };
    let addr = SocketAddr::new(remote.ip(), r.port);
    tokio::spawn(async move {
        if let Err(e) = push(&s, peer, addr, content).await {
            tracing::warn!("pull push failed: {e}");
        }
    });
    StatusCode::ACCEPTED
}

async fn push(s: &Shared, peer: PairedDevice, addr: SocketAddr, content: SlotContent) -> crate::Result<()> {
    let session = Session::new(addr, &s.identity, &peer)?;
    let opts = SendOptions::default();
    match content {
        SlotContent::Text(t) => session.send_text(&t).await,
        SlotContent::Image(png) => {
            let dir = tempfile::tempdir()?;
            let p = dir.path().join("image.png");
            std::fs::write(&p, png)?;
            send::send_path(&session, &p, &opts).await.map(|_| ())
        }
        SlotContent::Files(paths) => {
            for p in paths {
                send::send_path(&session, &p, &opts).await?;
            }
            Ok(())
        }
    }
}
