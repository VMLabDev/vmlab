//! Shared QMP data types.

use serde::Deserialize;
use serde_json::Value;

/// An asynchronous QMP event, e.g. `SHUTDOWN`, `STOP`, `RESET`.
#[derive(Debug, Clone)]
pub struct QmpEvent {
    /// Event name as emitted by QEMU (e.g. `"SHUTDOWN"`).
    pub event: String,
    /// Event payload (`data` member); `Value::Null` when absent.
    pub data: Value,
    /// Host-side timestamp QEMU attached to the event. No consumer reads
    /// it today; carried so the event mirrors the wire format.
    #[allow(dead_code)]
    pub timestamp: EventTimestamp,
}

/// Timestamp attached to QMP events (`{"seconds": ..., "microseconds": ...}`).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[allow(dead_code)]
pub struct EventTimestamp {
    #[serde(default)]
    pub seconds: i64,
    #[serde(default)]
    pub microseconds: i64,
}

/// VM run state as reported by `query-status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Running,
    Paused,
    Shutdown,
    Suspended,
    PreLaunch,
    InternalError,
    IoError,
    Watchdog,
    GuestPanicked,
    FinishMigrate,
    PostMigrate,
    RestoreVm,
    SaveVm,
    Debug,
    /// Any state this enum does not name explicitly; carries the raw
    /// `status` string from QEMU.
    Other(String),
}

impl RunState {
    /// Map a `query-status` `status` string onto a [`RunState`].
    pub fn from_status(status: &str) -> Self {
        match status {
            "running" => RunState::Running,
            "paused" => RunState::Paused,
            "shutdown" => RunState::Shutdown,
            "suspended" => RunState::Suspended,
            "prelaunch" => RunState::PreLaunch,
            "internal-error" => RunState::InternalError,
            "io-error" => RunState::IoError,
            "watchdog" => RunState::Watchdog,
            "guest-panicked" => RunState::GuestPanicked,
            "finish-migrate" => RunState::FinishMigrate,
            "postmigrate" => RunState::PostMigrate,
            "restore-vm" => RunState::RestoreVm,
            "save-vm" => RunState::SaveVm,
            "debug" => RunState::Debug,
            other => RunState::Other(other.to_string()),
        }
    }
}
