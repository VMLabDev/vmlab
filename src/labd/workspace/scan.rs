//! What each side is holding — the input the reconciliation reasons over
//! (PRD §19.6).
//!
//! Two walks with one vocabulary. The host's is a real directory descent that
//! reads the ignore rules as it goes; the guest's is a **probe of the paths
//! the host already knows about**, not a tree walk — this direction never asks
//! the guest what it has, only whether it still holds what was agreed. (The
//! full guest stat-walk, which the exception paths need, is its own ticket.)
//!
//! Three rules both sides obey:
//!
//! - **Symlinks are never followed.** Never-follow is the load-bearing half: a
//!   link pointing at `/` that the syncer followed would walk the entire host
//!   filesystem into the guest. The target string is *content*, digested like
//!   any other and never translated across the seam.
//! - **Special files are skipped loudly** — FIFOs, sockets, device nodes. A
//!   build leaving a `.sock` in the tree is normal and must not stop a dev
//!   machine; omitting it silently is the failure mode §19.6 keeps rejecting.
//!   They never enter the ledger, so they cannot produce a phantom conflict.
//! - **A path that cannot be read is a loud, named skip, not a halt**, and it
//!   is left strictly alone — never mistaken for absence, which would seed
//!   straight over whatever is really there.
//!
//! **Digests only for suspects.** A side is hashed only where its own
//! `(size, mtime)` no longer matches its own recorded pair, which is the one
//! thing the pre-filter is allowed to decide.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use futures::stream::StreamExt;

use super::guest::GuestFs;
use super::ignore::{Ignores, join_rel};
use super::ledger::{Kind, Ledger, Side};
use super::plan::{State, needs_digest};

/// How many guest probes ride the channel at once. The session is pipelined
/// and replies may complete out of order, so the window is what turns the
/// round trip from a per-path cost into a one-off.
const PROBE_WINDOW: usize = 32;

/// What a suspect's second round trip answers with: the content digest, plus
/// the link target where it is a symlink (whose target string *is* its
/// content).
type Content = (String, Option<String>);

/// A path neither direction may touch, and why. Always reported: this is the
/// one thing a syncer must never do quietly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub path: String,
    pub why: String,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.why)
    }
}

/// The host tree as it stands.
#[derive(Debug, Default)]
pub struct HostScan {
    /// Every synced path, keyed `/`-separated relative to the workspace root.
    pub tree: BTreeMap<String, State>,
    /// Every directory inside the workspace, `""` for the root — what the
    /// watcher registers on, already pruned of everything guest-owned.
    pub dirs: Vec<String>,
    pub skipped: Vec<Skip>,
}

/// What the guest holds at the paths it was asked about.
#[derive(Debug, Default)]
pub struct GuestProbe {
    pub tree: BTreeMap<String, State>,
    pub skipped: Vec<Skip>,
}

