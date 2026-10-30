//! The second adapter: `vmlab-hv`, vmlab's own hypervisor, as one process
//! per machine (PRD §22, ADR-0021).
//!
//! A [`Launch`] becomes a [`vmlab_hv_proto::MachineDescription`] written to
//! the machine's runtime directory ([`describe`]); the process is
//! `vmlab-hv --machine <file> --control <socket> --paused`; and its control
//! socket becomes the seam's [`Control`] ([`control`]). Phase 1 runs
//! container micro-VMs on x86_64 under KVM; phases 2a and 2b a modern Linux
//! VM on q35 and SeaBIOS, whose framebuffer and input are vmlab-hv's native
//! frame protocol on `display.sock` beside the control socket ([`display`]),
//! with an RFB server on `vnc.sock` for human viewers.
//!
//! The helpers a machine needs beside its hypervisor are the host's, as
//! under QEMU: the same virtiofsd serves container volumes over vhost-user.
//! A VM's shares wait for vmlab-hv's own virtio-fs (phase 2e), so a VM's
//! adapter reports no virtiofsd and `transport = "auto"` shares fall back to
//! SMB. There is no software TPM to start until phase 2c.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{GuestAsset, Hypervisor, Launch, LaunchSpec, Process, Running};
use crate::qemu::Proc;

pub(crate) mod control;
pub(crate) mod describe;
pub(crate) mod display;

use control::{HvControl, HvProcess};

/// The binary's name, which is also what a host config without `vmlab_hv`
/// finds on `PATH`.
pub const BINARY: &str = "vmlab-hv";

/// `vmlab-hv` on this host.
pub struct VmlabHv {
    binary: PathBuf,
    /// Whether this adapter runs a VM, whose shares have no virtio-fs to
    /// ride until phase 2e.
    vm: bool,
    /// The host config's `firmware_dir`, searched for SeaBIOS and the
    /// option ROMs ahead of the host distro's.
    firmware_dir: Option<PathBuf>,
}

impl VmlabHv {
    /// The binary at `binary`: the host config's `vmlab_hv`, else
    /// [`BINARY`] on `PATH`.
    pub fn new(binary: Option<PathBuf>) -> Self {
        Self {
            binary: binary.unwrap_or_else(|| PathBuf::from(BINARY)),
            vm: false,
            firmware_dir: None,
        }
    }

    /// The adapter for a VM, finding its firmware under `firmware_dir`
    /// first.
    pub fn for_vm(binary: Option<PathBuf>, firmware_dir: Option<PathBuf>) -> Self {
        Self {
            vm: true,
            firmware_dir,
            ..Self::new(binary)
        }
    }
}

