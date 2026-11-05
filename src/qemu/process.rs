//! QEMU (and swtpm) process management: spawn with logs, watch for exit,
//! kill. Graceful-stop policy lives in the lab daemon's lifecycle (§7.2);
//! this layer is mechanics only.
//!
//! The waiter task owns the `Child` exclusively (holding a lock across
//! `child.wait()` would deadlock `kill()`); killing goes through a signal to
//! the recorded pid instead.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use tokio::process::Command;
use tokio::sync::watch;

/// Conservatively hide the remainder of any argv element carrying an OSK.
pub fn redact_args(args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| match arg.split_once("osk=") {
            Some((prefix, _)) => format!("{prefix}osk=[REDACTED]"),
            None => arg.clone(),
        })
        .collect()
}

fn osk_secrets(args: &[String]) -> Vec<String> {
    args.iter()
        .filter_map(|arg| arg.split_once("osk=").map(|(_, value)| value))
        .filter(|value| !value.is_empty())
        .flat_map(|value| [value.to_string(), value.replace(",,", ",")])
        .collect()
}

async fn copy_redacted(
    stream: impl tokio::io::AsyncRead + Unpin,
    log: std::fs::File,
    secrets: Vec<String>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(stream);
    let mut log = tokio::fs::File::from_std(log);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).await? != 0 {
        let mut text = String::from_utf8_lossy(&line).into_owned();
        for secret in &secrets {
            text = text.replace(secret, "[REDACTED]");
        }
        log.write_all(text.as_bytes()).await?;
        line.clear();
    }
    log.flush().await
}

/// A spawned VM (or helper) process.
pub struct Proc {
    pub name: String,
    /// 0 once the process has been reaped.
    pid: AtomicU32,
    /// Becomes Some(status_string) when the process exits.
    exited: watch::Receiver<Option<String>>,
}

/// A parent-held fd installed at a fixed descriptor number in the child —
/// how pre-opened tap netdevs reach QEMU (`-netdev tap,fd=N`).
pub struct ChildFd {
    pub parent: std::os::fd::OwnedFd,
    pub child: i32,
}

/// Changes to the environment a child inherits from this process: `remove`
/// is applied first, then `set`. The default leaves it untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildEnv {
    pub remove: Vec<String>,
    pub set: Vec<(String, String)>,
}

impl Proc {
    /// Spawn `binary` with `args` under `env`, stdout+stderr appended to
    /// `log_path`.
    pub async fn spawn(
        name: &str,
        binary: &str,
        args: &[String],
        env: &ChildEnv,
        log_path: &Path,
    ) -> Result<Arc<Proc>> {
        Self::spawn_with_fds(name, binary, args, env, log_path, Vec::new()).await
    }

    /// [`Proc::spawn`], additionally installing `fds` at their fixed
    /// descriptor numbers in the child.
    pub async fn spawn_with_fds(
        name: &str,
        binary: &str,
        args: &[String],
        env: &ChildEnv,
        log_path: &Path,
        fds: Vec<ChildFd>,
    ) -> Result<Arc<Proc>> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let log_err = log.try_clone()?;
        let secrets = osk_secrets(args);
        let filter_output = !secrets.is_empty();

        let mut cmd = Command::new(binary);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(if filter_output {
                Stdio::piped()
            } else {
                Stdio::from(log.try_clone()?)
            })
            .stderr(if filter_output {
                Stdio::piped()
            } else {
                Stdio::from(log_err.try_clone()?)
            })
            .kill_on_drop(false);
        for var in &env.remove {
            cmd.env_remove(var);
        }
        cmd.envs(env.set.iter().map(|(k, v)| (k, v)));
        #[cfg(feature = "ebpf")]
        if !fds.is_empty() {
            use command_fds::CommandFdExt as _;
            let mappings = fds
                .into_iter()
                .map(|f| command_fds::FdMapping {
                    parent_fd: f.parent,
                    child_fd: f.child,
                })
                .collect();
            cmd.as_std_mut()
                .fd_mappings(mappings)
                .map_err(|e| anyhow::anyhow!("child fd mapping: {e:?}"))?;
        }
        #[cfg(not(feature = "ebpf"))]
        anyhow::ensure!(
            fds.is_empty(),
            "pre-opened netdev fds need the `ebpf` feature"
        );

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {binary} for {name}"))?;

