//! Driving a guest's share mount steps under their [`RetryPolicy`] (PRD
//! §7.5), apart from the guest itself: the caller hands in an `exec` that
//! runs one attempt, so a test drives the same loop with a fake guest on a
//! paused clock.
//!
//! Both loops are plain futures that spawn nothing: dropping one stops it,
//! during an attempt or between two, which is how a machine stop can cancel
//! its mounts.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

use super::mount_report::{MountReport, elapsed_text};
use crate::smb::steps::{MountPlan, RetryPolicy};

/// How much longer than its own timeout an attempt's `exec` may take before
/// the loop stops waiting for it. The agent enforces the timeout in the
/// guest and its answer says more, so it gets the first word; this only
/// bounds a guest that never answers.
const EXEC_GRACE: Duration = Duration::from_secs(5);

/// What one attempt's `exec` came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exited {
    pub exit_code: i32,
    pub stderr: String,
}

/// How one step ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// Attempt `attempt` (0 for the first) succeeded, `elapsed` after the
    /// first started.
    Done { attempt: u32, elapsed: Duration },
    /// The guest refused it with the step's `refused_exit`: not retried.
    Refused { error: String },
    /// Still failing when no attempt could start before the deadline.
    GaveUp {
        attempts: u32,
        elapsed: Duration,
        error: String,
    },
}

