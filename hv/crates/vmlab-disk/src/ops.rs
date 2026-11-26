//! Image operations vmlab performs with `qemu-img` today (D15): create,
//! linked clone, rebase, info, resize, convert, snapshot, commit. Each is a
//! function over paths so vmlab can link it directly.
//!
//! The behaviours mirror `qemu-img` where vmlab observes them (see
//! `src/template/qimg.rs` in the vmlab tree): a linked clone records an
//! absolute backing path, `rebase -u` records the backing string exactly as
//! given, safe rebase writes only clusters whose old-chain and new-backing
//! content differ, `convert -l … -B …` writes only the clusters allocated in
//! the source's own layer, and `commit -d` leaves the top image untouched.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::qcow2::{Qcow2Image, probe_format};
use crate::{DiskImage, Error, Format, Result};

const DEFAULT_CLUSTER_BITS: u32 = 16;

/// Subset of `qemu-img info --output=json` vmlab reads, plus the full
/// backing chain. Field names match qemu-img so vmlab can consume the JSON
/// unchanged.
#[derive(Debug, Clone, Serialize)]
pub struct ImageInfo {
    pub format: String,
    #[serde(rename = "virtual-size")]
    pub virtual_size: u64,
    #[serde(rename = "actual-size")]
    pub actual_size: u64,
    #[serde(rename = "backing-filename", skip_serializing_if = "Option::is_none")]
    pub backing_file: Option<String>,
    #[serde(
        rename = "full-backing-filename",
        skip_serializing_if = "Option::is_none"
    )]
    pub full_backing_file: Option<PathBuf>,
    #[serde(
        rename = "backing-filename-format",
        skip_serializing_if = "Option::is_none"
    )]
    pub backing_file_format: Option<String>,
    pub snapshots: Vec<SnapshotJson>,
    /// The full backing chain, top image first.
    pub chain: Vec<ChainEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotJson {
    pub id: String,
    pub name: String,
    #[serde(rename = "vm-state-size")]
    pub vm_state_size: u64,
    #[serde(rename = "date-sec")]
    pub date_sec: u64,
    #[serde(rename = "date-nsec")]
    pub date_nsec: u32,
    #[serde(rename = "vm-clock-nsec")]
    pub vm_clock_nsec: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChainEntry {
    pub filename: PathBuf,
    pub format: String,
    #[serde(rename = "virtual-size")]
    pub virtual_size: u64,
}

/// Create a blank qcow2 of `size` bytes, optionally backed by `backing`
/// (recorded exactly as given, like `qemu-img create -b`).
pub fn create_qcow2(
    path: &Path,
    size: u64,
    backing: Option<&Path>,
    backing_format: Option<&str>,
) -> Result<()> {
    let backing_ref = backing.map(|b| (b, backing_format));
    let mut img = Qcow2Image::create(path, size, DEFAULT_CLUSTER_BITS, backing_ref)?;
    img.flush()
}

/// Create a qcow2 linked clone at `dest` backed by `backing`. The backing
/// path is canonicalised so the clone works from any cwd (vmlab relies on
/// this).
pub fn create_linked_clone(backing: &Path, dest: &Path) -> Result<()> {
    let canonical = backing
        .canonicalize()
        .map_err(|e| Error::Unsupported(format!("cannot resolve {}: {e}", backing.display())))?;
    create_qcow2(
        dest,
        probe_size_of(backing)?,
        Some(&canonical),
        Some("qcow2"),
    )
}

fn probe_size_of(path: &Path) -> Result<u64> {
    let fmt = probe_format(path)?;
    let img = crate::open(path, fmt, true)?;
    Ok(img.size())
}

/// Rewrite `path`'s backing-file reference to `backing` (recorded exactly as
/// given, so a relative path stays relative to the image's directory)
/// without touching data: `qemu-img rebase -u`. Only correct when `backing`
/// names the same image the overlay was written against.
pub fn rebase_unsafe(path: &Path, backing: &str, format: Option<&str>) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    img.set_backing(backing, format)?;
    img.flush()
}

