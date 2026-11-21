//! vmlab's disk-image operations (PRD §7.1, §7.3, §21; HV-D15): blank
//! disks, linked clones, rebase, inspection, resize, convert, offline
//! snapshots and commit.
//!
//! Every operation goes through [`ImageTool`], which has two
//! implementations: [`VmlabDisk`], the linked `vmlab-disk` crate and the
//! default, and [`QemuImg`], the `qemu-img` binary, kept as a fallback the
//! operator selects with `VMLAB_IMAGE_TOOL=qemu-img`. Nothing outside
//! [`QemuImg`] spawns `qemu-img`. The async functions below run the selected
//! tool on the blocking pool; synchronous callers use [`tool`] directly.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde::Deserialize;

/// The environment variable that selects the image tool: `vmlab-disk` (the
/// default) or `qemu-img`.
pub const TOOL_ENV: &str = "VMLAB_IMAGE_TOOL";

/// Failures from an image operation.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("cannot run qemu-img (is QEMU installed?): {0}")]
    Spawn(#[source] std::io::Error),
    #[error("{command} failed ({status}): {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("cannot parse `qemu-img info` output for {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot resolve backing file {path}: {source}")]
    Backing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("vmlab-disk {op} {path}: {source}")]
    Disk {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: vmlab_disk::Error,
    },
    #[error("image operation {op} panicked or was cancelled: {message}")]
    Join { op: &'static str, message: String },
}

pub type Result<T> = std::result::Result<T, ImageError>;

/// What vmlab reads about an image (the subset of `qemu-img info
/// --output=json` both tools report).
#[derive(Debug, Clone, Deserialize)]
pub struct ImageInfo {
    /// Guest-visible disk size in bytes.
    #[serde(rename = "virtual-size")]
    pub virtual_size: u64,
    /// Bytes actually allocated on the host (the sparseness test reads it).
    #[serde(rename = "actual-size")]
    pub actual_size: u64,
    /// Backing image, present for linked clones.
    #[serde(rename = "backing-filename", default)]
    pub backing_file: Option<PathBuf>,
    /// The backing image resolved against the image's directory: absolute
    /// even when the image records a relative path.
    #[serde(rename = "full-backing-filename", default)]
    pub full_backing_file: Option<PathBuf>,
    /// The qcow2-internal snapshots the image holds (PRD §7.3).
    #[serde(default)]
    pub snapshots: Vec<InternalSnapshot>,
}

/// One qcow2-internal snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InternalSnapshot {
    pub name: String,
    /// Bytes of saved machine state: non-zero for an online snapshot's
    /// primary disk, which no offline operation can carry elsewhere.
    #[serde(rename = "vm-state-size", default)]
    pub vm_state_size: u64,
}

/// The image operations vmlab performs. Every method is blocking.
pub trait ImageTool: Send + Sync {
    /// The tool's name, as `VMLAB_IMAGE_TOOL` spells it.
    fn name(&self) -> &'static str;
    /// Create a blank qcow2 of `size_bytes` (PRD §6.5 scratch disks).
    fn create_blank(&self, path: &Path, size_bytes: u64) -> Result<()>;
    /// Create a qcow2 linked clone at `dest` backed by `backing` (PRD §7.1),
    /// recording the backing path absolute so the clone works from any cwd.
    fn create_linked_clone(&self, backing: &Path, dest: &Path) -> Result<()>;
    /// Rewrite `path`'s backing reference to `backing` exactly as written,
    /// without touching data (`qemu-img rebase -u`). Only correct when
    /// `backing` names the same image the overlay was written against (§6.6).
    fn rebase_unsafe(&self, path: &Path, backing: &str) -> Result<()>;
    /// Move `path` onto `backing` keeping its content, writing into `path`
    /// only the clusters where the old and new chains differ (PRD §21.1).
    fn rebase_safe(&self, path: &Path, backing: &Path) -> Result<()>;
    /// Inspect one image's header. The backing chain is not opened, and the
    /// image may be a running machine's disk.
    fn image_info(&self, path: &Path) -> Result<ImageInfo>;
    /// Grow an image to `new_size` bytes (shrinking is not offered).
    fn resize(&self, path: &Path, new_size: u64) -> Result<()>;
    /// Convert any image (a raw FAT volume, a qcow2 chain) to a standalone
    /// qcow2.
    fn convert_to_qcow2(&self, src: &Path, dest: &Path) -> Result<()>;
    /// Create a qcow2-internal snapshot on a powered-off image (PRD §7.3).
    fn snapshot_create(&self, path: &Path, name: &str) -> Result<()>;
    /// Apply (revert to) a qcow2-internal snapshot.
    fn snapshot_apply(&self, path: &Path, name: &str) -> Result<()>;
    /// Delete a qcow2-internal snapshot.
    fn snapshot_delete(&self, path: &Path, name: &str) -> Result<()>;
    /// Write `dest` as an overlay on `backing` holding `src`'s state at its
    /// internal snapshot `snapshot`. Only what `src`'s own layer allocated is
    /// written; `backing` must hold the content `src`'s backing did (§21.1).
    fn export_overlay(&self, src: &Path, snapshot: &str, backing: &Path, dest: &Path)
    -> Result<()>;
    /// Write `top`'s active layer into its backing file, leaving `top` as it
    /// was (`qemu-img commit -d`).
    fn commit(&self, top: &Path) -> Result<()>;
}

