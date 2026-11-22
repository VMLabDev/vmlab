//! Flatten an ordered stack of OCI image layers into one squashfs image.
//!
//! Container layers are tars applied lowest→highest with OCI/overlayfs
//! whiteout semantics: a `.wh.<name>` entry deletes `<name>` (and anything
//! beneath it) from lower layers, a `.wh..wh..opq` entry makes its directory
//! opaque (everything beneath it from lower layers is dropped), and a path
//! present in several layers is won by the highest one. Rather than unpack
//! to disk (slow, permission-fraught) the layers are streamed twice:
//!
//! 1. **Survey** — walk every layer's entries recording, per path, the
//!    winning layer and the whiteout effects, while hashing each layer's
//!    decompressed bytes to verify its manifest `diff_id`.
//! 2. **Emit** — re-stream the layers lowest→highest, appending only the
//!    surviving entries to a merged tar piped straight into `sqfstar`,
//!    which builds the squashfs without the tree ever touching disk.
//!
//! A layer comes from a registry, so its content is hostile input: the survey
//! costs time and memory in proportion to the layer's bytes and no more, the
//! whole image is held to [`Limits`], and both passes stop at the caller's
//! cancel flag (the work runs on a blocking thread, which an aborted task
//! does not stop).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};
use tar::EntryType;

use crate::qemu::process::binary_on_path;

/// I/O copy buffer.
const COPY_BUF: usize = 1 << 20; // 1 MiB

/// The squashfs-tools binary that turns a tar stream into a squashfs.
const SQFSTAR: &str = "sqfstar";

/// Whiteout file-name prefix: `.wh.<name>` deletes `<name>` from lower layers.
const WHITEOUT_PREFIX: &str = ".wh.";
/// Opaque-directory marker: drops everything beneath its directory from
/// lower layers.
const OPAQUE_MARKER: &str = ".wh..wh..opq";

/// What one image may ask of the flatten. A registry chooses every number a
/// layer holds, so each of these is a refusal, not a truncation.
#[derive(Clone, Copy, Debug)]
struct Limits {
    /// Tar entries over all layers, and distinct paths the survey remembers.
    entries: u64,
    /// Bytes of path the survey remembers.
    name_bytes: u64,
    /// Decompressed bytes over all layers.
    unpacked_bytes: u64,
}

impl Limits {
    /// The limits of a pulled container image: several times the largest
    /// images in ordinary use, and a survey of at most a few GiB of memory.
    const IMAGE: Limits = Limits {
        entries: 4_000_000,
        name_bytes: 256 << 20,
        unpacked_bytes: 64 << 30,
    };
}

/// The message of a flatten that stopped at its cancel flag.
const CANCELLED: &str = "the flatten was cancelled";

/// Sets a flatten's cancel flag when dropped. The async caller holds it
/// across the `spawn_blocking` await: aborting that task drops it, and the
/// blocking thread — which an abort does not reach — stops at its next read.
pub struct CancelOnDrop(Arc<AtomicBool>);

