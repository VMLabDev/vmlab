//! Logging for messages a guest can trigger at will.
//!
//! A guest drives the FUSE server with requests it chooses, so every message
//! the server prints about a request is one the guest can repeat as fast as
//! it can fill the queue. Each such message goes through a [`GuestLog`]: at
//! most [`GUEST_LOG_BURST`] lines per [`GUEST_LOG_WINDOW`], each one cut at
//! [`GUEST_LOG_LINE_MAX`] bytes while it is formatted, and one line that
//! counts what was dropped when the next window opens. No line carries a
//! guest buffer: a request is named by its opcode, its length and the errno
//! it was answered with.

use std::fmt::{self, Write as _};
use std::time::{Duration, Instant};

use log::Level;
use parking_lot::Mutex;

/// Lines let through in one window.
pub(crate) const GUEST_LOG_BURST: u32 = 10;
/// The length of a window.
pub(crate) const GUEST_LOG_WINDOW: Duration = Duration::from_secs(10);
/// The most bytes of one line, the truncation marker included.
pub(crate) const GUEST_LOG_LINE_MAX: usize = 256;
const TRUNCATED: &str = " [truncated]";
const TARGET: &str = "vmlab_hv_devices::virtio::fs::fuse";

/// Where the lines go: the `log` facade, or a test's collector.
pub(crate) type GuestLogSink = Box<dyn Fn(Level, &str) + Send + Sync>;

/// A rate limit and a length cap on the server's guest-triggered messages.
pub(crate) struct GuestLog {
    state: Mutex<Window>,
    sink: GuestLogSink,
}

#[derive(Default)]
struct Window {
    start: Option<Instant>,
    emitted: u32,
    suppressed: u64,
}

impl Default for GuestLog {
    fn default() -> Self {
        Self::with_sink(Box::new(|level, line| {
            log::log!(target: TARGET, level, "{line}");
        }))
    }
}

impl GuestLog {
    pub(crate) fn with_sink(sink: GuestLogSink) -> Self {
        GuestLog {
            state: Mutex::new(Window::default()),
            sink,
        }
    }

    /// Log one guest-triggered message, if the window has room for it.
    pub(crate) fn log(&self, level: Level, args: fmt::Arguments<'_>) {
        self.log_at(Instant::now(), level, args);
    }

    pub(crate) fn log_at(&self, now: Instant, level: Level, args: fmt::Arguments<'_>) {
        let suppressed = {
            let mut window = self.state.lock();
            let mut suppressed = 0;
            let expired = window
                .start
                .is_none_or(|start| now.saturating_duration_since(start) >= GUEST_LOG_WINDOW);
            if expired {
                suppressed = std::mem::take(&mut window.suppressed);
                window.start = Some(now);
                window.emitted = 0;
            }
            if window.emitted >= GUEST_LOG_BURST {
                window.suppressed += 1;
                return;
            }
            window.emitted += 1;
            suppressed
        };
        if suppressed > 0 {
            (self.sink)(
                Level::Warn,
                &format!(
                    "fuse: {suppressed} guest-triggered messages suppressed in the last {}s",
                    GUEST_LOG_WINDOW.as_secs()
                ),
            );
        }
        (self.sink)(level, &bounded(args, GUEST_LOG_LINE_MAX));
    }
}

/// Format `args` into at most `max` bytes. Formatting stops at the cap, so
/// a large argument costs no more than the cap.
pub(crate) fn bounded(args: fmt::Arguments<'_>, max: usize) -> String {
    struct Capped {
        text: String,
        room: usize,
        full: bool,
    }
    impl fmt::Write for Capped {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if s.len() <= self.room {
                self.text.push_str(s);
                self.room -= s.len();
                return Ok(());
            }
            let mut end = self.room;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            self.text.push_str(&s[..end]);
            self.room = 0;
            self.full = true;
            Err(fmt::Error)
        }
    }
    let room = max.saturating_sub(TRUNCATED.len());
    let mut out = Capped {
        text: String::with_capacity(max.min(GUEST_LOG_LINE_MAX)),
        room,
        full: false,
    };
    let _ = out.write_fmt(args);
    if out.full {
        out.text.push_str(TRUNCATED);
    }
    out.text
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;

    pub(crate) type Lines = Arc<Mutex<Vec<(Level, String)>>>;

    pub(crate) fn collecting() -> (GuestLog, Lines) {
        let lines: Lines = Arc::default();
        let sink_lines = lines.clone();
        let log = GuestLog::with_sink(Box::new(move |level, line| {
            sink_lines.lock().push((level, line.to_owned()));
        }));
        (log, lines)
    }

    #[test]
    fn a_burst_passes_and_the_rest_is_counted_once_the_window_turns() {
        let (log, lines) = collecting();
        let start = Instant::now();
        for i in 0..1000 {
            log.log_at(start, Level::Warn, format_args!("message {i}"));
        }
        assert_eq!(lines.lock().len(), GUEST_LOG_BURST as usize);

        log.log_at(
            start + GUEST_LOG_WINDOW,
            Level::Warn,
            format_args!("after"),
        );
        let lines = lines.lock();
        assert_eq!(lines.len(), GUEST_LOG_BURST as usize + 2);
        let notice = &lines[GUEST_LOG_BURST as usize].1;
        assert!(notice.contains(&format!("{} guest-triggered", 1000 - GUEST_LOG_BURST)));
        assert_eq!(lines.last().unwrap().1, "after");
    }

    #[test]
    fn a_line_is_cut_while_it_is_formatted() {
        let huge = vec![0xffu8; 1 << 20];
        let line = bounded(format_args!("buffer {huge:?}"), GUEST_LOG_LINE_MAX);
        assert!(line.len() <= GUEST_LOG_LINE_MAX, "{}", line.len());
        assert!(line.ends_with(TRUNCATED));
        assert_eq!(bounded(format_args!("short {}", 1), 64), "short 1");
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let text = "é".repeat(400);
        let line = bounded(format_args!("{text}"), 101);
        assert!(line.len() <= 101);
        assert!(line.ends_with(TRUNCATED));
    }
}