/// Move `path` onto `backing` keeping its content: `qemu-img rebase` in safe
/// mode. Reads the old chain (through the image) and the new backing, and
/// writes into the image only the clusters where they differ.
pub fn rebase_safe(path: &Path, backing: &Path, format: Option<&str>) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    let fmt = match format {
        Some(f) => format_to_format(f)?,
        None => probe_format(backing)?,
    };
    let mut new_backing = crate::open(backing, fmt, true)?;
    let cluster = img.cluster_size_bytes();
    let size = img.size();
    let mut offset = 0u64;
    while offset < size {
        let n = cluster.min(size - offset);
        let mut old_view = vec![0u8; n as usize];
        img.read_at(offset, &mut old_view)?;
        let mut new_view = vec![0u8; n as usize];
        if offset < new_backing.size() {
            let avail = (new_backing.size() - offset).min(n);
            new_backing.read_at(offset, &mut new_view[..avail as usize])?;
        }
        if old_view != new_view {
            img.write_at(offset, &old_view)?;
        }
        offset += n;
    }
    let backing_str = backing.to_string_lossy().into_owned();
    img.set_backing(&backing_str, format)?;
    img.flush()
}

fn format_to_format(name: &str) -> Result<Format> {
    match name {
        "qcow2" => Ok(Format::Qcow2),
        "raw" => Ok(Format::Raw),
        other => Err(Error::Unsupported(format!(
            "backing format {other} is not supported"
        ))),
    }
}

/// Inspect an image; mirrors the fields vmlab reads from
/// `qemu-img info --output=json` plus the full backing chain.
pub fn info(path: &Path) -> Result<ImageInfo> {
    let fmt = probe_format(path)?;
    let actual_size = actual_size(path)?;
    match fmt {
        Format::Raw => {
            let img = crate::raw::RawImage::open(path, true)?;
            let virtual_size = img.size();
            Ok(ImageInfo {
                format: "raw".into(),
                virtual_size,
                actual_size,
                backing_file: None,
                full_backing_file: None,
                backing_file_format: None,
                snapshots: Vec::new(),
                chain: vec![ChainEntry {
                    filename: path.to_path_buf(),
                    format: "raw".into(),
                    virtual_size,
                }],
            })
        }
        Format::Qcow2 => {
            let img = Qcow2Image::open(path, true)?;
            let virtual_size = img.size();
            let snapshots = img
                .snapshots()?
                .into_iter()
                .map(|s| SnapshotJson {
                    id: s.id,
                    name: s.name,
                    vm_state_size: s.vm_state_size,
                    date_sec: s.date_sec,
                    date_nsec: s.date_nsec,
                    vm_clock_nsec: s.vm_clock_nsec,
                })
                .collect();
            let backing_file = img.backing_file_name().map(str::to_string);
            let full_backing_file = img.backing_file_path();
            let backing_file_format = img.backing_format().map(str::to_string);

            // Walk the chain: top first, each entry as an absolute path.
            let mut chain = vec![ChainEntry {
                filename: absolute(path),
                format: "qcow2".into(),
                virtual_size,
            }];
            let mut cursor = full_backing_file.clone();
            while let Some(bp) = cursor {
                let bfmt = probe_format(&bp)?;
                match bfmt {
                    Format::Raw => {
                        let size = crate::raw::RawImage::open(&bp, true)?.size();
                        chain.push(ChainEntry {
                            filename: bp,
                            format: "raw".into(),
                            virtual_size: size,
                        });
                        cursor = None;
                    }
                    Format::Qcow2 => {
                        let bimg = Qcow2Image::open(&bp, true)?;
                        let next = bimg.backing_file_path();
                        chain.push(ChainEntry {
                            filename: bp,
                            format: "qcow2".into(),
                            virtual_size: bimg.size(),
                        });
                        cursor = next;
                    }
                }
            }

            Ok(ImageInfo {
                format: "qcow2".into(),
                virtual_size,
                actual_size,
                backing_file,
                full_backing_file,
                backing_file_format,
                snapshots,
                chain,
            })
        }
    }
}