        let output = if filter_output {
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");
            Some((
                tokio::spawn(copy_redacted(stdout, log, secrets.clone())),
                tokio::spawn(copy_redacted(stderr, log_err, secrets)),
            ))
        } else {
            None
        };

        let (tx, rx) = watch::channel(None);
        let proc = Arc::new(Proc {
            name: name.to_string(),
            pid: AtomicU32::new(child.id().unwrap_or(0)),
            exited: rx,
        });

        // Waiter task: sole owner of the Child; reaps and publishes the
        // exit status.
        let watcher = proc.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            if let Some((stdout, stderr)) = output {
                for task in [stdout, stderr] {
                    if !matches!(task.await, Ok(Ok(()))) {
                        tracing::warn!("emulator output could not be written to its log");
                    }
                }
            }
            let s = match status {
                Ok(st) => st.to_string(),
                Err(e) => format!("wait failed: {e}"),
            };
            watcher.pid.store(0, Ordering::SeqCst);
            let _ = tx.send(Some(s));
        });

        Ok(proc)
    }

    pub fn pid(&self) -> Option<u32> {
        match self.pid.load(Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }

    pub fn is_running(&self) -> bool {
        self.exited.borrow().is_none()
    }

    /// Exit status string, if the process has exited.
    pub fn exit_status(&self) -> Option<String> {
        self.exited.borrow().clone()
    }

    /// Wait for exit with a timeout. Ok(status) on exit, Err on timeout.
    pub async fn wait_exit(&self, timeout: std::time::Duration) -> Result<String> {
        let mut rx = self.exited.clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(s) = rx.borrow().clone() {
                return Ok(s);
            }
            tokio::time::timeout_at(deadline, rx.changed())
                .await
                .map_err(|_| anyhow::anyhow!("{} did not exit within {timeout:?}", self.name))?
                .map_err(|_| anyhow::anyhow!("process watcher gone"))?;
        }
    }

    /// SIGKILL the process (the hard end of the §7.2 stop ladder).
    pub async fn kill(&self) {
        if let Some(pid) = self.pid() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// How many of its last lines a process that died during startup is quoted
/// by. QEMU says why in one or two; the rest is margin for a warning or two
/// printed ahead of the fatal line.
pub const STARTUP_TAIL_LINES: usize = 8;

/// The length of `log` now: where the next run's output will start, since
/// every run appends to the same file.
pub fn log_offset(log: &Path) -> u64 {
    std::fs::metadata(log).map(|m| m.len()).unwrap_or(0)
}

/// What starts the lines vmlab itself writes into an emulator's log, so a
/// startup-failure tail can leave them out.
pub const LAUNCH_LOG_PREFIX: &str = "vmlab: ";

/// Append the emulator's argv and the variables vmlab set in its environment
/// to `log`, ahead of anything the emulator writes there: the first thing
/// anyone debugging a device line or a GPU pick needs, and otherwise only
/// recoverable from `/proc` while it runs.
pub fn write_launch_header(
    log: &Path,
    binary: &str,
    args: &[String],
    env: &ChildEnv,
) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("opening {}", log.display()))?;
    let set: Vec<String> = env.set.iter().map(|(k, v)| format!("{k}={v}")).collect();
    writeln!(
        f,
        "{LAUNCH_LOG_PREFIX}argv: {binary} {}\n{LAUNCH_LOG_PREFIX}env: {}",
        redact_args(args).join(" "),
        if set.is_empty() {
            "(none set)".to_string()
        } else {
            set.join(" ")
        }
    )?;
    Ok(())
}

