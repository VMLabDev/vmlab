//! What the workspace syncs, and what it leaves to the guest (PRD §19.6).
//!
//! **Ignore rules are repo-tree-first and not a WCL argument**: a built-in
//! floor, then the repo's `.gitignore`, then `.vmlabignore` for the delta
//! including negations. `.gitignore` is the right default source because what
//! you do not want to sync is almost exactly what you do not commit — both are
//! "reconstructible, large, or machine-specific" — and `.vmlabignore` covers
//! where *almost* fails, since a gitignored file you do want guest-side
//! (`.env`, a local cert, `appsettings.Development.json`) needs a `!`
//! negation or the app will not start and the reason will be invisible.
//!
//! An ignored path is not *skipped*, it is **guest-owned**: `node_modules` is
//! the proving case, where the guest runs its own install and holds
//! guest-native binaries, diverging permanently and on purpose. Neither
//! direction ever touches one, which is why the answer is spelled
//! [`Verdict::GuestOwned`] rather than "excluded".
//!
//! ### Why the rules are read during the descent
//!
//! A `.gitignore` applies to its own directory and everything below it, so the
//! rules for a directory are not known until it has been entered — and whether
//! it should be entered at all is decided by the rules above it. That is
//! git's own model, and it is why this is a stack fed by the walk
//! ([`Ignores::read_dir_rules`]) rather than a matcher built once up front.
//!
//! Precedence, deepest first, is git's: the closest ignore file wins, and
//! within one directory `.vmlabignore` beats `.gitignore` because it exists to
//! be the delta. The floor beats both — a negation must not be able to make
//! the syncer's own scratch files into sync objects. **A path under an ignored
//! directory stays ignored**, exactly as `git` has it: a negation cannot
//! re-include a file whose parent directory is gone from the set.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The developer's delta over `.gitignore`, including `!` negations.
pub const VMLAB_IGNORE: &str = ".vmlabignore";
/// The repo's own rules, the default source of the ignore set.
pub const GIT_IGNORE: &str = ".gitignore";

/// Prefix of the temp name every apply writes before renaming it into place
/// (§19.6). In the floor so the temp never becomes a sync object itself.
pub const TEMP_PREFIX: &str = ".vmlab-sync.";

/// vmlab's own rules, which no repo rule may override.
///
/// The `.vmlab-sync*` glob covers both scratch names the syncer can leave in
/// a workspace: an apply's temp ([`TEMP_PREFIX`]), and the halt marker the
/// conflict policy writes at the workspace root. The marker is in the floor
/// from the start rather than when it is first written, because a signal file
/// that syncs itself into the guest is worse than no signal at all.
///
/// `*.lock` under `.git` is here for the reason §19.6 gives: a lock file is
/// one side's transient claim on the mutable git set and means nothing on the
/// other. The *deferral* the rest of that set needs — hold off while a lock is
/// held — is timing rather than an ignore rule, and lands with the conflict
/// halt.
const FLOOR: &[&str] = &[".vmlab-sync*", ".git/**/*.lock"];

/// What the rules say about one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// In the workspace: the two sides are kept in step.
    Synced,
    /// The guest's own, permanently and on purpose. Never touched in either
    /// direction, and never entered in the ledger.
    GuestOwned,
}

impl Verdict {
    pub fn is_guest_owned(self) -> bool {
        self == Verdict::GuestOwned
    }
}

/// One directory's ignore files, in the order they are consulted.
#[derive(Debug, Default)]
struct DirRules {
    /// `.vmlabignore` — the delta, so it is asked first.
    vmlab: Option<Gitignore>,
    git: Option<Gitignore>,
}

/// The layered ignore set for one workspace, grown one directory at a time by
/// the walk that consults it.
#[derive(Debug)]
pub struct Ignores {
    floor: Gitignore,
    /// Keyed by the directory's `/`-separated path relative to the workspace
    /// root; `""` is the root itself. Only directories that actually carry an
    /// ignore file appear.
    dirs: BTreeMap<String, DirRules>,
    /// Every rule file's bytes, in path order — hashed into
    /// [`Ignores::digest`].
    sources: Vec<(String, Vec<u8>)>,
    /// What the rules say about *directory prefixes*, as opposed to about
    /// paths — the input to [`Ignores::prune_list`].
    prune: PruneCandidates,
}