impl CancelOnDrop {
    /// The guard and the flag to hand to [`flatten_to_squashfs`].
    pub fn pair() -> (Self, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        (Self(flag.clone()), flag)
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn ensure_running(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "{CANCELLED}");
    Ok(())
}

/// Flatten `layers` (compressed layer blob path + its manifest media type,
/// lowest first) into a squashfs at `dest`, verifying each layer's
/// decompressed stream against its `diff_id`. Synchronous — callers on the
/// async side wrap it in `spawn_blocking` and hold a [`CancelOnDrop`] for
/// `cancel`, which stops the work once set.
pub fn flatten_to_squashfs(
    layers: &[(PathBuf, String)],
    diff_ids: &[String],
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    ensure!(
        layers.len() == diff_ids.len(),
        "manifest has {} layers but the image config lists {} diff_ids — \
         the image is inconsistent",
        layers.len(),
        diff_ids.len()
    );
    if !binary_on_path(SQFSTAR) {
        bail!("`{SQFSTAR}` not found on PATH — install squashfs-tools to run container images");
    }
    let winners = survey_layers(layers, diff_ids, &Limits::IMAGE, cancel)?;
    write_squashfs(layers, &winners, dest, cancel)
}

/// Pass 1: stream every layer recording, per surviving path, the layer that
/// wins it, applying whiteouts/opaque markers as they appear. Also hashes
/// each layer's decompressed bytes and hard-errors on a `diff_id` mismatch.
fn survey_layers(
    layers: &[(PathBuf, String)],
    diff_ids: &[String],
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Survivors> {
    let mut winners = Survivors::new(limits);
    let mut entries: u64 = 0;
    let mut unpacked_left = limits.unpacked_bytes;
    for (i, (path, media_type)) in layers.iter().enumerate() {
        let mut reader =
            LayerStream::hashed(layer_reader(path, media_type)?, cancel, unpacked_left);
        {
            let mut archive = tar::Archive::new(&mut reader);
            for entry in archive
                .entries()
                .with_context(|| format!("cannot read layer {i} ({})", path.display()))?
            {
                ensure_running(cancel)?;
                let entry =
                    entry.with_context(|| format!("cannot read layer {i} ({})", path.display()))?;
                entries += 1;
                ensure!(
                    entries <= limits.entries,
                    "the image has more than {} entries in its layers — refused",
                    limits.entries
                );
                let Some(name) = normalise(&entry.path()?) else {
                    continue;
                };
                winners.apply(i, &name, entry.header().entry_type())?;
            }
        }
        // The tar reader stops at the end-of-archive marker; drain to EOF so
        // the hash covers the whole decompressed stream (what diff_ids are
        // digests of).
        io::copy(&mut reader, &mut io::sink())
            .with_context(|| format!("cannot decompress layer {i} ({})", path.display()))?;
        unpacked_left = reader.left;
        let got = format!("sha256:{}", reader.finish());
        if !got.eq_ignore_ascii_case(&diff_ids[i]) {
            bail!(
                "layer {i} ({}) diff_id mismatch: manifest says {}, got {got} — \
                 the layer is corrupt",
                path.display(),
                diff_ids[i]
            );
        }
    }
    Ok(winners)
}

/// The paths that survive the layer stack, as a tree of path components.
///
/// A whiteout, an opaque marker and a file over a directory each erase a
/// whole lower subtree. The tree records that as one number on the subtree's
/// root (`cut`) instead of visiting the subtree, so folding an entry in
/// costs its own path length whatever the image holds, and so does asking
/// who won a path.
struct Survivors {
    nodes: Vec<Node>,
    name_bytes: u64,
    max_nodes: u64,
    max_name_bytes: u64,
}

#[derive(Default)]
struct Node {
    /// The layer whose entry at this path is the newest not erased here.
    entry: Option<usize>,
    /// Entries strictly beneath this path from a layer below `cut` are
    /// erased. Layers are folded in lowest first, so a later entry is never
    /// from a layer below a cut that is already recorded.
    cut: usize,
    children: HashMap<Box<str>, usize>,
}

impl Survivors {
    fn new(limits: &Limits) -> Self {
        Self {
            nodes: vec![Node::default()],
            name_bytes: 0,
            max_nodes: limits.entries,
            max_name_bytes: limits.name_bytes,
        }
    }

    /// The node of `path` (`""` is the root), made if absent.
    fn node(&mut self, path: &str) -> Result<usize> {
        let mut at = 0;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            at = self.node_under(at, comp)?;
        }
        Ok(at)
    }

    /// Fold one tar entry from `layer` into the tree. Whiteouts and opaque
    /// markers only erase *lower* layers — entries this layer already
    /// recorded are kept (a layer can legitimately whiteout a path and
    /// recreate it, in either order).
    fn apply(&mut self, layer: usize, name: &str, kind: EntryType) -> Result<()> {
        let (dir, base) = match name.rsplit_once('/') {
            Some((dir, base)) => (dir, base),
            None => ("", name),
        };
        if base == OPAQUE_MARKER {
            let at = self.node(dir)?;
            let node = &mut self.nodes[at];
            node.cut = node.cut.max(layer);
            return Ok(());
        }
        if let Some(target_base) = base.strip_prefix(WHITEOUT_PREFIX) {
            if target_base.is_empty() {
                return Ok(()); // `.wh.` names no target
            }
            let at = self.node(dir)?;
            let at = self.node_under(at, target_base)?;
            let node = &mut self.nodes[at];
            if node.entry.is_some_and(|l| l < layer) {
                node.entry = None;
            }
            node.cut = node.cut.max(layer);
            return Ok(());
        }
        let at = self.node(name)?;
        let node = &mut self.nodes[at];
        if kind != EntryType::Directory {
            // A non-directory shadows any lower directory tree at the same path.
            node.cut = node.cut.max(layer);
        }
        node.entry = Some(layer);
        Ok(())
    }

    /// The node of the one component `comp` beneath node `at`, made if absent.
    fn node_under(&mut self, at: usize, comp: &str) -> Result<usize> {
        if let Some(&child) = self.nodes[at].children.get(comp) {
            return Ok(child);
        }
        self.name_bytes += comp.len() as u64;
        ensure!(
            self.nodes.len() as u64 <= self.max_nodes,
            "the image has more than {} paths in its layers — refused",
            self.max_nodes
        );
        ensure!(
            self.name_bytes <= self.max_name_bytes,
            "the paths in the image's layers take more than {} bytes — refused",
            self.max_name_bytes
        );
        let child = self.nodes.len();
        self.nodes.push(Node::default());
        self.nodes[at].children.insert(comp.into(), child);
        Ok(child)
    }

    /// The layer that wins `name`, or `None` when no layer's entry survives.
    fn winner(&self, name: &str) -> Option<usize> {
        let mut at = 0;
        let mut cut = 0;
        for comp in name.split('/').filter(|c| !c.is_empty()) {
            cut = cut.max(self.nodes[at].cut);
            at = *self.nodes[at].children.get(comp)?;
        }
        self.nodes[at].entry.filter(|layer| *layer >= cut)
    }
}

/// Pass 2: spawn `sqfstar` and stream the surviving entries into its stdin
/// as one merged tar, then wait and surface its stderr on failure.
fn write_squashfs(
    layers: &[(PathBuf, String)],
    winners: &Survivors,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    // sqfstar refuses to overwrite an existing image.
    let _ = std::fs::remove_file(dest);

    let mut child = Command::new(SQFSTAR)
        .arg(dest)
        .arg("-no-progress")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot spawn sqfstar")?;
    let stdin = child.stdin.take().expect("stdin was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    // Drain stderr concurrently so a chatty sqfstar can never dead-lock
    // against our stdin writes.
    let drain = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stderr.read_to_string(&mut out);
        out
    });

