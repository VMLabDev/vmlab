//! The template store (PRD §4, §6.1, §6.2, §7.1).
//!
//! Layout: `<root>/<arch>/<name>/<version>/{disk.qcow2, template.wcl}`,
//! plus `variants/<variant>/disk.qcow2` per sealed variant — a qcow2 overlay
//! whose backing file is the base, `../../disk.qcow2`, by relative path
//! (§6.6).
//! Reads are lock-free; every mutation (install / remove / import) holds
//! an exclusive flock on `<root>/.lock` so concurrent vmlab processes
//! cannot corrupt the store (PRD §3). The only way content enters the
//! store is an atomic `rename(2)` of a fully staged directory, so a
//! failed build or import leaves nothing behind (PRD §6.1).

use std::cmp::Ordering;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use nix::fcntl::{Flock, FlockArg};
use sha2::{Digest, Sha256};

use super::meta::{META_FILE, TemplateMeta, VARIANTS_DIR, variant_disk_rel};
use crate::config::model::{BASE_VARIANT, VariantRef};

/// Disk image file name inside a version directory.
pub const DISK_FILE: &str = "disk.qcow2";
const LOCK_FILE: &str = ".lock";
const STAGING_PREFIX: &str = ".staging-";

/// One installed template version, as stored: its directory, its base disk,
/// and its metadata as the file records it — base fields and the variant
/// blocks whose overlays are present (a variant counts only when both its
/// block and its disk exist, §6.6).
///
/// Nothing that boots a machine reads this directly: [`Self::select`] turns
/// it into the disk and effective metadata a reference names.
#[derive(Debug, Clone)]
pub struct TemplateVersion {
    pub dir: PathBuf,
    /// The base disk, `<version>/disk.qcow2`.
    pub disk_path: PathBuf,
    pub meta: TemplateMeta,
}

pub use super::meta::Purpose;

/// A reference resolved to one disk of one version (§6.6): the disk a clone
/// is backed on, and the effective metadata — the base's overridden by the
/// variant's ([`TemplateMeta::effective`]) — that everything downstream reads.
#[derive(Debug, Clone)]
pub struct ResolvedTemplate {
    /// The version directory.
    pub dir: PathBuf,
    /// The base disk, or the variant's overlay.
    pub disk_path: PathBuf,
    /// Effective metadata; lists no variants.
    pub meta: TemplateMeta,
    /// The variant resolved to; `None` for the base.
    pub variant: Option<String>,
}

impl ResolvedTemplate {
    /// `<arch>/<name>@<version>[#<variant>]` — exactly what was resolved.
    pub fn reference(&self) -> String {
        let m = &self.meta;
        match &self.variant {
            Some(v) => format!("{}/{}@{}#{v}", m.arch, m.name, m.version),
            None => format!("{}/{}@{}", m.arch, m.name, m.version),
        }
    }
}

impl TemplateVersion {
    /// `<arch>/<name>@<version>`.
    pub fn reference(&self) -> String {
        format!(
            "{}/{}@{}",
            self.meta.arch, self.meta.name, self.meta.version
        )
    }

    /// Absolute path of a variant's overlay.
    pub fn variant_disk(&self, name: &str) -> PathBuf {
        self.dir.join(variant_disk_rel(name))
    }

    /// Every disk this version holds — the base, then each variant overlay.
    /// What a removal of the whole version deletes.
    pub fn disks(&self) -> Vec<PathBuf> {
        let mut out = vec![self.disk_path.clone()];
        out.extend(
            self.meta
                .variants
                .iter()
                .map(|v| self.variant_disk(&v.name)),
        );
        out
    }

    /// The variant names, each marked when it is the default, for messages.
    pub fn variant_list(&self) -> String {
        self.meta.variant_list()
    }

    /// Resolve the `#<variant>` part of a reference against this version
    /// (§6.6) — [`TemplateMeta::select_variant`]'s answer, turned into the
    /// disk it names and its effective metadata.
    pub fn select(
        &self,
        variant: Option<&VariantRef>,
        purpose: Purpose,
    ) -> Result<ResolvedTemplate> {
        let chosen = self
            .meta
            .select_variant(variant, purpose)
            .map_err(anyhow::Error::msg)?;
        Ok(ResolvedTemplate {
            dir: self.dir.clone(),
            disk_path: match chosen {
                Some(v) => self.variant_disk(&v.name),
                None => self.disk_path.clone(),
            },
            meta: self.meta.effective(chosen),
            variant: chosen.map(|v| v.name.clone()),
        })
    }
}

/// Handle on a template store root. Callers normally pass
/// [`crate::paths::template_store_dir()`]; tests pass temp dirs.
#[derive(Debug, Clone)]
pub struct TemplateStore {
    root: PathBuf,
}

