//! Windows costs vmlab three actions (PRD §19.6).
//!
//! Each is a precondition of the *mechanism*, which is a different category
//! from the toolchain being the lab author's `provision {}` — so vmlab does
//! them rather than documenting them:
//!
//! 1. **The NTFS case-sensitive flag on every directory the syncer creates,
//!    at creation.** The host can hold `Foo.cs` and `foo.cs`; a default
//!    Windows guest cannot, and letting it happen means the second write
//!    silently lands on the first — the exact silent-divergence class that
//!    disqualified the share transports. The flag takes only while a
//!    directory is empty, which the syncer's always is, so it rides the
//!    `mkdir` the file vocabulary already carries, per directory, with
//!    **no reliance on inheritance**.
//! 2. **Symlinks attempted, and a warning by name on failure.** A
//!    symlink-capable image is a documented precondition (§19.4); vmlab does
//!    not work around it silently.
//! 3. **`core.autocrlf = false` in the guest's git config.** Git for Windows
//!    ships it `true`, which would rewrite the whole working tree to CRLF on
//!    the first guest-side checkout, sync every file back as modified, and —
//!    if the host had touched anything — halt the whole workspace. The
//!    syncer itself translates nothing; bytes cross verbatim and git does all
//!    normalisation on both sides, from settings that now agree.
//!
//! Where the flag cannot be set, a case collision at that path is a **loud
//! refusal** — refuse-at-seed is the fallback, not the policy.
//!
//! **A Windows login declared `elevated = false` degrades the workspace in
//! exactly two ways — no case-sensitive directories and no symlinks — and
//! that is reported up front**, because otherwise both fail at a random path
//! hours in, looking like a vmlab bug.
//!
//! This is a value computed before the loop runs (ADR-0003): the guest family
//! and the machine's declared default login decide it once, so no part of the
//! syncer has to ask "am I on Windows?" mid-pass.

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::guest::GuestFs;
use super::ignore::TEMP_PREFIX;
use super::scan::join_guest;
use crate::config::model::{Login, default_login};
use crate::labd::guest_os::GuestOs;

/// The guest's git config, set as the machine's default login — whose
/// checkouts these are, and whose `--global` config git will read.
pub const GIT_LINE_ENDINGS: [&str; 5] = ["git", "config", "--global", "core.autocrlf", "false"];

fn git_line_endings() -> Vec<String> {
    GIT_LINE_ENDINGS.iter().map(|a| a.to_string()).collect()
}

/// What one guest costs the syncer, resolved once before it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Preconditions {
    /// The guest is Windows-family. Everything below is nothing at all on
    /// Linux, where directories are case-sensitive by construction, symlinks
    /// need no privilege and git converts no line endings.
    pub windows: bool,
    /// Ask for the NTFS flag at every `mkdir`. False on Windows means the
    /// login cannot set it, which is a *declared* degradation rather than a
    /// discovered one.
    pub case_sensitive_dirs: bool,
    /// Symlinks are expected to work. They are attempted either way — "attempt
    /// and warn" is the rule — but a false here is what makes the warning
    /// expected rather than a mystery.
    pub symlinks: bool,
}

impl Preconditions {
    /// The two named degradations, in the words the developer gets **up
    /// front**. Empty where nothing is degraded, which is every Linux guest
    /// and every Windows guest whose dev login is elevated.
    pub fn degradations(self) -> Vec<&'static str> {
        let mut said = Vec::new();
        if !self.windows {
            return said;
        }
        if !self.case_sensitive_dirs {
            said.push(
                "this machine's default login is declared `elevated = false`, so the workspace's \
                 directories cannot be made case-sensitive: two host paths differing only in case \
                 are refused by name rather than silently collapsed onto one",
            );
        }
        if !self.symlinks {
            said.push(
                "this machine's default login is declared `elevated = false`, so symlinks cannot \
                 be created: each one is attempted and named as it fails",
            );
        }
        said
    }
}

/// Resolve one machine's preconditions from its guest family and its declared
/// `login {}` blocks.
///
/// Elevation is read from the declaration rather than from a minted logon, so
/// this needs no secret and cannot fail: a machine whose password is missing
/// still gets its degradations reported, and the missing password is the
/// session open's business to refuse.
///
/// An undeclared `elevated` is elevated on Windows — §19.2's default, because
/// the parity bar is devcontainers and a devcontainer gives you root — and a
/// machine declaring no login at all runs as the agent identity, which is
/// `SYSTEM` and elevated by construction.
pub fn preconditions(guest_os: GuestOs, logins: &[Login]) -> Preconditions {
    if guest_os != GuestOs::Windows {
        return Preconditions::default();
    }
    let elevated = default_login(logins).is_none_or(|login| login.elevated.unwrap_or(true));
    Preconditions {
        windows: true,
        case_sensitive_dirs: elevated,
        symlinks: elevated,
    }
}

