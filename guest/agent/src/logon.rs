//! The cached logon and its lifetime (PRD §19.2).
//!
//! One measured `LogonUser` costs ~97 ms, so a cache is what keeps attach
//! latency a non-issue — but its real job is sameness: three channels that
//! resolve the same account share one token, one `LogonId` and one ticket
//! cache, which is what makes *"the SFTP logon is the same logon"* true by
//! construction rather than by discipline.
//!
//! The policy is here, portable and testable with no guest; what a logon
//! *is* stays with the platform that mints it (`windows/logon.rs`), and the
//! cache only promises to drop it — which is where the profile is unloaded.
//!
//! **Keyed on (account, secret, machine).** The machine is the cache itself:
//! it lives in the agent, which lives and dies with the machine, so a stopped
//! machine cannot leave a logon behind. The other two are the key's whole
//! content, and what is *not* in it matters as much — no label, so two labels
//! naming one account share a session; and the secret, so a changed password
//! mints a fresh logon rather than failing against a stale one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an unused logon survives before the sweeper drops it. Aligned
/// with the `ControlPersist` the generated SSH alias sets (§19.3): a
/// developer whose editor reconnects inside the window keeps the same
/// session, and one who walks away stops holding a hive mounted.
pub const IDLE_GRACE: Duration = Duration::from_secs(10 * 60);

/// How old a logon may get before it is recycled at the next idle moment.
///
/// A dev box left up over a weekend would otherwise wake holding a logon
/// whose TGT expired days ago, which surfaces as "the share stopped working"
/// with no visible cause. Ten hours is the domain default ticket lifetime;
/// recycling slightly early costs one ~97 ms logon.
pub const TICKET_LIFETIME: Duration = Duration::from_secs(10 * 60 * 60);

/// How often the sweeper looks for logons to drop.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// What a cached logon is keyed on. The machine is the cache; the label
/// deliberately is not part of it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogonKey {
    account: String,
    secret: String,
}

impl LogonKey {
    pub fn new(account: &str, secret: &str) -> LogonKey {
        LogonKey {
            account: account.to_string(),
            secret: secret.to_string(),
        }
    }
}

/// A minted logon and when it was minted. Handed out behind an `Arc` so a
/// channel can hold it open, and dropped — token closed, profile unloaded —
/// only once the cache and every channel have let go.
pub struct Held<T> {
    pub value: T,
    minted: Instant,
}

impl<T> std::ops::Deref for Held<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

struct Slot<T> {
    held: Arc<Held<T>>,
    /// When a channel last took this logon. Combined with "no channel holds
    /// it", this is what the idle grace is measured from.
    last_used: Instant,
}

/// The agent's logon cache.
pub struct LogonCache<T> {
    slots: Mutex<HashMap<LogonKey, Slot<T>>>,
    grace: Duration,
    lifetime: Duration,
}

impl<T> LogonCache<T> {
    pub fn new() -> LogonCache<T> {
        LogonCache::with_policy(IDLE_GRACE, TICKET_LIFETIME)
    }

    pub fn with_policy(grace: Duration, lifetime: Duration) -> LogonCache<T> {
        LogonCache {
            slots: Mutex::new(HashMap::new()),
            grace,
            lifetime,
        }
    }

    /// The logon for `key`, minting one if the cache has none it can still
    /// offer.
    ///
    /// A cached logon older than the ticket lifetime is recycled here — but
    /// only when no channel holds it, because yanking a token out from under
    /// a live shell would end the session for a reason the developer cannot
    /// see. A long-lived channel therefore keeps its (aging) logon until it
    /// closes, which is the one place this trades correctness of the ticket
    /// against not breaking a session in progress.
    pub fn get_or_mint(
        &self,
        key: LogonKey,
        now: Instant,
        mint: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<Arc<Held<T>>> {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&key) {
            let stale = now.duration_since(slot.held.minted) >= self.lifetime;
            if !(stale && idle(&slot.held)) {
                slot.last_used = now;
                return Ok(slot.held.clone());
            }
            slots.remove(&key);
        }
        // Minting can block for ~97 ms with the map locked. That is
        // deliberate: two channels opening as the same account at once must
        // end up on one logon, not two.
        let held = Arc::new(Held {
            value: mint()?,
            minted: now,
        });
        slots.insert(
            key,
            Slot {
                held: held.clone(),
                last_used: now,
            },
        );
        Ok(held)
    }

    /// Drop every logon no channel holds and nothing has taken for the idle
    /// grace. Dropping is what closes the token and unloads the profile — a
    /// hive left mounted for the machine's life is the failure this exists
    /// to prevent.
    pub fn sweep(&self, now: Instant) {
        self.slots.lock().unwrap().retain(|_, slot| {
            !(idle(&slot.held) && now.duration_since(slot.last_used) >= self.grace)
        });
    }

    /// How many logons the cache holds.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.slots.lock().unwrap().len()
    }
}

impl<T> Default for LogonCache<T> {
    fn default() -> Self {
        LogonCache::new()
    }
}

