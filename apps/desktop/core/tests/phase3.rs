use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pinchshare_core::controller::{Controller, ControllerConfig, Payload, UiEvent};
use pinchshare_core::metrics::Outcome;
use pinchshare_core::slot::{SlotContent, SlotKind, Trigger};
use pinchshare_core::{Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;

struct Side {
    c: Arc<Controller>,
    ui: UnboundedReceiver<UiEvent>,
    node: Arc<Node>,
    recv: PathBuf,
}

struct Rig {
    a: Side, // has content (copy side)
    b: Side, // pinch-out side
    _dirs: Vec<tempfile::TempDir>,
}

fn cfg(d: &std::path::Path, window_ms: u64, ttl_ms: u64) -> ControllerConfig {
    ControllerConfig {
        slot_ttl: Duration::from_millis(ttl_ms),
        cancel_window: Duration::from_millis(window_ms),
        cooldown: Duration::from_millis(300),
        idle_timeout: Duration::from_secs(5),
        peer_query_timeout: Duration::from_secs(1),
        metrics_path: Some(d.join("metrics.jsonl")),
    }
}

async fn rig(window_ms: u64, ttl_ms: u64) -> Rig {
    let mut dirs = vec![];
    let mut sides = vec![];
    for name in ["A", "B"] {
        let d = tempfile::tempdir().unwrap();
        let recv = d.path().join("recv");
        let (n, rx) = Node::start(NodeConfig { data_dir: d.path().join("data"), device_name: name.into(), bind: "127.0.0.1:0".parse().unwrap(), mdns: false, save_dir: Some(recv.clone()) }).await.unwrap();
        let node = Arc::new(n);
        let (c, ui) = Controller::start(node.clone(), rx, cfg(d.path(), window_ms, ttl_ms));
        sides.push(Side { c, ui, node, recv });
        dirs.push(d);
    }
    let b = sides.pop().unwrap();
    let a = sides.pop().unwrap();
    let code = a.node.begin_pairing(Duration::from_secs(60));
    let dev = b.node.pair_with(a.node.addr, &code).await.unwrap();
    b.node.set_peer_addr(&dev.id, a.node.addr);
    Rig { a, b, _dirs: dirs }
}

/// Wait for the first event matching `f`, panic after 8s.
async fn expect<T>(rx: &mut UnboundedReceiver<UiEvent>, mut f: impl FnMut(&UiEvent) -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(8);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(ev)) => if let Some(t) = f(&ev) { return t },
            _ => panic!("timed out waiting for event"),
        }
    }
}

async fn quiet(rx: &mut UnboundedReceiver<UiEvent>, ms: u64, mut bad: impl FnMut(&UiEvent) -> bool) {
    let end = Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(ev)) = tokio::time::timeout(end.saturating_duration_since(Instant::now()), rx.recv()).await {
        assert!(!bad(&ev), "unexpected event {ev:?}");
    }
}

#[tokio::test]
async fn text_flows_after_cancel_window() {
    let mut r = rig(300, 60_000).await;
    r.a.c.copy(SlotContent::Text("héllo".into()), Trigger::Hotkey);
    expect(&mut r.a.ui, |e| matches!(e, UiEvent::Copied { kind: SlotKind::Text, .. }).then_some(())).await;
    let t0 = Instant::now();
    r.b.c.paste(Trigger::Gesture);
    let (name, secs) = expect(&mut r.b.ui, |e| if let UiEvent::Countdown { device_name, secs, .. } = e { Some((device_name.clone(), *secs)) } else { None }).await;
    assert_eq!(name, "A");
    assert!((secs - 0.3).abs() < 0.01);
    let from = expect(&mut r.b.ui, |e| if let UiEvent::Deliver { payload: Payload::Text(t), from_name } = e { assert_eq!(t, "héllo"); Some(from_name.clone()) } else { None }).await;
    assert_eq!(from, "A");
    assert!(t0.elapsed() >= Duration::from_millis(300), "transfer started before the cancel window ended");
    let recs = r.b.c.metrics.read_all();
    assert_eq!(recs.len(), 1);
    assert_eq!((recs[0].outcome.clone(), recs[0].misfire, recs[0].trigger), (Outcome::Delivered, false, Trigger::Gesture));
}

#[tokio::test]
async fn gesture_cancel_is_a_misfire_and_sends_nothing() {
    let mut r = rig(600, 60_000).await;
    r.a.c.copy(SlotContent::Text("secret".into()), Trigger::Hotkey);
    r.b.c.paste(Trigger::Gesture);
    expect(&mut r.b.ui, |e| matches!(e, UiEvent::Countdown { .. }).then_some(())).await;
    r.b.c.cancel();
    let mis = expect(&mut r.b.ui, |e| if let UiEvent::Cancelled { misfire } = e { Some(*misfire) } else { None }).await;
    assert!(mis);
    quiet(&mut r.b.ui, 900, |e| matches!(e, UiEvent::Pulling { .. } | UiEvent::Deliver { .. } | UiEvent::Progress { .. })).await;
    let recs = r.b.c.metrics.read_all();
    assert_eq!((recs[0].outcome.clone(), recs[0].misfire), (Outcome::Cancelled, true));
}

