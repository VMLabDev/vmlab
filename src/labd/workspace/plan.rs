//! Reconciliation as a value (PRD §19.6, ADR-0003).
//!
//! Host state, guest state and the ledger go in; what to do about each path
//! comes out. Nothing here performs I/O, which is what makes the rules that
//! matter most — *digest is the truth*, *a host mtime is never compared to a
//! guest mtime*, *a missing ledger is not a decision* — testable as arithmetic
//! rather than as a property of a running lab.
//!
//! Per path, each side is `unchanged | modified | deleted | replaced-by-other-
//! kind` **relative to the ledger**. One side changed → propagate. Both
//! changed → conflict, with the four riders §19.6 names:
//!
//! - **both modified with identical content is not a conflict** — adopt as
//!   agreed and transfer nothing, which is the common case after a host-side
//!   `git checkout` lands bytes the guest already had;
//! - **modified one side / deleted the other is a conflict, not delete-wins**,
//!   because deletion is unrecoverable and the modification is not yet
//!   propagated;
//! - **mode-only changes are not conflicts** and are not synced — which falls
//!   out of the ledger holding no mode at all, so a bit that cannot be
//!   represented on one side can never look like a disagreement;
//! - **file↔directory replacement is a conflict** where both sides moved.
//!
//! This ticket carries the **host→guest** direction. A path only the guest
//! moved is recorded as [`Plan::pending_guest`] rather than acted on — it is
//! left strictly alone, not lost, and the syncer says so out loud rather than
//! leaving a developer with nothing happening.

use std::collections::{BTreeMap, BTreeSet};

use super::ledger::{Agreed, Kind, Ledger, Side};

/// What one side holds at one path right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub kind: Kind,
    pub size: u64,
    pub mtime_ns: i64,
    /// The content digest, present **only when it was actually computed**.
    /// Absent means this side's `(size, mtime)` matched the ledger, so the
    /// ledger's digest still stands — the pre-filter's one and only power.
    pub digest: Option<String>,
    /// A symlink's target string, verbatim. Content like any other, and never
    /// translated across the seam.
    pub target: Option<String>,
    /// This side holds a file the size guard is going to refuse, so it was
    /// **never hashed** — the guard fires before the transfer, and hashing
    /// four gigabytes to then refuse them is the same wasted ten minutes in a
    /// different place.
    ///
    /// It therefore cannot read as unchanged: an absent digest normally means
    /// *the pre-filter vouched for this side*, and here it means the opposite.
    pub oversize: bool,
}

impl State {
    /// This side's own change-detector, which is only ever compared against
    /// this side's recorded one.
    pub fn side(&self) -> Side {
        Side::new(self.size, self.mtime_ns)
    }
}

/// One thing to do to the guest. Every apply is temp-name-then-rename in the
/// target's own directory, and the ledger is written only after the rename —
/// see [`apply`](super::apply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Create the directory. Carries the host's own `(size, mtime)` so the
    /// agreement can be recorded once the guest reports its own.
    MakeDir { path: String, host: Side },
    /// Place the file's bytes.
    PutFile {
        path: String,
        host: Side,
        digest: String,
    },
    /// Place the link verbatim. Never followed: a link pointing at `/` that
    /// the syncer followed would walk the entire host filesystem into the
    /// guest.
    PutSymlink {
        path: String,
        target: String,
        host: Side,
        digest: String,
        /// Windows picks a different object for a file link and a directory
        /// link **at creation** and cannot infer which from a target that is
        /// not there yet, so the kind has to be decided here — see
        /// [`dir_link`]. Ignored outright on a Linux guest.
        dir_link: bool,
    },
    /// Remove what the host no longer holds. **Host→guest deletes are
    /// unguarded** (§19.6): a `git checkout` removing 400 files just removes
    /// them, because the guest copy is the reconstructible one.
    Remove { path: String, kind: Kind },
}

impl Action {
    pub fn path(&self) -> &str {
        match self {
            Action::MakeDir { path, .. }
            | Action::PutFile { path, .. }
            | Action::PutSymlink { path, .. }
            | Action::Remove { path, .. } => path,
        }
    }
}

/// Why two sides cannot both be right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Neither side was ever agreed and they hold different things.
    BothCreated,
    /// Both moved since the agreement, to different content.
    BothModified,
    /// Deletion is unrecoverable and the modification is not yet propagated,
    /// so this is a conflict rather than delete-wins — in either direction.
    ModifiedAndDeleted,
    /// One side is now a directory where the other is a file (or a link).
    KindReplaced,
}

impl std::fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ConflictKind::BothCreated => "both sides created it, with different content",
            ConflictKind::BothModified => "both sides changed it since they last agreed",
            ConflictKind::ModifiedAndDeleted => "one side changed it and the other deleted it",
            ConflictKind::KindReplaced => "the two sides hold different kinds of entry",
        })
    }
}

/// A path both sides moved. The two copies already exist, one per side, and a
/// halt writes neither and deletes neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub kind: ConflictKind,
}

/// A file the size guard refuses, **before** transfer. Per file, so the
/// failure never depends on unrelated files and the message can always name a
/// culprit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Oversize {
    pub path: String,
    pub size: u64,
    pub cap: u64,
}

