//! `vmlab dev` (PRD §19.7) — the two verbs that earn the `dev` noun because
//! they are meaningless for a machine that is not `@dev`.
//!
//! **`dev attach` is cold-to-editing in one command**: it ups the machine,
//! waits until it is [`attachable`](crate::attach), prints the alias and the
//! editor snippet, and **becomes a shell on it**. It launches no editor and
//! knows none — the developer opens their own and picks the alias out of the
//! picker, which the managed block guarantees is there. A host-config `editor`
//! command template mirroring the existing `viewer` key was real prior art and
//! was rejected on coupling.
//!
//! **Because attach becomes a shell, nothing it starts may be its own.**
//! Closing the shell cannot stop the workspace syncer while the editor is
//! still attached, so the syncer is started by the lab daemon inside `up`
//! (§19.6) and this process starts nothing it owns — it ends by `exec`ing
//! `ssh`, at which point there is no vmlab process left to own anything.
//!
//! **`dev use` records which dev machine is *mine*** in the lab's own
//! gitignored `.vmlab/`, which `vmlab.wcl` structurally cannot say because it
//! is committed and shared. The ladder both verbs resolve through lives in
//! [`crate::dev::select`].

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::daemon::{self, remote};
use super::lab::{effective_profile_name, exec_ssh, guest_family, lab_status, load_lab_here, rt};
use crate::config::LabFile;
use crate::dev::ResolvedDev;
use crate::dev::select::{self, Selected};
use crate::labd::machine::Capabilities;
use crate::labd::workspace::diff::{Diff, SideCopy};
use crate::labd::workspace::plan::Winner;
use crate::proto::LabRequest;
use crate::proto::client::LabClient;
use crate::status::{MachineStatus, PowerState, WorkspaceSyncStatus};

/// How long `dev attach` waits for a machine to become attachable **after
/// `up` has already returned**.
///
/// `up` is what waits for a guest to boot and provision, streaming its own
/// progress; by the time it returns the machine is ready and the only thing
/// still outstanding is the agent handshake. The bound exists so a machine
/// whose agent will never answer says so instead of holding a terminal
/// forever, and it is set well past a cold domain-joined Windows guest's
/// handshake rather than at what a healthy one takes — being wrong here costs
/// a developer their attach.
const ATTACHABLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the wait asks. Frequent enough to feel immediate, rare enough
/// that a status projection per second is nothing.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// `vmlab dev use <machine>` — record which dev machine is mine (§19.7).
///
/// It records rather than attaches, so it needs no daemon and starts nothing:
/// the file lives inside the lab it describes.
pub fn cmd_dev_use(machine: &str) -> Result<()> {
    // A selection recorded against a lab that does not validate is a promise
    // vmlab cannot keep — and "two machines declare `default = true`" is
    // exactly the kind of thing to hear when picking one (§5.1).
    super::validate::validate_current()?;
    let (file, root) = load_lab_here()?;
    // The block is not load-bearing for this verb, so its failure warns like
    // it does for every other command that merely loads a lab (§19.7).
    crate::ssh_config::refresh_or_warn(&file.lab, &root);

    let devs = dev_machines(&file);
    // Through the same ladder, at its top rung: `dev use` names a machine, so
    // what it accepts and what `dev attach <machine>` accepts are one answer.
    let selected = select::resolve(&file.lab.name, &devs, Some(machine), None, None)?;
    let path = select::record_selection(&crate::paths::lab_local_dir(&root), selected.machine())?;
    println!(
        "lab \"{}\": \"{}\" is your dev machine — recorded in {}",
        file.lab.name,
        selected.machine(),
        path.display(),
    );
    println!("`vmlab destroy` forgets it; `vmlab dev use <machine>` changes it.");
    Ok(())
}

/// `vmlab dev attach [machine]` — up, wait, and become a shell (§19.7).
pub fn cmd_dev_attach(machine: Option<String>) -> Result<()> {
    // Before any side effect, like every verb that acts (§5.1).
    super::validate::validate_current()?;
    let (file, root) = load_lab_here()?;
    let lab = file.lab.name.clone();

    let devs = dev_machines(&file);
    let lab_local = crate::paths::lab_local_dir(&root);
    let selected = select::resolve(
        &lab,
        &devs,
        machine.as_deref(),
        select::env_selection().as_deref(),
        select::read_selection(&lab_local).as_deref(),
    )?;
    let machine = selected.machine();

    // Here the alias is load-bearing, so the managed block's failure is this
    // command's failure rather than the warning the ambient refresh prints
    // (§19.7's ladder) — and it fails *before* a machine is booted for an
    // attach that could not have worked.
    let (managed, block, _) = crate::ssh_config::refresh_lab(&file.lab, &root)
        .context("the managed SSH block must be current before `dev attach` can use it")?;
    let alias = crate::ssh_config::Alias {
        machine: machine.to_string(),
        login: None,
    };
    // **This** alias, whether or not anything was written. A write verifies
    // the block's *first* alias, which in a lab of several machines is
    // somebody else's; and an unchanged block that has been losing to a
    // `Host *` all along would otherwise never say so. `ssh -G` is cheap and
    // this is the one command that must not attach to a stranger's
    // `ProxyCommand` (§19.7).
    managed.verify(&lab, Some(&alias))?;

    print!("{}", opening(&lab, &selected));
    rt()?.block_on(up_and_wait(&lab, &root, machine))?;

    let windows = guest_family(&file.lab, machine)? == crate::labd::guest_os::GuestOs::Windows;
    print!(
        "{}",
        attach_report(
            machine,
            &block.aliases_for(machine),
            selected.dev.workspace.as_deref(),
            windows,
        )
    );

    // Becoming `ssh` is what makes the shell the developer's own — and what
    // guarantees no vmlab process survives this command to own anything the
    // editor still depends on.
    exec_ssh(&alias.name(&lab), &[])
}