/// What a guest command did. Enough to say what went wrong by name, and
/// nothing more: the preconditions run commands for their effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    pub exit_code: i32,
    pub stderr: String,
}

/// Running one command in the guest as the machine's default login.
///
/// Separate from the file seam on purpose: [`GuestFs`](super::guest::GuestFs)
/// is the whole of what the *sync loop* does to a guest, and keeping it that
/// way is what makes "what does the syncer do to a guest" one page. This is
/// the preconditions' seam, not the loop's, and it has exactly one caller —
/// [`set_line_endings`].
#[async_trait]
pub trait GuestRun: Send + Sync {
    /// Run `argv` to completion as the machine's default login.
    async fn run(&self, argv: Vec<String>) -> Result<Ran>;
}

/// Set the guest's line-ending conversion off, returning what to say if it did
/// not take.
///
/// A guest with no git yet is a warning, not a failure: the toolchain is the
/// lab author's `provision {}`, and a dev machine whose workspace syncs fine
/// must not be stopped because git arrives later. But it is never silent —
/// the whole point of the action is that the failure it prevents is invisible
/// until a guest-side checkout rewrites the tree.
pub async fn set_line_endings(guest: &dyn GuestRun) -> Option<String> {
    match guest.run(git_line_endings()).await {
        Ok(ran) if ran.exit_code == 0 => None,
        Ok(ran) => Some(format!(
            "`{}` exited {}: {}",
            GIT_LINE_ENDINGS.join(" "),
            ran.exit_code,
            ran.stderr.trim()
        )),
        Err(e) => Some(format!("`{}`: {e:#}", GIT_LINE_ENDINGS.join(" "))),
    }
}

/// Create the workspace root, and find out whether this guest will **really**
/// make a directory case-sensitive (§19.6).
///
/// Asking the guest is the only honest answer, and asking it *before* the
/// pass's plan is what makes the collision refusal fire on the pass that
/// needs it rather than the one after. The declaration says whether the
/// *login* may set the flag; the filesystem — or a Windows build without the
/// component that implements it — has its own opinion, and a workspace that
/// discovered the disagreement halfway through its first seed would already
/// have landed one of a colliding pair on top of the other.
///
/// The probe is a scratch directory rather than the root itself, because a
/// root left over from an earlier run answers "already there, fine" without
/// the flag ever being attempted — which is exactly the reading that would
/// make the second run silently wrong.
///
/// Returns whether the flag is available. An unreachable guest is an error:
/// the caller defers the whole pass rather than concluding anything.
pub async fn prepare_root(guest: &dyn GuestFs, guest_root: &str, want: bool) -> Result<bool> {
    let mut available = want;
    if let Err(refused) = guest.mkdir_root(guest_root, want).await {
        // Either the flag was refused or the root cannot be made at all, and
        // the second attempt is what tells them apart.
        guest
            .mkdir_root(guest_root, false)
            .await
            .map_err(|_| refused)
            .with_context(|| format!("creating the workspace root {guest_root}"))?;
        available = false;
    }
    if !available {
        return Ok(false);
    }

    // In the ignore floor like every other `.vmlab-sync.` name, so a crash
    // that leaves it behind can never make it a sync object.
    let probe = join_guest(guest_root, &format!("{TEMP_PREFIX}case-probe"));
    // A leftover from a killed pass would answer "already there" — the same
    // false yes the root itself would give.
    let _ = guest.rmdir(&probe).await;
    let took = guest.mkdir(&probe, true).await.is_ok();
    let _ = guest.rmdir(&probe).await;
    Ok(took)
}

/// What the loop has learned about one guest that a single pass cannot
/// re-derive on its own. Deliberately small: everything else about a
/// workspace is recomputed from both sides every pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Learned {
    /// Carried from the preconditions so the derivations below need nothing
    /// else: on Linux there is no flag to lose and nothing to fold.
    windows: bool,
    /// This guest will really make a directory case-sensitive — the
    /// declaration, corrected by what the guest actually did.
    pub case_sensitive_dirs: bool,
    /// The guest's line-ending conversion is off. Until it is, every pass
    /// tries again: a guest git has not reached yet is the normal case on a
    /// machine still provisioning, not a failure to give up on.
    pub line_endings_off: bool,
    /// …and the one warning it owes has been said. Once, not once a pass.
    pub line_endings_said: bool,
}

