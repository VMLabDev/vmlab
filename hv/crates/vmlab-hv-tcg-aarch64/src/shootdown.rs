//! What one CPU of a machine must tell the others (card C13, 3e card J7).
//!
//! The `TLBI` operations of the Inner Shareable domain (`VMALLE1IS`,
//! `VAE1IS`, `ASIDE1IS` and the rest) invalidate the TLB of every CPU, not
//! only of the one that runs them. A page that gets translated code loses
//! its write entries in the fast TLB of every CPU before its code is read
//! (3e DESIGN.md H11c). A store into a page that holds translated code of
//! some CPU tells every other CPU to look at its blocks again before it runs
//! one (H11a).
//!
//! Each CPU has a mailbox. The CPU that posts applies the invalidation to
//! itself, posts it to every other mailbox, kicks every other CPU through
//! its seat (H10c), so that a CPU in compiled code leaves it at its next
//! block head, and then waits until each other CPU that is inside its run
//! loop has taken its mail. A CPU takes its mail before every block that the
//! loop starts and while it waits itself, so two CPUs that post at the same
//! time do not wait on each other. A waiter kicks again while it waits: a
//! kick can be lost (`vmlab_hv_tcg_jit::seat`). A CPU outside its run loop
//! (halted, paused, or not started) holds no translation that it uses, and
//! takes its mail when it runs again. So when the operation completes, no
//! other CPU translates through what it invalidated, which is what the guest
//! counts on after the `DSB ISH` that follows it.
//!
//! The rule is general knowledge of the architecture: a TLB maintenance
//! operation of the Inner Shareable domain affects every PE of that domain.
//! No file of S2 to S5 was opened, no QEMU source (X1) and no Arm pseudocode
//! (X2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use vmlab_hv_tcg_jit::seat::Seat;

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
    /// The write entries of the fast TLB for the physical page at this
    /// address: another CPU translates code there (H11c).
    CodePage(u64),
}

/// What a CPU takes from its mailbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mail {
    pub invalidations: Vec<Invalidation>,
    /// Another CPU stored into a page that holds translated code: the
    /// blocks are looked at again before the next one runs.
    pub code: bool,
}

#[derive(Default)]
struct Mailbox {
    /// There is mail. Read before every block, so it is one load.
    pending: AtomicBool,
    /// The CPU is inside its run loop and translates.
    running: AtomicBool,
    mail: Mutex<Mail>,
    /// The seat of the compiled code of the CPU, which a post kicks.
    seat: Mutex<Option<Arc<Seat>>>,
}

/// The mailboxes of the CPUs of one machine.
pub struct Shootdown {
    cpus: Vec<Mailbox>,
}

/// The most invalidations that a mailbox keeps one by one. More become one
/// invalidation of every entry, which drops the fast TLB whole too.
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

    /// Post `invalidation` to every CPU but `from`, and kick them.
    pub(crate) fn post(&self, from: usize, invalidation: Invalidation) {
        self.post_with(from, |mail| {
            let invalidations = &mut mail.invalidations;
            if invalidations.len() >= MAIL || invalidations.last() == Some(&Invalidation::All) {
                invalidations.clear();
                invalidations.push(Invalidation::All);
            } else {
                invalidations.push(invalidation);
            }
        });
    }

    /// Tell every CPU but `from` that a page of translated code was
    /// written, and kick them.
    pub(crate) fn post_code(&self, from: usize) {
        self.post_with(from, |mail| mail.code = true);
    }

    fn post_with(&self, from: usize, add: impl Fn(&mut Mail)) {
        for (id, mailbox) in self.cpus.iter().enumerate() {
            if id == from {
                continue;
            }
            {
                let mut mail = mailbox.mail.lock().unwrap_or_else(|e| e.into_inner());
                add(&mut mail);
                mailbox.pending.store(true, Ordering::SeqCst);
            }
            Self::kick(mailbox);
        }
    }

    fn kick(mailbox: &Mailbox) {
        if let Some(seat) = &*mailbox.seat.lock().unwrap_or_else(|e| e.into_inner()) {
            seat.kick();
        }
    }

    /// Kick again every CPU but `from` that runs and has not taken its
    /// mail: a kick can be lost.
    pub(crate) fn kick_outstanding(&self, from: usize) {
        for (id, mailbox) in self.cpus.iter().enumerate() {
            if id != from
                && mailbox.running.load(Ordering::Acquire)
                && mailbox.pending.load(Ordering::Acquire)
            {
                Self::kick(mailbox);
            }
        }
    }

    /// The mail of `cpu`, when it has some.
    pub(crate) fn take(&self, cpu: usize) -> Option<Mail> {
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
        self.cpus[cpu].running.store(running, Ordering::SeqCst);
    }

    /// The seat that the compiled code of `cpu` runs on, which a post
    /// kicks.
    pub(crate) fn seat(&self, cpu: usize, seat: &Arc<Seat>) {
        let mut place = self.cpus[cpu]
            .seat
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !place.as_ref().is_some_and(|now| Arc::ptr_eq(now, seat)) {
            *place = Some(seat.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(invalidations: Vec<Invalidation>) -> Mail {
        Mail {
            invalidations,
            code: false,
        }
    }

    #[test]
    fn a_post_kicks_the_seat_of_every_other_cpu_and_code_mail_is_a_flag() {
        let shootdown = Shootdown::new(2);
        let seat = Arc::new(Seat::new());
        shootdown.seat(1, &seat);
        // SAFETY: the seat lives, and no code runs on it.
        unsafe {
            (*seat.context()).budget = 100;
        }
        shootdown.post_code(0);
        shootdown.post_code(0);
        // SAFETY: as above.
        unsafe {
            assert!(vmlab_hv_tcg_jit::seat::kicked(seat.context()));
            assert_eq!(vmlab_hv_tcg_jit::seat::left(seat.context()), 100);
        }
        assert_eq!(
            shootdown.take(1),
            Some(Mail {
                invalidations: Vec::new(),
                code: true
            })
        );
        assert_eq!(shootdown.take(0), None);
    }

    #[test]
    fn mail_goes_to_every_other_cpu_once() {
        let shootdown = Shootdown::new(3);
        shootdown.post(1, Invalidation::Asid(7));
        assert_eq!(shootdown.take(0), Some(mail(vec![Invalidation::Asid(7)])));
        assert_eq!(shootdown.take(0), None);
        assert_eq!(shootdown.take(1), None);
        assert_eq!(shootdown.take(2), Some(mail(vec![Invalidation::Asid(7)])));
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
        assert_eq!(shootdown.take(1), Some(mail(vec![Invalidation::All])));
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