impl std::fmt::Display for Oversize {
    /// Names the file, the rule, and the two ways out — the point being not to
    /// spend ten minutes pushing something nobody wanted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is {} bytes, over the {}-byte workspace file cap: add an ignore rule for it in \
             .vmlabignore, or raise `workspace_max_file` in the host config",
            self.path, self.size, self.cap
        )
    }
}

/// Host paths that differ only in case, on a guest that folds case (§19.6).
///
/// **Refuse-at-seed is the fallback, not the policy.** The policy is the NTFS
/// case-sensitive flag at every `mkdir`; this is what is left where the flag
/// cannot be set, and the alternative is the second write silently landing on
/// the first — the exact silent-divergence class the share transports were
/// disqualified for. So neither copy is transferred and both names are said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    /// What the colliding paths fold to — the key they were grouped on, not a
    /// path either side holds.
    pub folded: String,
    /// Every host path folding onto it, in order.
    pub paths: Vec<String>,
}

impl std::fmt::Display for Collision {
    /// Names every path, and both ways out. Elevation is second because it is
    /// not always the reason: a filesystem with no concept of the flag refuses
    /// it however elevated the login is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} differ only in case and this guest cannot tell them apart, so none of them was \
             transferred: rename them apart on the host, or make this guest able to hold \
             case-sensitive directories — which needs the machine's default login to be \
             `elevated = true` and a filesystem that takes the flag",
            self.paths.join(", ")
        )
    }
}

/// A path the guest moved and the host did not. Left strictly alone by this
/// direction; guest→host propagation is its own ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGuest {
    pub path: String,
    /// The guest deleted it, rather than changing it.
    pub deleted: bool,
}

/// Everything one reconciliation decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Ordered: removals deepest-first, then creations parents-first, so a
    /// path replaced by another kind is cleared before it is remade.
    pub actions: Vec<Action>,
    /// Matching content adopted as agreed for free — no transfer, ledger
    /// written. This is what keeps a wiped `.vmlab/` from re-pushing a tree
    /// the guest already holds.
    pub adopt: Vec<(String, Agreed)>,
    /// Ledger entries there is no longer anything to agree about: nothing
    /// left on either side, or a path the ignore rules have since made
    /// guest-owned.
    pub forget: Vec<String>,
    pub conflicts: Vec<Conflict>,
    pub oversize: Vec<Oversize>,
    /// Host paths a case-folding guest cannot hold apart. Nothing is
    /// transferred for any of them.
    pub collisions: Vec<Collision>,
    pub pending_guest: Vec<PendingGuest>,
}

impl Plan {
    /// Nothing here changes the ledger. Deliberately silent about
    /// [`pending_guest`](Plan::pending_guest), which records what this
    /// direction leaves alone and so records nothing.
    pub fn nothing_to_record(&self) -> bool {
        self.actions.is_empty()
            && self.adopt.is_empty()
            && self.forget.is_empty()
            && self.conflicts.is_empty()
            && self.oversize.is_empty()
            && self.collisions.is_empty()
    }
}

/// What one reconciliation is computed from.
pub struct Inputs<'a> {
    /// The host tree, ignores already applied, keyed by `/`-separated path.
    pub host: &'a BTreeMap<String, State>,
    /// What the guest holds at the paths that were probed — the host's set
    /// plus the ledger's. A guest-only path is not probed here at all.
    pub guest: &'a BTreeMap<String, State>,
    pub ledger: &'a Ledger,
    /// Paths one side could not be read for — a special file, or something
    /// the syncer has no permission to open. Left strictly alone in both
    /// directions and reported by name.
    ///
    /// They are excluded rather than treated as absent because *"nothing is
    /// there"* and *"I could not look"* produce opposite actions, and only one
    /// of them is recoverable: a probe that failed and read as absence would
    /// seed straight over whatever the guest is really holding.
    pub undecided: &'a BTreeSet<String>,
    /// Ledger paths the ignore rules have since made **guest-owned**.
    ///
    /// The rules live *in* the tree and are developer-owned, so they change
    /// under the syncer. **Leaving scope is free**: the path leaves the
    /// ledger, both copies stay, and neither side is touched again. Without
    /// this the path would simply be missing from the host scan, read as a
    /// host-side delete, and take the guest's copy with it — which is the
    /// opposite of what un-ignoring a path is for.
    pub guest_owned: &'a BTreeSet<String>,
    /// The size guard's per-file cap.
    pub max_file_bytes: u64,
    /// The guest folds two names differing only in case onto one object
    /// (§19.6) — a Windows guest whose directories could not be made
    /// case-sensitive. Host paths that collide under folding are refused by
    /// name rather than raced onto one guest path.
    pub case_folding: bool,
}

/// One side's position relative to the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Moved {
    /// Nothing there, and nothing was ever agreed.
    Absent,
    Created,
    Unchanged,
    Modified,
    Deleted,
    ReplacedByOtherKind,
}

impl Moved {
    fn changed(self) -> bool {
        !matches!(self, Moved::Unchanged | Moved::Absent)
    }
}

/// Where one side stands against what was last agreed.
fn moved(now: Option<&State>, was: Option<&Agreed>) -> Moved {
    match (now, was) {
        (None, None) => Moved::Absent,
        (Some(_), None) => Moved::Created,
        (None, Some(_)) => Moved::Deleted,
        (Some(now), Some(was)) => {
            if now.kind != was.kind {
                Moved::ReplacedByOtherKind
            } else if now.oversize {
                // Never hashed, so its digest cannot vouch for it. Treating
                // an unhashed file as unchanged would let a file the guard
                // exists to refuse pass by in silence.
                Moved::Modified
            } else if digest_of(now, was) == was.digest {
                Moved::Unchanged
            } else {
                Moved::Modified
            }
        }
    }
}

