//! `vmlab port <machine> <guest-port>[/tcp|/udp]` — the host port a running
//! machine's forward to that guest port is bound on (PRD §21.7).
//!
//! A worktree lab's fixed ports are shifted by its port slot, and an automatic
//! port is the kernel's pick, so a script cannot read the bound port from the
//! lab file. This verb reads it from the lab daemon's status projection
//! (`LabStatus.forwards`) and prints nothing else; it needs no wire vocabulary
//! of its own and works for any lab, frozen or not. Which forward answers is
//! [`select`], a value computed from the projection before anything prints
//! (ADR-0003).

use anyhow::Result;

use super::daemon;
use super::lab::{current_lab, rt, split_vm_ref};
use crate::proto::CommandError;
use crate::status::{ForwardStatus, LabStatus, PowerState};

/// The protocol a `/tcp` or `/udp` suffix asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProto {
    Tcp,
    Udp,
}

impl PortProto {
    fn as_str(self) -> &'static str {
        match self {
            PortProto::Tcp => "tcp",
            PortProto::Udp => "udp",
        }
    }

    /// Whether a forward declared with `proto` carries this protocol: `both`
    /// carries either.
    fn carried_by(self, proto: &str) -> bool {
        proto == "both" || proto == self.as_str()
    }
}

/// The `<guest-port>[/tcp|/udp]` argument, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortTarget {
    pub guest_port: u16,
    pub proto: Option<PortProto>,
}

impl std::fmt::Display for PortTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.proto {
            Some(p) => write!(f, "{}/{}", self.guest_port, p.as_str()),
            None => write!(f, "{}", self.guest_port),
        }
    }
}

/// Parse `<guest-port>[/tcp|/udp]`.
pub fn parse_target(arg: &str) -> Result<PortTarget, CommandError> {
    let (port, proto) = match arg.split_once('/') {
        Some((port, proto)) => {
            let proto = match proto.to_ascii_lowercase().as_str() {
                "tcp" => PortProto::Tcp,
                "udp" => PortProto::Udp,
                _ => {
                    return Err(CommandError::invalid(format!(
                        "`{arg}`: the protocol is `tcp` or `udp`, not `{proto}`"
                    )));
                }
            };
            (port, Some(proto))
        }
        None => (arg, None),
    };
    match port.parse::<u16>() {
        Ok(guest_port) if guest_port != 0 => Ok(PortTarget { guest_port, proto }),
        _ => Err(CommandError::invalid(format!(
            "`{arg}` is not a guest port: expected <port>[/tcp|/udp] with a port from 1 to 65535"
        ))),
    }
}

/// The guest port of a forward. A daemon that predates `guest_port` leaves it
/// zero, and `guest` (`ip:port`) still carries it.
fn guest_port_of(f: &ForwardStatus) -> Option<u16> {
    if f.guest_port != 0 {
        return Some(f.guest_port);
    }
    f.guest.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
}

/// One forward the way an error lists it: `8080/tcp on host port 18280`.
fn describe(f: &ForwardStatus) -> String {
    let guest = guest_port_of(f).map_or_else(|| f.guest.clone(), |p| p.to_string());
    format!("{guest}/{} on host port {}", f.proto, f.host_port)
}