/// The error for a process that exited before it answered: its exit status,
/// plus the last lines it wrote to `log` past `from`. The status alone
/// (`exit status: 1`) never says why, and the reason otherwise only lives in
/// `vmlab logs`.
pub fn exited_during_startup(what: &str, status: &str, log: &Path, from: u64) -> anyhow::Error {
    let tail = log_tail_since(log, from, STARTUP_TAIL_LINES);
    if tail.is_empty() {
        return anyhow::anyhow!("{what} exited during startup: {status}");
    }
    anyhow::anyhow!(
        "{what} exited during startup: {status}\n{}",
        tail.iter()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The last `max` non-blank lines written to `log` past byte `from` — one
/// run's output, when the file is appended to across runs. Reads at most the
/// final 64 KiB; an unreadable log yields nothing.
pub fn log_tail_since(log: &Path, from: u64, max: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 64 * 1024;
    let Ok(mut file) = std::fs::File::open(log) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = from.max(len.saturating_sub(WINDOW)).min(len);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with(LAUNCH_LOG_PREFIX))
        .collect();
    lines[lines.len().saturating_sub(max)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// Is `bin` on PATH? Used by the lab daemon's pre-`up` binary check so a
/// missing package is one clear error instead of a spawn failure mid-boot.
pub fn binary_on_path(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(bin).is_file())
}

/// Spawn swtpm for a VM (PRD §5.3): TPM 2.0 emulator on a unix control
/// socket, state under `state_dir`.
pub async fn spawn_swtpm(
    vm_name: &str,
    state_dir: &Path,
    ctrl_sock: &Path,
    log_path: &Path,
) -> Result<Arc<Proc>> {
    std::fs::create_dir_all(state_dir)?;
    let args = vec![
        "socket".to_string(),
        "--tpm2".to_string(),
        "--tpmstate".to_string(),
        format!("dir={}", state_dir.display()),
        "--ctrl".to_string(),
        format!("type=unixio,path={}", ctrl_sock.display()),
        "--terminate".to_string(),
    ];
    Proc::spawn(
        &format!("swtpm:{vm_name}"),
        "swtpm",
        &args,
        &ChildEnv::default(),
        log_path,
    )
    .await
}

/// What marks one lab's processes in an argv. Each prefix ends in `/`, so
/// lab `foo` never matches `foobar`'s processes:
///
/// - `name`: our QEMU/micro-VM `-name vmlab:<lab>/<machine>` value (see
///   cmdline.rs),
/// - `runtime`: the lab's runtime directory, where every helper's socket
///   lives (`swtpm --ctrl type=unixio,path=…/swtpm.sock`, `virtiofsd
///   --socket-path …/virtiofs0.sock`) and vmlab-hv's machine file and control
///   socket (`--machine …/machine.json --control …/control.sock`, PRD §22),
/// - `smb`: the lab's SMB state directory, which is how `smbd -s
///   <…>/smb/smb.conf` is recognised. It carries no runtime-dir path, and it
///   needs the lab's `root`.
struct LabIdentity {
    name: String,
    runtime: String,
    smb: Option<String>,
}

/// The programs vmlab starts for a lab, known by the basename of `argv[0]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LabExecutable {
    Qemu,
    VmlabHv,
    Swtpm,
    Virtiofsd,
    Smbd,
}

impl LabExecutable {
    /// `qemu-system-<arch>` (also a host GPU's `qemu_binary`, which keeps
    /// that name) and `qemu-kvm`, `vmlab-hv`, `swtpm`, `virtiofsd`, `smbd`.
    fn of(argv0: &[u8]) -> Option<Self> {
        let base = argv0.rsplit(|b| *b == b'/').next().unwrap_or(argv0);
        match base {
            b"vmlab-hv" => Some(Self::VmlabHv),
            b"swtpm" => Some(Self::Swtpm),
            b"virtiofsd" => Some(Self::Virtiofsd),
            b"smbd" => Some(Self::Smbd),
            b"qemu-kvm" => Some(Self::Qemu),
            b if b.starts_with(b"qemu-system-") => Some(Self::Qemu),
            _ => None,
        }
    }
}

impl LabIdentity {
    fn new(lab: &str, root: Option<&Path>) -> Self {
        Self {
            name: format!("vmlab:{lab}/"),
            runtime: format!("{}/", crate::paths::lab_runtime_dir(lab).display()),
            smb: root.map(|root| {
                format!(
                    "{}/",
                    crate::paths::lab_local_dir(root).join("smb").display()
                )
            }),
        }
    }

    /// Whether a raw `/proc/<pid>/cmdline` (NUL-separated argv) is a process
    /// vmlab starts for this lab: a known executable carrying the lab's
    /// marker in the option vmlab passes it in. A path that merely appears in
    /// an argument (an editor, `tail`, `grep` on a file under the lab's
    /// directories) does not match.
    fn started(&self, cmdline: &[u8]) -> bool {
        let argv = split_cmdline(cmdline);
        let Some((argv0, args)) = argv.split_first() else {
            return false;
        };
        let under = |prefix: &str, value: &[u8]| value.starts_with(prefix.as_bytes());
        match LabExecutable::of(argv0) {
            Some(LabExecutable::Qemu) => option_values(args, "-name")
                .into_iter()
                .any(|v| under(&self.name, v)),
            Some(LabExecutable::VmlabHv) => option_values(args, "--machine")
                .into_iter()
                .chain(option_values(args, "--control"))
                .any(|v| under(&self.runtime, v)),
            Some(LabExecutable::Swtpm) => {
                args.first() == Some(&&b"socket"[..])
                    && option_values(args, "--ctrl").into_iter().any(|v| {
                        v.split(|b| *b == b',').any(|field| {
                            field
                                .strip_prefix(b"path=")
                                .is_some_and(|path| under(&self.runtime, path))
                        })
                    })
            }
            Some(LabExecutable::Virtiofsd) => option_values(args, "--socket-path")
                .into_iter()
                .any(|v| under(&self.runtime, v)),
            Some(LabExecutable::Smbd) => self.smb.as_deref().is_some_and(|smb| {
                option_values(args, "-s")
                    .into_iter()
                    .chain(option_values(args, "--configfile"))
                    .any(|v| under(smb, v))
            }),
            None => false,
        }
    }
}

/// The argv of a raw `/proc/<pid>/cmdline`, without the empty element after
/// the final NUL.
fn split_cmdline(cmdline: &[u8]) -> Vec<&[u8]> {
    let cmdline = cmdline.strip_suffix(b"\0").unwrap_or(cmdline);
    if cmdline.is_empty() {
        return Vec::new();
    }
    cmdline.split(|b| *b == 0).collect()
}

/// Every value `args` gives `option`, as `option value` or `option=value`.
fn option_values<'a>(args: &[&'a [u8]], option: &str) -> Vec<&'a [u8]> {
    let option = option.as_bytes();
    let mut values = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if *arg == option {
            if let Some(value) = args.get(i + 1) {
                values.push(*value);
            }
        } else if let Some(value) = arg
            .strip_prefix(option)
            .and_then(|rest| rest.strip_prefix(b"="))
        {
            values.push(value);
        }
    }
    values
}

