//! The guest clock at every handshake (§7.3, #186).
//!
//! A machine whose `clock` is `"host"` should run at the host's time, and at
//! a cold boot it does: QEMU starts the RTC at the host's UTC time. A reboot
//! inside the guest keeps whatever the guest last wrote to the RTC, though,
//! and a Windows guest whose agent first set `RealTimeIsUniversal` during the
//! boot before wrote local time there — so it comes back off by its UTC
//! offset. Windows reads the flag only at boot and offers no way to make the
//! running kernel write the RTC as UTC, so the agent cannot prevent that
//! write; the host corrects what it finds instead.
//!
//! So each time an agent handshakes — the first after a start, the first
//! after a reboot, after a restart of the agent — the lab daemon reads the
//! guest's clock (`get_time`) and, past [`DRIFT_THRESHOLD`], sets it to the
//! host's (`set_time`), emitting `machine.clock_drift` naming the drift. It
//! runs inside the connect that made the handshake, before the handle is
//! handed out, so whatever the caller does next runs at the corrected time.
//! A `"snapshot"` or fixed clock is never read; an agent that cannot report
//! its clock is left unchecked, said once per start with
//! `machine.clock_unchecked`.
//!
//! The decision is a value ([`plan`], [`judge`]), the I/O a thin loop over
//! the [`GuestClock`] seam.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};
use vmlab_agent_proto::features;

use super::vm_agent::AgentHandle;
use crate::config::model::Clock;

/// How far the guest's clock may stand from the host's before it is set.
///
/// The RTC has whole-second resolution, so a guest that read it at boot can
/// be up to a second behind before anything is wrong; the read itself costs a
/// round trip on top. Two seconds clears both, and stays far inside every
/// tolerance that breaks things — Kerberos refuses five minutes, TLS fails on
/// certificate validity measured in hours — while sparing a correct guest a
/// clock step it would notice (a step resets timers and shows in logs).
pub const DRIFT_THRESHOLD: Duration = Duration::from_secs(2);
/// How long the agent has to answer `get_time`.
const GET_WAIT: Duration = Duration::from_secs(5);

/// What a handshake does about the guest's clock, from the machine's
/// declaration and what its agent can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// `clock = "snapshot"` or a fixed instant: vmlab never moves it.
    Leave,
    /// The clock should be checked and cannot be; the reason.
    Unchecked(String),
    /// Read the guest's clock and judge it.
    Read,
}

/// The plan for a machine whose clock is `clock`, whose agent advertises
/// `get_time` and `set_time` as given.
pub fn plan(clock: Clock, can_get: bool, can_set: bool) -> Plan {
    match clock {
        Clock::Snapshot | Clock::Fixed(_) => Plan::Leave,
        Clock::Host if !can_get => Plan::Unchecked(
            "its agent cannot report the guest clock (no `get_time` feature), so a clock \
             wrong after a reboot is not corrected; `vmlab machine repair-agent` installs \
             one that can"
                .into(),
        ),
        Clock::Host if !can_set => {
            Plan::Unchecked("its agent cannot set the guest clock (no `set_time` feature)".into())
        }
        Clock::Host => Plan::Read,
    }
}

/// How far the guest stood from the host, and whether that is past
/// [`DRIFT_THRESHOLD`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Judgement {
    /// Guest minus host, in milliseconds: positive when the guest is ahead.
    pub drift_ms: i64,
    pub correct: bool,
}

/// Judge a guest clock reading `guest_ms` taken between the host's
/// `sent_ms` and `answered_ms`. The guest read its clock somewhere in that
/// window, so it is compared with the window's midpoint: a slow answer cannot
/// pass for drift.
pub fn judge(guest_ms: i64, sent_ms: i64, answered_ms: i64) -> Judgement {
    let host_ms = sent_ms + (answered_ms - sent_ms) / 2;
    let drift_ms = guest_ms.saturating_sub(host_ms);
    Judgement {
        drift_ms,
        correct: drift_ms.unsigned_abs() > DRIFT_THRESHOLD.as_millis() as u64,
    }
}

/// The agent calls the check makes — [`AgentHandle`] in the daemon, a fake in
/// the tests.
pub trait GuestClock {
    fn has_feature(&self, feature: &str) -> bool;
    fn get_time(&self) -> impl Future<Output = Result<i64>> + Send;
    fn set_time(&self, at: SystemTime) -> impl Future<Output = Result<()>> + Send;
}

impl GuestClock for AgentHandle {
    fn has_feature(&self, feature: &str) -> bool {
        AgentHandle::has_feature(self, feature)
    }

    fn get_time(&self) -> impl Future<Output = Result<i64>> + Send {
        AgentHandle::get_time(self, GET_WAIT)
    }

    fn set_time(&self, at: SystemTime) -> impl Future<Output = Result<()>> + Send {
        AgentHandle::set_time(self, at, super::lab::CLOCK_SET_WAIT)
    }
}

fn unix_ms(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("{ms} ms since 1970"))
}

