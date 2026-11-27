//! TLB maintenance that reaches the other CPUs of a machine (card C13): the
//! `TLBI` operations of the Inner Shareable domain (`VMALLE1IS`, `VAE1IS`,
//! `ASIDE1IS` and the rest) invalidate the TLB of every CPU, not only of the
//! one that runs them.
//!
//! Each CPU has a mailbox. The CPU that runs such an operation applies it to
//! its own TLB, posts it to every other mailbox, and then waits until each
//! other CPU that is inside its run loop has taken its mail. A CPU takes its
//! mail before every block and while it waits itself, so two CPUs that post
//! at the same time do not wait on each other. A CPU outside its run loop
//! (halted, paused, or not started) holds no translation that it uses, and
//! takes its mail when it runs again. So when the operation completes, no
//! other CPU translates through what it invalidated, which is what the guest
//! counts on after the `DSB ISH` that follows it.
//!
//! The rule is general knowledge of the architecture: a TLB maintenance
//! operation of the Inner Shareable domain affects every PE of that domain.
//! No file of S2 to S5 was opened, no QEMU source (X1) and no Arm pseudocode
//! (X2).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// One TLB invalidation of the EL1&0 regime, as each CPU applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalidation {
    /// Every entry (`VMALLE1`).
    All,
    /// The entries of one address, of one ASID or of every ASID (`VAE1`,
    /// `VALE1`, `VAAE1`, `VAALE1`). `argument` is the register of the
    /// operation, as the TLB reads it.
    Page { argument: u64, asid: Option<u16> },
    /// The entries of one ASID (`ASIDE1`).
    Asid(u16),
}

#[derive(Default)]
struct Mailbox {
    /// There is mail. Read before every block, so it is one load.
    pending: AtomicBool,
    /// The CPU is inside its run loop and translates.
    running: AtomicBool,
    mail: Mutex<Vec<Invalidation>>,
}

/// The mailboxes of the CPUs of one machine.
pub struct Shootdown {
    cpus: Vec<Mailbox>,
}

/// The most invalidations that a mailbox keeps one by one. More become one
/// invalidation of every entry.
const MAIL: usize = 64;

impl Shootdown {
    pub fn new(cpus: usize) -> Self {
        Self {
            cpus: (0..cpus).map(|_| Mailbox::default()).collect(),
        }
    }

    pub fn cpus(&self) -> usize {
        self.cpus.len()
    }

    /// Post `invalidation` to every CPU but `from`.
    pub(crate) fn post(&self, from: usize, invalidation: Invalidation) {
        for (id, mailbox) in self.cpus.iter().enumerate() {
            if id == from {
                continue;
            }
            let mut mail = mailbox.mail.lock().unwrap_or_else(|e| e.into_inner());
            if mail.len() >= MAIL || mail.last() == Some(&Invalidation::All) {
                mail.clear();
                mail.push(Invalidation::All);
            } else {
                mail.push(invalidation);
            }
            mailbox.pending.store(true, Ordering::Release);
        }
    }

    /// The mail of `cpu`, when it has some.
    pub(crate) fn take(&self, cpu: usize) -> Option<Vec<Invalidation>> {
        let mailbox = &self.cpus[cpu];
        if !mailbox.pending.load(Ordering::Acquire) {
            return None;
        }
        let mut mail = mailbox.mail.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.pending.store(false, Ordering::Release);
        Some(std::mem::take(&mut *mail))
    }

    /// Whether some CPU but `from` that runs has mail that it has not taken.
    pub(crate) fn outstanding(&self, from: usize) -> bool {
        self.cpus.iter().enumerate().any(|(id, mailbox)| {
            id != from
                && mailbox.running.load(Ordering::Acquire)
                && mailbox.pending.load(Ordering::Acquire)
        })
    }

    /// `cpu` enters or leaves its run loop. A CPU that enters has mail
    /// posted before it is in the loop taken by its first block.
    pub(crate) fn set_running(&self, cpu: usize, running: bool) {
        self.cpus[cpu].running.store(running, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_goes_to_every_other_cpu_once() {
        let shootdown = Shootdown::new(3);
        shootdown.post(1, Invalidation::Asid(7));
        assert_eq!(shootdown.take(0), Some(vec![Invalidation::Asid(7)]));
        assert_eq!(shootdown.take(0), None);
        assert_eq!(shootdown.take(1), None);
        assert_eq!(shootdown.take(2), Some(vec![Invalidation::Asid(7)]));
    }

    #[test]
    fn a_full_mailbox_becomes_one_invalidation_of_everything() {
        let shootdown = Shootdown::new(2);
        for page in 0..(MAIL as u64 + 5) {
            shootdown.post(
                0,
                Invalidation::Page {
                    argument: page,
                    asid: None,
                },
            );
        }
        assert_eq!(shootdown.take(1), Some(vec![Invalidation::All]));
    }

    #[test]
    fn only_a_running_cpu_with_mail_is_waited_for() {
        let shootdown = Shootdown::new(2);
        shootdown.post(0, Invalidation::All);
        assert!(!shootdown.outstanding(0));
        shootdown.set_running(1, true);
        assert!(shootdown.outstanding(0));
        shootdown.take(1);
        assert!(!shootdown.outstanding(0));
    }
}