/// The forward `vmlab port` answers with, or why there is none.
///
/// A stopped machine has none to offer, whatever the projection still lists.
/// Forwards match on machine and guest port, and on the suffix's protocol when
/// there is one (`both` matches either). Several matches that share one host
/// port are one answer — the first in the projection's order. Matches bound on
/// different host ports are ambiguous: the caller is asked for `/tcp` or `/udp`
/// when one of those would settle it, else shown every candidate.
pub fn select<'a>(
    status: &'a LabStatus,
    machine: &str,
    target: PortTarget,
) -> Result<&'a ForwardStatus, CommandError> {
    let Some(m) = status.machines.iter().find(|m| m.name == machine) else {
        return Err(CommandError::not_found(format!(
            "no vm or container \"{machine}\" in lab \"{}\"",
            status.lab
        )));
    };
    // A stopped machine's forwards can stay installed in the projection, but
    // nothing answers behind them: the verb reports a running machine's.
    if m.state == PowerState::Stopped {
        return Err(CommandError::conflict(format!(
            "\"{machine}\" in lab \"{}\" is not running",
            status.lab
        )));
    }
    let installed: Vec<&ForwardStatus> = status
        .forwards
        .iter()
        .filter(|f| f.machine == machine)
        .collect();
    let matches: Vec<&ForwardStatus> = installed
        .iter()
        .copied()
        .filter(|f| guest_port_of(f) == Some(target.guest_port))
        .filter(|f| target.proto.is_none_or(|p| p.carried_by(&f.proto)))
        .collect();
    let Some(first) = matches.first() else {
        let what = if installed.is_empty() {
            "it has no forwards installed".to_string()
        } else {
            let list: Vec<String> = installed.iter().map(|f| describe(f)).collect();
            format!("its forwards: {}", list.join(", "))
        };
        return Err(CommandError::not_found(format!(
            "\"{machine}\" in lab \"{}\" has no forward to guest port {target}; {what}",
            status.lab
        )));
    };
    if matches.iter().all(|f| f.host_port == first.host_port) {
        return Ok(first);
    }
    let list: Vec<String> = matches.iter().map(|f| describe(f)).collect();
    let settles = |p: PortProto| {
        let mut ports = matches
            .iter()
            .filter(|f| p.carried_by(&f.proto))
            .map(|f| f.host_port);
        ports
            .next()
            .is_some_and(|first| ports.all(|port| port == first))
    };
    let ask = if target.proto.is_none() && settles(PortProto::Tcp) && settles(PortProto::Udp) {
        format!("; name one as {0}/tcp or {0}/udp", target.guest_port)
    } else {
        String::new()
    };
    Err(CommandError::invalid(format!(
        "\"{machine}\" has more than one forward to guest port {target}: {}{ask}",
        list.join(", ")
    )))
}