/// Check one fresh handshake's guest clock, emitting what it found through
/// `emit`. `unchecked_said` is set once the machine's `machine.clock_unchecked`
/// has been emitted, so it is said once per start rather than per handshake.
/// Never fails: a clock that could not be read or set is said instead.
pub async fn check(
    agent: &impl GuestClock,
    machine: &str,
    clock: Clock,
    unchecked_said: &AtomicBool,
    emit: &(dyn Fn(&str, Value) + Send + Sync),
) {
    match plan(
        clock,
        agent.has_feature(features::GET_TIME),
        agent.has_feature(features::SET_TIME),
    ) {
        Plan::Leave => {}
        Plan::Unchecked(reason) => {
            if !unchecked_said.swap(true, Ordering::SeqCst) {
                emit(
                    "machine.clock_unchecked",
                    json!({"vm": machine, "machine": machine, "reason": reason}),
                );
            }
        }
        Plan::Read => {
            let sent = unix_ms(SystemTime::now());
            let guest = match agent.get_time().await {
                Ok(ms) => ms,
                Err(e) => {
                    tracing::warn!(machine, "guest clock not read: {e:#}");
                    return;
                }
            };
            let answered = unix_ms(SystemTime::now());
            let j = judge(guest, sent, answered);
            if !j.correct {
                tracing::debug!(machine, drift_ms = j.drift_ms, "guest clock in step");
                return;
            }
            let now = SystemTime::now();
            let mut data = json!({
                "vm": machine,
                "machine": machine,
                "drift_ms": j.drift_ms,
                "guest": rfc3339(guest),
                "host": rfc3339(answered),
            });
            match agent.set_time(now).await {
                Ok(()) => data["corrected"] = json!(true),
                Err(e) => {
                    data["corrected"] = json!(false);
                    data["error"] = json!(format!("{e:#}"));
                }
            }
            emit("machine.clock_drift", data);
        }
    }
}

/// Where the check's events go: the lab's event log in the daemon.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// What an [`super::machine::AgentSlot`] runs on each fresh handshake, before
/// it hands the handle out.
pub type HandshakeHook =
    Arc<dyn Fn(AgentHandle) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The hook a machine's agent slot carries for one start: [`check`] against
/// the machine's `clock`, emitting into `emit`.
pub fn hook(machine: String, clock: Clock, emit: Emit) -> HandshakeHook {
    let said = Arc::new(AtomicBool::new(false));
    Arc::new(move |agent: AgentHandle| {
        let (machine, emit, said) = (machine.clone(), Arc::clone(&emit), Arc::clone(&said));
        Box::pin(async move { check(&agent, &machine, clock, &said, &*emit).await })
    })
}

