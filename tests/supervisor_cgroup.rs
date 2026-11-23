//! The CLI's auto-started daemons survive teardown of the unit running up.

use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Command, Output};

use vmlab::proto::{LabRequest, client::LabClient};

const VMLAB: &str = env!("CARGO_BIN_EXE_vmlab");

struct Starter {
    unit: String,
    runtime: PathBuf,
    state: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

impl Starter {
    fn systemctl(&self) -> Command {
        let mut cmd = Command::new("systemctl");
        cmd.arg("--user").env("XDG_RUNTIME_DIR", &self.runtime);
        cmd
    }

    fn vmlab(&self) -> Command {
        let mut cmd = Command::new(VMLAB);
        cmd.env("XDG_RUNTIME_DIR", &self.runtime)
            .env("XDG_STATE_HOME", &self.state)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_DATA_HOME", &self.data);
        cmd
    }
}

impl Drop for Starter {
    fn drop(&mut self) {
        let _ = self.systemctl().args(["stop", &self.unit]).output();
        let _ = self.vmlab().args(["daemon", "stop"]).output();
    }
}

fn checked(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn lab_survives_stopping_the_unit_that_ran_up() {
    let manager_runtime = PathBuf::from(format!("/run/user/{}", nix::unistd::Uid::effective()));
    let manager = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .env("XDG_RUNTIME_DIR", &manager_runtime)
        .output();
    if !manager.is_ok_and(|output| output.status.success()) {
        eprintln!("skipping cgroup survival test: systemd user manager unavailable");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let starter = Starter {
        unit: format!(
            "vmlab-test-{}.service",
            root.file_name().unwrap().to_string_lossy()
        ),
        runtime: root.join("run"),
        state: root.join("state"),
        config: root.join("config"),
        data: root.join("data"),
    };
    std::fs::create_dir(&starter.runtime).unwrap();
    // Isolate vmlab's sockets while retaining access to the real user manager.
    for entry in ["systemd", "bus"] {
        symlink(manager_runtime.join(entry), starter.runtime.join(entry)).unwrap();
    }
    std::fs::write(
        root.join("vmlab.wcl"),
        "import <vmlab.wcl>\nlab \"cgroup-test\" {}\n",
    )
    .unwrap();

    let mut run = Command::new("systemd-run");
    run.args([
        "--user",
        "--quiet",
        "--remain-after-exit",
        "--property=Type=oneshot",
    ])
    .arg(format!("--unit={}", starter.unit))
    .arg(format!("--working-directory={}", root.display()))
    .env("XDG_RUNTIME_DIR", &starter.runtime);
    for (var, path) in [
        ("XDG_RUNTIME_DIR", &starter.runtime),
        ("XDG_STATE_HOME", &starter.state),
        ("XDG_CONFIG_HOME", &starter.config),
        ("XDG_DATA_HOME", &starter.data),
    ] {
        run.arg(format!("--setenv={var}={}", path.display()));
    }
    checked(
        run.args(["--", VMLAB, "--instance", "", "up"])
            .output()
            .unwrap(),
    );

    let status = || checked(starter.vmlab().args(["daemon", "status"]).output().unwrap());
    let before = status();
    let lab = before
        .lines()
        .find(|line| line.contains("cgroup-test [running]"))
        .unwrap_or_else(|| panic!("missing running lab in {before}"));
    checked(
        starter
            .systemctl()
            .args(["stop", &starter.unit])
            .output()
            .unwrap(),
    );
    let after = status();
    assert!(after.lines().any(|line| line == lab), "{after}");
    let runtime = vmlab::paths::runtime_dir_in(Some(&starter.runtime));
    let lab_dir = vmlab::paths::lab_runtime_dir_in(&runtime, "cgroup-test");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = LabClient::connect(&lab_dir.join("control.sock"))
            .await
            .unwrap();
        client.send(LabRequest::Ping {}).await.unwrap();
    });
}