impl TemplateStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn version_dir(&self, arch: &str, name: &str, version: &str) -> PathBuf {
        self.root.join(arch).join(name).join(version)
    }

    // ---- reads (lock-free) -------------------------------------------------

    /// All templates in the store, sorted by arch, name, then version
    /// (natural order), each with the variants whose disks are installed.
    /// Entries with unreadable metadata — invalid variant blocks included —
    /// are skipped with a warning rather than failing the whole listing.
    pub fn list(&self) -> Result<Vec<TemplateMeta>> {
        let mut out = Vec::new();
        for arch in subdirs(&self.root)? {
            for name in subdirs(&arch)? {
                for version in subdirs(&name)? {
                    let meta_path = version.join(META_FILE);
                    if !meta_path.is_file() {
                        continue;
                    }
                    match read_meta(&version) {
                        Ok(meta) => out.push(meta),
                        Err(e) => tracing::warn!(
                            "skipping unreadable template metadata {}: {e:#}",
                            meta_path.display()
                        ),
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            a.arch
                .cmp(&b.arch)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| compare_versions(&a.version, &b.version))
        });
        Ok(out)
    }

    /// Whether `<arch>/<name>` exists, at `version` if given, at any
    /// version otherwise.
    pub fn exists(&self, arch: &str, name: &str, version: Option<&str>) -> bool {
        match version {
            Some(v) => self.version_dir(arch, name, v).join(META_FILE).is_file(),
            None => !self.versions_of(arch, name).unwrap_or_default().is_empty(),
        }
    }

    /// Resolve `<arch>/<name>[@version][#variant]` to the disk a clone or a
    /// build is backed on, with its effective metadata (§6.6): the version by
    /// [`Self::resolve_version`], then the variant by
    /// [`TemplateVersion::select`].
    pub fn resolve(
        &self,
        arch: &str,
        name: &str,
        version: Option<&str>,
        variant: Option<&VariantRef>,
        purpose: Purpose,
    ) -> Result<ResolvedTemplate> {
        self.resolve_version(arch, name, version)?
            .select(variant, purpose)
    }

    /// Resolve `<arch>/<name>[@version]` to a store version. `version`
    /// of `None` selects the highest version by natural-sort order
    /// (PRD §6.2). A `version` that is a build-counter prefix (e.g. the
    /// 2-component `26100.1742`) resolves to the highest `<version>.<N>`
    /// sub-build present, falling back to an exact match (see
    /// [`resolve_version_pin`]).
    pub fn resolve_version(
        &self,
        arch: &str,
        name: &str,
        version: Option<&str>,
    ) -> Result<TemplateVersion> {
        let version = match version {
            Some(v) => resolve_version_pin(&self.versions_of(arch, name).unwrap_or_default(), v)
                .ok_or_else(|| anyhow!("template {arch}/{name}@{v} not found in the store"))?,
            None => self
                .versions_of(arch, name)?
                .into_iter()
                .max_by(|a, b| compare_versions(a, b))
                .ok_or_else(|| anyhow!("template {arch}/{name} not found in the store"))?,
        };
        self.installed(arch, name, &version)?
            .ok_or_else(|| anyhow!("template {arch}/{name}@{version} not found in the store"))
    }

    /// `<arch>/<name>@<version>` exactly as named — no pin resolution —
    /// when the store holds it.
    pub fn installed(
        &self,
        arch: &str,
        name: &str,
        version: &str,
    ) -> Result<Option<TemplateVersion>> {
        let dir = self.version_dir(arch, name, version);
        if !dir.join(META_FILE).is_file() {
            return Ok(None);
        }
        let meta = read_meta(&dir)?;
        let disk_path = dir.join(DISK_FILE);
        ensure!(
            disk_path.is_file(),
            "template {arch}/{name}@{version} is corrupt: missing {DISK_FILE}"
        );
        Ok(Some(TemplateVersion {
            disk_path,
            dir,
            meta,
        }))
    }

    /// Version directory names present for `<arch>/<name>` (those with
    /// metadata), unsorted.
    pub(crate) fn versions_of(&self, arch: &str, name: &str) -> Result<Vec<String>> {
        let mut versions = Vec::new();
        for dir in subdirs(&self.root.join(arch).join(name))? {
            if dir.join(META_FILE).is_file()
                && let Some(v) = dir.file_name().and_then(|n| n.to_str())
            {
                versions.push(v.to_string());
            }
        }
        Ok(versions)
    }

    // ---- mutations (exclusive flock) ----------------------------------------

    /// Atomically install a staged directory containing `disk.qcow2`.
    /// Writes `template.wcl` from `meta` into the staging dir, then
    /// renames it to `<arch>/<name>/<version>/` in one step — a failure
    /// anywhere leaves the store untouched (PRD §6.1). A staging dir on
    /// another filesystem is first copied into one inside the store, so the
    /// final step is still a rename. Refuses to replace an existing version
    /// unless `overwrite` is set.
    pub fn install(&self, staging_dir: &Path, meta: &TemplateMeta, overwrite: bool) -> Result<()> {
        let _lock = self.lock()?;
        self.install_locked(staging_dir, meta, overwrite)
    }

    fn install_locked(&self, staging: &Path, meta: &TemplateMeta, overwrite: bool) -> Result<()> {
        ensure!(
            staging.join(DISK_FILE).is_file(),
            "staging directory {} contains no {DISK_FILE}",
            staging.display()
        );
        let dest = self.version_dir(&meta.arch, &meta.name, &meta.version);
        if dest.exists() {
            if !overwrite {
                bail!(
                    "template {}/{}@{} already exists in the store (pass overwrite to replace)",
                    meta.arch,
                    meta.name,
                    meta.version
                );
            }
            fs::remove_dir_all(&dest)
                .with_context(|| format!("cannot replace {}", dest.display()))?;
        }
        meta.write_to(&staging.join(META_FILE))?;
        let parent = dest.parent().expect("version dir always has a parent");
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        self.move_in(staging, &dest, &|from, to| fs::rename(from, to))
    }

    /// Rename `staging` to `dest`. When `staging` is on another filesystem
    /// — a data dir whose cache and store are separate mounts — it is copied
    /// into a staging dir inside the store and renamed from there, so the
    /// version still appears in one step; the original is removed after.
    /// `rename` is [`fs::rename`] outside tests.
    fn move_in(
        &self,
        staging: &Path,
        dest: &Path,
        rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let moving = || format!("cannot move staged template into {}", dest.display());
        match rename(staging, dest) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                let local = StagingDir::create(&self.root)?;
                copy_tree(staging, local.path()).with_context(|| {
                    format!(
                        "cannot copy staged template {} into the store",
                        staging.display()
                    )
                })?;
                rename(local.path(), dest).with_context(moving)?;
                let _ = fs::remove_dir_all(staging);
                Ok(())
            }
            Err(e) => Err(e).with_context(moving),
        }
    }

    /// Remove `<arch>/<name>@<version>` — the base and every variant — or,
    /// with `variant`, that one variant alone (§6.6). `in_use` is handed the
    /// disks the removal would delete and reports why they cannot go (clones
    /// backed by one of them, PRD §7.1); a `Some(reason)` refuses the removal
    /// unless `force` is set.
    ///
    /// A variant removal deletes `variants/<name>/` and rewrites
    /// `template.wcl` without its block. It refuses the default variant while
    /// others remain, and the last variant — either would leave the
    /// unsealed base as the disk a bare reference selects.
    pub fn remove(
        &self,
        arch: &str,
        name: &str,
        version: &str,
        variant: Option<&str>,
        force: bool,
        in_use: &dyn Fn(&[PathBuf]) -> Option<String>,
    ) -> Result<()> {
        let _lock = self.lock()?;
        if let Some(variant) = variant {
            return self.remove_variant_locked(arch, name, version, variant, force, in_use);
        }
        let dir = self.version_dir(arch, name, version);
        if !dir.is_dir() {
            bail!("template {arch}/{name}@{version} not found in the store");
        }
        match read_meta(&dir) {
            Ok(meta) => {
                let found = TemplateVersion {
                    disk_path: dir.join(DISK_FILE),
                    dir: dir.clone(),
                    meta,
                };
                if let Some(reason) = in_use(&found.disks())
                    && !force
                {
                    bail!(
                        "refusing to remove template {arch}/{name}@{version}: {reason} \
                         (use --force to remove anyway)"
                    );
                }
            }
            Err(e) if !force => {
                return Err(e.context(format!(
                    "cannot read metadata for {arch}/{name}@{version} \
                     (use --force to remove anyway)"
                )));
            }
            Err(_) => {} // forced: remove the corrupt entry regardless
        }
        fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
        // Tidy now-empty parents; ignore failures (non-empty is fine).
        let _ = fs::remove_dir(self.root.join(arch).join(name));
        let _ = fs::remove_dir(self.root.join(arch));
        Ok(())
    }

    fn remove_variant_locked(
        &self,
        arch: &str,
        name: &str,
        version: &str,
        variant: &str,
        force: bool,
        in_use: &dyn Fn(&[PathBuf]) -> Option<String>,
    ) -> Result<()> {
        let found = self
            .installed(arch, name, version)?
            .ok_or_else(|| anyhow!("template {arch}/{name}@{version} not found in the store"))?;
        let reference = format!("{arch}/{name}@{version}#{variant}");
        if variant == BASE_VARIANT {
            bail!(
                "`#{BASE_VARIANT}` is the version itself — remove {arch}/{name}@{version} to \
                 remove the base with its variants"
            );
        }
        let Some(v) = found.meta.variant(variant) else {
            if found.meta.variants.is_empty() {
                bail!("template {arch}/{name}@{version} has no variants");
            }
            bail!(
                "template {arch}/{name}@{version} has no variant `{variant}` (its variants: {})",
                found.variant_list()
            );
        };
        if found.meta.variants.len() == 1 {
            bail!(
                "refusing to remove {reference}: it is the version's only variant, and without it \
                 a bare reference would select the unsealed base — remove {arch}/{name}@{version} \
                 instead"
            );
        }
        if found.meta.default_variant().map(|d| d.name.as_str()) == Some(variant) {
            bail!(
                "refusing to remove {reference}: it is the default variant, and the version's \
                 other variants ({}) would be left with none — remove {arch}/{name}@{version} \
                 instead, or rebuild it with another default",
                found
                    .meta
                    .variants
                    .iter()
                    .filter(|o| o.name != variant)
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let disk = found.variant_disk(&v.name);
        if let Some(reason) = in_use(std::slice::from_ref(&disk))
            && !force
        {
            bail!("refusing to remove {reference}: {reason} (use --force to remove anyway)");
        }
        // The block goes first: a crash between the two steps leaves an
        // overlay with no block, which counts as not installed, rather than a
        // block naming a disk that is gone.
        let mut on_disk = TemplateMeta::read_from(&found.dir.join(META_FILE))?;
        on_disk.variants.retain(|o| o.name != variant);
        write_meta_atomic(&found.dir, &on_disk)?;
        let vdir = found.dir.join(VARIANTS_DIR).join(variant);
        fs::remove_dir_all(&vdir).with_context(|| format!("cannot remove {}", vdir.display()))?;
        let _ = fs::remove_dir(found.dir.join(VARIANTS_DIR));
        Ok(())
    }

    /// Export a template version as a single portable `tar.zst` archive
    /// holding `template.wcl`, `disk.qcow2` and every installed variant's
    /// `variants/<name>/disk.qcow2` (PRD §6.2, §6.6). The `template.wcl` it
    /// carries is the one the store serves — variant blocks whose overlay is
    /// missing are left out. Read-only, so no lock.
    pub fn export(&self, arch: &str, name: &str, version: Option<&str>, out: &Path) -> Result<()> {
        let resolved = self.resolve_version(arch, name, version)?;
        let file = File::create(out).with_context(|| format!("cannot create {}", out.display()))?;
        let encoder = zstd::Encoder::new(file, 0).context("cannot start zstd stream")?;
        let mut tar = tar::Builder::new(encoder);
        let wcl = resolved.meta.to_wcl();
        let mut header = tar::Header::new_gnu();
        header.set_size(wcl.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(resolved.meta.created.timestamp().max(0) as u64);
        header.set_cksum();
        tar.append_data(&mut header, META_FILE, wcl.as_bytes())
            .context("cannot archive template.wcl")?;
        tar.append_path_with_name(&resolved.disk_path, DISK_FILE)
            .context("cannot archive disk.qcow2")?;
        for v in &resolved.meta.variants {
            let rel = variant_disk_rel(&v.name);
            tar.append_path_with_name(resolved.dir.join(&rel), &rel)
                .with_context(|| format!("cannot archive {}", rel.display()))?;
        }
        let encoder = tar.into_inner().context("cannot finish archive")?;
        encoder.finish().context("cannot finish zstd stream")?;
        Ok(())
    }

    /// Import an archive produced by [`Self::export`]. Unpacks to a
    /// staging dir inside the store root (same filesystem, so the final
    /// move is an atomic rename), verifies the base disk against its
    /// recorded digest and every variant against its own and its base's
    /// (§6.6), then installs.
    pub fn import(&self, archive: &Path, overwrite: bool) -> Result<TemplateMeta> {
        let _lock = self.lock()?;
        let staging = StagingDir::create(&self.root)?;

        let file =
            File::open(archive).with_context(|| format!("cannot open {}", archive.display()))?;
        let decoder = zstd::Decoder::new(file).context("cannot read zstd stream")?;
        tar::Archive::new(decoder)
            .unpack(staging.path())
            .with_context(|| format!("cannot unpack {}", archive.display()))?;

        let meta = TemplateMeta::read_from(&staging.path().join(META_FILE))
            .with_context(|| format!("{} is not a vmlab template archive", archive.display()))?;
        let disk = staging.path().join(DISK_FILE);
        ensure!(
            disk.is_file(),
            "{} is not a vmlab template archive: missing {DISK_FILE}",
            archive.display()
        );
        let mut base_digest = None;
        if let Some(expected) = &meta.sha256 {
            let actual = sha256_file(&disk)?;
            ensure!(
                actual.eq_ignore_ascii_case(expected),
                "disk image digest mismatch for {}/{}@{}: expected {expected}, got {actual} \
                 — archive is corrupt",
                meta.arch,
                meta.name,
                meta.version
            );
            base_digest = Some(actual);
        }
        verify_variants(staging.path(), &meta, base_digest.as_deref())?;
        self.install_locked(staging.path(), &meta, overwrite)?;
        Ok(meta)
    }

    /// Install a staged directory holding a whole version — `disk.qcow2` and
    /// the `variants/<name>/disk.qcow2` of every variant `meta` lists — after
    /// checking each variant belongs to this base and its disk is the one
    /// its block names (§6.6). What a pull of a version the store does not
    /// hold yet installs.
    pub fn install_with_variants(
        &self,
        staging_dir: &Path,
        meta: &TemplateMeta,
        overwrite: bool,
    ) -> Result<()> {
        verify_variants(staging_dir, meta, meta.sha256.as_deref())?;
        self.install(staging_dir, meta, overwrite)
    }

    /// Add variants to a version the store already holds (§6.6): each
    /// `variants/<name>/disk.qcow2` staged under `staging_dir` moves in, and
    /// its block is merged into `template.wcl`, replacing a block of the same
    /// name and keeping every other. An incoming default takes the default
    /// from any variant already installed.
    ///
    /// Each variant must have been built on the installed base. The overlay
    /// moves first and the block is written after, so a failure between the
    /// two leaves an overlay with no block — which does not count — never a
    /// block naming a disk that is not there.
    pub fn add_variants(
        &self,
        arch: &str,
        name: &str,
        version: &str,
        staging_dir: &Path,
        variants: &[super::meta::VariantMeta],
    ) -> Result<TemplateMeta> {
        let _lock = self.lock()?;
        let found = self
            .installed(arch, name, version)?
            .ok_or_else(|| anyhow!("template {arch}/{name}@{version} not found in the store"))?;
        let mut incoming = found.meta.clone();
        incoming.variants = variants.to_vec();
        verify_variants(staging_dir, &incoming, found.meta.sha256.as_deref())?;

        let mut on_disk = TemplateMeta::read_from(&found.dir.join(META_FILE))?;
        for v in variants {
            let vdir = found.dir.join(VARIANTS_DIR).join(&v.name);
            if vdir.exists() {
                fs::remove_dir_all(&vdir)
                    .with_context(|| format!("cannot replace {}", vdir.display()))?;
            }
            fs::create_dir_all(found.dir.join(VARIANTS_DIR))?;
            fs::rename(staging_dir.join(VARIANTS_DIR).join(&v.name), &vdir)
                .with_context(|| format!("cannot move variant `{}` into the store", v.name))?;
            if v.default {
                for other in &mut on_disk.variants {
                    other.default = false;
                }
            }
            match on_disk.variants.iter_mut().find(|o| o.name == v.name) {
                Some(slot) => *slot = v.clone(),
                None => on_disk.variants.push(v.clone()),
            }
        }
        write_meta_atomic(&found.dir, &on_disk)?;
        read_meta(&found.dir)
    }

    /// Exclusive advisory lock on the store. Held for the lifetime of
    /// the returned guard; concurrent mutators block here.
    fn lock(&self) -> Result<Flock<File>> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("cannot create store root {}", self.root.display()))?;
        let path = self.root.join(LOCK_FILE);
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open lock file {}", path.display()))?;
        Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, errno)| anyhow!("cannot lock template store: {errno}"))
    }
}

