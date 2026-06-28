//! Template metadata — the `template.wcl` file stored beside `disk.qcow2`
//! in the store (PRD §6.1, §6.2). Written as deterministic WCL text and
//! read back through `wcl_lang` against the embedded `vmlab-meta.wcl`
//! schema.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use wcl_lang::{Block, Document, Environment, Registry, Value, disk_loader};

/// Embedded schema, registered in the loader as `vmlab-meta.wcl`.
const META_SCHEMA: &str = include_str!("meta_schema.wcl");
const SCHEMA_IMPORT: &str = "import <vmlab-meta.wcl>";

/// Metadata file name beside the disk image.
pub const META_FILE: &str = "template.wcl";

/// Recorded hardware and provenance of a sealed template (PRD §6.1).
/// The hardware fields form the template layer of the VM inheritance
/// chain (VM block > template > profile, PRD §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateMeta {
    pub name: String,
    pub arch: String,
    pub version: String,
    pub profile: Option<String>,
    pub cpus: Option<u32>,
    /// RAM in bytes.
    pub memory: Option<u64>,
    /// Primary disk virtual size in bytes.
    pub disk: Option<u64>,
    pub firmware: Option<String>,
    pub tpm: Option<bool>,
    pub secure_boot: Option<bool>,
    pub display: Option<String>,
    pub created: DateTime<Utc>,
    /// Where the template came from — source ISO URL, registry ref, …
    pub origin: Option<String>,
    /// Full OCI repository this template publishes to (host/owner/[group/]name).
    pub registry: Option<String>,
    /// Hex SHA-256 digest of `disk.qcow2`.
    pub sha256: Option<String>,
    /// Embedded wscript script (full source text) run the first time a VM is
    /// instantiated from this template, before it is reported ready (PRD §6.1).
    pub first_boot_script: Option<String>,
}

impl TemplateMeta {
    /// Render as deterministic WCL text (fixed field order, omitted
    /// optionals). Output starts with the schema import.
    pub fn to_wcl(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{SCHEMA_IMPORT}");
        let _ = writeln!(out);
        let _ = writeln!(out, "template_meta {} {{", quote(&self.name));
        let _ = writeln!(out, "  arch = {}", quote(&self.arch));
        let _ = writeln!(out, "  version = {}", quote(&self.version));
        if let Some(p) = &self.profile {
            let _ = writeln!(out, "  profile = {}", quote(p));
        }
        if let Some(c) = self.cpus {
            let _ = writeln!(out, "  cpus = {c}");
        }
        if let Some(m) = self.memory {
            let _ = writeln!(out, "  memory = {}", format_size(m));
        }
        if let Some(d) = self.disk {
            let _ = writeln!(out, "  disk = {}", format_size(d));
        }
        if let Some(f) = &self.firmware {
            let _ = writeln!(out, "  firmware = {}", quote(f));
        }
        if let Some(t) = self.tpm {
            let _ = writeln!(out, "  tpm = {t}");
        }
        if let Some(s) = self.secure_boot {
            let _ = writeln!(out, "  secure_boot = {s}");
        }
        if let Some(d) = &self.display {
            let _ = writeln!(out, "  display = {}", quote(d));
        }
        let _ = writeln!(out, "  created = {}", quote(&self.created.to_rfc3339()));
        if let Some(o) = &self.origin {
            let _ = writeln!(out, "  origin = {}", quote(o));
        }
        if let Some(r) = &self.registry {
            let _ = writeln!(out, "  registry = {}", quote(r));
        }
        if let Some(s) = &self.sha256 {
            let _ = writeln!(out, "  sha256 = {}", quote(s));
        }
        if let Some(s) = &self.first_boot_script {
            let _ = writeln!(out, "  first_boot_script = {}", quote(s));
        }
        let _ = writeln!(out, "}}");
        out
    }

    /// Parse a `template.wcl` source. `name` labels error messages
    /// (usually the file path).
    pub fn from_wcl(source: &str, name: &str) -> Result<Self> {
        if !source.contains(SCHEMA_IMPORT) {
            bail!("{name}: missing `{SCHEMA_IMPORT}` — not a vmlab template metadata file");
        }
        let mut registry = Registry::new();
        registry.register("vmlab-meta.wcl", META_SCHEMA);
        let doc = Document::open_at_with_loader(
            source,
            name,
            None,
            &Environment::new(),
            registry.loader(disk_loader()),
        )
        .map_err(|e| anyhow!("{name}: parse error: {e}"))?;
        let schema_errors = doc.schema_errors();
        if let Some(e) = schema_errors.first() {
            bail!("{name}: schema error: {e}");
        }
        let block = doc
            .blocks()
            .find(|b| b.kind() == "template_meta")
            .ok_or_else(|| anyhow!("{name}: no `template_meta` block found"))?;
        extract(&block).with_context(|| format!("{name}: invalid template metadata"))
    }

