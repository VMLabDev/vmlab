//! Machine-scoped verbs that ask a guest what it can do or what it holds
//! (PRD §12): capabilities, metrics, clipboard. One implementation for both
//! kinds — a VM and a container answer the same commands (§18).
//!
//! Each renders for a person by default and pretty JSON under `--json`, the
//! convention `vmlab lab list --json` set. (`vmlab osinfo` predates it and
//! prints JSON unconditionally; that is its own compatibility question.)
//!
//! The renderers take the daemon's payload as a [`Value`] and return a string
//! rather than printing, so what a user reads can be asserted against a
//! payload built in a test — no lab, no daemon (ADR-0004's lesson).

use anyhow::Result;
use serde_json::Value;

use super::daemon::remote;
use super::lab::{lab_client_for, rt, split_vm_ref};
use super::{emit, print_json, yes_no};
use crate::proto::LabRequest;

/// `vmlab machine capabilities <machine>` — what this machine can do beyond
/// the universal commands, probed live rather than inferred from its kind.
/// Agent features come from a live handshake, so a machine that is up but not
/// yet answering reports none.
pub fn cmd_capabilities(machine_ref: &str, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let caps = client
            .send(LabRequest::MachineCapabilities { machine })
            .await
            .map_err(remote)?;
        emit(json, &caps, render_capabilities)
    })
}

/// One capability per line: the flags first, then whatever the agent
/// negotiated. A machine with no agent answering shows `-` rather than an
/// empty line, so "asked and got nothing" reads differently from "did not
/// ask".
fn render_capabilities(caps: &Value) -> String {
    use std::fmt::Write as _;
    let flag = |key: &str| yes_no(caps[key].as_bool().unwrap_or(false));
    let agent: Vec<&str> = caps["agent"]
        .as_array()
        .map(|f| f.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let agent = if agent.is_empty() {
        "-".to_string()
    } else {
        agent.join(", ")
    };
    let mut out = String::new();
    for (label, value) in [
        ("kind", caps["kind"].as_str().unwrap_or("?")),
        ("display", flag("display")),
        ("console log", flag("console_log")),
        ("reboot", flag("reboot")),
        ("healthcheck", flag("healthcheck")),
        ("agent", agent.as_str()),
    ] {
        let _ = writeln!(out, "{label:<12} {value}");
    }
    out
}

/// `vmlab machine stats <machine>` — the guest's latest metrics.
///
/// Reading subscribes the daemon's sampler, so a machine nothing had asked
/// about starts being sampled. That is a side effect of a read, which the
/// verb's help says outright rather than hiding.
pub fn cmd_stats(machine_ref: &str, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let stats = client
            .send(LabRequest::MachineStats { machine })
            .await
            .map_err(remote)?;
        emit(json, &stats, render_stats)
    })
}

/// CPU, memory and every mounted filesystem, one per line. Bytes are the wire
/// form; a person reads sizes.
fn render_stats(stats: &Value) -> String {
    use std::fmt::Write as _;
    let bytes = |v: &Value| v.as_u64().unwrap_or(0);
    let mut rows = vec![
        (
            "cpu".to_string(),
            format!("{:.1}%", stats["cpu_pct"].as_f64().unwrap_or(0.0)),
        ),
        (
            "memory".to_string(),
            usage(bytes(&stats["mem_used"]), bytes(&stats["mem_total"])),
        ),
    ];
    for disk in stats["disks"].as_array().unwrap_or(&Vec::new()) {
        rows.push((
            format!("disk {}", disk["mount"].as_str().unwrap_or("?")),
            usage(bytes(&disk["used"]), bytes(&disk["total"])),
        ));
    }
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (label, value) in rows {
        let _ = writeln!(out, "{label:<width$}  {value}");
    }
    out
}

/// `used / total (pct%)`, or just the used figure when the guest reported no
/// total — a percentage of nothing is worse than no percentage.
fn usage(used: u64, total: u64) -> String {
    if total == 0 {
        return human_bytes(used);
    }
    format!(
        "{} / {} ({}%)",
        human_bytes(used),
        human_bytes(total),
        used.saturating_mul(100) / total,
    )
}

/// A byte count at the largest unit that leaves a figure worth reading.
///
/// Not [`crate::template::meta::format_size`]: that renders exact multiples
/// for config round-tripping and falls back to raw digits otherwise, which is
/// what a live memory figure always is.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ];
    for (unit, suffix) in UNITS {
        if bytes >= unit {
            return format!("{:.1} {suffix}", bytes as f64 / unit as f64);
        }
    }
    format!("{bytes} B")
}

/// `vmlab clipboard get <machine>` — the guest clipboard, verbatim on stdout.
///
/// No trailing newline is added, so `vmlab clipboard get a | vmlab clipboard
/// set b` moves exactly what the guest held.
pub fn cmd_clipboard_get(machine_ref: &str, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let text = client
            .send(LabRequest::MachineClipboardGet { machine })
            .await
            .map_err(remote)?;
        if json {
            return print_json(&text);
        }
        use std::io::Write as _;
        print!("{}", text.as_str().unwrap_or_default());
        std::io::stdout().flush()?;
        Ok(())
    })
}

