//! The guest half of the workspace, as the syncer needs it (PRD §19.6).
//!
//! One trait over the file RPC session, for two reasons. It is the **whole**
//! of what crosses the seam — every guest-side effect the syncer can have is
//! one of the calls below, so "what does the syncer do to a guest" is
//! answered by reading one page. And it is the seam the reconciliation's
//! decisions are executed through, which is what lets the rules that matter —
//! temp-then-rename, the ledger written only after the rename, resume by
//! re-transfer — be tested as behaviour rather than inspected as code.
//!
//! The session it runs over is opened with the machine's **default login**
//! (§19.2): the syncer is the one named exception to vmlab's machinery running
//! as the agent identity, because otherwise the developer owns none of their
//! own source tree.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

use crate::labd::vm_agent::{Attrs, FileOps, LinkKind};

/// Everything the syncer can do to a guest tree.
#[async_trait]
pub trait GuestFs: Send + Sync {
    /// What is at `path` itself, never followed. `None` where nothing is.
    async fn lstat(&self, path: &str) -> Result<Option<Attrs>>;
    /// A symlink's target string, verbatim and untranslated.
    async fn readlink(&self, path: &str) -> Result<String>;
    /// The guest's own SHA-256 of what is on its disk.
    async fn digest(&self, path: &str) -> Result<String>;
    /// Create a directory, treating an existing one as success.
    async fn mkdir(&self, path: &str, case_sensitive: bool) -> Result<()>;
    /// Send a host file's bytes to `remote`, verified against the guest's own
    /// digest of what landed.
    async fn push(&self, local: &Path, remote: &str) -> Result<()>;
    async fn symlink(&self, target: &str, link: &str, kind: LinkKind) -> Result<()>;
    /// Rename, overwriting the destination. The atomic half of every apply.
    async fn rename(&self, from: &str, to: &str) -> Result<()>;
    /// Remove a file or symlink; already-absent is success.
    async fn remove(&self, path: &str) -> Result<()>;
    /// Remove an empty directory; already-absent is success.
    async fn rmdir(&self, path: &str) -> Result<()>;
}

#[async_trait]
impl GuestFs for FileOps {
    async fn lstat(&self, path: &str) -> Result<Option<Attrs>> {
        FileOps::lstat(self, path).await
    }

    async fn readlink(&self, path: &str) -> Result<String> {
        FileOps::readlink(self, path).await
    }

    async fn digest(&self, path: &str) -> Result<String> {
        FileOps::digest(self, path).await.map(|(sha256, _)| sha256)
    }

    async fn mkdir(&self, path: &str, case_sensitive: bool) -> Result<()> {
        FileOps::mkdir(self, path, case_sensitive).await
    }

    async fn push(&self, local: &Path, remote: &str) -> Result<()> {
        FileOps::push(self, local, remote, None).await.map(|_| ())
    }

    async fn symlink(&self, target: &str, link: &str, kind: LinkKind) -> Result<()> {
        FileOps::symlink(self, target, link, kind).await
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        FileOps::rename(self, from, to).await
    }

    async fn remove(&self, path: &str) -> Result<()> {
        FileOps::remove(self, path).await
    }

    async fn rmdir(&self, path: &str) -> Result<()> {
        FileOps::rmdir(self, path).await
    }
}

/// A shared handle is a guest session too. Without this every holder of an
/// `Arc<FakeGuest>` (or of a pooled real session) would hand-write nine
/// forwarding methods to say nothing.
#[async_trait]
impl<T: GuestFs + ?Sized> GuestFs for std::sync::Arc<T> {
    async fn lstat(&self, path: &str) -> Result<Option<Attrs>> {
        (**self).lstat(path).await
    }
    async fn readlink(&self, path: &str) -> Result<String> {
        (**self).readlink(path).await
    }
    async fn digest(&self, path: &str) -> Result<String> {
        (**self).digest(path).await
    }
    async fn mkdir(&self, path: &str, case_sensitive: bool) -> Result<()> {
        (**self).mkdir(path, case_sensitive).await
    }
    async fn push(&self, local: &Path, remote: &str) -> Result<()> {
        (**self).push(local, remote).await
    }
    async fn symlink(&self, target: &str, link: &str, kind: LinkKind) -> Result<()> {
        (**self).symlink(target, link, kind).await
    }
    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        (**self).rename(from, to).await
    }
    async fn remove(&self, path: &str) -> Result<()> {
        (**self).remove(path).await
    }
    async fn rmdir(&self, path: &str) -> Result<()> {
        (**self).rmdir(path).await
    }
}

