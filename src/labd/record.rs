//! Console recordings a lab daemon owns (§11.1, D5–D7, D14, D20): one per
//! machine, started from the CLI or a script, finalised by whoever ends the
//! machine or the run that started it.
//!
//! The daemon, not the CLI, owns a recording, so it outlives the verb that
//! started it and is always finalised to a playable file. [`Recordings`] is
//! the registry — shaped like the workspace syncers, one live entry per
//! machine — and the `impl LabRuntime` below is every verb over it. The
//! recorder itself knows nothing of a lab ([`crate::record`]); what this
//! module adds is who may record what, where the file goes by default, which
//! `ffmpeg` to run, and when a recording ends on the lab's account.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use serde_json::json;
use tokio::sync::Mutex;

use super::lab::LabRuntime;
use crate::proto::CommandError;
use crate::record::{
    InputTap, RecordOptions, RecordRequest, RecordStopped, Recorder, RecorderSpec, RecordingStatus,
    Settings, default_output, encoder,
};

/// A script run, so the recordings it started can be finalised when it ends
/// (D7). Allocated per run by [`Recordings::new_run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(u64);

/// A recording checked and ready to start: everything decided, no socket
/// opened yet. What `--record` on a start verb holds until the emulator is
/// up and paused (D20).
#[derive(Debug)]
pub struct Prepared {
    pub machine: String,
    pub output: PathBuf,
    pub settings: Settings,
    pub ffmpeg: PathBuf,
    pub owner: Option<RunId>,
}

struct Live {
    recorder: Recorder,
    path: PathBuf,
    started_at: chrono::DateTime<chrono::Utc>,
    settings: Settings,
    owner: Option<RunId>,
    tap: Arc<InputTap>,
}

/// The lab's live recordings, keyed by machine, plus the ones armed for a
/// machine about to boot.
#[derive(Default)]
pub struct Recordings {
    live: Mutex<HashMap<String, Live>>,
    pending: std::sync::Mutex<HashMap<String, Prepared>>,
    runs: AtomicU64,
}

impl Recordings {
    pub fn new_run(&self) -> RunId {
        RunId(self.runs.fetch_add(1, Ordering::SeqCst) + 1)
    }

    async fn is_live(&self, machine: &str) -> Option<PathBuf> {
        self.live.lock().await.get(machine).map(|l| l.path.clone())
    }

    #[cfg(test)]
    pub async fn live_machines(&self) -> Vec<String> {
        self.live.lock().await.keys().cloned().collect()
    }

    /// Whether a recording is armed for `machine`'s next boot.
    pub fn is_pending(&self, machine: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(machine)
    }
}

impl LabRuntime {
    /// Decide everything a recording needs before touching the machine:
    /// that it has a display (a container refuses, `unsupported`), that it is
    /// not already recording (`conflict`, naming the live file), that the
    /// path, size and rate are acceptable (`invalid_argument`), and that an
    /// ffmpeg with the format's encoder is on this host (`failed`).
    pub async fn record_prepare(
        &self,
        machine: &str,
        path: Option<&str>,
        options: &RecordOptions,
        owner: Option<RunId>,
    ) -> Result<Prepared> {
        let m = self.machine(machine)?;
        if m.display().is_none() {
            return Err(
                CommandError::unsupported(format!("machine `{machine}` has no display")).into(),
            );
        }
        if let Some(live) = self.recordings.is_live(machine).await {
            return Err(CommandError::conflict(format!(
                "machine `{machine}` is already recording to {}; stop it first (`vmlab record stop {machine}`)",
                live.display()
            ))
            .into());
        }
        if self.recordings.is_pending(machine) {
            return Err(CommandError::conflict(format!(
                "machine `{machine}` already has a recording waiting to start with it"
            ))
            .into());
        }
        let output = match path {
            Some(p) => {
                let p = PathBuf::from(p);
                if !p.is_absolute() {
                    return Err(CommandError::invalid(format!(
                        "recording path `{}` must be absolute by the time it reaches the daemon",
                        p.display()
                    ))
                    .into());
                }
                p
            }
            None => default_output(&self.lab_local, machine),
        };
        let settings =
            Settings::resolve(&output, options).map_err(|e| anyhow!(CommandError::invalid(e)))?;
        let avail = encoder::probe(self.host_cfg.ffmpeg.as_deref()).await;
        let ffmpeg = encoder::check(&avail, settings.format)
            .map_err(|e| anyhow!(CommandError::failed(e)))?;
        Ok(Prepared {
            machine: machine.to_string(),
            output,
            settings,
            ffmpeg,
            owner,
        })
    }