/// The digest that stands for this side: the one computed, or — where the
/// pre-filter vouched for the side and none was computed — the agreed one.
fn digest_of(now: &State, was: &Agreed) -> String {
    now.digest.clone().unwrap_or_else(|| was.digest.clone())
}

/// The digest a side holds when there is nothing agreed to fall back on. A
/// directory has no content, so it agrees with any other directory.
fn standalone_digest(now: &State) -> String {
    now.digest.clone().unwrap_or_default()
}

/// Decide what to do about every path in the host tree and the ledger.
///
/// The **seed is the first pass, not a separate mechanism**: an empty guest
/// tree is simply the case where every host path has no counterpart, which
/// falls out of the same matrix.
pub fn reconcile(inputs: &Inputs<'_>) -> Plan {
    let mut plan = Plan::default();
    let mut removals: Vec<Action> = Vec::new();
    let mut creations: Vec<Action> = Vec::new();

    let paths: BTreeSet<&String> = inputs
        .host
        .keys()
        .chain(inputs.ledger.entries.keys())
        .collect();

    // Before anything is decided about them: a guest that folds case cannot
    // hold `Foo.cs` and `foo.cs` apart, and the second write would land on the
    // first in silence. Both are refused, by name.
    plan.collisions = collisions(inputs);
    let colliding: BTreeSet<&str> = plan
        .collisions
        .iter()
        .flat_map(|c| c.paths.iter().map(String::as_str))
        .collect();

    for path in paths {
        if inputs.undecided.contains(path) || colliding.contains(path.as_str()) {
            continue;
        }
        // Leaving scope is free: out of the ledger, and out of both
        // directions' way.
        if inputs.guest_owned.contains(path) {
            plan.forget.push(path.clone());
            continue;
        }
        let host = inputs.host.get(path);
        let guest = inputs.guest.get(path);
        let was = inputs.ledger.entries.get(path);
        let (host_moved, guest_moved) = (moved(host, was), moved(guest, was));

        // Neither side has anything left: the agreement has nothing to be
        // about any more.
        if host.is_none() && guest.is_none() {
            if was.is_some() {
                plan.forget.push(path.clone());
            }
            continue;
        }

        if host_moved.changed() && guest_moved.changed() {
            match settle_both(path, host, guest, was) {
                Settled::Adopt(agreed) => plan.adopt.push((path.clone(), agreed)),
                Settled::Conflict(kind) => plan.conflicts.push(Conflict {
                    path: path.clone(),
                    kind,
                }),
            }
            continue;
        }

        // Only the guest moved: left strictly alone by this direction.
        if guest_moved.changed() {
            plan.pending_guest.push(PendingGuest {
                path: path.clone(),
                deleted: guest_moved == Moved::Deleted,
            });
            continue;
        }

        if !host_moved.changed() {
            continue;
        }

        // Only the host moved: propagate.
        match host {
            None => {
                if let Some(was) = was {
                    removals.push(Action::Remove {
                        path: path.clone(),
                        kind: was.kind,
                    });
                }
            }
            Some(state) => {
                // A kind replacement clears the old entry first; the guest
                // cannot rename a file over a directory.
                if let Some(guest) = guest
                    && guest.kind != state.kind
                {
                    removals.push(Action::Remove {
                        path: path.clone(),
                        kind: guest.kind,
                    });
                }
                match place(path, state, inputs, was) {
                    Ok(action) => creations.push(action),
                    Err(refusal) => plan.oversize.push(refusal),
                }
            }
        }
    }

    // Children before their parents on the way out, parents before their
    // children on the way in.
    removals.sort_by(|a, b| b.path().cmp(a.path()));
    creations.sort_by(|a, b| a.path().cmp(b.path()));
    plan.actions = removals;
    plan.actions.extend(creations);
    plan
}

/// The outcome for a path both sides moved.
enum Settled {
    Adopt(Agreed),
    Conflict(ConflictKind),
}

fn settle_both(
    path: &str,
    host: Option<&State>,
    guest: Option<&State>,
    was: Option<&Agreed>,
) -> Settled {
    let (Some(host), Some(guest)) = (host, guest) else {
        // One of them deleted it. Deletion is unrecoverable and the other
        // side's change is not yet propagated, so this is never delete-wins.
        let _ = path;
        return Settled::Conflict(ConflictKind::ModifiedAndDeleted);
    };
    if host.kind != guest.kind {
        return Settled::Conflict(ConflictKind::KindReplaced);
    }
    let (host_digest, guest_digest) = match was {
        Some(was) => (digest_of(host, was), digest_of(guest, was)),
        None => (standalone_digest(host), standalone_digest(guest)),
    };
    if host_digest == guest_digest {
        // Identical content is not a conflict: adopt as agreed and transfer
        // nothing. Common after a host-side `git checkout` lands bytes the
        // guest already had — and it is also what a first run against a live
        // guest gets for free.
        return Settled::Adopt(Agreed {
            kind: host.kind,
            digest: host_digest,
            host: host.side(),
            guest: guest.side(),
        });
    }
    Settled::Conflict(match was {
        Some(_) => ConflictKind::BothModified,
        None => ConflictKind::BothCreated,
    })
}

