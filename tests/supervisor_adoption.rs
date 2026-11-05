//! A supervisor that adopts lab daemons it did not spawn (code review F11).
//!
//! The real `vmlab __supervisord` runs against a registry naming fake lab
//! daemons this test owns: a `sleep` process stands in for each daemon's pid,
//! and the test serves each lab's control socket. The supervisor is not the
//! parent of any of them, which is exactly the position it is in after a
//! restart.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vmlab::proto::client::Client;
use vmlab::proto::server::{Handler, Server, Streamer};
use vmlab::proto::{CommandError, Event, LabRequest, SupRequest};

const WAIT: Duration = Duration::from_secs(20);

/// A process killed (and reaped) when the test is done with it.
struct Process(Child);

impl Process {
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A fake lab daemon's pid: a process nobody but this test owns.
fn fake_daemon() -> Arc<Mutex<Process>> {
    let child = Command::new("sleep")
        .arg("600")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    Arc::new(Mutex::new(Process(child)))
}

/// When `pid` started, as `/proc/<pid>/stat` field 22 says.
fn start_time(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (_, rest) = stat.rsplit_once(')').unwrap();
    rest.split_whitespace().nth(19).unwrap().parse().unwrap()
}

/// The control socket of a fake lab: answers `ping`, and on `shutdown` ends
/// its daemon the way a real one ends after its teardown.
struct FakeLab(Arc<Mutex<Process>>);

#[async_trait::async_trait]
impl Handler<LabRequest> for FakeLab {
    async fn handle(&self, request: LabRequest, _: &Streamer) -> Result<Value, CommandError> {
        match request {
            LabRequest::Ping {} => Ok(json!(true)),
            LabRequest::Shutdown {} => {
                self.0.lock().unwrap().kill();
                Ok(json!(true))
            }
            _ => Err(CommandError::unsupported("fake lab daemon")),
        }
    }
}

struct Host {
    root: tempfile::TempDir,
}

impl Host {
    fn new() -> Host {
        Host {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn runtime(&self) -> PathBuf {
        self.path().join("runtime/vmlab")
    }

    fn registry(&self) -> PathBuf {
        self.path().join("state/vmlab/labs.json")
    }

    fn lab_socket(&self, lab: &str) -> PathBuf {
        vmlab::paths::lab_runtime_dir_in(&self.runtime(), lab).join("control.sock")
    }

    fn supervisor(&self) -> Process {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path().join("vmlabd.log"))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_vmlab"))
            .arg("__supervisord")
            .current_dir(self.path())
            .env("HOME", self.path().join("home"))
            .env("XDG_RUNTIME_DIR", self.path().join("runtime"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("XDG_CONFIG_HOME", self.path().join("config"))
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            .env_remove("VMLAB_INSTANCE")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Process(child)
    }

    /// The supervisor's control connection, once it answers.
    async fn connect(&self) -> Client<SupRequest> {
        let sock = self.runtime().join("vmlabd.sock");
        let until = Instant::now() + WAIT;
        loop {
            if let Ok(client) = Client::<SupRequest>::connect(&sock).await {
                return client;
            }
            assert!(Instant::now() < until, "the supervisor never came up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The registry entry for `lab` as the supervisor last saved it.
    fn entry(&self, lab: &str) -> Option<Value> {
        let text = std::fs::read_to_string(self.registry()).ok()?;
        let registry: Value = serde_json::from_str(&text).ok()?;
        registry["labs"]
            .as_array()?
            .iter()
            .find(|e| e["name"] == lab)
            .cloned()
    }

    /// Wait until `lab`'s entry satisfies `ok`.
    async fn wait_entry(&self, lab: &str, what: &str, ok: impl Fn(Option<&Value>) -> bool) {
        let until = Instant::now() + WAIT;
        loop {
            let entry = self.entry(lab);
            if ok(entry.as_ref()) {
                return;
            }
            assert!(
                Instant::now() < until,
                "{lab}: {what}; the entry is {entry:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn state(entry: Option<&Value>) -> Option<&str> {
    entry.and_then(|e| e["state"].as_str())
}

/// Wait for `event` on `lab`, failing on a `lab.daemon_crashed` for any lab
/// in `never_crashes`.
async fn wait_event(
    events: &mut tokio::sync::mpsc::Receiver<Event>,
    event: &str,
    lab: &str,
    never_crashes: &[&str],
) {
    let until = tokio::time::Instant::now() + WAIT;
    loop {
        let got = tokio::time::timeout_at(until, events.recv())
            .await
            .unwrap_or_else(|_| panic!("no {event} for {lab}"))
            .expect("the event stream ended");
        if got.event == "lab.daemon_crashed" {
            assert!(
                !never_crashes.contains(&got.lab.as_str()),
                "{} was reported crashed",
                got.lab
            );
        }
        if got.event == event && got.lab == lab {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_adopted_daemon_is_watched_across_restart_shutdown_and_crash() {
    let host = Host::new();

    // `stops` is released, `crashes` dies on its own, and `reused` names a
    // live pid whose process is not the one the registry recorded.
    let stops = fake_daemon();
    let crashes = fake_daemon();
    let stops_pid = stops.lock().unwrap().0.id();
    let crashes_pid = crashes.lock().unwrap().0.id();
    let me = std::process::id();
    let mut servers = Vec::new();
    for (lab, daemon) in [
        ("stops", stops.clone()),
        ("crashes", crashes.clone()),
        ("reused", fake_daemon()),
    ] {
        let sock = host.lab_socket(lab);
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        servers.push(
            Server::bind(&sock, Arc::new(FakeLab(daemon)))
                .await
                .unwrap(),
        );
    }
    let lab_root = host.path().join("lab");
    std::fs::create_dir_all(&lab_root).unwrap();
    std::fs::write(lab_root.join("vmlab.wcl"), "lab \"t\" {}\n").unwrap();
    std::fs::create_dir_all(host.registry().parent().unwrap()).unwrap();
    std::fs::write(
        host.registry(),
        serde_json::to_string_pretty(&json!({"labs": [
            // Written before start times were recorded: adopted on its pid.
            {"name": "stops", "root": lab_root, "pid": stops_pid, "state": "running"},
            {"name": "crashes", "root": lab_root, "pid": crashes_pid,
             "started": start_time(crashes_pid), "state": "running"},
            {"name": "reused", "root": lab_root, "pid": me,
             "started": start_time(me) + 1, "state": "running"},
        ]}))
        .unwrap(),
    )
    .unwrap();

    // Adoption: the live daemons stay running, the old entry learns its
    // daemon's start time, and the reused pid is not taken for a daemon.
    let mut first = host.supervisor();
    host.connect().await;
    host.wait_entry("reused", "a reused pid must not be adopted", |e| {
        state(e) == Some("failed")
    })
    .await;
    host.wait_entry("stops", "adoption records the start time", |e| {
        e.is_some_and(|e| e["started"] == json!(start_time(stops_pid)))
    })
    .await;
    assert_eq!(state(host.entry("stops").as_ref()), Some("running"));
    assert_eq!(state(host.entry("crashes").as_ref()), Some("running"));

    // Restart: a supervisor that dies leaves its labs to the next one.
    first.kill();
    let _second = host.supervisor();
    let supervisor = host.connect().await;
    let mut events = supervisor.subscribe().await.unwrap();
    host.wait_entry("crashes", "readopted after the restart", |e| {
        state(e) == Some("running")
    })
    .await;

    // Unexpected exit: nobody asked `crashes` to stop.
    crashes.lock().unwrap().kill();
    wait_event(&mut events, "lab.daemon_crashed", "crashes", &["stops"]).await;
    host.wait_entry("crashes", "an unexpected exit is a crash", |e| {
        state(e) == Some("failed")
    })
    .await;

    // Shutdown: a release asks the daemon to stop; its exit is expected,
    // so the entry goes and no crash is reported, also after the monitor's
    // next poll has seen the process gone.
    supervisor
        .send(SupRequest::LabRelease {
            name: "stops".into(),
        })
        .await
        .unwrap();
    assert!(
        host.entry("stops").is_none(),
        "a released lab keeps no entry"
    );
    let quiet = tokio::time::timeout(Duration::from_secs(3), async {
        wait_event(&mut events, "never", "stops", &["stops"]).await
    })
    .await;
    assert!(quiet.is_err());
    assert!(host.entry("stops").is_none());
    drop(servers);
}
