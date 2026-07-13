//! Streaming exec: run an argv with piped stdio bridged to a channel.
//! stdin = host DATA frames (EOF via the `eof` control), stdout = DATA
//! frames back, stderr = DATA_ERR frames, exit code via `exited`.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vmlab_agent_proto::{AgentMsg, FrameKind, RecvWindow};

use crate::mux::{Input, Mux, pump_out};

pub fn open(
    mux: &Mux,
    id: u32,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
) {
    if argv.is_empty() {
        mux.send_error(Some(id), "exec: empty argv");
        return;
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            mux.send_error(Some(id), format!("exec {}: {e}", argv[0]));
            return;
        }
    };
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // The kill closure fires on host `close`; skip it once the child has
    // been reaped (its pid may be recycled).
    let done = Arc::new(AtomicBool::new(false));
    let pid = child.id();
    let kill_done = done.clone();
    let Some((input, credit)) = mux.register(
        id,
        None,
        Some(Box::new(move || {
            if !kill_done.load(Ordering::SeqCst) {
                crate::platform::kill_process(pid);
            }
        })),
    ) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    mux.send_ctrl(&AgentMsg::Opened { id });

    // stdin pump: host bytes → child stdin; credit granted once written.
    {
        let mux = mux.clone();
        thread::spawn(move || {
            let mut window = RecvWindow::default();
            for input in input {
                match input {
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
        thread::spawn(move || pump_out(&mux, id, FrameKind::Data, &credit, stdout))
    };
    let err_pump = {
        let (mux, credit) = (mux.clone(), credit.clone());
        thread::spawn(move || pump_out(&mux, id, FrameKind::DataErr, &credit, stderr))
    };

    // Reaper: wait for exit, let the output pumps flush what the pipes still
    // hold, then report and clean up.
    let mux = mux.clone();
    thread::spawn(move || {
        let code = match child.wait() {
            Ok(status) => exit_code(status),
            Err(_) => 127,
        };
        done.store(true, Ordering::SeqCst);
        let _ = out_pump.join();
        let _ = err_pump.join();
        mux.send_ctrl(&AgentMsg::Exited { id, code });
        mux.remove_finished(id);
    });
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(127)
}

#[cfg(windows)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(127)
}