/// The raw material of the prune list: which prefixes the rules exclude, and
/// where a negation names something inside one.
#[derive(Debug, Default)]
struct PruneCandidates {
    /// Root-relative directory prefixes an ignore rule names literally.
    excluded: BTreeSet<String>,
    /// The literal head of every **anchored** negation — the part before its
    /// first wildcard. An unanchored one (`!.env`) contributes nothing, for
    /// the reason [`Ignores::prune_list`] gives.
    negated: BTreeSet<String>,
}

impl Ignores {
    /// The floor alone. Repo rules arrive through [`read_dir_rules`] as the
    /// walk descends.
    ///
    /// [`read_dir_rules`]: Ignores::read_dir_rules
    pub fn new() -> Ignores {
        let mut builder = GitignoreBuilder::new("");
        for pattern in FLOOR {
            builder
                .add_line(None, pattern)
                .expect("the built-in floor is a valid ignore pattern");
        }
        Ignores {
            floor: builder.build().expect("the built-in floor builds"),
            dirs: BTreeMap::new(),
            sources: Vec::new(),
            prune: PruneCandidates::default(),
        }
    }

    /// Read `dir`'s own ignore files, if it has any. Called on entering a
    /// directory, before anything in it is classified.
    ///
    /// `rel` is the directory's path relative to the workspace root, `""` for
    /// the root. An unreadable rule file is an error rather than an empty
    /// rule set: silently syncing a `node_modules` because its rule file could
    /// not be opened is the failure this section keeps refusing.
    pub fn read_dir_rules(&mut self, root: &Path, rel: &str) -> Result<()> {
        let dir = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        let mut rules = DirRules::default();
        for (name, slot) in [
            (VMLAB_IGNORE, &mut rules.vmlab),
            (GIT_IGNORE, &mut rules.git),
        ] {
            let file = dir.join(name);
            let source = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).context(format!("reading {}", file.display())),
            };
            let mut builder = GitignoreBuilder::new(&dir);
            for line in String::from_utf8_lossy(&source).lines() {
                builder
                    .add_line(Some(file.clone()), line)
                    .with_context(|| format!("in {}", file.display()))?;
                self.prune.read_line(rel, line);
            }
            *slot = Some(
                builder
                    .build()
                    .with_context(|| format!("in {}", file.display()))?,
            );
            self.sources.push((join_rel(rel, name), source));
        }
        if rules.vmlab.is_some() || rules.git.is_some() {
            self.dirs.insert(rel.to_string(), rules);
        }
        Ok(())
    }

    /// What the rules say about `rel` — a `/`-separated path relative to the
    /// workspace root.
    ///
    /// A path whose parent directory is guest-owned is guest-owned, whatever
    /// its own rules say: git does not let a negation re-include a file under
    /// an excluded directory, and neither does this. Callers walking the tree
    /// never reach that case (they stop descending), but callers holding a
    /// path from a watcher event do.
    pub fn verdict(&self, rel: &str, is_dir: bool) -> Verdict {
        let mut cut = 0;
        while let Some(next) = rel[cut..].find('/') {
            cut += next;
            if self.decide(&rel[..cut], true) == Some(Verdict::GuestOwned) {
                return Verdict::GuestOwned;
            }
            cut += 1;
        }
        self.decide(rel, is_dir).unwrap_or(Verdict::Synced)
    }

    /// The closest rule that has an opinion about `rel` itself, ignoring its
    /// ancestors. `None` where no rule mentions it.
    fn decide(&self, rel: &str, is_dir: bool) -> Option<Verdict> {
        // The floor is vmlab's, so no repo rule and no negation can reach it.
        if matches!(self.floor.matched(rel, is_dir), ignore::Match::Ignore(_)) {
            return Some(Verdict::GuestOwned);
        }
        // Deepest directory first: the closest ignore file wins, and inside
        // one directory `.vmlabignore` is the delta over `.gitignore`.
        let mut dir = parent_of(rel);
        loop {
            if let Some(rules) = self.dirs.get(dir) {
                let below = relative_to(dir, rel);
                for matcher in [rules.vmlab.as_ref(), rules.git.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    match matcher.matched(below, is_dir) {
                        ignore::Match::Ignore(_) => return Some(Verdict::GuestOwned),
                        ignore::Match::Whitelist(_) => return Some(Verdict::Synced),
                        ignore::Match::None => {}
                    }
                }
            }
            if dir.is_empty() {
                return None;
            }
            dir = parent_of(dir);
        }
    }

    /// The **prune list** the guest's watcher is handed (§19.6): root-relative
    /// directory prefixes to register no watcher under.
    ///
    /// **Registration is a different act from filtering.** Filtering stays
    /// here, host-side, whole — the guest is never asked to decide, for a file
    /// it created itself, whether that file is in the synced set. What forces
    /// pruning is a resource fact instead: `inotify` costs one watch
    /// descriptor per directory where `ReadDirectoryChangesW` is a single
    /// recursive handle, `max_user_watches` defaults to 8192, and a
    /// `node_modules` tree is routinely tens of thousands of directories — so
    /// an unpruned registration is *silently incomplete* on Linux, the exact
    /// failure class that disqualified the share transports.
    ///
    /// Two sources, because the guest holds directories the host has never
    /// seen — `node_modules` is guest-owned precisely so the guest can run its
    /// own install into it:
    ///
    /// - the **rules**, which name a prefix whether or not anything is there
    ///   host-side. This is the one that matters: it is what covers a
    ///   dependency tree that exists only in the guest.
    /// - `discovered`, the directories a host walk declined to enter, which
    ///   catches what a rule matches at a depth its literal text does not
    ///   name.
    ///
    /// Only *literal* prefixes are derived. A wildcard rule (`*.log`) names no
    /// directory, and an unanchored one (`target`) is taken at the rule file's
    /// own level: under-pruning costs watch descriptors on a tree that is
    /// filtered host-side anyway, where over-pruning would silently stop
    /// syncing a subtree, and only one of those two is recoverable.
    ///
    /// Two things take a candidate back off the list, and both are the same
    /// refusal to let a prefix answer a question a rule set answers:
    ///
    /// - **the rules do not actually exclude it.** Every candidate is put back
    ///   through [`verdict`](Ignores::verdict), so a path a later rule file
    ///   negates (`.env`, the case §19.6 names) is never pruned. Pruning it
    ///   would stop the guest reporting it at all — silently, on the one path
    ///   the developer added a negation to get.
    /// - **a negation reaches below it.** Only an *anchored* one counts:
    ///   `!node_modules/.bin/tool` names something inside the excluded tree,
    ///   where `!.env` cannot re-include anything under an excluded directory
    ///   — git's rule, and `verdict`'s. Reading the unanchored form as
    ///   reaching everywhere would disable pruning for the exact repo §19.6
    ///   names as the common case.
    pub fn prune_list(&self, discovered: &[String]) -> Vec<String> {
        let mut prefixes: BTreeSet<&str> = self
            .prune
            .excluded
            .iter()
            .chain(discovered.iter())
            .filter(|p| !p.is_empty())
            .map(String::as_str)
            .collect();
        // The rules decide, not the pattern text: a prefix derived from a
        // rule some other file negates is not excluded at all.
        prefixes.retain(|prefix| self.verdict(prefix, true).is_guest_owned());
        // A negation inside one: the guest is handed prefixes, not rules, and
        // a prefix cannot carry "except this". Keeping the watcher on costs
        // descriptors; taking it off would stop a path syncing.
        prefixes.retain(|prefix| {
            !self
                .prune
                .negated
                .iter()
                .any(|negated| at_or_below(negated, prefix))
        });
        // Shortest wins: `a` covers `a/b`, and sending both is the guest
        // matching a prefix twice for nothing.
        prefixes
            .iter()
            .filter(|prefix| {
                !prefixes
                    .iter()
                    .any(|other| other != *prefix && at_or_below(prefix, other))
            })
            .map(|prefix| (*prefix).to_string())
            .collect()
    }

    /// The rules' own digest, which the ledger carries so a halt can say
    /// *these conflict because you just changed the rules* (§19.6).
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut sorted: Vec<&(String, Vec<u8>)> = self.sources.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut hasher = Sha256::new();
        for (path, source) in sorted {
            hasher.update((path.len() as u64).to_le_bytes());
            hasher.update(path.as_bytes());
            hasher.update((source.len() as u64).to_le_bytes());
            hasher.update(source);
        }
        hex::encode(hasher.finalize())
    }
}

