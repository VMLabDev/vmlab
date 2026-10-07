//! What `up` says about a machine's share mounts before it calls the lab up
//! (PRD §7.5).
//!
//! The mounts run in a task of their own, outside the wave, because a mount
//! step may retry for minutes. That task records each share's progress in a
//! [`MountReport`]; `up` waits a bounded [`MOUNT_REPORT_HOLD`] for the tasks
//! to finish and then prints [`MountReport::lines`] — a warning for every
//! share that will not mount, and one for every share still failing when the
//! wait ran out, so neither is left for the user to discover by using it.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

/// How long `up`, once everything else is done, waits for the mount tasks to
/// finish. Long enough for a ready Linux guest's mounts, which take a second;
/// a share still retrying at the end of it is reported as such, never dropped.
pub const MOUNT_REPORT_HOLD: Duration = Duration::from_secs(30);

/// One machine's share mounts as far as they have got.
#[derive(Debug, Default)]
pub struct MountReport {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Whether the guest has become ready, which is when mounting starts.
    ready: bool,
    /// The retry policy's attempt count, for "attempt 3 of 30".
    attempts: u32,
    shares: BTreeMap<String, ShareState>,
    /// Refusals that name no share of their own: a plan note, or a step
    /// shared by every share (the WinFsp registration).
    notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShareState {
    /// Not mounted yet; `last` is the most recent failure, if any.
    Mounting {
        attempt: u32,
        last: Option<String>,
    },
    Mounted,
    Unmountable(String),
}

impl MountReport {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `shares` are about to be mounted and `unsupported` never will be.
    /// Replaces the shares a previous call named: before readiness they are
    /// the declared names, after it the plan's own (a virtiofs mount tag),
    /// which are what a `share.unmountable` event names too.
    pub fn planned<'a>(
        &self,
        shares: impl IntoIterator<Item = &'a str>,
        unsupported: &[String],
        attempts: u32,
    ) {
        let mut inner = self.lock();
        inner.attempts = attempts;
        inner.shares = shares
            .into_iter()
            .map(|share| {
                (
                    share.to_string(),
                    ShareState::Mounting {
                        attempt: 0,
                        last: None,
                    },
                )
            })
            .collect();
        inner.notes.extend(unsupported.iter().cloned());
    }

    /// The guest answered; the mount steps start now.
    pub fn ready(&self) {
        self.lock().ready = true;
    }

    /// An attempt at one of `share`'s steps failed and will be retried.
    pub fn attempt_failed(&self, share: Option<&str>, attempt: u32, err: &str) {
        let Some(share) = share else { return };
        if let Some(ShareState::Mounting { attempt: a, last }) = self.lock().shares.get_mut(share) {
            *a = attempt + 1;
            *last = Some(err.trim().to_string());
        }
    }

    /// Every step of `share` succeeded.
    pub fn mounted(&self, share: &str) {
        self.lock()
            .shares
            .insert(share.to_string(), ShareState::Mounted);
    }

    /// A step gave up: `share` will not mount, or with no share, none of the
    /// shares behind it will.
    pub fn gave_up(&self, share: Option<&str>, reason: &str) {
        let mut inner = self.lock();
        match share {
            Some(share) => {
                inner.shares.insert(
                    share.to_string(),
                    ShareState::Unmountable(reason.to_string()),
                );
            }
            None => inner.notes.push(reason.to_string()),
        }
    }

    /// Nothing can mount at all — the guest has no agent to run the steps
    /// through. Every share not yet settled is unmountable for `reason`.
    pub fn abandoned(&self, reason: &str) {
        for state in self.lock().shares.values_mut() {
            if matches!(state, ShareState::Mounting { .. }) {
                *state = ShareState::Unmountable(reason.to_string());
            }
        }
    }

