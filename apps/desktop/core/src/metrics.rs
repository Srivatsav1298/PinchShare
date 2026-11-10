//! Local pilot instrumentation: one JSON line per paste attempt. Never contains content.
//! Stays on this device; the user can export it voluntarily (Phase 5).
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::slot::{SlotKind, Trigger};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Delivered,
    /// User hit Cancel inside the confirmation window.
    Cancelled,
    Failed,
    NothingToPaste,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub ts_unix: u64,
    pub kind: Option<SlotKind>,
    pub bytes: u64,
    pub duration_ms: u64,
    pub trigger: Trigger,
    pub outcome: Outcome,
    /// Gesture trigger that was cancelled within the confirmation window.
    pub misfire: bool,
    /// Short error category only (never file names or content).
    pub error: Option<String>,
}

pub struct Metrics {
    path: Option<PathBuf>,
    lock: Mutex<()>,
}

impl Metrics {
    pub fn new(path: Option<PathBuf>) -> Metrics {
        Metrics { path, lock: Mutex::new(()) }
    }

    pub fn record(&self, r: &Record) {
        let Some(p) = &self.path else { return };
        let _g = self.lock.lock().unwrap();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{}", serde_json::to_string(r).unwrap_or_default());
        }
    }

    pub fn read_all(&self) -> Vec<Record> {
        let Some(p) = &self.path else { return Vec::new() };
        std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }
}