// ---------------------------------------------------------------------------
// `vmlab dev sync` (§19.6)
// ---------------------------------------------------------------------------

/// The dev machine these verbs are about, through the same ladder `dev attach`
/// resolves — argument, `$VMLAB_DEV_MACHINE`, the `dev use` selection, the
/// lab's default `@dev` machine — so `dev sync status` and `dev attach` are
/// never talking about different machines.
fn sync_target(machine: Option<String>) -> Result<(String, String)> {
    let (file, root) = load_lab_here()?;
    let lab = file.lab.name.clone();
    let devs = dev_machines(&file);
    let lab_local = crate::paths::lab_local_dir(&root);
    let selected = select::resolve(
        &lab,
        &devs,
        machine.as_deref(),
        select::env_selection().as_deref(),
        select::read_selection(&lab_local).as_deref(),
    )?;
    Ok((lab, selected.machine().to_string()))
}

/// A lab daemon that is **already running**, or the reason there is nothing to
/// ask.
///
/// Deliberately not `ensure`: a syncer exists only while its machine is up, so
/// starting a daemon to be told "no syncer" would boot a lab in order to
/// deliver a negative answer.
async fn running_lab(lab: &str) -> Result<LabClient> {
    daemon::try_lab_daemon(lab).await.ok_or_else(|| {
        anyhow!(
            "lab \"{lab}\" is not running, so no workspace is syncing — `vmlab up` starts it, and \
             `vmlab dev attach` starts just the dev machine"
        )
    })
}

/// `vmlab dev sync status` — what the syncer last decided (§19.6).
///
/// Read off the lab status projection rather than through a verb of its own:
/// the console shows the same halt from the same value, so there is one answer
/// to keep correct rather than two (ADR-0004).
pub fn cmd_dev_sync_status(machine: Option<String>) -> Result<()> {
    let (lab, machine) = sync_target(machine)?;
    let sync = rt()?.block_on(async {
        let client = running_lab(&lab).await?;
        sync_status(&client, &machine).await
    })?;
    print!("{}", sync_report(&machine, &sync));
    Ok(())
}

/// `vmlab dev sync flush` — run a pass now and wait for it.
pub fn cmd_dev_sync_flush(machine: Option<String>) -> Result<()> {
    let (lab, machine) = sync_target(machine)?;
    let sync: WorkspaceSyncStatus = rt()?.block_on(async {
        let client = running_lab(&lab).await?;
        let payload = client
            .send(LabRequest::WorkspaceFlush {
                machine: machine.clone(),
            })
            .await
            .map_err(remote)?;
        serde_json::from_value(payload)
            .context("the lab daemon reported a workspace status vmlab cannot read")
    })?;
    print!("{}", sync_report(&machine, &sync));
    Ok(())
}

/// `vmlab dev sync diff` — the guest's copy, host-side (§19.6).
pub fn cmd_dev_sync_diff(machine: Option<String>, paths: Vec<String>) -> Result<()> {
    let (lab, machine) = sync_target(machine)?;
    let payload = rt()?.block_on(async {
        let client = running_lab(&lab).await?;
        client
            .send(LabRequest::WorkspaceDiff {
                machine: machine.clone(),
                paths,
            })
            .await
            .map_err(remote)
    })?;
    // The producer's own type, so a field renamed in `workspace::diff` stops
    // this compiling rather than quietly becoming "no guest copy" (ADR-0004).
    let diff: Diff = serde_json::from_value(payload)
        .context("the lab daemon reported a diff vmlab cannot read")?;
    print!("{}", render_diff(&diff));
    Ok(())
}

