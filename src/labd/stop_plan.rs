//! The **stop plan**: the rungs a VM's graceful stop climbs (PRD §7.2),
//! worked out as one value before the ladder touches the guest (ADR-0003).
//!
//! The ladder used to decide as it went, and it decided from one flag: had
//! the agent answered yet? A guest stopped while it was still booting had
//! not, so the ladder skipped the agent rung and pressed the ACPI power
//! button — which macOS ignores outright, and which a Linux guest whose
//! logind is not up yet drops — and the stop ended thirty seconds later in
//! an emulator quit: a power cut, for the guest. `vmlab up` returns before a
//! clone's agent answers, so `up` then `down` hit it every time.
//!
//! So the plan waits for an agent that is coming. A machine whose template
//! sealed one has an agent on the way while it boots, and its stop waits for
//! that agent to answer — a minute on a guest that would act on ACPI anyway,
//! as long as readiness itself would wait on a guest that will not (the
//! profile's `acpi_shutdown = false`, macOS) — before it asks the guest to
//! shut down. A guest that ignores ACPI gets no ACPI rung at all, since it
//! could only delay the quit, and its agent rung waits longer for the guest
//! to finish, since there is nothing graceful behind it.

use std::time::Duration;

/// How long a stop waits for the agent of a booting guest that would also
/// act on ACPI: the agent is the better rung — it shuts the guest down
/// whatever its desktop is showing — but not worth a long wait when the
/// power button works too.
pub const BOOTING_AGENT_WAIT: Duration = Duration::from_secs(60);

/// How long a rung waits for the guest to power off once asked.
pub const RUNG_EXIT_WAIT: Duration = Duration::from_secs(30);

/// How long the agent rung waits for a guest that ignores ACPI to power off:
/// nothing graceful follows it, so it gives a guest closing its applications
/// the time it takes.
pub const AGENT_ONLY_EXIT_WAIT: Duration = Duration::from_secs(120);

/// What the plan is made from — everything about the machine the ladder's
/// choices depend on, read once when the stop begins.
#[derive(Debug, Clone, Copy)]
pub struct StopFacts {
    /// The caller asked for a forced stop.
    pub force: bool,
    /// The profile gives the guest an agent channel (§7.4).
    pub agent_channel: bool,
    /// The agent has answered since this boot began.
    pub agent_answered: bool,
    /// An agent is on its way: the template sealed one (§6.1), so a guest
    /// still booting will run it.
    pub agent_sealed: bool,
    /// The guest shuts down on an ACPI power-button press (the profile's
    /// `acpi_shutdown`).
    pub acpi_shutdown: bool,
    /// The machine's readiness budget ([`Machine::ready_timeout`]) — how
    /// long a booting guest that ignores ACPI may take to bring its agent up.
    ///
    /// [`Machine::ready_timeout`]: super::machine::Machine::ready_timeout
    pub ready_timeout: Duration,
}

/// One rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    /// Ask the guest agent to power the guest down. `answer_within` is how
    /// long to wait for the agent to answer first — zero for one attempt —
    /// and `exit_within` how long the guest then has to power off.
    Agent {
        answer_within: Duration,
        exit_within: Duration,
    },
    /// Press the ACPI power button and wait for the guest to power off.
    Acpi { exit_within: Duration },
    /// Tell the emulator to quit: the guest is not shut down, but the
    /// emulator flushes its block caches on the way out.
    Quit { exit_within: Duration },
    /// End the emulator process.
    Kill,
}

impl Rung {
    /// What the daemon log calls it.
    pub fn name(&self) -> &'static str {
        match self {
            Rung::Agent { .. } => "agent",
            Rung::Acpi { .. } => "ACPI",
            Rung::Quit { .. } => "quit",
            Rung::Kill => "kill",
        }
    }
}

/// The rungs a stop climbs, in order; the ladder stops at the first that
/// ends the machine, and the last one always does.
pub fn plan_stop(facts: &StopFacts) -> Vec<Rung> {
    if facts.force {
        return vec![Rung::Kill];
    }
    let mut rungs = Vec::with_capacity(4);
    if facts.agent_channel && (facts.agent_answered || facts.agent_sealed) {
        let answer_within = if facts.agent_answered {
            Duration::ZERO
        } else if facts.acpi_shutdown {
            BOOTING_AGENT_WAIT
        } else {
            facts.ready_timeout
        };
        let exit_within = if facts.acpi_shutdown {
            RUNG_EXIT_WAIT
        } else {
            AGENT_ONLY_EXIT_WAIT
        };
        rungs.push(Rung::Agent {
            answer_within,
            exit_within,
        });
    }
    if facts.acpi_shutdown {
        rungs.push(Rung::Acpi {
            exit_within: RUNG_EXIT_WAIT,
        });
    }
    rungs.push(Rung::Quit {
        exit_within: RUNG_EXIT_WAIT,
    });
    rungs.push(Rung::Kill);
    rungs
}

