//! The host control channel: the virtio-serial port named `vmlab.ctl.0`,
//! carrying newline-delimited JSON — [`CtlEvent`]s out, [`CtlCommand`]s in.
//!
//! Without udev there are no /dev/virtio-ports/<name> symlinks, so the port
//! is located by scanning /sys/class/virtio-ports/*/name for the wanted name
//! and opening the matching /dev/vportNpM node.
//!
//! Two virtio-serial quirks shape this module:
//!
//! - Ports are **exclusive-open** (a second open fails with EBUSY), so the
//!   device is opened once read+write and the reader thread gets a
//!   `try_clone` of the same fd (fine on a character device).
//! - Writes **block while the host side is not connected**. Events are
//!   therefore written by a dedicated writer thread fed through a channel:
//!   init never stalls on an absent host, and events emitted before the host
//!   attaches are delivered once it does. [`Ctl::drain`] bounds the final
//!   flush before poweroff.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmlab_cinit_proto::{CtlCommand, CtlEvent, PROTO_VERSION};

/// Resolve a virtio-serial port by its name property.
pub fn find_virtio_port(name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir("/sys/class/virtio-ports").ok()?;
    for entry in entries.flatten() {
        let port_name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if port_name.trim() == name {
            return Some(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    None
}

/// Count of event lines handed to the writer thread but not yet written.
#[derive(Default)]
struct Pending {
    count: Mutex<usize>,
    drained: Condvar,
}

/// Lifecycle events remembered for [`Ctl::resync`]: after an online snapshot
/// restore the resumed guest never repeats `net_up`/`started`/`idle`/`health`, so
/// the host asks for a replay instead.
#[derive(Default)]
struct Replay {
    net_up: Option<CtlEvent>,
    started: Option<CtlEvent>,
    health: Option<CtlEvent>,
}

/// Event writer for the ctl port. Degrades to console-only when the port is
/// absent (a hand-launched debug VM), so boot still proceeds.
pub struct Ctl {
    tx: Option<Sender<String>>,
    pending: Arc<Pending>,
    /// Parsed host commands, fed by the reader thread; drained directly
    /// ([`Ctl::recv_command`]) before the container runs, then handed to the
    /// runtime dispatcher ([`Ctl::spawn_dispatcher`]).
    commands: Mutex<Option<Receiver<CtlCommand>>>,
    replay: Mutex<Replay>,
}

impl Ctl {
    pub fn open() -> Ctl {
        let disabled = Ctl {
            tx: None,
            pending: Arc::default(),
            commands: Mutex::new(None),
            replay: Mutex::new(Replay::default()),
        };
        let Some(path) = find_virtio_port("vmlab.ctl.0") else {
            eprintln!(
                "vmlab-cinit: warning: ctl port vmlab.ctl.0 not found; events go to console only"
            );
            return disabled;
        };
        let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "vmlab-cinit: warning: cannot open ctl port {}: {e}",
                    path.display()
                );
                return disabled;
            }
        };
        let commands = match file.try_clone() {
            Ok(reader) => {
                let (cmd_tx, cmd_rx) = channel::<CtlCommand>();
                spawn_command_reader(reader, cmd_tx);
                Some(cmd_rx)
            }
            Err(_) => {
                eprintln!("vmlab-cinit: warning: ctl reader clone failed; commands disabled");
                None
            }
        };

        let (tx, rx) = channel::<String>();
        let pending = Arc::new(Pending::default());
        let thread_pending = pending.clone();
        thread::spawn(move || {
            for line in rx {
                // Blocks until the host side is connected — that is the point
                // of doing it on this thread.
                if let Err(e) = writeln!(file, "{line}").and_then(|()| file.flush()) {
                    eprintln!("vmlab-cinit: warning: ctl write failed: {e}");
                }
                let mut count = thread_pending.count.lock().unwrap();
                *count -= 1;
                if *count == 0 {
                    thread_pending.drained.notify_all();
                }
            }
        });
        Ctl {
            tx: Some(tx),
            pending,
            commands: Mutex::new(commands),
            replay: Mutex::new(Replay::default()),
        }
    }

    /// Whether the ctl port exists — without it there is no way to receive
    /// the spec, so boot cannot proceed.
    pub fn available(&self) -> bool {
        self.tx.is_some()
    }

    /// Emit one event line (also echoed to the console for debuggability).
    /// Never blocks: delivery is the writer thread's problem.
    pub fn emit(&self, ev: &CtlEvent) {
        // Infallible in practice; a serialisation bug must not take init down.
        let Ok(line) = serde_json::to_string(ev) else {
            eprintln!("vmlab-cinit: warning: cannot serialise event");
            return;
        };
        {
            let mut replay = self.replay.lock().unwrap();
            match ev {
                CtlEvent::NetUp { .. } => replay.net_up = Some(ev.clone()),
                CtlEvent::Started { .. } | CtlEvent::Idle => replay.started = Some(ev.clone()),
                CtlEvent::Health { .. } => replay.health = Some(ev.clone()),
                _ => {}
            }
        }
        println!("vmlab-cinit: event {line}");
        if let Some(tx) = &self.tx {
            *self.pending.count.lock().unwrap() += 1;
            if tx.send(line).is_err() {
                *self.pending.count.lock().unwrap() -= 1;
            }
        }
    }

    /// Wait (bounded) for queued events to reach the host — called before
    /// poweroff so `exited` is not lost. Returns early when the queue empties.
    pub fn drain(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut count = self.pending.count.lock().unwrap();
        while *count > 0 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                eprintln!("vmlab-cinit: warning: ctl drain timed out ({count} events unsent)");
                return;
            };
            let (guard, _) = self.pending.drained.wait_timeout(count, remaining).unwrap();
            count = guard;
        }
    }

    /// Wait up to `timeout` for the next host command. Used during boot,
    /// before the runtime dispatcher owns the stream. `None` = timeout (or
    /// no port / dispatcher already running).
    pub fn recv_command(&self, timeout: Duration) -> Option<CtlCommand> {
        let guard = self.commands.lock().unwrap();
        let rx = guard.as_ref()?;
        match rx.recv_timeout(timeout) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                thread::sleep(timeout);
                None
            }
        }
    }

    /// Hand the command stream to a dedicated thread invoking `on_cmd` for
    /// each command from here on. No-op when the port is absent.
    pub fn spawn_dispatcher(&self, on_cmd: impl Fn(CtlCommand) + Send + 'static) {
        let Some(rx) = self.commands.lock().unwrap().take() else {
            return;
        };
        thread::spawn(move || {
            for cmd in rx {
                on_cmd(cmd);
            }
        });
    }

    /// Replay current state for a host that just (re)attached — sent after an
    /// online snapshot restore: `boot`, then whichever of `net_up`,
    /// `started`/`idle`, and `health` have happened.
    pub fn resync(&self) {
        self.emit(&CtlEvent::Boot {
            proto_version: PROTO_VERSION,
        });
        let (net_up, started, health) = {
            let replay = self.replay.lock().unwrap();
            (
                replay.net_up.clone(),
                replay.started.clone(),
                replay.health.clone(),
            )
        };
        for ev in [net_up, started, health].into_iter().flatten() {
            self.emit(&ev);
        }
    }
}

/// Read host command lines off the ctl port on a dedicated thread, parsing
/// each into the channel. Runs for the life of the machine.
fn spawn_command_reader(file: fs::File, tx: Sender<CtlCommand>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                // EOF: host side detached; it may reconnect, don't spin.
                Ok(0) => thread::sleep(Duration::from_millis(200)),
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<CtlCommand>(trimmed) {
                        Ok(cmd) => {
                            if tx.send(cmd).is_err() {
                                return; // both consumers gone — machine is dying
                            }
                        }
                        Err(e) => {
                            eprintln!("vmlab-cinit: warning: bad ctl command {trimmed:?}: {e}")
                        }
                    }
                }
                Err(e) => {
                    eprintln!("vmlab-cinit: warning: ctl read failed: {e}");
                    thread::sleep(Duration::from_millis(200));
                }
            }
        }
    });
}