/// Every set of host paths a case-folding guest would land on one object.
///
/// Keyed on the whole relative path rather than per directory, because that is
/// exactly the question being asked: two host paths that lower-case to the
/// same string are two host paths that become one guest path — whether they
/// differ in the file name or three directories up.
fn collisions(inputs: &Inputs<'_>) -> Vec<Collision> {
    if !inputs.case_folding {
        return Vec::new();
    }
    let mut folded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in inputs.host.keys() {
        folded
            .entry(path.to_lowercase())
            .or_default()
            .push(path.clone());
    }
    folded
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|(folded, paths)| Collision { folded, paths })
        .collect()
}

/// Which Windows object a link needs, decided **without following it**.
///
/// Never-follow is load-bearing — a link pointing at `/` that the syncer
/// followed walks the entire host filesystem into the guest — so the target is
/// resolved as a *string* against the link's own directory and looked up in
/// the host tree the scan already produced. A target that is absolute, that
/// escapes the workspace, or that names nothing the host holds gets a file
/// link: it is going to dangle either way, and §19.6 says a dangling link is
/// correct rather than something to interpret.
fn dir_link(link: &str, target: &str, host: &BTreeMap<String, State>) -> bool {
    // A drive letter, not any colon: `weird:name` is a perfectly legal
    // relative name on the host this tree came off.
    let drive_qualified =
        target.as_bytes().get(1) == Some(&b':') && target.as_bytes()[0].is_ascii_alphabetic();
    if target.starts_with('/') || target.starts_with('\\') || drive_qualified {
        return false;
    }
    let mut parts: Vec<&str> = match link.rfind('/') {
        Some(cut) => link[..cut].split('/').collect(),
        None => Vec::new(),
    };
    for segment in target.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    // Out of the workspace, which the host tree cannot answer
                    // for and the syncer will not walk to find out.
                    return false;
                }
            }
            segment => parts.push(segment),
        }
    }
    host.get(&parts.join("/"))
        .is_some_and(|state| state.kind == Kind::Dir)
}

/// The action that puts `state` on the guest, or the size guard's refusal.
fn place(
    path: &str,
    state: &State,
    inputs: &Inputs<'_>,
    was: Option<&Agreed>,
) -> Result<Action, Oversize> {
    let cap = inputs.max_file_bytes;
    let host = state.side();
    match state.kind {
        Kind::Dir => Ok(Action::MakeDir {
            path: path.to_string(),
            host,
        }),
        Kind::File => {
            // Before transfer, always: the point is not to spend ten minutes
            // pushing something unwanted and refuse afterwards.
            if state.size > cap {
                return Err(Oversize {
                    path: path.to_string(),
                    size: state.size,
                    cap,
                });
            }
            Ok(Action::PutFile {
                path: path.to_string(),
                host,
                digest: digest_or_agreed(state, was),
            })
        }
        Kind::Symlink => {
            let target = state.target.clone().unwrap_or_default();
            Ok(Action::PutSymlink {
                dir_link: dir_link(path, &target, inputs.host),
                path: path.to_string(),
                target,
                host,
                digest: digest_or_agreed(state, was),
            })
        }
    }
}

fn digest_or_agreed(state: &State, was: Option<&Agreed>) -> String {
    match was {
        Some(was) => digest_of(state, was),
        None => standalone_digest(state),
    }
}