impl Default for Ignores {
    fn default() -> Self {
        Ignores::new()
    }
}

impl PruneCandidates {
    /// Take one line of one rule file, from the directory `dir` holds it in.
    ///
    /// Deliberately a *second* reading of the same text rather than something
    /// asked of the matcher: a matcher answers "is this path ignored", and
    /// what the prune list needs is "which directory does this rule name",
    /// which is a property of the pattern and not of any path.
    fn read_line(&mut self, dir: &str, line: &str) {
        let line = line.trim_end();
        let pattern = line.trim_start();
        if pattern.is_empty() || pattern.starts_with('#') {
            return;
        }
        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        // A trailing `/` is git's "directories only", which is the same
        // prefix either way.
        let pattern = pattern.trim_end_matches('/');
        // A leading `/` anchors to the rule file's own directory, which is
        // where every pattern with an inner separator is anchored anyway.
        let anchored = pattern.starts_with('/') || pattern.trim_end_matches('/').contains('/');
        let pattern = pattern.trim_start_matches('/');
        if pattern.is_empty() || pattern.contains("..") {
            return;
        }
        if negated {
            // Only the literal head: `!build/*.env` still names `build`, and
            // that is enough to keep the watcher on it.
            if !anchored {
                return;
            }
            let head: Vec<&str> = pattern
                .split('/')
                .take_while(|part| !is_glob(part))
                .collect();
            if !head.is_empty() {
                self.negated.insert(join_rel(dir, &head.join("/")));
            }
            return;
        }
        // An exclusion has to name a directory outright to become a prefix:
        // a wildcard segment could match anything at that level, and pruning
        // on a guess would stop a synced subtree without a word.
        if pattern.split('/').any(is_glob) {
            return;
        }
        self.excluded.insert(join_rel(dir, pattern));
    }
}

