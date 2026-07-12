//! vmlab-cinit — PID 1 of an OCI-container micro-VM.
//!
//! Boot sequence:
//!  1. mount /proc, /sys, /dev; load the kernel modules the asset ships
//!  2. parse `vmlab.root=` / `vmlab.scratch=` from the kernel cmdline
//!  3. squashfs (RO) + ext4 scratch (mkfs on first boot) → overlay /rootfs
//!  4. open the `vmlab.ctl.0` virtio port; emit `boot` (repeating) until the
//!     host answers with the `spec` command (ContainerSpec)
//!  5. mount runtime filesystems, write identity files; mount virtiofs
//!     volumes (proto v4 — vhost-user-fs devices the host attached; its
//!     virtiofsd migrates state through the snapshot, PRD §18)
//!  6. loopback + DHCP per NIC (busybox udhcpc — see net.rs), emit `net_up`
//!  7. mount CIFS volumes from the segment gateway (network must be up;
//!     the fallback when the host has no virtiofsd)
//!  8. start bundled qemu-ga in the init namespace (not the container root)
//!  9. in idle mode emit `idle` and wait for stop; otherwise resolve user,
//!     build env, clone namespaces + exec the container, and emit `started`
//! 10. reap children; when a workload container exits: emit `exited`, power off
//!
//! Any fatal init error prints to the console and powers off, so the host's
//! missing-`exited` handling classifies the VM as crashed.

mod cmdline;
mod container;
mod ctl;
mod health;
mod mounts;
mod net;
mod reap;
mod sig;
mod tty;
mod users;
mod util;

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::waitpid;
use nix::unistd::Pid;

use vmlab_cinit_proto::{ContainerSpec, CtlCommand, CtlEvent, PROTO_VERSION, RuntimeMode};

use crate::util::{Result, power_off};

fn main() {
    println!("vmlab-cinit: starting (proto v{PROTO_VERSION})");
    if let Err(e) = run() {
        eprintln!("vmlab-cinit: FATAL: {e}");
    }
    power_off();
}

/// Announce boot until the host answers with the spec. The `boot` line
/// repeats each second: with `server=on,wait=off` chardevs, lines written
/// before the host attaches are dropped, so a single announcement could be
/// lost. Commands other than `spec` are meaningless this early; `stop` aborts
/// boot.
fn wait_for_spec(ctl: &ctl::Ctl) -> Result<ContainerSpec> {
    if !ctl.available() {
        return Err("ctl port vmlab.ctl.0 absent — cannot receive the container spec".into());
    }
    loop {
        ctl.emit(&CtlEvent::Boot {
            proto_version: PROTO_VERSION,
        });
        match ctl.recv_command(Duration::from_secs(1)) {
            Some(CtlCommand::Spec { spec }) => return Ok(spec),
            Some(CtlCommand::Stop { .. }) => return Err("stop requested before spec".into()),
            Some(_) | None => {}
        }
    }
}

fn spawn_qemu_ga() {
    const QEMU_GA: &str = "/usr/bin/qemu-ga";
    if !std::path::Path::new(QEMU_GA).exists() {
        println!("vmlab-cinit: no qemu-ga in initramfs, skipping");
        return;
    }
    let Some(port) = ctl::find_virtio_port("org.qemu.guest_agent.0") else {
        println!("vmlab-cinit: no guest-agent virtio port, skipping qemu-ga");
        return;
    };
    // Runs in the init namespace (not the container root) as a plain child;
    // if it ever dies, the main reap loop collects it.
    match Command::new(QEMU_GA)
        .args(["-m", "virtio-serial", "-p"])
        .arg(&port)
        .args(["-t", "/run"])
        .spawn()
    {
        Ok(child) => println!("vmlab-cinit: qemu-ga running (pid {})", child.id()),
        Err(e) => eprintln!("vmlab-cinit: warning: qemu-ga failed to start: {e}"),
    }
}

