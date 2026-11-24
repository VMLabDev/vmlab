//! Cooperative cancellation of a running script (§7.2: the first failed step
//! fails `up` and cancels the steps still running).
//!
//! Every host call that waits on the daemon goes through
//! [`block_on`], and `sleep_ms` and the screen polls through [`sleep`]. Once
//! the script's token is cancelled, the next of those drops the work it was
//! waiting on and unwinds the script thread with [`Cancelled`], which the
//! runner turns back into [`ScriptCancelled`]. The runner also checks for
//! cancellation before and after execution and limits each VM's bytecode
//! budget so a script that never calls the host still terminates.
//!
//! The token rides a thread-local, set for the length of one script run by
//! [`enter`], so the host functions need no extra argument.

use std::cell::RefCell;
use std::future::Future;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

thread_local! {
    static TOKEN: RefCell<Option<RunCancellation>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct RunCancellation {
    lifetime: CancellationToken,
    request: Option<CancellationToken>,
}

impl RunCancellation {
    fn is_cancelled(&self) -> bool {
        self.lifetime.is_cancelled()
            || self
                .request
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
    }

    async fn cancelled(&self) {
        if let Some(request) = &self.request {
            tokio::select! {
                _ = self.lifetime.cancelled() => {}
                _ = request.cancelled() => {}
            }
        } else {
            self.lifetime.cancelled().await;
        }
    }
}

/// The unwind payload a cancelled script stops with. Raised with
/// `resume_unwind`, which skips the panic hook: a cancellation is not a
/// crash and writes nothing to the daemon log.
pub(crate) struct Cancelled;

/// A script stopped by cancellation.
#[derive(Debug)]
pub struct ScriptCancelled;

impl std::fmt::Display for ScriptCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for ScriptCancelled {}

/// Clears the thread's token when the script run ends, unwinding included —
/// blocking-pool threads are reused.
pub(crate) struct Entered;

impl Drop for Entered {
    fn drop(&mut self) {
        TOKEN.with(|t| t.borrow_mut().take());
    }
}

/// Make `token` the running script's for the life of the guard.
pub(crate) fn enter(lifetime: CancellationToken, request: Option<CancellationToken>) -> Entered {
    TOKEN.with(|t| *t.borrow_mut() = Some(RunCancellation { lifetime, request }));
    Entered
}

fn current() -> Option<RunCancellation> {
    TOKEN.with(|t| t.borrow().clone())
}

pub(crate) fn check() {
    if current().is_some_and(|token| token.is_cancelled()) {
        stop();
    }
}

fn stop() -> ! {
    std::panic::resume_unwind(Box::new(Cancelled))
}

/// `rt.block_on(fut)`, abandoned the moment the running script is cancelled.
pub(crate) fn block_on<F: Future>(rt: &tokio::runtime::Handle, fut: F) -> F::Output {
    let Some(token) = current() else {
        return rt.block_on(fut);
    };
    if token.is_cancelled() {
        stop();
    }
    let out = rt.block_on(async {
        tokio::select! {
            biased;
            _ = token.cancelled() => None,
            out = fut => Some(out),
        }
    });
    out.unwrap_or_else(|| stop())
}

/// `std::thread::sleep`, cut short when the running script is cancelled.
pub(crate) fn sleep(d: Duration) {
    let Some(token) = current() else {
        std::thread::sleep(d);
        return;
    };
    let deadline = Instant::now() + d;
    loop {
        if token.is_cancelled() {
            stop();
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_huge_timeout_is_an_error_not_a_panic() {
        let token = CancellationToken::new();
        token.cancel();
        let _entered = enter(token, None);
        let _ = sleep(Duration::from_millis(i64::MAX as u64));
    }

    #[test]
    fn without_a_token_nothing_changes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert_eq!(block_on(rt.handle(), async { 7 }), 7);
        sleep(Duration::from_millis(1));
    }

    #[test]
    fn a_cancelled_token_unwinds_with_the_payload() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let token = CancellationToken::new();
        let handle = rt.handle().clone();
        let t = token.clone();
        let waiter = std::thread::spawn(move || {
            let _entered = enter(t, None);
            block_on(&handle, std::future::pending::<()>());
        });
        token.cancel();
        let payload = waiter.join().unwrap_err();
        assert!(payload.is::<Cancelled>());
        assert!(current().is_none(), "the token is this thread's alone");
    }

    #[test]
    fn a_sleep_is_cut_short() {
        let token = CancellationToken::new();
        token.cancel();
        let t = token.clone();
        let started = Instant::now();
        let payload = std::thread::spawn(move || {
            let _entered = enter(t, None);
            sleep(Duration::from_secs(60));
        })
        .join()
        .unwrap_err();
        assert!(payload.is::<Cancelled>());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
