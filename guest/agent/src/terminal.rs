//! Interactive terminal sessions: a shell on a PTY/ConPTY bridged to a
//! channel. Host DATA frames are keystrokes, the terminal's whole VT stream
//! comes back as DATA frames, the exit code arrives via `exited`, and
//! `resize` retargets the terminal.
//!
//! The terminal itself comes from the [`Spawner`] seam, so this plumbing is
//! the same on both guest targets.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use crate::mux::{Input, Mux, pump_out};
use crate::spawn::{Identity, Spawned, Spawner, TerminalSpec};

pub fn open(mux: &Mux, spawner: &dyn Spawner, identity: Identity, id: u32, spec: TerminalSpec) {
    let Spawned {
        mut input,
        output,
        errors: _,
        resize,
        kill,
        wait,
    } = match spawner.terminal(identity, spec) {
        Ok(p) => p,
        Err(e) => {
            mux.send_error(Some(id), format!("terminal: {e}"));
            return;
        }
    };

    // The kill hook fires on host `close`; skip it once the shell has been
    // reaped (its pid/handle may be recycled).
    let done = Arc::new(AtomicBool::new(false));
    let kill_done = done.clone();
    let kill: Arc<dyn Fn() + Send + Sync> = Arc::from(kill);
    let session_kill = kill.clone();
    let Some((host_input, credit)) = mux.register(
        id,
        resize,
        Some(Box::new(move || {
            if !kill_done.load(Ordering::SeqCst) {
                session_kill();
            }
        })),
    ) else {
        kill();
        wait();
        return;
    };
    mux.send_ctrl(&AgentMsg::Opened { id });

    // Input pump: host bytes → the terminal. A dying shell may stop reading;
    // dropped input is fine, the session is ending anyway.
    {
        let mux = mux.clone();
        thread::spawn(move || {
            let mut window = RecvWindow::default();
            for chunk in host_input {
                match chunk {
                    Input::Bytes(b) => {
                        let _ = input.write_all(&b);
                        if let Some(grant) = window.recv(b.len()) {
                            mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
                        }
                    }
                    // A terminal has no stdin to close: keystrokes end when
                    // the session does.
                    Input::Eof => {}
                }
            }
        });
    }

    // Output pump: terminal → host.
    let out_pump = {
        let (mux, credit) = (mux.clone(), credit.clone());
        thread::spawn(move || pump_out(&mux, id, FrameKind::Data, &credit, output))
    };

    // Reaper: the shell exited, so `wait` also releases the terminal, which
    // ends the output pump; flush it, report, clean up.
    let mux = mux.clone();
    thread::spawn(move || {
        let code = wait();
        done.store(true, Ordering::SeqCst);
        let _ = out_pump.join();
        mux.send_ctrl(&AgentMsg::Exited { id, code });
        mux.remove_finished(id);
    });
}
