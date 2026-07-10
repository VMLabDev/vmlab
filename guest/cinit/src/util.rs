//! Tiny error plumbing (no anyhow in the guest — keep the binary lean) plus
//! the poweroff path every exit funnels through.

use std::thread;
use std::time::Duration;

/// String-typed errors: PID 1 only ever prints them to the console and powers
/// off, so a rich error type buys nothing.
pub type Result<T> = std::result::Result<T, String>;

/// `.ctx("what")` — attach context the way anyhow's `.context()` would.
pub trait Ctx<T> {
    fn ctx(self, what: &str) -> Result<T>;
}

impl<T, E: std::fmt::Display> Ctx<T> for std::result::Result<T, E> {
    fn ctx(self, what: &str) -> Result<T> {
        self.map_err(|e| format!("{what}: {e}"))
    }
}

/// Flush, sync disks, give the console a beat to drain, and power off.
/// Never returns. Used for both the normal exit path and fatal errors (the
/// host treats a missing `exited` event as a crash).
pub fn power_off() -> ! {
    unsafe { libc::sync() };
    thread::sleep(Duration::from_millis(200));
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF);
    // reboot(2) only fails if we are somehow not privileged; spin so the
    // signature stays `!`.
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
