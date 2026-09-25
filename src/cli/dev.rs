//! `vmlab dev` (PRD §19.7) — the verbs that earn the `dev` noun because they
//! are meaningless for a machine that is not `@dev`: the `dev sync` verbs a
//! workspace halt is resolved through (§19.6), which live here because
//! ADR-0013 leaves no guest→host control path to offer them from.
//!
//! Which dev machine a verb is about resolves through the ladder in
//! [`crate::dev::select`].

use anyhow::{Context, Result, anyhow, bail};

use super::daemon::{self, remote};
use super::lab::{effective_profile_name, lab_status, load_lab_here, rt};
use crate::config::LabFile;
use crate::dev::ResolvedDev;
use crate::dev::select;
use crate::labd::workspace::diff::{Diff, SideCopy};
use crate::labd::workspace::plan::Winner;
use crate::proto::LabRequest;
use crate::proto::client::LabClient;
use crate::status::WorkspaceSyncStatus;

// ---------------------------------------------------------------------------
// `vmlab dev sync` (§19.6)
// ---------------------------------------------------------------------------

/// The dev machine these verbs are about, through the ladder in
/// [`select`] — argument, `$VMLAB_DEV_MACHINE`, the lab's default `@dev`
/// machine.
fn sync_target(machine: Option<String>) -> Result<(String, String)> {
    let (file, _root) = load_lab_here()?;
    let lab = file.lab.name.clone();
    let devs = dev_machines(&file);
    let selected = select::resolve(
        &lab,
        &devs,
        machine.as_deref(),
        select::env_selection().as_deref(),
    )?;
    Ok((lab, selected.name.clone()))
}

/// A lab daemon that is **already running**, or the reason there is nothing to
/// ask.
///
/// Deliberately not `ensure`: a syncer exists only while its machine is up, so
/// starting a daemon to be told "no syncer" would boot a lab in order to
/// deliver a negative answer.
async fn running_lab(lab: &str) -> Result<LabClient> {
    daemon::try_lab_daemon(lab).await.ok_or_else(|| {
        anyhow!("lab \"{lab}\" is not running, so no workspace is syncing — `vmlab up` starts it")
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
        // Not "in step": the two copies still differ, which is the whole
        // question a snapshot capture asks before it goes ahead. Saying "in
        // step" here would make `dev sync status` disagree with the refusal a
        // capture is about to print.
        None if !sync.unsynced.is_empty() => {
            let _ = writeln!(
                out,
                "\"{machine}\"'s workspace is {} {} behind the canonical copy ({} passes).",
                sync.unsynced.len(),
                if sync.unsynced.len() == 1 {
                    "path"
                } else {
                    "paths"
                },
                sync.passes,
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
        ("re-seeding", sync.reseed.as_ref()),
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
    if !sync.unsynced.is_empty() {
        let _ = writeln!(
            out,
            "\nnot yet carried across — a snapshot capture refuses while any of these stand \
             (§19.6):"
        );
        for path in &sync.unsynced {
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
/// profile (§19.1).
fn dev_machines(file: &LabFile) -> Vec<ResolvedDev> {
    let profiles = crate::profiles::ProfileSet::shipped().ok();
    crate::dev::machines(&file.lab, |m| {
        let name = effective_profile_name(m)?;
        profiles.as_ref()?.get(&name).cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labd::workspace::diff::Sides;

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
            reseed: None,
            rescans: 0,
            skipped: Vec::new(),
            deferred: Vec::new(),
            unsynced: Vec::new(),
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
                reseed: None,
                rescans: 3,
                skipped: vec![crate::status::WorkspaceSkipStatus {
                    path: "build/app.sock".into(),
                    reason: "guest: not a file, directory or symlink".into(),
                }],
                deferred: vec![".git/index".into()],
                unsynced: Vec::new(),
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

    /// A workspace that is behind is **not** in step, and says which paths —
    /// these are exactly what a snapshot capture refuses on (§19.6), so
    /// `status` calling them "in step" would disagree with the refusal a
    /// developer is about to read.
    #[test]
    fn a_workspace_that_is_behind_says_so_rather_than_saying_in_step() {
        let said = sync_report(
            "dev01",
            &WorkspaceSyncStatus {
                halt: None,
                conflicts: Vec::new(),
                conflicts_total: 0,
                resolve: None,
                unsynced: vec!["src/main.rs".into(), "src/lib.rs".into()],
                ..halted_sync()
            },
        );
        assert!(!said.contains("in step"), "{said}");
        assert!(said.contains("2 paths behind the canonical copy"), "{said}");
        assert!(said.contains("src/main.rs"), "{said}");
        assert!(said.contains("a snapshot capture refuses"), "{said}");
    }

    /// The re-seed barrier is its own state, not a rescan. Telling a
    /// developer to wait for a walk that is never going to run is worse than
    /// saying nothing.
    #[test]
    fn a_re_seeding_workspace_says_the_restore_is_what_it_is_waiting_on() {
        let said = sync_report(
            "dev01",
            &WorkspaceSyncStatus {
                halt: None,
                conflicts: Vec::new(),
                conflicts_total: 0,
                resolve: None,
                reseed: Some("a snapshot restore rewound this machine".into()),
                ..halted_sync()
            },
        );
        assert!(
            said.contains("re-seeding: a snapshot restore rewound"),
            "{said}"
        );
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
}
