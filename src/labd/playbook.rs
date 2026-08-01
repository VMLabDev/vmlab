//! config-weave playbook runs against lab machines: guest-binary discovery,
//! the per-machine run engine behind `playbook.check` / `playbook.apply`
//! and the `up`-time apply steps, and the in-flight op registry.
//!
//! Guest layout stays consistent with config-weave's own testlab runner:
//! the binary at `/weave/config-weave` (Linux) or `C:/weave/config-weave.exe`
//! (Windows), playbooks under `<...>/weave/playbooks/`, forward slashes
//! throughout.

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::model::{Lab, Playbook};
use crate::labd::lab::LabRuntime;
use crate::labd::vm_agent::{AgentHandle, SessionEvent};
use crate::proto::CommandError;
use crate::scripting::OutputSink;
use crate::sync::LockRecover;

/// Guest OS family — picks the config-weave binary and path scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOs {
    Linux,
    Windows,
}

/// Env var overriding the config-weave binary directory (between the host
/// config setting and the XDG default).
pub const ENV_BIN_DIR: &str = "VMLAB_CONFIG_WEAVE_DIR";

/// Where the config-weave guest binaries live. Precedence: host config
/// `config_weave_bin_dir` → `$VMLAB_CONFIG_WEAVE_DIR` → where config-weave's
/// `just install` puts them. Pure on its inputs so tests need no real env.
pub fn resolve_bin_dir(cfg: Option<&Path>, env: Option<&str>) -> PathBuf {
    if let Some(d) = cfg {
        return d.to_path_buf();
    }
    if let Some(d) = env.filter(|s| !s.is_empty()) {
        return PathBuf::from(d);
    }
    crate::paths::config_weave_bin_dir()
}

/// [`resolve_bin_dir`] with the real environment.
pub fn default_bin_dir(cfg: Option<&Path>) -> PathBuf {
    let env = std::env::var(ENV_BIN_DIR).ok();
    resolve_bin_dir(cfg, env.as_deref())
}

/// The host-side config-weave binary for one guest (OS, arch). config-weave
/// cross-builds exactly two targets, both x86_64.
pub fn weave_binary(dir: &Path, os: GuestOs, arch: &str) -> Result<PathBuf> {
    if arch != "x86_64" {
        bail!("config-weave ships guest binaries only for x86_64 (guest is {arch})");
    }
    let name = match os {
        GuestOs::Linux => "config-weave-linux-x86_64",
        GuestOs::Windows => "config-weave-windows-x86_64.exe",
    };
    let path = dir.join(name);
    if !path.is_file() {
        bail!(
            "config-weave binary {} not found — run `just install` in the config-weave \
             repo, or point `config_weave_bin_dir` in ~/.config/vmlab/config.wcl (or \
             ${ENV_BIN_DIR}) at a directory holding the release binaries",
            path.display()
        );
    }
    Ok(path)
}

/// Guest OS family from the resolved profile name, for selecting the
/// config-weave binary — where XP-versus-modern Windows is irrelevant, so
/// `windows-legacy` answers `Windows` here. Deliberately not the same
/// question as [`crate::smb::guest_os_hint`], which selects a share mount
/// command and for which the distinction matters a great deal. Containers are
/// always Linux; VM answers are confirmed against the agent handshake
/// (`AgentInfo.os`) once connected.
pub fn guest_os_of(profile: Option<&str>) -> GuestOs {
    match profile {
        Some(p) if p.starts_with("windows") => GuestOs::Windows,
        _ => GuestOs::Linux,
    }
}

/// The `playbook {}` blocks declared inside `machine`, in declaration order.
/// `None` when the lab has no such VM or container.
pub fn playbooks_of<'a>(lab: &'a Lab, machine: &str) -> Option<&'a [Playbook]> {
    if let Some(vm) = lab.vms.iter().find(|v| v.name == machine) {
        return Some(&vm.playbooks);
    }
    lab.containers
        .iter()
        .find(|c| c.name == machine)
        .map(|c| c.playbooks.as_slice())
}

/// The single playbook block `machine` declares, optionally narrowed by
/// playbook path and/or play name. Errors name the candidates so callers
/// can disambiguate.
pub fn resolve_playbook<'a>(
    lab: &'a Lab,
    machine: &str,
    playbook: Option<&str>,
    play: Option<&str>,
) -> Result<&'a Playbook, String> {
    let declared =
        playbooks_of(lab, machine).ok_or_else(|| format!("no vm or container \"{machine}\""))?;
    let matches: Vec<&Playbook> = declared
        .iter()
        .filter(|p| playbook.is_none_or(|f| p.path.display().to_string() == f))
        .filter(|p| play.is_none_or(|f| p.play == f))
        .collect();
    match matches.len() {
        0 => Err(format!("\"{machine}\" declares no matching playbook")),
        1 => Ok(matches[0]),
        _ => {
            let candidates: Vec<String> = matches
                .iter()
                .map(|p| format!("{} play {}", p.path.display(), p.play))
                .collect();
            Err(format!(
                "machine \"{machine}\" declares {} playbooks — pass playbook/play \
                 to pick one (candidates: {})",
                matches.len(),
                candidates.join(", ")
            ))
        }
    }
}

