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
//! A VM's shares are vmlab-hv's own in-process virtio-fs (phase 2e), on the
//! host directory, so no virtiofsd runs for them; SMB still takes what the
//! guest cannot mount, by the share plan's rules. A VM's TPM is the host's
//! swtpm, started as for QEMU.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::{GuestAsset, Hypervisor, Launch, LaunchSpec, Process, Running};
use crate::qemu::Proc;

pub(crate) mod control;
pub(crate) mod describe;
pub(crate) mod display;
pub(crate) mod tsc;

use control::{HvControl, HvProcess};

/// One paused NIC installation. Descriptor order is TAP queues, then vhost files.
pub(crate) struct NetAttachment {
    pub mac: String,
    pub queue_pairs: u16,
    pub vhost: bool,
    pub fds: Vec<OwnedFd>,
}

fn net_attachments(
    nics: &[crate::qemu::NicSpec],
    fds: Vec<crate::qemu::process::ChildFd>,
) -> Result<Vec<NetAttachment>> {
    let mut pending = HashMap::new();
    for fd in fds {
        if pending.insert(fd.child, fd.parent).is_some() {
            bail!("duplicate netdev fd mapping {}", fd.child);
        }
    }
    let mut attachments = Vec::new();
    for nic in nics {
        let crate::qemu::NicBackend::TapQueues { tap_fds, vhost_fds } = &nic.backend else {
            continue;
        };
        if !(1..=31).contains(&tap_fds.len())
            || (!vhost_fds.is_empty() && vhost_fds.len() != tap_fds.len())
        {
            bail!("NIC {} has incompatible TAP/vhost queue counts", nic.mac);
        }
        let mut fds = Vec::with_capacity(tap_fds.len() + vhost_fds.len());
        for number in tap_fds.iter().chain(vhost_fds) {
            fds.push(pending.remove(number).with_context(|| {
                format!(
                    "NIC {} has no unique netdev mapping for fd {number}",
                    nic.mac
                )
            })?);
        }
        attachments.push(NetAttachment {
            mac: nic.mac.to_string(),
            queue_pairs: tap_fds.len() as u16,
            vhost: !vhost_fds.is_empty(),
            fds,
        });
    }
    if !pending.is_empty() {
        bail!("unreferenced netdev fd mappings in vmlab-hv launch");
    }
    Ok(attachments)
}

/// The binary's name, which is also what a host config without `vmlab_hv`
/// finds on `PATH`.
pub const BINARY: &str = "vmlab-hv";

/// `vmlab-hv` on this host.
pub struct VmlabHv {
    binary: PathBuf,
    /// Whether this adapter runs a VM, whose shares vmlab-hv serves in
    /// process rather than through a virtiofsd.
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
        crate::guest_asset::candidate_dirs()
            .into_iter()
            .map(|dir| dir.join("firmware/x86_64")),
    );
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
            seabios: find(&["bios-vmlab-q35.bin", "bios-256k.bin"]),
            vgabios_std: find(&["vgabios-stdvga.bin"]),
            vgabios_virtio: find(&["vgabios-virtio.bin"]),
            // vmlab-hv's own iPXE build, never QEMU's (HV phase-2 D14, D16).
            ipxe_virtio: find(&["1af41000.rom"]),
            ipxe_virtio_efi: find(&["virtio-net.rom", "1af41000.efirom"]),
        }
    }
}

/// Where the machine description goes: beside the control socket, in the
/// machine's runtime directory, which `kill_lab_orphans` recognises in an
/// argv — so an orphaned `vmlab-hv` is reaped like an orphaned QEMU.
fn machine_file(control_sock: &Path) -> PathBuf {
    control_sock.with_file_name("machine.json")
}

/// A description that holds a secret, handed to vmlab-hv through a pipe at
/// the machine file's path, then replaced there by its redacted form, which
/// is what every later reader (and `kill_lab_orphans`' argv match) sees.
struct SecretHandover {
    path: PathBuf,
    secret: Vec<u8>,
    redacted: Vec<u8>,
}

/// How long vmlab-hv may take to open its description.
const HANDOVER_DEADLINE: Duration = Duration::from_secs(30);

struct Handing {
    task: tokio::task::JoinHandle<Result<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    path: PathBuf,
    redacted: Vec<u8>,
}

