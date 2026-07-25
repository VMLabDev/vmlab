//! Event emission + JSON-lines event history (PRD §8.1, §8.3). Every event
//! goes three places: the daemon's broadcast stream (CLI subscribers + the
//! supervisor's aggregate), the lab's event-history file, and the tracing
//! log.

use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::Value;

use crate::logs::AppendLog;
use crate::proto::Event;
use crate::sync::LockRecover;

pub struct EventLog {
    lab: String,
    file: Mutex<AppendLog>,
    tx: tokio::sync::broadcast::Sender<Event>,
}

impl EventLog {
    /// `~/.local/state/vmlab/labs/<lab>/events.jsonl`
    pub fn history_path(lab: &str) -> PathBuf {
        crate::paths::state_dir()
            .join("labs")
            .join(lab)
            .join("events.jsonl")
    }

    pub fn new(lab: &str, tx: tokio::sync::broadcast::Sender<Event>) -> anyhow::Result<Self> {
        Ok(Self {
            lab: lab.to_string(),
            // Rotating: event history is append-only for the life of a lab.
            file: Mutex::new(AppendLog::open(Self::history_path(lab))),
            tx,
        })
    }

    pub fn emit(&self, event: &str, data: Value) {
        let ev = Event::new(event, self.lab.clone(), data);
        tracing::info!(event = %ev.event, data = %ev.data, "event");
        if let Ok(line) = serde_json::to_string(&ev) {
            // `lock_recover`, not `if let Ok(..)`: a panic anywhere else in the
            // daemon while this lock was held would otherwise silently end
            // event history for the rest of the daemon's life.
            self.file.lock_recover().write_line(&line);
        }
        let _ = self.tx.send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_writes_history_and_broadcasts() {
        let tmp = tempfile::tempdir().unwrap();
        // Redirect state dir via env is global; instead test the file side
        // through a custom path by constructing manually.
        let path = tmp.path().join("events.jsonl");
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let log = EventLog {
            lab: "t".into(),
            file: Mutex::new(AppendLog::open(path.clone())),
            tx,
        };
        log.emit("vm.ready", serde_json::json!({"vm": "a"}));
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event, "vm.ready");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("vm.ready"));
        let parsed: Event = serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(parsed.lab, "t");
    }
}
