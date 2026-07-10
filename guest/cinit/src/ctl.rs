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

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use vmlab_cinit_proto::{CtlCommand, CtlEvent};

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

/// Event writer for the ctl port. Degrades to console-only when the port is
/// absent (a hand-launched debug VM), so boot still proceeds.
pub struct Ctl {
    tx: Option<Sender<String>>,
    pending: Arc<Pending>,
    /// Cloned fd for the command reader, consumed by [`Ctl::spawn_reader`].
    reader: Mutex<Option<File>>,
}

impl Ctl {
    pub fn open() -> Ctl {
        let disabled = Ctl {
            tx: None,
            pending: Arc::default(),
            reader: Mutex::new(None),
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
        let reader = file.try_clone().ok();
        if reader.is_none() {
            eprintln!("vmlab-cinit: warning: ctl reader clone failed; commands disabled");
        }

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
            reader: Mutex::new(reader),
        }
    }

    /// Emit one event line (also echoed to the console for debuggability).
    /// Never blocks: delivery is the writer thread's problem.
    pub fn emit(&self, ev: &CtlEvent) {
        // Infallible in practice; a serialisation bug must not take init down.
        let Ok(line) = serde_json::to_string(ev) else {
            eprintln!("vmlab-cinit: warning: cannot serialise event");
            return;
        };
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

    /// Read commands line-by-line on a dedicated thread, invoking `on_cmd`
    /// for each parsed command. No-op when the port is absent.
    pub fn spawn_reader(&self, on_cmd: impl Fn(CtlCommand) + Send + 'static) {
        let Some(file) = self.reader.lock().unwrap().take() else {
            return;
        };
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
                            Ok(cmd) => on_cmd(cmd),
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
}