/// [`hook`] emitting into the lab's event log.
pub fn lab_hook(
    machine: &str,
    clock: Clock,
    events: Arc<super::events::EventLog>,
) -> HandshakeHook {
    hook(
        machine.to_string(),
        clock,
        Arc::new(move |e: &str, v: Value| events.emit(e, v)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// **Only a host clock is checked**, and only by an agent that can both
    /// read and set it; a snapshot or fixed clock is left alone whatever the
    /// agent can do.
    #[test]
    fn the_plan_follows_the_clock_and_the_agent() {
        let fixed = Clock::Fixed("2020-01-01T00:00:00Z".parse().unwrap());
        for (can_get, can_set) in [(true, true), (false, true), (false, false)] {
            assert_eq!(plan(Clock::Snapshot, can_get, can_set), Plan::Leave);
            assert_eq!(plan(fixed, can_get, can_set), Plan::Leave);
        }
        assert_eq!(plan(Clock::Host, true, true), Plan::Read);
        let Plan::Unchecked(why) = plan(Clock::Host, false, true) else {
            panic!("an old agent is unchecked");
        };
        assert!(
            why.contains("get_time") && why.contains("repair-agent"),
            "{why}"
        );
        let Plan::Unchecked(why) = plan(Clock::Host, true, false) else {
            panic!("an agent that cannot set is unchecked");
        };
        assert!(why.contains("set_time"), "{why}");
    }

    /// **The threshold is exclusive and symmetric**: two seconds either way
    /// is in step, a millisecond more is corrected, and the sign says which
    /// side is ahead.
    #[test]
    fn drift_past_two_seconds_either_way_is_corrected() {
        let host = 1_760_000_000_000;
        assert_eq!(
            judge(host + 2_000, host, host),
            Judgement {
                drift_ms: 2_000,
                correct: false
            }
        );
        assert_eq!(
            judge(host - 2_000, host, host),
            Judgement {
                drift_ms: -2_000,
                correct: false
            }
        );
        assert!(judge(host + 2_001, host, host).correct);
        assert!(judge(host - 2_001, host, host).correct);
        // The issue's guest: ten hours ahead after the first reboot.
        let j = judge(host + 36_000_000, host, host);
        assert_eq!(j.drift_ms, 36_000_000);
        assert!(j.correct);
    }

    /// A slow answer is measured from the middle of the round trip, so the
    /// wait cannot read as drift.
    #[test]
    fn the_round_trip_is_not_drift() {
        let sent = 1_760_000_000_000;
        let answered = sent + 3_000;
        assert_eq!(judge(sent + 1_500, sent, answered).drift_ms, 0);
        assert!(!judge(sent + 3_000, sent, answered).correct);
    }

    struct Fake {
        features: Vec<&'static str>,
        ahead_ms: i64,
        refuse: Option<&'static str>,
        set: Mutex<Vec<SystemTime>>,
    }

    impl Fake {
        fn new(features: &[&'static str], ahead_ms: i64) -> Self {
            Self {
                features: features.to_vec(),
                ahead_ms,
                refuse: None,
                set: Mutex::new(Vec::new()),
            }
        }
    }

    impl GuestClock for Fake {
        fn has_feature(&self, feature: &str) -> bool {
            self.features.contains(&feature)
        }
        fn get_time(&self) -> impl Future<Output = Result<i64>> + Send {
            let ms = unix_ms(SystemTime::now()) + self.ahead_ms;
            async move { Ok(ms) }
        }
        fn set_time(&self, at: SystemTime) -> impl Future<Output = Result<()>> + Send {
            let out = match self.refuse {
                Some(why) => Err(anyhow::anyhow!("the guest refused to set its clock: {why}")),
                None => {
                    self.set.lock().unwrap().push(at);
                    Ok(())
                }
            };
            async move { out }
        }
    }

    const BOTH: &[&str] = &[features::GET_TIME, features::SET_TIME];

    async fn run(agent: &Fake, clock: Clock, said: &AtomicBool) -> Vec<(String, Value)> {
        let seen = Mutex::new(Vec::new());
        let emit = |e: &str, v: Value| seen.lock().unwrap().push((e.to_string(), v));
        check(agent, "dc1", clock, said, &emit).await;
        seen.into_inner().unwrap()
    }

    /// **A guest ten hours ahead is set to the host's time**, and the event
    /// names the machine and the drift.
    #[tokio::test]
    async fn a_drifted_host_clock_is_set_and_said() {
        let agent = Fake::new(BOTH, 36_000_000);
        let seen = run(&agent, Clock::Host, &AtomicBool::new(false)).await;
        assert_eq!(agent.set.lock().unwrap().len(), 1);
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (event, data) = &seen[0];
        assert_eq!(event, "machine.clock_drift");
        assert_eq!(data["machine"], "dc1");
        assert_eq!(data["corrected"], true);
        let drift = data["drift_ms"].as_i64().unwrap();
        assert!((35_999_000..=36_001_000).contains(&drift), "{drift}");
    }

    /// A guest in step is neither set nor reported.
    #[tokio::test]
    async fn a_clock_in_step_is_left_alone() {
        let agent = Fake::new(BOTH, 300);
        assert!(
            run(&agent, Clock::Host, &AtomicBool::new(false))
                .await
                .is_empty()
        );
        assert!(agent.set.lock().unwrap().is_empty());
    }

    /// A refusal is reported on the same event, with why.
    #[tokio::test]
    async fn a_refused_set_is_said_with_the_reason() {
        let mut agent = Fake::new(BOTH, -60_000);
        agent.refuse = Some("privilege not held");
        let seen = run(&agent, Clock::Host, &AtomicBool::new(false)).await;
        assert_eq!(seen[0].0, "machine.clock_drift");
        assert_eq!(seen[0].1["corrected"], false);
        assert!(
            seen[0].1["error"]
                .as_str()
                .unwrap()
                .contains("privilege not held")
        );
    }

    /// **A snapshot or fixed clock is never read nor set**, however far off.
    #[tokio::test]
    async fn a_snapshot_or_fixed_clock_is_never_touched() {
        let fixed = Clock::Fixed("2020-01-01T00:00:00Z".parse().unwrap());
        for clock in [Clock::Snapshot, fixed] {
            let agent = Fake::new(BOTH, 36_000_000);
            assert!(run(&agent, clock, &AtomicBool::new(false)).await.is_empty());
            assert!(agent.set.lock().unwrap().is_empty());
        }
    }

    /// **An old agent is skipped and said once**: the first handshake of a
    /// start emits `machine.clock_unchecked`, later ones nothing, and the
    /// clock is not set blind.
    #[tokio::test]
    async fn an_old_agent_is_unchecked_and_said_once() {
        let agent = Fake::new(&[features::EXEC, features::SET_TIME], 36_000_000);
        let said = AtomicBool::new(false);
        let first = run(&agent, Clock::Host, &said).await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "machine.clock_unchecked");
        assert!(first[0].1["reason"].as_str().unwrap().contains("get_time"));
        assert!(run(&agent, Clock::Host, &said).await.is_empty());
        assert!(agent.set.lock().unwrap().is_empty());
    }
}