/// Which [`ImageTool`] runs vmlab's image operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    VmlabDisk,
    QemuImg,
}

impl Backend {
    /// Parse a `VMLAB_IMAGE_TOOL` value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "vmlab-disk" => Some(Self::VmlabDisk),
            "qemu-img" => Some(Self::QemuImg),
            _ => None,
        }
    }

    /// The selection from the environment: `VMLAB_IMAGE_TOOL`, else
    /// `vmlab-disk`. A malformed value is ignored with one warning.
    pub fn from_env() -> Self {
        match std::env::var(TOOL_ENV) {
            Ok(v) if !v.trim().is_empty() => Self::parse(&v).unwrap_or_else(|| {
                static WARNED: OnceLock<()> = OnceLock::new();
                WARNED.get_or_init(|| {
                    tracing::warn!(
                        "{TOOL_ENV}={v:?} is neither `vmlab-disk` nor `qemu-img`; using vmlab-disk"
                    )
                });
                Self::VmlabDisk
            }),
            _ => Self::VmlabDisk,
        }
    }

    pub fn tool(self) -> &'static dyn ImageTool {
        match self {
            Self::VmlabDisk => &VmlabDisk,
            Self::QemuImg => &QemuImg,
        }
    }

    /// The host binary this backend needs on PATH, if any.
    pub fn required_binary(self) -> Option<&'static str> {
        match self {
            Self::VmlabDisk => None,
            Self::QemuImg => Some("qemu-img"),
        }
    }
}

/// The backend in force: a test's override on this thread, else
/// [`Backend::from_env`].
pub fn backend() -> Backend {
    #[cfg(test)]
    if let Some(b) = testing::current() {
        return b;
    }
    Backend::from_env()
}

/// The image tool in force.
pub fn tool() -> &'static dyn ImageTool {
    backend().tool()
}

/// Run `f` with the selected tool on the blocking pool.
async fn blocking<T, F>(op: &'static str, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&'static dyn ImageTool) -> Result<T> + Send + 'static,
{
    #[cfg(test)]
    if let Some((entered, resume)) = testing::pause_for(op) {
        let _ = entered.send(());
        let _ = resume.await;
    }
    let tool = tool();
    tokio::task::spawn_blocking(move || f(tool))
        .await
        .map_err(|e| ImageError::Join {
            op,
            message: e.to_string(),
        })?
}

/// Create a blank qcow2 of `size_bytes` (PRD §6.5 scratch disks).
pub async fn create_blank(path: &Path, size_bytes: u64) -> Result<()> {
    let path = path.to_path_buf();
    blocking("create", move |t| t.create_blank(&path, size_bytes)).await
}

/// Create a qcow2 linked clone at `dest` backed by `backing` (PRD §7.1).
pub async fn create_linked_clone(backing: &Path, dest: &Path) -> Result<()> {
    let (backing, dest) = (backing.to_path_buf(), dest.to_path_buf());
    blocking("clone", move |t| t.create_linked_clone(&backing, &dest)).await
}

