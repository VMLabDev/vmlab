//! TCP tunnels (PRD §19.5): the agent dials `host:port` from inside the
//! guest and the channel becomes that connection's byte pipe.
//!
//! **Resolution is guest-side.** The host string arrives verbatim and the
//! guest's own resolver turns it into addresses, which is what makes a domain
//! name in a SOCKS request work. **There is no destination policy** — any
//! address the guest can reach, not loopback-only — because a dynamic forward
//! dials whatever the developer's tooling asks for, and vmlab is not a
//! security boundary (§1.2).
//!
//! A dial that does not succeed fails the channel with
//! [`ErrorCause::ConnectFailed`], so the SSH facade can answer
//! `SSH_OPEN_CONNECT_FAILED` rather than `ADMINISTRATIVELY_PROHIBITED`: a
//! SOCKS client has to tell "nothing is listening" from "vmlab refused you".
//!
//! Each direction half-closes on its own. The socket reaching EOF sends
//! `AgentMsg::Eof` and leaves the channel open for host→guest bytes; a host
//! `Eof` shuts the socket's write half and leaves it open for guest→host
//! bytes. The channel goes away once both halves are done, or as soon as the
//! host closes it.

use std::io::Write;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmlab_agent_proto::{AgentMsg, ErrorCause, FrameKind, RecvWindow};

use crate::mux::{Input, Mux, pump_out};

/// Total time a dial may take, resolution and every resolved address
/// included. Deliberately under the host's 15s wait for `opened`, so a dead
/// destination comes back as a reportable connect failure rather than as the
/// host giving up on the channel — which would lose the very distinction the
/// vocabulary exists to preserve.
const DIAL_BUDGET: Duration = Duration::from_secs(10);

/// The connected socket, shared with the channel's kill hook so a host
/// `close` can interrupt a blocked dial or a blocked read. `closed` lives
/// under the same lock as `stream`: without that, a `close` racing the dial
/// could find no socket to shut down and the dial could then publish one
/// nobody ever shuts down.
#[derive(Default)]
struct Shared {
    stream: Option<TcpStream>,
    closed: bool,
}

pub fn open(mux: &Mux, id: u32, host: String, port: u16) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let kill_shared = shared.clone();
    // Registered before the dial, not after: a host that gives up while we
    // are still connecting sends `close`, and the kill hook has to be in
    // place to catch it.
    let Some((input, credit)) = mux.register(
        id,
        None,
        Some(Box::new(move || {
            let mut s = kill_shared.lock().unwrap();
            s.closed = true;
            if let Some(stream) = s.stream.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        })),
    ) else {
        return;
    };

    let mux = mux.clone();
    thread::spawn(move || {
        let stream = match dial(&host, port) {
            Ok(stream) => stream,
            Err(e) => {
                mux.send_error_cause(
                    Some(id),
                    format!("tunnel {host}:{port}: {e}"),
                    ErrorCause::ConnectFailed,
                );
                mux.remove_finished(id);
                return;
            }
        };
        // One handle per direction, plus the one the kill hook shuts down.
        let (Ok(reader), Ok(mut writer)) = (stream.try_clone(), stream.try_clone()) else {
            mux.send_error(
                Some(id),
                format!("tunnel {host}:{port}: cannot share the socket"),
            );
            mux.remove_finished(id);
            return;
        };
        {
            let mut s = shared.lock().unwrap();
            if s.closed {
                return; // the host gave up while we were dialling
            }
            s.stream = Some(stream);
        }
        mux.send_ctrl(&AgentMsg::Opened { id });

        // Both halves must finish before the channel is spent; whichever
        // pump is last takes the session down.
        let halves = Arc::new(AtomicU8::new(0));

        {
            let (mux, halves) = (mux.clone(), halves.clone());
            thread::spawn(move || {
                pump_out(&mux, id, FrameKind::Data, &credit, reader);
                // The peer stopped sending. Report the half-close rather
                // than failing the channel: the host may still have bytes
                // for it.
                if !shared.lock().unwrap().closed {
                    mux.send_ctrl(&AgentMsg::Eof { id });
                }
                finish_half(&mux, id, &halves);
            });
        }

        let mut window = RecvWindow::default();
        for msg in input {
            match msg {
                Input::Bytes(bytes) => {
                    if writer.write_all(&bytes).is_err() {
                        break; // the peer is gone; the read half reports it
                    }
                    if let Some(grant) = window.recv(bytes.len()) {
                        mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
                    }
                }
                Input::Eof => {
                    // The host's write half is done. Pass the FIN on so the
                    // peer can finish, and keep taking guest→host bytes.
                    let _ = writer.shutdown(Shutdown::Write);
                    break;
                }
            }
        }
        finish_half(&mux, id, &halves);
    });
}

/// Retire one direction; the second one to arrive drops the session.
fn finish_half(mux: &Mux, id: u32, halves: &AtomicU8) {
    if halves.fetch_add(1, Ordering::SeqCst) == 1 {
        mux.remove_finished(id);
    }
}

/// Resolve and connect inside the guest, trying every address the name gives
/// until one answers or the budget runs out. The last failure is what the
/// caller reports, so "connection refused" survives to the host.
fn dial(host: &str, port: u16) -> std::io::Result<TcpStream> {
    use std::io::{Error, ErrorKind};
    let deadline = Instant::now() + DIAL_BUDGET;
    let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
    let mut last: Option<Error> = None;
    for addr in addrs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&addr, left) {
            Ok(stream) => {
                // Interactive traffic: a forwarded keystroke must not wait
                // for Nagle to fill a segment.
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        Error::new(
            ErrorKind::TimedOut,
            format!("no address answered within {DIAL_BUDGET:?}"),
        )
    }))
}