    /// What `up` prints for `machine`: nothing when every share mounted.
    /// `finished` says whether the mount task ended; when it has not, a share
    /// still mounting is reported as still being tried rather than dropped.
    pub fn lines(&self, machine: &str, finished: bool) -> Vec<String> {
        let inner = self.lock();
        let mut out: Vec<String> = inner
            .notes
            .iter()
            .map(|note| format!("WARNING: \"{machine}\": share will not mount: {note}\n"))
            .collect();
        for (share, state) in &inner.shares {
            match state {
                ShareState::Mounted => {}
                ShareState::Unmountable(reason) => out.push(format!(
                    "WARNING: \"{machine}\": share \"{share}\" will not mount: {reason}\n"
                )),
                // Every way out of the mount task settles its shares, so
                // this is a path nobody foresaw: say so rather than nothing.
                ShareState::Mounting { .. } if finished => out.push(format!(
                    "WARNING: \"{machine}\": share \"{share}\" was not mounted; see `vmlab logs`\n"
                )),
                ShareState::Mounting { attempt, last } => out.push(match (inner.ready, last) {
                    (false, _) => format!(
                        "\"{machine}\": share \"{share}\" not mounted yet: the guest is not \
                         ready; it mounts once it is, and a share that cannot is reported \
                         in `vmlab logs`\n"
                    ),
                    (true, None) => format!(
                        "\"{machine}\": share \"{share}\" still mounting; a share that \
                         cannot mount is reported in `vmlab logs`\n"
                    ),
                    (true, Some(err)) => format!(
                        "WARNING: \"{machine}\": share \"{share}\" not mounted yet, still \
                         retrying (attempt {attempt} of {}): {err}\n",
                        inner.attempts
                    ),
                }),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(shares: &[&str]) -> MountReport {
        let r = MountReport::default();
        r.planned(shares.iter().copied(), &[], 30);
        r
    }

    /// The common case adds nothing to `up`'s output.
    #[test]
    fn every_share_mounted_says_nothing() {
        let r = report(&["src", "docs"]);
        r.ready();
        r.mounted("src");
        r.mounted("docs");
        assert!(r.lines("lin01", true).is_empty());
    }

    /// A share that gave up names the machine, the share and the reason.
    #[test]
    fn a_share_that_gave_up_is_a_warning_naming_machine_share_and_reason() {
        let r = report(&["src", "docs"]);
        r.ready();
        r.mounted("src");
        r.gave_up(Some("docs"), "`mount` refused: exited 64: not empty");
        assert_eq!(
            r.lines("win", true),
            [
                "WARNING: \"win\": share \"docs\" will not mount: `mount` refused: exited 64: not empty\n"
            ]
        );
    }

    /// A share still retrying when `up` stops waiting is reported with its
    /// last error, not dropped.
    #[test]
    fn a_share_still_retrying_at_the_bound_is_reported_with_its_last_error() {
        let r = report(&["src"]);
        r.ready();
        r.attempt_failed(Some("src"), 2, "exited 1: mkdir: can't create directory\n");
        let lines = r.lines("lin01", false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("WARNING: \"lin01\": share \"src\""),
            "{lines:?}"
        );
        assert!(
            lines[0].contains("still retrying (attempt 3 of 30)"),
            "{lines:?}"
        );
        assert!(lines[0].contains("can't create directory\n"), "{lines:?}");
    }

    /// Mounting that has not failed yet is said, without a warning.
    #[test]
    fn a_share_not_yet_tried_is_a_note_not_a_warning() {
        let r = report(&["src"]);
        let lines = r.lines("lin01", false);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("the guest is not ready"), "{lines:?}");
        assert!(!lines[0].starts_with("WARNING"), "{lines:?}");
        r.ready();
        assert!(r.lines("lin01", false)[0].contains("still mounting"));
    }

    /// Plan notes and share-less steps that gave up are warnings too.
    #[test]
    fn notes_and_shareless_failures_are_warnings() {
        let r = MountReport::default();
        r.planned(["v"], &["virtiofs share \"v\": no WinFsp".to_string()], 30);
        r.gave_up(None, "`reg` still failing after 30 attempts: denied");
        r.abandoned("no agent to mount through");
        let lines = r.lines("xp", true);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines.iter().all(|l| l.starts_with("WARNING: \"xp\"")),
            "{lines:?}"
        );
        assert!(
            lines[2].contains("share \"v\" will not mount: no agent"),
            "{lines:?}"
        );
    }
}
