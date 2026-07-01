//! Cooperative shutdown for daemon background tasks.
//!
//! A [`TaskGroup`] owns a [`CancellationToken`] and the join handles of the
//! long-lived tasks a daemon spawns (disk watchdog, event→handler dispatch,
//! in-flight handler scripts). Shutdown cancels the token and then joins
//! every registered task under one grace deadline, aborting stragglers — so
//! daemon exit is deterministic instead of racing detached tasks.
//!
//! Tasks owned by droppable structures keep their existing semantics
//! (gateway tasks abort when their handle drops, switch ports exit on
//! socket EOF, NAT pumps end when their channel closes); the group is for
//! tasks that would otherwise only stop at `process::exit`.

use std::sync::Mutex;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::sync::LockRecover;

pub struct TaskGroup {
    cancel: CancellationToken,
    tasks: Mutex<Vec<(&'static str, JoinHandle<()>)>>,
}

impl TaskGroup {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// The group's cancellation signal, for tasks that need to select on it
    /// without being registered here.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Spawn a task and register it for joined shutdown. The future should
    /// watch [`Self::cancel_token`] if it loops.
    pub fn spawn<F>(&self, name: &'static str, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.adopt(name, tokio::spawn(fut));
    }

    /// Register an already-spawned task for joined shutdown.
    pub fn adopt(&self, name: &'static str, handle: JoinHandle<()>) {
        let mut tasks = self.tasks.lock_recover();
        // Registrations accumulate over the daemon's lifetime (one per
        // handler run); shed finished entries as we go.
        tasks.retain(|(_, h)| !h.is_finished());
        tasks.push((name, handle));
    }

    /// Cancel the group and join every registered task within `grace`.
    /// Tasks still running at the deadline are aborted (and logged), so the
    /// caller can exit knowing nothing useful was left mid-flight silently.
    pub async fn shutdown(&self, grace: Duration) {
        self.cancel.cancel();
        let tasks: Vec<_> = std::mem::take(&mut *self.tasks.lock_recover());
        let deadline = tokio::time::Instant::now() + grace;
        for (name, handle) in tasks {
            let abort = handle.abort_handle();
            if tokio::time::timeout_at(deadline, handle).await.is_err() {
                tracing::warn!(task = name, "shutdown grace expired; aborting task");
                abort.abort();
            }
        }
    }
}

impl Default for TaskGroup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_joins_cooperative_tasks() {
        let group = TaskGroup::new();
        let token = group.cancel_token();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        group.spawn("looper", async move {
            token.cancelled().await;
            let _ = tx.send(());
        });
        group.shutdown(Duration::from_secs(5)).await;
        rx.await.expect("task observed cancellation before exit");
    }

    #[tokio::test]
    async fn shutdown_survives_a_stuck_task() {
        let group = TaskGroup::new();
        group.spawn("stuck", async {
            futures::future::pending::<()>().await;
        });
        // Must return despite the task never finishing.
        group.shutdown(Duration::from_millis(50)).await;
    }
}