/// Whether this side has to be hashed before it can be reasoned about — the
/// pre-filter, and the only place `(size, mtime)` is allowed an opinion.
///
/// Absent agreement there is nothing to pre-filter against, so a digest is
/// always needed; otherwise the same side's own recorded pair decides.
pub fn needs_digest(agreed: Option<&Agreed>, kind: Kind, side: Side, host_side: bool) -> bool {
    if kind == Kind::Dir {
        return false;
    }
    match agreed {
        None => true,
        Some(agreed) => {
            let recorded = if host_side { agreed.host } else { agreed.guest };
            agreed.kind != kind || !recorded.unchanged(side.size, side.mtime_ns)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const CAP: u64 = 1 << 30;

    fn file(digest: Option<&str>, size: u64, mtime_ns: i64) -> State {
        State {
            kind: Kind::File,
            size,
            mtime_ns,
            digest: digest.map(str::to_string),
            target: None,
            oversize: false,
        }
    }

    fn dir(mtime_ns: i64) -> State {
        State {
            kind: Kind::Dir,
            size: 0,
            mtime_ns,
            digest: None,
            target: None,
            oversize: false,
        }
    }

    fn link(target: &str, mtime_ns: i64) -> State {
        State {
            kind: Kind::Symlink,
            size: target.len() as u64,
            mtime_ns,
            digest: Some(format!("digest-of:{target}")),
            target: Some(target.to_string()),
            oversize: false,
        }
    }

    fn tree(entries: &[(&str, State)]) -> BTreeMap<String, State> {
        entries
            .iter()
            .map(|(p, s)| ((*p).to_string(), s.clone()))
            .collect()
    }

    fn ledger(entries: &[(&str, Agreed)]) -> Ledger {
        let mut ledger = Ledger::new(Path::new("/lab/src"), "/src");
        for (path, agreed) in entries {
            ledger.entries.insert((*path).to_string(), agreed.clone());
        }
        ledger
    }

    fn agreed(kind: Kind, digest: &str, host: Side, guest: Side) -> Agreed {
        Agreed {
            kind,
            digest: digest.to_string(),
            host,
            guest,
        }
    }

    fn run(host: &BTreeMap<String, State>, guest: &BTreeMap<String, State>, l: &Ledger) -> Plan {
        reconcile(&Inputs {
            host,
            guest,
            ledger: l,
            undecided: &BTreeSet::new(),
            guest_owned: &BTreeSet::new(),
            max_file_bytes: CAP,
            case_folding: false,
        })
    }

    /// The same, against a guest that cannot tell `Foo.cs` from `foo.cs`.
    fn run_folding(host: &BTreeMap<String, State>) -> Plan {
        reconcile(&Inputs {
            host,
            guest: &BTreeMap::new(),
            ledger: &ledger(&[]),
            undecided: &BTreeSet::new(),
            guest_owned: &BTreeSet::new(),
            max_file_bytes: CAP,
            case_folding: true,
        })
    }

    /// **Where the flag cannot be set, a collision is a loud refusal naming
    /// the paths** (§19.6). Never a quiet second write landing on the first,
    /// which is the silent-divergence class this whole design rules out.
    #[test]
    fn a_case_collision_is_refused_by_name_where_the_guest_folds_case() {
        let host = tree(&[
            ("src", dir(10)),
            ("src/Foo.cs", file(Some("aa"), 2, 11)),
            ("src/foo.cs", file(Some("bb"), 2, 12)),
            ("src/other.cs", file(Some("cc"), 2, 13)),
        ]);
        let plan = run_folding(&host);

        assert_eq!(plan.collisions.len(), 1, "{:?}", plan.collisions);
        assert_eq!(
            plan.collisions[0].paths,
            vec!["src/Foo.cs".to_string(), "src/foo.cs".to_string()]
        );
        let said = plan.collisions[0].to_string();
        for path in ["src/Foo.cs", "src/foo.cs"] {
            assert!(said.contains(path), "unnamed: {said}");
        }
        assert!(said.contains("elevated = true"), "no way out: {said}");

        // Neither is transferred — picking one would be exactly the silent
        // overwrite the refusal exists to prevent — and the rest still lands.
        let touched: Vec<&str> = plan.actions.iter().map(Action::path).collect();
        assert_eq!(touched, vec!["src", "src/other.cs"]);
        assert!(!plan.nothing_to_record(), "a refusal is not nothing");
    }

    /// The collision is about the whole path, not just the file name: two
    /// directories differing only in case are one guest directory too.
    #[test]
    fn a_directory_differing_only_in_case_collides_as_well() {
        let host = tree(&[
            ("Src", dir(10)),
            ("src", dir(11)),
            ("Src/a.rs", file(Some("aa"), 2, 12)),
        ]);
        let plan = run_folding(&host);
        assert_eq!(
            plan.collisions
                .iter()
                .map(|c| &c.folded)
                .collect::<Vec<_>>(),
            vec!["src"]
        );
        assert_eq!(
            plan.actions.iter().map(Action::path).collect::<Vec<_>>(),
            vec!["Src/a.rs"],
        );
    }

    /// **The policy, not the fallback.** With the flag set the guest is
    /// genuinely case-sensitive, so both names are ordinary paths and both are
    /// transferred.
    #[test]
    fn a_case_sensitive_guest_carries_both_names_and_refuses_nothing() {
        let host = tree(&[
            ("src", dir(10)),
            ("src/Foo.cs", file(Some("aa"), 2, 11)),
            ("src/foo.cs", file(Some("bb"), 2, 12)),
        ]);
        let plan = run(&host, &BTreeMap::new(), &ledger(&[]));
        assert!(plan.collisions.is_empty());
        assert_eq!(
            plan.actions.iter().map(Action::path).collect::<Vec<_>>(),
            vec!["src", "src/Foo.cs", "src/foo.cs"],
        );
    }

    /// Windows picks the object at creation and cannot infer it from a target
    /// that is not there yet, so the plan decides — from the host tree it
    /// already holds, and **never** by following the link.
    #[test]
    fn a_link_kind_is_decided_from_the_host_tree_rather_than_by_following_it() {
        let host = tree(&[
            ("pkg", dir(1)),
            ("pkg/mod.rs", file(Some("aa"), 2, 1)),
            ("here", link("pkg", 1)),
            ("up", link("./pkg/../pkg", 1)),
            ("at-file", link("pkg/mod.rs", 1)),
            ("nowhere", link("gone", 1)),
            ("outside", link("../elsewhere", 1)),
            ("absolute", link("/usr/lib/foo", 1)),
            ("drive", link("C:\\src\\pkg", 1)),
            // A colon is a legal character in a host-side name, and only a
            // drive letter makes one a Windows absolute path.
            ("colon", dir(1)),
            ("odd:name", dir(1)),
            ("at-odd", link("odd:name", 1)),
        ]);
        let plan = run(&host, &BTreeMap::new(), &ledger(&[]));
        let kinds: BTreeMap<&str, bool> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::PutSymlink { path, dir_link, .. } => Some((path.as_str(), *dir_link)),
                _ => None,
            })
            .collect();
        assert!(kinds["here"], "a link at a directory in the tree");
        assert!(kinds["up"], "resolved as a string, not walked");
        assert!(!kinds["at-file"]);
        assert!(!kinds["nowhere"], "it is going to dangle either way");
        assert!(!kinds["outside"], "the workspace ends at its root");
        assert!(!kinds["absolute"]);
        assert!(!kinds["drive"], "a drive-qualified target is absolute");
        assert!(
            kinds["at-odd"],
            "a colon in a name does not make it a drive"
        );
    }

    /// The seed *is* the first pass: an empty guest tree is the case where
    /// every host path has no counterpart, decided by the same matrix as
    /// everything else.
    #[test]
    fn a_first_pass_against_an_empty_guest_seeds_the_whole_tree() {
        let host = tree(&[
            ("src", dir(10)),
            ("src/main.rs", file(Some("aa"), 12, 11)),
            ("src/lib", link("../lib", 12)),
        ]);
        let plan = run(&host, &BTreeMap::new(), &ledger(&[]));
        assert_eq!(
            plan.actions,
            vec![
                Action::MakeDir {
                    path: "src".into(),
                    host: Side::new(0, 10),
                },
                Action::PutSymlink {
                    path: "src/lib".into(),
                    target: "../lib".into(),
                    host: Side::new(6, 12),
                    digest: "digest-of:../lib".into(),
                    dir_link: false,
                },
                Action::PutFile {
                    path: "src/main.rs".into(),
                    host: Side::new(12, 11),
                    digest: "aa".into(),
                },
            ]
        );
        assert!(plan.conflicts.is_empty());
    }

    /// A directory is created before what goes in it.
    #[test]
    fn parents_are_created_before_their_contents() {
        let host = tree(&[
            ("a/b/c.rs", file(Some("cc"), 1, 1)),
            ("a", dir(1)),
            ("a/b", dir(1)),
        ]);
        let plan = run(&host, &BTreeMap::new(), &ledger(&[]));
        let order: Vec<&str> = plan.actions.iter().map(|a| a.path()).collect();
        assert_eq!(order, vec!["a", "a/b", "a/b/c.rs"]);
    }

    /// The rule the whole ledger exists for: **a missing ledger is not a
    /// decision.** Matching digests are adopted for free and nothing is
    /// pushed over the guest's copy.
    #[test]
    fn a_missing_ledger_adopts_matching_digests_rather_than_seeding_over_them() {
        let host = tree(&[("src/main.rs", file(Some("same"), 12, 900))]);
        let guest = tree(&[("src/main.rs", file(Some("same"), 12, 100))]);
        let plan = run(&host, &guest, &ledger(&[]));
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        assert_eq!(
            plan.adopt,
            vec![(
                "src/main.rs".to_string(),
                agreed(Kind::File, "same", Side::new(12, 900), Side::new(12, 100)),
            )]
        );
    }

    /// The other half of the same rule: differing content with no ledger is
    /// the ordinary conflict path, never a blind host→guest seed. This is the
    /// case that eats a developer's work if it is got wrong.
    #[test]
    fn a_missing_ledger_never_blind_seeds_over_differing_guest_content() {
        let host = tree(&[("src/main.rs", file(Some("host"), 12, 900))]);
        let guest = tree(&[("src/main.rs", file(Some("guest"), 14, 100))]);
        let plan = run(&host, &guest, &ledger(&[]));
        assert!(plan.actions.is_empty());
        assert_eq!(
            plan.conflicts,
            vec![Conflict {
                path: "src/main.rs".into(),
                kind: ConflictKind::BothCreated,
            }]
        );
    }

    /// Digest is the truth and the tuple only a pre-filter: a same-size
    /// in-place write is exactly what the share transports were caught
    /// missing, so a moved mtime alone still ends in a hash comparison.
    #[test]
    fn a_same_size_in_place_write_still_propagates() {
        let host = tree(&[("a.txt", file(Some("new"), 12, 900))]);
        let guest = tree(&[("a.txt", file(None, 12, 50))]);
        let l = ledger(&[(
            "a.txt",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 50)),
        )]);
        let plan = run(&host, &guest, &l);
        assert_eq!(
            plan.actions,
            vec![Action::PutFile {
                path: "a.txt".into(),
                host: Side::new(12, 900),
                digest: "new".into(),
            }]
        );
    }

    /// Each side's mtime is compared only against its own recorded value. A
    /// restored guest resumes with a clock *behind* the host and every file it
    /// holds looks older — which must produce no verdict at all.
    #[test]
    fn a_guest_clock_behind_the_host_decides_nothing() {
        let host = tree(&[("a.txt", file(None, 12, 2_000))]);
        let guest = tree(&[("a.txt", file(None, 12, 1))]);
        let l = ledger(&[(
            "a.txt",
            agreed(Kind::File, "same", Side::new(12, 2_000), Side::new(12, 1)),
        )]);
        assert!(run(&host, &guest, &l).nothing_to_record());
    }

    /// Both sides moved to the same bytes — the common case after a host-side
    /// `git checkout` lands what the guest already had.
    #[test]
    fn both_modified_to_identical_content_is_adopted_not_conflicted() {
        let host = tree(&[("a.txt", file(Some("new"), 20, 900))]);
        let guest = tree(&[("a.txt", file(Some("new"), 20, 800))]);
        let l = ledger(&[(
            "a.txt",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 50)),
        )]);
        let plan = run(&host, &guest, &l);
        assert!(plan.conflicts.is_empty());
        assert!(plan.actions.is_empty());
        assert_eq!(plan.adopt[0].1.digest, "new");
    }

    #[test]
    fn both_modified_to_different_content_is_a_conflict() {
        let host = tree(&[("a.txt", file(Some("h"), 20, 900))]);
        let guest = tree(&[("a.txt", file(Some("g"), 21, 800))]);
        let l = ledger(&[(
            "a.txt",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 50)),
        )]);
        assert_eq!(
            run(&host, &guest, &l).conflicts[0].kind,
            ConflictKind::BothModified
        );
    }

    /// Deletion is unrecoverable and the modification is not yet propagated,
    /// so this is a conflict rather than delete-wins — whichever side deleted.
    #[test]
    fn modified_one_side_and_deleted_the_other_is_a_conflict() {
        let l = ledger(&[(
            "a.txt",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 50)),
        )]);
        let host_modified = run(
            &tree(&[("a.txt", file(Some("h"), 20, 900))]),
            &BTreeMap::new(),
            &l,
        );
        assert_eq!(
            host_modified.conflicts[0].kind,
            ConflictKind::ModifiedAndDeleted
        );
        assert!(host_modified.actions.is_empty());

        let guest_modified = run(
            &BTreeMap::new(),
            &tree(&[("a.txt", file(Some("g"), 20, 900))]),
            &l,
        );
        assert_eq!(
            guest_modified.conflicts[0].kind,
            ConflictKind::ModifiedAndDeleted
        );
        assert!(guest_modified.actions.is_empty());
    }

    #[test]
    fn a_file_replaced_by_a_directory_on_both_sides_is_a_conflict() {
        let host = tree(&[("a", dir(900))]);
        let guest = tree(&[("a", file(Some("g"), 3, 800))]);
        let l = ledger(&[(
            "a",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 50)),
        )]);
        assert_eq!(
            run(&host, &guest, &l).conflicts[0].kind,
            ConflictKind::KindReplaced
        );
    }

    /// One side only: a kind replacement is propagated, clearing what is
    /// there first — the guest cannot rename a directory over a file.
    #[test]
    fn a_one_sided_kind_replacement_clears_before_it_places() {
        let host = tree(&[("a", dir(900))]);
        let guest = tree(&[("a", file(None, 12, 50))]);
        let l = ledger(&[(
            "a",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 50)),
        )]);
        assert_eq!(
            run(&host, &guest, &l).actions,
            vec![
                Action::Remove {
                    path: "a".into(),
                    kind: Kind::File,
                },
                Action::MakeDir {
                    path: "a".into(),
                    host: Side::new(0, 900),
                },
            ]
        );
    }

    /// Host→guest deletes are unguarded — a `git checkout` removing 400 files
    /// just removes them, because the guest copy is the reconstructible one —
    /// and children go before their parents.
    #[test]
    fn a_host_side_delete_removes_children_before_parents() {
        let l = ledger(&[
            (
                "old",
                agreed(Kind::Dir, "", Side::new(0, 1), Side::new(0, 1)),
            ),
            (
                "old/a.txt",
                agreed(Kind::File, "x", Side::new(1, 1), Side::new(1, 1)),
            ),
        ]);
        let guest = tree(&[("old", dir(1)), ("old/a.txt", file(None, 1, 1))]);
        let plan = run(&BTreeMap::new(), &guest, &l);
        assert_eq!(
            plan.actions,
            vec![
                Action::Remove {
                    path: "old/a.txt".into(),
                    kind: Kind::File,
                },
                Action::Remove {
                    path: "old".into(),
                    kind: Kind::Dir,
                },
            ]
        );
    }

    /// Nothing on either side and an entry in the ledger: the agreement has
    /// nothing left to be about.
    #[test]
    fn a_path_gone_from_both_sides_leaves_the_ledger() {
        let l = ledger(&[(
            "a.txt",
            agreed(Kind::File, "x", Side::new(1, 1), Side::new(1, 1)),
        )]);
        let plan = run(&BTreeMap::new(), &BTreeMap::new(), &l);
        assert_eq!(plan.forget, vec!["a.txt".to_string()]);
        assert!(plan.actions.is_empty());
    }

    /// This direction leaves a guest-side change strictly alone — but says so,
    /// because from the developer's side an unpropagated edit is otherwise
    /// nothing happening.
    #[test]
    fn a_guest_only_change_is_left_alone_and_reported() {
        let host = tree(&[("a.txt", file(None, 12, 100))]);
        let guest = tree(&[("a.txt", file(Some("g"), 20, 800))]);
        let l = ledger(&[(
            "a.txt",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 50)),
        )]);
        let plan = run(&host, &guest, &l);
        assert!(plan.actions.is_empty());
        assert_eq!(
            plan.pending_guest,
            vec![PendingGuest {
                path: "a.txt".into(),
                deleted: false,
            }]
        );
    }

    /// The guard is per file and fires before the transfer, so the failure
    /// never depends on unrelated files and the rest of the tree still lands.
    #[test]
    fn the_size_guard_refuses_one_file_and_lets_the_rest_through() {
        let host = tree(&[
            ("big.vhdx", file(Some("b"), CAP + 1, 1)),
            ("small.rs", file(Some("s"), 10, 1)),
        ]);
        let plan = run(&host, &BTreeMap::new(), &ledger(&[]));
        assert_eq!(
            plan.actions,
            vec![Action::PutFile {
                path: "small.rs".into(),
                host: Side::new(10, 1),
                digest: "s".into(),
            }]
        );
        assert_eq!(plan.oversize.len(), 1);
        let refusal = plan.oversize[0].to_string();
        assert!(refusal.contains("big.vhdx"), "{refusal}");
        assert!(refusal.contains(".vmlabignore"), "{refusal}");
        assert!(refusal.contains("workspace_max_file"), "{refusal}");
    }

    /// A symlink's target string is content: it crosses verbatim, is recorded
    /// like any other content, and is never translated for the guest OS.
    #[test]
    fn a_symlink_target_crosses_verbatim() {
        let host = tree(&[("lib", link("/usr/lib/foo", 1))]);
        let plan = run(&host, &BTreeMap::new(), &ledger(&[]));
        assert_eq!(
            plan.actions,
            vec![Action::PutSymlink {
                path: "lib".into(),
                target: "/usr/lib/foo".into(),
                host: Side::new(12, 1),
                digest: "digest-of:/usr/lib/foo".into(),
                dir_link: false,
            }]
        );
    }

    /// A link whose target changed is a content change like any other.
    #[test]
    fn a_retargeted_symlink_is_a_modification() {
        let host = tree(&[("lib", link("../new", 900))]);
        let guest = tree(&[("lib", link("../old", 50))]);
        let l = ledger(&[(
            "lib",
            agreed(
                Kind::Symlink,
                "digest-of:../old",
                Side::new(6, 100),
                Side::new(6, 50),
            ),
        )]);
        assert_eq!(
            run(&host, &guest, &l).actions,
            vec![Action::PutSymlink {
                path: "lib".into(),
                target: "../new".into(),
                host: Side::new(6, 900),
                digest: "digest-of:../new".into(),
                dir_link: false,
            }]
        );
    }

    /// **Leaving scope is free.** A path the developer has just ignored
    /// leaves the ledger and neither copy is touched — it must not read as a
    /// host-side delete and take the guest's copy with it, which is the exact
    /// opposite of what ignoring a path is for.
    #[test]
    fn a_path_that_became_guest_owned_leaves_the_ledger_with_both_copies_intact() {
        let l = ledger(&[(
            "app.log",
            agreed(Kind::File, "x", Side::new(3, 1), Side::new(3, 1)),
        )]);
        // The host scan no longer reports it: it is guest-owned now.
        let plan = reconcile(&Inputs {
            host: &BTreeMap::new(),
            guest: &tree(&[("app.log", file(None, 3, 1))]),
            ledger: &l,
            undecided: &BTreeSet::new(),
            guest_owned: &BTreeSet::from(["app.log".to_string()]),
            max_file_bytes: CAP,
            case_folding: false,
        });
        assert!(plan.actions.is_empty(), "{:?}", plan.actions);
        assert_eq!(plan.forget, vec!["app.log".to_string()]);
        assert!(plan.conflicts.is_empty());
    }

    /// A file over the cap is never hashed, so it must never read as
    /// unchanged either — otherwise the one file the guard exists to refuse
    /// is the one file that passes in silence.
    #[test]
    fn an_unhashed_oversize_file_is_refused_rather_than_read_as_unchanged() {
        let mut big = file(None, CAP + 1, 900);
        big.oversize = true;
        let l = ledger(&[(
            "big.vhdx",
            agreed(Kind::File, "old", Side::new(12, 100), Side::new(12, 100)),
        )]);
        let guest = tree(&[("big.vhdx", file(None, 12, 100))]);
        let plan = run(&tree(&[("big.vhdx", big)]), &guest, &l);
        assert!(plan.actions.is_empty());
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.oversize.len(), 1);
        assert_eq!(plan.oversize[0].path, "big.vhdx");
    }

    /// A path one side could not be read for is left exactly as it is —
    /// never mistaken for absence, which would delete or overwrite it.
    #[test]
    fn an_undecided_path_is_left_strictly_alone() {
        let l = ledger(&[(
            "a.sock",
            agreed(Kind::File, "x", Side::new(1, 1), Side::new(1, 1)),
        )]);
        let plan = reconcile(&Inputs {
            host: &BTreeMap::new(),
            guest: &BTreeMap::new(),
            ledger: &l,
            undecided: &BTreeSet::from(["a.sock".to_string()]),
            guest_owned: &BTreeSet::new(),
            max_file_bytes: CAP,
            case_folding: false,
        });
        assert!(plan.nothing_to_record(), "{plan:?}");
        assert!(plan.forget.is_empty());
    }

    /// The pre-filter decides only whether to hash, and each side asks it
    /// about its own recorded pair.
    #[test]
    fn the_pre_filter_asks_each_side_about_its_own_record() {
        let was = agreed(Kind::File, "x", Side::new(12, 100), Side::new(12, 50));
        assert!(!needs_digest(
            Some(&was),
            Kind::File,
            Side::new(12, 100),
            true
        ));
        assert!(needs_digest(
            Some(&was),
            Kind::File,
            Side::new(12, 50),
            true
        ));
        assert!(!needs_digest(
            Some(&was),
            Kind::File,
            Side::new(12, 50),
            false
        ));
        // Nothing to pre-filter against.
        assert!(needs_digest(None, Kind::File, Side::new(12, 100), true));
        // A directory has no content to hash.
        assert!(!needs_digest(None, Kind::Dir, Side::new(0, 1), true));
        // A kind change is never vouched for by a size and an mtime.
        assert!(needs_digest(
            Some(&was),
            Kind::Symlink,
            Side::new(12, 100),
            true
        ));
    }
}