/// Whether a glob metacharacter makes this path segment unpredictable.
fn is_glob(part: &str) -> bool {
    part.contains(['*', '?', '[', ']', '\\'])
}

/// Whether `path` is `prefix` itself or lies under it — the same reading the
/// agent's own prune match uses, so the two sides never disagree about what a
/// prefix covers.
fn at_or_below(path: &str, prefix: &str) -> bool {
    path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

/// `a/b/c` → `a/b`; a path with no separator → `""` (the root).
fn parent_of(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(cut) => &rel[..cut],
        None => "",
    }
}

/// `rel` re-expressed against `dir`, which must be `rel`'s ancestor.
fn relative_to<'a>(dir: &str, rel: &'a str) -> &'a str {
    if dir.is_empty() {
        rel
    } else {
        &rel[dir.len() + 1..]
    }
}

/// `a/b` from `a` and `b`, with `""` meaning the root.
pub fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out a tree of ignore files and read every one of them, in the
    /// order a descent would.
    fn ignores(root: &Path, files: &[(&str, &str)]) -> Ignores {
        for (path, body) in files {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        let mut dirs: Vec<String> = files
            .iter()
            .map(|(path, _)| parent_of(&path.replace('\\', "/")).to_string())
            .collect();
        dirs.push(String::new());
        dirs.sort();
        dirs.dedup();
        let mut set = Ignores::new();
        for dir in dirs {
            set.read_dir_rules(root, &dir).unwrap();
        }
        set
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_repo_with_no_rules_syncs_everything() {
        let dir = tmp();
        let set = ignores(dir.path(), &[]);
        assert_eq!(set.verdict("src/main.rs", false), Verdict::Synced);
        assert_eq!(set.verdict("node_modules", true), Verdict::Synced);
    }

    /// The repo's own `.gitignore` is the default source: what you do not
    /// commit is almost exactly what you do not sync.
    #[test]
    fn gitignore_is_the_default_source() {
        let dir = tmp();
        let set = ignores(dir.path(), &[(".gitignore", "target/\n*.log\n")]);
        assert_eq!(set.verdict("target", true), Verdict::GuestOwned);
        assert_eq!(set.verdict("debug.log", false), Verdict::GuestOwned);
        assert_eq!(set.verdict("src/main.rs", false), Verdict::Synced);
    }

    /// Where *almost* fails. A gitignored file the app needs guest-side comes
    /// back with a `!` in `.vmlabignore` — and it has to beat `.gitignore`,
    /// or the app does not start and the reason is invisible.
    #[test]
    fn vmlabignore_negates_what_gitignore_excluded() {
        let dir = tmp();
        let set = ignores(
            dir.path(),
            &[(".gitignore", ".env\n*.pem\n"), (".vmlabignore", "!.env\n")],
        );
        assert_eq!(set.verdict(".env", false), Verdict::Synced);
        assert_eq!(set.verdict("local.pem", false), Verdict::GuestOwned);
    }

    /// `.vmlabignore` is a delta in both directions: it adds rules of its own
    /// as well as taking them away.
    #[test]
    fn vmlabignore_adds_rules_of_its_own() {
        let dir = tmp();
        let set = ignores(dir.path(), &[(".vmlabignore", "*.iso\n")]);
        assert_eq!(set.verdict("images/win.iso", false), Verdict::GuestOwned);
    }

    /// git's rule, kept: a negation cannot re-include a file whose parent
    /// directory left the set. The whole subtree is guest-owned, which is the
    /// point — the guest holds its own diverging content there.
    #[test]
    fn nothing_under_a_guest_owned_directory_comes_back() {
        let dir = tmp();
        let set = ignores(
            dir.path(),
            &[
                (".gitignore", "node_modules/\n"),
                (".vmlabignore", "!node_modules/keep.js\n"),
            ],
        );
        assert_eq!(set.verdict("node_modules", true), Verdict::GuestOwned);
        assert_eq!(
            set.verdict("node_modules/keep.js", false),
            Verdict::GuestOwned
        );
        assert_eq!(
            set.verdict("node_modules/pkg/index.js", false),
            Verdict::GuestOwned
        );
    }

    /// A nested `.gitignore` governs its own subtree, and the closest one
    /// wins — the same layering git applies, because the rules are read as
    /// the walk descends.
    #[test]
    fn the_closest_rule_file_wins() {
        let dir = tmp();
        let set = ignores(
            dir.path(),
            &[
                (".gitignore", "*.txt\n"),
                ("docs/.gitignore", "!*.txt\n"),
                ("docs/.vmlabignore", "draft.txt\n"),
            ],
        );
        assert_eq!(set.verdict("readme.txt", false), Verdict::GuestOwned);
        assert_eq!(set.verdict("docs/readme.txt", false), Verdict::Synced);
        assert_eq!(set.verdict("docs/draft.txt", false), Verdict::GuestOwned);
    }

    /// A directory-only rule (`build/`) says nothing about a file of the same
    /// name, which is what the kind argument is for.
    #[test]
    fn a_directory_rule_does_not_match_a_file_of_that_name() {
        let dir = tmp();
        let set = ignores(dir.path(), &[(".gitignore", "build/\n")]);
        assert_eq!(set.verdict("build", true), Verdict::GuestOwned);
        assert_eq!(set.verdict("build", false), Verdict::Synced);
    }

    /// The floor is vmlab's own and no repo rule reaches it: an apply's temp
    /// name must never become a sync object, whatever the developer writes.
    #[test]
    fn the_floor_outranks_every_repo_rule() {
        let dir = tmp();
        let set = ignores(dir.path(), &[(".vmlabignore", "!.vmlab-sync*\n")]);
        assert_eq!(
            set.verdict(&format!("{TEMP_PREFIX}a1b2c3"), false),
            Verdict::GuestOwned
        );
        assert_eq!(
            set.verdict(&format!("src/{TEMP_PREFIX}a1b2c3"), false),
            Verdict::GuestOwned
        );
        assert_eq!(set.verdict(".vmlab-sync-halt", false), Verdict::GuestOwned);
    }

    /// A lock file is one side's transient claim on the mutable git set and
    /// means nothing on the other; everything else in `.git` syncs.
    #[test]
    fn git_lock_files_never_sync_but_the_rest_of_git_does() {
        let dir = tmp();
        let set = ignores(dir.path(), &[]);
        assert_eq!(set.verdict(".git/index.lock", false), Verdict::GuestOwned);
        assert_eq!(
            set.verdict(".git/refs/heads/main.lock", false),
            Verdict::GuestOwned
        );
        assert_eq!(set.verdict(".git/index", false), Verdict::Synced);
        assert_eq!(set.verdict(".git/HEAD", false), Verdict::Synced);
    }

    /// The prune list comes from the **rules**, not from the host tree —
    /// which is the whole point of it. `node_modules` is guest-owned so the
    /// guest can run its own install into it, so the tens of thousands of
    /// directories that would exhaust `max_user_watches` exist *only*
    /// guest-side and no host walk can ever discover them.
    #[test]
    fn the_prune_list_names_a_directory_the_host_has_never_seen() {
        let dir = tmp();
        let set = ignores(
            dir.path(),
            &[(".gitignore", "node_modules/\ntarget/\n*.log\n")],
        );
        assert_eq!(
            set.prune_list(&[]),
            vec!["node_modules".to_string(), "target".to_string()]
        );
    }

    /// A wildcard rule names no directory, so it contributes no prefix: the
    /// guest matches literal prefixes, and guessing which directory `build-*`
    /// meant would take a watcher off a subtree that still syncs.
    #[test]
    fn a_wildcard_rule_contributes_no_prefix() {
        let dir = tmp();
        let set = ignores(dir.path(), &[(".gitignore", "*.log\nbuild-*/\n**/tmp\n")]);
        assert!(set.prune_list(&[]).is_empty());
    }

    /// A rule in a nested rule file is a prefix from that file's own
    /// directory, exactly as the rule itself is.
    #[test]
    fn a_nested_rule_file_names_prefixes_under_itself() {
        let dir = tmp();
        let set = ignores(
            dir.path(),
            &[(".gitignore", "target/\n"), ("web/.gitignore", "dist/\n")],
        );
        assert_eq!(
            set.prune_list(&[]),
            vec!["target".to_string(), "web/dist".to_string()]
        );
    }

    /// An **anchored** negation naming something inside an excluded tree
    /// takes it off the list: a prefix is all the guest gets, and a prefix
    /// cannot say "except this".
    #[test]
    fn a_negation_reaching_below_a_prefix_keeps_the_watcher_on() {
        let dir = tmp();
        let set = ignores(
            dir.path(),
            &[
                (".gitignore", "node_modules/\ntarget/\n"),
                (".vmlabignore", "!node_modules/.bin/tool\n"),
            ],
        );
        assert_eq!(set.prune_list(&[]), vec!["target".to_string()]);
    }

    /// The §19.6 common case, which must not disable pruning: `!.env` is
    /// git's ordinary "this file is not ignored" and cannot re-include
    /// anything under an excluded directory — the rule `verdict` already
    /// holds — so it reaches below nothing.
    #[test]
    fn an_unanchored_negation_does_not_disable_pruning() {
        let dir = tmp();
        let set = ignores(
            dir.path(),
            &[
                (".gitignore", "node_modules/\n.env\n"),
                (".vmlabignore", "!.env\n"),
            ],
        );
        assert_eq!(set.prune_list(&[]), vec!["node_modules".to_string()]);
        assert_eq!(set.verdict(".env", false), Verdict::Synced);
        assert_eq!(
            set.verdict("node_modules/.env", false),
            Verdict::GuestOwned,
            "a negation cannot reach under an excluded directory"
        );
    }

    /// What a host walk declined to enter joins the list, which is how a rule
    /// that matches at a depth its own text does not name still prunes.
    #[test]
    fn what_the_host_walk_declined_to_enter_joins_the_list() {
        let dir = tmp();
        let set = ignores(dir.path(), &[(".gitignore", "target/\n")]);
        assert_eq!(
            set.prune_list(&["crates/api/target".to_string()]),
            vec!["crates/api/target".to_string(), "target".to_string()]
        );
    }

    /// The shortest prefix covers the rest: sending both is the guest
    /// matching twice to reach the same answer.
    #[test]
    fn a_prefix_under_another_prefix_is_dropped() {
        let dir = tmp();
        let set = ignores(dir.path(), &[(".gitignore", "target/\ntarget/debug/\n")]);
        assert_eq!(set.prune_list(&[]), vec!["target".to_string()]);
    }

    /// The rules' digest is part of the ledger, so it has to move when they
    /// do — and stay put when nothing did.
    #[test]
    fn the_rules_digest_tracks_the_rules() {
        let a = tmp();
        let b = tmp();
        let c = tmp();
        let first = ignores(a.path(), &[(".gitignore", "target/\n")]);
        let same = ignores(b.path(), &[(".gitignore", "target/\n")]);
        let changed = ignores(c.path(), &[(".gitignore", "target/\nbuild/\n")]);
        assert_eq!(first.digest(), same.digest());
        assert_ne!(first.digest(), changed.digest());
    }

    /// An unreadable rule file fails loudly. Treating it as "no rules" would
    /// quietly pull a whole dependency tree across the seam.
    #[test]
    fn an_unreadable_rule_file_is_an_error() {
        let dir = tmp();
        let mut set = Ignores::new();
        std::fs::create_dir(dir.path().join(GIT_IGNORE)).unwrap();
        let err = set.read_dir_rules(dir.path(), "").unwrap_err();
        assert!(format!("{err:#}").contains(GIT_IGNORE), "{err:#}");
    }

    #[test]
    fn rel_paths_split_and_join_around_the_root() {
        assert_eq!(parent_of("a/b/c"), "a/b");
        assert_eq!(parent_of("a"), "");
        assert_eq!(relative_to("", "a/b"), "a/b");
        assert_eq!(relative_to("a", "a/b"), "b");
        assert_eq!(join_rel("", "a"), "a");
        assert_eq!(join_rel("a", "b"), "a/b");
    }
}