/// `vmlab dev sync resolve` — pick a side, and carry it out (§19.6).
pub fn cmd_dev_sync_resolve(
    machine: Option<String>,
    paths: Vec<String>,
    host: bool,
    guest: bool,
    all: bool,
) -> Result<()> {
    // No default. Which copy survives is the one thing this verb decides, and
    // a default would make the answer depend on which flag someone forgot.
    let winner = match (host, guest) {
        (true, false) => Winner::Host,
        (false, true) => Winner::Guest,
        _ => bail!(
            "say which side wins: `--host` keeps the canonical copy and overwrites the guest's, \
             `--guest` keeps the guest's working copy and overwrites the canonical one. \
             `vmlab dev sync diff` shows both, and making them identical by hand needs neither flag."
        ),
    };
    if paths.is_empty() && !all {
        bail!("name the paths to resolve, or pass `--all` to take the whole batch");
    }
    let (lab, machine) = sync_target(machine)?;
    let sync: WorkspaceSyncStatus = rt()?.block_on(async {
        let client = running_lab(&lab).await?;
        let payload = client
            .send(LabRequest::WorkspaceResolve {
                machine: machine.clone(),
                paths,
                all,
                winner: winner.as_str().to_string(),
            })
            .await
            .map_err(remote)?;
        serde_json::from_value(payload)
            .context("the lab daemon reported a workspace status vmlab cannot read")
    })?;
    print!("{}", sync_report(&machine, &sync));
    Ok(())
}

/// One machine's syncer, off the projection every surface reads.
async fn sync_status(client: &LabClient, machine: &str) -> Result<WorkspaceSyncStatus> {
    let status = lab_status(client).await?;
    let found = status
        .machines
        .iter()
        .find(|m| m.name == machine)
        .ok_or_else(|| anyhow!("the lab daemon does not know machine \"{machine}\""))?;
    found
        .dev
        .as_ref()
        .and_then(|dev| dev.sync.clone())
        .ok_or_else(|| {
            anyhow!(
                "\"{machine}\" has no workspace syncer running: it is not up, or it declares no \
                 `@dev(workspace = …)`"
            )
        })
}

/// What one machine's syncer says, in the order it matters.
///
/// The halt first and whole, because a stopped workspace is the only state
/// here a developer has to act on — and then everything the syncer *declined*
/// to do, each by name. A skipped `.sock`, a refused 4 GB file and a deferred
/// `.git/index` are all normal, and every one of them is the kind of thing
/// that silently stops a tree syncing if nobody says it out loud.
fn sync_report(machine: &str, sync: &WorkspaceSyncStatus) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    match &sync.halt {
        Some(halt) => {
            let _ = writeln!(out, "{halt}");
            let _ = writeln!(out);
            for conflict in &sync.conflicts {
                let _ = writeln!(out, "  {}\n      {}", conflict.path, conflict.reason);
            }
            if sync.conflicts_total > sync.conflicts.len() {
                let _ = writeln!(
                    out,
                    "  … and {} more (not listed; `--all` needs no list)",
                    sync.conflicts_total - sync.conflicts.len(),
                );
            }
            if let Some(resolve) = &sync.resolve {
                let _ = writeln!(out, "\n{resolve}");
            }
        }
        None if sync.passes == 0 => {
            let _ = writeln!(
                out,
                "\"{machine}\"'s workspace has not completed a pass yet — it is still starting, or \
                 the guest is not answering."
            );
        }
        None => {
            let _ = writeln!(
                out,
                "\"{machine}\"'s workspace is in step ({} passes).",
                sync.passes
            );
        }
    }
    for (label, said) in [
        ("waiting", sync.rescan.as_ref()),
        ("volume", sync.volume.as_ref()),
        ("trouble", sync.trouble.as_ref()),
    ] {
        if let Some(said) = said {
            let _ = writeln!(out, "\n{label}: {said}");
        }
    }
    if sync.rescans > 0 && sync.rescan.is_none() {
        let _ = writeln!(
            out,
            "\nwatch discontinuities answered with a full walk: {} (repeated ones mean the guest \
             is writing faster than its watch can report)",
            sync.rescans,
        );
    }
    if !sync.deferred.is_empty() {
        let _ = writeln!(
            out,
            "\ndeferred while git holds a lock — timing, not a conflict, and it clears itself:"
        );
        for path in &sync.deferred {
            let _ = writeln!(out, "  {path}");
        }
    }
    if !sync.skipped.is_empty() {
        let _ = writeln!(out, "\nnot synced, by name:");
        for skip in &sync.skipped {
            let _ = writeln!(out, "  {}\n      {}", skip.path, skip.reason);
        }
    }
    out
}

/// Both copies, side by side, as text.
///
/// A unified diff where both sides are readable text and a description
/// otherwise — because the question this verb answers is *what does the guest
/// hold*, and for a 4 GB file or a binary the honest answer is its size and its
/// digest rather than its bytes.
fn render_diff(diff: &Diff) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "host {}  ⟷  \"{}\":{}",
        diff.host_root, diff.machine, diff.guest_root
    );
    for file in &diff.files {
        let _ = writeln!(out, "\n=== {}", file.path);
        if file.identical {
            let _ = writeln!(
                out,
                "  the two copies are identical — the next pass adopts them as agreed, and the \
                 halt clears with no verb at all"
            );
            continue;
        }
        match (&file.host, &file.guest) {
            (None, None) => {
                let _ = writeln!(out, "  neither side holds it");
                continue;
            }
            (None, Some(_)) => {
                let _ = writeln!(out, "  the host does not hold it; the guest does");
            }
            (Some(_), None) => {
                let _ = writeln!(out, "  the guest does not hold it; the host does");
            }
            (Some(_), Some(_)) => {}
        }
        let text = |copy: &Option<SideCopy>| -> Option<String> {
            copy.as_ref().and_then(|copy| copy.text.clone())
        };
        match (text(&file.host), text(&file.guest)) {
            (Some(host), Some(guest)) => {
                let _ = write!(out, "{}", unified(&host, &guest));
            }
            _ => {
                for (side, copy) in [("host", &file.host), ("guest", &file.guest)] {
                    let Some(copy) = copy else { continue };
                    let _ = write!(out, "  {side}: {} bytes", copy.size);
                    if !copy.digest.is_empty() {
                        let _ = write!(
                            out,
                            ", sha256 {}",
                            &copy.digest[..16.min(copy.digest.len())]
                        );
                    }
                    match &copy.omitted {
                        Some(why) => {
                            let _ = writeln!(out, " — not shown: {why}");
                        }
                        None => {
                            let _ = writeln!(out);
                        }
                    }
                }
            }
        }
    }
    out
}