/// An in-memory guest tree, for the tests that have to watch what the syncer
/// actually does to one.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use anyhow::{Result, anyhow, bail};
    use vmlab_agent_proto::watch::EntryKind;

    use crate::labd::vm_agent::{ErrorCode, FileOpsError};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Node {
        File(Vec<u8>),
        Dir,
        Symlink(String),
        /// A socket, FIFO or device node: the guest holds one, and the syncer
        /// must skip it by name rather than record it.
        Special,
    }

    #[derive(Debug, Default)]
    struct Guest {
        nodes: BTreeMap<String, (Node, i64)>,
        /// Every path the guest was asked to write to, in order — so a test
        /// can assert a rename happened *after* the bytes landed, and that a
        /// target was never written to directly.
        writes: Vec<String>,
        /// Paths whose next push fails, standing in for a dropped channel.
        fail_push: Vec<String>,
        /// Paths the guest refuses to be read at all.
        unreadable: Vec<String>,
    }

    /// A fake guest filesystem. Paths are whatever the caller passes, so the
    /// tests see exactly the strings that would go on the wire.
    #[derive(Debug, Default)]
    pub struct FakeGuest {
        inner: Mutex<Guest>,
    }

    impl FakeGuest {
        pub fn new() -> FakeGuest {
            FakeGuest::default()
        }

        pub fn put(&self, path: &str, node: Node, mtime_ns: i64) {
            self.inner
                .lock()
                .expect("fake guest")
                .nodes
                .insert(path.to_string(), (node, mtime_ns));
        }

        pub fn file(&self, path: &str, body: &str, mtime_ns: i64) {
            self.put(path, Node::File(body.as_bytes().to_vec()), mtime_ns);
        }

        pub fn dir(&self, path: &str) {
            self.put(path, Node::Dir, 0);
        }

        pub fn get(&self, path: &str) -> Option<Node> {
            self.inner
                .lock()
                .expect("fake guest")
                .nodes
                .get(path)
                .map(|(n, _)| n.clone())
        }

        pub fn text(&self, path: &str) -> Option<String> {
            match self.get(path)? {
                Node::File(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                _ => None,
            }
        }

        pub fn paths(&self) -> Vec<String> {
            self.inner
                .lock()
                .expect("fake guest")
                .nodes
                .keys()
                .cloned()
                .collect()
        }

        pub fn writes(&self) -> Vec<String> {
            self.inner.lock().expect("fake guest").writes.clone()
        }

        /// The next push to `path` fails, as a dropped channel would.
        pub fn fail_push(&self, path: &str) {
            self.inner
                .lock()
                .expect("fake guest")
                .fail_push
                .push(path.to_string());
        }

        /// Reading `path` fails — the root-owned artefact a login cannot open.
        pub fn unreadable(&self, path: &str) {
            self.inner
                .lock()
                .expect("fake guest")
                .unreadable
                .push(path.to_string());
        }
    }

    #[async_trait]
    impl GuestFs for FakeGuest {
        async fn lstat(&self, path: &str) -> Result<Option<Attrs>> {
            let guest = self.inner.lock().expect("fake guest");
            if guest.unreadable.iter().any(|p| p == path) {
                bail!("permission denied: {path}");
            }
            let Some((node, mtime_ns)) = guest.nodes.get(path) else {
                return Ok(None);
            };
            let (kind, size) = match node {
                Node::File(bytes) => (EntryKind::File, bytes.len() as u64),
                Node::Dir => (EntryKind::Dir, 0),
                Node::Symlink(target) => (EntryKind::Symlink, target.len() as u64),
                Node::Special => (EntryKind::Other, 0),
            };
            Ok(Some(Attrs {
                kind,
                size,
                mtime_ns: *mtime_ns,
                atime_ns: *mtime_ns,
                mode: Some(0o644),
            }))
        }

        async fn readlink(&self, path: &str) -> Result<String> {
            match self.get(path) {
                Some(Node::Symlink(target)) => Ok(target),
                _ => Err(anyhow!("not a symlink: {path}")),
            }
        }

        async fn digest(&self, path: &str) -> Result<String> {
            use sha2::{Digest, Sha256};
            let guest = self.inner.lock().expect("fake guest");
            if guest.unreadable.iter().any(|p| p == path) {
                bail!("permission denied: {path}");
            }
            match guest.nodes.get(path) {
                Some((Node::File(bytes), _)) => Ok(hex::encode(Sha256::digest(bytes))),
                _ => Err(anyhow!("no such file: {path}")),
            }
        }

        async fn mkdir(&self, path: &str, _case_sensitive: bool) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            guest.writes.push(path.to_string());
            guest
                .nodes
                .entry(path.to_string())
                .or_insert((Node::Dir, 0));
            Ok(())
        }

        async fn push(&self, local: &Path, remote: &str) -> Result<()> {
            let bytes = std::fs::read(local)?;
            let mut guest = self.inner.lock().expect("fake guest");
            guest.writes.push(remote.to_string());
            if let Some(at) = guest.fail_push.iter().position(|p| p == remote) {
                guest.fail_push.remove(at);
                bail!("the channel dropped while pushing {remote}");
            }
            guest
                .nodes
                .insert(remote.to_string(), (Node::File(bytes), 9));
            Ok(())
        }

        async fn symlink(&self, target: &str, link: &str, _kind: LinkKind) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            guest.writes.push(link.to_string());
            if guest.nodes.contains_key(link) {
                bail!("already exists: {link}");
            }
            guest
                .nodes
                .insert(link.to_string(), (Node::Symlink(target.to_string()), 9));
            Ok(())
        }

        async fn rename(&self, from: &str, to: &str) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            let Some(node) = guest.nodes.remove(from) else {
                bail!("no such file: {from}");
            };
            guest.nodes.insert(to.to_string(), node);
            Ok(())
        }

        async fn remove(&self, path: &str) -> Result<()> {
            self.inner.lock().expect("fake guest").nodes.remove(path);
            Ok(())
        }

        async fn rmdir(&self, path: &str) -> Result<()> {
            let mut guest = self.inner.lock().expect("fake guest");
            let under = format!("{path}/");
            if guest.nodes.keys().any(|p| p.starts_with(&under)) {
                // What a real guest answers when the directory still holds
                // something — a guest-owned subtree the syncer never touches.
                return Err(anyhow::Error::new(FileOpsError {
                    code: ErrorCode::NotEmpty,
                    msg: format!("directory not empty: {path}"),
                }));
            }
            guest.nodes.remove(path);
            Ok(())
        }
    }
}
