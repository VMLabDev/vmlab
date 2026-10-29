//! The pieces `up`'s parallel provisioning (§7.2) runs on that are not the
//! lab runtime's: each machine's output led by its name, and the one error a
//! step stopped by someone else's failure carries.
//!
//! Machines provision in parallel, so their lines interleave on the one
//! stream `up` prints. [`LinePrefix`] buffers a step's output to whole lines
//! and leads each with `<machine>: `, leaving alone a line that already does
//! (the runtime's own `"vm01": waiting for …` and a script's `vm01: …`).

use std::sync::{Arc, Mutex};

use crate::scripting::{OutputSink, ScriptCancelled};
use crate::sync::LockRecover;

/// A step that did not finish because another machine's step failed first
/// (§7.2: the first failed step fails `up` and cancels the rest). It is never
/// the error `up` reports while the failure that caused it is known.
#[derive(Debug)]
pub struct ProvisionCancelled;

impl std::fmt::Display for ProvisionCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled: another machine's provisioning failed")
    }
}

impl std::error::Error for ProvisionCancelled {}

/// Whether `e` is a cancellation rather than a failure of its own — a
/// provision script stopped at a host call, or a wait or playbook abandoned.
pub fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.is::<ProvisionCancelled>() || c.is::<ScriptCancelled>())
}

/// Of a failure already in hand and another, the one `up` should report: the
/// first real failure, never a cancellation it caused.
pub fn first_failure(held: anyhow::Error, next: anyhow::Error) -> anyhow::Error {
    if is_cancelled(&held) && !is_cancelled(&next) {
        next
    } else {
        held
    }
}

/// Whether `line` already leads with `machine`'s name.
fn names_machine(line: &str, machine: &str) -> bool {
    let bare = line
        .strip_prefix(machine)
        .is_some_and(|rest| rest.starts_with(": "));
    let quoted = line
        .strip_prefix('"')
        .and_then(|l| l.strip_prefix(machine))
        .is_some_and(|rest| rest.starts_with("\": "));
    bare || quoted
}

/// `line` led by `machine`'s name, unless it already is.
pub fn lead(machine: &str, line: &str) -> String {
    if names_machine(line, machine) {
        line.to_string()
    } else {
        format!("{machine}: {line}")
    }
}

/// One machine's output during `up`, a line at a time, each led by its name.
pub struct LinePrefix {
    machine: String,
    sink: OutputSink,
    partial: Mutex<String>,
}

impl LinePrefix {
    pub fn new(machine: &str, sink: OutputSink) -> Arc<Self> {
        Arc::new(Self {
            machine: machine.to_string(),
            sink,
            partial: Mutex::new(String::new()),
        })
    }

    /// This prefixer as the sink a step writes to.
    pub fn sink(self: &Arc<Self>) -> OutputSink {
        let me = Arc::clone(self);
        Arc::new(move |chunk: String| me.write(&chunk))
    }

    fn write(&self, chunk: &str) {
        let lines: Vec<String> = {
            let mut partial = self.partial.lock_recover();
            partial.push_str(chunk);
            let mut lines = Vec::new();
            while let Some(end) = partial.find('\n') {
                let line: String = partial.drain(..=end).collect();
                lines.push(lead(&self.machine, &line));
            }
            lines
        };
        for line in lines {
            (self.sink)(line);
        }
    }

    /// Emit what is left of an unterminated line, ended, so the next writer
    /// starts on a line of its own.
    pub fn flush(&self) {
        let rest = std::mem::take(&mut *self.partial.lock_recover());
        if !rest.is_empty() {
            (self.sink)(lead(&self.machine, &format!("{rest}\n")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture() -> (OutputSink, Arc<Mutex<Vec<String>>>) {
        let got: Arc<Mutex<Vec<String>>> = Arc::default();
        let into = Arc::clone(&got);
        (Arc::new(move |s: String| into.lock().unwrap().push(s)), got)
    }

    #[test]
    fn every_line_is_led_by_the_machine() {
        let (sink, got) = capture();
        let p = LinePrefix::new("vm01", sink);
        let out = p.sink();
        out("one\ntw".into());
        out("o\n".into());
        out("three".into());
        p.flush();
        assert_eq!(
            *got.lock().unwrap(),
            vec!["vm01: one\n", "vm01: two\n", "vm01: three\n"]
        );
    }

    #[test]
    fn a_line_that_already_names_the_machine_is_left_alone() {
        assert_eq!(lead("vm01", "vm01: ready\n"), "vm01: ready\n");
        assert_eq!(
            lead("vm01", "\"vm01\": waiting for its shares\n"),
            "\"vm01\": waiting for its shares\n"
        );
        assert_eq!(
            lead("vm01", "vm011: someone else\n"),
            "vm01: vm011: someone else\n",
            "a longer name sharing the prefix is not this machine"
        );
        assert_eq!(
            lead("vm01", "e2e-provision done on vm01\n"),
            "vm01: e2e-provision done on vm01\n"
        );
    }

    #[test]
    fn a_failure_outranks_the_cancellations_it_caused() {
        let real = anyhow::anyhow!("provision a.ws: boom");
        let cancelled = anyhow::Error::new(ProvisionCancelled);
        let script = anyhow::Error::new(ScriptCancelled).context("provision b.ws");
        assert!(is_cancelled(&cancelled) && is_cancelled(&script));
        assert!(!is_cancelled(&real));
        let held = first_failure(cancelled, real);
        assert_eq!(held.to_string(), "provision a.ws: boom");
        let held = first_failure(held, script);
        assert_eq!(held.to_string(), "provision a.ws: boom");
    }
}