/// See [`ImageTool::rebase_unsafe`].
pub async fn rebase_unsafe(path: &Path, backing: &str) -> Result<()> {
    let (path, backing) = (path.to_path_buf(), backing.to_string());
    blocking("rebase", move |t| t.rebase_unsafe(&path, &backing)).await
}

/// See [`ImageTool::rebase_safe`].
pub async fn rebase_safe(path: &Path, backing: &Path) -> Result<()> {
    let (path, backing) = (path.to_path_buf(), backing.to_path_buf());
    blocking("rebase", move |t| t.rebase_safe(&path, &backing)).await
}

/// See [`ImageTool::image_info`].
pub async fn image_info(path: &Path) -> Result<ImageInfo> {
    let path = path.to_path_buf();
    blocking("info", move |t| t.image_info(&path)).await
}

/// See [`ImageTool::resize`].
pub async fn resize(path: &Path, new_size: u64) -> Result<()> {
    let path = path.to_path_buf();
    blocking("resize", move |t| t.resize(&path, new_size)).await
}

/// See [`ImageTool::convert_to_qcow2`].
pub async fn convert_to_qcow2(src: &Path, dest: &Path) -> Result<()> {
    let (src, dest) = (src.to_path_buf(), dest.to_path_buf());
    blocking("convert", move |t| t.convert_to_qcow2(&src, &dest)).await
}

/// See [`ImageTool::snapshot_create`].
pub async fn snapshot_create(path: &Path, name: &str) -> Result<()> {
    let (path, name) = (path.to_path_buf(), name.to_string());
    blocking("snapshot", move |t| t.snapshot_create(&path, &name)).await
}

/// See [`ImageTool::snapshot_apply`].
pub async fn snapshot_apply(path: &Path, name: &str) -> Result<()> {
    let (path, name) = (path.to_path_buf(), name.to_string());
    blocking("snapshot", move |t| t.snapshot_apply(&path, &name)).await
}

/// See [`ImageTool::snapshot_delete`].
pub async fn snapshot_delete(path: &Path, name: &str) -> Result<()> {
    let (path, name) = (path.to_path_buf(), name.to_string());
    blocking("snapshot", move |t| t.snapshot_delete(&path, &name)).await
}

/// See [`ImageTool::export_overlay`].
pub async fn export_overlay(src: &Path, snapshot: &str, backing: &Path, dest: &Path) -> Result<()> {
    let (src, snapshot) = (src.to_path_buf(), snapshot.to_string());
    let (backing, dest) = (backing.to_path_buf(), dest.to_path_buf());
    blocking("convert", move |t| {
        t.export_overlay(&src, &snapshot, &backing, &dest)
    })
    .await
}

/// See [`ImageTool::commit`].
pub async fn commit(top: &Path) -> Result<()> {
    let top = top.to_path_buf();
    blocking("commit", move |t| t.commit(&top)).await
}

