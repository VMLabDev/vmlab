//! Two supervisors of one account on one data dir (VML-90).
//!
//! Every `vmlabd` sweeps `<data>/vmlab/cache/builds` at startup (ADR-0010).
//! Two agents, or a user and an agent, or two shells with different
//! `XDG_RUNTIME_DIR`s, run separate supervisors that share that dir, and a
//! starting one used to delete the other's in-flight build. Supervisor A runs
//! here with a build in flight — the test process claims the workdir through
//! the same [`claim_workdir`] a build uses, standing in for A's build task, so
//! no VM has to boot — and supervisor B then starts on the same data dir with
//! its own runtime dir. B must sweep the dead build's leftover and keep A's.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vmlab::proto::SupRequest;
use vmlab::proto::client::Client;
use vmlab::template::build::claim_workdir;

const WAIT: Duration = Duration::from_secs(20);

/// A process killed (and reaped) when the test is done with it.
struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One account's home: a data dir every supervisor shares, and a runtime and
/// state dir of its own for each supervisor.
struct Account {
    root: tempfile::TempDir,
}

impl Account {
    fn path(&self) -> &Path {
        self.root.path()
    }

    fn builds(&self) -> PathBuf {
        self.path().join("data/vmlab/cache/builds")
    }

    fn runtime(&self, who: &str) -> PathBuf {
        self.path().join(format!("runtime-{who}"))
    }

    fn supervisor(&self, who: &str) -> Process {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path().join(format!("vmlabd-{who}.log")))
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_vmlab"))
            .arg("__supervisord")
            .current_dir(self.path())
            .env("HOME", self.path().join("home"))
            .env("XDG_RUNTIME_DIR", self.runtime(who))
            .env("XDG_STATE_HOME", self.path().join(format!("state-{who}")))
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

    /// Wait until supervisor `who` answers on its control socket.
    async fn connect(&self, who: &str) -> Client<SupRequest> {
        let sock = self.runtime(who).join("vmlab/vmlabd.sock");
        let until = Instant::now() + WAIT;
        loop {
            if let Ok(client) = Client::<SupRequest>::connect(&sock).await {
                return client;
            }
            assert!(
                Instant::now() < until,
                "supervisor {who} never came up; log:\n{}",
                std::fs::read_to_string(self.path().join(format!("vmlabd-{who}.log")))
                    .unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_starting_supervisor_keeps_another_supervisors_build_workdir() {
    let account = Account {
        root: tempfile::tempdir().unwrap(),
    };

    // Supervisor A is up, and it has swept the empty builds dir already.
    let _a = account.supervisor("a");
    account.connect("a").await;

    // A's build is in flight: its workdir holds the media the build still
    // needs (vml-38's windows-95 build lost its ISO after Setup this way).
    let held = account.builds().join("x86_64-windows-95-1.0");
    let build = claim_workdir(&held).unwrap();
    std::fs::write(held.join("media.iso"), "pretend").unwrap();
    // A build that died with its supervisor long ago left this behind.
    let leftover = account.builds().join("x86_64-dead-1.0");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("disk.qcow2"), "pretend").unwrap();

    // Supervisor B starts on the same data dir, with its own runtime dir.
    let _b = account.supervisor("b");
    account.connect("b").await;
    // The sweep runs after B starts listening: wait for it to take the
    // leftover, which proves it has run.
    let until = Instant::now() + WAIT;
    while leftover.exists() {
        assert!(
            Instant::now() < until,
            "supervisor B never swept the dead build's workdir"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        held.join("media.iso").is_file(),
        "supervisor B deleted supervisor A's in-flight build workdir"
    );
    // A is still serving.
    account.connect("a").await;

    // A's build ends: its workdir goes with it.
    drop(build);
    assert!(!held.exists());
}