    // Stream survivors; stdin closes when this returns (even on error), so
    // the child always sees EOF and exits before we wait on it.
    let streamed = stream_survivors(layers, winners, stdin, cancel);
    if streamed.is_err() {
        // Nothing will read what sqfstar has built so far: do not wait for it
        // to finish an image of a part of the stream.
        let _ = child.kill();
    }
    let status = child.wait().context("waiting for sqfstar")?;
    let stderr_text = drain.join().unwrap_or_default();
    if !status.success() {
        let detail = stderr_text.trim();
        match streamed {
            Err(e) => bail!("sqfstar failed ({status}): {detail} (while streaming: {e:#})"),
            Ok(()) => bail!("sqfstar failed ({status}): {detail}"),
        }
    }
    streamed
}

/// Re-stream the layers lowest→highest, appending each entry whose path this
/// layer won (headers preserved: mode, uid/gid, mtime, link targets).
fn stream_survivors(
    layers: &[(PathBuf, String)],
    winners: &Survivors,
    stdin: std::process::ChildStdin,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut builder = tar::Builder::new(stdin);
    for (i, (path, media_type)) in layers.iter().enumerate() {
        let reader = LayerStream::verified(layer_reader(path, media_type)?, cancel);
        let mut archive = tar::Archive::new(reader);
        for entry in archive
            .entries()
            .with_context(|| format!("cannot re-read layer {i} ({})", path.display()))?
        {
            ensure_running(cancel)?;
            let mut entry =
                entry.with_context(|| format!("cannot re-read layer {i} ({})", path.display()))?;
            let Some(name) = normalise(&entry.path()?) else {
                continue;
            };
            let base = name.rsplit('/').next().unwrap_or(&name);
            if base.starts_with(WHITEOUT_PREFIX) {
                continue; // whiteouts/opaque markers never reach the output
            }
            if winners.winner(&name) != Some(i) {
                continue; // shadowed by a higher layer (or whiteout-erased)
            }
            // Preserve the original header; append via `append_data`/
            // `append_link` (rather than raw `append`) purely so paths and
            // link targets longer than the ustar fields survive — the crate
            // re-emits the GNU long-name records the source layer used.
            let mut header = entry.header().clone();
            match header.entry_type() {
                EntryType::Link | EntryType::Symlink => {
                    let target = entry
                        .link_name()
                        .with_context(|| format!("layer {i}: bad link target for `{name}`"))?
                        .ok_or_else(|| anyhow!("layer {i}: link entry `{name}` has no target"))?
                        .into_owned();
                    builder
                        .append_link(&mut header, &name, &target)
                        .with_context(|| format!("cannot append link `{name}`"))?;
                }
                _ => {
                    builder
                        .append_data(&mut header, &name, &mut entry)
                        .with_context(|| format!("cannot append `{name}`"))?;
                }
            }
        }
    }
    // `into_inner` finishes the archive (termination blocks) and hands back
    // the child's stdin, dropped here so sqfstar sees EOF.
    builder
        .into_inner()
        .context("cannot finish the merged tar stream")?;
    Ok(())
}

