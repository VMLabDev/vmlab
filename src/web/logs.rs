//! `GET /api/labs/{lab}/logs` — a WebSocket that streams a lab's logs to the
//! browser. There is no daemon RPC for logs (the CLI reads the state-dir files
//! directly), so this server-side tailer reads the same files: it sends a
//! backlog of recent lines on connect, then polls for appended lines and for
//! newly-created VM log files, forwarding each as a JSON [`vmlab::logs::LogEntry`]
//! text frame. The SPA buffers them and filters by VM / substring client-side.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use actix_web::{Error, HttpRequest, HttpResponse, web};
use chrono::{DateTime, Utc};
use futures::StreamExt;

use super::state::AppState;
use vmlab::logs;

/// Backlog lines sent per source on connect (and when a new source appears).
const BACKLOG: usize = 200;
/// How often to poll the log files for growth / new VMs.
const POLL: Duration = Duration::from_millis(400);

/// Tailing state for one log file.
struct Tail {
    source: String,
    stream: String,
    path: PathBuf,
    /// Byte offset we've consumed up to.
    offset: u64,
    /// Bytes read past the last newline, awaiting the rest of the line.
    partial: String,
}

pub async fn logs(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<HttpResponse, Error> {
    let lab = path.into_inner();
    // The name comes straight from the URL and becomes a log-directory path.
    if !super::state::valid_name(&lab) {
        return Ok(actix_web::HttpResponse::BadRequest()
            .json(serde_json::json!({"error": "invalid lab name"})));
    }
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, body)?;

    actix_web::rt::spawn(async move {
        // Touch state so the lab daemon is known/registered; not fatal if it
        // isn't running — logs persist on disk regardless.
        let _ = state
            .lab_call(&lab, vmlab::proto::LabRequest::Ping {})
            .await;

        if !logs::lab_dir(&lab).exists() {
            let _ = session
                .close(Some(actix_ws::CloseReason {
                    code: actix_ws::CloseCode::Normal,
                    description: Some(format!("no logs for lab `{lab}`")),
                }))
                .await;
            return;
        }

        let mut tails: Vec<Tail> = Vec::new();
        let mut known: HashSet<PathBuf> = HashSet::new();
        let mut ticker = tokio::time::interval(POLL);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Pick up newly-created files (VMs that started after we
                    // opened, or streams that didn't exist yet).
                    for f in logs::enumerate(&lab) {
                        if known.insert(f.path.clone()) {
                            tails.push(seed(&mut session, f.source, f.stream, f.path).await);
                        }
                    }
                    // Forward any appended lines, in a stable source order.
                    for t in &mut tails {
                        if !pump(&mut session, t).await {
                            return;
                        }
                    }
                }
                incoming = msg_stream.next() => match incoming {
                    Some(Ok(actix_ws::Message::Ping(p))) => {
                        if session.pong(&p).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(actix_ws::Message::Close(_))) | None => {
                        let _ = session.close(None).await;
                        return;
                    }
                    _ => {}
                },
            }
        }
    });

    Ok(response)
}

/// Register a new file: emit its backlog of complete lines, then return a
/// [`Tail`] positioned at end-of-file so only future lines stream.
async fn seed(
    session: &mut actix_ws::Session,
    source: String,
    stream: String,
    path: PathBuf,
) -> Tail {
    // Read the backlog from the end rather than slurping the file: a serial log
    // can be tens of MB and this runs per file, per stream connection, on the
    // single web worker.
    let (mut lines, offset) = vmlab::logs::tail_with_len(&path, BACKLOG + 1);
    // Backlog lines have no per-line timestamp; stamp them with the file's last
    // modification time so they sort near when they were actually written
    // (close to boot for a quiet serial log, ~now for a busy one).
    let at = file_mtime(&path).unwrap_or_else(Utc::now);
    // A file not ending in a newline has a half-written last line: hold it back
    // as the partial so it isn't emitted now and duplicated when it completes.
    let ends_complete = ends_with_newline(&path, offset);
    let partial = if ends_complete {
        String::new()
    } else {
        lines.pop().unwrap_or_default()
    };
    let start = lines.len().saturating_sub(BACKLOG);
    for line in &lines[start..] {
        let _ = send(session, &source, &stream, line, at).await;
    }
    Tail {
        source,
        stream,
        path,
        offset,
        partial,
    }
}

/// Does the file's last byte terminate a line? One 1-byte read, so the seed
/// never has to hold the file in memory to answer it.
fn ends_with_newline(path: &Path, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return true;
    };
    if f.seek(SeekFrom::Start(len - 1)).is_err() {
        return true;
    }
    let mut last = [0u8; 1];
    std::io::Read::read_exact(&mut f, &mut last)
        .map(|_| last[0] == b'\n')
        .unwrap_or(true)
}

/// Read any bytes appended to `t` since last time, emit the complete lines, and
/// keep the trailing partial. Returns false if the session has closed.
async fn pump(session: &mut actix_ws::Session, t: &mut Tail) -> bool {
    let len = std::fs::metadata(&t.path)
        .map(|m| m.len())
        .unwrap_or(t.offset);
    if len == t.offset {
        return true;
    }
    if len < t.offset {
        // Truncated / rotated — restart from the top.
        t.offset = 0;
        t.partial.clear();
    }
    let Ok(mut f) = std::fs::File::open(&t.path) else {
        return true;
    };
    if f.seek(SeekFrom::Start(t.offset)).is_err() {
        return true;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return true;
    }
    t.offset = len;
    t.partial.push_str(&String::from_utf8_lossy(&buf));

    // Lines arriving live are stamped now — that is when they were written.
    let now = Utc::now();
    // Emit every complete line; retain the trailing partial.
    while let Some(i) = t.partial.find('\n') {
        let line: String = t.partial.drain(..=i).collect();
        let line = line.trim_end_matches(['\n', '\r']);
        if send(session, &t.source, &t.stream, line, now)
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

/// The file's last-modified time as a UTC timestamp, if available.
fn file_mtime(path: &Path) -> Option<DateTime<Utc>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified))
}

/// Send one parsed line as a JSON frame. Lines without an inherent timestamp
/// (everything but `events.jsonl`) fall back to `at`, so the client can order
/// the merged stream chronologically.
async fn send(
    session: &mut actix_ws::Session,
    source: &str,
    stream: &str,
    raw: &str,
    at: DateTime<Utc>,
) -> Result<(), actix_ws::Closed> {
    let mut entry = logs::parse_line(source, stream, raw);
    if entry.ts.is_none() {
        entry.ts = Some(at);
    }
    match serde_json::to_string(&entry) {
        Ok(json) => session.text(json).await,
        Err(_) => Ok(()),
    }
}