fn canonical_backing(backing: &Path) -> Result<PathBuf> {
    backing.canonicalize().map_err(|e| ImageError::Backing {
        path: backing.to_path_buf(),
        source: e,
    })
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---- qemu-img --------------------------------------------------------------

/// The `qemu-img` binary: the fallback implementation.
pub struct QemuImg;

impl QemuImg {
    fn run(args: &[&str]) -> Result<Vec<u8>> {
        let output = Command::new("qemu-img")
            .args(args)
            .output()
            .map_err(ImageError::Spawn)?;
        if !output.status.success() {
            return Err(ImageError::Failed {
                command: format!("qemu-img {}", args.join(" ")),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(output.stdout)
    }
}

impl ImageTool for QemuImg {
    fn name(&self) -> &'static str {
        "qemu-img"
    }

    fn create_blank(&self, path: &Path, size_bytes: u64) -> Result<()> {
        let (path, size) = (path_str(path), size_bytes.to_string());
        Self::run(&["create", "-q", "-f", "qcow2", &path, &size])?;
        Ok(())
    }

    fn create_linked_clone(&self, backing: &Path, dest: &Path) -> Result<()> {
        let backing = path_str(&canonical_backing(backing)?);
        let dest = path_str(dest);
        Self::run(&[
            "create", "-q", "-f", "qcow2", "-b", &backing, "-F", "qcow2", &dest,
        ])?;
        Ok(())
    }

    fn rebase_unsafe(&self, path: &Path, backing: &str) -> Result<()> {
        let path = path_str(path);
        Self::run(&["rebase", "-u", "-b", backing, "-F", "qcow2", &path])?;
        Ok(())
    }

    fn rebase_safe(&self, path: &Path, backing: &Path) -> Result<()> {
        let (path, backing) = (path_str(path), path_str(backing));
        Self::run(&["rebase", "-q", "-b", &backing, "-F", "qcow2", &path])?;
        Ok(())
    }

    /// `-U` (force-share): the image may be a running machine's disk, which
    /// QEMU holds a write lock on, and reading its header is all this does.
    /// Without it the in-use check behind `template rm` and `clean` could not
    /// see the clones of running machines — exactly the ones it must protect.
    fn image_info(&self, path: &Path) -> Result<ImageInfo> {
        let path_arg = path_str(path);
        let stdout = Self::run(&["info", "-U", "--output=json", &path_arg])?;
        serde_json::from_slice(&stdout).map_err(|e| ImageError::Parse {
            path: path.to_path_buf(),
            source: e,
        })
    }

    fn resize(&self, path: &Path, new_size: u64) -> Result<()> {
        let (path, size) = (path_str(path), new_size.to_string());
        Self::run(&["resize", "-q", &path, &size])?;
        Ok(())
    }

    fn convert_to_qcow2(&self, src: &Path, dest: &Path) -> Result<()> {
        let (src, dest) = (path_str(src), path_str(dest));
        Self::run(&["convert", "-O", "qcow2", &src, &dest])?;
        Ok(())
    }

    fn snapshot_create(&self, path: &Path, name: &str) -> Result<()> {
        Self::run(&["snapshot", "-c", name, &path_str(path)])?;
        Ok(())
    }

    fn snapshot_apply(&self, path: &Path, name: &str) -> Result<()> {
        Self::run(&["snapshot", "-a", name, &path_str(path)])?;
        Ok(())
    }

    fn snapshot_delete(&self, path: &Path, name: &str) -> Result<()> {
        Self::run(&["snapshot", "-d", name, &path_str(path)])?;
        Ok(())
    }

    fn export_overlay(
        &self,
        src: &Path,
        snapshot: &str,
        backing: &Path,
        dest: &Path,
    ) -> Result<()> {
        let opt = format!("snapshot.name={snapshot}");
        let (src, backing, dest) = (path_str(src), path_str(backing), path_str(dest));
        Self::run(&[
            "convert", "-O", "qcow2", "-l", &opt, "-B", &backing, "-F", "qcow2", &src, &dest,
        ])?;
        Ok(())
    }

    fn commit(&self, top: &Path) -> Result<()> {
        Self::run(&["commit", "-q", "-d", &path_str(top)])?;
        Ok(())
    }
}

// ---- vmlab-disk ------------------------------------------------------------

/// The linked `vmlab-disk` crate: the default implementation (HV-D15).
pub struct VmlabDisk;

fn disk<T>(op: &'static str, path: &Path, r: vmlab_disk::Result<T>) -> Result<T> {
    r.map_err(|source| ImageError::Disk {
        op,
        path: path.to_path_buf(),
        source,
    })
}

/// `qemu-img create` replaces a file already at `path`; vmlab-disk creates
/// only new files, so remove one first to keep the two tools alike.
fn replace_existing(op: &'static str, path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(ImageError::Disk {
            op,
            path: path.to_path_buf(),
            source: e.into(),
        }),
        _ => Ok(()),
    }
}