/// Open a layer blob with the right decompressor for its media type:
/// `+gzip`/`.gzip` (OCI/docker gzip), `+zstd`/`.zstd`, else plain tar.
fn layer_reader(path: &Path, media_type: &str) -> Result<Box<dyn Read>> {
    let file = File::open(path).with_context(|| format!("cannot open layer {}", path.display()))?;
    let buf = BufReader::with_capacity(COPY_BUF, file);
    if media_type.ends_with("+gzip") || media_type.ends_with(".gzip") {
        Ok(Box::new(flate2::read::GzDecoder::new(buf)))
    } else if media_type.ends_with("+zstd") || media_type.ends_with(".zstd") {
        Ok(Box::new(zstd::Decoder::new(buf).with_context(|| {
            format!("cannot read zstd layer {}", path.display())
        })?))
    } else {
        Ok(Box::new(buf))
    }
}

/// Normalise a tar entry path to a `/`-joined relative string (no leading
/// `./`, no trailing `/`). Absolute or `..`-containing paths are hostile in
/// a layer tar and yield `None` (the entry is ignored entirely).
fn normalise(path: &Path) -> Option<String> {
    let mut out = String::new();
    for comp in path.components() {
        match comp {
            Component::Normal(c) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&c.to_string_lossy());
            }
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A layer's decompressed stream as both passes read it: a read past `left`
/// bytes is refused, and so is any read once `cancel` is set. The survey's
/// stream also feeds every byte through SHA-256 on its way out, so the stream is
/// hashed exactly once while tar walks it.
struct LayerStream<'a, R> {
    inner: R,
    hasher: Option<Sha256>,
    cancel: &'a AtomicBool,
    /// Decompressed bytes the image may still yield.
    left: u64,
}

impl<'a, R: Read> LayerStream<'a, R> {
    /// The survey's stream: hashed, and held to `left` bytes.
    fn hashed(inner: R, cancel: &'a AtomicBool, left: u64) -> Self {
        Self {
            inner,
            hasher: Some(Sha256::new()),
            cancel,
            left,
        }
    }

    /// The emit pass's stream, over bytes the survey has hashed and held to
    /// the limit already.
    fn verified(inner: R, cancel: &'a AtomicBool) -> Self {
        Self {
            inner,
            hasher: None,
            cancel,
            left: u64::MAX,
        }
    }

    /// The hex digest of everything read from a hashed stream.
    fn finish(self) -> String {
        hex::encode(self.hasher.unwrap_or_default().finalize())
    }
}

