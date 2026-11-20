//! A host-side event trace of the GPU display path, for the tearing
//! investigation of card #233 (guest/win3d/docs/tearing.md).
//!
//! Off unless the environment of vmlab-hv sets `VMLAB_HV_GPU_TRACE` to a
//! directory. Then each process appends one line per event to
//! `<dir>/gpu-trace-<pid>.txt`:
//!
//! ```text
//! <CLOCK_MONOTONIC ns> <event> key=value ...
//! ```
//!
//! The lines are buffered and written at most every 100 ms, so that a trace
//! changes the timing of the path it records as little as it can. An event
//! of the last 100 ms before the process ends can be lost.
//!
//! `VMLAB_HV_GPU_TRACE_REREAD=1` also reads each console readback twice and
//! records the rows in which the two reads differ: a write into a published
//! scanout while the display reads it.

use std::fmt::Arguments;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Sink {
    out: BufWriter<File>,
    flushed: Instant,
}

struct Trace {
    sink: Mutex<Sink>,
    reread: bool,
}

static TRACE: OnceLock<Option<Trace>> = OnceLock::new();

const FLUSH_EVERY: Duration = Duration::from_millis(100);

fn open(dir: &Path, reread: bool) -> Option<Trace> {
    let path = dir.join(format!("gpu-trace-{}.txt", std::process::id()));
    match File::options().create(true).append(true).open(&path) {
        Ok(file) => {
            log::info!("GPU trace: writing {}", path.display());
            Some(Trace {
                sink: Mutex::new(Sink {
                    out: BufWriter::with_capacity(1 << 20, file),
                    flushed: Instant::now(),
                }),
                reread,
            })
        }
        Err(error) => {
            log::warn!("GPU trace: cannot open {}: {error}", path.display());
            None
        }
    }
}

fn trace() -> Option<&'static Trace> {
    TRACE
        .get_or_init(|| {
            let dir = std::env::var_os("VMLAB_HV_GPU_TRACE").filter(|dir| !dir.is_empty())?;
            let reread = std::env::var_os("VMLAB_HV_GPU_TRACE_REREAD").is_some_and(|v| v == "1");
            open(Path::new(&dir), reread)
        })
        .as_ref()
}

/// Whether the trace is on.
pub fn on() -> bool {
    trace().is_some()
}

/// Whether each console readback is read twice ([`on`] must hold too).
pub fn reread() -> bool {
    trace().is_some_and(|t| t.reread)
}

/// CLOCK_MONOTONIC in nanoseconds, the clock of every event.
pub fn now_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the clock is valid and time points to writable timespec storage.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return 0;
    }
    time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64
}

/// Record one event at the time `at_ns`. Does nothing with the trace off.
pub fn write_at(at_ns: u64, event: &str, fields: Arguments<'_>) {
    let Some(trace) = trace() else {
        return;
    };
    let Ok(mut sink) = trace.sink.lock() else {
        return;
    };
    let _ = writeln!(sink.out, "{at_ns} {event} {fields}");
    if sink.flushed.elapsed() >= FLUSH_EVERY {
        let _ = sink.out.flush();
        sink.flushed = Instant::now();
    }
}

/// Record one event now. Does nothing with the trace off.
pub fn write(event: &str, fields: Arguments<'_>) {
    if on() {
        write_at(now_ns(), event, fields);
    }
}

/// Write out what is buffered.
pub fn flush() {
    if let Some(trace) = trace()
        && let Ok(mut sink) = trace.sink.lock()
    {
        let _ = sink.out.flush();
        sink.flushed = Instant::now();
    }
}

/// Rows of two reads of one image that differ, as (first, last, count).
/// None when the reads are equal.
pub fn differing_rows(first: &[u8], second: &[u8], stride: usize) -> Option<(usize, usize, usize)> {
    if stride == 0 {
        return None;
    }
    let mut result: Option<(usize, usize, usize)> = None;
    for (row, (a, b)) in first.chunks(stride).zip(second.chunks(stride)).enumerate() {
        if a != b {
            let (start, _, count) = result.unwrap_or((row, row, 0));
            result = Some((start, row, count + 1));
        }
    }
    result
}

/// A cheap digest of an image, to tell two reads or two frames apart.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let (words, rest) = bytes.as_chunks::<8>();
    for word in words {
        hash = (hash ^ u64::from_le_bytes(*word))
            .rotate_left(23)
            .wrapping_mul(0x0100_0000_01b3);
    }
    for &byte in rest {
        hash = (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_writes_one_line_per_event_with_its_time() {
        let dir = tempfile::tempdir().unwrap();
        let trace = open(dir.path(), false).unwrap();
        {
            let mut sink = trace.sink.lock().unwrap();
            writeln!(sink.out, "{} {} {}", 42, "publish", format_args!("ts={} res={}", 7, 3)).unwrap();
            sink.out.flush().unwrap();
        }
        let path = dir.path().join(format!("gpu-trace-{}.txt", std::process::id()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "42 publish ts=7 res=3\n");
    }

    #[test]
    fn the_trace_is_off_without_its_variable() {
        // The test process does not set VMLAB_HV_GPU_TRACE.
        if std::env::var_os("VMLAB_HV_GPU_TRACE").is_none() {
            assert!(!on());
            assert!(!reread());
            write("publish", format_args!("ts=1"));
        }
    }

    #[test]
    fn differing_rows_name_the_first_and_last_row_and_their_count() {
        let a = vec![0u8; 4 * 8];
        let mut b = a.clone();
        assert_eq!(differing_rows(&a, &b, 8), None);
        b[8] = 1;
        b[3 * 8 + 7] = 1;
        assert_eq!(differing_rows(&a, &b, 8), Some((1, 3, 2)));
        assert_ne!(digest(&a), digest(&b));
        assert_eq!(digest(&a), digest(&a.clone()));
    }
}