impl Learned {
    pub fn from(preconditions: Preconditions) -> Learned {
        Learned {
            windows: preconditions.windows,
            case_sensitive_dirs: preconditions.case_sensitive_dirs,
            // Nothing to do on a Linux guest, so it starts done.
            line_endings_off: !preconditions.windows,
            line_endings_said: false,
        }
    }

    /// Whether the guest will fold two names differing only in case onto one
    /// object, so a host-side pair like `Foo.cs`/`foo.cs` cannot both land —
    /// which is what turns a collision into a refusal naming the paths.
    ///
    /// False on Linux for the obvious reason, and false on a Windows guest
    /// that took the flag, because the flag is what stops the folding.
    pub fn case_folding(&self) -> bool {
        self.windows && !self.case_sensitive_dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::Span;

    const NO_SPAN: Span = (0, 0);

    fn login(label: &str, elevated: Option<bool>, default: bool) -> Login {
        Login {
            label: label.into(),
            user: format!(r"PROBE\{label}"),
            password: Some("vmlab123!".into()),
            elevated,
            default: default.then_some(true),
            span: NO_SPAN,
        }
    }

    /// A Linux guest costs nothing: its directories are case-sensitive by
    /// construction, so there is no flag to ask for and no collision to refuse.
    #[test]
    fn a_linux_guest_has_no_preconditions_at_all() {
        let got = preconditions(GuestOs::Linux, &[login("dev", None, true)]);
        assert_eq!(got, Preconditions::default());
        assert!(!Learned::from(got).case_folding());
        assert_eq!(got.degradations(), Vec::<&str>::new());
    }

    /// The ordinary Windows dev machine: the flag is asked for at every
    /// `mkdir`, symlinks are expected, and nothing is degraded.
    #[test]
    fn an_elevated_windows_login_gets_case_sensitive_directories_and_symlinks() {
        let got = preconditions(GuestOs::Windows, &[login("dev", None, true)]);
        assert!(got.case_sensitive_dirs);
        assert!(got.symlinks);
        assert!(
            !Learned::from(got).case_folding(),
            "the flag is what stops the folding"
        );
        assert_eq!(got.degradations(), Vec::<&str>::new());
    }

    /// **The headline of the ticket.** `elevated = false` degrades the
    /// workspace in exactly two ways, and both are said up front rather than
    /// discovered at a random path hours in.
    #[test]
    fn a_non_elevated_windows_login_reports_both_degradations_up_front() {
        let got = preconditions(GuestOs::Windows, &[login("dev", Some(false), true)]);
        assert!(!got.case_sensitive_dirs);
        assert!(!got.symlinks);
        assert!(
            Learned::from(got).case_folding(),
            "a folding guest refuses collisions"
        );

        let said = got.degradations();
        assert_eq!(said.len(), 2, "exactly two, no more: {said:?}");
        assert!(
            said.iter().any(|s| s.contains("case-sensitive")),
            "{said:?}"
        );
        assert!(said.iter().any(|s| s.contains("symlink")), "{said:?}");
        for s in &said {
            assert!(s.contains("elevated = false"), "unattributed: {s}");
        }
    }

    /// It is the *default* login the syncer runs as, so it is that login's
    /// elevation that decides — not some other declaration on the machine.
    #[test]
    fn it_is_the_default_login_that_decides() {
        let logins = vec![login("admin", None, false), login("dev", Some(false), true)];
        assert!(!preconditions(GuestOs::Windows, &logins).case_sensitive_dirs);
    }

    /// A machine declaring no login runs the syncer as the agent identity,
    /// which is `SYSTEM`: elevated by construction, so nothing is degraded.
    #[test]
    fn no_declared_login_is_the_agent_identity_and_elevated() {
        let got = preconditions(GuestOs::Windows, &[]);
        assert!(got.case_sensitive_dirs);
        assert!(got.symlinks);
    }

    /// The command is the one git reads for the login whose checkouts these
    /// are, and it is `false` — not `input`, which still rewrites on commit.
    #[test]
    fn the_line_ending_setting_is_off_globally_for_the_login() {
        assert_eq!(
            GIT_LINE_ENDINGS.join(" "),
            "git config --global core.autocrlf false"
        );
    }

    /// A guest that answers however it was told to, recording what it was
    /// asked to run.
    struct FakeRun {
        answer: std::sync::Mutex<Option<Result<Ran>>>,
        ran: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl FakeRun {
        fn answering(answer: Result<Ran>) -> FakeRun {
            FakeRun {
                answer: std::sync::Mutex::new(Some(answer)),
                ran: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl GuestRun for FakeRun {
        async fn run(&self, argv: Vec<String>) -> Result<Ran> {
            self.ran.lock().unwrap().push(argv);
            self.answer
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| anyhow::bail!("asked twice"))
        }
    }

    #[tokio::test]
    async fn the_setting_taking_says_nothing() {
        let guest = FakeRun::answering(Ok(Ran {
            exit_code: 0,
            stderr: String::new(),
        }));
        assert_eq!(set_line_endings(&guest).await, None);
        assert_eq!(
            guest.ran.lock().unwrap().as_slice(),
            [GIT_LINE_ENDINGS.map(str::to_string).to_vec()]
        );
    }

    /// A guest with no git yet is a warning, not a failure — the toolchain is
    /// the lab author's `provision {}` — but it is never silent, because the
    /// failure it prevents is invisible until a checkout rewrites the tree.
    #[tokio::test]
    async fn a_guest_with_no_git_is_named_rather_than_passed_over() {
        let guest = FakeRun::answering(Ok(Ran {
            exit_code: 127,
            stderr: "'git' is not recognized".into(),
        }));
        let said = set_line_endings(&guest).await.expect("silent");
        assert!(said.contains("core.autocrlf"), "{said}");
        assert!(said.contains("not recognized"), "{said}");
    }

    /// So is a guest that could not be reached at all.
    #[tokio::test]
    async fn an_unreachable_guest_is_named_too() {
        let guest = FakeRun::answering(Err(anyhow::anyhow!("the agent channel closed")));
        let said = set_line_endings(&guest).await.expect("silent");
        assert!(said.contains("the agent channel closed"), "{said}");
    }

    /// The workspace root is a directory the syncer creates like any other, so
    /// it carries the flag too — without it the files at the top of the tree
    /// land in the one directory nobody set it on.
    #[tokio::test]
    async fn the_workspace_root_is_created_case_sensitive() {
        let guest = super::super::guest::fake::FakeGuest::new();
        guest.folding();
        assert!(prepare_root(&guest, "/src", true).await.unwrap());
        assert!(guest.is_case_sensitive("/src"));
    }

    /// **The answer comes from the guest, before the plan needs it.** A guest
    /// that will not take the flag says so here rather than through a failed
    /// `mkdir` halfway through a seed that has already collapsed a colliding
    /// pair onto one path.
    #[tokio::test]
    async fn a_guest_that_will_not_take_the_flag_says_so_before_the_plan() {
        let guest = super::super::guest::fake::FakeGuest::new();
        guest.folding().refuse_case_flag();
        assert!(!prepare_root(&guest, "/src", true).await.unwrap());
        // …and the root still exists, because the workspace still has to go
        // somewhere.
        assert!(guest.get("/src").is_some());
        assert!(!guest.is_case_sensitive("/src"));
    }

    /// The probe is a scratch directory, not the root, because a root left by
    /// an earlier run answers "already there, fine" without the flag ever
    /// being attempted — the reading that makes the *second* run silently
    /// wrong. And it leaves nothing behind either way.
    #[tokio::test]
    async fn an_existing_root_is_still_probed_honestly_and_the_probe_is_cleaned_up() {
        let guest = super::super::guest::fake::FakeGuest::new();
        guest.folding().refuse_case_flag();
        guest.dir("/src");

        assert!(!prepare_root(&guest, "/src", true).await.unwrap());
        assert!(
            !guest.paths().iter().any(|p| p.contains("case-probe")),
            "{:?}",
            guest.paths()
        );
    }

    /// Nothing to probe where the flag was never on the table: the root is
    /// made and that is all.
    #[tokio::test]
    async fn a_guest_that_was_never_going_to_get_the_flag_is_not_probed() {
        let guest = super::super::guest::fake::FakeGuest::new();
        assert!(!prepare_root(&guest, "/src", false).await.unwrap());
        assert_eq!(guest.writes(), vec!["/src".to_string()]);
    }
}
