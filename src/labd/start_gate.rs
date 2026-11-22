//! Starts against stops and destroys: what keeps a machine from booting
//! behind the operation that was meant to end it.
//!
//! A machine's own power state cannot do this. A stop of a machine whose
//! start has not spawned an emulator yet finds nothing to stop and returns,
//! and requests are served concurrently, so a start can run between a
//! destroy's stop and its removal of the disks. Three rules close that, and
//! every start, `down` and destroy of the lab runtime goes through them:
//!
//! - **A stop makes the starts before it stale.** A start carries a
//!   [`Ticket`] taken when its request began — for `up`, once for all its
//!   waves. A `down` or a destroy of a machine refuses every ticket older
//!   than itself for that machine, so an `up` under way boots nothing after
//!   the `down` that crossed it.
//! - **One machine, one of them at a time.** A start holds the machine's
//!   lock from the check of its ticket until the machine has started. A stop
//!   takes the lock before it stops, so it never acts on a start that has
//!   nothing to stop yet, and a destroy holds it until the disks are gone.
//! - **A lab that is going away takes no start.** The daemon's teardown and
//!   a lab destroy [`close`](StartGate::close) the gate: every ticket is
//!   refused, old or new.
//!
//! The order is always: mark the stop, then take the lock. A start that
//! passed its check did so under the lock, so the stop waits for it and then
//! stops it. A start that takes the lock later sees the mark.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::proto::CommandError;
use crate::sync::LockRecover;

/// When a start was asked for, on the gate's own counter.
#[derive(Debug, Clone, Copy)]
pub(super) struct Ticket(u64);

#[derive(Default)]
pub(super) struct StartGate {
    inner: std::sync::Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Counts stop requests. A ticket is the count when it was taken.
    clock: u64,
    /// Why the lab takes no start at all, while it does not.
    closed: Option<&'static str>,
    /// The count at the last stop of the whole lab.
    lab_stopped: u64,
    /// The count at the last stop or destroy of each machine.
    stopped: HashMap<String, u64>,
    locks: HashMap<String, Arc<Mutex<()>>>,
}

impl Inner {
    fn refusal(&self, name: &str, ticket: Ticket) -> Option<CommandError> {
        if let Some(why) = self.closed {
            return Some(CommandError::conflict(format!(
                "the lab is {why}: \"{name}\" was not started"
            )));
        }
        let stopped = self
            .stopped
            .get(name)
            .copied()
            .unwrap_or(0)
            .max(self.lab_stopped);
        (ticket.0 < stopped).then(|| {
            CommandError::conflict(format!(
                "\"{name}\" was stopped or destroyed while its start was under way, \
                 and was not started"
            ))
        })
    }
}

impl StartGate {
    /// The ticket of a start request that begins now. Refused while the lab
    /// is closed.
    pub(super) fn ticket(&self) -> Result<Ticket> {
        let inner = self.inner.lock_recover();
        if let Some(why) = inner.closed {
            return Err(CommandError::conflict(format!(
                "the lab is {why}: nothing can be started"
            ))
            .into());
        }
        Ok(Ticket(inner.clock))
    }

    /// Take `name`'s lock for a start, and refuse the start when the lab is
    /// closed or the machine was stopped or destroyed since `ticket`.
    /// Checked under the lock: whoever stops the machine next waits for the
    /// guard, so it finds the machine this start made.
    pub(super) async fn admit(&self, name: &str, ticket: Ticket) -> Result<OwnedMutexGuard<()>> {
        let guard = self.hold(name).await;
        match self.inner.lock_recover().refusal(name, ticket) {
            Some(refusal) => Err(refusal.into()),
            None => Ok(guard),
        }
    }

    /// Make every start of these machines that was asked for before now
    /// stale.
    pub(super) fn stop_requested<'a>(&self, names: impl IntoIterator<Item = &'a str>) {
        let mut inner = self.inner.lock_recover();
        inner.clock += 1;
        let now = inner.clock;
        for name in names {
            inner.stopped.insert(name.to_string(), now);
        }
    }

    /// [`stop_requested`](Self::stop_requested) for every machine of the lab.
    pub(super) fn lab_stop_requested(&self) {
        let mut inner = self.inner.lock_recover();
        inner.clock += 1;
        inner.lab_stopped = inner.clock;
    }

    /// Refuse every start from now on, with `why` in the refusal ("shutting
    /// down"). Starts already asked for go stale as well.
    pub(super) fn close(&self, why: &'static str) {
        self.lab_stop_requested();
        self.inner.lock_recover().closed = Some(why);
    }

    /// Take starts again, after an operation that closed the lab failed and
    /// left it standing. Starts asked for before the close stay stale.
    pub(super) fn reopen(&self) {
        self.inner.lock_recover().closed = None;
    }

    /// `name`'s lock, waiting for a start of it that is under way. A stop
    /// takes and drops it before it stops the machine; a destroy keeps it
    /// until the disks are removed.
    pub(super) async fn hold(&self, name: &str) -> OwnedMutexGuard<()> {
        self.lock_of(name).lock_owned().await
    }

    /// `name`'s lock itself, for a stop that runs in a task of its own.
    pub(super) fn lock_of(&self, name: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.inner
                .lock_recover()
                .locks
                .entry(name.to_string())
                .or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stop_refuses_the_starts_asked_for_before_it_and_no_others() {
        let gate = StartGate::default();
        let before = gate.ticket().unwrap();
        gate.stop_requested(["a"]);
        let after = gate.ticket().unwrap();

        let refused = gate.admit("a", before).await.unwrap_err();
        assert!(
            refused.to_string().contains("\"a\" was stopped"),
            "{refused}"
        );
        assert!(
            gate.admit("b", before).await.is_ok(),
            "\"b\" was not stopped"
        );
        assert!(gate.admit("a", after).await.is_ok(), "a later start");
    }

    #[tokio::test]
    async fn a_lab_stop_refuses_the_earlier_starts_of_every_machine() {
        let gate = StartGate::default();
        let before = gate.ticket().unwrap();
        gate.lab_stop_requested();
        assert!(gate.admit("a", before).await.is_err());
        assert!(gate.admit("b", before).await.is_err());
        assert!(gate.admit("a", gate.ticket().unwrap()).await.is_ok());
    }

    #[tokio::test]
    async fn a_closed_lab_takes_no_start_until_it_is_reopened() {
        let gate = StartGate::default();
        let before = gate.ticket().unwrap();
        gate.close("shutting down");
        let refused = gate.ticket().unwrap_err();
        assert!(refused.to_string().contains("shutting down"), "{refused}");
        assert!(gate.admit("a", before).await.is_err());

        gate.reopen();
        assert!(gate.admit("a", before).await.is_err(), "still stale");
        assert!(gate.admit("a", gate.ticket().unwrap()).await.is_ok());
    }

    #[tokio::test]
    async fn a_start_that_was_admitted_keeps_a_stop_waiting() {
        let gate = StartGate::default();
        let admitted = gate.admit("a", gate.ticket().unwrap()).await.unwrap();
        gate.stop_requested(["a"]);
        assert!(gate.lock_of("a").try_lock().is_err(), "the stop must wait");
        drop(admitted);
        assert!(gate.lock_of("a").try_lock().is_ok());
    }
}