/// Whether a raw `/proc/<pid>/cmdline` is one [`kill_lab_orphans`] would
/// reap for `lab` — for the tests of argvs composed elsewhere (vmlab-hv's).
#[cfg(test)]
pub(crate) fn cmdline_belongs_to(cmdline: &[u8], lab: &str) -> bool {
    LabIdentity::new(lab, None).started(cmdline)
}

/// Does this raw `/proc/<pid>/cmdline` carry one of `markers` anywhere in any
/// argument? Broad on purpose, and only for refusing (see [`lab_processes`]):
/// it also matches an editor or `tail` holding a file under a marker path.
fn cmdline_mentions(cmdline: &[u8], markers: &[String]) -> bool {
    cmdline.split(|b| *b == 0).any(|arg| {
        markers
            .iter()
            .any(|m| arg.windows(m.len()).any(|w| w == m.as_bytes()))
    })
}

/// SIGKILL every process vmlab started for `lab` — QEMU or vmlab-hv **and**
/// their helpers (swtpm, virtiofsd, smbd). Returns how many were signalled.
///
/// Used to reap what a lab daemon orphaned by dying without stopping anything:
/// there is no `Proc` handle left, so we scan `/proc` for argvs
/// `LabIdentity::started` recognises. Pass the lab's `root` when known so
/// the lab's `smbd` is covered too — an orphaned smbd holds its port against
/// the next `up`.
pub fn kill_lab_orphans(lab: &str, root: Option<&Path>) -> usize {
    let pids = lab_orphans(&process_table(), &LabIdentity::new(lab, root));
    for pid in &pids {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(*pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    pids.len()
}

/// The pids in `table` that vmlab started for the lab `identity` names.
fn lab_orphans(table: &[(i32, Vec<u8>)], identity: &LabIdentity) -> Vec<i32> {
    table
        .iter()
        .filter(|(_, cmdline)| identity.started(cmdline))
        .map(|(pid, _)| *pid)
        .collect()
}

/// Every process still running for `lab` — QEMU and its helpers — or with
/// the lab's `-name` marker, runtime directory or working data `lab_local`
/// anywhere in its argv.
///
/// For `vmlab lab move`, which must not move a disk out from under a process
/// that has it open, and has no daemon left to ask once the name is released.
/// It only refuses, so it matches broadly: an editor on a lab file counts.
pub fn lab_processes(lab: &str, lab_local: &Path) -> Vec<i32> {
    let identity = LabIdentity::new(lab, None);
    let markers = [
        identity.name.clone(),
        identity.runtime.clone(),
        format!("{}/", lab_local.display()),
    ];
    process_table()
        .into_iter()
        .filter(|(_, cmdline)| cmdline_mentions(cmdline, &markers))
        .map(|(pid, _)| pid)
        .collect()
}

/// `(pid, raw cmdline)` of every process in `/proc` other than this one —
/// our own argv can name the lab (`vmlab __labd --lab …`).
fn process_table() -> Vec<(i32, Vec<u8>)> {
    let us = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut table = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid as u32 == us {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        table.push((pid, cmdline));
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn apple_smc_osk_is_redacted_in_headers_output_and_startup_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("qemu.log");
        let key = "TEST".repeat(16);
        let args = vec![
            "-c".into(),
            "printf '%s\\n' \"$0\"; printf '%s\\n' \"$0\" >&2; exit 1".into(),
            format!("isa-applesmc,osk={key}"),
        ];
        super::write_launch_header(&log, "/bin/sh", &args, &Default::default()).unwrap();
        let proc = super::Proc::spawn("redaction", "/bin/sh", &args, &Default::default(), &log)
            .await
            .unwrap();
        let status = proc
            .wait_exit(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(!text.contains(&key));
        assert_eq!(text.matches("osk=[REDACTED]").count(), 3);
        let error = super::exited_during_startup("QEMU", &status, &log, 0).to_string();
        assert!(error.contains("[REDACTED]"));
        assert!(!error.contains(&key));
    }
    #[tokio::test]
    async fn redaction_hides_both_qemu_escaped_and_raw_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("qemu.log");
        let key = "T, ".repeat(21) + "~";
        let escaped = key.replace(',', ",,");
        let args = vec![format!("isa-applesmc,osk={escaped}")];
        let output = format!("device option: {}\nraw key: {key}", args[0]);
        copy_redacted(
            output.as_bytes(),
            std::fs::File::create(&log).unwrap(),
            osk_secrets(&args),
        )
        .await
        .unwrap();
        let text = std::fs::read_to_string(log).unwrap();
        assert!(!text.contains(&key));
        assert!(!text.contains(&escaped));
        assert_eq!(text.matches("[REDACTED]").count(), 2);
    }

    /// Only this run's output is quoted: the log is appended to across runs,
    /// and an earlier run's failure must not be passed off as this one's.
    #[test]
    fn a_startup_failure_quotes_only_this_runs_output() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("qemu.log");
        std::fs::write(&log, "qemu: an older run's complaint\n").unwrap();
        let from = log_offset(&log);
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        std::io::Write::write_all(
            &mut f,
            b"\nqemu-system-x86_64: -drive file=/x.qcow2: Could not open '/x.qcow2'\n",
        )
        .unwrap();

        let err = exited_during_startup("QEMU", "exit status: 1", &log, from).to_string();
        assert!(
            err.starts_with("QEMU exited during startup: exit status: 1\n"),
            "{err}"
        );
        assert!(err.contains("Could not open '/x.qcow2'"), "{err}");
        assert!(!err.contains("older run"), "{err}");

        // Nothing written this run: the status alone.
        let quiet = exited_during_startup("QEMU", "signal: 6", &log, log_offset(&log));
        assert_eq!(quiet.to_string(), "QEMU exited during startup: signal: 6");
    }

    /// The launch header records the argv and what vmlab set, and a
    /// startup-failure tail leaves it out.
    #[test]
    fn the_launch_header_leads_the_log_and_stays_out_of_the_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("qemu.log");
        let env = ChildEnv {
            remove: vec!["VK_DRIVER_FILES".into()],
            set: vec![("VK_LOADER_DRIVERS_SELECT".into(), "*radeon*".into())],
        };
        write_launch_header(&log, "qemu-system-x86_64", &["-S".into()], &env).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"qemu: boom\n"))
            .unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "vmlab: argv: qemu-system-x86_64 -S");
        assert_eq!(lines[1], "vmlab: env: VK_LOADER_DRIVERS_SELECT=*radeon*");
        assert_eq!(log_tail_since(&log, 0, 5), vec!["qemu: boom"]);
    }

    #[test]
    fn the_tail_keeps_the_last_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("qemu.log");
        let body: String = (0..20).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&log, body).unwrap();
        let tail = log_tail_since(&log, 0, 3);
        assert_eq!(tail, vec!["line 17", "line 18", "line 19"]);
        assert!(log_tail_since(&tmp.path().join("missing.log"), 0, 3).is_empty());
    }

    #[tokio::test]
    async fn spawn_watch_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("p.log");
        let p = Proc::spawn(
            "t",
            "sh",
            &["-c".into(), "echo hi; exit 3".into()],
            &ChildEnv::default(),
            &log,
        )
        .await
        .unwrap();
        let status = p
            .wait_exit(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(status.contains('3'), "{status}");
        assert!(!p.is_running());
        assert!(p.pid().is_none());
        let logged = std::fs::read_to_string(&log).unwrap();
        assert_eq!(logged.trim(), "hi");
    }

    /// A child sees `remove`d variables gone even when this process has
    /// them, and `set` ones with their value.
    #[tokio::test]
    async fn a_child_env_removes_then_sets_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("p.log");
        // PATH is always in the test's environment, so its removal shows.
        let env = ChildEnv {
            remove: vec!["PATH".into()],
            set: vec![("VMLAB_SPAWN_TEST".into(), "*radeon*".into())],
        };
        let p = Proc::spawn("t", "/usr/bin/env", &[], &env, &log)
            .await
            .unwrap();
        p.wait_exit(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(!logged.lines().any(|l| l.starts_with("PATH=")), "{logged}");
        assert!(
            logged.lines().any(|l| l == "VMLAB_SPAWN_TEST=*radeon*"),
            "{logged}"
        );
    }

    #[tokio::test]
    async fn kill_terminates_even_after_waiter_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Proc::spawn(
            "t",
            "sleep",
            &["30".into()],
            &ChildEnv::default(),
            &tmp.path().join("p.log"),
        )
        .await
        .unwrap();
        assert!(p.is_running());
        // Let the waiter task start waiting first — this order used to
        // deadlock when the waiter held a lock across child.wait().
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        p.kill().await;
        let status = p
            .wait_exit(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert!(status.contains("signal"), "{status}");
    }

    #[test]
    fn binary_on_path_probes_path() {
        assert!(binary_on_path("sh"));
        assert!(!binary_on_path("definitely-not-a-real-binary-1a2b3c"));
    }

    #[test]
    fn lab_qemu_cmdline_matching() {
        let lab = LabIdentity::new("mylab", None);
        // Real-ish argv: NUL-separated, with the -name marker.
        let cmd = b"qemu-system-x86_64\0-name\0vmlab:mylab/web\0-machine\0q35\0";
        assert!(lab.started(cmd));
        // A host GPU's `qemu_binary` is an absolute path to the same name.
        assert!(lab.started(b"/opt/qemu-vmlab/bin/qemu-system-x86_64\0-name\0vmlab:mylab/web\0"));
        // A different lab must not match.
        assert!(!LabIdentity::new("other", None).started(cmd));
        // Prefix collision: `my` must not match `mylab`'s VMs.
        assert!(!LabIdentity::new("my", None).started(cmd));
        // No marker at all.
        assert!(!lab.started(b"sleep\x0030\x00"));
        // The marker outside `-name` is not ours.
        assert!(!lab.started(b"qemu-system-x86_64\0-name\0other\0-L\0vmlab:mylab/web\0"));
        assert!(!lab.started(b""));
    }

    /// The argvs vmlab composes for one lab's machines, with the lab's root.
    fn lab_argvs(lab: &str, root: &Path) -> Vec<(&'static str, String)> {
        let run = crate::paths::lab_runtime_dir(lab);
        let smb = crate::paths::lab_local_dir(root).join("smb");
        vec![
            (
                "qemu",
                format!("qemu-system-x86_64\0-name\0vmlab:{lab}/web\0-machine\0q35\0"),
            ),
            (
                "vmlab-hv",
                format!(
                    "/opt/vmlab-hv/bin/vmlab-hv\0--machine\0{0}/vms/web/machine.json\0\
                     --control\0{0}/vms/web/control.sock\0--paused\0",
                    run.display()
                ),
            ),
            (
                "swtpm",
                format!(
                    "swtpm\0socket\0--tpm2\0--tpmstate\0dir=/labs/state/tpm\0\
                     --ctrl\0type=unixio,path={}/vms/web/swtpm.sock\0--terminate\0",
                    run.display()
                ),
            ),
            (
                "virtiofsd",
                format!(
                    "/usr/libexec/virtiofsd\0--socket-path\0{}/vms/web/virtiofs0.sock\0\
                     --shared-dir\0/srv\0--cache\0auto\0",
                    run.display()
                ),
            ),
            (
                "smbd",
                format!(
                    "smbd\0-F\0--no-process-group\0-s\0{0}/smb.conf\0-l\0{0}\0",
                    smb.display()
                ),
            ),
        ]
    }

    /// QEMU, vmlab-hv and the helpers a crashed daemon orphans are known by
    /// their executable and the option vmlab passes the lab's path in.
    #[test]
    fn helper_cmdline_matching() {
        let root = Path::new("/labs/mylab");
        let lab = LabIdentity::new("mylab", Some(root));
        for (what, argv) in lab_argvs("mylab", root) {
            assert!(lab.started(argv.as_bytes()), "{what}: {argv:?}");
        }

        // Another lab's processes are untouched.
        let other = LabIdentity::new("otherlab", Some(Path::new("/labs/otherlab")));
        for (what, argv) in lab_argvs("mylab", root) {
            assert!(!other.started(argv.as_bytes()), "{what}: {argv:?}");
        }

        // Without the lab's root its smbd is not known, the rest still are.
        let rootless = LabIdentity::new("mylab", None);
        for (what, argv) in lab_argvs("mylab", root) {
            assert_eq!(rootless.started(argv.as_bytes()), what != "smbd", "{what}");
        }

        // `--option=value` is the same option.
        let run = crate::paths::lab_runtime_dir("mylab");
        let vfsd = format!("virtiofsd\0--socket-path={}/vms/web/v.sock\0", run.display());
        assert!(lab.started(vfsd.as_bytes()));
    }

    /// F7: a process that only mentions a lab path in an argument — an
    /// editor, a pager, `tail`, `grep` — is not one vmlab started.
    #[test]
    fn processes_that_only_mention_a_lab_path_are_not_matched() {
        let root = Path::new("/labs/mylab");
        let lab = LabIdentity::new("mylab", Some(root));
        let run = crate::paths::lab_runtime_dir("mylab").display().to_string();
        let smb = crate::paths::lab_local_dir(root)
            .join("smb")
            .display()
            .to_string();
        let argvs = [
            format!("vim\0{smb}/smb.conf\0"),
            format!("/usr/bin/nvim\0{run}/vms/web/machine.json\0"),
            format!("tail\0-F\0{run}/vms/web/qemu.log\0"),
            format!("less\0{smb}/log.smbd\0"),
            format!("grep\0-r\0vmlab:mylab/\0{run}/\0"),
            format!("grep\0-name\0vmlab:mylab/web\0"),
            format!("find\0{run}\0-name\0vmlab:mylab/web\0"),
            format!("bash\0-c\0cat {run}/vms/web/swtpm.sock\0"),
            format!("socat\0-\0UNIX-CONNECT:{run}/vms/web/control.sock\0"),
            // Our helper names with the path in an option vmlab never uses.
            format!("swtpm\0socket\0--tpmstate\0dir={run}/tpm\0"),
            format!("swtpm\0ioctl\0--unix\0{run}/vms/web/swtpm.sock\0"),
            format!("virtiofsd\0--shared-dir\0{run}/share\0"),
            format!("smbd\0-l\0{smb}/\0"),
            format!("vmlab-hv\0--probe\0{run}/\0"),
            format!("qemu-img\0info\0{run}/vms/web/disk.qcow2\0"),
        ];
        for argv in &argvs {
            assert!(!lab.started(argv.as_bytes()), "{argv:?}");
        }
    }

    /// The reap picks its pids from a process table, so the selection is
    /// tested on a fake one; no test signals a real process.
    #[test]
    fn lab_orphans_picks_only_the_labs_vmlab_processes() {
        let root = Path::new("/labs/mylab");
        let run = crate::paths::lab_runtime_dir("mylab").display().to_string();
        let mut table: Vec<(i32, Vec<u8>)> = lab_argvs("mylab", root)
            .into_iter()
            .zip(100..)
            .map(|((_, argv), pid)| (pid, argv.into_bytes()))
            .collect();
        table.extend(
            lab_argvs("otherlab", Path::new("/labs/otherlab"))
                .into_iter()
                .zip(200..)
                .map(|((_, argv), pid)| (pid, argv.into_bytes())),
        );
        table.push((300, format!("vim\0{run}/vms/web/machine.json\0").into_bytes()));
        table.push((301, format!("tail\0-f\0{run}/vms/web/qemu.log\0").into_bytes()));
        table.push((302, Vec::new())); // a kernel thread's empty cmdline

        let picked = lab_orphans(&table, &LabIdentity::new("mylab", Some(root)));
        assert_eq!(picked, vec![100, 101, 102, 103, 104]);
        assert!(lab_orphans(&table, &LabIdentity::new("some-other-lab", None)).is_empty());
    }

    /// `lab move` only refuses, so it still counts a process that mentions
    /// the lab's paths.
    #[test]
    fn lab_move_counts_any_mention() {
        let markers = ["/labs/mylab/.vmlab/".to_string()];
        assert!(cmdline_mentions(b"vim\0/labs/mylab/.vmlab/disk.qcow2\0", &markers));
        assert!(cmdline_mentions(b"qemu\0-drive\0file=/labs/mylab/.vmlab/d\0", &markers));
        assert!(!cmdline_mentions(b"vim\0/labs/other/.vmlab/d\0", &markers));
    }
}
