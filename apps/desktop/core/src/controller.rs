//! Copy/paste orchestration: ready slot, 3-second cancel window, pull from a peer, delivery.
//! Platform glue (gestures, hotkeys, clipboard, toasts) lives in the app and talks to this.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, Notify};

use crate::metrics::{Metrics, Outcome, Record};
use crate::slot::{SlotContent, SlotInfo, SlotKind, Trigger};
use crate::{Error, Incoming, Node, Result};

#[derive(Clone, Debug)]
pub struct ControllerConfig {
    pub slot_ttl: Duration,
    pub cancel_window: Duration,
    /// Ignore pastes this soon after the previous one finished (gesture events repeat).
    pub cooldown: Duration,
    /// Abort a pull if nothing arrives for this long.
    pub idle_timeout: Duration,
    pub peer_query_timeout: Duration,
    pub metrics_path: Option<PathBuf>,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        ControllerConfig {
            slot_ttl: Duration::from_secs(60),
            cancel_window: Duration::from_secs(3),
            cooldown: Duration::from_millis(1000),
            idle_timeout: Duration::from_secs(20),
            peer_query_timeout: Duration::from_millis(1500),
            metrics_path: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    Text(String),
    Image(PathBuf),
    Files(Vec<PathBuf>),
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Copied { kind: SlotKind, trigger: Trigger },
    SlotExpired,
    NothingToPaste,
    /// Start of the cancel window: show "Sending to <device>... Cancel" for `secs`.
    Countdown { device_id: String, device_name: String, kind: SlotKind, secs: f32 },
    Cancelled { misfire: bool },
    Pulling { device_name: String },
    Progress { name: String, received: u64, total: u64 },
    /// Content arrived because of this device's paste request.
    Deliver { from_name: String, payload: Payload },
    /// Content arrived without a paste request (sent by a peer deliberately).
    Received { from_name: String, payload: Payload },
    Error(String),
}

struct Expect {
    device_id: String,
    kind: SlotKind,
    count: usize,
    files: Vec<PathBuf>,
    done: Option<oneshot::Sender<Payload>>,
    activity: Arc<Notify>,
}

#[derive(Default)]
struct State {
    busy: bool,
    last_done: Option<Instant>,
    cancel: Option<Arc<Notify>>,
    expect: Option<Expect>,
}

pub struct Controller {
    node: Arc<Node>,
    cfg: ControllerConfig,
    ui: UnboundedSender<UiEvent>,
    state: Mutex<State>,
    pub metrics: Metrics,
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Controller {
    /// Takes ownership of the node's incoming-event stream.
    pub fn start(node: Arc<Node>, mut rx: UnboundedReceiver<Incoming>, cfg: ControllerConfig) -> (Arc<Controller>, UnboundedReceiver<UiEvent>) {
        let (ui, ui_rx) = unbounded_channel();
        let metrics = Metrics::new(cfg.metrics_path.clone());
        let c = Arc::new(Controller { node, cfg, ui, state: Mutex::default(), metrics });
        let c2 = c.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                c2.on_incoming(ev);
            }
        });
        (c, ui_rx)
    }

    fn emit(&self, e: UiEvent) {
        let _ = self.ui.send(e);
    }

    /// Put content in the ready slot (copy side of the gesture).
    pub fn copy(self: &Arc<Self>, content: SlotContent, trigger: Trigger) {
        let kind = content.kind();
        let gen = self.node.set_slot(content, self.cfg.slot_ttl, trigger);
        self.emit(UiEvent::Copied { kind, trigger });
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(me.cfg.slot_ttl).await;
            if me.node.slot_generation() == Some(gen) {
                me.node.clear_slot();
                me.emit(UiEvent::SlotExpired);
            }
        });
    }

    pub fn clear_slot(&self) {
        self.node.clear_slot();
    }

    /// Cancel the pending paste (inside the confirmation window).
    pub fn cancel(&self) {
        if let Some(n) = self.state.lock().unwrap().cancel.clone() {
            n.notify_one();
        }
    }

    /// Paste side of the gesture: find the freshest ready slot among paired peers and pull it.
    pub fn paste(self: &Arc<Self>, trigger: Trigger) {
        {
            let mut st = self.state.lock().unwrap();
            if st.busy || st.last_done.is_some_and(|t| t.elapsed() < self.cfg.cooldown) {
                return;
            }
            st.busy = true;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let mut rec = Record { ts_unix: now_unix(), kind: None, bytes: 0, duration_ms: 0, trigger, outcome: Outcome::Failed, misfire: false, error: None };
            match me.paste_flow(trigger, &mut rec).await {
                Ok(()) => {}
                Err(Error::Cancelled) => {}
                Err(e) => {
                    rec.outcome = Outcome::Failed;
                    rec.error = Some(error_category(&e));
                    me.emit(UiEvent::Error(e.to_string()));
                }
            }
            rec.duration_ms = started.elapsed().as_millis() as u64;
            me.metrics.record(&rec);
            let mut st = me.state.lock().unwrap();
            st.busy = false;
            st.cancel = None;
            st.expect = None;
            st.last_done = Some(Instant::now());
        });
    }

    async fn paste_flow(&self, trigger: Trigger, rec: &mut Record) -> Result<()> {
        let Some((session, peer_name, info)) = self.best_peer().await else {
            rec.outcome = Outcome::NothingToPaste;
            self.emit(UiEvent::NothingToPaste);
            return Ok(());
        };
        let kind = info.kind.unwrap_or(SlotKind::Text);
        rec.kind = Some(kind);
        rec.bytes = info.size;
        let peer_id = session.peer().id.clone();

        // Confirmation window: the guard against accidental gestures.
        let cancel = Arc::new(Notify::new());
        self.state.lock().unwrap().cancel = Some(cancel.clone());
        self.emit(UiEvent::Countdown { device_id: peer_id.clone(), device_name: peer_name.clone(), kind, secs: self.cfg.cancel_window.as_secs_f32() });
        tokio::select! {
            _ = cancel.notified() => {
                rec.outcome = Outcome::Cancelled;
                rec.misfire = trigger == Trigger::Gesture;
                self.emit(UiEvent::Cancelled { misfire: rec.misfire });
                return Err(Error::Cancelled);
            }
            _ = tokio::time::sleep(self.cfg.cancel_window) => {}
        }
        self.state.lock().unwrap().cancel = None;

        let (done_tx, done_rx) = oneshot::channel();
        let activity = Arc::new(Notify::new());
        self.state.lock().unwrap().expect = Some(Expect {
            device_id: peer_id, kind, count: info.count.max(1), files: Vec::new(), done: Some(done_tx), activity: activity.clone(),
        });
        self.emit(UiEvent::Pulling { device_name: peer_name.clone() });
        session.pull(self.node.addr.port()).await?;

        tokio::pin!(done_rx);
        loop {
            tokio::select! {
                r = &mut done_rx => {
                    let payload = r.map_err(|_| Error::Http("transfer aborted".into()))?;
                    rec.outcome = Outcome::Delivered;
                    self.emit(UiEvent::Deliver { from_name: peer_name, payload });
                    return Ok(());
                }
                _ = activity.notified() => {}
                _ = tokio::time::sleep(self.cfg.idle_timeout) => {
                    return Err(Error::Http("peer did not deliver the content in time".into()));
                }
            }
        }
    }

    /// Ask every reachable paired device what it has ready; pick the freshest.
    async fn best_peer(&self) -> Option<(crate::client::Session, String, SlotInfo)> {
        let mut tasks = Vec::new();
        for dev in self.node.store.list() {
            let Some(addr) = self.node.addr_of(&dev.id).ok() else { continue };
            let Ok(session) = self.node.session(addr, &dev.id) else { continue };
            let timeout = self.cfg.peer_query_timeout;
            tasks.push(tokio::spawn(async move {
                let info = tokio::time::timeout(timeout, session.slot_info()).await.ok()?.ok()?;
                info.available.then_some((session, info))
            }));
        }
        let mut best: Option<(crate::client::Session, SlotInfo)> = None;
        for t in tasks {
            if let Ok(Some((s, i))) = t.await {
                if best.as_ref().is_none_or(|(_, b)| i.age_ms < b.age_ms) {
                    best = Some((s, i));
                }
            }
        }
        let (s, i) = best?;
        let name = s.peer().name.clone();
        Some((s, name, i))
    }

    fn on_incoming(&self, ev: Incoming) {
        match ev {
            Incoming::Progress { name, received, total, from_id, .. } => {
                if let Some(e) = &self.state.lock().unwrap().expect {
                    if e.device_id == from_id {
                        e.activity.notify_one();
                    }
                }
                self.emit(UiEvent::Progress { name, received, total });
            }
            Incoming::Text { from_id, from_name, text } => {
                self.deliver(&from_id, &from_name, SlotKind::Text, Payload::Text(text));
            }
            Incoming::File { from_id, from_name, path, .. } => {
                let mut st = self.state.lock().unwrap();
                if let Some(e) = st.expect.as_mut().filter(|e| e.device_id == from_id && e.kind != SlotKind::Text) {
                    e.activity.notify_one();
                    e.files.push(path.clone());
                    if e.files.len() >= e.count || e.kind == SlotKind::Image {
                        let payload = if e.kind == SlotKind::Image { Payload::Image(path) } else { Payload::Files(std::mem::take(&mut e.files)) };
                        if let Some(tx) = e.done.take() {
                            let _ = tx.send(payload);
                        }
                    }
                } else {
                    drop(st);
                    self.emit(UiEvent::Received { from_name, payload: Payload::Files(vec![path]) });
                }
            }
        }
    }

    fn deliver(&self, from_id: &str, from_name: &str, kind: SlotKind, payload: Payload) {
        let mut st = self.state.lock().unwrap();
        if let Some(e) = st.expect.as_mut().filter(|e| e.device_id == from_id && e.kind == kind) {
            if let Some(tx) = e.done.take() {
                let _ = tx.send(payload);
                return;
            }
        }
        drop(st);
        self.emit(UiEvent::Received { from_name: from_name.to_string(), payload });
    }
}

fn error_category(e: &Error) -> String {
    match e {
        Error::Http(_) => "network",
        Error::Rejected(_) => "rejected",
        Error::PeerNotFound => "peer_not_found",
        Error::NotPaired => "not_paired",
        Error::Checksum(_) => "checksum",
        _ => "other",
    }
    .into()
}