    /// Start a recording on a running machine now.
    pub async fn record_start(
        &self,
        machine: &str,
        path: Option<&str>,
        options: &RecordOptions,
        owner: Option<RunId>,
    ) -> Result<RecordingStatus> {
        let prepared = self.record_prepare(machine, path, options, owner).await?;
        self.record_launch(prepared).await
    }

    /// Start a prepared recording: connect to the machine's VNC socket, run
    /// ffmpeg, register the entry.
    async fn record_launch(&self, prepared: Prepared) -> Result<RecordingStatus> {
        let Prepared {
            machine,
            output,
            settings,
            ffmpeg,
            owner,
        } = prepared;
        let display = self.machine(&machine)?.display().ok_or_else(|| {
            CommandError::unsupported(format!("machine `{machine}` has no display"))
        })?;
        let tap = display.input_tap();
        let mut live = self.recordings.live.lock().await;
        if let Some(existing) = live.get(&machine) {
            return Err(CommandError::conflict(format!(
                "machine `{machine}` is already recording to {}",
                existing.path.display()
            ))
            .into());
        }
        let inputs = tap.attach();
        let recorder = Recorder::start(RecorderSpec {
            sock: display.vnc_sock(),
            output: output.clone(),
            settings: settings.clone(),
            ffmpeg,
            inputs,
        })
        .await
        .map_err(|e| {
            tap.detach();
            anyhow!(CommandError::failed(format!(
                "starting the recording of `{machine}`: {e:#}"
            )))
        })?;
        let started_at = chrono::Utc::now();
        live.insert(
            machine.clone(),
            Live {
                recorder,
                path: output.clone(),
                started_at,
                settings: settings.clone(),
                owner,
                tap,
            },
        );
        drop(live);
        self.events.emit(
            "record.started",
            json!({"vm": machine, "path": output, "options": settings.options()}),
        );
        Ok(RecordingStatus {
            machine,
            path: output,
            started_at,
            elapsed_secs: 0.0,
            options: settings.options(),
        })
    }

    /// Finalise `machine`'s recording: close ffmpeg's stdin, let it write
    /// the trailer, wait for it. `not_found` when nothing is recording.
    pub async fn record_stop(&self, machine: &str) -> Result<RecordStopped> {
        let live = self.recordings.live.lock().await.remove(machine);
        let Some(live) = live else {
            return Err(
                CommandError::not_found(format!("machine `{machine}` is not recording")).into(),
            );
        };
        self.record_finalise(machine, live, "stopped").await
    }

    async fn record_finalise(&self, machine: &str, live: Live, why: &str) -> Result<RecordStopped> {
        live.tap.detach();
        let result = live.recorder.stop().await;
        match result {
            Ok(done) => {
                self.events.emit(
                    "record.stopped",
                    json!({
                        "vm": machine,
                        "path": done.path,
                        "duration_secs": done.duration.as_secs_f64(),
                        "reason": done.ended_by.clone().unwrap_or_else(|| why.to_string()),
                    }),
                );
                Ok(RecordStopped {
                    machine: machine.to_string(),
                    path: done.path,
                    duration_secs: done.duration.as_secs_f64(),
                    timeline: done.timeline,
                    ended_by: done.ended_by,
                })
            }
            Err(e) => {
                self.events.emit(
                    "record.stopped",
                    json!({"vm": machine, "path": live.path, "error": format!("{e:#}")}),
                );
                Err(anyhow!(CommandError::failed(format!(
                    "finalising the recording of `{machine}` ({}): {e:#}",
                    live.path.display()
                ))))
            }
        }
    }

    /// Finalise `machine`'s recording if it has one; nothing otherwise. What
    /// `vm stop`, `destroy` of one machine and `down` call before the machine
    /// goes (D6).
    pub async fn record_finalise_machine(&self, machine: &str, why: &str) {
        let live = self.recordings.live.lock().await.remove(machine);
        if let Some(live) = live
            && let Err(e) = self.record_finalise(machine, live, why).await
        {
            tracing::warn!("{machine}: {e:#}");
        }
    }

