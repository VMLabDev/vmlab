//! Reading vmlab's on-disk logs (PRD §8.3). The CLI (`vmlab logs`) and the web
//! UI's log stream both read the same state-dir files directly — there is no
//! daemon RPC for logs — so the enumeration and per-line parsing live here,
//! shared by both.
//!
//! Layout under `state_dir()/labs/<lab>/`:
//!   - `events.jsonl` — structured [`crate::proto::Event`] lines (timestamped)
//!   - `lab.log`      — provision/script output (raw text)
//!   - `vms/<vm>/{serial,qemu,swtpm}.log` — raw per-VM text
//!   - `containers/<name>/console.log` — micro-VM kernel + container stdout/stderr

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::proto::Event;

/// The synthetic source name for lab-level (non-VM) logs.
pub const LAB_SOURCE: &str = "lab";

/// Size at which the logs vmlab appends to itself (`events.jsonl`, `lab.log`)
/// roll over to `<name>.1`. They used to grow without bound for the life of a
/// lab — a long-running lab with chatty provisions could leave hundreds of MB
/// behind, and every reader (`vmlab logs`, the web log stream) paid for it.
pub const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

/// An append-only log file that rolls over at [`MAX_LOG_BYTES`], keeping one
/// previous generation as `<name>.1`.
///
/// Size is tracked as bytes written rather than stat'd per line. A file that
/// cannot be opened leaves the handle empty and writes become no-ops: log
/// history is best-effort and must never take a daemon down.
pub struct AppendLog {
    path: PathBuf,
    file: Option<std::fs::File>,
    size: u64,
    max: u64,
}

impl AppendLog {
    pub fn open(path: PathBuf) -> Self {
        Self::open_with_max(path, MAX_LOG_BYTES)
    }

    fn open_with_max(path: PathBuf, max: u64) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        let size = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        Self {
            path,
            file,
            size,
            max,
        }
    }

    /// Append raw text (callers include their own newlines).
    pub fn write(&mut self, text: &str) {
        if self.size >= self.max {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut()
            && f.write_all(text.as_bytes()).is_ok()
        {
            self.size += text.len() as u64;
        }
    }

    /// Append one line, adding the newline.
    pub fn write_line(&mut self, line: &str) {
        self.write(line);
        self.write("\n");
    }

    fn rotate(&mut self) {
        self.file = None; // close before renaming
        let previous = self.path.with_extension(match self.path.extension() {
            Some(ext) => format!("{}.1", ext.to_string_lossy()),
            None => "1".to_string(),
        });
        if std::fs::rename(&self.path, &previous).is_err() {
            // Couldn't roll over (read-only dir, say): keep appending to the
            // current file rather than losing the log entirely.
            let reopened = Self::open_with_max(std::mem::take(&mut self.path), self.max);
            *self = reopened;
            self.max = u64::MAX;
            return;
        }
        let reopened = Self::open_with_max(std::mem::take(&mut self.path), self.max);
        *self = reopened;
    }
}

/// One parsed log line, tagged with where it came from. Raw lines (serial/qemu/
/// swtpm/lab) pass through verbatim with no timestamp; `events.jsonl` lines are
/// parsed into a timestamp plus a flattened `event key=value …` summary.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// `"lab"`, the VM name, or the container name.
    pub source: String,
    /// `"events" | "lab" | "serial" | "qemu" | "swtpm" | "console"`.
    pub stream: String,
    /// Present only for `events.jsonl` lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<chrono::DateTime<chrono::Utc>>,
    /// Formatted event summary, or the raw line verbatim.
    pub text: String,
}

/// A log file on disk plus the source/stream tags its lines should carry.
#[derive(Debug, Clone)]
pub struct LogFile {
    pub source: String,
    pub stream: String,
    pub path: PathBuf,
}

/// `state_dir()/labs/<lab>` — the directory holding a lab's logs.
pub fn lab_dir(lab: &str) -> PathBuf {
    crate::paths::state_dir().join("labs").join(lab)
}

/// Every log file that currently exists for a lab, in a stable order: the
/// lab-level events then `lab.log`, then each VM's serial/qemu/swtpm (VMs
/// sorted by name), then each container's console log (sorted by name).
/// Re-scanning picks up VMs/containers that start after the stream opens.
pub fn enumerate(lab: &str) -> Vec<LogFile> {
    enumerate_in(&lab_dir(lab))
}

