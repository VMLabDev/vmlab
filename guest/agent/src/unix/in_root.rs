//! Resolving a container path the way the container sees it (code review
//! F15).
//!
//! A terminal or an exec in a container runs chrooted, so `/` is the
//! container's root filesystem for every component of every path it names.
//! A file session, a `tail` and a `watch` run in the agent instead, and a
//! path that only had the rootfs put in front of it is resolved by the
//! micro-VM's kernel against the micro-VM's root: an absolute symlink inside
//! the container lands outside it, and `..` climbs out of it. `vmlab cp`
//! then reads a different file than `vmlab exec cat`.
//!
//! [`resolve`] walks the path one component at a time instead, as
//! `openat2(RESOLVE_IN_ROOT)` does: a symlink's target is read and resolved
//! against the container's root, `..` stops at that root, and a chain of more
//! than [`MAX_LINKS`] links is a loop. What it hands back names no symlink
//! the kernel could follow out of the root, except the last component when
//! the caller keeps it ([`Last::Keep`]) — that one is the operation's own
//! subject, a link to remove, rename or read.
//!
//! The walk and the operation that follows are two steps, so the container
//! could swap a directory for a symlink between them. The container is the
//! user's own workload, and this is about seeing its files as it does, not a
//! boundary against it: the micro-VM is that boundary.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::mux::Last;

/// Links followed in one resolution before it is a loop: Linux's own limit.
pub const MAX_LINKS: usize = 40;

/// `path`, as the container names it, on the agent's side of `root`.
///
/// Every component but the last is followed when it is a symlink, inside
/// `root`; the last is followed only for [`Last::Follow`]. A component that
/// does not exist yet (a file about to be created) is taken as named.
pub fn resolve(root: &Path, path: &str, last: Last) -> std::io::Result<PathBuf> {
    let mut pending: Vec<OsString> = components(Path::new(path));
    pending.reverse();
    let mut inside: Vec<OsString> = Vec::new();
    let mut links = 0;
    while let Some(component) = pending.pop() {
        if component == ".." {
            // The container's root is its own parent.
            inside.pop();
            continue;
        }
        let is_last = pending.is_empty();
        let candidate = joined(root, &inside).join(&component);
        let follow = !is_last || matches!(last, Last::Follow);
        let target = match follow {
            true => std::fs::read_link(&candidate).ok(),
            false => None,
        };
        let Some(target) = target else {
            inside.push(component);
            continue;
        };
        links += 1;
        if links > MAX_LINKS {
            return Err(std::io::Error::other(format!(
                "{path}: too many levels of symbolic links"
            )));
        }
        if target.is_absolute() {
            inside.clear();
        }
        // The target's components take this one's place, ahead of the rest.
        let mut expanded = components(&target);
        expanded.reverse();
        pending.extend(expanded);
    }
    Ok(joined(root, &inside))
}

/// The named components of `path`: no root, no `.`, `..` kept for the walk.
fn components(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_os_string()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

fn joined(root: &Path, inside: &[OsString]) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in inside {
        debug_assert!(!component.as_bytes().contains(&b'/'));
        path.push(component);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::{Last, MAX_LINKS, resolve};
    use std::os::unix::fs::symlink;
    use std::path::Path;

    /// A micro-VM with a container root inside it. Every name exists twice:
    /// `inner` inside the container, `OUTER` beside it in the micro-VM.
    struct Fixture {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let vm = dir.path();
        let root = vm.join("rootfs");
        for base in [vm, root.as_path()] {
            std::fs::create_dir_all(base.join("etc")).unwrap();
            std::fs::create_dir_all(base.join("srv/app")).unwrap();
        }
        // The same paths, with different contents on each side.
        std::fs::write(vm.join("etc/sentinel"), "OUTER").unwrap();
        std::fs::write(root.join("etc/sentinel"), "inner").unwrap();
        std::fs::write(vm.join("sentinel"), "OUTER").unwrap();
        std::fs::write(root.join("sentinel"), "inner").unwrap();
        // The container's own links, as an image ships them.
        symlink("/etc/sentinel", root.join("srv/absolute")).unwrap();
        symlink("../../etc/sentinel", root.join("srv/app/relative")).unwrap();
        symlink("/etc", root.join("srv/etcdir")).unwrap();
        symlink("../../../../../../../../sentinel", root.join("srv/climb")).unwrap();
        Fixture { _dir: dir, root }
    }

    fn read(f: &Fixture, path: &str) -> String {
        std::fs::read_to_string(resolve(&f.root, path, Last::Follow).unwrap()).unwrap()
    }

    /// The review's test: absolute links, relative links and `..` all name
    /// a file both inside and outside the root, and each resolves inside.
    #[test]
    fn links_and_dotdot_resolve_inside_the_container_root() {
        let f = fixture();
        assert_eq!(read(&f, "/srv/absolute"), "inner", "absolute link");
        assert_eq!(read(&f, "/srv/app/relative"), "inner", "relative link");
        assert_eq!(
            read(&f, "/srv/etcdir/sentinel"),
            "inner",
            "absolute link to a dir"
        );
        assert_eq!(read(&f, "/srv/climb"), "inner", "a link that climbs past /");
        assert_eq!(
            read(&f, "/../../../etc/sentinel"),
            "inner",
            "`..` at the root"
        );
        assert_eq!(
            read(&f, "/srv/../../../sentinel"),
            "inner",
            "`..` past the root"
        );
        assert_eq!(
            read(&f, "srv/./app/../../etc/sentinel"),
            "inner",
            "relative path"
        );
    }

    /// A link that is the operation's subject — removed, renamed, read — is
    /// left as named; the directories above it are still resolved inside.
    #[test]
    fn a_kept_last_component_is_the_link_itself() {
        let f = fixture();
        assert_eq!(
            resolve(&f.root, "/srv/absolute", Last::Keep).unwrap(),
            f.root.join("srv/absolute")
        );
        assert_eq!(
            resolve(&f.root, "/srv/etcdir/sentinel", Last::Keep).unwrap(),
            f.root.join("etc/sentinel")
        );
    }

    /// A file about to be created does not exist yet, and nor may its
    /// parent: both are taken as named.
    #[test]
    fn missing_components_are_taken_as_named() {
        let f = fixture();
        assert_eq!(
            resolve(&f.root, "/srv/etcdir/new/file", Last::Follow).unwrap(),
            f.root.join("etc/new/file")
        );
        assert_eq!(resolve(&f.root, "/", Last::Follow).unwrap(), f.root);
    }

    #[test]
    fn a_link_loop_is_refused() {
        let f = fixture();
        symlink("/srv/b", f.root.join("srv/a")).unwrap();
        symlink("a", f.root.join("srv/b")).unwrap();
        let err = resolve(&f.root, "/srv/a/file", Last::Follow).unwrap_err();
        assert!(err.to_string().contains("too many levels"), "{err}");
        // A long chain inside the limit still resolves.
        let mut previous = "/etc/sentinel".to_string();
        for n in 0..MAX_LINKS {
            let name = format!("/srv/chain{n}");
            symlink(&previous, f.root.join(name.trim_start_matches('/'))).unwrap();
            previous = name;
        }
        assert_eq!(read(&f, &previous), "inner");
        assert!(Path::new(&f.root).is_dir());
    }
}
