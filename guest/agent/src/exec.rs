//! Streaming exec: run an argv with piped stdio bridged to a channel.
//! stdin = host DATA frames (EOF via the `eof` control), stdout = DATA
//! frames back, stderr = DATA_ERR frames, an agent `eof` once both are
//! drained, exit code via `exited`.
//!
//! The process comes from the [`Spawner`] seam, so this plumbing is the same
//! on both guest targets.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use crate::mux::{Input, Mux, pump_out_with};
use crate::spawn::{Identity, ProcessSpec, Spawned, Spawner};

// As with terminals, a descendant can keep the output pipes open after exit.
const DRAIN: Duration = Duration::from_secs(2);

pub fn open(mux: &Mux, spawner: &dyn Spawner, identity: &Identity, id: u32, spec: ProcessSpec) {
    let mux = &mux.for_channel(id);
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

    // stdin pump: grant credit for bytes written or discarded after the
    // child closes its pipe.
    {
        let mux = mux.clone();
        thread::spawn(move || {
            let mut window = RecvWindow::default();
            for chunk in host_input {
                match chunk {
                    Input::Bytes(b) => {
                        if let Some(s) = stdin.as_mut()
                            && s.write_all(&b).is_err()
                        {
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
    let (pumped, pumps) = mpsc::channel::<()>();
    let accepting_output = Arc::new(Mutex::new(true));
    let start_pump = |kind, mut pipe: Box<dyn Read + Send>| {
        let (mux, credit) = (mux.clone(), credit.clone());
        let accepting_output = accepting_output.clone();
        let pumped = pumped.clone();
        thread::spawn(move || {
            pump_out_with(&credit, &mut pipe, move |bytes| {
                let accepting = accepting_output.lock().unwrap();
                if *accepting {
                    mux.send_data(kind, id, bytes);
                }
                *accepting
            });
            drop(pumped);
            // Descendants may still write to these pipes after the session
            // ends. Discard their output until they close the write ends.
            let _ = std::io::copy(&mut pipe, &mut std::io::sink());
        });
    };
    start_pump(FrameKind::Data, output);
    // An exec always asks the seam for piped stderr; an adapter that has
    // none simply produces no DATA_ERR frames rather than taking the agent
    // down.
    if let Some(errors) = errors {
        start_pump(FrameKind::DataErr, errors);
    }
    drop(pumped);

    // Reaper: wait for exit, let the output pumps flush what the pipes still
    // hold, then report and clean up.
    let mux = mux.clone();
    thread::spawn(move || {
        let code = wait();
        done.store(true, Ordering::SeqCst);
        if let Err(mpsc::RecvTimeoutError::Timeout) = pumps.recv_timeout(DRAIN) {
            // Stop forwarding before queuing EOF, including frames whose
            // pumps already took credit. Closing credit wakes stalled pumps.
            *accepting_output.lock().unwrap() = false;
            credit.close();
        }
        // The channel-bound mux suppresses both messages after host close.
        mux.send_ctrl(&AgentMsg::Eof { id });
        mux.send_ctrl(&AgentMsg::Exited { id, code });
        mux.remove_finished(id);
    });
}
