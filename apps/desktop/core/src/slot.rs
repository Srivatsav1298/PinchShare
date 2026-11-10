//! The "ready to send" slot: what this device has copied and will hand to a peer that pulls it.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub enum SlotContent {
    Text(String),
    /// PNG-encoded image.
    Image(Vec<u8>),
    Files(Vec<PathBuf>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    Text,
    Image,
    Files,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Gesture,
    Hotkey,
    Tray,
    Button,
}

impl SlotContent {
    pub fn kind(&self) -> SlotKind {
        match self {
            SlotContent::Text(_) => SlotKind::Text,
            SlotContent::Image(_) => SlotKind::Image,
            SlotContent::Files(_) => SlotKind::Files,
        }
    }
}

pub struct ReadySlot {
    pub content: SlotContent,
    pub created: Instant,
    pub expires: Instant,
    pub trigger: Trigger,
    pub generation: u64,
}

impl ReadySlot {
    pub fn new(content: SlotContent, ttl: Duration, trigger: Trigger, generation: u64) -> ReadySlot {
        let now = Instant::now();
        ReadySlot { content, created: now, expires: now + ttl, trigger, generation }
    }
    pub fn fresh(&self) -> bool {
        Instant::now() < self.expires
    }
}

/// Metadata about a slot, safe to share with paired peers (never includes content).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SlotInfo {
    pub available: bool,
    pub kind: Option<SlotKind>,
    pub count: usize,
    pub size: u64,
    pub age_ms: u64,
}

impl SlotInfo {
    pub fn none() -> SlotInfo {
        SlotInfo { available: false, kind: None, count: 0, size: 0, age_ms: 0 }
    }

    pub fn of(slot: &ReadySlot) -> SlotInfo {
        let (count, size) = match &slot.content {
            SlotContent::Text(t) => (1, t.len() as u64),
            SlotContent::Image(b) => (1, b.len() as u64),
            SlotContent::Files(p) => (p.len(), p.iter().filter_map(|f| std::fs::metadata(f).ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()),
        };
        SlotInfo { available: true, kind: Some(slot.content.kind()), count, size, age_ms: slot.created.elapsed().as_millis() as u64 }
    }
}

#[derive(Serialize, Deserialize)]
pub struct PullReq {
    /// Port of the requester's own PinchShare server; the host pushes content there.
    pub port: u16,
}
