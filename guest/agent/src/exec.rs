//! Streaming exec: run an argv with piped stdio bridged to a channel.
//! stdin = host DATA frames (EOF via the `eof` control), stdout = DATA
//! frames back, stderr = DATA_ERR frames, an agent `eof` once both are
//! drained, exit code via `exited`.
//!
//! The process comes from the [`Spawner`] seam, so this plumbing is the same
//! on both guest targets.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use crate::mux::{Input, Mux, pump_out};
use crate::spawn::{Identity, ProcessSpec, Spawned, Spawner};

pub fn open(mux: &Mux, spawner: &dyn Spawner, identity: &Identity, id: u32, spec: ProcessSpec) {
    let Some(exe) = spec.argv.first().cloned() else {
        mux.send_error(Some(id), "exec: empty argv");
        return;
    };
    let Spawned {
        input,
        output,
        errors,
        resize: _,
        kill,
        wait,
    } = match spawner.exec(identity, spec) {
        Ok(p) => p,
        Err(e) => {
            mux.send_error(Some(id), format!("exec {exe}: {e}"));
            return;
        }
    };
    let mut stdin = Some(input);

    // The kill hook fires on host `close`; skip it once the child has been
    // reaped (its pid may be recycled).
    let done = Arc::new(AtomicBool::new(false));
    let kill_done = done.clone();
    let kill: Arc<dyn Fn() + Send + Sync> = Arc::from(kill);
    let session_kill = kill.clone();
    let Some((host_input, credit)) = mux.register(
        id,
        None,
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

    // stdin pump: host bytes → child stdin; credit granted once written.
    {
        let mux = mux.clone();
        thread::spawn(move || {
            let mut window = RecvWindow::default();
            for chunk in host_input {
                match chunk {
                    Input::Bytes(b) => {
                        let Some(s) = stdin.as_mut() else { continue };
                        if s.write_all(&b).is_err() {
                            stdin = None; // child closed its end; keep draining
                        }
                        if let Some(grant) = window.recv(b.len()) {
                            mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
                        }
                    }
                    Input::Eof => {
                        stdin = None; // drop = close the pipe
                    }
                }
            }
        });
    }

    // stdout / stderr pumps.
    let out_pump = {
        let (mux, credit) = (mux.clone(), credit.clone());
        thread::spawn(move || pump_out(&mux, id, FrameKind::Data, &credit, output))
    };
    // An exec always asks the seam for piped stderr; an adapter that has
    // none simply produces no DATA_ERR frames rather than taking the agent
    // down.
    let err_pump = errors.map(|errors| {
        let (mux, credit) = (mux.clone(), credit.clone());
        thread::spawn(move || pump_out(&mux, id, FrameKind::DataErr, &credit, errors))
    });

    // Reaper: wait for exit, let the output pumps flush what the pipes still
    // hold, then report and clean up.
    let mux = mux.clone();
    thread::spawn(move || {
        let code = wait();
        done.store(true, Ordering::SeqCst);
        let _ = out_pump.join();
        if let Some(err_pump) = err_pump {
            let _ = err_pump.join();
        }
        // Both pipes are drained: the channel's guest→host bytes are
        // complete. A consumer that only wants the output no longer has to
        // read `exited` to learn that.
        mux.send_ctrl(&AgentMsg::Eof { id });
        mux.send_ctrl(&AgentMsg::Exited { id, code });
        mux.remove_finished(id);
    });
}