impl<R: Read> Read for LayerStream<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(io::Error::other(CANCELLED));
        }
        let n = self.inner.read(buf)?;
        if n as u64 > self.left {
            return Err(io::Error::other(
                "the image's layers unpack to more bytes than a pull accepts — refused",
            ));
        }
        self.left -= n as u64;
        if let Some(hasher) = &mut self.hasher {
            hasher.update(&buf[..n]);
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// True when `bin` is on PATH; tests skip (with a note) otherwise.
    fn have(bin: &str) -> bool {
        let found = binary_on_path(bin);
        if !found {
            eprintln!("skipping: {bin} not on PATH");
        }
        found
    }

    /// Builds one layer tar in memory; `build` compresses (or not) and
    /// writes it to disk, returning `(path, media_type, diff_id)`.
    struct Layer {
        tar: tar::Builder<Vec<u8>>,
    }

    impl Layer {
        fn new() -> Self {
            Self {
                tar: tar::Builder::new(Vec::new()),
            }
        }

        fn header(kind: EntryType, size: u64, mode: u32, uid: u64) -> tar::Header {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_size(size);
            h.set_mode(mode);
            h.set_uid(uid);
            h.set_gid(uid);
            h.set_cksum();
            h
        }

        fn file(mut self, path: &str, mode: u32, uid: u64, contents: &[u8]) -> Self {
            let mut h = Self::header(EntryType::Regular, contents.len() as u64, mode, uid);
            self.tar.append_data(&mut h, path, contents).unwrap();
            self
        }

        fn dir(mut self, path: &str, mode: u32) -> Self {
            let mut h = Self::header(EntryType::Directory, 0, mode, 0);
            self.tar.append_data(&mut h, path, &[][..]).unwrap();
            self
        }

        fn symlink(mut self, path: &str, target: &str) -> Self {
            let mut h = Self::header(EntryType::Symlink, 0, 0o777, 0);
            self.tar.append_link(&mut h, path, target).unwrap();
            self
        }

        fn hardlink(mut self, path: &str, target: &str) -> Self {
            let mut h = Self::header(EntryType::Link, 0, 0o644, 0);
            self.tar.append_link(&mut h, path, target).unwrap();
            self
        }

        /// `.wh.<name>` whiteout for `path`.
        fn whiteout(self, path: &str) -> Self {
            let (dir, base) = match path.rsplit_once('/') {
                Some((d, b)) => (format!("{d}/"), b.to_string()),
                None => (String::new(), path.to_string()),
            };
            self.file(&format!("{dir}{WHITEOUT_PREFIX}{base}"), 0o644, 0, b"")
        }

        /// `.wh..wh..opq` opaque marker inside `dir`.
        fn opaque(self, dir: &str) -> Self {
            self.file(&format!("{dir}/{OPAQUE_MARKER}"), 0o644, 0, b"")
        }

        fn build(self, out_dir: &Path, name: &str, gzip: bool) -> (PathBuf, String, String) {
            let tar_bytes = self.tar.into_inner().unwrap();
            let diff_id = format!("sha256:{}", hex::encode(Sha256::digest(&tar_bytes)));
            let (bytes, media) = if gzip {
                let mut enc =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(&tar_bytes).unwrap();
                (
                    enc.finish().unwrap(),
                    "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
                )
            } else {
                (
                    tar_bytes,
                    "application/vnd.oci.image.layer.v1.tar".to_string(),
                )
            };
            let path = out_dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            (path, media, diff_id)
        }
    }

    fn unsquashfs(args: &[&str]) -> String {
        let out = Command::new("unsquashfs").args(args).output().unwrap();
        assert!(
            out.status.success(),
            "unsquashfs {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn flatten_applies_layer_semantics() {
        if !have(SQFSTAR) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();

        // Lower layer: files to be overridden / whited out / made opaque,
        // plus a symlink and a hardlink pair. Gzipped (the common case).
        let (l0, m0, d0) = Layer::new()
            .dir("etc", 0o755)
            .file("etc/version", 0o644, 0, b"v1")
            .file("gone.txt", 0o644, 0, b"bye")
            .dir("opq", 0o755)
            .file("opq/lower.txt", 0o644, 0, b"low")
            .file("hard.txt", 0o644, 0, b"h")
            .hardlink("hardlink.txt", "hard.txt")
            .symlink("link", "etc/version")
            .build(tmp.path(), "layer0.tar.gz", true);

        // Upper layer: overrides etc/version (new mode + owner), deletes
        // gone.txt, makes opq/ opaque with new content. Plain tar to cover
        // the passthrough decompressor.
        let (l1, m1, d1) = Layer::new()
            .file("etc/version", 0o600, 1000, b"v2")
            .whiteout("gone.txt")
            .dir("opq", 0o755)
            .opaque("opq")
            .file("opq/upper.txt", 0o644, 0, b"up")
            .build(tmp.path(), "layer1.tar", false);

        let dest = tmp.path().join("rootfs.sqfs");
        flatten_to_squashfs(
            &[(l0, m0), (l1, m1)],
            &[d0, d1],
            &dest,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(dest.is_file());
        assert!(std::fs::metadata(&dest).unwrap().len() > 0);

        if !have("unsquashfs") {
            return; // squashfs built; content assertions need unsquashfs
        }
        let dest_str = dest.to_str().unwrap();
        // -lln lists numeric uids/gids (a resolvable uid would print a name).
        let listing = unsquashfs(&["-lln", dest_str]);

        // last writer wins, with its mode/uid preserved
        let version_line = listing
            .lines()
            .find(|l| l.ends_with("/etc/version") && !l.contains("->"))
            .expect("etc/version listed");
        assert!(version_line.contains("rw-------"), "{version_line}");
        assert!(version_line.contains("1000/1000"), "{version_line}");
        assert_eq!(unsquashfs(&["-cat", dest_str, "etc/version"]), "v2");

        // whiteout removed the lower file (and its whiteout never appears)
        assert!(!listing.contains("gone.txt"), "{listing}");
        assert!(!listing.contains(".wh."), "{listing}");

        // opaque dir dropped lower contents, kept upper
        assert!(!listing.contains("lower.txt"), "{listing}");
        assert!(listing.contains("opq/upper.txt"), "{listing}");

        // symlink and hardlink survive
        assert!(listing.contains("link -> etc/version"), "{listing}");
        assert!(listing.contains("hard.txt"), "{listing}");
        assert!(listing.contains("hardlink.txt"), "{listing}");
        assert_eq!(unsquashfs(&["-cat", dest_str, "hardlink.txt"]), "h");
    }

    #[test]
    fn whiteout_deletes_whole_subtree() {
        if !have(SQFSTAR) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (l0, m0, d0) = Layer::new()
            .dir("data", 0o755)
            .file("data/a", 0o644, 0, b"a")
            .file("data/b", 0o644, 0, b"b")
            .file("keep", 0o644, 0, b"k")
            .build(tmp.path(), "l0.tar.gz", true);
        let (l1, m1, d1) = Layer::new()
            .whiteout("data")
            .build(tmp.path(), "l1.tar.gz", true);

        let dest = tmp.path().join("out.sqfs");
        flatten_to_squashfs(
            &[(l0, m0), (l1, m1)],
            &[d0, d1],
            &dest,
            &AtomicBool::new(false),
        )
        .unwrap();

        if !have("unsquashfs") {
            return;
        }
        let listing = unsquashfs(&["-ll", dest.to_str().unwrap()]);
        assert!(!listing.contains("data"), "{listing}");
        assert!(listing.contains("keep"), "{listing}");
    }

    #[test]
    fn diff_id_mismatch_is_fatal() {
        if !have(SQFSTAR) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (l0, m0, _real) =
            Layer::new()
                .file("x", 0o644, 0, b"x")
                .build(tmp.path(), "l0.tar.gz", true);
        let bogus = format!("sha256:{}", "0".repeat(64));
        let err = flatten_to_squashfs(
            &[(l0, m0)],
            &[bogus],
            &tmp.path().join("out.sqfs"),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("diff_id mismatch"), "{err}");
    }

    #[test]
    fn layer_count_mismatch_is_fatal() {
        // Checked before the sqfstar preflight, so this runs everywhere.
        let err = flatten_to_squashfs(
            &[(PathBuf::from("/nonexistent"), "tar".into())],
            &[],
            Path::new("/nonexistent-out"),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("diff_ids"), "{err}");
    }

    #[test]
    fn hostile_paths_are_ignored() {
        assert_eq!(
            normalise(Path::new("./etc/version")).as_deref(),
            Some("etc/version")
        );
        assert_eq!(normalise(Path::new("etc/")).as_deref(), Some("etc"));
        assert_eq!(normalise(Path::new("/etc/passwd")), None);
        assert_eq!(normalise(Path::new("../escape")), None);
        assert_eq!(normalise(Path::new("a/../../b")), None);
        assert_eq!(normalise(Path::new("./")), None);
    }

    /// Review F6: every non-directory entry scanned the whole survivor map,
    /// so a layer of `n` files cost `n²/2` visits.
    #[test]
    fn the_survey_of_a_large_layer_is_linear() {
        let tmp = tempfile::tempdir().unwrap();
        let mut layer = Layer::new().dir("d", 0o755);
        for i in 0..200_000 {
            layer = layer.file(&format!("d/f{i}"), 0o644, 0, b"");
        }
        let (path, media, diff_id) = layer.build(tmp.path(), "big.tar", false);
        let started = std::time::Instant::now();
        let cancel = AtomicBool::new(false);
        let winners = survey_layers(&[(path, media)], &[diff_id], &Limits::IMAGE, &cancel).unwrap();
        assert_eq!(winners.winner("d/f199999"), Some(0));
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(10),
            "the survey of 200,000 files took {took:?}"
        );
    }

    const ROOMY: Limits = Limits {
        entries: 1 << 20,
        name_bytes: 1 << 30,
        unpacked_bytes: 1 << 40,
    };

    /// Review F6: the survivor map had no entry limit.
    #[test]
    fn a_layer_with_too_many_entries_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let limits = Limits {
            entries: 100,
            ..ROOMY
        };
        let cancel = AtomicBool::new(false);
        let layer_of = |files: usize, name: &str| {
            let mut layer = Layer::new();
            for i in 0..files {
                layer = layer.file(&format!("f{i}"), 0o644, 0, b"");
            }
            let (path, media, diff_id) = layer.build(tmp.path(), name, false);
            (vec![(path, media)], vec![diff_id])
        };

        let (layers, diff_ids) = layer_of(100, "at-the-limit.tar");
        survey_layers(&layers, &diff_ids, &limits, &cancel).unwrap();

        let (layers, diff_ids) = layer_of(101, "over.tar");
        let err = survey_layers(&layers, &diff_ids, &limits, &cancel)
            .err()
            .expect("101 entries pass a limit of 100");
        assert!(
            format!("{err:#}").contains("more than 100 entries"),
            "{err:#}"
        );

        // One entry that names many directories: the paths are counted too.
        let deep = (0..200).map(|i| format!("d{i}")).collect::<Vec<_>>();
        let (path, media, diff_id) =
            Layer::new()
                .file(&deep.join("/"), 0o644, 0, b"")
                .build(tmp.path(), "deep.tar", false);
        let err = survey_layers(&[(path, media)], &[diff_id], &limits, &cancel)
            .err()
            .expect("200 paths pass a limit of 100");
        assert!(
            format!("{err:#}").contains("more than 100 paths"),
            "{err:#}"
        );
    }

    #[test]
    fn a_layer_whose_names_take_too_much_memory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let limits = Limits {
            name_bytes: 64,
            ..ROOMY
        };
        let (path, media, diff_id) = Layer::new()
            .file(&"a".repeat(40), 0o644, 0, b"")
            .file(&"b".repeat(40), 0o644, 0, b"")
            .build(tmp.path(), "names.tar", false);
        let err = survey_layers(
            &[(path, media)],
            &[diff_id],
            &limits,
            &AtomicBool::new(false),
        )
        .err()
        .expect("80 bytes of names pass a limit of 64");
        assert!(format!("{err:#}").contains("more than 64 bytes"), "{err:#}");
    }

    /// A gzip layer of zeroes is a thousandth of what it unpacks to.
    #[test]
    fn layers_that_unpack_to_too_much_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let limits = Limits {
            unpacked_bytes: 64 * 1024,
            ..ROOMY
        };
        let cancel = AtomicBool::new(false);
        let layer = |name: &str| {
            Layer::new()
                .file("zeroes", 0o644, 0, &[0u8; 40 * 1024])
                .build(tmp.path(), name, true)
        };
        let (p0, m0, d0) = layer("l0.tar.gz");
        let (p1, m1, d1) = layer("l1.tar.gz");

        survey_layers(&[(p0.clone(), m0.clone())], &[d0.clone()], &limits, &cancel).unwrap();
        // The limit is over the image, not over each layer.
        let err = survey_layers(&[(p0, m0), (p1, m1)], &[d0, d1], &limits, &cancel)
            .err()
            .expect("80 KiB pass a limit of 64 KiB");
        assert!(
            format!("{err:#}").contains("unpack to more bytes"),
            "{err:#}"
        );
    }

    /// Review F6: the flatten runs on a blocking thread, which an aborted
    /// pull task leaves running.
    #[test]
    fn a_cancelled_flatten_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let (path, media, diff_id) =
            Layer::new()
                .file("x", 0o644, 0, b"x")
                .build(tmp.path(), "l0.tar.gz", true);
        let layers = [(path, media)];
        let diff_ids = [diff_id];

        let (guard, cancel) = CancelOnDrop::pair();
        let winners = survey_layers(&layers, &diff_ids, &ROOMY, &cancel).unwrap();
        drop(guard);
        let err = survey_layers(&layers, &diff_ids, &ROOMY, &cancel)
            .err()
            .expect("the survey ran on after the cancel");
        assert!(format!("{err:#}").contains(CANCELLED), "{err:#}");

        if !have(SQFSTAR) {
            return;
        }
        let dest = tmp.path().join("out.sqfs");
        let err = write_squashfs(&layers, &winners, &dest, &cancel).unwrap_err();
        assert!(format!("{err:#}").contains(CANCELLED), "{err:#}");
        let err = flatten_to_squashfs(&layers, &diff_ids, &dest, &cancel).unwrap_err();
        assert!(format!("{err:#}").contains(CANCELLED), "{err:#}");
    }

    /// The tree must answer as the map it replaced did. That map is the
    /// definition of the layer semantics: every erasure visits every path.
    #[test]
    fn the_survivor_tree_agrees_with_a_scan_of_every_path() {
        fn scan(winners: &mut HashMap<String, usize>, layer: usize, name: &str, kind: EntryType) {
            let (dir, base) = match name.rsplit_once('/') {
                Some((dir, base)) => (dir, base),
                None => ("", name),
            };
            if base == OPAQUE_MARKER {
                let prefix = if dir.is_empty() {
                    String::new()
                } else {
                    format!("{dir}/")
                };
                winners.retain(|p, l| {
                    *l == layer || !(p.len() > prefix.len() && p.starts_with(&prefix))
                });
                return;
            }
            if let Some(target_base) = base.strip_prefix(WHITEOUT_PREFIX) {
                let target = if dir.is_empty() {
                    target_base.to_string()
                } else {
                    format!("{dir}/{target_base}")
                };
                let prefix = format!("{target}/");
                winners.retain(|p, l| *l == layer || !(*p == target || p.starts_with(&prefix)));
                return;
            }
            if kind != EntryType::Directory {
                let prefix = format!("{name}/");
                winners.retain(|p, l| *l == layer || !p.starts_with(&prefix));
            }
            winners.insert(name.to_string(), layer);
        }

        // Every path of up to three components over two names, so that the
        // entries collide, nest and erase each other often.
        let names = ["a", "b"];
        let mut paths: Vec<String> = Vec::new();
        for x in names {
            paths.push(x.to_string());
            for y in names {
                paths.push(format!("{x}/{y}"));
                for z in names {
                    paths.push(format!("{x}/{y}/{z}"));
                }
            }
        }
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |below: usize| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % below
        };
        for round in 0..2000 {
            let mut map = HashMap::new();
            let mut tree = Survivors::new(&ROOMY);
            let mut script = Vec::new();
            for layer in 0..4 {
                for _ in 0..next(6) {
                    let path = &paths[next(paths.len())];
                    let (dir, base) = match path.rsplit_once('/') {
                        Some((dir, base)) => (format!("{dir}/"), base),
                        None => (String::new(), path.as_str()),
                    };
                    let (name, kind) = match next(8) {
                        0 => (format!("{dir}{WHITEOUT_PREFIX}{base}"), EntryType::Regular),
                        1 => (format!("{path}/{OPAQUE_MARKER}"), EntryType::Regular),
                        2 => (OPAQUE_MARKER.to_string(), EntryType::Regular),
                        3 => (format!("{dir}{WHITEOUT_PREFIX}"), EntryType::Regular),
                        4 | 5 => (path.clone(), EntryType::Directory),
                        _ => (path.clone(), EntryType::Regular),
                    };
                    scan(&mut map, layer, &name, kind);
                    tree.apply(layer, &name, kind).unwrap();
                    script.push((layer, name, kind));
                }
            }
            for path in &paths {
                assert_eq!(
                    tree.winner(path),
                    map.get(path).copied(),
                    "round {round}, path `{path}`, after {script:?}"
                );
            }
        }
    }

    #[test]
    fn survey_records_winners_and_whiteouts() {
        // Pure pass-1 logic, no sqfstar required.
        let mut winners = Survivors::new(&ROOMY);
        let mut apply_entry = |layer: usize, name: &str, kind: EntryType| {
            winners.apply(layer, name, kind).unwrap();
        };
        // layer 0
        apply_entry(0, "etc", EntryType::Directory);
        apply_entry(0, "etc/version", EntryType::Regular);
        apply_entry(0, "gone", EntryType::Regular);
        apply_entry(0, "opq/lower", EntryType::Regular);
        apply_entry(0, "tree/child", EntryType::Regular);
        // layer 1: override, whiteout, opaque, file-over-directory
        apply_entry(1, "etc/version", EntryType::Regular);
        apply_entry(1, ".wh.gone", EntryType::Regular);
        apply_entry(1, "opq/.wh..wh..opq", EntryType::Regular);
        apply_entry(1, "opq/upper", EntryType::Regular);
        apply_entry(1, "tree", EntryType::Regular);

        assert_eq!(winners.winner("etc"), Some(0), "merged dir stays");
        assert_eq!(winners.winner("etc/version"), Some(1), "last writer wins");
        assert_eq!(winners.winner("gone"), None, "whiteout deletes");
        assert_eq!(winners.winner("opq/lower"), None, "opaque drops lower");
        assert_eq!(
            winners.winner("opq/upper"),
            Some(1),
            "opaque keeps same layer"
        );
        assert_eq!(
            winners.winner("tree/child"),
            None,
            "file shadows lower dir tree"
        );
        assert_eq!(winners.winner("tree"), Some(1));
    }
}