// ---- guest layout ----------------------------------------------------------

const MAX_REBOOTS: u32 = 3;
/// Hard ceiling per config-weave invocation (matches the first-boot policy:
/// slow is fine, a hung guest must not wedge `up` forever… doubled — applies
/// converge whole systems).
const RUN_TIMEOUT: Duration = Duration::from_secs(3600);
/// Attempts for the guest push steps (binary + playbook folder). Windows can
/// hold files briefly — a lingering config-weave process or the antivirus
/// scanning a freshly written binary shows up as a "file in use" sharing
/// violation — and the pushes are idempotent, so ride it out.
const PUSH_ATTEMPTS: u32 = 5;

/// Run an idempotent guest operation, retrying failures with backoff
/// (3s → 6s → 12s → 15s, ~36s total across [`PUSH_ATTEMPTS`] attempts).
/// Every retry is announced through `log` so streamed output shows what is
/// being waited on. `op` returns owning futures (plain `FnMut`, not an async
/// closure — lending closures trip rustc's higher-ranked `Send` inference
/// and poison the whole `up` future at unrelated spawn sites).
async fn retry_push<T, F>(what: &str, log: &impl Fn(&str), mut op: impl FnMut() -> F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let mut delay = Duration::from_secs(3);
    for attempt in 1..PUSH_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                log(&format!(
                    "{what} failed ({e:#}) — retrying ({attempt}/{})",
                    PUSH_ATTEMPTS - 1
                ));
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(15));
            }
        }
    }
    op().await
}

fn guest_base(os: GuestOs) -> &'static str {
    match os {
        GuestOs::Linux => "/weave",
        GuestOs::Windows => "C:/weave",
    }
}

fn guest_binary_path(os: GuestOs) -> String {
    match os {
        GuestOs::Linux => format!("{}/config-weave", guest_base(os)),
        GuestOs::Windows => format!("{}/config-weave.exe", guest_base(os)),
    }
}

/// Flatten a lab-relative playbook path into one guest directory name
/// (`playbooks/baseline` → `playbooks__baseline`), collision-free per lab.
pub fn sanitize_guest_dir(path: &Path) -> String {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        "playbook".to_string()
    } else {
        parts.join("__")
    }
}

// ---- config-weave output handling ------------------------------------------