impl ImageTool for VmlabDisk {
    fn name(&self) -> &'static str {
        "vmlab-disk"
    }

    fn create_blank(&self, path: &Path, size_bytes: u64) -> Result<()> {
        replace_existing("create", path)?;
        let r = vmlab_disk::ops::create_qcow2(path, size_bytes, None, None);
        disk("create", path, r)
    }

    fn create_linked_clone(&self, backing: &Path, dest: &Path) -> Result<()> {
        let backing = canonical_backing(backing)?;
        replace_existing("clone", dest)?;
        let r = vmlab_disk::ops::create_linked_clone(&backing, dest);
        disk("clone", dest, r)
    }

    fn rebase_unsafe(&self, path: &Path, backing: &str) -> Result<()> {
        let r = vmlab_disk::ops::rebase_unsafe(path, backing, Some("qcow2"));
        disk("rebase -u", path, r)
    }

    fn rebase_safe(&self, path: &Path, backing: &Path) -> Result<()> {
        let r = vmlab_disk::ops::rebase_safe(path, backing, Some("qcow2"));
        disk("rebase", path, r)
    }

    fn image_info(&self, path: &Path) -> Result<ImageInfo> {
        let i = disk("info", path, vmlab_disk::ops::info_header(path))?;
        Ok(ImageInfo {
            virtual_size: i.virtual_size,
            actual_size: i.actual_size,
            backing_file: i.backing_file.map(PathBuf::from),
            full_backing_file: i.full_backing_file,
            snapshots: i
                .snapshots
                .into_iter()
                .map(|s| InternalSnapshot {
                    name: s.name,
                    vm_state_size: s.vm_state_size,
                })
                .collect(),
        })
    }

    fn resize(&self, path: &Path, new_size: u64) -> Result<()> {
        disk("resize", path, vmlab_disk::ops::resize(path, new_size))
    }

    fn convert_to_qcow2(&self, src: &Path, dest: &Path) -> Result<()> {
        let r = vmlab_disk::ops::convert(src, dest, None, None, None);
        disk("convert", src, r)
    }

    fn snapshot_create(&self, path: &Path, name: &str) -> Result<()> {
        let r = vmlab_disk::ops::snapshot_create(path, name);
        disk("snapshot -c", path, r)
    }

    fn snapshot_apply(&self, path: &Path, name: &str) -> Result<()> {
        let r = vmlab_disk::ops::snapshot_apply(path, name).map(drop);
        disk("snapshot -a", path, r)
    }

    fn snapshot_delete(&self, path: &Path, name: &str) -> Result<()> {
        let r = vmlab_disk::ops::snapshot_delete(path, name);
        disk("snapshot -d", path, r)
    }

    fn export_overlay(
        &self,
        src: &Path,
        snapshot: &str,
        backing: &Path,
        dest: &Path,
    ) -> Result<()> {
        let r = vmlab_disk::ops::convert(src, dest, Some(snapshot), Some(backing), Some("qcow2"));
        disk("convert", src, r)
    }

    fn commit(&self, top: &Path) -> Result<()> {
        disk("commit", top, vmlab_disk::ops::commit(top))
    }
}

/// Run vmlab's own tests against either tool (the D15 parity measure).
#[cfg(test)]
pub(crate) mod testing {
    use std::cell::{Cell, RefCell};
    use tokio::sync::oneshot;

    use super::Backend;

    type Pause = (oneshot::Sender<()>, oneshot::Receiver<()>);

