//! The template store (PRD §4, §6.1, §6.2, §7.1).
//!
//! Layout: `<root>/<arch>/<name>/<version>/{disk.qcow2, template.wcl}`.
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

use super::meta::{META_FILE, TemplateMeta};

/// Disk image file name inside a version directory.
pub const DISK_FILE: &str = "disk.qcow2";
const LOCK_FILE: &str = ".lock";
const STAGING_PREFIX: &str = ".staging-";

/// A resolved store entry: its directory, disk image and metadata.
#[derive(Debug, Clone)]
pub struct ResolvedTemplate {
    pub dir: PathBuf,
    pub disk_path: PathBuf,
    pub meta: TemplateMeta,
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
    /// (natural order). Entries with unreadable metadata are skipped
    /// with a warning rather than failing the whole listing.
    pub fn list(&self) -> Result<Vec<TemplateMeta>> {
        let mut out = Vec::new();
        for arch in subdirs(&self.root)? {
            for name in subdirs(&arch)? {
                for version in subdirs(&name)? {
                    let meta_path = version.join(META_FILE);
                    if !meta_path.is_file() {
                        continue;
                    }
                    match TemplateMeta::read_from(&meta_path) {
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

    /// Resolve `<arch>/<name>[@version]` to a store entry. `version`
    /// of `None` selects the highest version by natural-sort order
    /// (PRD §6.2). A `version` that is a build-counter prefix (e.g. the
    /// 2-component `26100.1742`) resolves to the highest `<version>.<N>`
    /// sub-build present, falling back to an exact match (see
    /// [`resolve_version_pin`]).
    pub fn resolve(
        &self,
        arch: &str,
        name: &str,
        version: Option<&str>,
    ) -> Result<ResolvedTemplate> {
        let version = match version {
            Some(v) => resolve_version_pin(&self.versions_of(arch, name).unwrap_or_default(), v)
                .ok_or_else(|| anyhow!("template {arch}/{name}@{v} not found in the store"))?,
            None => self
                .versions_of(arch, name)?
                .into_iter()
                .max_by(|a, b| compare_versions(a, b))
                .ok_or_else(|| anyhow!("template {arch}/{name} not found in the store"))?,
        };
        let dir = self.version_dir(arch, name, &version);
        let meta_path = dir.join(META_FILE);
        if !meta_path.is_file() {
            bail!("template {arch}/{name}@{version} not found in the store");
        }
        let meta = TemplateMeta::read_from(&meta_path)?;
        let disk_path = dir.join(DISK_FILE);
        ensure!(
            disk_path.is_file(),
            "template {arch}/{name}@{version} is corrupt: missing {DISK_FILE}"
        );
        Ok(ResolvedTemplate {
            disk_path,
            dir,
            meta,
        })
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
    /// anywhere leaves the store untouched (PRD §6.1). The staging dir
    /// must live on the same filesystem as the store root. Refuses to
    /// replace an existing version unless `overwrite` is set.
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
        fs::rename(staging, &dest).with_context(|| {
            format!(
                "cannot move staged template into {} (staging must be on the same \
                 filesystem as the store)",
                dest.display()
            )
        })?;
        Ok(())
    }

    /// Re-tag an existing template under refreshed metadata (a new version
    /// and/or registry), reusing the on-disk image — no rebuild. Moves the
    /// existing version directory aside, then re-installs it as `new_meta`.
    /// Refuses to clobber a *different* existing version.
    pub fn relabel(
        &self,
        arch: &str,
        name: &str,
        old_version: &str,
        new_meta: &TemplateMeta,
    ) -> Result<()> {
        let _lock = self.lock()?;
        let old_dir = self.version_dir(arch, name, old_version);
        ensure!(
            old_dir.join(META_FILE).is_file(),
            "template {arch}/{name}@{old_version} not found in the store"
        );
        let dest = self.version_dir(&new_meta.arch, &new_meta.name, &new_meta.version);
        if dest.exists() && dest != old_dir {
            bail!(
                "cannot relabel onto existing {}/{}@{} — remove it first",
                new_meta.arch,
                new_meta.name,
                new_meta.version
            );
        }
        let staging = self.root.join(format!(
            "{STAGING_PREFIX}relabel-{arch}-{name}-{old_version}"
        ));
        let _ = fs::remove_dir_all(&staging);
        fs::rename(&old_dir, &staging)
            .with_context(|| format!("cannot stage {}", old_dir.display()))?;
        self.install_locked(&staging, new_meta, true)
    }

    /// Remove `<arch>/<name>@<version>`. `in_use` reports why the
    /// template cannot go (e.g. clones backed by it, PRD §7.1); a
    /// `Some(reason)` refuses the removal unless `force` is set.
    pub fn remove(
        &self,
        arch: &str,
        name: &str,
        version: &str,
        force: bool,
        in_use: &dyn Fn(&TemplateMeta) -> Option<String>,
    ) -> Result<()> {
        let _lock = self.lock()?;
        let dir = self.version_dir(arch, name, version);
        if !dir.is_dir() {
            bail!("template {arch}/{name}@{version} not found in the store");
        }
        match TemplateMeta::read_from(&dir.join(META_FILE)) {
            Ok(meta) => {
                if let Some(reason) = in_use(&meta)
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

    /// Export a template as a single portable `tar.zst` archive holding
    /// `template.wcl` + `disk.qcow2` (PRD §6.2). Read-only, so no lock.
    pub fn export(&self, arch: &str, name: &str, version: Option<&str>, out: &Path) -> Result<()> {
        let resolved = self.resolve(arch, name, version)?;
        let file = File::create(out).with_context(|| format!("cannot create {}", out.display()))?;
        let encoder = zstd::Encoder::new(file, 0).context("cannot start zstd stream")?;
        let mut tar = tar::Builder::new(encoder);
        tar.append_path_with_name(resolved.dir.join(META_FILE), META_FILE)
            .context("cannot archive template.wcl")?;
        tar.append_path_with_name(&resolved.disk_path, DISK_FILE)
            .context("cannot archive disk.qcow2")?;
        let encoder = tar.into_inner().context("cannot finish archive")?;
        encoder.finish().context("cannot finish zstd stream")?;
        Ok(())
    }

    /// Import an archive produced by [`Self::export`]. Unpacks to a
    /// staging dir inside the store root (same filesystem, so the final
    /// move is an atomic rename), verifies the disk digest against the
    /// metadata when recorded, then installs.
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
        }
        self.install_locked(staging.path(), &meta, overwrite)?;
        Ok(meta)
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

/// Streaming hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("cannot hash {}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
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
            created: "2026-06-12T00:00:00Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
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
        let r = store.resolve("x86_64", "alpine", Some("3.19")).unwrap();
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"old");
        assert_eq!(r.meta.version, "3.19");
        assert_eq!(r.dir, r.disk_path.parent().unwrap());

        // None means highest by natural sort
        let r = store.resolve("x86_64", "alpine", None).unwrap();
        assert_eq!(r.meta.version, "3.20");
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"new");

        assert!(store.resolve("x86_64", "alpine", Some("9.9")).is_err());
        assert!(store.resolve("x86_64", "ghost", None).is_err());
    }

    #[test]
    fn resolve_highest_natural_not_lexical() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "win", "9.9"), b"a").unwrap();
        install(&store, &meta("x86_64", "win", "10.2"), b"b").unwrap();
        let r = store.resolve("x86_64", "win", None).unwrap();
        assert_eq!(r.meta.version, "10.2");
    }

    #[test]
    fn resolve_2digit_pin_picks_latest_subbuild() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "windows-11", "26100.1742.0"), b"a").unwrap();
        install(&store, &meta("x86_64", "windows-11", "26100.1742.2"), b"b").unwrap();
        // A 2-component pin resolves to the highest matching sub-build.
        let r = store
            .resolve("x86_64", "windows-11", Some("26100.1742"))
            .unwrap();
        assert_eq!(r.meta.version, "26100.1742.2");
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"b");
        // An exact sub-build pin still resolves exactly.
        let r = store
            .resolve("x86_64", "windows-11", Some("26100.1742.0"))
            .unwrap();
        assert_eq!(r.meta.version, "26100.1742.0");
        // A prefix with no sub-builds present is not found.
        assert!(
            store
                .resolve("x86_64", "windows-11", Some("26100.9999"))
                .is_err()
        );
    }

    #[test]
    fn install_refuses_overwrite_without_flag() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "t", "1"), b"one").unwrap();
        let err = install(&store, &meta("x86_64", "t", "1"), b"two").unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        // original untouched
        let r = store.resolve("x86_64", "t", Some("1")).unwrap();
        assert_eq!(fs::read(&r.disk_path).unwrap(), b"one");

        // with overwrite it replaces
        let staging = tempfile::tempdir_in(store.root()).unwrap();
        fs::write(staging.path().join(DISK_FILE), b"two").unwrap();
        store
            .install(&staging.keep(), &meta("x86_64", "t", "1"), true)
            .unwrap();
        let r = store.resolve("x86_64", "t", Some("1")).unwrap();
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

        let busy = |_: &TemplateMeta| Some("2 clones in lab \"dev\" are backed by it".to_string());
        let err = store.remove("x86_64", "t", "1", false, &busy).unwrap_err();
        assert!(err.to_string().contains("2 clones"), "{err}");
        assert!(err.to_string().contains("--force"), "{err}");
        assert!(store.exists("x86_64", "t", Some("1")));

        store.remove("x86_64", "t", "1", true, &busy).unwrap();
        assert!(!store.exists("x86_64", "t", None));
        // empty parents pruned
        assert!(!store.root().join("x86_64").exists());
    }

    #[test]
    fn remove_free_template_without_force() {
        let (_tmp, store) = new_store();
        install(&store, &meta("x86_64", "t", "1"), b"x").unwrap();
        install(&store, &meta("x86_64", "t", "2"), b"y").unwrap();
        store.remove("x86_64", "t", "1", false, &|_| None).unwrap();
        assert!(!store.exists("x86_64", "t", Some("1")));
        assert!(store.exists("x86_64", "t", Some("2")), "siblings survive");
    }

    #[test]
    fn remove_missing_errors() {
        let (_tmp, store) = new_store();
        let err = store
            .remove("x86_64", "ghost", "1", false, &|_| None)
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
        let r = other.resolve("x86_64", "alpine", None).unwrap();
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
}