fn enumerate_in(base: &Path) -> Vec<LogFile> {
    let mut files = Vec::new();

    for (stream, name) in [("events", "events.jsonl"), ("lab", "lab.log")] {
        let path = base.join(name);
        if path.is_file() {
            files.push(LogFile {
                source: LAB_SOURCE.to_string(),
                stream: stream.to_string(),
                path,
            });
        }
    }

    let mut vms: Vec<PathBuf> = std::fs::read_dir(base.join("vms"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    vms.sort();
    for vm_dir in vms {
        let Some(vm) = vm_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        for stream in ["serial", "qemu", "swtpm"] {
            let path = vm_dir.join(format!("{stream}.log"));
            if path.is_file() {
                files.push(LogFile {
                    source: vm.to_string(),
                    stream: stream.to_string(),
                    path,
                });
            }
        }
    }

    // Containers: one console.log each (kernel messages + stdout/stderr).
    let mut containers: Vec<PathBuf> = std::fs::read_dir(base.join("containers"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    containers.sort();
    for dir in containers {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let path = dir.join("console.log");
        if path.is_file() {
            files.push(LogFile {
                source: name.to_string(),
                stream: "console".to_string(),
                path,
            });
        }
    }
    files
}

/// Flatten an event into a one-line `event key=value …` summary (no color, no
/// timestamp — callers add those). Shared with the CLI's pretty printer.
pub fn format_event(ev: &Event) -> String {
    let data = match ev.data.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                _ => format!("{k}={v}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        None if ev.data.is_null() => String::new(),
        None => ev.data.to_string(),
    };
    format!("{} {}", ev.event, data).trim_end().to_string()
}

/// Parse one raw line from a log file into a [`LogEntry`]. Lines from the
/// `events` stream are decoded as [`Event`] (falling back to the raw text if
/// they don't parse); every other stream passes through verbatim.
pub fn parse_line(source: &str, stream: &str, raw: &str) -> LogEntry {
    if stream == "events"
        && let Ok(ev) = serde_json::from_str::<Event>(raw)
    {
        return LogEntry {
            source: source.to_string(),
            stream: stream.to_string(),
            ts: Some(ev.ts),
            text: format_event(&ev),
        };
    }
    LogEntry {
        source: source.to_string(),
        stream: stream.to_string(),
        ts: None,
        text: raw.to_string(),
    }
}

/// How far back [`tail`] will read looking for `n` lines. A serial log can be
/// tens of MB; reading it whole to print 200 lines is what this avoids.
const TAIL_SCAN_LIMIT: u64 = 4 * 1024 * 1024;

/// The last `n` lines of a file (empty if it can't be read), read from the end
/// rather than by slurping the whole file.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    tail_with_len(path, n).0
}

/// [`tail`] plus the file length the lines were read at, so a follower can pick
/// up exactly where the backlog ended without a second stat.
pub fn tail_with_len(path: &Path, n: usize) -> (Vec<String>, u64) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (Vec::new(), 0);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if n == 0 || len == 0 {
        return (Vec::new(), len);
    }
    // Walk backwards in growing chunks until we have enough newlines (one more
    // than requested, so the first line isn't a fragment) or run out of budget.
    let mut window = 64 * 1024u64;
    let (text, from) = loop {
        let start = len.saturating_sub(window);
        let mut buf = vec![0u8; (len - start) as usize];
        if f.seek(SeekFrom::Start(start)).is_err() {
            return (Vec::new(), len);
        }
        if std::io::Read::read_exact(&mut f, &mut buf).is_err() {
            return (Vec::new(), len);
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let enough = text.matches('\n').count() > n;
        if enough || start == 0 || window >= TAIL_SCAN_LIMIT {
            break (text, start);
        }
        window = (window * 8).min(TAIL_SCAN_LIMIT);
    };
    // A partial first line (we started mid-file) is dropped.
    let body = if from > 0 {
        match text.find('\n') {
            Some(i) => &text[i + 1..],
            None => "",
        }
    } else {
        text.as_str()
    };
    let all: Vec<&str> = body.lines().collect();
    let start = all.len().saturating_sub(n);
    (all[start..].iter().map(|s| s.to_string()).collect(), len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_events_line_extracts_ts_and_summary() {
        let line = json!({
            "event": "vm.started",
            "lab": "demo",
            "data": {"vm": "web01", "pid": 1234},
            "ts": "2026-06-21T14:32:01Z"
        })
        .to_string();
        let e = parse_line(LAB_SOURCE, "events", &line);
        assert_eq!(e.source, "lab");
        assert_eq!(e.stream, "events");
        assert!(e.ts.is_some());
        assert!(e.text.starts_with("vm.started"));
        assert!(e.text.contains("vm=web01"));
        assert!(e.text.contains("pid=1234"));
    }

    #[test]
    fn parse_raw_line_passes_through() {
        let e = parse_line("web01", "serial", "Booting kernel...");
        assert_eq!(e.source, "web01");
        assert_eq!(e.stream, "serial");
        assert!(e.ts.is_none());
        assert_eq!(e.text, "Booting kernel...");
    }

    #[test]
    fn malformed_events_line_falls_back_to_raw() {
        let e = parse_line(LAB_SOURCE, "events", "not json");
        assert!(e.ts.is_none());
        assert_eq!(e.text, "not json");
    }

    #[test]
    fn enumerate_finds_lab_and_vm_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(base.join("vms/web01")).unwrap();
        std::fs::create_dir_all(base.join("vms/db01")).unwrap();
        std::fs::write(base.join("events.jsonl"), "{}\n").unwrap();
        std::fs::write(base.join("lab.log"), "hi\n").unwrap();
        std::fs::write(base.join("vms/web01/serial.log"), "a\n").unwrap();
        std::fs::write(base.join("vms/web01/qemu.log"), "b\n").unwrap();
        std::fs::write(base.join("vms/db01/serial.log"), "c\n").unwrap();

        let files = enumerate_in(base);
        // lab events + lab.log come first.
        assert_eq!(files[0].stream, "events");
        assert_eq!(files[0].source, "lab");
        assert_eq!(files[1].stream, "lab");
        // VMs are sorted: db01 before web01.
        let vm_sources: Vec<_> = files[2..].iter().map(|f| f.source.as_str()).collect();
        assert_eq!(vm_sources[0], "db01");
        assert!(vm_sources.contains(&"web01"));
        // swtpm.log absent → not listed.
        assert!(!files.iter().any(|f| f.stream == "swtpm"));
    }

    #[test]
    fn enumerate_finds_container_console_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::create_dir_all(base.join("containers/web")).unwrap();
        std::fs::create_dir_all(base.join("containers/empty")).unwrap();
        std::fs::write(base.join("containers/web/console.log"), "hello\n").unwrap();

        let files = enumerate_in(base);
        let consoles: Vec<_> = files.iter().filter(|f| f.stream == "console").collect();
        assert_eq!(consoles.len(), 1);
        assert_eq!(consoles[0].source, "web");
    }

    #[test]
    fn tail_returns_last_n_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let p = dir.join("f.log");
        std::fs::write(&p, "1\n2\n3\n4\n5\n").unwrap();
        assert_eq!(tail(&p, 2), vec!["4".to_string(), "5".to_string()]);
        assert_eq!(tail(&p, 99).len(), 5);
        assert!(tail(&dir.join("missing.log"), 5).is_empty());
        // A file with no trailing newline still yields its last line.
        std::fs::write(&p, "a\nb").unwrap();
        assert_eq!(tail(&p, 1), vec!["b".to_string()]);
        assert_eq!(tail(&p, 0).len(), 0);
    }

    /// The backwards read must agree with a whole-file slice, including when
    /// the file is far bigger than the first scan window.
    #[test]
    fn tail_matches_whole_file_slicing_past_the_scan_window() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("big.log");
        let mut content = String::new();
        for i in 0..200_000 {
            content.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&p, &content).unwrap();
        let every: Vec<&str> = content.lines().collect();
        let expected: Vec<String> = every[every.len() - 50..]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (got, len) = tail_with_len(&p, 50);
        assert_eq!(got, expected);
        assert_eq!(len, content.len() as u64);
    }

    #[test]
    fn append_log_rotates_at_its_cap_keeping_one_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("lab.log");
        let mut log = AppendLog::open_with_max(p.clone(), 32);
        for i in 0..12 {
            log.write_line(&format!("line {i} padding"));
        }
        let previous = tmp.path().join("lab.log.1");
        assert!(previous.is_file(), "previous generation kept");
        // The live file holds the most recent lines and stays near the cap.
        let live = std::fs::read_to_string(&p).unwrap();
        assert!(live.contains("line 11"), "{live}");
        assert!(
            std::fs::metadata(&p).unwrap().len() <= 64,
            "live log stays bounded"
        );
        // Exactly one old generation exists — no .2, .3, … pile-up.
        let rolled = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("lab.log."))
            .count();
        assert_eq!(rolled, 1);
    }

    #[test]
    fn append_log_survives_an_unwritable_path() {
        let mut log = AppendLog::open(PathBuf::from("/definitely/not/a/writable/path/x.log"));
        log.write_line("dropped, not panicked");
    }
}
