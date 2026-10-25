//! Lab instances: one lab file run as separate labs from separate
//! directories, such as two git worktrees of one repository.
//!
//! A lab's name is its host-global runtime identity (ADR-0011), so two
//! directories declaring `lab "demo"` cannot run at once. An instance gives a
//! directory its own name, `<declared>-<instance>`, and every name-keyed
//! resource (registry entry, runtime and state directories, sockets, process
//! markers, generated MACs, guest DNS names) follows from it. The instance is
//! chosen by the CLI, in this order:
//!
//! 1. `--instance <id>` (an empty value means none),
//! 2. `VMLAB_INSTANCE` (set but empty means none),
//! 3. the git worktree the lab directory is in, when that is a linked
//!    worktree: its name under `.git/worktrees/`. The main worktree has none.
//!
//! The lab daemon is told the effective name by the supervisor and adopts it
//! (see [`adopt`]), so it never resolves an instance itself.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::config::LabFile;

/// The environment variable naming the instance.
pub const ENV: &str = "VMLAB_INSTANCE";

/// A lab name is a DNS label.
const LABEL_MAX: usize = 63;

/// Hex characters of the instance's hash kept when the effective name has to
/// be shortened.
const HASH_LEN: usize = 6;

static FLAG: OnceLock<String> = OnceLock::new();

/// Record `--instance` for this process. Called once, from argument parsing.
pub fn set_flag(value: String) {
    let _ = FLAG.set(value);
}

/// Where an instance came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Worktree,
}

/// The instance a lab directory runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub id: String,
    pub source: Source,
}

/// The instance for the lab at `root`, from `--instance`, `VMLAB_INSTANCE`
/// or the git worktree, in that order. `None` runs the lab under its declared
/// name.
pub fn resolve(root: &Path) -> Option<Instance> {
    let env = std::env::var(ENV).ok();
    resolve_with(FLAG.get().map(String::as_str), env.as_deref(), root)
}

fn resolve_with(flag: Option<&str>, env: Option<&str>, root: &Path) -> Option<Instance> {
    let explicit = |id: &str, source| {
        (!id.is_empty()).then(|| Instance {
            id: id.to_string(),
            source,
        })
    };
    if let Some(id) = flag {
        return explicit(id, Source::Flag);
    }
    if let Some(id) = env {
        return explicit(id, Source::Env);
    }
    worktree_name(root).map(|id| Instance {
        id,
        source: Source::Worktree,
    })
}

/// The name of the linked git worktree `root` is in, or `None` in a main
/// worktree, a submodule or outside git.
///
/// A linked worktree's `.git` is a file, `gitdir: <repo>/.git/worktrees/<name>`.
/// A submodule's is a file too, but points under `.git/modules/`. Read
/// directly so the CLI needs no `git` binary.
pub fn worktree_name(root: &Path) -> Option<String> {
    for dir in root.ancestors() {
        let dotgit = dir.join(".git");
        if dotgit.is_dir() {
            return None;
        }
        if dotgit.is_file() {
            let text = std::fs::read_to_string(&dotgit).ok()?;
            let gitdir = PathBuf::from(text.lines().next()?.strip_prefix("gitdir:")?.trim());
            let parent = gitdir.parent()?;
            if parent.file_name()? != "worktrees" {
                return None;
            }
            return gitdir.file_name()?.to_str().map(str::to_string);
        }
    }
    None
}

/// `<declared>-<instance>`, with the instance lowercased and reduced to
/// letters, digits and single hyphens so the result is a DNS label. When that
/// would pass 63 characters the instance is cut short and a 6-character hash
/// of it appended, so two long instances that share a prefix stay distinct.
pub fn effective_name(declared: &str, id: &str) -> Result<String> {
    let mut slug = String::new();
    for c in id.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        bail!("instance `{id}` has no letters or digits to name a lab with");
    }
    let full = format!("{declared}-{slug}");
    if full.len() <= LABEL_MAX {
        return Ok(full);
    }
    // declared, '-', a cut of the slug, '-', the hash.
    let room = LABEL_MAX
        .checked_sub(declared.len() + 2 + HASH_LEN)
        .filter(|&n| n > 0)
        .ok_or_else(|| {
            anyhow!(
                "lab name \"{declared}\" is too long to carry instance `{id}`: \
                 the two together must fit in 63 characters"
            )
        })?;
    let cut = slug[..room].trim_end_matches('-');
    let hash = hex::encode(Sha256::digest(slug.as_bytes()));
    Ok(format!("{declared}-{cut}-{}", &hash[..HASH_LEN]))
}

/// Rename `file`'s lab to the effective name for the instance at `root`.
pub fn apply(file: &mut LabFile, root: &Path) -> Result<()> {
    if let Some(instance) = resolve(root) {
        file.lab.name = effective_name(&file.lab.name, &instance.id)?;
    }
    Ok(())
}