#[tokio::test]
async fn hotkey_cancel_is_not_a_misfire() {
    let mut r = rig(600, 60_000).await;
    r.a.c.copy(SlotContent::Text("x".into()), Trigger::Hotkey);
    r.b.c.paste(Trigger::Hotkey);
    expect(&mut r.b.ui, |e| matches!(e, UiEvent::Countdown { .. }).then_some(())).await;
    r.b.c.cancel();
    let mis = expect(&mut r.b.ui, |e| if let UiEvent::Cancelled { misfire } = e { Some(*misfire) } else { None }).await;
    assert!(!mis);
    let recs = r.b.c.metrics.read_all();
    assert_eq!((recs[0].outcome.clone(), recs[0].misfire), (Outcome::Cancelled, false));
}

#[tokio::test]
async fn slot_expires() {
    let mut r = rig(100, 400).await;
    r.a.c.copy(SlotContent::Text("short lived".into()), Trigger::Hotkey);
    expect(&mut r.a.ui, |e| matches!(e, UiEvent::SlotExpired).then_some(())).await;
    r.b.c.paste(Trigger::Hotkey);
    expect(&mut r.b.ui, |e| matches!(e, UiEvent::NothingToPaste).then_some(())).await;
    assert_eq!(r.b.c.metrics.read_all()[0].outcome, Outcome::NothingToPaste);
}

#[tokio::test]
async fn recopy_resets_expiry() {
    let mut r = rig(100, 700).await;
    r.a.c.copy(SlotContent::Text("one".into()), Trigger::Hotkey);
    tokio::time::sleep(Duration::from_millis(450)).await;
    r.a.c.copy(SlotContent::Text("two".into()), Trigger::Hotkey);
    tokio::time::sleep(Duration::from_millis(450)).await; // original TTL has passed; new one has not
    r.b.c.paste(Trigger::Hotkey);
    expect(&mut r.b.ui, |e| if let UiEvent::Deliver { payload: Payload::Text(t), .. } = e { assert_eq!(t, "two"); Some(()) } else { None }).await;
}

#[tokio::test]
async fn nothing_to_paste_when_nobody_has_content() {
    let mut r = rig(100, 60_000).await;
    r.b.c.paste(Trigger::Gesture);
    expect(&mut r.b.ui, |e| matches!(e, UiEvent::NothingToPaste).then_some(())).await;
}

#[tokio::test]
async fn repeated_gesture_events_are_debounced() {
    let mut r = rig(200, 60_000).await;
    r.a.c.copy(SlotContent::Text("once".into()), Trigger::Hotkey);
    for _ in 0..5 { r.b.c.paste(Trigger::Gesture); }
    expect(&mut r.b.ui, |e| matches!(e, UiEvent::Deliver { .. }).then_some(())).await;
    r.b.c.paste(Trigger::Gesture); // inside cooldown
    quiet(&mut r.b.ui, 250, |e| matches!(e, UiEvent::Countdown { .. })).await;
    assert_eq!(r.b.c.metrics.read_all().len(), 1);
}

#[tokio::test]
async fn files_and_folders_are_pulled() {
    let mut r = rig(100, 60_000).await;
    let src = tempfile::tempdir().unwrap();
    let f = src.path().join("a.txt");
    std::fs::write(&f, "AAA").unwrap();
    let dir = src.path().join("Docs");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/b.txt"), "BBB").unwrap();
    r.a.c.copy(SlotContent::Files(vec![f, dir]), Trigger::Hotkey);
    r.b.c.paste(Trigger::Hotkey);
    let paths = expect(&mut r.b.ui, |e| if let UiEvent::Deliver { payload: Payload::Files(p), .. } = e { Some(p.clone()) } else { None }).await;
    assert_eq!(paths.len(), 2);
    assert!(paths.iter().all(|p| p.starts_with(&r.b.recv)));
    let all: Vec<String> = paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert!(all.contains(&"a.txt".into()) && all.contains(&"Docs".into()));
    let docs = paths.iter().find(|p| p.ends_with("Docs")).unwrap();
    assert_eq!(std::fs::read_to_string(docs.join("sub/b.txt")).unwrap(), "BBB");
}

#[tokio::test]
async fn image_is_pulled_as_png() {
    let mut r = rig(100, 60_000).await;
    let png: Vec<u8> = [&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..], &[7u8; 5000][..]].concat();
    r.a.c.copy(SlotContent::Image(png.clone()), Trigger::Gesture);
    r.b.c.paste(Trigger::Gesture);
    let p = expect(&mut r.b.ui, |e| if let UiEvent::Deliver { payload: Payload::Image(p), .. } = e { Some(p.clone()) } else { None }).await;
    assert_eq!(std::fs::read(&p).unwrap(), png);
    assert_eq!(p.extension().unwrap(), "png");
}

#[tokio::test]
async fn unpaired_peer_gets_nothing() {
    let mut r = rig(100, 60_000).await;
    r.a.c.copy(SlotContent::Text("private".into()), Trigger::Hotkey);
    r.b.node.unpair_all().unwrap();
    r.b.c.paste(Trigger::Hotkey);
    expect(&mut r.b.ui, |e| matches!(e, UiEvent::NothingToPaste).then_some(())).await;
    // And A's slot can't be read by a device A has forgotten.
    r.a.node.unpair_all().unwrap();
    let _ = &r.a.ui;
}