/// `vmlab port`. A read-only query: it starts no daemon, and a lab with none
/// running has no forwards to report.
pub fn cmd_port(machine_ref: &str, target: &str, json: bool) -> Result<()> {
    let target = parse_target(target)?;
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (name, client) = match lab {
            Some(name) => {
                let client = daemon::try_lab_daemon(&name).await?;
                (name, client)
            }
            None => {
                let (name, root) = current_lab()?;
                let client = daemon::try_own_lab_daemon(&name, &root).await?;
                (name, client)
            }
        };
        let Some(client) = client else {
            return Err(CommandError::conflict(format!("lab \"{name}\" is not running")).into());
        };
        let status = super::lab::lab_status(&client).await?;
        let forward = select(&status, &machine, target)?;
        if json {
            return super::print_json(&serde_json::to_value(forward)?);
        }
        println!("{}", forward.host_port);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::fixtures::{container, lab, machine};

    fn forward(machine: &str, guest_port: u16, proto: &str, host_port: u16) -> ForwardStatus {
        ForwardStatus {
            machine: machine.into(),
            segment: "lan".into(),
            host_port,
            guest: format!("10.0.0.5:{guest_port}"),
            proto: proto.into(),
            auto: false,
            guest_port,
            declared_port: None,
        }
    }

    fn running(name: &str) -> crate::status::MachineStatus {
        machine(name, PowerState::Running, true, container(None, None))
    }

    fn status(forwards: Vec<ForwardStatus>) -> LabStatus {
        LabStatus {
            forwards,
            ..lab(vec![
                running("web"),
                running("db"),
                machine("off", PowerState::Stopped, false, container(None, None)),
            ])
        }
    }

    fn target(arg: &str) -> PortTarget {
        parse_target(arg).expect("parses")
    }

    #[test]
    fn the_argument_is_a_port_and_an_optional_protocol() {
        assert_eq!(
            target("80"),
            PortTarget {
                guest_port: 80,
                proto: None
            }
        );
        assert_eq!(target("53/udp").proto, Some(PortProto::Udp));
        assert_eq!(target("53/TCP").proto, Some(PortProto::Tcp));
        for bad in ["0", "70000", "http", "80/sctp", "/tcp", ""] {
            let err = parse_target(bad).unwrap_err();
            assert_eq!(err.code, crate::proto::ErrorCode::InvalidArgument, "{bad}");
        }
    }

    #[test]
    fn one_match_answers_and_other_machines_do_not_count() {
        let s = status(vec![
            forward("web", 8080, "tcp", 18280),
            forward("db", 8080, "tcp", 18281),
        ]);
        assert_eq!(select(&s, "web", target("8080")).unwrap().host_port, 18280);
        assert_eq!(
            select(&s, "db", target("8080/tcp")).unwrap().host_port,
            18281
        );
    }

    #[test]
    fn no_match_names_the_installed_forwards() {
        let s = status(vec![forward("web", 8080, "tcp", 18280)]);
        let err = select(&s, "web", target("80")).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::NotFound);
        assert!(
            err.message.contains("guest port 80;")
                && err.message.contains("8080/tcp on host port 18280"),
            "{}",
            err.message
        );
        // A udp suffix does not match a tcp forward.
        let err = select(&s, "web", target("8080/udp")).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::NotFound);

        let err = select(&s, "db", target("80")).unwrap_err();
        assert!(
            err.message.contains("no forwards installed"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_stopped_machine_or_an_unknown_one_says_so() {
        // The daemon can keep a stopped machine's forwards in the projection.
        let s = status(vec![forward("off", 80, "tcp", 18080)]);
        let err = select(&s, "off", target("80")).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::Conflict);
        assert!(err.message.contains("not running"), "{}", err.message);

        let err = select(&s, "nope", target("80")).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::NotFound);
        assert_eq!(err.message, "no vm or container \"nope\" in lab \"demo\"");
    }

    #[test]
    fn tcp_and_udp_on_different_host_ports_ask_for_the_protocol() {
        let s = status(vec![
            forward("web", 53, "tcp", 15353),
            forward("web", 53, "udp", 15354),
        ]);
        let err = select(&s, "web", target("53")).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::InvalidArgument);
        assert!(err.message.contains("53/tcp or 53/udp"), "{}", err.message);
        assert_eq!(
            select(&s, "web", target("53/tcp")).unwrap().host_port,
            15353
        );
        assert_eq!(
            select(&s, "web", target("53/udp")).unwrap().host_port,
            15354
        );
    }

    #[test]
    fn tcp_and_udp_on_one_host_port_are_one_answer() {
        let s = status(vec![
            forward("web", 53, "tcp", 15353),
            forward("web", 53, "udp", 15353),
        ]);
        let f = select(&s, "web", target("53")).unwrap();
        assert_eq!((f.host_port, f.proto.as_str()), (15353, "tcp"));
    }

    #[test]
    fn both_matches_either_suffix() {
        let s = status(vec![forward("web", 9000, "both", 19000)]);
        for arg in ["9000", "9000/tcp", "9000/udp"] {
            assert_eq!(
                select(&s, "web", target(arg)).unwrap().host_port,
                19000,
                "{arg}"
            );
        }
    }

    #[test]
    fn two_forwards_one_protocol_list_the_candidates_without_asking_for_one() {
        let mut other = forward("web", 80, "tcp", 18081);
        other.segment = "back".into();
        let s = status(vec![forward("web", 80, "tcp", 18080), other]);
        let err = select(&s, "web", target("80")).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::InvalidArgument);
        assert!(
            err.message.contains("host port 18080")
                && err.message.contains("host port 18081")
                && !err.message.contains("/udp"),
            "{}",
            err.message
        );
    }

    /// A daemon that predates `guest_port` still answers through `guest`.
    #[test]
    fn an_old_daemon_s_forward_matches_on_its_guest_address() {
        let mut f = forward("web", 8080, "tcp", 18280);
        f.guest_port = 0;
        let s = status(vec![f]);
        assert_eq!(select(&s, "web", target("8080")).unwrap().host_port, 18280);
    }
}
