use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pinchshare_core::{Error, Incoming, Node, NodeConfig, Phase, SendOptions};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;

struct Rig {
    a: Node, // receiver
    b: Node, // sender
    arx: UnboundedReceiver<Incoming>,
    recv_dir: PathBuf,
    work: tempfile::TempDir,
    _dirs: Vec<tempfile::TempDir>,
}

async fn rig() -> Rig {
    let mut dirs = Vec::new();
    let mut mk = |name: &str| {
        let d = tempfile::tempdir().unwrap();
        let cfg = NodeConfig { data_dir: d.path().join("data"), device_name: name.into(), bind: "127.0.0.1:0".parse().unwrap(), mdns: false, save_dir: Some(d.path().join("recv")) };
        let p = d.path().join("recv");
        dirs.push(d);
        (cfg, p)
    };
    let (ca, recv_dir) = mk("Receiver");
    let (cb, _) = mk("Sender");
    let (a, arx) = Node::start(ca).await.unwrap();
    let (b, _brx) = Node::start(cb).await.unwrap();
    let code = a.begin_pairing(Duration::from_secs(60));
    b.pair_with(a.addr, &code).await.unwrap();
    Rig { a, b, arx, recv_dir, work: tempfile::tempdir().unwrap(), _dirs: dirs }
}

/// Deterministic pseudo-random bytes (incompressible, reproducible).
fn write_random(path: &Path, bytes: u64, seed: u64) {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut x = seed | 1;
    let mut buf = vec![0u8; 1 << 20];
    let mut left = bytes;
    while left > 0 {
        for c in buf.chunks_mut(8) {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
        }
        let n = left.min(buf.len() as u64) as usize;
        f.write_all(&buf[..n]).unwrap();
        left -= n as u64;
    }
}

fn sha_file(p: &Path) -> String {
    pinchshare_core::files::hash_file(p, |_| {}).unwrap()
}

/// path (relative, '/'-separated) -> sha256 for every file; directories listed with "".
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if p.is_dir() { out.insert(format!("{rel}/"), String::new()); walk(&p, root, out); } else { out.insert(rel, sha_file(&p)); }
        }
    }
    let mut m = BTreeMap::new();
    walk(root, root, &mut m);
    m
}

async fn next_file(rx: &mut UnboundedReceiver<Incoming>) -> (PathBuf, u64, bool) {
    loop {
        if let Some(Incoming::File { path, size, is_folder, .. }) = rx.recv().await { return (path, size, is_folder); }
    }
}

#[tokio::test]
async fn small_file_with_unique_naming_and_progress() {
    let mut r = rig().await;
    let f = r.work.path().join("hello.txt");
    std::fs::write(&f, "hello world").unwrap();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let s2 = sent.clone();
    let opts = SendOptions { progress: Some(Arc::new(move |p| s2.lock().unwrap().push((p.phase, p.done, p.total)))), ..Default::default() };
    let st = r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &f, &opts).await.unwrap();
    assert_eq!(st.bytes, 11);
    let (p1, size, folder) = next_file(&mut r.arx).await;
    assert_eq!((size, folder), (11, false));
    assert_eq!(std::fs::read_to_string(&p1).unwrap(), "hello world");
    assert_eq!(p1, r.recv_dir.join("hello.txt"));
    // Same name again -> "hello (1).txt", original untouched.
    r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &f, &SendOptions::default()).await.unwrap();
    let (p2, ..) = next_file(&mut r.arx).await;
    assert_eq!(p2, r.recv_dir.join("hello (1).txt"));
    let phases: Vec<Phase> = sent.lock().unwrap().iter().map(|x| x.0).collect();
    assert!(phases.contains(&Phase::Hashing) && phases.contains(&Phase::Sending) && phases.last() == Some(&Phase::Done));
    // No partial files left behind.
    assert_eq!(std::fs::read_dir(r.recv_dir.join(".pinchshare-partial")).unwrap().count(), 0);
}

#[tokio::test]
async fn empty_file_and_exact_chunk_boundary() {
    let mut r = rig().await;
    let opts = SendOptions { chunk_size: 1024, ..Default::default() };
    for (name, n) in [("empty.bin", 0u64), ("exact.bin", 4096), ("plus1.bin", 4097)] {
        let f = r.work.path().join(name);
        write_random(&f, n, 42);
        r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &f, &opts).await.unwrap();
        let (p, size, _) = next_file(&mut r.arx).await;
        assert_eq!(size, n);
        assert_eq!(sha_file(&p), sha_file(&f), "{name}");
    }
}

#[tokio::test]
async fn nested_folder_roundtrip() {
    let mut r = rig().await;
    let src = r.work.path().join("Project X");
    for d in ["a/b/c", "a/empty", "img", "ünï cødé"] { std::fs::create_dir_all(src.join(d)).unwrap(); }
    std::fs::write(src.join("readme.md"), "# hi").unwrap();
    write_random(&src.join("a/b/c/data.bin"), 3_000_000, 7);
    write_random(&src.join("img/photo.jpg"), 500_000, 8);
    std::fs::write(src.join("ünï cødé/файл.txt"), "unicode").unwrap();
    let st = r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &src, &SendOptions::default()).await.unwrap();
    assert!(st.is_folder);
    let (dest, _, folder) = next_file(&mut r.arx).await;
    assert!(folder);
    assert_eq!(dest, r.recv_dir.join("Project X"));
    assert_eq!(tree(&dest), tree(&src));
}