    /// Write to a file (overwriting).
    pub fn write_to(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_wcl())
            .with_context(|| format!("cannot write {}", path.display()))
    }

    /// Read from a file.
    pub fn read_from(path: &Path) -> Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::from_wcl(&source, &path.display().to_string())
    }
}

fn extract(block: &Block) -> Result<TemplateMeta> {
    let name = match block.labels() {
        Ok(labels) => match labels.first() {
            Some(Value::Utf8(s)) | Some(Value::Ascii(s)) | Some(Value::Identifier(s)) => s.clone(),
            _ => bail!("`template_meta` requires a name label"),
        },
        Err(e) => bail!("cannot evaluate `template_meta` label: {e}"),
    };
    let created_raw = require_str(block, "created")?;
    let created = DateTime::parse_from_rfc3339(&created_raw)
        .map_err(|e| anyhow!("malformed `created` timestamp `{created_raw}`: {e}"))?
        .with_timezone(&Utc);
    Ok(TemplateMeta {
        name,
        arch: require_str(block, "arch")?,
        version: require_str(block, "version")?,
        profile: get_str(block, "profile")?,
        cpus: get_cpus(block)?,
        memory: get_size(block, "memory")?,
        disk: get_size(block, "disk")?,
        firmware: get_str(block, "firmware")?,
        tpm: get_bool(block, "tpm")?,
        secure_boot: get_bool(block, "secure_boot")?,
        display: get_str(block, "display")?,
        created,
        origin: get_str(block, "origin")?,
        registry: get_str(block, "registry")?,
        sha256: get_str(block, "sha256")?,
        first_boot_script: get_str(block, "first_boot_script")?,
    })
}

// ---- field readers ---------------------------------------------------------

fn field_value(block: &Block, name: &str) -> Result<Option<Value>> {
    let Some(field) = block.field(name) else {
        return Ok(None);
    };
    match field.value() {
        Ok(Value::None) => Ok(None),
        Ok(v) => Ok(Some(v.clone())),
        Err(e) => bail!("cannot evaluate `{name}`: {e}"),
    }
}

fn get_str(block: &Block, name: &str) -> Result<Option<String>> {
    match field_value(block, name)? {
        None => Ok(None),
        Some(Value::Utf8(s)) | Some(Value::Ascii(s)) | Some(Value::Identifier(s)) => Ok(Some(s)),
        Some(other) => bail!("`{name}` must be a string, got {other:?}"),
    }
}

fn require_str(block: &Block, name: &str) -> Result<String> {
    get_str(block, name)?.ok_or_else(|| anyhow!("missing required field `{name}`"))
}

fn get_bool(block: &Block, name: &str) -> Result<Option<bool>> {
    match field_value(block, name)? {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(b)),
        Some(other) => bail!("`{name}` must be a bool, got {other:?}"),
    }
}

fn get_cpus(block: &Block) -> Result<Option<u32>> {
    let n = match field_value(block, "cpus")? {
        None => return Ok(None),
        Some(Value::I64(n)) => n,
        Some(Value::I32(n)) => i64::from(n),
        Some(Value::U32(n)) => i64::from(n),
        Some(Value::U64(n)) => i64::try_from(n).map_err(|_| anyhow!("`cpus` out of range"))?,
        Some(other) => bail!("`cpus` must be an integer, got {other:?}"),
    };
    u32::try_from(n)
        .map(Some)
        .map_err(|_| anyhow!("`cpus` out of range: {n}"))
}

fn get_size(block: &Block, name: &str) -> Result<Option<u64>> {
    let n = match field_value(block, name)? {
        None => return Ok(None),
        Some(Value::I64(n)) => n,
        Some(other) => bail!("`{name}` must be a byte size, got {other:?}"),
    };
    u64::try_from(n)
        .map(Some)
        .map_err(|_| anyhow!("`{name}` must not be negative: {n}"))
}

// ---- formatting ------------------------------------------------------------

/// Format a byte count as the shortest exact `std.ByteSize` literal — an
/// IEC suffix (`KiB`/`MiB`/`GiB`/`TiB`, powers of 1024) when the count is a
/// whole multiple, else a bare byte integer. Both forms re-parse to the same
/// `u64` via the metadata read path.
pub(crate) fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ];
    for (unit, suffix) in UNITS {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{}{suffix}", bytes / unit);
        }
    }
    bytes.to_string()
}

