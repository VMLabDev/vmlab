//! S1 on the emulated aarch64 `virt` machine (phase 3d, card C14): the
//! aarch64 container guest asset boots on the emulator to cinit `idle` and
//! agent ready, an online snapshot of the running guest is taken over the
//! control socket, the guest changes, the snapshot is loaded, and the agent
//! answers after it with the state of the save. A guest reset in place
//! follows, and the agent answers after that too.
//!
//! The test needs the aarch64 guest asset (`guest/build-asset.sh aarch64`,
//! or `VMLAB_HV_TEST_GUEST_DIST_AARCH64`) and a rootfs; it skips with a
//! printed reason without them. It needs no KVM.

use std::time::{Duration, Instant};

use vmlab_cinit_proto::CtlEvent;
use vmlab_hv_proto::control::{Command as Ctrl, Event, Outcome, RunState};
use vmlab_hv_tests::agent::AgentClient;
use vmlab_hv_tests::ctl::CtlClient;
use vmlab_hv_tests::{MachineSpec, probe_assets_aarch64};

const BUSYBOX: &str = "/.vmlab/busybox";

/// A boot on the emulator, to agent ready.
const BOOT: Duration = Duration::from_secs(600);

fn ok(outcome: Outcome) {
    assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
}

fn exec(guest: &vmlab_hv_tests::Guest, agent: &mut AgentClient, script: &str) -> Vec<u8> {
    let output = agent
        .exec(&[BUSYBOX, "sh", "-c", script], Duration::from_secs(120))
        .unwrap_or_else(|e| panic!("{script}: {e}\n{}", guest.diagnostics("exec")));
    assert_eq!(output.exit_code, 0, "{output:?}");
    output.stdout
}

#[test]
#[cfg_attr(
    vmlab_ci_no_host_assets,
    ignore = "requires the aarch64 guest asset and a rootfs"
)]
fn an_emulated_aarch64_guest_answers_after_a_snapshot_load_and_a_reset() {
    let assets = probe_assets_aarch64();
    if let Some(reason) = assets.skip_reason() {
        eprintln!("skipping S1 aarch64: {reason}");
        return;
    }
    let mut spec = MachineSpec::idle("s1-aarch64-snapshot");
    spec.aarch64 = true;
    let started = Instant::now();
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container = spec.container_spec();
    let mut ctl =
        CtlClient::connect(&guest.ctl_sock, Duration::from_secs(60)).expect("connect the ctl port");
    ctl.send_spec(&container).expect("send spec");
    ctl.wait_event(
        &container,
        BOOT,
        |e| matches!(e, CtlEvent::Idle),
        |_| {},
        || guest.diagnostics("waiting for cinit idle"),
    );
    let mut agent = AgentClient::connect(&guest.agent_sock, BOOT).expect("agent handshake");
    eprintln!(
        "aarch64 agent ready in {:.1} s: {} on {}",
        started.elapsed().as_secs_f64(),
        agent.info().agent_version,
        agent.info().os
    );
    assert_eq!(exec(&guest, &mut agent, "uname -m"), b"aarch64\n");
    exec(&guest, &mut agent, "echo saved > /marker; sync");

    ok(guest
        .control
        .call(Ctrl::SnapshotSave {
            name: "saved".into(),
        })
        .unwrap());
    exec(&guest, &mut agent, "echo changed > /marker; sync");
    assert_eq!(exec(&guest, &mut agent, "cat /marker"), b"changed\n");
    let loaded = Instant::now();
    ok(guest
        .control
        .call(Ctrl::SnapshotLoad {
            name: "saved".into(),
        })
        .unwrap());
    assert_eq!(
        guest.control.call(Ctrl::Status).unwrap(),
        Outcome::Ok {
            status: Some(RunState::Running)
        }
    );
    drop(agent);
    let mut agent = AgentClient::connect(&guest.agent_sock, BOOT).expect("agent after the load");
    assert_eq!(exec(&guest, &mut agent, "cat /marker"), b"saved\n");
    eprintln!(
        "the agent answered {:.1} s after the load",
        loaded.elapsed().as_secs_f64()
    );

    // A reset in place: the guest boots again in the same process, and the
    // agent answers on the same socket.
    guest.control.take_events();
    let _ = agent.exec(
        &[BUSYBOX, "sh", "-c", "echo b > /proc/sysrq-trigger"],
        Duration::from_secs(5),
    );
    drop(agent);
    let deadline = Instant::now() + BOOT;
    while !guest.control.take_events().contains(&Event::Reset) {
        assert!(
            Instant::now() < deadline,
            "no reset: {}",
            guest.diagnostics("reset")
        );
        guest.control.call(Ctrl::Status).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(ctl);
    let mut ctl =
        CtlClient::connect(&guest.ctl_sock, Duration::from_secs(60)).expect("ctl after the reset");
    ctl.send_spec(&container).expect("send spec");
    ctl.wait_event(
        &container,
        BOOT,
        |e| matches!(e, CtlEvent::Idle),
        |_| {},
        || guest.diagnostics("waiting for cinit idle after the reset"),
    );
    let mut agent = AgentClient::connect(&guest.agent_sock, BOOT).expect("agent after the reset");
    assert_eq!(exec(&guest, &mut agent, "uname -m"), b"aarch64\n");
    guest.quit_and_exit_zero();
}