/// The most lines either side may have before the diff gives up on being one.
///
/// The comparison below is quadratic in **memory** as well as time — one `u32`
/// per pair of lines — so the cap is what bounds a `dev sync diff` on a
/// generated file to a few megabytes rather than a few hundred. Past it both
/// copies are still on this host and the developer's own tool is better at it
/// anyway.
const DIFF_LINES: usize = 2_000;

/// A unified diff of two texts, `-` host and `+` guest.
///
/// Written here rather than taken from a crate because it is thirty lines and
/// the alternative is a dependency for one command's output. It is a longest-
/// common-subsequence diff, which is what `diff -u` produces for files this
/// size; the point is legibility, not minimality.
fn unified(host: &str, guest: &str) -> String {
    use std::fmt::Write as _;

    let (a, b): (Vec<&str>, Vec<&str>) = (host.lines().collect(), guest.lines().collect());
    if a.len() > DIFF_LINES || b.len() > DIFF_LINES {
        return format!(
            "  too long to diff here ({} host lines, {} guest lines) — both copies are on this \
             host; compare them with your own tool\n",
            a.len(),
            b.len(),
        );
    }
    // lcs[i][j] = the longest common subsequence of a[i..] and b[j..]. `u32`
    // rather than `usize` because this table is the whole memory cost, and
    // DIFF_LINES caps either side well inside it.
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            let _ = writeln!(out, "   {}", a[i]);
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            let _ = writeln!(out, "  -{}", a[i]);
            i += 1;
        } else {
            let _ = writeln!(out, "  +{}", b[j]);
            j += 1;
        }
    }
    for line in &a[i..] {
        let _ = writeln!(out, "  -{line}");
    }
    for line in &b[j..] {
        let _ = writeln!(out, "  +{line}");
    }
    out
}

/// The lab's dev machines, resolved against each machine's **effective**
/// profile (§19.1) — the same profile the editor snippet's guest family comes
/// from, so the two cannot disagree.
fn dev_machines(file: &LabFile) -> Vec<ResolvedDev> {
    let profiles = crate::profiles::ProfileSet::shipped().ok();
    crate::dev::machines(&file.lab, |m| {
        let name = effective_profile_name(m)?;
        profiles.as_ref()?.get(&name).cloned()
    })
}

/// Up the machine and wait until its agent can serve an attach.
///
/// `up` is asked for **this machine**, not the lab: attaching to a dev machine
/// is not a reason to boot the four other guests a lab happens to declare.
async fn up_and_wait(lab: &str, root: &std::path::Path, machine: &str) -> Result<()> {
    let client = daemon::ensure_lab_daemon(lab, root).await?;
    client
        .send_streaming(
            LabRequest::Up {
                machines: vec![machine.to_string()],
            },
            |chunk| print!("{chunk}"),
        )
        .await
        .map_err(remote)?;
    wait_attachable(&client, machine).await
}

/// What one look at the machine says about attaching to it.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Its agent serves an attach; go.
    Attachable,
    /// Not yet, for a reason worth showing while it is being waited on.
    Waiting(String),
    /// It never will as it stands — §19.4's hard rung, with both remedies.
    Refused(String),
}

/// Read one poll. Pure, because *which* of the three answers a machine's state
/// earns is the decision worth pinning, and none of it needs a daemon.
///
/// `features` is what the machine's agent advertises, asked for exactly when
/// [`asks_the_agent`] says so — before then, silence is a machine still coming
/// up rather than an agent that cannot answer, and guessing from silence is
/// the inference §19.4 exists to avoid. An empty list is an agent that has not
/// answered; a non-empty one that lacks an attach feature is the refusal.
fn verdict(machine: &str, m: &MachineStatus, features: Option<&[String]>) -> Verdict {
    if m.attachable {
        return Verdict::Attachable;
    }
    if m.state != PowerState::Running {
        return Verdict::Waiting(format!(
            "waiting for \"{machine}\" to run — it is {}",
            m.label.text
        ));
    }
    if !m.ready {
        return Verdict::Waiting(format!("waiting for \"{machine}\" to become ready"));
    }
    match features {
        Some(features) if !features.is_empty() => Verdict::Refused(crate::attach::refusal(
            Some(machine),
            "an attach",
            &crate::attach::missing(features),
        )),
        _ => Verdict::Waiting(format!("waiting for \"{machine}\"'s agent to answer")),
    }
}