/// Whether no channel holds this logon — the cache's own reference is the
/// only one left.
fn idle<T>(held: &Arc<Held<T>>) -> bool {
    Arc::strong_count(held) == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stand-in for the platform's token: counts how many were minted and
    /// records the drop, which on Windows is where the profile is unloaded.
    struct FakeToken {
        id: usize,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for FakeToken {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Minter {
        next: AtomicUsize,
        dropped: Arc<AtomicUsize>,
    }

    impl Minter {
        fn new() -> Minter {
            Minter {
                next: AtomicUsize::new(0),
                dropped: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn mint(&self) -> std::io::Result<FakeToken> {
            Ok(FakeToken {
                id: self.next.fetch_add(1, Ordering::SeqCst),
                dropped: self.dropped.clone(),
            })
        }
        fn minted(&self) -> usize {
            self.next.load(Ordering::SeqCst)
        }
        fn dropped(&self) -> usize {
            self.dropped.load(Ordering::SeqCst)
        }
    }

    /// §19.2: the key is (account, secret, machine) and *not* the label, so
    /// two labels naming one account share a session — which is what makes
    /// the SFTP logon and the shell's logon the same logon.
    #[test]
    fn two_channels_naming_one_account_share_one_logon() {
        let cache = LogonCache::new();
        let m = Minter::new();
        let now = Instant::now();
        let a = cache
            .get_or_mint(LogonKey::new(r"PROBE\dev", "s3cret"), now, || m.mint())
            .unwrap();
        let b = cache
            .get_or_mint(LogonKey::new(r"PROBE\dev", "s3cret"), now, || m.mint())
            .unwrap();
        assert_eq!(m.minted(), 1);
        assert_eq!(a.id, b.id);
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// The secret is in the key so a changed password mints a fresh logon
    /// rather than failing against a stale token.
    #[test]
    fn a_changed_secret_mints_a_fresh_logon() {
        let cache = LogonCache::new();
        let m = Minter::new();
        let now = Instant::now();
        let old = cache
            .get_or_mint(LogonKey::new(r"PROBE\dev", "old"), now, || m.mint())
            .unwrap();
        let new = cache
            .get_or_mint(LogonKey::new(r"PROBE\dev", "new"), now, || m.mint())
            .unwrap();
        assert_eq!(m.minted(), 2);
        assert_ne!(old.id, new.id);
    }

    /// A logon lives while any channel uses it plus a bounded idle grace.
    /// Dropping it is what unloads the profile.
    #[test]
    fn an_idle_logon_is_dropped_after_the_grace_and_a_held_one_is_not() {
        let cache = LogonCache::with_policy(Duration::from_secs(600), TICKET_LIFETIME);
        let m = Minter::new();
        let t0 = Instant::now();
        let held = cache
            .get_or_mint(LogonKey::new("dev", "s"), t0, || m.mint())
            .unwrap();

        // A channel still holds it: age alone does not drop it.
        cache.sweep(t0 + Duration::from_secs(3600));
        assert_eq!(cache.len(), 1);
        assert_eq!(m.dropped(), 0);

        drop(held);
        // Inside the grace, a reconnecting client still gets the same logon.
        cache.sweep(t0 + Duration::from_secs(599));
        assert_eq!(cache.len(), 1);
        cache.sweep(t0 + Duration::from_secs(600));
        assert_eq!(cache.len(), 0);
        assert_eq!(m.dropped(), 1, "the profile is unloaded when the logon is");
    }

    /// Every take restarts the grace: a session that keeps opening channels
    /// never loses its logon underneath it.
    #[test]
    fn taking_a_logon_restarts_its_idle_grace() {
        let cache = LogonCache::with_policy(Duration::from_secs(600), TICKET_LIFETIME);
        let m = Minter::new();
        let t0 = Instant::now();
        drop(
            cache
                .get_or_mint(LogonKey::new("dev", "s"), t0, || m.mint())
                .unwrap(),
        );
        let later = t0 + Duration::from_secs(500);
        drop(
            cache
                .get_or_mint(LogonKey::new("dev", "s"), later, || m.mint())
                .unwrap(),
        );
        assert_eq!(m.minted(), 1);
        cache.sweep(later + Duration::from_secs(599));
        assert_eq!(cache.len(), 1);
    }

    /// §19.2: recycled at idle once older than its Kerberos ticket lifetime.
    /// A box left up over a weekend would otherwise wake holding a logon
    /// whose TGT expired days ago.
    #[test]
    fn a_logon_older_than_its_ticket_is_recycled_at_idle() {
        let cache = LogonCache::with_policy(Duration::from_secs(600), Duration::from_secs(36_000));
        let m = Minter::new();
        let t0 = Instant::now();
        let first = cache
            .get_or_mint(LogonKey::new("dev", "s"), t0, || m.mint())
            .unwrap();
        let weekend = t0 + Duration::from_secs(3 * 24 * 3600);

        // While a channel holds it, the aging logon is kept: ending a live
        // shell for an invisible reason is worse than an aging ticket.
        let same = cache
            .get_or_mint(LogonKey::new("dev", "s"), weekend, || m.mint())
            .unwrap();
        assert_eq!(m.minted(), 1);
        assert_eq!(same.id, first.id);

        drop(first);
        drop(same);
        let fresh = cache
            .get_or_mint(LogonKey::new("dev", "s"), weekend, || m.mint())
            .unwrap();
        assert_eq!(m.minted(), 2);
        assert_ne!(fresh.id, 0);
        assert_eq!(m.dropped(), 1);
    }

    /// A logon that cannot be minted leaves nothing behind to be found by
    /// the next channel — §19.2's "never a silent fallback" needs the cache
    /// to hold no half-entry.
    #[test]
    fn a_failed_mint_caches_nothing() {
        let cache: LogonCache<FakeToken> = LogonCache::new();
        let now = Instant::now();
        let Err(err) = cache.get_or_mint(LogonKey::new(r"PROBE\dev", "wrong"), now, || {
            Err(std::io::Error::other(
                "the user name or password is incorrect",
            ))
        }) else {
            panic!("a wrong secret must not mint a logon");
        };
        assert!(err.to_string().contains("password is incorrect"));
        assert_eq!(cache.len(), 0);
    }
}