/// Inspect one image's own header, like `qemu-img info` without
/// `--backing-chain`: the backing chain is not opened, so an image whose
/// backing file is missing still answers. `chain` holds the image alone.
pub fn info_header(path: &Path) -> Result<ImageInfo> {
    let actual_size = actual_size(path)?;
    let (format, virtual_size, img) = match probe_format(path)? {
        Format::Raw => ("raw", std::fs::metadata(path)?.len(), None),
        Format::Qcow2 => {
            let img = Qcow2Image::open_header(path)?;
            ("qcow2", img.size(), Some(img))
        }
    };
    let snapshots = match &img {
        Some(img) => img
            .snapshots()?
            .into_iter()
            .map(|s| SnapshotJson {
                id: s.id,
                name: s.name,
                vm_state_size: s.vm_state_size,
                date_sec: s.date_sec,
                date_nsec: s.date_nsec,
                vm_clock_nsec: s.vm_clock_nsec,
            })
            .collect(),
        None => Vec::new(),
    };
    Ok(ImageInfo {
        format: format.into(),
        virtual_size,
        actual_size,
        backing_file: img
            .as_ref()
            .and_then(|i| i.backing_file_name().map(str::to_string)),
        full_backing_file: img.as_ref().and_then(Qcow2Image::backing_file_path),
        backing_file_format: img
            .as_ref()
            .and_then(|i| i.backing_format().map(str::to_string)),
        snapshots,
        chain: vec![ChainEntry {
            filename: absolute(path),
            format: format.into(),
            virtual_size,
        }],
    })
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(unix)]
fn actual_size(path: &Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.blocks() * 512)
}