/// A version directory's metadata with the variants it can actually serve:
/// a variant block whose overlay is missing does not count (§6.6).
pub(crate) fn read_meta(dir: &Path) -> Result<TemplateMeta> {
    let mut meta = TemplateMeta::read_from(&dir.join(META_FILE))?;
    meta.variants
        .retain(|v| dir.join(variant_disk_rel(&v.name)).is_file());
    Ok(meta)
}

/// Replace `template.wcl` in `dir` by writing a sibling and renaming it over,
/// so a reader never sees half a file.
fn write_meta_atomic(dir: &Path, meta: &TemplateMeta) -> Result<()> {
    let tmp = dir.join(format!(".{META_FILE}.{:08x}", rand::random::<u32>()));
    meta.write_to(&tmp)?;
    fs::rename(&tmp, dir.join(META_FILE)).with_context(|| {
        let _ = fs::remove_file(&tmp);
        format!("cannot replace {}", dir.join(META_FILE).display())
    })
}

/// Streaming hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    crate::hashing::feed(&mut file, &mut hasher)
        .with_context(|| format!("cannot hash {}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

/// The backing file a variant overlay names: its base, two directories up
/// (§6.6). Relative, so a moved store or an imported export keeps working.
pub const VARIANT_BACKING: &str = "../../disk.qcow2";

/// The backing file name a qcow2 image's header records, `None` when it has
/// none. Read straight from the header — no `qemu-img`, and no lock to
/// contend with.
pub fn qcow2_backing_file(path: &Path) -> Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut header = [0u8; 20];
    f.read_exact(&mut header)
        .with_context(|| format!("{} is too short to be a qcow2 image", path.display()))?;
    ensure!(
        header[..4] == *b"QFI\xfb",
        "{} is not a qcow2 image",
        path.display()
    );
    let offset = u64::from_be_bytes(header[8..16].try_into().expect("8 bytes"));
    let size = u32::from_be_bytes(header[16..20].try_into().expect("4 bytes"));
    if offset == 0 || size == 0 {
        return Ok(None);
    }
    ensure!(
        size <= 1023,
        "{} records a {size}-byte backing file name",
        path.display()
    );
    let mut name = vec![0u8; size as usize];
    f.seek(SeekFrom::Start(offset))?;
    f.read_exact(&mut name)
        .with_context(|| format!("cannot read {}'s backing file name", path.display()))?;
    Ok(Some(String::from_utf8_lossy(&name).into_owned()))
}

/// Make a variant overlay name its base by [`VARIANT_BACKING`], the store's
/// layout (§6.6). `true` when it had to be rewritten — which changes its
/// bytes, so the caller re-hashes it.
pub fn ensure_variant_backing(overlay: &Path) -> Result<bool> {
    if qcow2_backing_file(overlay)?.as_deref() == Some(VARIANT_BACKING) {
        return Ok(false);
    }
    let out = std::process::Command::new("qemu-img")
        .args(["rebase", "-u", "-b", VARIANT_BACKING, "-F", "qcow2"])
        .arg(overlay)
        .output()
        .context("cannot run qemu-img")?;
    ensure!(
        out.status.success(),
        "cannot point {} at {VARIANT_BACKING}: {}",
        overlay.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(true)
}

/// Check every variant a staged version directory's `meta` lists, before it
/// enters the store (§6.6): its overlay is there, hashes to its `sha256`,
/// names the base by [`VARIANT_BACKING`], and was built on this base —
/// `base_sha256` equal to the digest of the staged `disk.qcow2`, which
/// `base_digest` is when known.
fn verify_variants(dir: &Path, meta: &TemplateMeta, base_digest: Option<&str>) -> Result<()> {
    if meta.variants.is_empty() {
        return Ok(());
    }
    let reference = format!("{}/{}@{}", meta.arch, meta.name, meta.version);
    let base = match base_digest {
        Some(d) => d.to_string(),
        None => sha256_file(&dir.join(DISK_FILE))?,
    };
    for v in &meta.variants {
        let overlay = dir.join(variant_disk_rel(&v.name));
        ensure!(
            overlay.is_file(),
            "variant `{}` of {reference} has no {}",
            v.name,
            variant_disk_rel(&v.name).display()
        );
        let actual = sha256_file(&overlay)?;
        ensure!(
            digest_matches(&actual, &v.sha256),
            "variant `{}` of {reference}: disk digest mismatch, expected {}, got {actual} \
             — the archive is corrupt",
            v.name,
            v.sha256
        );
        ensure!(
            digest_matches(&base, &v.base_sha256),
            "variant `{}` of {reference} was built on base {} but the base here is {base} \
             — refusing a variant that does not belong to this base",
            v.name,
            v.base_sha256
        );
        let backing = qcow2_backing_file(&overlay)?;
        ensure!(
            backing.as_deref() == Some(VARIANT_BACKING),
            "variant `{}` of {reference} is backed by {} rather than {VARIANT_BACKING}",
            v.name,
            backing.as_deref().unwrap_or("nothing")
        );
    }
    Ok(())
}

/// Two hex digests, either maybe `sha256:`-prefixed, compared without case.
fn digest_matches(a: &str, b: &str) -> bool {
    let strip = |d: &str| d.strip_prefix("sha256:").unwrap_or(d).to_ascii_lowercase();
    strip(a) == strip(b)
}

/// Natural-sort version comparison: the strings are split into numeric
/// and non-numeric runs; numeric runs compare as numbers, text runs
/// lexically. So `10.2 > 9.9`, `26100.1 > 26100`, `1.2-rc2 > 1.2-rc1`.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (a_runs, b_runs) = (runs(a), runs(b));
    for (x, y) in a_runs.iter().zip(&b_runs) {
        let ord = match (x, y) {
            (Run::Num(x), Run::Num(y)) => compare_digits(x, y),
            (Run::Text(x), Run::Text(y)) => x.cmp(y),
            // A number sorts before text at the same position ("1.2" vs "1.b").
            (Run::Num(_), Run::Text(_)) => Ordering::Less,
            (Run::Text(_), Run::Num(_)) => Ordering::Greater,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    // Shared prefix: more runs wins ("1.2.1" > "1.2"); identical run
    // values fall back to the raw strings ("01" vs "1") for determinism.
    a_runs.len().cmp(&b_runs.len()).then_with(|| a.cmp(b))
}

/// Next build version for a fixed `prefix` (the template's declared `version`):
/// append a trailing build-counter component. `N` = the highest existing
/// `<prefix>.<N>` plus one, or `0` if none exist yet — so `26100.1742` →
/// `26100.1742.0`, `.1`, `.2`, … Only exact `<prefix>.<digits>` forms count
/// (one extra all-numeric component); the bare prefix and unrelated versions are
/// ignored, so changing the declared prefix restarts the counter at `.0`.
pub fn next_subbuild(prefix: &str, existing: &[String]) -> String {
    let dotted = format!("{prefix}.");
    let max = existing.iter().filter_map(|v| {
        let rest = v.strip_prefix(&dotted)?;
        if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        rest.parse::<u64>().ok()
    });
    match max.max() {
        Some(n) => format!("{prefix}.{}", n + 1),
        None => format!("{prefix}.0"),
    }
}

/// Resolve a version `pin` against the available `versions`. Prefers the highest
/// `<pin>.<N>` sub-build — so a build-counter prefix like `26100.1742` resolves
/// to the latest `26100.1742.<N>` — and falls back to an exact `<pin>` match
/// (so a full version, a moving alias, or a template with no sub-builds still
/// resolves). Returns `None` when neither is present. Mirrors
/// [`next_subbuild`]'s notion of a sub-build: exactly one extra all-numeric
/// component, so `26100.1742` matches `26100.1742.3` but not `26100.17425` or
/// `26100.1742.3.1`.
pub fn resolve_version_pin(versions: &[String], pin: &str) -> Option<String> {
    let dotted = format!("{pin}.");
    let sub = versions
        .iter()
        .filter(|v| {
            v.strip_prefix(&dotted)
                .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
        })
        .max_by(|a, b| compare_versions(a, b));
    if let Some(s) = sub {
        return Some(s.clone());
    }
    versions.iter().find(|v| v.as_str() == pin).cloned()
}

/// Resolve a version `pin` against a registry repository's published `tags`,
/// taking `latest` — the version the repository's moving `latest` tag names,
/// when known — as the authority on which version is newest and which
/// versioning scheme is current (PRD §6.4). Tag order alone is not: a
/// repository can keep tags from a retired scheme that sort above the current
/// one (`24.04.20260520` above `24.04.4.2`).
///
/// 1. A pin `latest`'s version falls under (equal to it, or a dot-component
///    prefix of it) resolves to that version, so `24.04` and `24.04.4` both
///    name what `latest` names.
/// 2. Otherwise the highest tag under the pin with as many components as
///    `latest`'s version, so a pin on an older line still skips the tags of a
///    scheme `latest` no longer uses.
/// 3. Otherwise [`resolve_version_pin`]'s rule, which is also the whole rule
///    when `latest` is unknown.
pub fn resolve_registry_pin(tags: &[String], pin: &str, latest: Option<&str>) -> Option<String> {
    let dotted = format!("{pin}.");
    let under = |v: &str| {
        v.strip_prefix(&dotted).is_some_and(|rest| {
            rest.split('.')
                .all(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    if let Some(latest) = latest.filter(|l| tags.iter().any(|t| t == l)) {
        if latest == pin || under(latest) {
            return Some(latest.to_string());
        }
        let shape = latest.split('.').count();
        let same_scheme = tags
            .iter()
            .filter(|t| under(t) && t.split('.').count() == shape)
            .max_by(|a, b| compare_versions(a, b));
        if let Some(t) = same_scheme {
            return Some(t.clone());
        }
    }
    resolve_version_pin(tags, pin)
}

enum Run<'a> {
    Num(&'a str),
    Text(&'a str),
}

fn runs(s: &str) -> Vec<Run<'_>> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let numeric = bytes[start].is_ascii_digit();
        let mut end = start + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() == numeric {
            end += 1;
        }
        let run = &s[start..end];
        out.push(if numeric {
            Run::Num(run)
        } else {
            Run::Text(run)
        });
        start = end;
    }
    out
}

/// Compare two ASCII digit strings numerically without overflow:
/// strip leading zeros, then longer wins, then lexicographic.
fn compare_digits(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Immediate subdirectories, skipping dot-entries (`.lock`, staging
/// dirs). A missing parent is an empty result, not an error.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read {}", dir.display()))?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Staging directory inside the store root, removed on drop unless the
/// install rename already consumed it.
/// Copy the directory tree at `from` into the existing directory `to`.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir(&target)?;
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    fn create(root: &Path) -> Result<Self> {
        let path = root.join(format!("{STAGING_PREFIX}{:08x}", rand::random::<u32>()));
        fs::create_dir_all(&path)
            .with_context(|| format!("cannot create staging dir {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(arch: &str, name: &str, version: &str) -> TemplateMeta {
        TemplateMeta {
            name: name.into(),
            arch: arch.into(),
            version: version.into(),
            profile: Some("linux".into()),
            cpus: Some(2),
            memory: Some(4 << 30),
            disk: Some(20 << 30),
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            cpu_model: None,
            accel: None,
            created: "2026-06-12T00:00:00Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
            variants: Vec::new(),
        }
    }

    /// Stage a fake disk (store code never parses qcow2 content) and
    /// install it.
    fn install(store: &TemplateStore, meta: &TemplateMeta, contents: &[u8]) -> Result<()> {
        fs::create_dir_all(store.root()).unwrap();
        let staging = tempfile::tempdir_in(store.root()).unwrap();
        fs::write(staging.path().join(DISK_FILE), contents).unwrap();
        // tempdir would double-delete after a successful rename; keep it.
        let path = staging.keep();
        store.install(&path, meta, false)
    }

    fn new_store() -> (tempfile::TempDir, TemplateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = TemplateStore::new(dir.path().join("templates"));
        (dir, store)
    }

    // ---- compare_versions ---------------------------------------------------

    #[test]
    fn version_natural_order() {
        use Ordering::*;
        assert_eq!(compare_versions("10.2", "9.9"), Greater);
        assert_eq!(compare_versions("26100.1", "26100"), Greater);
        assert_eq!(compare_versions("1.2-rc2", "1.2-rc1"), Greater);
        assert_eq!(compare_versions("1.2", "1.2"), Equal);
        assert_eq!(compare_versions("2", "10"), Less);
        assert_eq!(compare_versions("1.10", "1.9"), Greater);
        assert_eq!(compare_versions("3.20", "3.20.1"), Less);
        assert_eq!(compare_versions("a", "b"), Less);
        // huge numerics must not overflow
        assert_eq!(
            compare_versions("99999999999999999999999999999999999999990", "9"),
            Greater
        );
    }

    // ---- next_subbuild ------------------------------------------------------

    #[test]
    fn next_subbuild_appends_and_increments_counter() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        // No existing builds for the prefix -> start at .0.
        assert_eq!(next_subbuild("26100.1742", &[]), "26100.1742.0");
        // Highest existing trailing component + 1.
        assert_eq!(
            next_subbuild("26100.1742", &s(&["26100.1742.0"])),
            "26100.1742.1"
        );
        assert_eq!(
            next_subbuild("26100.1742", &s(&["26100.1742.0", "26100.1742.3"])),
            "26100.1742.4"
        );
        // The bare prefix, a different prefix, and a non-dot continuation are all
        // ignored when counting -> still .0.
        assert_eq!(
            next_subbuild(
                "26100.1742",
                &s(&["26100.1742", "26100.1743", "26100.17425"])
            ),
            "26100.1742.0"
        );
        // Works for a declared 3-component prefix (counter becomes the 4th).
        assert_eq!(next_subbuild("3.23.4", &s(&["3.23.4.2"])), "3.23.4.3");
        assert_eq!(next_subbuild("3.23.4", &[]), "3.23.4.0");
    }

    // ---- resolve_version_pin ------------------------------------------------

    #[test]
    fn resolve_version_pin_prefers_latest_subbuild() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        // 2-component pin -> highest matching sub-build.
        assert_eq!(
            resolve_version_pin(
                &s(&["26100.1742.0", "26100.1742.3", "26100.1742.10"]),
                "26100.1742"
            )
            .as_deref(),
            Some("26100.1742.10")
        );
        // Sub-build wins even when an exact bare-prefix version also exists.
        assert_eq!(
            resolve_version_pin(&s(&["26100.1742", "26100.1742.0"]), "26100.1742").as_deref(),
            Some("26100.1742.0")
        );
        // No sub-build -> exact match fallback (full pins / aliases still work).
        assert_eq!(
            resolve_version_pin(&s(&["26100.1742.0", "latest"]), "26100.1742.0").as_deref(),
            Some("26100.1742.0")
        );
        assert_eq!(
            resolve_version_pin(&s(&["26100.1742.0", "latest"]), "latest").as_deref(),
            Some("latest")
        );
        // Unrelated / near-miss versions are ignored.
        assert_eq!(
            resolve_version_pin(
                &s(&["26100.1743.0", "26100.17425", "26100.1742.3.1"]),
                "26100.1742"
            )
            .as_deref(),
            None
        );
    }

    // ---- resolve_registry_pin -----------------------------------------------

    /// The tags `ghcr.io/vmlabdev/vmlab-templates/ubuntu-24.04` carried when
    /// the stale date-scheme tags were found sorting above `latest`.
    fn ubuntu_tags() -> Vec<String> {
        [
            "24.04.20260518",
            "latest",
            "24.04.20260519",
            "24.04.20260520",
            "24.04.4.0",
            "24.04.4.2",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn registry_pin_under_latest_resolves_to_latest() {
        let tags = ubuntu_tags();
        let latest = Some("24.04.4.2");
        // The bare release and the declared prefix both name latest, not the
        // date-scheme tags that sort above it.
        assert_eq!(
            resolve_registry_pin(&tags, "24.04", latest).as_deref(),
            Some("24.04.4.2")
        );
        assert_eq!(
            resolve_registry_pin(&tags, "24.04.4", latest).as_deref(),
            Some("24.04.4.2")
        );
        assert_eq!(
            resolve_registry_pin(&tags, "24.04.4.2", latest).as_deref(),
            Some("24.04.4.2")
        );
        // Full tags of either scheme still resolve to themselves.
        assert_eq!(
            resolve_registry_pin(&tags, "24.04.4.0", latest).as_deref(),
            Some("24.04.4.0")
        );
        assert_eq!(
            resolve_registry_pin(&tags, "24.04.20260519", latest).as_deref(),
            Some("24.04.20260519")
        );
        // The alias itself is an exact tag.
        assert_eq!(
            resolve_registry_pin(&tags, "latest", latest).as_deref(),
            Some("latest")
        );
    }

    #[test]
    fn registry_pin_under_latest_skips_a_newer_prerelease() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // 26100.1742.11 was pushed with --prerelease; latest stays on .10.
        let tags = s(&[
            "26100.1742.10",
            "26100.1742.11",
            "latest",
            "latest-prerelease",
        ]);
        assert_eq!(
            resolve_registry_pin(&tags, "26100.1742", Some("26100.1742.10")).as_deref(),
            Some("26100.1742.10")
        );
        // Named exactly, the prerelease is still reachable.
        assert_eq!(
            resolve_registry_pin(&tags, "26100.1742.11", Some("26100.1742.10")).as_deref(),
            Some("26100.1742.11")
        );
    }

    #[test]
    fn registry_pin_off_latest_prefers_latests_scheme() {
        let mut tags = ubuntu_tags();
        tags.push("26.04.1.0".into());
        // latest moved to another release: a 24.04 pin takes the highest
        // tag shaped like latest's version, not the date-scheme one.
        assert_eq!(
            resolve_registry_pin(&tags, "24.04", Some("26.04.1.0")).as_deref(),
            Some("24.04.4.2")
        );
        // Counter pins on an older line keep working.
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let tags = s(&["26100.1742.3", "26100.1742.10", "26100.1743.0", "latest"]);
        assert_eq!(
            resolve_registry_pin(&tags, "26100.1742", Some("26100.1743.0")).as_deref(),
            Some("26100.1742.10")
        );
    }

    #[test]
    fn registry_pin_without_latest_is_the_store_rule() {
        let tags = ubuntu_tags();
        // No latest (or one that names no published tag): the counter rule
        // alone, which is what tag order gives.
        for latest in [None, Some("24.04.9.9")] {
            assert_eq!(
                resolve_registry_pin(&tags, "24.04", latest).as_deref(),
                Some("24.04.20260520")
            );
        }
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            resolve_registry_pin(
                &s(&["26100.1742.0", "26100.1742.3", "26100.1742.10"]),
                "26100.1742",
                None
            )
            .as_deref(),
            Some("26100.1742.10")
        );
    }

    // ---- install / list / resolve --------------------------------------------

    #[test]
    fn install_list_resolve() {
        let (_tmp, store) = new_store();
        assert!(store.list().unwrap().is_empty());
        assert!(!store.exists("x86_64", "alpine", None));

        install(&store, &meta("x86_64", "alpine", "3.19"), b"old").unwrap();
        install(&store, &meta("x86_64", "alpine", "3.20"), b"new").unwrap();
        install(&store, &meta("aarch64", "alpine", "3.20"), b"arm").unwrap();

        let listed = store.list().unwrap();
        let keys: Vec<_> = listed
            .iter()
            .map(|m| format!("{}/{}@{}", m.arch, m.name, m.version))
            .collect();
        assert_eq!(
            keys,
            [
                "aarch64/alpine@3.20",
                "x86_64/alpine@3.19",
                "x86_64/alpine@3.20"
            ]
        );

        assert!(store.exists("x86_64", "alpine", None));
        assert!(store.exists("x86_64", "alpine", Some("3.19")));
        assert!(!store.exists("x86_64", "alpine", Some("3.21")));
        assert!(!store.exists("riscv64", "alpine", None));

        // explicit version
        let r = store
            .resolve_version("x86_64", "alpine", Some("3.19"))
            .unwrap();
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"old");
        assert_eq!(r.meta.version, "3.19");
        assert_eq!(r.dir, r.disk_path.parent().unwrap());

        // None means highest by natural sort
        let r = store.resolve_version("x86_64", "alpine", None).unwrap();
        assert_eq!(r.meta.version, "3.20");
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"new");

        assert!(
            store
                .resolve_version("x86_64", "alpine", Some("9.9"))
                .is_err()
        );
        assert!(store.resolve_version("x86_64", "ghost", None).is_err());
    }

    #[test]
    fn resolve_highest_natural_not_lexical() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "win", "9.9"), b"a").unwrap();
        install(&store, &meta("x86_64", "win", "10.2"), b"b").unwrap();
        let r = store.resolve_version("x86_64", "win", None).unwrap();
        assert_eq!(r.meta.version, "10.2");
    }

    #[test]
    fn resolve_2digit_pin_picks_latest_subbuild() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "windows-11", "26100.1742.0"), b"a").unwrap();
        install(&store, &meta("x86_64", "windows-11", "26100.1742.2"), b"b").unwrap();
        // A 2-component pin resolves to the highest matching sub-build.
        let r = store
            .resolve_version("x86_64", "windows-11", Some("26100.1742"))
            .unwrap();
        assert_eq!(r.meta.version, "26100.1742.2");
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"b");
        // An exact sub-build pin still resolves exactly.
        let r = store
            .resolve_version("x86_64", "windows-11", Some("26100.1742.0"))
            .unwrap();
        assert_eq!(r.meta.version, "26100.1742.0");
        // A prefix with no sub-builds present is not found.
        assert!(
            store
                .resolve_version("x86_64", "windows-11", Some("26100.9999"))
                .is_err()
        );
    }

    /// A staging dir on another filesystem than the store (EXDEV on rename)
    /// is copied into the store and renamed from there, and the original
    /// is removed.
    #[test]
    fn install_from_another_filesystem_copies_into_the_store() {
        let (_tmp, store) = new_store();
        let elsewhere = tempfile::tempdir().unwrap();
        let staging = elsewhere.path().join("staging");
        fs::create_dir_all(staging.join("extra")).unwrap();
        fs::write(staging.join(DISK_FILE), b"disk").unwrap();
        fs::write(staging.join("extra").join("note"), b"n").unwrap();
        let m = meta("x86_64", "t", "1");
        m.write_to(&staging.join(META_FILE)).unwrap();
        let dest = store.version_dir("x86_64", "t", "1");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();

        let root = store.root().to_path_buf();
        let cross_device = |from: &Path, to: &Path| {
            if from.starts_with(&root) {
                fs::rename(from, to)
            } else {
                Err(std::io::Error::from(std::io::ErrorKind::CrossesDevices))
            }
        };
        store.move_in(&staging, &dest, &cross_device).unwrap();

        let r = store.resolve_version("x86_64", "t", Some("1")).unwrap();
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"disk");
        assert_eq!(fs::read(r.dir.join("extra").join("note")).unwrap(), b"n");
        assert!(!staging.exists(), "the original staging dir is removed");
        let leftovers: Vec<_> = fs::read_dir(store.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "no staging dir left in the store");
    }

    #[test]
    fn install_refuses_overwrite_without_flag() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "t", "1"), b"one").unwrap();
        let err = install(&store, &meta("x86_64", "t", "1"), b"two").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        // original untouched
        let r = store.resolve_version("x86_64", "t", Some("1")).unwrap();
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"one");

        // with overwrite it replaces
        let staging = tempfile::tempdir_in(store.root()).unwrap();
        fs::write(staging.path().join(DISK_FILE), b"two").unwrap();
        store
            .install(&staging.keep(), &meta("x86_64", "t", "1"), true)
            .unwrap();
        let r = store.resolve_version("x86_64", "t", Some("1")).unwrap();
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"two");
    }

    #[test]
    fn install_without_disk_leaves_store_empty() {
        let (_tmp, store) = new_store();
        fs::create_dir_all(store.root()).unwrap();
        let staging = tempfile::tempdir_in(store.root()).unwrap();
        let err = store
            .install(staging.path(), &meta("x86_64", "t", "1"), false)
            .unwrap_err();
        assert!(err.to_string().contains(DISK_FILE), "{err}");
        assert!(store.list().unwrap().is_empty());
        assert!(!store.root().join("x86_64").exists(), "no partial dirs");
    }

    // ---- remove ---------------------------------------------------------------

    #[test]
    fn remove_respects_in_use_and_force() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "t", "1"), b"x").unwrap();

        let busy = |_: &[PathBuf]| Some("2 clones in lab \"dev\" are backed by it".to_string());
        let err = store
            .remove("x86_64", "t", "1", None, false, &busy)
            .unwrap_err();
        assert!(err.to_string().contains("2 clones"), "{err}");
        assert!(err.to_string().contains("--force"), "{err}");
        assert!(store.exists("x86_64", "t", Some("1")));

        store.remove("x86_64", "t", "1", None, true, &busy).unwrap();
        assert!(!store.exists("x86_64", "t", None));
        // empty parents pruned
        assert!(!store.root().join("x86_64").exists());
    }

    #[test]
    fn remove_free_template_without_force() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "t", "1"), b"x").unwrap();
        install(&store, &meta("x86_64", "t", "2"), b"y").unwrap();
        store
            .remove("x86_64", "t", "1", None, false, &|_| None)
            .unwrap();
        assert!(!store.exists("x86_64", "t", Some("1")));
        assert!(store.exists("x86_64", "t", Some("2")), "siblings survive");
    }

    #[test]
    fn remove_missing_errors() {
        let (_tmp, store) = new_store();
        let err = store
            .remove("x86_64", "ghost", "1", None, false, &|_| None)
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    // ---- export / import --------------------------------------------------------

    #[test]
    fn export_import_round_trip() {
        let (_tmp, store) = new_store();
        let disk_bytes = b"pretend qcow2 bytes".as_slice();
        let mut m = meta("x86_64", "alpine", "3.20");
        m.sha256 = Some(hex::encode(Sha256::digest(disk_bytes)));
        install(&store, &m, disk_bytes).unwrap();

        let out_dir = tempfile::tempdir().unwrap();
        let archive = out_dir.path().join("alpine.tar.zst");
        store.export("x86_64", "alpine", None, &archive).unwrap();
        assert!(archive.is_file());

        let (_tmp2, other) = new_store();
        let imported = other.import(&archive, false).unwrap();
        assert_eq!(imported, m);
        let r = other.resolve_version("x86_64", "alpine", None).unwrap();
        assert_eq!(r.meta, m);
        assert_eq!(fs::read(&r.disk_path).unwrap(), disk_bytes);

        // re-import refuses without overwrite, succeeds with it
        let err = other.import(&archive, false).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        other.import(&archive, true).unwrap();
    }

    #[test]
    fn import_verifies_disk_digest() {
        let (_tmp, store) = new_store();
        let mut m = meta("x86_64", "t", "1");
        m.sha256 = Some(hex::encode(Sha256::digest(b"genuine")));
        install(&store, &m, b"genuine").unwrap();
        let out = tempfile::tempdir().unwrap();
        let archive = out.path().join("t.tar.zst");
        store.export("x86_64", "t", Some("1"), &archive).unwrap();

        // corrupt the disk inside the archive by tampering post-install
        let (_tmp2, victim) = new_store();
        // build a tampered archive: same meta, different disk bytes
        let staging = tempfile::tempdir().unwrap();
        m.write_to(&staging.path().join(META_FILE)).unwrap();
        fs::write(staging.path().join(DISK_FILE), b"tampered").unwrap();
        let bad = out.path().join("bad.tar.zst");
        let enc = zstd::Encoder::new(File::create(&bad).unwrap(), 0).unwrap();
        let mut tar = tar::Builder::new(enc);
        tar.append_path_with_name(staging.path().join(META_FILE), META_FILE)
            .unwrap();
        tar.append_path_with_name(staging.path().join(DISK_FILE), DISK_FILE)
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let err = victim.import(&bad, false).unwrap_err();
        assert!(err.to_string().contains("digest mismatch"), "{err}");
        // nothing landed in the store and staging was cleaned up
        assert!(victim.list().unwrap().is_empty());
        let leftovers: Vec<_> = fs::read_dir(victim.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(STAGING_PREFIX))
            .collect();
        assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");

        // the genuine archive still imports
        victim.import(&archive, false).unwrap();
    }

    #[test]
    fn import_rejects_garbage() {
        let (_tmp, store) = new_store();
        let out = tempfile::tempdir().unwrap();
        let junk = out.path().join("junk.tar.zst");
        fs::write(&junk, b"not an archive").unwrap();
        assert!(store.import(&junk, false).is_err());
        assert!(store.list().unwrap().is_empty());
    }

    // ---- variants (§6.6) --------------------------------------------------------

    fn have_qemu_img() -> bool {
        let found = std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join("qemu-img").is_file())
        });
        if !found {
            eprintln!("skipping: qemu-img not found on PATH");
        }
        found
    }

    fn qemu_img(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("qemu-img")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "qemu-img {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn qemu_img_info(path: &Path) -> serde_json::Value {
        let out = std::process::Command::new("qemu-img")
            .args(["info", "--output=json"])
            .arg(path)
            .output()
            .unwrap();
        assert!(out.status.success());
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn variant_meta(name: &str, default: bool, base_sha256: &str) -> VariantMeta {
        VariantMeta {
            name: name.into(),
            default,
            created: "2026-07-01T00:00:00Z".parse().unwrap(),
            sha256: "cd".repeat(32),
            base_sha256: base_sha256.into(),
            description: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
            profile: None,
            cpus: None,
            memory: None,
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            cpu_model: None,
            accel: None,
        }
    }

    use super::super::meta::VariantMeta;

    /// A store holding real qcow2 files, laid out as §6.6 says:
    /// `x86_64/win@1` with a base and the overlays `standard` (default) and
    /// `extra` (`memory` overridden), each backed on `../../disk.qcow2`; and
    /// `x86_64/plain@1`, base-only.
    fn variant_store() -> (tempfile::TempDir, TemplateStore) {
        let (tmp, store) = new_store();
        let dir = store.version_dir("x86_64", "win", "1");
        fs::create_dir_all(&dir).unwrap();
        qemu_img(&dir, &["create", "-q", "-f", "qcow2", DISK_FILE, "16M"]);
        let base_sha = sha256_file(&dir.join(DISK_FILE)).unwrap();
        for v in ["standard", "extra"] {
            let vdir = dir.join(VARIANTS_DIR).join(v);
            fs::create_dir_all(&vdir).unwrap();
            qemu_img(
                &vdir,
                &[
                    "create",
                    "-q",
                    "-f",
                    "qcow2",
                    "-b",
                    "../../disk.qcow2",
                    "-F",
                    "qcow2",
                    DISK_FILE,
                ],
            );
        }
        let mut m = meta("x86_64", "win", "1");
        m.sha256 = Some(base_sha.clone());
        m.first_boot_script = Some("base script".into());
        let mut extra = variant_meta("extra", false, &base_sha);
        extra.memory = Some(8 << 30);
        m.variants = vec![variant_meta("standard", true, &base_sha), extra];
        m.write_to(&dir.join(META_FILE)).unwrap();

        let plain = store.version_dir("x86_64", "plain", "1");
        fs::create_dir_all(&plain).unwrap();
        qemu_img(&plain, &["create", "-q", "-f", "qcow2", DISK_FILE, "16M"]);
        meta("x86_64", "plain", "1")
            .write_to(&plain.join(META_FILE))
            .unwrap();
        (tmp, store)
    }

    fn named(n: &str) -> VariantRef {
        VariantRef::Named(n.into())
    }

    /// Row 1: no `#` — the default variant with variants, the base without.
    #[test]
    fn resolve_without_a_variant() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let r = store
            .resolve("x86_64", "win", None, None, Purpose::Lab)
            .unwrap();
        assert_eq!(r.variant.as_deref(), Some("standard"));
        assert_eq!(r.disk_path, r.dir.join("variants/standard/disk.qcow2"));
        assert_eq!(r.reference(), "x86_64/win@1#standard");
        assert!(r.meta.variants.is_empty(), "effective metadata is flat");

        let r = store
            .resolve("x86_64", "plain", None, None, Purpose::Lab)
            .unwrap();
        assert_eq!(r.variant, None);
        assert_eq!(r.disk_path, r.dir.join(DISK_FILE));
    }

    /// Row 2: `#<name>` — that variant (with its effective metadata), an
    /// unknown name listing the variants, and an error on a base-only one.
    #[test]
    fn resolve_a_named_variant() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let r = store
            .resolve(
                "x86_64",
                "win",
                Some("1"),
                Some(&named("extra")),
                Purpose::Lab,
            )
            .unwrap();
        assert_eq!(r.variant.as_deref(), Some("extra"));
        assert_eq!(r.disk_path, r.dir.join("variants/extra/disk.qcow2"));
        assert_eq!(r.meta.memory, Some(8 << 30), "the variant's override");
        assert_eq!(r.meta.cpus, Some(2), "the base's own value");
        assert_eq!(r.meta.first_boot_script.as_deref(), Some("base script"));
        // The overlay is a real qcow2 on the base, by relative path.
        let info = qemu_img_info(&r.disk_path);
        assert_eq!(info["backing-filename"], "../../disk.qcow2");
        assert_eq!(info["virtual-size"], 16 << 20);

        let err = store
            .resolve("x86_64", "win", None, Some(&named("ghost")), Purpose::Lab)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no variant `ghost`"), "{err}");
        assert!(err.contains("standard (default), extra"), "{err}");

        let err = store
            .resolve(
                "x86_64",
                "plain",
                None,
                Some(&named("extra")),
                Purpose::Build,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no variants"), "{err}");
    }

    /// Row 3: `#base` — refused for a lab on a template with variants, the
    /// base in a build context, and the base everywhere without variants.
    #[test]
    fn resolve_the_base() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let base = VariantRef::Base;
        let err = store
            .resolve("x86_64", "win", None, Some(&base), Purpose::Lab)
            .unwrap_err()
            .to_string();
        assert!(err.contains("x86_64/win@1#base is refused"), "{err}");
        assert!(err.contains("unsealed base"), "{err}");
        assert!(err.contains("standard (default), extra"), "{err}");

        let r = store
            .resolve("x86_64", "win", None, Some(&base), Purpose::Build)
            .unwrap();
        assert_eq!(r.variant, None);
        assert_eq!(r.disk_path, r.dir.join(DISK_FILE));
        assert_eq!(r.meta.memory, Some(4 << 30), "the base's own metadata");

        for purpose in [Purpose::Lab, Purpose::Build] {
            let r = store
                .resolve("x86_64", "plain", None, Some(&base), purpose)
                .unwrap();
            assert_eq!(r.disk_path, r.dir.join(DISK_FILE));
        }
    }

    /// A variant counts only with both its block and its overlay; a stale one
    /// is refused by name.
    #[test]
    fn missing_overlays_and_stale_variants() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let dir = store.version_dir("x86_64", "win", "1");
        fs::remove_dir_all(dir.join("variants/extra")).unwrap();
        let v = store.resolve_version("x86_64", "win", None).unwrap();
        let names: Vec<_> = v.meta.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["standard"]);
        assert!(
            store
                .resolve("x86_64", "win", None, Some(&named("extra")), Purpose::Lab)
                .is_err()
        );
        let listed = store.list().unwrap();
        let win = listed.iter().find(|m| m.name == "win").unwrap();
        assert_eq!(
            win.variants.len(),
            1,
            "list exposes installed variants only"
        );

        // Rebuild the base under the variant: it goes stale.
        qemu_img(&dir, &["create", "-q", "-f", "qcow2", DISK_FILE, "32M"]);
        let mut m = TemplateMeta::read_from(&dir.join(META_FILE)).unwrap();
        m.sha256 = Some(sha256_file(&dir.join(DISK_FILE)).unwrap());
        m.write_to(&dir.join(META_FILE)).unwrap();
        let err = store
            .resolve("x86_64", "win", None, None, Purpose::Lab)
            .unwrap_err()
            .to_string();
        assert!(err.contains("variant `standard`"), "{err}");
        assert!(err.contains("rebuild the variant"), "{err}");
    }

    /// Invalid default rules make the store entry unreadable to resolution
    /// and leave it out of the listing.
    #[test]
    fn an_ambiguous_default_is_reported_by_the_reader() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let dir = store.version_dir("x86_64", "win", "1");
        let text = fs::read_to_string(dir.join(META_FILE)).unwrap();
        fs::write(
            dir.join(META_FILE),
            text.replace("    default = true\n", ""),
        )
        .unwrap();
        let err = format!(
            "{:#}",
            store
                .resolve("x86_64", "win", None, None, Purpose::Lab)
                .unwrap_err()
        );
        assert!(
            err.contains("exactly one variant must be the default"),
            "{err}"
        );
        assert!(store.list().unwrap().iter().all(|m| m.name != "win"));
    }

    /// A linked clone of the resolved disk chains clone → variant → base.
    #[test]
    fn a_clone_of_a_variant_chains_to_the_base() {
        if !have_qemu_img() {
            return;
        }
        let (tmp, store) = variant_store();
        let r = store
            .resolve("x86_64", "win", None, Some(&named("extra")), Purpose::Lab)
            .unwrap();
        let backing = r.disk_path.canonicalize().unwrap();
        qemu_img(
            tmp.path(),
            &[
                "create",
                "-q",
                "-f",
                "qcow2",
                "-b",
                backing.to_str().unwrap(),
                "-F",
                "qcow2",
                "clone.qcow2",
            ],
        );
        let out = std::process::Command::new("qemu-img")
            .args(["info", "--backing-chain", "--output=json"])
            .arg(tmp.path().join("clone.qcow2"))
            .output()
            .unwrap();
        let chain: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(chain.len(), 3, "clone, variant, base");
        assert_eq!(chain[0]["backing-filename"], backing.to_str().unwrap());
        let base = PathBuf::from(chain[2]["filename"].as_str().unwrap());
        assert_eq!(
            base.canonicalize().unwrap(),
            r.dir.join(DISK_FILE).canonicalize().unwrap()
        );
    }

    /// Removing a variant drops its overlay and its block alone, and is
    /// refused while a clone is backed by that overlay; removing the version
    /// hands every disk — base and overlays — to the in-use check.
    #[test]
    fn remove_one_variant() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let dir = store.version_dir("x86_64", "win", "1");
        let extra = dir.join("variants/extra/disk.qcow2");
        let standard = dir.join("variants/standard/disk.qcow2");

        // In use: a clone is backed by `extra`.
        let backed_on_extra = |doomed: &[PathBuf]| {
            doomed
                .iter()
                .any(|d| d == &extra)
                .then(|| "the clone of machine lab/vm is backed by it".to_string())
        };
        let err = store
            .remove("x86_64", "win", "1", Some("extra"), false, &backed_on_extra)
            .unwrap_err()
            .to_string();
        assert!(err.contains("x86_64/win@1#extra"), "{err}");
        assert!(err.contains("lab/vm"), "{err}");
        assert!(extra.is_file());
        // The same clone does not hold `standard`'s check, nor the base's
        // when only a variant goes.
        let seen = std::cell::RefCell::new(Vec::new());
        let record = |doomed: &[PathBuf]| {
            seen.borrow_mut().extend_from_slice(doomed);
            None
        };
        store
            .remove("x86_64", "win", "1", Some("extra"), false, &record)
            .unwrap();
        assert_eq!(*seen.borrow(), std::slice::from_ref(&extra));
        assert!(!extra.exists());
        assert!(!dir.join("variants/extra").exists());
        assert!(standard.is_file(), "the other variant stays");
        assert!(dir.join(DISK_FILE).is_file(), "the base stays");
        let v = store.resolve_version("x86_64", "win", None).unwrap();
        assert!(v.meta.variant("extra").is_none(), "its block is gone");
        assert!(v.meta.variant("standard").is_some());
        // Re-read from disk, not just filtered.
        let on_disk = TemplateMeta::read_from(&dir.join(META_FILE)).unwrap();
        assert_eq!(on_disk.variants.len(), 1);

        // The last variant cannot go on its own.
        let err = store
            .remove("x86_64", "win", "1", Some("standard"), false, &|_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only variant"), "{err}");
        // Nor can `#base`, nor a name it does not have.
        assert!(
            store
                .remove("x86_64", "win", "1", Some("base"), false, &|_| None)
                .is_err()
        );
        let err = store
            .remove("x86_64", "win", "1", Some("ghost"), false, &|_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no variant `ghost`"), "{err}");
    }

    #[test]
    fn remove_the_default_variant_is_refused() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let err = store
            .remove("x86_64", "win", "1", Some("standard"), false, &|_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("default variant"), "{err}");
        assert!(err.contains("extra"), "{err}");
    }

    #[test]
    fn remove_a_version_removes_its_variants() {
        if !have_qemu_img() {
            return;
        }
        let (_tmp, store) = variant_store();
        let dir = store.version_dir("x86_64", "win", "1");
        let seen = std::cell::RefCell::new(Vec::new());
        let busy = |doomed: &[PathBuf]| {
            seen.borrow_mut().extend_from_slice(doomed);
            doomed
                .iter()
                .any(|d| d.ends_with("variants/standard/disk.qcow2"))
                .then(|| "a clone is backed by it".to_string())
        };
        let err = store
            .remove("x86_64", "win", "1", None, false, &busy)
            .unwrap_err()
            .to_string();
        assert!(err.contains("a clone is backed by it"), "{err}");
        assert_eq!(
            *seen.borrow(),
            [
                dir.join(DISK_FILE),
                dir.join("variants/standard/disk.qcow2"),
                dir.join("variants/extra/disk.qcow2"),
            ]
        );
        store
            .remove("x86_64", "win", "1", None, false, &|_| None)
            .unwrap();
        assert!(!dir.exists(), "base and variants go together");
    }

    // ---- misc -------------------------------------------------------------------

    #[test]
    fn sha256_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn list_skips_lock_and_staging_entries() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "t", "1"), b"x").unwrap();
        // lock file already exists from install; add a stray staging dir
        fs::create_dir_all(store.root().join(".staging-deadbeef")).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "t");
    }

    #[test]
    fn list_skips_corrupt_metadata() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "good", "1"), b"x").unwrap();
        let bad = store.root().join("x86_64/bad/1");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join(META_FILE), "garbage {{{").unwrap();
        fs::write(bad.join(DISK_FILE), b"y").unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "good");
    }

    #[test]
    fn concurrent_mutations_serialise() {
        // Two threads install different versions through the same lock;
        // both must land intact.
        let (_tmp, store) = new_store();
        fs::create_dir_all(store.root()).unwrap();
        let s1 = store.clone();
        let s2 = store.clone();
        let m1 = meta("x86_64", "t", "1");
        let m2 = meta("x86_64", "t", "2");
        let t1 = std::thread::spawn(move || install(&s1, &m1, b"one").unwrap());
        let t2 = std::thread::spawn(move || install(&s2, &m2, b"two").unwrap());
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }

    /// Write `entries` (name, bytes) into a `tar.zst` the way an export does.
    fn repack(out: &Path, from: &Path, entries: &[&str]) {
        let file = File::create(out).unwrap();
        let mut tar = tar::Builder::new(zstd::Encoder::new(file, 0).unwrap());
        for e in entries {
            tar.append_path_with_name(from.join(e), e).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    fn unpack(archive: &Path, to: &Path) {
        let decoder = zstd::Decoder::new(File::open(archive).unwrap()).unwrap();
        tar::Archive::new(decoder).unpack(to).unwrap();
    }

    /// Give `variant_store`'s overlays real digests, so an import can check
    /// them.
    fn record_variant_digests(store: &TemplateStore) {
        let dir = store.version_dir("x86_64", "win", "1");
        let mut m = TemplateMeta::read_from(&dir.join(META_FILE)).unwrap();
        for v in &mut m.variants {
            v.sha256 = sha256_file(&dir.join(variant_disk_rel(&v.name))).unwrap();
        }
        m.write_to(&dir.join(META_FILE)).unwrap();
    }

    /// An export carries `variants/` and the full `template.wcl`; an import
    /// restores both, every overlay still backed on `../../disk.qcow2`.
    #[test]
    fn export_import_round_trip_with_variants() {
        if !have_qemu_img() {
            return;
        }
        let (tmp, store) = variant_store();
        record_variant_digests(&store);
        let archive = tmp.path().join("win.tar.zst");
        store.export("x86_64", "win", Some("1"), &archive).unwrap();

        let listing = tmp.path().join("listing");
        unpack(&archive, &listing);
        for f in [
            "template.wcl",
            "disk.qcow2",
            "variants/standard/disk.qcow2",
            "variants/extra/disk.qcow2",
        ] {
            assert!(listing.join(f).is_file(), "{f} missing from the export");
        }

        let (_tmp2, other) = new_store();
        let imported = other.import(&archive, false).unwrap();
        let original = store.resolve_version("x86_64", "win", Some("1")).unwrap();
        assert_eq!(imported, original.meta);
        let back = other.resolve_version("x86_64", "win", Some("1")).unwrap();
        assert_eq!(back.meta, original.meta);
        for v in ["standard", "extra"] {
            let disk = back.variant_disk(v);
            assert_eq!(
                fs::read(&disk).unwrap(),
                fs::read(original.variant_disk(v)).unwrap()
            );
            assert_eq!(
                qcow2_backing_file(&disk).unwrap().as_deref(),
                Some(VARIANT_BACKING)
            );
            let info = qemu_img_info(&disk);
            assert_eq!(info["backing-filename"], VARIANT_BACKING);
        }
        let r = other
            .resolve(
                "x86_64",
                "win",
                Some("1"),
                Some(&named("extra")),
                Purpose::Lab,
            )
            .unwrap();
        assert_eq!(r.meta.memory, Some(8 << 30));
    }

    /// An import refuses a variant whose overlay is not the one its block
    /// names, one built on another base, and one whose overlay is missing —
    /// and installs nothing.
    #[test]
    fn import_refuses_a_tampered_variant() {
        if !have_qemu_img() {
            return;
        }
        let (tmp, store) = variant_store();
        record_variant_digests(&store);
        let archive = tmp.path().join("win.tar.zst");
        store.export("x86_64", "win", Some("1"), &archive).unwrap();
        let entries = [
            "template.wcl",
            "disk.qcow2",
            "variants/standard/disk.qcow2",
            "variants/extra/disk.qcow2",
        ];

        let refused = |name: &str, tamper: &dyn Fn(&Path), want: &str| {
            let dir = tmp.path().join(name);
            unpack(&archive, &dir);
            tamper(&dir);
            let bad = tmp.path().join(format!("{name}.tar.zst"));
            let present: Vec<&str> = entries
                .iter()
                .copied()
                .filter(|e| dir.join(e).is_file())
                .collect();
            repack(&bad, &dir, &present);
            let (_t, other) = new_store();
            let err = format!("{:#}", other.import(&bad, false).unwrap_err());
            assert!(err.contains(want), "{name}: {err}");
            assert!(other.list().unwrap().is_empty(), "{name}: installed anyway");
        };

        refused(
            "overlay",
            &|dir| {
                let disk = dir.join("variants/extra/disk.qcow2");
                let mut bytes = fs::read(&disk).unwrap();
                bytes.push(0);
                fs::write(&disk, bytes).unwrap();
            },
            "variant `extra`",
        );
        refused(
            "base",
            &|dir| {
                let mut m = TemplateMeta::read_from(&dir.join(META_FILE)).unwrap();
                m.variants[0].base_sha256 = "00".repeat(32);
                m.write_to(&dir.join(META_FILE)).unwrap();
            },
            "built on base",
        );
        refused(
            "missing",
            &|dir| fs::remove_file(dir.join("variants/standard/disk.qcow2")).unwrap(),
            "has no variants/standard/disk.qcow2",
        );
    }
}
