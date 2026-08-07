//! The in-memory adapter for the [`Spawner`] seam, and the test platform
//! that serves it.
//!
//! ADR-0015 takes its bar from ADR-0001: a seam only earns its keep when
//! the fake is cheap. This one spawns nothing and touches no filesystem, so
//! session behaviour — and, from PRD §19.2 onwards, *who* a session runs
//! as — is testable with no guest anywhere.

#![cfg(test)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmlab_agent_proto::{NetInterface, OsInfo, ShutdownMode, features};

use crate::mux::{Mux, Platform};
use crate::spawn::{Adopter, Identity, ProcessSpec, Spawned, Spawner, TerminalSpec};

/// One creation the seam was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    Terminal {
        identity: Identity,
        command: Option<Vec<String>>,
        cols: u16,
        rows: u16,
        env: Vec<(String, String)>,
    },
    Exec {
        identity: Identity,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
    },
    /// A session asked for an identity to open files through (`tail`, and
    /// every `fileops` session).
    Adopt { identity: Identity },
}

/// The test's end of a process the fake handed out.
#[derive(Clone)]
pub struct FakeProcess {
    input: Arc<Mutex<Vec<u8>>>,
    stdout: Arc<Mutex<Option<SyncSender<Vec<u8>>>>>,
    stderr: Arc<Mutex<Option<SyncSender<Vec<u8>>>>>,
    resizes: Arc<Mutex<Vec<(u16, u16)>>>,
    killed: Arc<AtomicBool>,
    exit: SyncSender<i32>,
}

impl FakeProcess {
    /// Bytes the session has written into the process so far.
    pub fn input(&self) -> Vec<u8> {
        self.input.lock().unwrap().clone()
    }

    /// Block until the session has written exactly `expected` into the
    /// process. The input pump is a thread of its own and nothing on the
    /// output path orders it, so a test that asserts on input must wait
    /// for it rather than sample it.
    pub fn expect_input(&self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let got = self.input();
            if got == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected:?}; got {got:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Produce bytes on the process's output stream.
    pub fn say(&self, bytes: &[u8]) {
        send(&self.stdout, bytes);
    }

    /// Produce bytes on the process's error stream.
    pub fn say_err(&self, bytes: &[u8]) {
        send(&self.stderr, bytes);
    }

    /// Terminal sizes the session applied, oldest first.
    pub fn resizes(&self) -> Vec<(u16, u16)> {
        self.resizes.lock().unwrap().clone()
    }

    /// Whether the session force-stopped the process.
    pub fn killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }

    /// End the process: close its output streams — which is what lets the
    /// session's output pumps finish — then release its `wait`.
    pub fn finish(&self, code: i32) {
        *self.stdout.lock().unwrap() = None;
        *self.stderr.lock().unwrap() = None;
        let _ = self.exit.try_send(code);
    }
}

fn send(stream: &Arc<Mutex<Option<SyncSender<Vec<u8>>>>>, bytes: &[u8]) {
    if let Some(tx) = stream.lock().unwrap().as_ref() {
        let _ = tx.send(bytes.to_vec());
    }
}

/// An in-memory [`Spawner`]: records what it was asked to create, hands back
/// processes and files the test drives directly.
#[derive(Default)]
pub struct FakeSpawner {
    calls: Mutex<Vec<Call>>,
    processes: Mutex<Vec<FakeProcess>>,
    /// Creations queued to fail, oldest first. An empty queue succeeds.
    failures: Mutex<VecDeque<String>>,
}

impl FakeSpawner {
    pub fn new() -> FakeSpawner {
        FakeSpawner::default()
    }

    /// Every creation the seam was asked for, oldest first.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// The `n`th process handed out (0-based).
    pub fn process(&self, n: usize) -> FakeProcess {
        self.processes.lock().unwrap()[n].clone()
    }

    /// Make the next creation fail with `msg`.
    pub fn fail_next(&self, msg: &str) {
        self.failures.lock().unwrap().push_back(msg.to_string());
    }

    fn next_failure(&self) -> Option<std::io::Error> {
        let msg = self.failures.lock().unwrap().pop_front()?;
        Some(std::io::Error::other(msg))
    }