    thread_local! {
        static OVERRIDE: Cell<Option<Backend>> = const { Cell::new(None) };
        static PAUSE: RefCell<Option<(&'static str, Pause)>> = const { RefCell::new(None) };
    }

    pub(crate) fn pause_next(op: &'static str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, wait) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        PAUSE.with(|pause| pause.replace(Some((op, (entered, resume)))));
        (wait, release)
    }

    pub(super) fn pause_for(op: &str) -> Option<Pause> {
        PAUSE.with(|pause| {
            let mut pause = pause.borrow_mut();
            if pause.as_ref().is_some_and(|(wanted, _)| *wanted == op) {
                pause.take().map(|(_, channels)| channels)
            } else {
                None
            }
        })
    }

    pub(super) fn current() -> Option<Backend> {
        OVERRIDE.with(Cell::get)
    }

    /// Restores the previous selection when dropped.
    pub(crate) struct Selected(Option<Backend>);

    impl Drop for Selected {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(self.0));
        }
    }

    /// Whether `b` can run on this host: `qemu-img` must be on PATH.
    pub(crate) fn available(b: Backend) -> bool {
        b.required_binary().is_none_or(|bin| {
            std::env::var_os("PATH")
                .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        })
    }

    /// Select `b` on this thread for as long as the guard lives, or `None`
    /// (with a note) when it cannot run here and the test should skip. The
    /// async operations read the selection before leaving the thread, so a
    /// current-thread `#[tokio::test]` sees it throughout.
    pub(crate) fn select(b: Backend) -> Option<Selected> {
        if !available(b) {
            eprintln!("skipping: {} not found on PATH", b.tool().name());
            return None;
        }
        let previous = OVERRIDE.with(|o| o.replace(Some(b)));
        Some(Selected(previous))
    }

    /// One test per image tool around a body fn of the same name:
    /// `on_both_tools!(async my_test);` gives `my_test::qemu_img` and
    /// `my_test::vmlab_disk`, each running `my_test()` under that tool.
    macro_rules! on_both_tools {
        (async $name:ident) => {
            mod $name {
                use $crate::template::qimg::{Backend, testing::select};

                #[tokio::test]
                async fn qemu_img() {
                    if let Some(_tool) = select(Backend::QemuImg) {
                        super::$name().await
                    }
                }

                #[tokio::test]
                async fn vmlab_disk() {
                    if let Some(_tool) = select(Backend::VmlabDisk) {
                        super::$name().await
                    }
                }
            }
        };
        ($name:ident) => {
            mod $name {
                use $crate::template::qimg::{Backend, testing::select};

                #[test]
                fn qemu_img() {
                    if let Some(_tool) = select(Backend::QemuImg) {
                        super::$name()
                    }
                }

                #[test]
                fn vmlab_disk() {
                    if let Some(_tool) = select(Backend::VmlabDisk) {
                        super::$name()
                    }
                }
            }
        };
    }
    pub(crate) use on_both_tools;
}

#[cfg(test)]
mod tests {
    use super::testing::{on_both_tools, select};
    use super::*;