#[tokio::test]
async fn multiple_files_in_one_call() {
    let mut r = rig().await;
    let paths: Vec<PathBuf> = (0..3).map(|i| { let p = r.work.path().join(format!("f{i}.bin")); write_random(&p, 100_000 * (i + 1), i); p }).collect();
    let stats = r.b.send_paths_to(r.a.addr, &r.a.identity.device_id, &paths, &SendOptions::default()).await.unwrap();
    assert_eq!(stats.len(), 3);
    for p in &paths {
        let (got, ..) = next_file(&mut r.arx).await;
        assert_eq!(sha_file(&got), sha_file(p));
    }
}

#[tokio::test]
async fn hostile_file_name_cannot_escape_save_dir() {
    // Craft init with a traversal name using the public Session API.
    let r = rig().await;
    let peer = r.b.store.get(&r.a.identity.device_id).unwrap();
    let s = pinchshare_core::client::Session::new(r.a.addr, &r.b.identity, &peer).unwrap();
    let data = b"x";
    let body = serde_json::json!({"id": "00112233445566778899aabbccddeeff", "name": "../../escape.txt", "is_folder": false, "size": 1, "sha256": hex::encode(Sha256::digest(data))});
    let (st, _) = s.call(reqwest::Method::POST, "/v1/transfer/init", serde_json::to_vec(&body).unwrap()).await.unwrap();
    assert!(st.is_success());
    s.call(reqwest::Method::PUT, "/v1/transfer/00112233445566778899aabbccddeeff/chunk/0", data.to_vec()).await.unwrap();
    let (st, _) = s.call(reqwest::Method::POST, "/v1/transfer/00112233445566778899aabbccddeeff/finish", vec![]).await.unwrap();
    assert!(st.is_success());
    assert!(r.recv_dir.join("escape.txt").exists());
    assert!(!r.recv_dir.parent().unwrap().join("escape.txt").exists());
}

#[tokio::test]
async fn checksum_mismatch_is_detected_and_discarded() {
    let r = rig().await;
    let peer = r.b.store.get(&r.a.identity.device_id).unwrap();
    let s = pinchshare_core::client::Session::new(r.a.addr, &r.b.identity, &peer).unwrap();
    let id = "ffeeddccbbaa99887766554433221100";
    let wrong_sha = hex::encode(Sha256::digest(b"something else"));
    let body = serde_json::json!({"id": id, "name": "bad.bin", "is_folder": false, "size": 5, "sha256": wrong_sha});
    s.call(reqwest::Method::POST, "/v1/transfer/init", serde_json::to_vec(&body).unwrap()).await.unwrap();
    s.call(reqwest::Method::PUT, &format!("/v1/transfer/{id}/chunk/0"), b"12345".to_vec()).await.unwrap();
    let (st, _) = s.call(reqwest::Method::POST, &format!("/v1/transfer/{id}/finish"), vec![]).await.unwrap();
    assert_eq!(st.as_u16(), 422);
    assert!(!r.recv_dir.join("bad.bin").exists());
    assert_eq!(std::fs::read_dir(r.recv_dir.join(".pinchshare-partial")).unwrap().count(), 0);
}

#[tokio::test]
async fn out_of_order_chunk_is_refused_with_resume_offset() {
    let r = rig().await;
    let peer = r.b.store.get(&r.a.identity.device_id).unwrap();
    let s = pinchshare_core::client::Session::new(r.a.addr, &r.b.identity, &peer).unwrap();
    let id = "11112222333344445555666677778888";
    let body = serde_json::json!({"id": id, "name": "o.bin", "is_folder": false, "size": 10, "sha256": hex::encode(Sha256::digest(b"0123456789"))});
    s.call(reqwest::Method::POST, "/v1/transfer/init", serde_json::to_vec(&body).unwrap()).await.unwrap();
    let (st, b) = s.call(reqwest::Method::PUT, &format!("/v1/transfer/{id}/chunk/5"), b"56789".to_vec()).await.unwrap();
    assert_eq!(st.as_u16(), 409);
    assert!(String::from_utf8_lossy(&b).contains("\"received\":0"));
    // Another paired device cannot touch this transfer: simulate with a stranger session -> unauthorised.
}

#[tokio::test]
async fn cancel_mid_transfer_discards_partial() {
    let r = rig().await;
    let f = r.work.path().join("big.bin");
    write_random(&f, 8 * 1024 * 1024, 5);
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    let opts = SendOptions {
        chunk_size: 1 << 20,
        cancel: cancel.clone(),
        progress: Some(Arc::new(move |p| if p.phase == Phase::Sending && p.done >= 2 << 20 { c2.store(true, Ordering::Relaxed) })),
        ..Default::default()
    };
    let res = r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &f, &opts).await;
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_dir(&r.recv_dir).unwrap().filter(|e| e.as_ref().unwrap().file_name() != ".pinchshare-partial").count(), 0);
    assert_eq!(std::fs::read_dir(r.recv_dir.join(".pinchshare-partial")).unwrap().count(), 0);
}