/// Walk the host tree, applying the ignore rules as the descent learns them.
///
/// Blocking, deliberately: it is filesystem work, and the caller runs it off
/// the runtime rather than pretending otherwise.
pub fn host_scan(root: &Path, ledger: &Ledger, max_file_bytes: u64) -> Result<(HostScan, Ignores)> {
    let mut ignores = Ignores::new();
    let mut scan = HostScan::default();
    let mut queue = vec![String::new()];
    while let Some(dir) = queue.pop() {
        ignores.read_dir_rules(root, &dir)?;
        let here = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir)
        };
        let entries = std::fs::read_dir(&here)
            .with_context(|| format!("reading directory {}", here.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading directory {}", here.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = join_rel(&dir, &name);
            let path = entry.path();
            // Never followed, so a link is an entry to record rather than a
            // directory to descend.
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) => {
                    scan.skipped.push(Skip {
                        path: rel,
                        why: format!("host: cannot read it ({e})"),
                    });
                    continue;
                }
            };
            let kind = if meta.is_dir() {
                Kind::Dir
            } else if meta.is_symlink() {
                Kind::Symlink
            } else if meta.is_file() {
                Kind::File
            } else {
                scan.skipped.push(Skip {
                    path: rel,
                    why: "host: not a file, directory or symlink".into(),
                });
                continue;
            };
            if ignores.verdict(&rel, kind == Kind::Dir).is_guest_owned() {
                // Guest-owned, not skipped: the guest is expected to hold its
                // own diverging content here, so neither direction descends.
                continue;
            }
            let side = Side::new(meta.len(), mtime_ns(&meta));
            let mut state = State {
                kind,
                size: side.size,
                mtime_ns: side.mtime_ns,
                digest: None,
                target: None,
                oversize: false,
            };
            if kind == Kind::Symlink {
                match std::fs::read_link(&path) {
                    Ok(target) => {
                        let target = target.to_string_lossy().into_owned();
                        state.size = target.len() as u64;
                        state.digest = Some(digest_of_target(&target));
                        state.target = Some(target);
                    }
                    Err(e) => {
                        scan.skipped.push(Skip {
                            path: rel,
                            why: format!("host: cannot read the link target ({e})"),
                        });
                        continue;
                    }
                }
            } else if needs_digest(ledger.entries.get(&rel), kind, side, true) {
                if side.size > max_file_bytes {
                    // The size guard is going to refuse it, and hashing four
                    // gigabytes to reach that conclusion is the same wasted
                    // work the guard exists to prevent. The plan needs only
                    // the size.
                    state.oversize = true;
                } else {
                    match digest_file(&path) {
                        Ok(digest) => state.digest = Some(digest),
                        Err(e) => {
                            scan.skipped.push(Skip {
                                path: rel,
                                why: format!("host: cannot read it ({e:#})"),
                            });
                            continue;
                        }
                    }
                }
            }
            if kind == Kind::Dir {
                queue.push(rel.clone());
                scan.dirs.push(rel.clone());
            }
            scan.tree.insert(rel, state);
        }
    }
    scan.dirs.push(String::new());
    scan.dirs.sort();
    Ok((scan, ignores))
}

/// Ask the guest what it holds at each of `paths` — the host's tree plus
/// everything the ledger remembers.
///
/// An answer that cannot be got is a named skip rather than an assumption of
/// absence: "nothing is there" and "I could not look" produce opposite
/// actions, and only one of them is recoverable.
pub async fn probe_guest(
    files: &dyn GuestFs,
    guest_root: &str,
    paths: &BTreeSet<String>,
    ledger: &Ledger,
) -> GuestProbe {
    let mut probe = GuestProbe::default();
    let stats: Vec<(String, Result<Option<crate::labd::vm_agent::Attrs>>)> =
        futures::stream::iter(paths.iter().cloned())
            .map(|rel| async move {
                let attrs = files.lstat(&join_guest(guest_root, &rel)).await;
                (rel, attrs)
            })
            .buffered(PROBE_WINDOW)
            .collect()
            .await;

    let mut suspects: Vec<(String, State)> = Vec::new();
    for (rel, attrs) in stats {
        let attrs = match attrs {
            Ok(Some(attrs)) => attrs,
            // Absent is an answer: this direction is about to create it.
            Ok(None) => continue,
            Err(e) => {
                probe.skipped.push(Skip {
                    path: rel,
                    why: format!("guest: cannot read it ({e:#})"),
                });
                continue;
            }
        };
        let Some(kind) = Kind::of(attrs.kind) else {
            probe.skipped.push(Skip {
                path: rel,
                why: "guest: not a file, directory or symlink".into(),
            });
            continue;
        };
        let side = Side::new(attrs.size, attrs.mtime_ns);
        let state = State {
            kind,
            size: side.size,
            mtime_ns: side.mtime_ns,
            digest: None,
            target: None,
            // Host-side only: the guard refuses a *host* file before
            // transferring it, and this direction never transfers out.
            oversize: false,
        };
        if kind == Kind::Symlink || needs_digest(ledger.entries.get(&rel), kind, side, false) {
            suspects.push((rel, state));
        } else {
            probe.tree.insert(rel, state);
        }
    }

    let answered: Vec<(String, State, Result<Content>)> = futures::stream::iter(suspects)
        .map(|(rel, state)| {
            let guest_path = join_guest(guest_root, &rel);
            async move {
                let answer = match state.kind {
                    // A link's target string is its content, so reading it
                    // *is* digesting it — and it is never followed.
                    Kind::Symlink => files
                        .readlink(&guest_path)
                        .await
                        .map(|target| (digest_of_target(&target), Some(target))),
                    _ => files.digest(&guest_path).await.map(|sha256| (sha256, None)),
                };
                (rel, state, answer)
            }
        })
        .buffered(PROBE_WINDOW)
        .collect()
        .await;

    for (rel, mut state, answer) in answered {
        match answer {
            Ok((digest, target)) => {
                if let Some(target) = &target {
                    state.size = target.len() as u64;
                }
                state.digest = Some(digest);
                state.target = target;
                probe.tree.insert(rel, state);
            }
            // The reciprocal of the watch running as the agent identity: a
            // path the login cannot open fails here, and that is a named skip
            // rather than a halt — a build leaving a root-owned artefact in
            // the tree must not stop the dev machine.
            Err(e) => probe.skipped.push(Skip {
                path: rel,
                why: format!("guest: cannot read it ({e:#})"),
            }),
        }
    }
    probe
}