/// Quote a string as a WCL `"..."` literal (plain strings do not
/// interpolate, so only backslash escapes are needed).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_meta() -> TemplateMeta {
        TemplateMeta {
            name: "win11".into(),
            arch: "x86_64".into(),
            version: "26100.1".into(),
            profile: Some("windows11".into()),
            cpus: Some(4),
            memory: Some(8 << 30),
            disk: Some(64 << 30),
            firmware: Some("uefi".into()),
            tpm: Some(true),
            secure_boot: Some(true),
            display: Some("vnc".into()),
            created: "2026-06-12T10:20:30.123456Z".parse().unwrap(),
            origin: Some("https://example.com/win11.iso".into()),
            registry: Some("ghcr.io/vmlabdev/vmlab-templates/win11".into()),
            sha256: Some("ab".repeat(32)),
            first_boot_script: Some(
                "use vmlab\nfn main(lab) {\n    let vm = lab.this_vm()\n}\n".into(),
            ),
        }
    }

    fn minimal_meta() -> TemplateMeta {
        TemplateMeta {
            name: "alpine".into(),
            arch: "aarch64".into(),
            version: "3.20".into(),
            profile: None,
            cpus: None,
            memory: None,
            disk: None,
            firmware: None,
            tpm: None,
            secure_boot: None,
            display: None,
            created: "2026-01-02T03:04:05Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
        }
    }

    #[test]
    fn round_trip_full() {
        let meta = full_meta();
        let text = meta.to_wcl();
        assert!(text.starts_with(SCHEMA_IMPORT));
        let back = TemplateMeta::from_wcl(&text, "<test>").unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn round_trip_minimal() {
        let meta = minimal_meta();
        let back = TemplateMeta::from_wcl(&meta.to_wcl(), "<test>").unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn round_trip_odd_sizes_and_strings() {
        let mut meta = minimal_meta();
        meta.memory = Some((1 << 30) + 1); // not unit-aligned: bare bytes
        meta.disk = Some(1536 << 20); // 1.5G → "1536MiB"
        meta.origin = Some("say \"hi\" \\ back\ttab".into());
        let text = meta.to_wcl();
        assert!(text.contains("memory = 1073741825"));
        assert!(text.contains("disk = 1536MiB"));
        let back = TemplateMeta::from_wcl(&text, "<test>").unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn round_trip_first_boot_script() {
        // A real first-boot script: multi-line, embedded quotes, and Windows
        // backslash paths (C:\Windows\Temp) — the exact shapes most likely to
        // break WCL string escaping.
        let mut meta = minimal_meta();
        meta.first_boot_script = Some(
            "use vmlab\nfn main(lab) {\n    let vm = lab.this_vm()\n    \
             vm.exec(\"cmd\", [\"/c\", \"del C:\\\\Windows\\\\Temp\\\\vmlab-firstboot.done\"])\n}\n"
                .into(),
        );
        let text = meta.to_wcl();
        let back = TemplateMeta::from_wcl(&text, "<test>").unwrap();
        assert_eq!(meta, back);
        assert_eq!(meta.first_boot_script, back.first_boot_script);
    }

    #[test]
    fn round_trip_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(META_FILE);
        let meta = full_meta();
        meta.write_to(&path).unwrap();
        assert_eq!(TemplateMeta::read_from(&path).unwrap(), meta);
    }

    #[test]
    fn deterministic_output() {
        assert_eq!(full_meta().to_wcl(), full_meta().to_wcl());
    }

    #[test]
    fn rejects_missing_import() {
        let err = TemplateMeta::from_wcl("template_meta \"x\" {}", "<test>").unwrap_err();
        assert!(err.to_string().contains("vmlab-meta.wcl"), "{err}");
    }

    #[test]
    fn rejects_missing_required_field() {
        let src = format!(
            "{SCHEMA_IMPORT}\ntemplate_meta \"x\" {{ arch = \"x86_64\" version = \"1\" }}\n"
        );
        let err = TemplateMeta::from_wcl(&src, "<test>").unwrap_err();
        assert!(format!("{err:#}").contains("created"), "{err:#}");
    }

    #[test]
    fn rejects_unknown_field() {
        let src = format!(
            "{SCHEMA_IMPORT}\ntemplate_meta \"x\" {{ arch = \"a\" version = \"1\" \
             created = \"2026-01-02T03:04:05Z\" bogus = 1 }}\n"
        );
        assert!(TemplateMeta::from_wcl(&src, "<test>").is_err());
    }

    #[test]
    fn rejects_bad_timestamp() {
        let src = format!(
            "{SCHEMA_IMPORT}\ntemplate_meta \"x\" {{ arch = \"a\" version = \"1\" \
             created = \"yesterday\" }}\n"
        );
        let err = TemplateMeta::from_wcl(&src, "<test>").unwrap_err();
        assert!(format!("{err:#}").contains("created"), "{err:#}");
    }

    #[test]
    fn format_size_cases() {
        assert_eq!(format_size(8 << 30), "8GiB");
        assert_eq!(format_size(512 << 20), "512MiB");
        assert_eq!(format_size(2 << 40), "2TiB");
        assert_eq!(format_size(4 << 10), "4KiB");
        assert_eq!(format_size(1536 << 20), "1536MiB");
        assert_eq!(format_size(1023), "1023");
        assert_eq!(format_size(0), "0");
        // every case round-trips through the metadata read path
        for n in [
            8u64 << 30,
            512 << 20,
            2 << 40,
            1536 << 20,
            1023,
            (1 << 30) + 1,
        ] {
            let mut meta = minimal_meta();
            meta.memory = Some(n);
            let back = TemplateMeta::from_wcl(&meta.to_wcl(), "<test>").unwrap();
            assert_eq!(back.memory, Some(n));
        }
    }
}
