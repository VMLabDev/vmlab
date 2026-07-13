//! Follow a file like `tail -F`: an initial tail of what already exists,
//! then live appends, surviving rotation (new inode at the same path) and
//! truncation. Polling keeps it portable and dependency-free; a quarter
//! second is plenty for log-watching.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use vmlab_agent_proto::{AgentMsg, FrameKind};

use crate::mux::Mux;

const POLL: Duration = Duration::from_millis(250);
/// How much of an existing file to send up front.
const BACKLOG: u64 = 64 * 1024;

pub fn open(mux: &Mux, id: u32, path: String) {
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            mux.send_error(Some(id), format!("tail {path}: {e}"));
            return;
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let kill_stop = stop.clone();
    let Some((_input, credit)) = mux.register(
        id,
        None,
        Some(Box::new(move || kill_stop.store(true, Ordering::SeqCst))),
    ) else {
        return;
    };
    mux.send_ctrl(&AgentMsg::Opened { id });

    let mux = mux.clone();
    thread::spawn(move || {
        // Start BACKLOG bytes from the end (or the start of a small file).
        let mut pos = file
            .metadata()
            .map(|m| m.len().saturating_sub(BACKLOG))
            .unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(pos));
        let mut buf = [0u8; 16 * 1024];
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match file.read(&mut buf) {
                Ok(0) => {
                    // Caught up. Rotated (a fresh file now lives at path) or
                    // truncated (len < pos)? Reopen from the start.
                    let rotated = std::fs::metadata(&path)
                        .map(|m| m.len() < pos || !same_file(&file, &path))
                        .unwrap_or(false);
                    if rotated && let Ok(f) = File::open(&path) {
                        file = f;
                        pos = 0;
                    }
                    thread::sleep(POLL);
                }
                Ok(n) => {
                    pos += n as u64;
                    let mut off = 0;
                    while off < n {
                        let take = credit.take(n - off);
                        if take == 0 {
                            return; // closed under us
                        }
                        mux.send_data(FrameKind::Data, id, &buf[off..off + take]);
                        off += take;
                    }
                }
                Err(e) => {
                    mux.send_error(Some(id), format!("tail {path}: read: {e}"));
                    mux.remove_finished(id);
                    return;
                }
            }
        }
    });
}

/// Whether the open handle still refers to the file currently at `path`
/// (i.e. it has not been rotated away). Unix compares inodes; on Windows
/// rotation-by-rename of an open file is rare, so size-shrink detection
/// (handled by the caller) has to do.
#[cfg(unix)]
fn same_file(file: &File, path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.ino() == b.ino() && a.dev() == b.dev(),
        _ => false,
    }
}

#[cfg(windows)]
fn same_file(_file: &File, _path: &str) -> bool {
    true
}