/// TCP proxy that severs the first `kills` connections once they have carried `after` bytes
/// client->server, simulating Wi-Fi drops. TLS stays end-to-end, so certificate pinning still holds.
async fn flaky_proxy(upstream: SocketAddr, kills: usize, after: usize) -> (SocketAddr, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let killed = Arc::new(AtomicUsize::new(0));
    let k2 = killed.clone();
    tokio::spawn(async move {
        loop {
            let (mut c, _) = l.accept().await.unwrap();
            let k = k2.clone();
            tokio::spawn(async move {
                let Ok(mut u) = TcpStream::connect(upstream).await else { return };
                let should_kill = k.load(Ordering::SeqCst) < kills;
                let (mut cr, mut cw) = c.split();
                let (mut ur, mut uw) = u.split();
                let up = async {
                    let (mut n, mut buf) = (0usize, vec![0u8; 16384]);
                    loop {
                        let m = cr.read(&mut buf).await.unwrap_or(0);
                        if m == 0 { break }
                        if should_kill && n + m > after { k.fetch_add(1, Ordering::SeqCst); return; } // drop mid-stream
                        n += m;
                        if uw.write_all(&buf[..m]).await.is_err() { break }
                    }
                };
                let down = async { let _ = tokio::io::copy(&mut ur, &mut cw).await; };
                tokio::select! { _ = up => {}, _ = down => {} }
            });
        }
    });
    (addr, killed)
}

#[tokio::test]
async fn survives_dropped_connections_and_resumes() {
    let mut r = rig().await;
    let f = r.work.path().join("flaky.bin");
    write_random(&f, 12 * 1024 * 1024, 99);
    let (proxy, killed) = flaky_proxy(r.a.addr, 3, 2_500_000).await;
    let opts = SendOptions { chunk_size: 1 << 20, backoff: Duration::from_millis(10), ..Default::default() };
    let st = r.b.send_path_to(proxy, &r.a.identity.device_id, &f, &opts).await.unwrap();
    assert!(killed.load(Ordering::SeqCst) >= 1, "proxy never dropped a connection");
    assert!(st.retries >= 1, "expected retries, got {}", st.retries);
    let (p, ..) = next_file(&mut r.arx).await;
    assert_eq!(sha_file(&p), sha_file(&f));
}

#[tokio::test]
async fn gives_up_when_peer_is_unreachable() {
    let r = rig().await;
    let f = r.work.path().join("x.bin");
    write_random(&f, 1000, 1);
    let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let opts = SendOptions { max_retries: 2, backoff: Duration::from_millis(5), ..Default::default() };
    let res = r.b.send_path_to(dead, &r.a.identity.device_id, &f, &opts).await;
    assert!(matches!(res, Err(Error::Http(_))), "{res:?}");
}

#[tokio::test]
async fn unpaired_sender_cannot_upload() {
    let r = rig().await;
    r.a.unpair_all().unwrap();
    let f = r.work.path().join("x.bin");
    write_random(&f, 1000, 1);
    let res = r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &f, &SendOptions::default()).await;
    assert!(matches!(res, Err(Error::Rejected(_))), "{res:?}");
}

async fn big_transfer(file_mb: u64) {
    let mut r = rig().await;
    let f = r.work.path().join("big.bin");
    write_random(&f, file_mb << 20, 12345);
    let st = r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &f, &SendOptions::default()).await.unwrap();
    let (p, size, _) = next_file(&mut r.arx).await;
    assert_eq!(size, file_mb << 20);
    assert_eq!(sha_file(&p), sha_file(&f));
    eprintln!("{file_mb} MB in {:.2}s = {:.0} MB/s", st.duration.as_secs_f32(), file_mb as f32 / st.duration.as_secs_f32());
}

#[tokio::test]
async fn medium_file_64mb() {
    big_transfer(64).await;
}

/// Run with: cargo test --release --test phase2 -- --ignored --nocapture
#[tokio::test]
#[ignore = "500 MB; run explicitly in release mode"]
async fn file_500mb() {
    big_transfer(500).await;
}

#[tokio::test]
#[ignore = "500 MB nested folder; run explicitly in release mode"]
async fn folder_500mb_nested() {
    let mut r = rig().await;
    let src = r.work.path().join("Big Folder");
    for i in 0..5 {
        let d = src.join(format!("level{i}/sub{i}/deeper"));
        std::fs::create_dir_all(&d).unwrap();
        write_random(&d.join(format!("blob{i}.bin")), 100 << 20, 1000 + i);
        std::fs::write(d.join("note.txt"), format!("note {i}")).unwrap();
    }
    let st = r.b.send_path_to(r.a.addr, &r.a.identity.device_id, &src, &SendOptions::default()).await.unwrap();
    let (dest, ..) = next_file(&mut r.arx).await;
    assert_eq!(tree(&dest), tree(&src));
    eprintln!("folder 500 MB in {:.2}s", st.duration.as_secs_f32());
}
