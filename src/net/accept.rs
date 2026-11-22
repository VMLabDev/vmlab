//! What a listener's accept loop does when `accept` fails.
//!
//! `accept` fails at once, and keeps failing, while the process is at its
//! descriptor limit (`EMFILE`, `ENFILE`) or the kernel is out of buffers. A
//! loop that tries again straight away uses a whole core, and one that logs
//! every failure also writes a line per iteration
//! (docs/reviews/networking.md F11). So a loop waits [`PAUSE`] after a
//! failure, and logs the first failure of a run only.

use std::time::Duration;

/// How long a listener waits after a failed `accept`.
pub const PAUSE: Duration = Duration::from_millis(100);

/// The failures of one accept loop.
#[derive(Default)]
pub struct AcceptErrors {
    failing: bool,
}

impl AcceptErrors {
    /// `accept` failed: warn when the last one did not, and wait.
    pub async fn failed(&mut self, listener: &str, error: &std::io::Error) {
        if !self.failing {
            self.failing = true;
            tracing::warn!("{listener}: accept failed: {error}; trying again until it works");
        }
        tokio::time::sleep(PAUSE).await;
    }

    /// `accept` gave a connection.
    pub fn accepted(&mut self) {
        self.failing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_failure_waits_before_the_next_accept() {
        let mut errors = AcceptErrors::default();
        let error = std::io::Error::from_raw_os_error(nix::libc::EMFILE);
        let start = tokio::time::Instant::now();
        errors.failed("test listener", &error).await;
        errors.failed("test listener", &error).await;
        assert_eq!(start.elapsed(), PAUSE * 2);
        assert!(errors.failing);
        errors.accepted();
        assert!(!errors.failing);
    }
}