#[cfg(not(unix))]
fn actual_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Grow an image to `new_size` bytes (growing only; shrinking needs
/// `--shrink` in qemu-img and is deliberately not offered, matching vmlab).
pub fn resize(path: &Path, new_size: u64) -> Result<()> {
    resize_with_format(path, probe_format(path)?, new_size)
}

fn resize_with_format(path: &Path, format: Format, new_size: u64) -> Result<()> {
    match format {
        Format::Raw => {
            let file = std::fs::OpenOptions::new().write(true).open(path)?;
            let old = file.metadata()?.len();
            if new_size < old {
                return Err(Error::Unsupported(format!(
                    "shrinking a raw image from {old} to {new_size} bytes is not supported"
                )));
            }
            file.set_len(new_size)?;
            Ok(())
        }
        Format::Qcow2 => {
            let mut img = Qcow2Image::open(path, false)?;
            img.resize_grow(new_size)?;
            img.flush()
        }
    }
}

/// Convert `src` (any format) to a fresh qcow2 at `dest`.
///
/// * `snapshot`: read `src` at that internal snapshot (`convert -l`), without
///   modifying the source.
/// * `backing`: create `dest` on top of that backing file (`convert -B`), and
///   copy only the clusters allocated in the source's own layer, so `dest`
///   holds just the delta and reads through its backing for the rest.
pub fn convert(
    src: &Path,
    dest: &Path,
    snapshot: Option<&str>,
    backing: Option<&Path>,
    backing_format: Option<&str>,
) -> Result<()> {
    let dest_exists = dest.exists();
    if dest_exists {
        // The destination is replaced, so it must not be an image this
        // convert reads: the source, the `backing` argument, or a file of
        // either backing chain, under any name (review vmlab-disk F11).
        for input in std::iter::once(src).chain(backing) {
            if let Some(used) = chain_file_same_as(input, dest) {
                return Err(Error::Unsupported(format!(
                    "convert destination {} is {}, an image the convert reads; \
                     nothing was changed",
                    dest.display(),
                    used.display()
                )));
            }
        }
    }
    // The source opens before the destination is removed, so a source that
    // is missing, refused or lacks the snapshot leaves an existing
    // destination as it was (review vmlab-disk-2 F14).
    enum Source {
        Raw(crate::raw::RawImage),
        Qcow2(Box<Qcow2Image>),
    }
    let mut source = match snapshot {
        Some(name) => {
            let mut view = Qcow2Image::open(src, true)?;
            view.install_snapshot_view(name)?;
            Source::Qcow2(Box::new(view))
        }
        None => match probe_format(src)? {
            Format::Raw => Source::Raw(crate::raw::RawImage::open(src, true)?),
            Format::Qcow2 => Source::Qcow2(Box::new(Qcow2Image::open(src, true)?)),
        },
    };
    if dest_exists {
        std::fs::remove_file(dest)?;
    }
    let backing_ref = backing.map(|b| (b, backing_format));
    let size = match &source {
        Source::Raw(reader) => reader.size(),
        Source::Qcow2(reader) => reader.size(),
    };
    let mut dest_img = Qcow2Image::create(dest, size, DEFAULT_CLUSTER_BITS, backing_ref)?;
    match &mut source {
        Source::Raw(reader) => copy_full(reader, &mut dest_img)?,
        Source::Qcow2(reader) if backing.is_some() => copy_own_layer(reader, &mut dest_img)?,
        Source::Qcow2(reader) => copy_full(reader.as_mut(), &mut dest_img)?,
    }
    dest_img.flush()
}

/// The first file of `image`'s backing chain (the image itself first) that
/// is the same file as `target`, by any name. The walk reads headers only
/// and stops at the first file it cannot read: `convert` reports that error
/// itself when it opens the chain.
fn chain_file_same_as(image: &Path, target: &Path) -> Option<PathBuf> {
    // A chain longer than this is a loop, which the open refuses.
    const MAX_DEPTH: usize = 1024;
    let mut cursor = Some(image.to_path_buf());
    for _ in 0..MAX_DEPTH {
        let path = cursor?;
        if same_file(&path, target) {
            return Some(path);
        }
        cursor = match probe_format(&path) {
            Ok(Format::Qcow2) => Qcow2Image::open_header(&path)
                .ok()
                .and_then(|img| img.backing_file_path()),
            _ => None,
        };
    }
    None
}

/// Whether two paths name one file, through links of either kind.
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Copy the whole virtual disk, skipping all-zero chunks (sparse output,
/// like `qemu-img convert`).
fn copy_full(src: &mut dyn DiskImage, dest: &mut Qcow2Image) -> Result<()> {
    let size = src.size();
    let chunk = 1024 * 1024u64;
    let mut offset = 0u64;
    while offset < size {
        let n = chunk.min(size - offset) as usize;
        let mut buf = vec![0u8; n];
        src.read_at(offset, &mut buf)?;
        if !buf.iter().all(|&b| b == 0) {
            dest.write_at(offset, &buf)?;
        }
        offset += n as u64;
    }
    Ok(())
}

/// Copy only the clusters allocated in `src`'s own layer.
fn copy_own_layer(src: &mut Qcow2Image, dest: &mut dyn DiskImage) -> Result<()> {
    let cluster = src.cluster_size_bytes();
    let size = src.size();
    src.for_each_own_cluster(&mut |src, guest_cluster| {
        let offset = guest_cluster * cluster;
        if offset >= size {
            return Ok(()); // vmstate area never belongs in the copy
        }
        let mut buf = vec![0u8; cluster.min(size - offset) as usize];
        src.read_at(offset, &mut buf)?;
        dest.write_at(offset, &buf)?;
        Ok(())
    })
}

/// Create a qcow2-internal snapshot on a powered-off image (disk-only, like
/// `qemu-img snapshot -c`).
pub fn snapshot_create(path: &Path, name: &str) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    img.snapshot_create(name, 0, 0)?;
    img.flush()
}

/// Apply (revert to) a qcow2-internal snapshot.
pub fn snapshot_apply(path: &Path, name: &str) -> Result<crate::SnapshotInfo> {
    let mut img = Qcow2Image::open(path, false)?;
    let info = img.snapshot_apply(name)?;
    img.flush()?;
    Ok(info)
}

/// Delete a qcow2-internal snapshot.
pub fn snapshot_delete(path: &Path, name: &str) -> Result<()> {
    let mut img = Qcow2Image::open(path, false)?;
    img.snapshot_delete(name)?;
    img.flush()
}

/// List a qcow2 image's internal snapshots.
pub fn snapshot_list(path: &Path) -> Result<Vec<crate::SnapshotInfo>> {
    Qcow2Image::open(path, true)?.snapshots()
}

/// Write `top`'s active layer into its backing file, leaving `top` exactly
/// as it was (`qemu-img commit -d`).
pub fn commit(top: &Path) -> Result<()> {
    let mut img = Qcow2Image::open(top, true)?; // top is only read
    let backing_path = img
        .backing_file_path()
        .ok_or_else(|| Error::Unsupported("image has no backing file to commit into".into()))?;
    let backing_format = match img.backing_format() {
        Some(f) => format_to_format(f)?,
        None => probe_format(&backing_path)?,
    };
    let top_size = img.size();
    let mut backing = crate::open(&backing_path, backing_format, false)?;
    if backing.size() < top_size {
        drop(backing);
        resize_with_format(&backing_path, backing_format, top_size)?;
        backing = crate::open(&backing_path, backing_format, false)?;
    }
    copy_own_layer(&mut img, backing.as_mut())?;
    backing.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header-only inspection answers for an image whose backing file is
    /// gone, which the full chain walk cannot.
    #[test]
    fn info_header_survives_a_missing_backing_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        let top = dir.path().join("top.qcow2");
        create_qcow2(&base, 1 << 20, None, None).unwrap();
        create_linked_clone(&base, &top).unwrap();
        snapshot_create(&top, "one").unwrap();
        let recorded = base.canonicalize().unwrap();
        std::fs::remove_file(&base).unwrap();
        assert!(info(&top).is_err());
        let i = info_header(&top).unwrap();
        assert_eq!(i.virtual_size, 1 << 20);
        assert_eq!(i.full_backing_file, Some(recorded));
        assert_eq!(i.snapshots.len(), 1);
        assert_eq!(i.snapshots[0].name, "one");
    }

    fn patterned(path: &Path, byte: u8) {
        create_qcow2(path, 1 << 20, None, None).unwrap();
        let mut img = Qcow2Image::open(path, false).unwrap();
        img.write_at(0, &[byte; 4096]).unwrap();
        img.flush().unwrap();
    }

    fn first_bytes(path: &Path) -> Vec<u8> {
        let mut img = Qcow2Image::open(path, true).unwrap();
        let mut buf = vec![0u8; 4096];
        img.read_at(0, &mut buf).unwrap();
        buf
    }

    /// Review vmlab-disk F11: `convert` removes an existing destination, so
    /// a destination that is the source must be refused before the removal.
    #[test]
    fn convert_onto_the_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let same = dir.path().join("same.qcow2");
        patterned(&same, 0xa5);
        assert!(convert(&same, &same, None, None, None).is_err());
        assert!(same.exists(), "the source was deleted");
        assert_eq!(first_bytes(&same), vec![0xa5; 4096]);
    }

    /// The same for a destination that is a second name of the source.
    #[cfg(unix)]
    #[test]
    fn convert_onto_a_second_name_of_the_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.qcow2");
        patterned(&src, 0x5a);
        let hard = dir.path().join("hard.qcow2");
        std::fs::hard_link(&src, &hard).unwrap();
        let soft = dir.path().join("soft.qcow2");
        std::os::unix::fs::symlink(&src, &soft).unwrap();
        for dest in [&hard, &soft] {
            assert!(convert(&src, dest, None, None, None).is_err());
            assert!(dest.symlink_metadata().is_ok(), "{dest:?} was removed");
            assert_eq!(first_bytes(&src), vec![0x5a; 4096]);
        }
    }

    /// The same for a destination that is a backing file the convert reads:
    /// the source's own, or the one given as `backing`.
    #[test]
    fn convert_onto_a_backing_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        patterned(&base, 0x3c);
        let top = dir.path().join("top.qcow2");
        create_linked_clone(&base, &top).unwrap();
        assert!(convert(&top, &base, None, None, None).is_err());
        assert_eq!(first_bytes(&top), vec![0x3c; 4096]);

        let other = dir.path().join("other.qcow2");
        patterned(&other, 0x11);
        assert!(convert(&other, &base, None, Some(&base), Some("qcow2")).is_err());
        assert_eq!(first_bytes(&base), vec![0x3c; 4096]);
    }

    /// An existing destination that the convert does not read is replaced,
    /// as before.
    #[test]
    fn convert_replaces_an_unrelated_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.qcow2");
        patterned(&src, 0x77);
        let dest = dir.path().join("dest.qcow2");
        patterned(&dest, 0x01);
        convert(&src, &dest, None, None, None).unwrap();
        assert_eq!(first_bytes(&dest), vec![0x77; 4096]);
    }

    /// Review vmlab-disk-2 F14: a convert that fails because its source does
    /// not open leaves an existing destination as it was.
    #[test]
    fn convert_from_a_missing_source_keeps_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("keep.qcow2");
        patterned(&dest, 0x42);
        let missing = dir.path().join("nosuch.qcow2");
        assert!(convert(&missing, &dest, None, None, None).is_err());
        assert!(dest.exists(), "the destination was deleted");
        assert_eq!(first_bytes(&dest), vec![0x42; 4096]);
    }

    /// The same when the source opens and the snapshot it is read at does
    /// not exist.
    #[test]
    fn convert_of_a_missing_snapshot_keeps_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.qcow2");
        patterned(&src, 0x10);
        let dest = dir.path().join("keep.qcow2");
        patterned(&dest, 0x24);
        assert!(convert(&src, &dest, Some("nosuch"), None, None).is_err());
        assert!(dest.exists(), "the destination was deleted");
        assert_eq!(first_bytes(&dest), vec![0x24; 4096]);
    }
}
