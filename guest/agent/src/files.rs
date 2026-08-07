//! File push/pull over a channel, sha256-verified.
//!
//! Push: host streams DATA frames, then `eof`; the bytes land at `path`
//! (parent directories created) and the agent answers `file_done` with the
//! digest of what it wrote. Pull: the agent streams `path` as DATA frames
//! and finishes with `file_done`. A host `close` mid-transfer aborts: pushes
//! leave a partial file behind (the host treats the transfer as failed),
//! pulls simply stop.

use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use sha2::{Digest, Sha256};
use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use crate::mux::{Input, Mux};
use crate::spawn::{Identity, Spawner};

pub fn open_push(
    mux: &Mux,
    spawner: &dyn Spawner,
    identity: &Identity,
    id: u32,
    path: String,
    mode: Option<u32>,
) {
    // The file is created through the seam: who owns the developer's files
    // is the same per-channel decision as who a process runs as (PRD §19.2).
    let mut file = match spawner.create_file(identity, &path) {
        Ok(f) => f,
        Err(e) => {
            mux.send_error(Some(id), format!("push {path}: {e}"));
            return;
        }
    };
    let Some((input, _credit)) = mux.register(id, None, None) else {
        return;
    };
    mux.send_ctrl(&AgentMsg::Opened { id });

    let mux = mux.clone();
    thread::spawn(move || {
        let mut hasher = Sha256::new();
        let mut len: u64 = 0;
        let mut window = RecvWindow::default();
        for input in input {
            match input {
                Input::Bytes(b) => {
                    if let Err(e) = file.write_all(&b) {
                        mux.send_error(Some(id), format!("push {path}: write: {e}"));
                        mux.remove_finished(id);
                        return;
                    }
                    hasher.update(&b);
                    len += b.len() as u64;
                    if let Some(grant) = window.recv(b.len()) {
                        mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
                    }
                }
                Input::Eof => {
                    if let Err(e) = file.flush() {
                        mux.send_error(Some(id), format!("push {path}: flush: {e}"));
                        mux.remove_finished(id);
                        return;
                    }
                    if let Some(mode) = mode {
                        file.set_mode(mode);
                    }
                    mux.send_ctrl(&AgentMsg::FileDone {
                        id,
                        sha256: hex(&hasher.finalize()),
                        len,
                    });
                    mux.remove_finished(id);
                    return;
                }
            }
        }
        // Input sender dropped without EOF: host closed mid-push.
    });
}

pub fn open_pull(mux: &Mux, id: u32, path: String) {
    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            mux.send_error(Some(id), format!("pull {path}: {e}"));
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
        let mut hasher = Sha256::new();
        let mut len: u64 = 0;
        let mut buf = [0u8; 32 * 1024];
        loop {
            if stop.load(Ordering::SeqCst) {
                return; // host closed the channel; it already gave up
            }
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buf[..n]);
                    len += n as u64;
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
                    mux.send_error(Some(id), format!("pull {path}: read: {e}"));
                    mux.remove_finished(id);
                    return;
                }
            }
        }
        mux.send_ctrl(&AgentMsg::FileDone {
            id,
            sha256: hex(&hasher.finalize()),
            len,
        });
        mux.remove_finished(id);
    });
}

pub fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