/// Where the host keeps SeaBIOS and SeaVGABIOS: the host config's
/// `firmware_dir` in its `<arch>/` layout, then the distro's.
fn rom_dirs(firmware_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = firmware_dir.map(|d| d.join("x86_64")).into_iter().collect();
    dirs.extend(
        [
            "/usr/share/qemu",
            "/usr/share/seabios",
            "/usr/share/vmlab-hv",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    dirs
}

impl describe::HostRoms {
    /// This host's firmware and option ROMs, each the first of its names
    /// found in [`rom_dirs`].
    pub(crate) fn probe(firmware_dir: Option<&Path>) -> Self {
        let dirs = rom_dirs(firmware_dir);
        let find = |names: &[&str]| {
            dirs.iter()
                .flat_map(|d| names.iter().map(move |n| d.join(n)))
                .find(|p| p.is_file())
        };
        Self {
            seabios: find(&["bios-256k.bin"]),
            vgabios_std: find(&["vgabios-stdvga.bin"]),
            vgabios_virtio: find(&["vgabios-virtio.bin"]),
            // vmlab-hv's own iPXE build, never QEMU's (HV phase-2 D14, D16).
            ipxe_virtio: find(&["1af41000.rom"]),
        }
    }
}

/// Where the machine description goes: beside the control socket, in the
/// machine's runtime directory, which `kill_lab_orphans` recognises in an
/// argv — so an orphaned `vmlab-hv` is reaped like an orphaned QEMU.
fn machine_file(control_sock: &Path) -> PathBuf {
    control_sock.with_file_name("machine.json")
}

/// The argv after the binary.
fn args(machine: &Path, control_sock: &Path) -> Vec<String> {
    vec![
        "--machine".into(),
        machine.display().to_string(),
        "--control".into(),
        control_sock.display().to_string(),
        // Built but not run, as QEMU's `-S`: the caller resumes once it has
        // subscribed to what it needs.
        "--paused".into(),
    ]
}

#[async_trait::async_trait]
impl Hypervisor for VmlabHv {
    async fn start_tpm(
        &self,
        machine: &str,
        _state_dir: &Path,
        _ctrl_sock: &Path,
        _log: &Path,
    ) -> Result<Arc<dyn Process>> {
        bail!("{machine}: vmlab-hv has no TPM yet — a VM with one runs on QEMU (PRD §22)")
    }

    fn virtiofsd(&self) -> Option<crate::qemu::virtiofsd::Virtiofsd> {
        if self.vm {
            return None;
        }
        crate::qemu::virtiofsd::found()
    }

    async fn start_virtiofsd(
        &self,
        machine: &str,
        socket: &Path,
        shared_dir: &Path,
        readonly: bool,
        log: &Path,
    ) -> Result<Arc<dyn Process>> {
        super::qemu::start_host_virtiofsd(machine, socket, shared_dir, readonly, log).await
    }

    async fn start_emulator(&self, spec: LaunchSpec) -> Result<Running> {
        let description = match &spec.machine {
            Launch::Container {
                lab,
                resolved,
                paths,
            } => describe::container(lab, resolved, paths)?,
            Launch::Vm {
                lab,
                resolved,
                paths,
                accel,
                ..
            } => describe::vm(
                lab,
                resolved,
                paths,
                *accel,
                &describe::HostRoms::probe(self.firmware_dir.as_deref()),
            )?,
        };
        let control_sock = &spec.control_sock;
        let machine = machine_file(control_sock);
        if let Some(dir) = machine.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&machine, serde_json::to_vec_pretty(&description)?)
            .with_context(|| format!("writing {}", machine.display()))?;
        // vmlab-hv binds every socket it serves and refuses a path that is
        // already there; QEMU unlinks a stale one itself. A previous run that
        // was killed leaves them behind.
        // The display sockets, where the description names them, else
        // beside the control socket.
        let display_socks = description.display.as_ref().map(|d| {
            [
                d.socket
                    .clone()
                    .unwrap_or_else(|| display::native_sock(control_sock)),
                d.vnc_socket
                    .clone()
                    .unwrap_or_else(|| display::rfb_sock(control_sock)),
            ]
        });
        for stale in description
            .ports
            .iter()
            .map(|p| p.socket.clone())
            .chain([control_sock.clone()])
            .chain(display_socks.iter().flatten().cloned())
        {
            match std::fs::remove_file(&stale) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("removing {}", stale.display()));
                }
            }
        }

        let binary = self.binary.display().to_string();
        let args = args(&machine, control_sock);
        crate::qemu::process::write_launch_header(&spec.log, &binary, &args, &spec.env)?;
        let log_from = crate::qemu::process::log_offset(&spec.log);
        let proc =
            Proc::spawn_with_fds(&spec.label, &binary, &args, &spec.env, &spec.log, spec.fds)
                .await?;
        let proc: Arc<dyn Process> = super::qemu::host_process(proc);
        let mut control = connect_retry(control_sock, &proc, &spec.log, log_from).await?;
        if let Some([native, _]) = display_socks {
            control.set_display(native);
        }
        tracing::debug!(
            machine = %spec.label,
            agent = %spec.channels.agent.display(),
            ctl = ?spec.channels.ctl,
            "vmlab-hv up"
        );
        Ok(Running {
            proc: Arc::new(HvProcess {
                proc,
                drained: control.drained(),
            }),
            control: Arc::new(control),
        })
    }

    fn emulator_binary(&self, _arch: &str) -> String {
        self.binary.display().to_string()
    }

    fn guest_asset(&self, arch: &str) -> Result<GuestAsset> {
        crate::guest_asset::ensure_guest_asset(arch)
    }
}

/// Wait for the control socket to answer the handshake, failing fast — and
/// quoting what the hypervisor wrote past `log_from` — if it exits first.
async fn connect_retry(
    sock: &Path,
    proc: &Arc<dyn Process>,
    log: &Path,
    log_from: u64,
) -> Result<HvControl> {
    let mut last = None;
    for _ in 0..100 {
        if !proc.is_running() {
            return Err(crate::qemu::process::exited_during_startup(
                "vmlab-hv",
                &proc.exit_status().unwrap_or_default(),
                log,
                log_from,
            ));
        }
        match HvControl::connect(sock).await {
            Ok(c) => return Ok(c),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    proc.kill().await;
    let why = last.map(|e| format!(": {e:#}")).unwrap_or_default();
    bail!(
        "vmlab-hv control socket {} never came up{why}",
        sock.display()
    )
}

#[cfg(test)]
mod tests;
