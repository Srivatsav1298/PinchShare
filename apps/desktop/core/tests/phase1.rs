use std::net::SocketAddr;
use std::time::Duration;

use pinchshare_core::{crypto, tls::PinVerifier, Error, Incoming, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;

async fn node(name: &str, mdns: bool) -> (Node, UnboundedReceiver<Incoming>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (n, rx) = Node::start(NodeConfig {
        data_dir: dir.path().to_path_buf(),
        device_name: name.into(),
        bind: if mdns { "0.0.0.0:0" } else { "127.0.0.1:0" }.parse::<SocketAddr>().unwrap(),
        mdns,
        save_dir: Some(dir.path().join("recv")),
    })
    .await
    .unwrap();
    (n, rx, dir)
}

async fn next_text(rx: &mut UnboundedReceiver<Incoming>) -> Incoming {
    loop {
        let ev = rx.recv().await.unwrap();
        if matches!(ev, Incoming::Text { .. }) {
            return ev;
        }
    }
}

async fn paired_pair() -> (Node, UnboundedReceiver<Incoming>, Node, UnboundedReceiver<Incoming>, Vec<tempfile::TempDir>) {
    let (a, arx, d1) = node("Alpha", false).await;
    let (b, brx, d2) = node("Beta", false).await;
    let code = a.begin_pairing(Duration::from_secs(60));
    let dev = b.pair_with(a.addr, &code).await.unwrap();
    assert_eq!(dev.id, a.identity.device_id);
    (a, arx, b, brx, vec![d1, d2])
}

#[tokio::test]
async fn pair_then_send_text_both_directions() {
    let (a, mut arx, b, mut brx, _d) = paired_pair().await;
    // Beta -> Alpha
    b.send_text_to(a.addr, &a.identity.device_id, "héllo 🌍 from beta").await.unwrap();
    match next_text(&mut arx).await {
        Incoming::Text { from_name, text, from_id } => {
            assert_eq!(text, "héllo 🌍 from beta");
            assert_eq!(from_name, "Beta");
            assert_eq!(from_id, b.identity.device_id);
        }
        _ => unreachable!(),
    }
    // Alpha -> Beta (host learned the client's pinned cert during pairing)
    a.send_text_to(b.addr, &b.identity.device_id, "reply").await.unwrap();
    let Incoming::Text { text, .. } = next_text(&mut brx).await else { unreachable!() };
    assert_eq!(text, "reply");
}

#[tokio::test]
async fn pairing_survives_restart() {
    let (a, _arx, b, _brx, _d) = paired_pair().await;
    assert_eq!(b.store.list().len(), 1);
    assert_eq!(a.store.list().len(), 1);
    // Reload store from disk.
    let reloaded = pinchshare_core::store::Store::open(_d[1].path()).unwrap();
    assert_eq!(reloaded.list(), b.store.list());
}

#[tokio::test]
async fn wrong_code_rejected_and_locks_out_after_three() {
    let (a, _arx, _d1) = node("Alpha", false).await;
    let (b, _brx, _d2) = node("Beta", false).await;
    let code = a.begin_pairing(Duration::from_secs(60));
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for _ in 0..3 {
        assert!(matches!(b.pair_with(a.addr, wrong).await, Err(Error::Pairing(_))));
    }
    // Session is burned: even the right code no longer works.
    assert!(b.pair_with(a.addr, &code).await.is_err());
    assert!(b.store.list().is_empty());
    assert!(a.store.list().is_empty());
}

#[tokio::test]
async fn pairing_requires_an_active_offer() {
    let (a, _arx, _d1) = node("Alpha", false).await;
    let (b, _brx, _d2) = node("Beta", false).await;
    assert!(b.pair_with(a.addr, "123456").await.is_err());
    let code = a.begin_pairing(Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(b.pair_with(a.addr, &code).await.is_err(), "expired code must be refused");
    let code = a.begin_pairing(Duration::from_secs(30));
    a.cancel_pairing();
    assert!(b.pair_with(a.addr, &code).await.is_err());
}

#[tokio::test]
async fn unpaired_device_is_rejected() {
    let (a, mut arx, _d1) = node("Alpha", false).await;
    let (c, _crx, _d2) = node("Stranger", false).await;
    // Stranger fabricates a paired record with a random key.
    c.store
        .upsert(pinchshare_core::PairedDevice {
            id: a.identity.device_id.clone(),
            name: "Alpha".into(),
            auth_key_hex: hex::encode([9u8; 32]),
            cert_fp: a.identity.fingerprint(),
        })
        .unwrap();
    let r = c.send_text_to(a.addr, &a.identity.device_id, "evil").await;
    assert!(matches!(r, Err(Error::Rejected(_))), "got {r:?}");
    assert!(arx.try_recv().is_err());
}

#[tokio::test]
async fn certificate_pinning_blocks_wrong_peer() {
    let (a, _arx, b, _brx, _d) = paired_pair().await;
    let (imposter, _irx, _d3) = node("Imposter", false).await;
    // Beta thinks it is talking to Alpha but the address belongs to someone with a different cert.
    let r = b.send_text_to(imposter.addr, &a.identity.device_id, "secret").await;
    assert!(matches!(r, Err(Error::Http(_))), "got {r:?}");
}

#[tokio::test]
async fn unpair_all_revokes_access() {
    let (a, mut arx, b, _brx, _d) = paired_pair().await;
    a.unpair_all().unwrap();
    let r = b.send_text_to(a.addr, &a.identity.device_id, "after unpair").await;
    assert!(matches!(r, Err(Error::Rejected(_))), "got {r:?}");
    assert!(arx.try_recv().is_err());
}

#[tokio::test]
async fn replayed_request_is_rejected() {
    let (a, mut arx, b, _brx, _d) = paired_pair().await;
    let peer = b.store.get(&a.identity.device_id).unwrap();
    let key = hex::decode(&peer.auth_key_hex).unwrap();
    let body = br#"{"text":"once"}"#.to_vec();
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let sha = crypto::sha256_hex(&body);
    let sig = crypto::sign_request(&key, "POST", "/v1/text", &b.identity.device_id, ts, "fixednonce", &sha);
    let v = PinVerifier::new(Some(peer.cert_fp.clone()));
    let c = reqwest::Client::builder().use_preconfigured_tls(v.client_config()).build().unwrap();
    let send = |ts: u64, sig: String| {
        c.post(format!("https://{}/v1/text", a.addr))
            .header("x-ps-device", &b.identity.device_id).header("x-ps-ts", ts.to_string())
            .header("x-ps-nonce", "fixednonce").header("x-ps-body-sha256", &sha).header("x-ps-sig", sig)
            .header("content-type", "application/json").body(body.clone()).send()
    };
    assert_eq!(send(ts, sig.clone()).await.unwrap().status(), 200);
    assert_eq!(send(ts, sig.clone()).await.unwrap().status(), 401, "replay accepted");
    // Stale timestamp (valid signature, 5 minutes old) is refused.
    let old = ts - 300;
    let sig_old = crypto::sign_request(&key, "POST", "/v1/text", &b.identity.device_id, old, "fixednonce", &sha);
    assert_eq!(send(old, sig_old).await.unwrap().status(), 401);
    arx.recv().await.unwrap();
    assert!(arx.try_recv().is_err());
}

#[tokio::test]
async fn tampered_body_is_rejected() {
    let (a, mut arx, b, _brx, _d) = paired_pair().await;
    let peer = b.store.get(&a.identity.device_id).unwrap();
    let key = hex::decode(&peer.auth_key_hex).unwrap();
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let sha = crypto::sha256_hex(br#"{"text":"good"}"#);
    let sig = crypto::sign_request(&key, "POST", "/v1/text", &b.identity.device_id, ts, "n2", &sha);
    let v = PinVerifier::new(Some(peer.cert_fp.clone()));
    let c = reqwest::Client::builder().use_preconfigured_tls(v.client_config()).build().unwrap();
    let r = c.post(format!("https://{}/v1/text", a.addr))
        .header("x-ps-device", &b.identity.device_id).header("x-ps-ts", ts.to_string()).header("x-ps-nonce", "n2")
        .header("x-ps-body-sha256", &sha).header("x-ps-sig", sig).header("content-type", "application/json")
        .body(r#"{"text":"bad!"}"#).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert!(arx.try_recv().is_err());
}

#[tokio::test]
async fn mdns_discovery_and_send_by_id() {
    let (a, mut arx, _d1) = node("Alpha", true).await;
    let (b, _brx, _d2) = node("Beta", true).await;
    // Pair using loopback (discovery is what we test next).
    let code = a.begin_pairing(Duration::from_secs(60));
    b.pair_with(a.addr, &code).await.unwrap();
    let id = a.identity.device_id.clone();
    let mut found = false;
    for _ in 0..40 {
        if b.peers().iter().any(|p| p.id == id) { found = true; break }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(found, "Beta never discovered Alpha via mDNS; peers={:?}", b.peers());
    b.send_text(&id, "via mdns").await.unwrap();
    let Incoming::Text { text, .. } = next_text(&mut arx).await else { unreachable!() };
    assert_eq!(text, "via mdns");
    a.shutdown(); b.shutdown();
}
