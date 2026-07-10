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

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Flatten `layers` (compressed layer blob path + its manifest media type,
/// lowest first) into a squashfs at `dest`, verifying each layer's
/// decompressed stream against its `diff_id`. Synchronous — callers on the
/// async side wrap it in `spawn_blocking`.
pub fn flatten_to_squashfs(
    layers: &[(PathBuf, String)],
    diff_ids: &[String],
    dest: &Path,
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
    let winners = survey_layers(layers, diff_ids)?;
    write_squashfs(layers, &winners, dest)
}

/// Pass 1: stream every layer recording, per surviving path, the layer that
/// wins it, applying whiteouts/opaque markers as they appear. Also hashes
/// each layer's decompressed bytes and hard-errors on a `diff_id` mismatch.
fn survey_layers(
    layers: &[(PathBuf, String)],
    diff_ids: &[String],
) -> Result<HashMap<String, usize>> {
    let mut winners: HashMap<String, usize> = HashMap::new();
    for (i, (path, media_type)) in layers.iter().enumerate() {
        let mut reader = HashingReader::new(layer_reader(path, media_type)?);
        {
            let mut archive = tar::Archive::new(&mut reader);
            for entry in archive
                .entries()
                .with_context(|| format!("cannot read layer {i} ({})", path.display()))?
            {
                let entry =
                    entry.with_context(|| format!("cannot read layer {i} ({})", path.display()))?;
                let Some(name) = normalise(&entry.path()?) else {
                    continue;
                };
                apply_entry(&mut winners, i, &name, entry.header().entry_type());
            }
        }
        // The tar reader stops at the end-of-archive marker; drain to EOF so
        // the hash covers the whole decompressed stream (what diff_ids are
        // digests of).
        io::copy(&mut reader, &mut io::sink())
            .with_context(|| format!("cannot decompress layer {i} ({})", path.display()))?;
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

/// Fold one tar entry from `layer` into the survivor map. Whiteouts and
/// opaque markers only erase *lower* layers — entries this layer already
/// recorded are kept (a layer can legitimately whiteout a path and recreate
/// it, in either order).
fn apply_entry(winners: &mut HashMap<String, usize>, layer: usize, name: &str, kind: EntryType) {
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
        winners.retain(|p, l| *l == layer || !(p.len() > prefix.len() && p.starts_with(&prefix)));
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
        // A non-directory shadows any lower directory tree at the same path.
        let prefix = format!("{name}/");
        winners.retain(|p, l| *l == layer || !p.starts_with(&prefix));
    }
    winners.insert(name.to_string(), layer);
}

/// Pass 2: spawn `sqfstar` and stream the surviving entries into its stdin
/// as one merged tar, then wait and surface its stderr on failure.
fn write_squashfs(
    layers: &[(PathBuf, String)],
    winners: &HashMap<String, usize>,
    dest: &Path,
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
    let streamed = stream_survivors(layers, winners, stdin);
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
    winners: &HashMap<String, usize>,
    stdin: std::process::ChildStdin,
) -> Result<()> {
    let mut builder = tar::Builder::new(stdin);
    for (i, (path, media_type)) in layers.iter().enumerate() {
        let reader = layer_reader(path, media_type)?;
        let mut archive = tar::Archive::new(reader);
        for entry in archive
            .entries()
            .with_context(|| format!("cannot re-read layer {i} ({})", path.display()))?
        {
            let mut entry =
                entry.with_context(|| format!("cannot re-read layer {i} ({})", path.display()))?;
            let Some(name) = normalise(&entry.path()?) else {
                continue;
            };
            let base = name.rsplit('/').next().unwrap_or(&name);
            if base.starts_with(WHITEOUT_PREFIX) {
                continue; // whiteouts/opaque markers never reach the output
            }
            if winners.get(&name) != Some(&i) {
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

/// A `Read` adapter feeding every byte through SHA-256 on its way out, so a
/// layer's decompressed stream is hashed exactly once while tar walks it.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// The hex digest of everything read.
    fn finish(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
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
        flatten_to_squashfs(&[(l0, m0), (l1, m1)], &[d0, d1], &dest).unwrap();
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
        flatten_to_squashfs(&[(l0, m0), (l1, m1)], &[d0, d1], &dest).unwrap();

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
        let err =
            flatten_to_squashfs(&[(l0, m0)], &[bogus], &tmp.path().join("out.sqfs")).unwrap_err();
        assert!(err.to_string().contains("diff_id mismatch"), "{err}");
    }

    #[test]
    fn layer_count_mismatch_is_fatal() {
        // Checked before the sqfstar preflight, so this runs everywhere.
        let err = flatten_to_squashfs(
            &[(PathBuf::from("/nonexistent"), "tar".into())],
            &[],
            Path::new("/nonexistent-out"),
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

    #[test]
    fn survey_records_winners_and_whiteouts() {
        // Pure pass-1 logic, no sqfstar required.
        let mut winners = HashMap::new();
        // layer 0
        apply_entry(&mut winners, 0, "etc", EntryType::Directory);
        apply_entry(&mut winners, 0, "etc/version", EntryType::Regular);
        apply_entry(&mut winners, 0, "gone", EntryType::Regular);
        apply_entry(&mut winners, 0, "opq/lower", EntryType::Regular);
        apply_entry(&mut winners, 0, "tree/child", EntryType::Regular);
        // layer 1: override, whiteout, opaque, file-over-directory
        apply_entry(&mut winners, 1, "etc/version", EntryType::Regular);
        apply_entry(&mut winners, 1, ".wh.gone", EntryType::Regular);
        apply_entry(&mut winners, 1, "opq/.wh..wh..opq", EntryType::Regular);
        apply_entry(&mut winners, 1, "opq/upper", EntryType::Regular);
        apply_entry(&mut winners, 1, "tree", EntryType::Regular);

        assert_eq!(winners.get("etc"), Some(&0), "merged dir stays");
        assert_eq!(winners.get("etc/version"), Some(&1), "last writer wins");
        assert_eq!(winners.get("gone"), None, "whiteout deletes");
        assert_eq!(winners.get("opq/lower"), None, "opaque drops lower");
        assert_eq!(
            winners.get("opq/upper"),
            Some(&1),
            "opaque keeps same layer"
        );
        assert_eq!(
            winners.get("tree/child"),
            None,
            "file shadows lower dir tree"
        );
        assert_eq!(winners.get("tree"), Some(&1));
    }
}