#[cfg(test)]
mod tests {
    use super::*;

    const READY: Duration = Duration::from_secs(600);

    fn facts() -> StopFacts {
        StopFacts {
            force: false,
            agent_channel: true,
            agent_answered: true,
            agent_sealed: true,
            acpi_shutdown: true,
            ready_timeout: READY,
        }
    }

    const QUIT: Rung = Rung::Quit {
        exit_within: RUNG_EXIT_WAIT,
    };
    const ACPI: Rung = Rung::Acpi {
        exit_within: RUNG_EXIT_WAIT,
    };

    /// The ordinary stop: an agent that has answered is asked once, then the
    /// power button, the quit and the kill.
    #[test]
    fn an_answered_agent_is_asked_once_before_acpi() {
        assert_eq!(
            plan_stop(&facts()),
            vec![
                Rung::Agent {
                    answer_within: Duration::ZERO,
                    exit_within: RUNG_EXIT_WAIT,
                },
                ACPI,
                QUIT,
                Rung::Kill,
            ]
        );
    }

    /// Force is the kill and nothing else, whatever the guest could answer.
    #[test]
    fn a_forced_stop_is_the_kill_alone() {
        let f = StopFacts {
            force: true,
            ..facts()
        };
        assert_eq!(plan_stop(&f), vec![Rung::Kill]);
    }

    /// A booting guest whose template sealed an agent waits for it — a
    /// bounded minute where ACPI would work too.
    #[test]
    fn a_booting_guest_waits_a_minute_for_its_sealed_agent() {
        let f = StopFacts {
            agent_answered: false,
            ..facts()
        };
        assert_eq!(
            plan_stop(&f)[0],
            Rung::Agent {
                answer_within: BOOTING_AGENT_WAIT,
                exit_within: RUNG_EXIT_WAIT,
            }
        );
        assert_eq!(plan_stop(&f)[1], ACPI, "ACPI still follows");
    }

    /// The macOS case: a guest that ignores ACPI waits for its agent as long
    /// as readiness would, gives the guest longer to power off, and never
    /// presses a power button nothing answers.
    #[test]
    fn a_guest_that_ignores_acpi_relies_on_its_agent() {
        let booting = StopFacts {
            agent_answered: false,
            acpi_shutdown: false,
            ..facts()
        };
        assert_eq!(
            plan_stop(&booting),
            vec![
                Rung::Agent {
                    answer_within: READY,
                    exit_within: AGENT_ONLY_EXIT_WAIT,
                },
                QUIT,
                Rung::Kill,
            ]
        );
        let up = StopFacts {
            acpi_shutdown: false,
            ..facts()
        };
        assert_eq!(
            plan_stop(&up)[0],
            Rung::Agent {
                answer_within: Duration::ZERO,
                exit_within: AGENT_ONLY_EXIT_WAIT,
            }
        );
    }

    /// No agent is waited for where none is coming: a scratch machine that
    /// has not answered, and a profile with no channel, go straight to ACPI.
    #[test]
    fn no_agent_rung_without_an_agent_on_the_way() {
        let scratch = StopFacts {
            agent_answered: false,
            agent_sealed: false,
            ..facts()
        };
        assert_eq!(plan_stop(&scratch), vec![ACPI, QUIT, Rung::Kill]);
        let vintage = StopFacts {
            agent_channel: false,
            ..facts()
        };
        assert_eq!(plan_stop(&vintage), vec![ACPI, QUIT, Rung::Kill]);
    }

    /// A scratch machine whose agent did answer is asked, as ever.
    #[test]
    fn an_answered_agent_is_asked_even_unsealed() {
        let f = StopFacts {
            agent_sealed: false,
            ..facts()
        };
        assert!(matches!(plan_stop(&f)[0], Rung::Agent { .. }));
    }

    /// A guest that ignores ACPI and has no agent coming still ends cleanly
    /// at the quit before the kill.
    #[test]
    fn an_agentless_acpi_ignoring_guest_ends_at_the_quit() {
        let f = StopFacts {
            agent_answered: false,
            agent_sealed: false,
            acpi_shutdown: false,
            ..facts()
        };
        assert_eq!(plan_stop(&f), vec![QUIT, Rung::Kill]);
    }
}