/// Line-split a stderr chunk, carrying a trailing partial line in `carry`
/// for the next chunk.
pub fn split_ndjson_lines(carry: &mut String, chunk: &[u8]) -> Vec<String> {
    carry.push_str(&String::from_utf8_lossy(chunk));
    let mut lines = Vec::new();
    while let Some(pos) = carry.find('\n') {
        let line: String = carry.drain(..=pos).collect();
        let line = line.trim_end_matches(['\n', '\r']);
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Human line for one config-weave `--events-ndjson` event (None = too noisy
/// to surface — step start/phase churn).
pub fn render_cw_event(ev: &Value) -> Option<String> {
    match ev["event"].as_str()? {
        "run_started" => Some(format!(
            "play {} ({}): {} step(s)",
            ev["play"].as_str().unwrap_or("?"),
            ev["mode"].as_str().unwrap_or("?"),
            ev["steps"].as_array().map_or(0, |s| s.len()),
        )),
        "gather_started" => Some(format!(
            "gathering {} fact set(s)",
            ev["unique"].as_u64().unwrap_or(0)
        )),
        "step_finished" => {
            let mut line = format!(
                "  [{}] {} ({:.1}s)",
                ev["status"].as_str().unwrap_or("?"),
                ev["name"].as_str().unwrap_or("?"),
                ev["duration_secs"].as_f64().unwrap_or(0.0),
            );
            if let Some(msg) = ev["message"].as_str().filter(|m| !m.is_empty()) {
                line.push_str(&format!(" — {msg}"));
            }
            Some(line)
        }
        "step_resolved" => Some(format!(
            "  [{}] {} (resolved)",
            ev["status"].as_str().unwrap_or("?"),
            ev["name"].as_str().unwrap_or("?"),
        )),
        _ => None,
    }
}

/// Best-effort parse of the `--json` final report from collected stdout.
/// Resource scripts may leak stray text onto stdout; fall back to the
/// outermost `{…}` slice, and to None when there is no report at all
/// (exit 2 validation failures print diagnostics instead).
pub fn parse_report(stdout: &str) -> Option<Value> {
    let trimmed = stdout.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && v.is_object()
    {
        return Some(v);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    serde_json::from_str::<Value>(&trimmed[start..=end])
        .ok()
        .filter(Value::is_object)
}

// ---- op registry -------------------------------------------------------------

const LOG_CAP: usize = 500;

struct OpState {
    kind: &'static str,
    playbook: String,
    play: String,
    op_id: u64,
    started: chrono::DateTime<chrono::Utc>,
    log: VecDeque<String>,
    /// "running" while config-weave executes, "rebooting" while the guest
    /// restarts between apply attempts (the UI's phase indicator).
    phase: &'static str,
    /// Reboot attempt this run is on (0 until the first reboot).
    reboot_attempt: u32,
}

#[derive(Default)]
struct OpsInner {
    /// In-flight runs keyed by machine — one per machine at a time.
    ops: HashMap<String, OpState>,
    /// sha256 of the config-weave binary last pushed to each machine.
    pushed: HashMap<String, String>,
    next_op: u64,
}

/// Registry of in-flight config-weave runs (`playbook.op_status` resync
/// source) plus the per-machine pushed-binary cache. Std mutex, never held
/// across await.
#[derive(Clone, Default)]
pub struct PlaybookOps {
    inner: Arc<Mutex<OpsInner>>,
}

impl PlaybookOps {
    /// Claim `machine` for one run; the guard releases on drop so error and
    /// panic paths cannot wedge the machine.
    fn try_begin(
        &self,
        machine: &str,
        playbook: &str,
        play: &str,
        kind: &'static str,
    ) -> Result<(OpGuard, u64), CommandError> {
        let mut inner = self.inner.lock_recover();
        if let Some(op) = inner.ops.get(machine) {
            return Err(CommandError::conflict(format!(
                "{} of {} play {} already running for \"{machine}\"",
                op.kind, op.playbook, op.play
            )));
        }
        inner.next_op += 1;
        let op_id = inner.next_op;
        inner.ops.insert(
            machine.to_string(),
            OpState {
                kind,
                playbook: playbook.to_string(),
                play: play.to_string(),
                op_id,
                started: chrono::Utc::now(),
                log: VecDeque::new(),
                phase: "running",
                reboot_attempt: 0,
            },
        );
        Ok((
            OpGuard {
                ops: self.clone(),
                machine: machine.to_string(),
            },
            op_id,
        ))
    }

    fn set_phase(&self, machine: &str, phase: &'static str, reboot_attempt: u32) {
        let mut inner = self.inner.lock_recover();
        if let Some(op) = inner.ops.get_mut(machine) {
            op.phase = phase;
            op.reboot_attempt = reboot_attempt;
        }
    }

    fn append_log(&self, machine: &str, line: &str) {
        let mut inner = self.inner.lock_recover();
        if let Some(op) = inner.ops.get_mut(machine) {
            if op.log.len() == LOG_CAP {
                op.log.pop_front();
            }
            op.log.push_back(line.to_string());
        }
    }

    fn pushed_sha(&self, machine: &str) -> Option<String> {
        self.inner.lock_recover().pushed.get(machine).cloned()
    }

    fn set_pushed(&self, machine: &str, sha: &str) {
        self.inner
            .lock()
            .unwrap()
            .pushed
            .insert(machine.to_string(), sha.to_string());
    }

    /// The running op for `machine` (`playbook.list` decoration).
    pub fn op_of(&self, machine: &str) -> Value {
        let inner = self.inner.lock_recover();
        match inner.ops.get(machine) {
            Some(op) => json!({
                "machine": machine,
                "kind": op.kind,
                "op_id": op.op_id,
                "started": op.started.to_rfc3339(),
            }),
            None => Value::Null,
        }
    }

    /// All in-flight runs with log tails (`playbook.op_status` — the resync
    /// source for reconnecting UIs).
    pub fn status(&self) -> Value {
        let inner = self.inner.lock_recover();
        let mut rows: Vec<Value> = inner
            .ops
            .iter()
            .map(|(machine, op)| {
                json!({
                    "machine": machine,
                    "playbook": op.playbook,
                    "play": op.play,
                    "kind": op.kind,
                    "op_id": op.op_id,
                    "started": op.started.to_rfc3339(),
                    "log_tail": op.log.iter().collect::<Vec<_>>(),
                    "phase": op.phase,
                    "reboot_attempt": op.reboot_attempt,
                    "reboot_max": MAX_REBOOTS,
                })
            })
            .collect();
        rows.sort_by(|a, b| a["machine"].as_str().cmp(&b["machine"].as_str()));
        Value::Array(rows)
    }
}

/// Releases a [`PlaybookOps`] claim on drop.
struct OpGuard {
    ops: PlaybookOps,
    machine: String,
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        self.ops.inner.lock_recover().ops.remove(&self.machine);
    }
}

// ---- run engine ---------------------------------------------------------------

/// Which config-weave verb to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybookMode {
    Check,
    Apply,
}

impl PlaybookMode {
    pub fn verb(&self) -> &'static str {
        match self {
            PlaybookMode::Check => "check",
            PlaybookMode::Apply => "apply",
        }
    }
}

/// What one run produced. `exit_code` is config-weave's (0 ok, 1 step error,
/// 2 validation, 3 reboot still required after retries); `report` is the
/// parsed `--json` run report when stdout carried one.
#[derive(Debug)]
pub struct PlaybookOutcome {
    pub exit_code: i32,
    pub reboots: u32,
    pub report: Option<Value>,
}

/// Run one config-weave play against one machine: ensure the guest binary,
/// re-push the playbook folder (always — the whole point is a fast edit→run
/// loop), execute with `--json --events-ndjson`, stream progress to `output`
/// and the `playbook.op.*` events, and on apply exit 3 reboot the guest and
/// re-run (bounded). Infrastructure failures are `Err`; config-weave's own
/// verdict comes back as [`PlaybookOutcome::exit_code`].
pub async fn run_playbook(
    lab: &Arc<LabRuntime>,
    machine: &str,
    pb: &Playbook,
    mode: PlaybookMode,
    output: &OutputSink,
) -> Result<PlaybookOutcome> {
    let pb_path = pb.path.display().to_string();
    let (_guard, op_id) = lab
        .playbook_ops
        .try_begin(machine, &pb_path, &pb.play, mode.verb())
        .map_err(anyhow::Error::new)?;

    let base = json!({
        "machine": machine, "playbook": pb_path, "play": pb.play, "op_id": op_id,
    });
    let emit = |event: &str, extra: Value| {
        let mut payload = base.clone();
        if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                p.insert(k.clone(), v.clone());
            }
        }
        lab.events.emit(event, payload);
    };
    emit("playbook.op.start", json!({"mode": mode.verb()}));

    let result = run_inner(lab, machine, pb, mode, output, op_id).await;
    match &result {
        Ok(outcome) => {
            emit(
                "playbook.op.done",
                json!({
                    "exit_code": outcome.exit_code,
                    "reboots": outcome.reboots,
                    "report": outcome.report.clone().unwrap_or(Value::Null),
                }),
            );
            if outcome.exit_code == 0 && mode == PlaybookMode::Apply {
                lab.events.emit(
                    "playbook.applied",
                    json!({"machine": machine, "playbook": pb_path, "play": pb.play}),
                );
            } else if outcome.exit_code != 0 {
                lab.events.emit(
                    "playbook.failed",
                    json!({
                        "machine": machine, "playbook": pb_path, "play": pb.play,
                        "mode": mode.verb(), "exit_code": outcome.exit_code,
                    }),
                );
            }
        }
        Err(e) => {
            emit("playbook.op.error", json!({"error": format!("{e:#}")}));
            lab.events.emit(
                "playbook.failed",
                json!({
                    "machine": machine, "playbook": pb_path, "play": pb.play,
                    "mode": mode.verb(),
                }),
            );
        }
    }
    result
}