/// Load the lab file at `root` under its effective name: what every CLI verb
/// that names a lab uses.
pub fn load(root: &Path) -> Result<LabFile> {
    let mut file =
        crate::config::load_lab_root(root).map_err(|e| anyhow!("{:?}", miette::Report::new(e)))?;
    apply(&mut file, root)?;
    Ok(file)
}

/// Whether a lab running as `name` comes from a file declaring `declared`:
/// the declared name itself or an instance of it, `<declared>-<instance>`.
pub fn belongs_to(name: &str, declared: &str) -> bool {
    name == declared
        || name
            .strip_prefix(declared)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|rest| !rest.is_empty())
}

/// The lab daemon's side: it is started as `--lab <name>` and runs `file`
/// under that name, which must be the declared one or an instance of it.
pub fn adopt(file: &mut LabFile, name: &str, root: &Path) -> Result<()> {
    let declared = &file.lab.name;
    if !belongs_to(name, declared) {
        bail!(
            "lab file at {} defines \"{declared}\", not \"{name}\"",
            root.display()
        );
    }
    file.lab.name = name.to_string();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_name_joins_and_slugs() {
        assert_eq!(effective_name("demo", "feat-x").unwrap(), "demo-feat-x");
        assert_eq!(
            effective_name("demo", "Feature/Login_Page").unwrap(),
            "demo-feature-login-page"
        );
        assert_eq!(effective_name("demo", "--a..b--").unwrap(), "demo-a-b");
        assert!(effective_name("demo", "///").is_err());
    }

    #[test]
    fn long_instances_stay_labels_and_distinct() {
        let a = effective_name("demo", &format!("{}-one", "x".repeat(80))).unwrap();
        let b = effective_name("demo", &format!("{}-two", "x".repeat(80))).unwrap();
        assert!(a.len() <= LABEL_MAX && b.len() <= LABEL_MAX, "{a} {b}");
        assert_ne!(a, b);
        assert!(a.starts_with("demo-x"));
        assert!(!a.contains("--"));
    }

    #[test]
    fn a_full_length_lab_name_cannot_carry_an_instance() {
        let declared = "d".repeat(62);
        assert!(effective_name(&declared, "x").is_err());
    }

    #[test]
    fn flag_beats_env_beats_worktree_and_empty_means_none() {
        let root = Path::new("/nonexistent");
        let got = resolve_with(Some("a"), Some("b"), root).unwrap();
        assert_eq!((got.id.as_str(), got.source), ("a", Source::Flag));
        let got = resolve_with(None, Some("b"), root).unwrap();
        assert_eq!((got.id.as_str(), got.source), ("b", Source::Env));
        assert_eq!(resolve_with(Some(""), Some("b"), root), None);
        assert_eq!(resolve_with(None, Some(""), root), None);
    }

    #[test]
    fn worktree_name_reads_linked_worktrees_only() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("repo");
        std::fs::create_dir_all(main.join(".git/worktrees/feat-x")).unwrap();
        std::fs::create_dir_all(main.join("lab")).unwrap();
        assert_eq!(worktree_name(&main.join("lab")), None);

        let linked = tmp.path().join("feat-x");
        std::fs::create_dir_all(linked.join("lab")).unwrap();
        std::fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/feat-x").display()),
        )
        .unwrap();
        assert_eq!(
            worktree_name(&linked.join("lab")).as_deref(),
            Some("feat-x")
        );
        let got = resolve_with(None, None, &linked.join("lab")).unwrap();
        assert_eq!(got.source, Source::Worktree);

        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../repo/.git/modules/sub\n").unwrap();
        assert_eq!(worktree_name(&sub), None);
    }

    #[test]
    fn belongs_to_matches_the_declared_name_and_its_instances_only() {
        assert!(belongs_to("demo", "demo"));
        assert!(belongs_to("demo-feat-x", "demo"));
        assert!(!belongs_to("demolition", "demo"));
        assert!(!belongs_to("demo-", "demo"));
        assert!(!belongs_to("dem", "demo"));
    }

    #[test]
    fn adopt_accepts_the_declared_name_or_an_instance_of_it() {
        let mut file = test_file("demo");
        adopt(&mut file, "demo-feat-x", Path::new("/r")).unwrap();
        assert_eq!(file.lab.name, "demo-feat-x");
        let mut file = test_file("demo");
        adopt(&mut file, "demo", Path::new("/r")).unwrap();
        assert!(adopt(&mut test_file("demo"), "demolition", Path::new("/r")).is_err());
        assert!(adopt(&mut test_file("demo"), "demo-", Path::new("/r")).is_err());
        assert!(adopt(&mut test_file("demo"), "other", Path::new("/r")).is_err());
    }

    fn test_file(name: &str) -> LabFile {
        let source = format!("import <vmlab.wcl>\nlab \"{name}\" {{ }}\n");
        crate::config::load_lab_source(&source, "vmlab.wcl", Path::new("/r"))
            .expect("minimal lab file parses")
    }
}