impl SecretHandover {
    fn prepare(path: &Path, secret: Vec<u8>, redacted: Vec<u8>) -> Result<Self> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
        nix::unistd::mkfifo(path, nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            secret,
            redacted,
        })
    }

    /// Write the description once a reader opens the pipe.
    fn start(self) -> Handing {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (path, secret, flag) = (self.path.clone(), self.secret, stop.clone());
        let task = tokio::task::spawn_blocking(move || write_to_reader(&path, &secret, &flag));
        Handing {
            task,
            stop,
            path: self.path,
            redacted: self.redacted,
        }
    }
}

impl Handing {
    /// Wait for the write (or stop it when the launch already failed), then
    /// leave the redacted description at the path.
    async fn finish(self, launched: bool) -> Result<()> {
        if !launched {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let written = self
            .task
            .await
            .map_err(|e| anyhow::anyhow!("description handover task: {e}"))
            .and_then(|r| r);
        let _ = std::fs::remove_file(&self.path);
        std::fs::write(&self.path, &self.redacted)
            .with_context(|| format!("writing {}", self.path.display()))?;
        written
    }
}

/// Open `path` (a pipe) for writing as soon as a reader has it open, write
/// `bytes` and close, so the reader sees end of file.
fn write_to_reader(
    path: &Path,
    bytes: &[u8],
    stop: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let deadline = std::time::Instant::now() + HANDOVER_DEADLINE;
    let mut pipe = loop {
        // Non-blocking: ENXIO until the reader opens its end.
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(pipe) => break pipe,
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {}
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        }
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            bail!("vmlab-hv exited before reading its description");
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "vmlab-hv did not read its description within {}s",
                HANDOVER_DEADLINE.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Blocking from here: the reader is draining it.
    let flags = nix::fcntl::fcntl(&pipe, nix::fcntl::FcntlArg::F_GETFL)?;
    let flags = nix::fcntl::OFlag::from_bits_truncate(flags) & !nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(&pipe, nix::fcntl::FcntlArg::F_SETFL(flags))?;
    pipe.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
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
    /// The same swtpm QEMU's VMs get (HV-D17): vmlab-hv's `tpm-tis` speaks
    /// its control socket.
    async fn start_tpm(
        &self,
        machine: &str,
        state_dir: &Path,
        ctrl_sock: &Path,
        log: &Path,
    ) -> Result<Arc<dyn Process>> {
        super::qemu::start_host_swtpm(machine, state_dir, ctrl_sock, log).await
    }

    fn virtiofsd(&self) -> Option<crate::qemu::virtiofsd::Virtiofsd> {
        if self.vm {
            return Some(crate::qemu::virtiofsd::Virtiofsd::in_process(&self.binary));
        }
        crate::qemu::virtiofsd::found()
    }

    fn virtiofs_in_process(&self) -> bool {
        self.vm
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
        // A description carrying the Apple SMC key goes through a pipe at the
        // usual path, never a file (HV macOS phase: no file ever holds it).
        let handover = if description.applesmc.is_some() {
            Some(SecretHandover::prepare(
                &machine,
                serde_json::to_vec(&description)?,
                serde_json::to_vec_pretty(&description.redacted())?,
            )?)
        } else {
            std::fs::write(&machine, serde_json::to_vec_pretty(&description)?)
                .with_context(|| format!("writing {}", machine.display()))?;
            None
        };
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
        let nics = match &spec.machine {
            Launch::Vm { paths, .. } => paths.nics.as_slice(),
            Launch::Container { .. } => &[],
        };
        let attachments = net_attachments(nics, spec.fds)?;
        let args = args(&machine, control_sock);
        crate::qemu::process::write_launch_header(&spec.log, &binary, &args, &spec.env)?;
        let log_from = crate::qemu::process::log_offset(&spec.log);
        let proc = Proc::spawn(&spec.label, &binary, &args, &spec.env, &spec.log).await?;
        let proc: Arc<dyn Process> = super::qemu::host_process(proc);
        let handing = handover.map(SecretHandover::start);
        let connected = connect_retry(control_sock, &proc, &spec.log, log_from).await;
        if let Some(handing) = handing {
            // vmlab-hv reads its description before it binds the control
            // socket, so a connection means the pipe was read.
            let delivered = handing.finish(connected.is_ok()).await;
            if let Err(error) = delivered
                && connected.is_ok()
            {
                proc.kill().await;
                let _ = proc.wait_exit(Duration::from_secs(5)).await;
                return Err(error).context("handing the machine description to vmlab-hv");
            }
        }
        let mut control = connected?;
        for attachment in attachments {
            if let Err(error) = control.attach_net(attachment).await {
                proc.kill().await;
                let _ = proc.wait_exit(Duration::from_secs(5)).await;
                return Err(error).context("installing passed TAP queues before VM resume");
            }
        }
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