/// `vmlab clipboard set <machine> [text]` — write the guest clipboard from
/// the argument, or from stdin when there is no argument.
///
/// Stdin is passed through byte for byte, including a trailing newline: `get`
/// adds none, so the pair round-trips. A caller who does not want `echo`'s
/// newline has `echo -n`.
pub fn cmd_clipboard_set(machine_ref: &str, text: Option<String>, json: bool) -> Result<()> {
    let text = match text {
        Some(text) => text,
        None => std::io::read_to_string(std::io::stdin())?,
    };
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(machine_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let done = client
            .send(LabRequest::MachineClipboardSet {
                machine: machine.clone(),
                text: text.clone(),
            })
            .await
            .map_err(remote)?;
        emit(json, &done, |_| {
            format!("copied {} bytes to \"{machine}\" clipboard\n", text.len())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{human_bytes, render_capabilities, render_stats, usage};
    use crate::labd::machine::Capabilities;
    use crate::status::MachineKind;
    use serde_json::json;

    /// The payload the report reads is the producer's own [`Capabilities`],
    /// serialised the way the daemon serialises it — so a field renamed in
    /// `src/labd/machine.rs` stops compiling here instead of blanking a line
    /// in front of a user (ADR-0004's lesson).
    #[test]
    fn capabilities_report_every_probed_flag_and_the_agent_features() {
        let caps = Capabilities {
            kind: MachineKind::Vm,
            display: true,
            console_log: false,
            reboot: true,
            healthcheck: false,
            agent: vec!["terminal".into(), "exec".into(), "metrics".into()],
        };
        let out = render_capabilities(&serde_json::to_value(caps).unwrap());
        assert!(out.contains("kind         vm"), "got:\n{out}");
        assert!(out.contains("display      yes"), "got:\n{out}");
        assert!(out.contains("console log  no"), "got:\n{out}");
        assert!(out.contains("reboot       yes"), "got:\n{out}");
        assert!(out.contains("healthcheck  no"), "got:\n{out}");
        assert!(
            out.contains("agent        terminal, exec, metrics"),
            "got:\n{out}"
        );
    }

    /// No agent answering is a live fact, not a missing field: it reads as a
    /// dash, so "probed and got nothing" cannot be mistaken for "not probed".
    #[test]
    fn a_machine_with_no_agent_answering_reports_a_dash() {
        let caps = Capabilities {
            kind: MachineKind::Container,
            display: false,
            console_log: true,
            reboot: false,
            healthcheck: true,
            agent: Vec::new(),
        };
        let out = render_capabilities(&serde_json::to_value(caps).unwrap());
        assert!(out.contains("agent        -"), "got:\n{out}");
        assert!(out.contains("kind         container"), "got:\n{out}");
    }

    /// Metrics arrive as bytes; a person reads sizes and a percentage.
    #[test]
    fn stats_render_bytes_as_sizes_with_a_line_per_disk() {
        let out = render_stats(&json!({
            "cpu_pct": 12.5,
            "mem_used": 2u64 << 30,
            "mem_total": 4u64 << 30,
            "disks": [
                {"mount": "/", "used": 8u64 << 30, "total": 20u64 << 30},
                {"mount": "/var", "used": 512u64 << 20, "total": 2u64 << 30},
            ],
        }));
        // One label column, wide enough for the longest mount point.
        assert_eq!(
            out,
            "cpu        12.5%\n\
             memory     2.0 GiB / 4.0 GiB (50%)\n\
             disk /     8.0 GiB / 20.0 GiB (40%)\n\
             disk /var  512.0 MiB / 2.0 GiB (25%)\n",
        );
    }

    /// A guest that reported no disks still reports its CPU and memory, rather
    /// than rendering an empty table or panicking on the missing array.
    #[test]
    fn stats_survive_a_guest_that_reported_no_disks() {
        let out = render_stats(&json!({"cpu_pct": 0.0, "mem_used": 0, "mem_total": 0}));
        assert_eq!(out, "cpu     0.0%\nmemory  0 B\n");
    }

    /// A total of zero is a guest that has not worked out the figure, not a
    /// full disk: no percentage is better than a divide by zero.
    #[test]
    fn usage_without_a_total_reports_only_what_is_used() {
        assert_eq!(usage(1 << 20, 0), "1.0 MiB");
        assert_eq!(usage(0, 1 << 20), "0 B / 1.0 MiB (0%)");
    }

    #[test]
    fn human_bytes_picks_the_largest_useful_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 << 20), "3.0 MiB");
        assert_eq!(human_bytes((3 << 30) + (512 << 20)), "3.5 GiB");
        assert_eq!(human_bytes(2 << 40), "2.0 TiB");
    }
}