fn run() -> Result<()> {
    // -- filesystems ---------------------------------------------------------
    mounts::mount_early()?;
    mounts::load_modules()?;
    let devices = cmdline::read()?;
    mounts::mount_container_root(&devices.root, &devices.scratch)?;

    // -- ctl channel + spec ---------------------------------------------------
    let ctl = Arc::new(ctl::Ctl::open());
    let spec = wait_for_spec(&ctl)?;

    mounts::mount_rootfs_runtime()?;
    mounts::install_shell_fallback();
    mounts::write_identity(&spec.hostname)?;

    // -- virtiofs volumes (device mounts — no network needed) ------------------
    for vol in spec.volumes.iter().filter(|v| v.tag.is_some()) {
        let tag = vol.tag.as_deref().expect("filtered on tag");
        mounts::mount_virtiofs(tag, &vol.target, vol.read_only)?;
    }

    // -- networking ----------------------------------------------------------
    net::loopback_up()?;
    for n in 0..spec.nics {
        let lease = net::dhcp_up(n)?;
        if n == 0
            && let Some(ip) = lease
        {
            ctl.emit(&CtlEvent::NetUp { ip });
        }
    }
    if spec.nics > 0 {
        net::write_resolv_conf(spec.nics)?;
    }

    // -- CIFS volumes (network mounts — after DHCP) ----------------------------
    if spec.volumes.iter().any(|v| v.tag.is_none()) {
        let smb = spec
            .smb
            .as_ref()
            .ok_or("spec declares CIFS volumes but no smb coordinates")?;
        for vol in spec.volumes.iter().filter(|v| v.tag.is_none()) {
            mounts::mount_volume(smb, &vol.share, &vol.target, vol.read_only)?;
        }
    }

    // -- guest agent ---------------------------------------------------------
    spawn_qemu_ga();

    if spec.mode == RuntimeMode::Idle {
        println!("vmlab-cinit: idle mode; OCI entrypoint and cmd are disabled");
        ctl.emit(&CtlEvent::Idle);
        let ctl_replay = ctl.clone();
        ctl.spawn_dispatcher(move |cmd| match cmd {
            CtlCommand::Stop { .. } => {
                println!("vmlab-cinit: idle stop requested");
                power_off();
            }
            CtlCommand::Resync => {
                println!("vmlab-cinit: resync requested");
                ctl_replay.resync();
            }
            CtlCommand::TtyResize { .. } => {}
            CtlCommand::Spec { .. } => {
                eprintln!("vmlab-cinit: warning: spec received after boot, ignoring");
            }
        });
        loop {
            match waitpid(None::<Pid>, None) {
                Ok(status) => {
                    if let Some((pid, code)) = reap::exit_code(&status) {
                        eprintln!("vmlab-cinit: idle child {pid} exited with code {code}");
                    }
                }
                Err(Errno::EINTR) => {}
                Err(Errno::ECHILD) => thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(format!("waitpid: {e}")),
            }
        }
    }

    // -- container -----------------------------------------------------------
    let user = match &spec.user {
        Some(u) => Some(users::resolve_user_in_rootfs(u, mounts::ROOTFS)?),
        None => None,
    };
    let env = container::build_env(&spec, user.as_ref());
    let stop_signal = match &spec.stop_signal {
        Some(name) => sig::parse_signal(name)?,
        None => Signal::SIGTERM,
    };

    let reaper = Arc::new(reap::Reaper::default());
    let exited = Arc::new(AtomicBool::new(false));

    let process = container::spawn(&spec, user.as_ref(), &env)?;
    let child = process.pid;
    // Interactive shell sessions join the workload's PID and mount
    // namespaces, then chroot into its rootfs.
    let tty = tty::Tty::start(&spec, &env, process.namespaces.clone(), reaper.clone());
    ctl.emit(&CtlEvent::Started {
        pid: child.as_raw() as u32,
    });

    // Host commands: Stop = graceful signal now, SIGKILL after the grace;
    // TtyResize goes to the shell session's PTY; Resync replays lifecycle
    // events after an online snapshot restore. A duplicate Spec is ignored.
    {
        let exited = exited.clone();
        let ctl_replay = ctl.clone();
        ctl.spawn_dispatcher(move |cmd| match cmd {
            CtlCommand::Stop { grace_secs } => {
                println!("vmlab-cinit: stop requested (grace {grace_secs}s)");
                let _ = kill(child, stop_signal);
                let exited = exited.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_secs(grace_secs));
                    if !exited.load(Ordering::SeqCst) {
                        println!("vmlab-cinit: grace expired, killing container");
                        let _ = kill(child, Signal::SIGKILL);
                    }
                });
            }
            CtlCommand::TtyResize { cols, rows } => tty.resize(cols, rows),
            CtlCommand::Resync => {
                println!("vmlab-cinit: resync requested");
                ctl_replay.resync();
            }
            CtlCommand::Spec { .. } => {
                eprintln!("vmlab-cinit: warning: spec received after boot, ignoring");
            }
        });
    }

    if let Some(hc) = &spec.healthcheck {
        health::spawn(
            hc,
            &env,
            ctl.clone(),
            reaper.clone(),
            exited.clone(),
            process.namespaces.clone(),
        )?;
    }

    // -- reap loop (main thread) ---------------------------------------------
    // Sole waitpid(-1) caller: routes non-container exits (healthcheck runs,
    // a dying qemu-ga) through the reaper, breaks when the container is done.
    let code = loop {
        match waitpid(None::<Pid>, None) {
            Ok(status) => {
                if let Some((pid, code)) = reap::exit_code(&status) {
                    if pid == child.as_raw() {
                        break code;
                    }
                    reaper.route(pid, code);
                }
            }
            Err(Errno::EINTR) => {}
            // No children at all — should not happen while the container
            // lives, but don't busy-spin if it somehow does.
            Err(Errno::ECHILD) => thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("waitpid: {e}")),
        }
    };
    exited.store(true, Ordering::SeqCst);
    println!("vmlab-cinit: container exited with code {code}");
    ctl.emit(&CtlEvent::Exited { code });
    // Bounded flush so `exited` reaches a connected host; power_off() (via
    // main) then syncs and drops the machine.
    ctl.drain(Duration::from_secs(2));
    Ok(())
}