/// Whether this poll needs the agent's feature list — the one rung of
/// [`verdict`] that cannot be answered off the status projection alone.
///
/// Stated here beside the verdict rather than in the loop, so the rule about
/// when silence is evidence lives in one place: a machine that is running and
/// ready and still not attachable is the only state whose meaning depends on
/// what its agent said.
fn asks_the_agent(m: &MachineStatus) -> bool {
    !m.attachable && m.state == PowerState::Running && m.ready
}

/// Wait until the machine is attachable, saying what is being waited for.
///
/// The wait is **visible**: every distinct reason is printed once, so a
/// developer watching a cold Windows guest sees which of the stages it is in
/// rather than a cursor. Repeating the same line every second would be noise,
/// not progress.
async fn wait_attachable(client: &LabClient, machine: &str) -> Result<()> {
    let started = Instant::now();
    let mut last = String::new();
    loop {
        let status = lab_status(client).await?;
        let found = status
            .machines
            .iter()
            .find(|m| m.name == machine)
            .ok_or_else(|| {
                anyhow!(
                    "the lab daemon does not know machine \"{machine}\" — it predates an edit to \
                     {}; run `vmlab lab restart`",
                    crate::paths::LAB_FILE
                )
            })?;
        let features = match asks_the_agent(found) {
            true => Some(agent_features(client, machine).await?),
            false => None,
        };
        match verdict(machine, found, features.as_deref()) {
            Verdict::Attachable => {
                println!("\"{machine}\" is attachable");
                return Ok(());
            }
            Verdict::Refused(why) => bail!(why),
            Verdict::Waiting(why) => {
                if why != last {
                    println!("{why}");
                    last = why;
                }
                if started.elapsed() >= ATTACHABLE_TIMEOUT {
                    bail!(timed_out(machine, &last));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

/// What the machine's agent advertises right now — the probed evidence
/// `attachable` is derived from, asked for through the verb that already
/// reports it (`machine capabilities`) rather than through a second path.
///
/// Read back as the producer's own [`Capabilities`], so a field renamed there
/// stops compiling here rather than quietly becoming "no features" and a wait
/// that never ends (ADR-0004's lesson).
async fn agent_features(client: &LabClient, machine: &str) -> Result<Vec<String>> {
    let payload = client
        .send(LabRequest::MachineCapabilities {
            machine: machine.to_string(),
        })
        .await
        .map_err(remote)?;
    let caps: Capabilities = serde_json::from_value(payload)
        .context("the lab daemon reported capabilities vmlab cannot read")?;
    Ok(caps.agent)
}

/// The wait gave up. It says what it was waiting for when it did, because
/// "timed out" alone cannot be acted on.
fn timed_out(machine: &str, waiting_for: &str) -> String {
    format!(
        "\"{machine}\" is still not attachable after {}s — {waiting_for}.\nThe machine is up; \
         `vmlab status` and `vmlab machine capabilities {machine}` say what it is doing, and \
         `vmlab ssh {machine}` still works if its agent serves a shell.",
        ATTACHABLE_TIMEOUT.as_secs(),
    )
}

/// The line `dev attach` opens with: which machine, and which rung of the
/// ladder chose it. Printed before the `up` so a developer who expected
/// another machine can interrupt rather than watch the wrong one boot.
fn opening(lab: &str, selected: &Selected) -> String {
    format!(
        "attaching to \"{}\" in lab \"{lab}\" ({})\n",
        selected.machine(),
        selected.source.describe(),
    )
}

/// What lands on screen just before the shell: the alias, every labelled
/// alias beside it, and the editor snippet.
///
/// vmlab launches no editor, so this is the whole handover — the alias is what
/// the developer picks out of their own client's host list, and saying it here
/// means they never have to work out its spelling.
///
/// `workspace` is the host directory the machine declared, relative to the lab
/// root ([`ResolvedDev::workspace`]); `None` is a dev machine with no
/// workspace, which is still perfectly attachable.
fn attach_report(
    machine: &str,
    aliases: &[String],
    workspace: Option<&std::path::Path>,
    windows: bool,
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out);
    for (i, alias) in aliases.iter().enumerate() {
        let label = if i == 0 { "ssh alias" } else { "" };
        let _ = writeln!(out, "  {label:<9}  {alias}");
    }
    let _ = writeln!(
        out,
        "\nPick that host in your editor — vmlab launches none, and the alias is already in \
         your ~/.ssh/config."
    );
    // The one refusal §19.3 says is worth spending vmlab's own words on: the
    // protocol carries no text for it, so a forwarded key fails silently in
    // the guest and surfaces later as an unrelated-looking auth failure.
    let _ = writeln!(
        out,
        "Agent forwarding (`ssh -A`) is not served and refuses silently — `SSH_AUTH_SOCK` is \
         simply empty in the guest (§19.3)."
    );
    if let Some(workspace) = workspace {
        // The reassurance that makes closing the shell safe (§19.7): the
        // syncer belongs to the lab daemon, not to this command.
        let _ = writeln!(
            out,
            "The workspace syncer is the lab daemon's, so leaving this shell does not stop it \
             ({} in the lab directory).",
            workspace.display(),
        );
    }
    let _ = writeln!(out);
    let _ = write!(
        out,
        "{}",
        crate::ssh_config::editor_snippet(aliases, windows)
    );
    let _ = writeln!(out, "\nopening a shell on \"{machine}\"…");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::select::Source;
    use crate::labd::workspace::diff::Sides;
    use crate::status::fixtures::{attachable, container, machine, vm};

    fn resolved(workspace: Option<&str>) -> ResolvedDev {
        ResolvedDev {
            name: "dev01".into(),
            default: true,
            workspace: workspace.map(std::path::PathBuf::from),
            workspace_guest: "/src".into(),
        }
    }

    /// A host workspace path as the declaration carries it.
    fn workspace(path: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(path)
    }

    /// The wait ends on `attachable` and nothing else — not on running, not
    /// on ready. That is the whole point of waiting past `up` (§19.4).
    #[test]
    fn the_wait_ends_on_attachable() {
        let ready = attachable(machine("dev01", PowerState::Running, true, vm()));
        assert_eq!(verdict("dev01", &ready, None), Verdict::Attachable);
    }

    /// While it is coming up, each stage says which stage it is: a cold
    /// Windows guest spends minutes in these, and a cursor says nothing.
    #[test]
    fn each_stage_of_the_wait_says_what_it_is_waiting_for() {
        let starting = machine("dev01", PowerState::Starting, false, vm());
        let Verdict::Waiting(said) = verdict("dev01", &starting, None) else {
            panic!("a machine that is not running is still being waited on");
        };
        assert!(said.contains("waiting for \"dev01\" to run"), "{said}");

        let booting = machine("dev01", PowerState::Running, false, vm());
        let Verdict::Waiting(said) = verdict("dev01", &booting, None) else {
            panic!("a machine that is not ready is still being waited on");
        };
        assert!(said.contains("become ready"), "{said}");

        // Ready, and its agent has said nothing yet — silence is a handshake
        // still to come, never a verdict.
        let silent = machine("dev01", PowerState::Running, true, vm());
        let Verdict::Waiting(said) = verdict("dev01", &silent, Some(&[])) else {
            panic!("an agent that has not answered is still being waited on");
        };
        assert!(said.contains("agent to answer"), "{said}");
    }

    /// An agent that *has* answered and cannot serve an attach is §19.4's hard
    /// rung: the wait stops, names what is missing, and names both remedies —
    /// waiting out the timeout for an answer that already arrived is worse
    /// than useless.
    #[test]
    fn an_agent_that_answered_without_the_features_is_refused_not_waited_on() {
        let stale = machine("dev01", PowerState::Running, true, vm());
        let features = ["terminal".to_string(), "exec".to_string()];
        let Verdict::Refused(said) = verdict("dev01", &stale, Some(&features)) else {
            panic!("an agent that answered and cannot attach is refused");
        };
        assert!(said.starts_with("an attach:"), "{said}");
        assert!(
            said.contains("\"dev01\"'s agent serves no `tunnel` and `fileops`"),
            "{said}"
        );
        assert!(said.contains("rebuild the template"), "{said}");
        assert!(said.contains("repair-agent dev01"), "{said}");
    }

    /// The agent is asked exactly when its silence would be evidence — the
    /// one state whose meaning the status projection cannot settle by itself.
    /// Asking earlier would cost a request per second through a whole boot;
    /// not asking there would wait out the timeout for an answer that already
    /// arrived.
    #[test]
    fn the_agent_is_asked_exactly_when_its_silence_would_be_evidence() {
        assert!(asks_the_agent(&machine(
            "dev01",
            PowerState::Running,
            true,
            vm()
        )));
        assert!(!asks_the_agent(&machine(
            "dev01",
            PowerState::Running,
            false,
            vm()
        )));
        assert!(!asks_the_agent(&machine(
            "dev01",
            PowerState::Starting,
            false,
            vm()
        )));
        assert!(!asks_the_agent(&attachable(machine(
            "dev01",
            PowerState::Running,
            true,
            vm()
        ))));
    }

    /// Giving up says what it was waiting for, and where to look — a bare
    /// timeout cannot be acted on.
    #[test]
    fn the_timeout_says_what_it_was_waiting_for() {
        let said = timed_out("dev01", "waiting for \"dev01\"'s agent to answer");
        assert!(said.contains("agent to answer"), "{said}");
        assert!(said.contains("vmlab machine capabilities dev01"), "{said}");
        assert!(said.contains("vmlab ssh dev01"), "{said}");
    }

    /// Which machine, and why that one — printed before anything boots, so a
    /// developer who expected another can stop it.
    #[test]
    fn the_opening_line_names_the_machine_and_the_rung_that_chose_it() {
        let dev = resolved(None);
        let said = opening(
            "lab",
            &Selected {
                dev: &dev,
                source: Source::Selection,
            },
        );
        assert!(
            said.contains("attaching to \"dev01\" in lab \"lab\""),
            "{said}"
        );
        assert!(said.contains("recorded by `vmlab dev use`"), "{said}");
    }

    /// The handover: every alias, the instruction to pick one, and the editor
    /// snippet — and no editor launched or named.
    #[test]
    fn the_report_hands_over_the_alias_and_the_snippet_and_launches_nothing() {
        let out = attach_report(
            "dev01",
            &[
                "vmlab-lab-dev01".to_string(),
                "vmlab-lab-dev01-admin".to_string(),
            ],
            None,
            true,
        );
        assert!(out.contains("ssh alias  vmlab-lab-dev01"), "{out}");
        assert!(
            out.contains("vmlab-lab-dev01-admin"),
            "the labelled login is a pick too: {out}"
        );
        assert!(out.contains("vmlab launches none"), "{out}");
        // §19.3's one refusal worth vmlab's own words: the protocol carries no
        // text for it, so nothing else can say it.
        assert!(out.contains("Agent forwarding (`ssh -A`)"), "{out}");
        assert!(out.contains("SSH_AUTH_SOCK"), "{out}");
        // The Windows half of the snippet (§19.8), for the machine that needs
        // it — and the client-side key both families need.
        assert!(out.contains("remote.SSH.localServerDownload"), "{out}");
        assert!(out.contains("\"vmlab-lab-dev01\": \"windows\""), "{out}");
    }

    /// A dev machine with a workspace gets the one sentence that makes
    /// closing the shell safe: the syncer is not this process's (§19.7).
    #[test]
    fn a_workspace_is_promised_to_outlive_the_shell() {
        let ws = workspace("./src");
        let out = attach_report("dev01", &["vmlab-lab-dev01".into()], Some(&ws), false);
        assert!(out.contains("leaving this shell does not stop it"), "{out}");
        assert!(out.contains("./src"), "{out}");

        // A dev machine without one says nothing about sync at all.
        let out = attach_report("dev01", &["vmlab-lab-dev01".into()], None, false);
        assert!(!out.contains("syncer"), "{out}");
    }

    /// One machine's syncer, halted on two paths.
    fn halted_sync() -> WorkspaceSyncStatus {
        WorkspaceSyncStatus {
            halt: Some(
                "the workspace on \"dev01\" has stopped, both directions, on 2 conflicting paths"
                    .into(),
            ),
            conflicts: vec![
                crate::status::WorkspaceConflictStatus {
                    path: "src/main.rs".into(),
                    reason: "both sides changed it since they last agreed".into(),
                },
                crate::status::WorkspaceConflictStatus {
                    path: ".env".into(),
                    reason: "both sides created it, with different content".into(),
                },
            ],
            conflicts_total: 2,
            resolve: Some("`vmlab dev sync resolve <path> --host` or `--guest`".into()),
            volume: None,
            rescan: None,
            rescans: 0,
            skipped: Vec::new(),
            deferred: Vec::new(),
            trouble: None,
            passes: 7,
        }
    }

    /// A halt is reported whole: every path in the batch, with why, and the
    /// routes out. One at a time would turn one `git pull` into thirty
    /// resolve-and-resume round trips.
    #[test]
    fn a_halt_is_reported_with_every_path_and_the_routes_out() {
        let said = sync_report("dev01", &halted_sync());
        assert!(said.contains("has stopped, both directions"), "{said}");
        assert!(said.contains("src/main.rs"), "{said}");
        assert!(said.contains(".env"), "{said}");
        assert!(said.contains("both sides created it"), "{said}");
        assert!(said.contains("--host"), "{said}");
    }

    /// The cap is *said*, because a truncation nobody is told about reads as
    /// a complete list — and the way out of the 30 000-file case needs no
    /// list at all.
    #[test]
    fn a_capped_halt_says_what_it_did_not_list() {
        let said = sync_report(
            "dev01",
            &WorkspaceSyncStatus {
                conflicts_total: 30_000,
                ..halted_sync()
            },
        );
        assert!(said.contains("29998 more"), "{said}");
        assert!(said.contains("`--all` needs no list"), "{said}");
    }

    /// Everything the syncer declined to do, by name — a `.sock` in the tree,
    /// a deferred `.git/index`, a burst. None of them is a halt, and every one
    /// of them silently stops a path syncing if nobody says it.
    #[test]
    fn the_status_names_the_warnings_the_skips_and_the_deferrals() {
        let said = sync_report(
            "dev01",
            &WorkspaceSyncStatus {
                halt: None,
                conflicts: Vec::new(),
                conflicts_total: 0,
                resolve: None,
                volume: Some("this pass is carrying 4000 paths under target".into()),
                rescan: Some("the guest's watch lost coverage".into()),
                rescans: 3,
                skipped: vec![crate::status::WorkspaceSkipStatus {
                    path: "build/app.sock".into(),
                    reason: "guest: not a file, directory or symlink".into(),
                }],
                deferred: vec![".git/index".into()],
                trouble: None,
                passes: 12,
            },
        );
        assert!(said.contains("in step"), "{said}");
        assert!(said.contains("4000 paths under target"), "{said}");
        assert!(said.contains("watch lost coverage"), "{said}");
        assert!(said.contains("build/app.sock"), "{said}");
        assert!(said.contains(".git/index"), "{said}");
        assert!(
            said.contains("timing, not a conflict"),
            "a deferral must not read as something to resolve: {said}"
        );
    }

    /// A machine whose workspace agrees with itself says so plainly, rather
    /// than saying nothing — which is what a stopped syncer also looks like.
    #[test]
    fn a_workspace_in_step_says_so() {
        let said = sync_report(
            "dev01",
            &WorkspaceSyncStatus {
                halt: None,
                conflicts: Vec::new(),
                conflicts_total: 0,
                resolve: None,
                ..halted_sync()
            },
        );
        assert!(said.contains("is in step (7 passes)"), "{said}");
    }

    /// The verb this one exists for: the guest's copy, host-side, next to the
    /// host's — because the developer is attached *into* the guest and would
    /// otherwise attach twice to see both.
    #[test]
    fn the_diff_shows_both_copies_as_a_unified_diff() {
        let reply = Diff {
            machine: "dev01".into(),
            host_root: "/lab/src".into(),
            guest_root: "/src".into(),
            files: vec![Sides {
                path: "main.rs".into(),
                host: Some(SideCopy {
                    size: 20,
                    digest: "a".repeat(64),
                    text: Some("fn main() {\n    host();\n}\n".into()),
                    omitted: None,
                }),
                guest: Some(SideCopy {
                    size: 21,
                    digest: "b".repeat(64),
                    text: Some("fn main() {\n    guest();\n}\n".into()),
                    omitted: None,
                }),
                identical: false,
            }],
        };
        let said = render_diff(&reply);
        assert!(said.contains("=== main.rs"), "{said}");
        assert!(said.contains("  -    host();"), "{said}");
        assert!(said.contains("  +    guest();"), "{said}");
        assert!(said.contains("   fn main() {"), "unchanged lines: {said}");
    }

    /// Bytes nobody would read are described rather than dumped, and both
    /// sides are still compared by the thing that answers the question.
    #[test]
    fn an_undiffable_pair_is_described_rather_than_printed() {
        let reply = Diff {
            machine: "dev01".into(),
            host_root: "/lab/src".into(),
            guest_root: "/src".into(),
            files: vec![Sides {
                path: "disk.vhdx".into(),
                host: Some(SideCopy {
                    size: 4_000_000_000,
                    digest: String::new(),
                    text: None,
                    omitted: Some("it is 4000000000 bytes, over the inline cap".into()),
                }),
                guest: None,
                identical: false,
            }],
        };
        let said = render_diff(&reply);
        assert!(said.contains("the guest does not hold it"), "{said}");
        assert!(said.contains("over the inline cap"), "{said}");
    }

    /// The third route, which needs no verb: identical copies are adopted as
    /// agreed by the next pass, so the diff says so rather than printing an
    /// empty diff.
    #[test]
    fn identical_copies_are_reported_as_the_resolution_they_are() {
        let reply = Diff {
            machine: "dev01".into(),
            host_root: "/lab/src".into(),
            guest_root: "/src".into(),
            files: vec![Sides {
                path: "a.rs".into(),
                host: Some(SideCopy {
                    size: 4,
                    digest: "c".repeat(64),
                    text: Some("same".into()),
                    omitted: None,
                }),
                guest: Some(SideCopy {
                    size: 4,
                    digest: "c".repeat(64),
                    text: Some("same".into()),
                    omitted: None,
                }),
                identical: true,
            }],
        };
        let said = render_diff(&reply);
        assert!(said.contains("identical"), "{said}");
        assert!(said.contains("adopts them as agreed"), "{said}");
    }

    /// A unified diff of the ordinary shapes, so the renderer is not trusted
    /// on the strength of one example.
    #[test]
    fn the_unified_diff_handles_insertions_deletions_and_empty_sides() {
        assert_eq!(unified("a\nb\n", "a\nb\n"), "   a\n   b\n");
        assert_eq!(unified("a\n", "a\nb\n"), "   a\n  +b\n");
        assert_eq!(unified("a\nb\n", "a\n"), "   a\n  -b\n");
        assert_eq!(unified("", "new\n"), "  +new\n");
        assert_eq!(unified("gone\n", ""), "  -gone\n");
    }

    /// A container is a dev machine like any other (§19.1), and the wait reads
    /// the same two flags off the same projection for it — there is no
    /// machine-kind branch anywhere in here.
    #[test]
    fn a_container_dev_machine_is_waited_on_the_same_way() {
        let c = machine(
            "buildbox",
            PowerState::Running,
            true,
            container(Some(true), None),
        );
        assert!(matches!(
            verdict("buildbox", &c, Some(&[])),
            Verdict::Waiting(_)
        ));
        assert_eq!(
            verdict("buildbox", &attachable(c), None),
            Verdict::Attachable
        );
    }
}