async fn run_inner(
    lab: &Arc<LabRuntime>,
    machine: &str,
    pb: &Playbook,
    mode: PlaybookMode,
    output: &OutputSink,
    op_id: u64,
) -> Result<PlaybookOutcome> {
    let pb_path = pb.path.display().to_string();
    let m = lab.machine(machine)?;
    let arch = m.arch();

    // A line to everything watching: CLI stream / lab log, the op registry's
    // resync tail, and the event bus.
    let base = json!({
        "machine": machine, "playbook": pb_path, "play": pb.play, "op_id": op_id,
    });
    let log_line = |line: &str| {
        output(format!("{line}\n"));
        lab.playbook_ops.append_log(machine, line);
        let mut payload = base.clone();
        payload["line"] = Value::String(line.to_string());
        lab.events.emit("playbook.op.log", payload);
    };
    // Explicit run phase ("running" ↔ "rebooting") so UIs can show a reboot
    // as a reboot instead of a silent stall; mirrored into the op registry
    // for reconnect resync.
    let set_phase = |phase: &'static str, attempt: u32| {
        lab.playbook_ops.set_phase(machine, phase, attempt);
        let mut payload = base.clone();
        payload["phase"] = Value::String(phase.to_string());
        payload["attempt"] = attempt.into();
        payload["max"] = MAX_REBOOTS.into();
        lab.events.emit("playbook.op.phase", payload);
    };

    let agent = m.wait_agent(Duration::from_secs(300)).await?;
    // The profile-name heuristic decides before boot; the agent's handshake
    // is authoritative once connected.
    let os = match agent.info().os.as_str() {
        "windows" => GuestOs::Windows,
        _ => match m.guest_os() {
            GuestOs::Windows => GuestOs::Linux, // agent says non-windows: believe it
            other => other,
        },
    };

    // Guest binary: re-push when the host binary changed, or when a probe
    // says the guest lost it (snapshot restores roll the disk back under a
    // warm cache).
    let bin_dir = default_bin_dir(lab.host_cfg.config_weave_bin_dir.as_deref());
    let host_bin = weave_binary(&bin_dir, os, &arch)?;
    let host_sha = crate::template::store::sha256_file(&host_bin)?;
    let guest_bin = guest_binary_path(os);
    let cached = lab.playbook_ops.pushed_sha(machine);
    let mut need_push = cached.as_deref() != Some(host_sha.as_str());
    if !need_push {
        let probe = agent
            .exec(
                vec![guest_bin.clone(), "version".to_string()],
                Vec::new(),
                None,
                None,
                Duration::from_secs(30),
            )
            .await;
        need_push = !matches!(probe, Ok(ref o) if o.exit_code == 0);
    }
    if need_push {
        log_line(&format!("pushing config-weave to {machine}"));
        retry_push("config-weave binary push", &log_line, || {
            let agent = agent.clone();
            let host_bin = host_bin.clone();
            let guest_bin = guest_bin.clone();
            async move {
                agent.push_file(&host_bin, &guest_bin, Some(0o755)).await?;
                let smoke = agent
                    .exec(
                        vec![guest_bin, "version".to_string()],
                        Vec::new(),
                        None,
                        None,
                        Duration::from_secs(30),
                    )
                    .await?;
                if smoke.exit_code != 0 {
                    bail!(
                        "version probe exited {}: {}",
                        smoke.exit_code,
                        String::from_utf8_lossy(&smoke.stderr)
                    );
                }
                Ok(())
            }
        })
        .await
        .with_context(|| format!("pushing config-weave into {machine}"))?;
        lab.playbook_ops.set_pushed(machine, &host_sha);
    }

    // Playbook folder: pre-clean then push fresh every run, so deleted
    // source files never linger in the guest.
    let guest_dir = format!(
        "{}/playbooks/{}",
        guest_base(os),
        sanitize_guest_dir(&pb.path)
    );
    let rm: Vec<String> = match os {
        GuestOs::Linux => vec!["rm".into(), "-rf".into(), guest_dir.clone()],
        GuestOs::Windows => vec![
            "cmd".into(),
            "/c".into(),
            "rmdir".into(),
            "/s".into(),
            "/q".into(),
            guest_dir.replace('/', "\\"),
        ],
    };
    let src = lab.root.join(&pb.path);
    // The pre-clean rides inside the retry so a locked file that survived
    // one rmdir gets another sweep before the next push attempt.
    let (files, bytes) = retry_push("playbook folder push", &log_line, || {
        let agent = agent.clone();
        let rm = rm.clone();
        let src = src.clone();
        let guest_dir = guest_dir.clone();
        async move {
            let _ = agent
                .exec(rm, Vec::new(), None, None, Duration::from_secs(60))
                .await; // absent dir is fine
            agent.push_tree(&src, &guest_dir).await
        }
    })
    .await
    .with_context(|| format!("pushing playbook {} into {machine}", pb.path.display()))?;
    log_line(&format!(
        "pushed {} ({files} files, {bytes} bytes)",
        pb.path.display()
    ));

    // Run, rebooting and re-running while apply says "reboot required".
    let argv = run_argv(&guest_bin, &guest_dir, pb, mode);
    if !pb.vars.is_empty() {
        log_line(&format!(
            "variables: {}",
            pb.vars
                .iter()
                .map(|v| format!("{}={}", v.name, v.value))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    let mut reboots = 0u32;
    loop {
        let agent = m.wait_agent(Duration::from_secs(300)).await?;
        let on_line = |line: String| match serde_json::from_str::<Value>(&line) {
            Ok(cw) => {
                if let Some(human) = render_cw_event(&cw) {
                    output(format!("{human}\n"));
                    lab.playbook_ops.append_log(machine, &human);
                }
                let mut payload = base.clone();
                payload["cw"] = cw;
                lab.events.emit("playbook.op.step", payload);
            }
            Err(_) => log_line(&line),
        };
        let (exit_code, stdout) = exec_streaming(&agent, argv.clone(), on_line).await?;
        let report = parse_report(&stdout);

        if exit_code == 3 && mode == PlaybookMode::Apply {
            {
                if !m.can_reboot() {
                    bail!(
                        "playbook {} play {} on {machine}: reboot required, but this \
                         machine cannot reboot in place (a container micro-VM restarts \
                         from a fresh rootfs)",
                        pb.path.display(),
                        pb.play
                    );
                }
                {
                    if reboots >= MAX_REBOOTS {
                        log_line(&format!(
                            "still reboot-required after {MAX_REBOOTS} reboots — giving up"
                        ));
                        return Ok(PlaybookOutcome {
                            exit_code,
                            reboots,
                            report,
                        });
                    }
                    reboots += 1;
                    log_line(&format!(
                        "reboot required — restarting {machine} (attempt {reboots}/{MAX_REBOOTS})"
                    ));
                    set_phase("rebooting", reboots);
                    m.reboot_guest().await?;
                    // Wait for the agent to actually drop before waiting for
                    // it to come back; some guests keep answering briefly.
                    let grace = tokio::time::Instant::now() + Duration::from_secs(120);
                    while m.agent_answering().await && tokio::time::Instant::now() < grace {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    // Narrated come-back wait: a DC's first post-promotion
                    // boot can be quiet for many minutes, which reads as a
                    // hang without periodic output.
                    let started = tokio::time::Instant::now();
                    let deadline = started + Duration::from_secs(600);
                    let mut next_note = started + Duration::from_secs(30);
                    loop {
                        if m.agent_answering().await {
                            break;
                        }
                        if m.state().await == crate::labd::vm::PowerState::Stopped {
                            bail!("{machine} stopped while rebooting for the playbook");
                        }
                        let now = tokio::time::Instant::now();
                        if now >= deadline {
                            bail!("{machine}: guest did not come back within 600s of the reboot");
                        }
                        if now >= next_note {
                            log_line(&format!(
                                "waiting for {machine} to come back ({}s)… — Windows can \
                                 take a while here",
                                started.elapsed().as_secs()
                            ));
                            next_note = now + Duration::from_secs(30);
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    log_line(&format!(
                        "{machine} is back after {}s — re-running {}",
                        started.elapsed().as_secs(),
                        mode.verb()
                    ));
                    set_phase("running", reboots);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }
        }
        return Ok(PlaybookOutcome {
            exit_code,
            reboots,
            report,
        });
    }
}

/// The guest command line for one run. Each declared variable becomes a
/// `--var name=value` in declaration order; the value is passed through
/// verbatim (no shell is involved), so config-weave applies its usual rule —
/// parse it as a WCL expression where it can, string otherwise.
fn run_argv(guest_bin: &str, guest_dir: &str, pb: &Playbook, mode: PlaybookMode) -> Vec<String> {
    let mut argv = vec![
        guest_bin.to_string(),
        mode.verb().to_string(),
        guest_dir.to_string(),
        pb.play.clone(),
        "--json".to_string(),
        "--events-ndjson".to_string(),
    ];
    for var in &pb.vars {
        argv.push("--var".to_string());
        argv.push(format!("{}={}", var.name, var.value));
    }
    argv
}

/// Streaming exec: stderr lines go to `on_line` as they arrive (the ndjson
/// progress feed), stdout accumulates (the final `--json` report). Returns
/// `(exit_code, stdout)`.
async fn exec_streaming(
    agent: &AgentHandle,
    argv: Vec<String>,
    mut on_line: impl FnMut(String),
) -> Result<(i32, String)> {
    let display = argv.join(" ");
    let mut session = agent.open_exec(argv, Vec::new(), None).await?;
    session.eof().await?;
    let mut stdout = Vec::new();
    let mut carry = String::new();
    let deadline = tokio::time::Instant::now() + RUN_TIMEOUT;
    loop {
        let ev = tokio::time::timeout_at(deadline, session.recv())
            .await
            .map_err(|_| anyhow!("`{display}` timed out after {RUN_TIMEOUT:?}"))?;
        match ev {
            Some(SessionEvent::Data(b)) => stdout.extend(b),
            Some(SessionEvent::Stderr(b)) => {
                for line in split_ndjson_lines(&mut carry, &b) {
                    on_line(line);
                }
            }
            Some(SessionEvent::Exited(code)) => {
                let tail = carry.trim();
                if !tail.is_empty() {
                    on_line(tail.to_string());
                }
                return Ok((code, String::from_utf8_lossy(&stdout).into_owned()));
            }
            Some(SessionEvent::Error(msg)) => bail!("`{display}`: {msg}"),
            Some(SessionEvent::FileDone { .. }) => {}
            None => bail!("agent channel closed during `{display}`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second run on a busy machine is a conflict, and stays one all the
    /// way to the surface: the code has to survive the `anyhow` that
    /// `run_playbook` returns, or the console shows a bad gateway.
    #[test]
    fn a_second_run_on_one_machine_is_a_conflict() {
        use crate::proto::ErrorCode;

        let ops = PlaybookOps::default();
        let _claim = ops
            .try_begin("dc01", "playbooks/web", "main", "apply")
            .unwrap();

        let Err(err) = ops.try_begin("dc01", "playbooks/web", "main", "check") else {
            panic!("a second claim on one machine should be rejected");
        };
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(err.message.contains("already running"), "{err}");

        // The trip `run_playbook` puts it through.
        let surfaced: CommandError = anyhow::Error::new(err).into();
        assert_eq!(surfaced.code, ErrorCode::Conflict);

        // Another machine is unaffected.
        ops.try_begin("dc02", "playbooks/web", "main", "check")
            .unwrap();
    }

    #[test]
    fn bin_dir_precedence() {
        let cfg = PathBuf::from("/from/config");
        assert_eq!(
            resolve_bin_dir(Some(&cfg), Some("/from/env")),
            PathBuf::from("/from/config")
        );
        assert_eq!(
            resolve_bin_dir(None, Some("/from/env")),
            PathBuf::from("/from/env")
        );
        assert_eq!(
            resolve_bin_dir(None, Some("")),
            crate::paths::config_weave_bin_dir()
        );
        assert_eq!(
            resolve_bin_dir(None, None),
            crate::paths::config_weave_bin_dir()
        );
    }

    #[test]
    fn weave_binary_naming_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config-weave-linux-x86_64"), "").unwrap();
        std::fs::write(dir.path().join("config-weave-windows-x86_64.exe"), "").unwrap();

        let linux = weave_binary(dir.path(), GuestOs::Linux, "x86_64").unwrap();
        assert!(linux.ends_with("config-weave-linux-x86_64"));
        let win = weave_binary(dir.path(), GuestOs::Windows, "x86_64").unwrap();
        assert!(win.ends_with("config-weave-windows-x86_64.exe"));

        let err = weave_binary(dir.path(), GuestOs::Linux, "aarch64").unwrap_err();
        assert!(err.to_string().contains("only for x86_64"), "{err}");

        let empty = tempfile::tempdir().unwrap();
        let err = weave_binary(empty.path(), GuestOs::Linux, "x86_64").unwrap_err();
        assert!(err.to_string().contains("just install"), "{err}");
    }

    #[test]
    fn guest_os_heuristic() {
        assert_eq!(guest_os_of(Some("windows-server")), GuestOs::Windows);
        assert_eq!(guest_os_of(Some("windows-legacy")), GuestOs::Windows);
        assert_eq!(guest_os_of(Some("linux-modern")), GuestOs::Linux);
        assert_eq!(guest_os_of(None), GuestOs::Linux);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_push_retries_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = AtomicU32::new(0);
        let logged = std::sync::Mutex::new(Vec::<String>::new());
        let log = |line: &str| logged.lock().unwrap().push(line.to_string());
        let result = retry_push("test op", &log, || async {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 3 {
                bail!("file in use")
            }
            Ok(n)
        })
        .await
        .unwrap();
        assert_eq!(result, 3);
        {
            let lines = logged.lock().unwrap();
            assert_eq!(lines.len(), 2, "{lines:#?}");
            assert!(
                lines[0].contains("test op failed") && lines[0].contains("1/4"),
                "{lines:#?}"
            );
        }

        // Exhausted attempts surface the final error.
        let log2 = |_: &str| {};
        let err = retry_push("test op", &log2, || async {
            Err::<(), _>(anyhow!("still locked"))
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("still locked"), "{err}");
    }

    #[test]
    fn sanitize_flattens_paths() {
        assert_eq!(
            sanitize_guest_dir(Path::new("playbooks/baseline")),
            "playbooks__baseline"
        );
        assert_eq!(sanitize_guest_dir(Path::new("pb")), "pb");
        assert_eq!(
            sanitize_guest_dir(Path::new("./a/../b")),
            "a__b" // lexical only; the dot components drop out
        );
        assert_eq!(sanitize_guest_dir(Path::new("")), "playbook");
    }

    #[test]
    fn ndjson_lines_carry_partials_across_chunks() {
        let mut carry = String::new();
        let lines = split_ndjson_lines(&mut carry, b"{\"a\":1}\n{\"b\"");
        assert_eq!(lines, vec!["{\"a\":1}"]);
        let lines = split_ndjson_lines(&mut carry, b":2}\r\n\n{\"c\":3}\n");
        assert_eq!(lines, vec!["{\"b\":2}", "{\"c\":3}"]);
        assert!(carry.is_empty());
    }

    #[test]
    fn renders_cw_events() {
        let ev = json!({"event": "run_started", "play": "base", "mode": "apply",
                        "steps": [{"name": "a"}, {"name": "b"}]});
        assert_eq!(
            render_cw_event(&ev).unwrap(),
            "play base (apply): 2 step(s)"
        );

        let ev = json!({"event": "step_finished", "name": "hostname", "status": "configured",
                        "duration_secs": 0.25, "message": null});
        assert_eq!(
            render_cw_event(&ev).unwrap(),
            "  [configured] hostname (0.2s)"
        );

        let ev = json!({"event": "step_finished", "name": "pkg", "status": "error",
                        "duration_secs": 1.0, "message": "boom"});
        assert_eq!(render_cw_event(&ev).unwrap(), "  [error] pkg (1.0s) — boom");

        // Churny events stay quiet.
        let ev = json!({"event": "step_phase", "idx": 0, "name": "x", "phase": "checking"});
        assert_eq!(render_cw_event(&ev), None);
        let ev = json!({"event": "step_started", "idx": 0, "name": "x"});
        assert_eq!(render_cw_event(&ev), None);
    }

    #[test]
    fn report_parses_with_stray_stdout() {
        let clean = r#"{"playbook":"p","exit_code":0,"steps":[]}"#;
        assert!(parse_report(clean).is_some());
        let noisy = format!("resource echoed this\n{clean}");
        // Leading noise before the outermost object still parses.
        assert_eq!(parse_report(&noisy).unwrap()["playbook"], "p");
        assert!(parse_report("no json here").is_none());
        assert!(parse_report("").is_none());
    }

    fn lab(src: &str) -> Lab {
        crate::config::load_lab_source(src, "<test>", Path::new("/tmp"))
            .expect("parse")
            .lab
    }

    #[test]
    fn resolve_playbook_scoping_and_ambiguity() {
        let lab = lab(r#"import <vmlab.wcl>
lab "l" {
  vm "web01" {
    template = "x86_64/t"
    playbook "pb/a" { play = "base" }
    playbook "pb/b" { play = "base" }
  }
  vm "db01" {
    template = "x86_64/t"
    playbook "pb/b" { play = "base" }
  }
  container "cache" {
    image = "redis"
    playbook "pb/c" { play = "base" }
  }
}"#);
        // One block, no filter needed.
        let p = resolve_playbook(&lab, "db01", None, None).unwrap();
        assert_eq!(p.path.display().to_string(), "pb/b");
        // Containers declare theirs the same way.
        let p = resolve_playbook(&lab, "cache", None, None).unwrap();
        assert_eq!(p.path.display().to_string(), "pb/c");
        // web01 declares two → ambiguous without a filter…
        let err = resolve_playbook(&lab, "web01", None, None).unwrap_err();
        assert!(err.contains("declares 2 playbooks"), "{err}");
        // …and unambiguous with one.
        let p = resolve_playbook(&lab, "web01", Some("pb/a"), None).unwrap();
        assert_eq!(p.path.display().to_string(), "pb/a");
        // A machine with no blocks, and a machine that doesn't exist.
        let err = resolve_playbook(&lab, "ghost", None, None).unwrap_err();
        assert!(err.contains("no vm or container"), "{err}");
    }

    #[test]
    fn resolve_playbook_needs_a_declared_block() {
        let lab = lab(r#"import <vmlab.wcl>
lab "l" { vm "web01" { template = "x86_64/t" } }"#);
        let err = resolve_playbook(&lab, "web01", None, None).unwrap_err();
        assert!(err.contains("declares no matching playbook"), "{err}");
    }

    /// Variables become repeated `--var name=value` in declaration order,
    /// after the flags — passed verbatim, since there is no shell in between.
    #[test]
    fn argv_carries_vars_in_declaration_order() {
        let lab = lab(r#"import <vmlab.wcl>
lab "l" {
  vm "srv01" {
    template = "x86_64/t"
    playbook "pb/ad" {
      play = "member-server"
      var "domain"   { value = "corp.example.com" }
      var "new_name" { value = "SRV01" }
      var "retries"  { value = "3" }
    }
  }
}"#);
        let pb = resolve_playbook(&lab, "srv01", None, None).unwrap();
        let argv = run_argv(
            "/weave/config-weave",
            "/weave/playbooks/pb__ad",
            pb,
            PlaybookMode::Apply,
        );
        assert_eq!(
            argv,
            vec![
                "/weave/config-weave",
                "apply",
                "/weave/playbooks/pb__ad",
                "member-server",
                "--json",
                "--events-ndjson",
                "--var",
                "domain=corp.example.com",
                "--var",
                "new_name=SRV01",
                "--var",
                "retries=3",
            ]
        );
    }

    #[test]
    fn argv_without_vars_is_unchanged() {
        let lab = lab(r#"import <vmlab.wcl>
lab "l" {
  vm "srv01" { template = "x86_64/t" playbook "pb/ad" { play = "base" } }
}"#);
        let pb = resolve_playbook(&lab, "srv01", None, None).unwrap();
        let argv = run_argv("cw", "/d", pb, PlaybookMode::Check);
        assert_eq!(
            argv,
            vec!["cw", "check", "/d", "base", "--json", "--events-ndjson"]
        );
    }
}