/// Run one step until it succeeds, is refused, or the policy's deadline,
/// counted from the first attempt, leaves no room for another attempt.
///
/// `exec` runs one attempt with the timeout it is given:
/// [`RetryPolicy::exec_timeout`] of the time left. `on_failure` hears every
/// failed attempt with its number (0 for the first), the time since the
/// first started, and the error. At least one attempt is always made, and
/// the step ends at most one exec timeout after the deadline.
pub async fn run_step<E, F>(
    policy: &RetryPolicy,
    refused_exit: Option<i32>,
    mut exec: E,
    mut on_failure: impl FnMut(u32, Duration, &str),
) -> StepOutcome
where
    E: FnMut(Duration) -> F,
    F: Future<Output = anyhow::Result<Exited>>,
{
    let started = Instant::now();
    let mut attempt = 0;
    loop {
        let left = policy.deadline.saturating_sub(started.elapsed());
        let timeout = policy.exec_timeout(left);
        let error = match tokio::time::timeout(timeout + EXEC_GRACE, exec(timeout)).await {
            Ok(Ok(r)) if r.exit_code == 0 => {
                return StepOutcome::Done {
                    attempt,
                    elapsed: started.elapsed(),
                };
            }
            Ok(Ok(r)) => {
                let error = format!("exited {}: {}", r.exit_code, r.stderr);
                // The guest said no in a way another attempt will not
                // change; waiting out the deadline only delays saying so.
                if refused_exit == Some(r.exit_code) {
                    on_failure(attempt, started.elapsed(), &error);
                    return StepOutcome::Refused { error };
                }
                error
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("no answer within {}", elapsed_text(timeout)),
        };
        let elapsed = started.elapsed();
        on_failure(attempt, elapsed, &error);
        let delay = policy.delay(attempt);
        if elapsed + delay >= policy.deadline {
            return StepOutcome::GaveUp {
                attempts: attempt + 1,
                elapsed,
                error,
            };
        }
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// Run every step of `plan` in order, recording progress in `report` and
/// handing each `share.unmountable` payload to `unmountable`. `exec` runs
/// one attempt of one step: its argv and timeout.
///
/// A share whose step gave up is not retried by its next step: a mount
/// after a failed `mkdir` only fails the same way, five minutes later, and
/// holds every share behind it up for as long.
pub async fn run_plan<E, F>(
    vm_name: &str,
    plan: &MountPlan,
    report: &MountReport,
    mut exec: E,
    mut unmountable: impl FnMut(serde_json::Value),
) where
    E: FnMut(Vec<String>, Duration) -> F,
    F: Future<Output = anyhow::Result<Exited>>,
{
    let mut given_up: Vec<&str> = Vec::new();
    for (i, step) in plan.steps.iter().enumerate() {
        if step.share.as_deref().is_some_and(|s| given_up.contains(&s)) {
            continue;
        }
        let mut argv = vec![step.command.clone()];
        argv.extend(step.args.iter().cloned());
        let outcome = run_step(
            &plan.retry,
            step.refused_exit,
            |timeout| exec(argv.clone(), timeout),
            |attempt, elapsed, err| {
                tracing::debug!("{vm_name}: mount attempt {attempt} ({elapsed:?}): {err}");
                report.attempt_failed(step.share.as_deref(), attempt, elapsed, err);
            },
        )
        .await;
        let reason = match outcome {
            StepOutcome::Done { attempt, elapsed } => {
                tracing::info!(
                    "{vm_name}: mount step `{}` ok (attempt {attempt}, {elapsed:?})",
                    step.command
                );
                if let Some(share) = step.share.as_deref()
                    && plan.steps[i + 1..]
                        .iter()
                        .all(|s| s.share.as_deref() != Some(share))
                {
                    // The share's last step: it is mounted.
                    report.mounted(share);
                }
                continue;
            }
            StepOutcome::Refused { error } => {
                format!("`{}` refused: {}", step.command, error.trim())
            }
            StepOutcome::GaveUp {
                attempts,
                elapsed,
                error,
            } => format!(
                "`{}` still failing after {} of retries ({attempts} attempts): {}",
                step.command,
                elapsed_text(elapsed),
                error.trim()
            ),
        };
        tracing::warn!("{vm_name}: mount step `{}` failed: {reason}", step.command);
        // A share that never appears is the author's to see, on the feed,
        // not only in the daemon's log.
        unmountable(serde_json::json!({"vm": vm_name, "share": step.share, "reason": reason}));
        report.gave_up(step.share.as_deref(), &reason);
        if let Some(share) = step.share.as_deref() {
            given_up.push(share);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smb::{MountStep, OsHint};
    use std::sync::Mutex;

    const ERROR_67: &str = "System error 67 has occurred.\r\n\r\nThe network name cannot be found.\r\n";

    fn net_use(share: &str, refused_exit: Option<i32>) -> MountStep {
        MountStep {
            os_hint: OsHint::Windows,
            command: "net".into(),
            args: vec!["use".into(), r"\\10.0.0.1\repo".into()],
            share: Some(share.into()),
            refused_exit,
        }
    }

    fn plan(steps: Vec<MountStep>) -> MountPlan {
        MountPlan {
            steps,
            retry: RetryPolicy::MOUNT,
            unsupported: Vec::new(),
        }
    }

    fn report_for(share: &str) -> MountReport {
        let report = MountReport::default();
        report.planned([share], &[], RetryPolicy::MOUNT.deadline);
        report.ready();
        report
    }

    /// Windows' SMB client is still starting for four minutes after boot,
    /// and `net use` fails with error 67 until it is up. The step keeps
    /// retrying through all of it and the share is reported mounted, on a
    /// paused clock that lives through those minutes at once.
    #[tokio::test(start_paused = true)]
    async fn a_four_minute_windows_warm_up_still_mounts() {
        let boot = Instant::now();
        let warm_up = Duration::from_secs(240);
        let report = report_for("c_repo");
        let events = Mutex::new(Vec::new());
        let calls = Mutex::new(0u32);
        run_plan(
            "win",
            &plan(vec![net_use("c_repo", Some(85))]),
            &report,
            |_argv, _timeout| {
                *calls.lock().unwrap() += 1;
                async move {
                    // A failing `net use` still takes a moment.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if boot.elapsed() < warm_up {
                        Ok(Exited {
                            exit_code: 2,
                            stderr: ERROR_67.into(),
                        })
                    } else {
                        Ok(Exited {
                            exit_code: 0,
                            stderr: String::new(),
                        })
                    }
                }
            },
            |event| events.lock().unwrap().push(event),
        )
        .await;
        let took = boot.elapsed();
        assert!(events.lock().unwrap().is_empty(), "{events:?}");
        assert!(report.lines("win", true).is_empty(), "the share is mounted");
        assert!(took >= warm_up && took < warm_up + Duration::from_secs(11), "{took:?}");
        // The curve: 1, 2, 4 and 8 s, then every 10 s.
        assert!(*calls.lock().unwrap() > 20, "{calls:?}");
    }

    /// A step that never succeeds gives up within the deadline plus one
    /// exec timeout, even when every attempt runs its whole timeout, and
    /// reports how long it tried.
    #[tokio::test(start_paused = true)]
    async fn a_step_that_never_succeeds_gives_up_within_the_deadline() {
        let started = Instant::now();
        let report = report_for("c_repo");
        let events = Mutex::new(Vec::new());
        let timeouts = Mutex::new(Vec::new());
        run_plan(
            "win",
            &plan(vec![net_use("c_repo", None)]),
            &report,
            |_argv, timeout| {
                timeouts.lock().unwrap().push(timeout);
                async move {
                    tokio::time::sleep(timeout).await;
                    Err(anyhow::anyhow!("exec timed out after {timeout:?}"))
                }
            },
            |event| events.lock().unwrap().push(event),
        )
        .await;
        let took = started.elapsed();
        let policy = RetryPolicy::MOUNT;
        assert!(took >= policy.deadline - policy.max_delay, "{took:?}");
        assert!(took <= policy.deadline + RetryPolicy::EXEC_TIMEOUT, "{took:?}");
        let timeouts = timeouts.lock().unwrap();
        assert_eq!(timeouts[0], RetryPolicy::EXEC_TIMEOUT);
        assert!(timeouts.iter().all(|t| *t <= RetryPolicy::EXEC_TIMEOUT));
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["share"], "c_repo");
        let reason = events[0]["reason"].as_str().unwrap();
        assert!(reason.contains("still failing after"), "{reason}");
        assert!(reason.contains("exec timed out"), "{reason}");
        let lines = report.lines("win", true);
        assert!(lines[0].contains("will not mount"), "{lines:?}");
    }

    /// A guest that never answers an exec is bounded too: the loop stops
    /// waiting shortly after the attempt's own timeout.
    #[tokio::test(start_paused = true)]
    async fn an_exec_that_never_returns_is_bounded() {
        let started = Instant::now();
        let outcome = run_step(
            &RetryPolicy::MOUNT,
            None,
            |_timeout| std::future::pending::<anyhow::Result<Exited>>(),
            |_, _, _| {},
        )
        .await;
        let StepOutcome::GaveUp { error, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert!(error.contains("no answer within"), "{error}");
        let took = started.elapsed();
        assert!(
            took <= RetryPolicy::MOUNT.deadline + RetryPolicy::EXEC_TIMEOUT,
            "{took:?}"
        );
    }

    /// The step's `refused_exit` stops it at the first refusal, with no
    /// retry and no wait, and the share's later steps are skipped.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_stops_the_step_at_once() {
        let started = Instant::now();
        let report = report_for("c_repo");
        let events = Mutex::new(Vec::new());
        let calls = Mutex::new(0u32);
        run_plan(
            "win",
            &plan(vec![net_use("c_repo", Some(85)), net_use("c_repo", None)]),
            &report,
            |_argv, _timeout| {
                *calls.lock().unwrap() += 1;
                async {
                    Ok(Exited {
                        exit_code: 85,
                        stderr: "a directory is in the way\n".into(),
                    })
                }
            },
            |event| events.lock().unwrap().push(event),
        )
        .await;
        assert_eq!(*calls.lock().unwrap(), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        let reason = events[0]["reason"].as_str().unwrap();
        assert_eq!(reason, "`net` refused: exited 85: a directory is in the way");
    }

    /// Dropping the loop stops it: no attempt runs after the drop.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_loop_stops_it() {
        let calls = std::sync::Arc::new(Mutex::new(0u32));
        let step = {
            let calls = std::sync::Arc::clone(&calls);
            run_step(
                &RetryPolicy::MOUNT,
                None,
                move |_timeout| {
                    *calls.lock().unwrap() += 1;
                    async {
                        Ok(Exited {
                            exit_code: 2,
                            stderr: ERROR_67.into(),
                        })
                    }
                },
                |_, _, _| {},
            )
        };
        let _ = tokio::time::timeout(Duration::from_secs(20), step).await;
        let seen = *calls.lock().unwrap();
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(*calls.lock().unwrap(), seen, "no attempt after the drop");
        assert!(seen > 1);
    }
}