    /// Finalise every live recording in the lab: `destroy`, a full `down`,
    /// the daemon leaving, the disk watchdog tripping (D14).
    pub async fn record_finalise_all(&self, why: &str) {
        let all: Vec<String> = self.recordings.live.lock().await.keys().cloned().collect();
        for machine in all {
            self.record_finalise_machine(&machine, why).await;
        }
    }

    /// Finalise whatever `run` started and did not stop itself (D7).
    pub async fn record_finalise_run(&self, run: RunId) {
        let owned: Vec<String> = self
            .recordings
            .live
            .lock()
            .await
            .iter()
            .filter(|(_, l)| l.owner == Some(run))
            .map(|(m, _)| m.clone())
            .collect();
        for machine in owned {
            self.record_finalise_machine(&machine, "script run ended")
                .await;
        }
    }

    /// Every live recording, or `machine`'s alone. A recording that ended on
    /// its own (its maximum duration, an encoder failure) is finalised here
    /// and no longer reported.
    pub async fn record_status(&self, machine: Option<&str>) -> Vec<RecordingStatus> {
        let finished: Vec<String> = self
            .recordings
            .live
            .lock()
            .await
            .iter()
            .filter(|(_, l)| l.recorder.is_finished())
            .map(|(m, _)| m.clone())
            .collect();
        for m in finished {
            self.record_finalise_machine(&m, "ended").await;
        }
        let now = chrono::Utc::now();
        let mut out: Vec<RecordingStatus> = self
            .recordings
            .live
            .lock()
            .await
            .iter()
            .filter(|(m, _)| machine.is_none_or(|want| want == m.as_str()))
            .map(|(m, l)| RecordingStatus {
                machine: m.clone(),
                path: l.path.clone(),
                started_at: l.started_at,
                elapsed_secs: (now - l.started_at)
                    .to_std()
                    .unwrap_or_default()
                    .as_secs_f64(),
                options: l.settings.options(),
            })
            .collect();
        out.sort_by(|a, b| a.machine.cmp(&b.machine));
        out
    }

    /// Check every `--record` a start verb carries and hold each until its
    /// machine boots (D20). All or nothing: one refusal arms none.
    pub async fn record_arm(&self, requests: &[RecordRequest], owner: Option<RunId>) -> Result<()> {
        let mut prepared = Vec::with_capacity(requests.len());
        for r in requests {
            if prepared.iter().any(|p: &Prepared| p.machine == r.machine) {
                return Err(CommandError::invalid(format!(
                    "machine `{}` is named by more than one --record",
                    r.machine
                ))
                .into());
            }
            prepared.push(
                self.record_prepare(&r.machine, r.path.as_deref(), &r.options, owner)
                    .await?,
            );
        }
        let mut pending = self
            .recordings
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for p in prepared {
            pending.insert(p.machine.clone(), p);
        }
        Ok(())
    }

    /// The paused-window hook (D20): a recording armed for `machine` starts
    /// now, before its CPUs run.
    pub(super) async fn record_at_resume(&self, machine: &str) -> Result<()> {
        let prepared = self
            .recordings
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(machine);
        if let Some(p) = prepared {
            self.record_launch(p).await?;
        }
        Ok(())
    }

    /// After a start verb returns: anything still armed belongs to a machine
    /// that did not boot (it was already running, or the start failed). A
    /// running machine is recorded from now; otherwise the request is dropped
    /// and reported.
    pub async fn record_settle(&self, machines: &[String]) -> Result<()> {
        let leftovers: Vec<Prepared> = {
            let mut pending = self
                .recordings
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            machines.iter().filter_map(|m| pending.remove(m)).collect()
        };
        for p in leftovers {
            let running = match self.machine(&p.machine) {
                Ok(m) => m.state().await == super::vm::PowerState::Running,
                Err(_) => false,
            };
            if running {
                self.record_launch(p).await?;
            } else {
                return Err(CommandError::failed(format!(
                    "machine `{}` did not start, so its recording was not started",
                    p.machine
                ))
                .into());
            }
        }
        Ok(())
    }
}