/// A guest path from the workspace root and a `/`-separated relative path.
/// Guest paths join with `/` whatever the guest OS; the agent normalises.
pub fn join_guest(root: &str, rel: &str) -> String {
    if rel.is_empty() {
        return root.to_string();
    }
    format!("{}/{rel}", root.trim_end_matches(['/', '\\']))
}

/// The digest of a symlink, which is the digest of its target string — the
/// same answer on both sides of the seam, because vmlab translates nothing.
pub fn digest_of_target(target: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(target.as_bytes()))
}

/// SHA-256 of a host file, streamed rather than read whole: the size guard
/// runs against the plan, and a scan must not hold a large file in memory to
/// find out it is large.
fn digest_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Modification time in nanoseconds since the Unix epoch, negative before it —
/// the watch vocabulary's spelling, so the two sides' records are the same
/// shape even though they are never compared to each other.
fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    let Ok(mtime) = meta.modified() else {
        return 0;
    };
    match mtime.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let file = dir.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        dir
    }

    /// A cap no test file comes near, so the guard is out of the way except
    /// where a test is about it.
    const NO_CAP: u64 = u64::MAX;

    fn empty_ledger() -> Ledger {
        Ledger::new(Path::new("/lab/src"), "/src")
    }

    #[test]
    fn a_scan_records_every_synced_path_and_the_directories_under_it() {
        let dir = workspace(&[("src/main.rs", "fn main() {}"), ("README.md", "hi")]);
        let (scan, _) = host_scan(dir.path(), &empty_ledger(), NO_CAP).unwrap();
        let paths: Vec<&String> = scan.tree.keys().collect();
        assert_eq!(paths, vec!["README.md", "src", "src/main.rs"]);
        assert_eq!(scan.dirs, vec!["".to_string(), "src".to_string()]);
        assert_eq!(scan.tree["src/main.rs"].size, 12);
        assert!(scan.tree["src/main.rs"].digest.is_some());
    }

    /// Guest-owned means guest-owned: the directory is not descended, so
    /// nothing under it is scanned, digested or watched.
    #[test]
    fn a_guest_owned_directory_is_never_entered() {
        let dir = workspace(&[
            (".gitignore", "node_modules/\n"),
            ("node_modules/pkg/index.js", "x"),
            ("src/main.rs", "y"),
        ]);
        let (scan, _) = host_scan(dir.path(), &empty_ledger(), NO_CAP).unwrap();
        assert!(!scan.tree.contains_key("node_modules"));
        assert!(!scan.tree.contains_key("node_modules/pkg/index.js"));
        assert!(!scan.dirs.iter().any(|d| d.starts_with("node_modules")));
        assert!(scan.tree.contains_key("src/main.rs"));
    }

    /// Never followed. A link at the root pointing at `/` must contribute one
    /// entry, not the entire host filesystem.
    #[test]
    fn a_symlink_is_an_entry_rather_than_a_door() {
        let dir = workspace(&[("src/main.rs", "x")]);
        symlink("/", dir.path().join("everything")).unwrap();
        symlink("../src", dir.path().join("src/self")).unwrap();
        let (scan, _) = host_scan(dir.path(), &empty_ledger(), NO_CAP).unwrap();
        assert_eq!(scan.tree["everything"].kind, Kind::Symlink);
        assert_eq!(scan.tree["everything"].target.as_deref(), Some("/"));
        assert_eq!(scan.tree["everything"].size, 1);
        assert_eq!(scan.tree["src/self"].target.as_deref(), Some("../src"));
        assert!(!scan.tree.keys().any(|p| p.starts_with("everything/")));
    }

    /// A link's target is content, digested the same way on both sides so the
    /// two records can be compared at all.
    #[test]
    fn a_symlink_is_digested_by_its_target_string() {
        let dir = workspace(&[]);
        symlink("/usr/lib/foo", dir.path().join("lib")).unwrap();
        let (scan, _) = host_scan(dir.path(), &empty_ledger(), NO_CAP).unwrap();
        assert_eq!(
            scan.tree["lib"].digest.as_deref(),
            Some(digest_of_target("/usr/lib/foo").as_str())
        );
    }

    /// A build leaving a socket in the tree is normal; it must not stop a dev
    /// machine, and it must not vanish without a word.
    #[test]
    fn a_special_file_is_skipped_loudly_and_never_recorded() {
        let dir = workspace(&[("src/main.rs", "x")]);
        let fifo = dir.path().join("build.sock");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let (scan, _) = host_scan(dir.path(), &empty_ledger(), NO_CAP).unwrap();
        assert!(!scan.tree.contains_key("build.sock"));
        assert_eq!(scan.skipped.len(), 1);
        assert_eq!(scan.skipped[0].path, "build.sock");
        assert!(scan.skipped[0].to_string().contains("build.sock"));
        assert!(scan.tree.contains_key("src/main.rs"));
    }

    /// The pre-filter's whole job: a path whose own recorded pair still
    /// matches is not hashed, and one whose does not is.
    #[test]
    fn only_suspects_are_digested() {
        let dir = workspace(&[("a.txt", "aaa"), ("b.txt", "bbb")]);
        let mut ledger = empty_ledger();
        let meta = std::fs::symlink_metadata(dir.path().join("a.txt")).unwrap();
        ledger.entries.insert(
            "a.txt".into(),
            super::super::ledger::Agreed {
                kind: Kind::File,
                digest: "recorded".into(),
                host: Side::new(meta.len(), mtime_ns(&meta)),
                guest: Side::new(meta.len(), 1),
            },
        );
        let (scan, _) = host_scan(dir.path(), &ledger, NO_CAP).unwrap();
        assert!(scan.tree["a.txt"].digest.is_none(), "vouched for");
        assert!(scan.tree["b.txt"].digest.is_some(), "never agreed");
    }

    /// A file over the cap is recorded, but not hashed: the guard refuses it
    /// before the transfer, and streaming it through SHA-256 first would
    /// spend the ten minutes the guard exists to save.
    #[test]
    fn a_file_over_the_cap_is_never_hashed() {
        let dir = workspace(&[("big.vhdx", "0123456789"), ("small.rs", "x")]);
        let (scan, _) = host_scan(dir.path(), &empty_ledger(), 4).unwrap();
        assert!(scan.tree["big.vhdx"].oversize);
        assert!(scan.tree["big.vhdx"].digest.is_none());
        assert!(!scan.tree["small.rs"].oversize);
        assert!(scan.tree["small.rs"].digest.is_some());
    }

    #[test]
    fn guest_paths_join_with_a_forward_slash_whatever_the_root() {
        assert_eq!(join_guest("/src", "a/b.rs"), "/src/a/b.rs");
        assert_eq!(join_guest("/src/", "a"), "/src/a");
        assert_eq!(join_guest("C:\\src", "a/b.rs"), "C:\\src/a/b.rs");
        assert_eq!(join_guest("/src", ""), "/src");
    }
}