    async fn blank_create_and_info() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.qcow2");
        create_blank(&disk, 64 << 20).await.unwrap();
        let info = image_info(&disk).await.unwrap();
        assert_eq!(info.virtual_size, 64 << 20);
        assert!(info.backing_file.is_none());
        assert!(info.actual_size < 64 << 20, "blank qcow2 must be sparse");
        assert!(info.snapshots.is_empty());
        // A second create replaces the file, as `qemu-img create` does.
        create_blank(&disk, 32 << 20).await.unwrap();
        assert_eq!(image_info(&disk).await.unwrap().virtual_size, 32 << 20);
    }
    on_both_tools!(async blank_create_and_info);

    async fn linked_clone_records_absolute_backing() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        let clone = dir.path().join("clone.qcow2");
        create_blank(&base, 32 << 20).await.unwrap();
        create_linked_clone(&base, &clone).await.unwrap();
        let info = image_info(&clone).await.unwrap();
        assert_eq!(info.virtual_size, 32 << 20, "clone inherits virtual size");
        let backing = info.backing_file.expect("clone must have a backing file");
        assert!(
            backing.is_absolute(),
            "backing path must be absolute: {backing:?}"
        );
        assert_eq!(backing, base.canonicalize().unwrap());
        assert_eq!(info.full_backing_file, Some(backing));
    }
    on_both_tools!(async linked_clone_records_absolute_backing);

    /// `rebase -u` records the backing string as written; the full backing
    /// path resolves it against the image's directory, and info answers even
    /// when that file does not exist.
    async fn rebase_unsafe_records_a_relative_backing() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        let sub = dir.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let overlay = sub.join("disk.qcow2");
        create_blank(&base, 8 << 20).await.unwrap();
        create_linked_clone(&base, &overlay).await.unwrap();
        rebase_unsafe(&overlay, "../../base.qcow2").await.unwrap();
        let info = image_info(&overlay).await.unwrap();
        assert_eq!(
            info.backing_file.as_deref(),
            Some(Path::new("../../base.qcow2"))
        );
        assert_eq!(info.full_backing_file, Some(sub.join("../../base.qcow2")));
        std::fs::remove_file(&base).unwrap();
        let info = image_info(&overlay).await.unwrap();
        assert_eq!(info.virtual_size, 8 << 20);
        assert_eq!(info.full_backing_file, Some(sub.join("../../base.qcow2")));
    }
    on_both_tools!(async rebase_unsafe_records_a_relative_backing);

    async fn resize_grows() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.qcow2");
        create_blank(&disk, 16 << 20).await.unwrap();
        resize(&disk, 64 << 20).await.unwrap();
        assert_eq!(image_info(&disk).await.unwrap().virtual_size, 64 << 20);
    }
    on_both_tools!(async resize_grows);

    /// Offline snapshots are listed by info and go away on delete.
    async fn snapshots_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.qcow2");
        create_blank(&disk, 16 << 20).await.unwrap();
        snapshot_create(&disk, "one").await.unwrap();
        snapshot_create(&disk, "two").await.unwrap();
        let names = |i: ImageInfo| i.snapshots.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(image_info(&disk).await.unwrap()), ["one", "two"]);
        snapshot_apply(&disk, "one").await.unwrap();
        snapshot_delete(&disk, "one").await.unwrap();
        assert_eq!(names(image_info(&disk).await.unwrap()), ["two"]);
        assert!(snapshot_apply(&disk, "one").await.is_err());
    }
    on_both_tools!(async snapshots_round_trip);

    /// Converting a linked clone gives a standalone qcow2 of the same size;
    /// a raw source converts too.
    async fn convert_flattens() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base.qcow2");
        let clone = dir.path().join("clone.qcow2");
        let flat = dir.path().join("flat.qcow2");
        create_blank(&base, 8 << 20).await.unwrap();
        create_linked_clone(&base, &clone).await.unwrap();
        convert_to_qcow2(&clone, &flat).await.unwrap();
        let info = image_info(&flat).await.unwrap();
        assert!(info.backing_file.is_none());
        assert_eq!(info.virtual_size, 8 << 20);

        let raw = dir.path().join("vol.img");
        std::fs::write(&raw, vec![0x5a; 1 << 20]).unwrap();
        let from_raw = dir.path().join("raw.qcow2");
        convert_to_qcow2(&raw, &from_raw).await.unwrap();
        assert_eq!(image_info(&from_raw).await.unwrap().virtual_size, 1 << 20);
        assert_eq!(image_info(&raw).await.unwrap().virtual_size, 1 << 20);
    }
    on_both_tools!(async convert_flattens);

    #[tokio::test]
    async fn qemu_img_failure_carries_stderr() {
        let Some(_tool) = select(Backend::QemuImg) else {
            return;
        };
        let err = image_info(Path::new("/nonexistent/nope.qcow2"))
            .await
            .unwrap_err();
        match err {
            ImageError::Failed {
                stderr, command, ..
            } => {
                assert!(!stderr.is_empty(), "stderr should be captured");
                assert!(command.starts_with("qemu-img info"), "{command}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn vmlab_disk_failure_names_the_operation_and_path() {
        let Some(_tool) = select(Backend::VmlabDisk) else {
            return;
        };
        let err = image_info(Path::new("/nonexistent/nope.qcow2"))
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(
            matches!(err, ImageError::Disk { op: "info", .. }),
            "{err:?}"
        );
        assert!(text.contains("/nonexistent/nope.qcow2"), "{text}");
    }

    async fn linked_clone_missing_backing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let err = create_linked_clone(
            &dir.path().join("missing.qcow2"),
            &dir.path().join("clone.qcow2"),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ImageError::Backing { .. }), "{err:?}");
    }
    on_both_tools!(async linked_clone_missing_backing_fails);

    #[test]
    fn the_default_tool_is_vmlab_disk() {
        assert_eq!(Backend::parse("vmlab-disk"), Some(Backend::VmlabDisk));
        assert_eq!(Backend::parse(" qemu-img "), Some(Backend::QemuImg));
        assert_eq!(Backend::parse("qemu"), None);
        assert_eq!(Backend::VmlabDisk.required_binary(), None);
        assert_eq!(Backend::QemuImg.required_binary(), Some("qemu-img"));
        if std::env::var_os(TOOL_ENV).is_none() {
            assert_eq!(Backend::from_env(), Backend::VmlabDisk);
        }
    }
}