    /// Build a process whose stdio is in-memory channels.
    fn hand_out(&self, shape: Shape) -> Spawned {
        let hosted = shape == Shape::Terminal;
        let input = Arc::new(Mutex::new(Vec::new()));
        let (out_tx, out_rx) = sync_channel(64);
        let (err_tx, err_rx) = sync_channel(64);
        let (exit_tx, exit_rx) = sync_channel(1);
        let handle = FakeProcess {
            input: input.clone(),
            stdout: Arc::new(Mutex::new(Some(out_tx))),
            stderr: Arc::new(Mutex::new((!hosted).then_some(err_tx))),
            resizes: Arc::new(Mutex::new(Vec::new())),
            killed: Arc::new(AtomicBool::new(false)),
            exit: exit_tx,
        };
        self.processes.lock().unwrap().push(handle.clone());

        // Killing closes the output streams the way a dying process does,
        // then reports the SIGKILL-shaped code the reaper would observe.
        let kill_handle = handle.clone();
        let resizes = handle.resizes.clone();
        Spawned {
            input: Box::new(SharedWriter(input)),
            output: Box::new(ChanReader::new(out_rx)),
            // A terminal multiplexes both streams onto its VT output and
            // is the only shape that can be resized.
            errors: (!hosted).then(|| Box::new(ChanReader::new(err_rx)) as Box<dyn Read + Send>),
            resize: hosted.then(|| {
                Box::new(move |cols, rows| resizes.lock().unwrap().push((cols, rows)))
                    as Box<dyn Fn(u16, u16) + Send + Sync>
            }),
            kill: Box::new(move || {
                kill_handle.killed.store(true, Ordering::SeqCst);
                kill_handle.finish(128 + 9);
            }),
            wait: Box::new(move || exit_rx.recv().unwrap_or(127)),
        }
    }
}

/// Which of the seam's two process shapes the fake is standing in for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Hosted on a terminal: one output stream, resizable.
    Terminal,
    /// Piped stdio: separate stdout and stderr, no terminal.
    Piped,
}

impl Spawner for FakeSpawner {
    fn terminal(&self, identity: &Identity, spec: TerminalSpec) -> std::io::Result<Spawned> {
        self.calls.lock().unwrap().push(Call::Terminal {
            identity: identity.clone(),
            command: spec.command,
            cols: spec.cols,
            rows: spec.rows,
            env: spec.env,
        });
        match self.next_failure() {
            Some(e) => Err(e),
            None => Ok(self.hand_out(Shape::Terminal)),
        }
    }

    fn exec(&self, identity: &Identity, spec: ProcessSpec) -> std::io::Result<Spawned> {
        self.calls.lock().unwrap().push(Call::Exec {
            identity: identity.clone(),
            argv: spec.argv,
            env: spec.env,
            cwd: spec.cwd,
        });
        match self.next_failure() {
            Some(e) => Err(e),
            None => Ok(self.hand_out(Shape::Piped)),
        }
    }

    fn adopter(&self, identity: &Identity) -> std::io::Result<Adopter> {
        self.calls.lock().unwrap().push(Call::Adopt {
            identity: identity.clone(),
        });
        match self.next_failure() {
            Some(e) => Err(e),
            None => Ok(crate::spawn::adopt_as_agent()),
        }
    }
}

/// A [`Platform`] with no guest behind it: everything OS-specific answers a
/// fixed value, and every session comes from the fake seam.
pub struct TestPlatform {
    pub spawner: Arc<FakeSpawner>,
}

impl TestPlatform {
    pub fn new() -> TestPlatform {
        TestPlatform {
            spawner: Arc::new(FakeSpawner::new()),
        }
    }
}

impl Platform for TestPlatform {
    fn os(&self) -> &'static str {
        "test"
    }
    fn features(&self) -> Vec<String> {
        vec![features::TERMINAL.to_string(), features::WATCH.to_string()]
    }
    fn spawner(&self) -> &dyn Spawner {
        self.spawner.as_ref()
    }
    fn open_eventlog(&self, mux: &Mux, id: u32, _: Option<String>) {
        mux.send_error(Some(id), "unsupported");
    }
    fn set_clipboard(&self, _: &Mux, _: String) {}
    fn get_clipboard(&self, mux: &Mux) {
        mux.send_error(None, "unsupported");
    }
    fn net_info(&self) -> Result<Vec<NetInterface>, String> {
        Ok(vec![NetInterface {
            name: "eth0".into(),
            mac: Some("52:54:00:00:00:01".into()),
            ipv4: vec!["10.0.0.2".into()],
            ipv6: vec![],
        }])
    }
    fn os_info(&self) -> Result<OsInfo, String> {
        Ok(OsInfo {
            id: "test".into(),
            name: "Test OS".into(),
            version: "1".into(),
            kernel: "0.0".into(),
            arch: "x86_64".into(),
            hostname: "testhost".into(),
        })
    }
    fn shutdown(&self, _: &Mux, _: ShutdownMode) {}
}

// ---- in-memory stdio -------------------------------------------------------

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A [`Read`] over a channel: bytes the test sends arrive here, and dropping
/// the last sender is end-of-stream.
struct ChanReader {
    rx: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl ChanReader {
    fn new(rx: Receiver<Vec<u8>>) -> ChanReader {
        ChanReader {
            rx,
            chunk: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ChanReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = (self.chunk.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
