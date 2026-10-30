//! Template metadata — the `template.wcl` file stored beside `disk.qcow2`
//! in the store (PRD §6.1, §6.2). Written as deterministic WCL text and
//! read back through `wcl_lang` against the embedded `vmlab-meta.wcl`
//! schema.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use wcl_lang::{Block, Document, Environment, Registry, disk_loader};

use crate::config::IssueList;
use crate::config::block::{Reader, Unspan, finish};
use crate::config::model::{BASE_VARIANT, SecureBootKeys, VariantRef};

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
    /// The key store secure boot starts from (§5.2). `Vars` means the
    /// version directory holds a pre-enrolled VARS file ([`super::store::VARS_FILE`])
    /// that clones start from.
    pub secure_boot_keys: Option<SecureBootKeys>,
    pub display: Option<String>,
    /// QEMU `-cpu` model the template pins for its clones (§5.2).
    pub cpu_model: Option<String>,
    /// Hyper-V enlightenments the template pins for its clones (§5.2).
    pub hyperv: Option<bool>,
    /// QEMU accelerator the template pins for its clones (§5.2).
    pub accel: Option<crate::config::model::AccelMode>,
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
    /// Version stamp of the vmlab-agent baked into the image by the template
    /// build (`None` = template predates agent support: no interactive
    /// terminal, no exec/copy).
    pub agent_version: Option<String>,
    /// Host wscript surface this template's embedded scripts were built
    /// against. `None` means a legacy, pre-versioning template and is accepted.
    pub wscript_surface: Option<crate::scripting::WscriptSurfaceVersion>,
    /// The base was built from a definition that declares variants, so it is
    /// unsealed (§6.6): with no default variant installed — between the base
    /// landing and its default variant, after every variant build failed, or
    /// once the overlays are gone — a lab ref is refused rather than cloning
    /// it. Written `variants_expected = true`; absent means false.
    pub variants_expected: bool,
    /// The version's sealed variants, in the order the file lists them
    /// (§6.6). Empty for a base-only template. A file that reads back always
    /// obeys the default-variant rule (see [`TemplateMeta::default_variant`]).
    pub variants: Vec<VariantMeta>,
}

/// One sealed variant of a template version (§6.6): a `variant` block inside
/// `template_meta`. Every `Option` hardware or script field overrides the
/// base's own when set; [`TemplateMeta::effective`] is the merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantMeta {
    pub name: String,
    /// Written `default = true`. A lone variant is the default without it.
    pub default: bool,
    pub created: DateTime<Utc>,
    /// Hex SHA-256 digest of `variants/<name>/disk.qcow2`.
    pub sha256: String,
    /// The base `sha256` this variant was built on. A variant whose base has
    /// since changed is stale ([`TemplateMeta::is_stale`]).
    pub base_sha256: String,
    pub description: Option<String>,
    pub first_boot_script: Option<String>,
    pub agent_version: Option<String>,
    pub wscript_surface: Option<crate::scripting::WscriptSurfaceVersion>,
    pub profile: Option<String>,
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    pub firmware: Option<String>,
    pub tpm: Option<bool>,
    pub secure_boot: Option<bool>,
    pub secure_boot_keys: Option<SecureBootKeys>,
    pub display: Option<String>,
    pub cpu_model: Option<String>,
    pub hyperv: Option<bool>,
    pub accel: Option<crate::config::model::AccelMode>,
}

/// What a resolution is for, which decides whether `#base` may name the base
/// of a template that has variants (§6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// A lab machine's clone: `#base` is refused on a template with variants,
    /// because the unsealed base would give every clone the same identity.
    Lab,
    /// A variant build or a layered `from`: `#base` is the base.
    Build,
}

/// Where a variant's disk lives inside its version directory:
/// `variants/<name>/disk.qcow2`.
pub fn variant_disk_rel(name: &str) -> std::path::PathBuf {
    std::path::Path::new(VARIANTS_DIR)
        .join(name)
        .join(super::store::DISK_FILE)
}

/// The directory holding a version's variant overlays.
pub const VARIANTS_DIR: &str = "variants";

impl TemplateMeta {
    /// The variant named `name`, if the metadata records one.
    pub fn variant(&self, name: &str) -> Option<&VariantMeta> {
        self.variants.iter().find(|v| v.name == name)
    }

    /// The variant a reference with no `#` selects: the one marked
    /// `default = true`, or a lone variant. `None` for a base-only template.
    /// Parsed metadata always has exactly one when it has any variants.
    pub fn default_variant(&self) -> Option<&VariantMeta> {
        match self.variants.as_slice() {
            [] => None,
            [only] => Some(only),
            several => several.iter().find(|v| v.default),
        }
    }

    /// The variant names, the default marked, for messages:
    /// `standard (default), win3d`.
    pub fn variant_list(&self) -> String {
        let default = self.default_variant().map(|d| d.name.as_str());
        self.variants
            .iter()
            .map(|v| {
                if Some(v.name.as_str()) == default {
                    format!("{} (default)", v.name)
                } else {
                    v.name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Resolve a reference's `#<variant>` against this version (§6.6):
    /// `Some` names the variant, `None` the base, and the error is the
    /// sentence the user reads.
    ///
    /// | `variant` | with variants | without |
    /// |---|---|---|
    /// | `None` | the default variant | the base |
    /// | `#<name>` | that variant, else an error listing them | an error |
    /// | `#base` | the base, [`Purpose::Build`] only | the base |
    ///
    /// A stale variant is refused with the word to rebuild it. A base marked
    /// [`variants_expected`](Self::variants_expected) with no default variant
    /// installed refuses every [`Purpose::Lab`] reference and a bare build
    /// one; only a build's `#base` (or `#<name>`) still resolves.
    pub fn select_variant(
        &self,
        variant: Option<&VariantRef>,
        purpose: Purpose,
    ) -> Result<Option<&VariantMeta>, String> {
        let reference = format!("{}/{}@{}", self.arch, self.name, self.version);
        let has_variants = !self.variants.is_empty();
        if self.variants_expected
            && self.default_variant().is_none()
            && (purpose == Purpose::Lab || variant.is_none())
        {
            return Err(self.unbuilt_variants(&reference));
        }
        let chosen = match variant {
            None if !has_variants => return Ok(None),
            None => self.default_variant().ok_or_else(|| {
                format!(
                    "template {reference} has variants ({}) but no default variant whose disk is \
                     installed — name one with `#<variant>`",
                    self.variant_list()
                )
            })?,
            Some(VariantRef::Base) if !has_variants || purpose == Purpose::Build => {
                return Ok(None);
            }
            Some(VariantRef::Base) => {
                return Err(format!(
                    "template {reference}#{BASE_VARIANT} is refused: `#{BASE_VARIANT}` names the \
                     unsealed base disk, which only a variant build or a layered `from` may use — \
                     every clone of it would share one machine identity. Name a variant ({}) or \
                     drop the `#{BASE_VARIANT}`",
                    self.variant_list()
                ));
            }
            Some(VariantRef::Named(name)) if !has_variants => {
                return Err(format!(
                    "template {reference} has no variants, so `#{name}` names nothing — drop the \
                     `#{name}`"
                ));
            }
            Some(VariantRef::Named(name)) => self.variant(name).ok_or_else(|| {
                format!(
                    "template {reference} has no variant `{name}` (its variants: {})",
                    self.variant_list()
                )
            })?,
        };
        if self.is_stale(chosen) {
            return Err(format!(
                "variant `{}` of template {reference} is stale: it was built on base {} but the \
                 base is now {} — rebuild the variant",
                chosen.name,
                short_digest(&chosen.base_sha256),
                self.sha256.as_deref().map_or("unrecorded", short_digest),
            ));
        }
        Ok(Some(chosen))
    }

    /// Why a variant-bearing base with no default variant installed is
    /// refused, and the build that fixes it.
    fn unbuilt_variants(&self, reference: &str) -> String {
        let build = format!(
            "vmlab template build {}@{} --variant <variant>",
            self.name, self.version
        );
        if self.variants.is_empty() {
            format!(
                "template {reference} is built from a definition with variants, but its variants \
                 are not built yet, and its base is unsealed — every clone of it would share one \
                 machine identity. Build them with `{build}`"
            )
        } else {
            format!(
                "template {reference} has variants ({}) but its default variant is not \
                 installed, and its base is unsealed — build the default with `{build}`",
                self.variant_list()
            )
        }
    }

    /// Whether `variant` was built on a base other than this version's: its
    /// `base_sha256` is not the base's `sha256`. A base that records no
    /// digest cannot vouch for any variant, so every variant of it is stale.
    pub fn is_stale(&self, variant: &VariantMeta) -> bool {
        match &self.sha256 {
            Some(base) => !digest_eq(base, &variant.base_sha256),
            None => true,
        }
    }

    /// The metadata a clone of `variant` runs on (§6.6): this base's, with
    /// each field the variant sets put in its place — hardware,
    /// `first_boot_script`, `wscript_surface`, `agent_version` — and the
    /// variant's own `created` and disk digest. `None` is the base itself.
    /// The result is flat: it lists no variants. Everything downstream of a
    /// resolution — hardware (ADR-0008), first boot, the surface check,
    /// agent staleness — reads this and nothing else.
    pub fn effective(&self, variant: Option<&VariantMeta>) -> TemplateMeta {
        let mut m = self.clone();
        m.variants.clear();
        let Some(v) = variant else { return m };
        m.variants_expected = false;
        fn over<T: Clone>(slot: &mut Option<T>, value: &Option<T>) {
            if value.is_some() {
                slot.clone_from(value);
            }
        }
        over(&mut m.profile, &v.profile);
        over(&mut m.cpus, &v.cpus);
        over(&mut m.memory, &v.memory);
        over(&mut m.firmware, &v.firmware);
        over(&mut m.tpm, &v.tpm);
        over(&mut m.secure_boot, &v.secure_boot);
        over(&mut m.secure_boot_keys, &v.secure_boot_keys);
        over(&mut m.display, &v.display);
        over(&mut m.cpu_model, &v.cpu_model);
        over(&mut m.hyperv, &v.hyperv);
        over(&mut m.accel, &v.accel);
        over(&mut m.first_boot_script, &v.first_boot_script);
        over(&mut m.agent_version, &v.agent_version);
        over(&mut m.wscript_surface, &v.wscript_surface);
        m.created = v.created;
        m.sha256 = Some(v.sha256.clone());
        m
    }

    /// Embedded first-boot script paired with the surface it targets.
    pub(crate) fn first_boot(&self) -> Option<crate::scripting::EmbeddedWscript> {
        self.first_boot_script
            .clone()
            .map(|source| crate::scripting::EmbeddedWscript {
                source,
                surface_version: self.wscript_surface,
            })
    }

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
        if let Some(k) = self.secure_boot_keys {
            let _ = writeln!(out, "  secure_boot_keys = {}", quote(k.as_str()));
        }
        if let Some(d) = &self.display {
            let _ = writeln!(out, "  display = {}", quote(d));
        }
        if let Some(c) = &self.cpu_model {
            let _ = writeln!(out, "  cpu_model = {}", quote(c));
        }
        if let Some(h) = self.hyperv {
            let _ = writeln!(out, "  hyperv = {h}");
        }
        if let Some(a) = self.accel {
            let _ = writeln!(out, "  accel = {}", quote(a.as_str()));
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
        if let Some(a) = &self.agent_version {
            let _ = writeln!(out, "  agent_version = {}", quote(a));
        }
        if let Some(v) = self.wscript_surface {
            let _ = writeln!(out, "  wscript_surface = {v}");
        }
        if self.variants_expected {
            let _ = writeln!(out, "  variants_expected = true");
        }
        for v in &self.variants {
            write_variant(&mut out, v);
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
        let mut issues = crate::config::schema_issues(&doc);
        let meta = match doc.blocks().find(|b| b.kind() == "template_meta") {
            Some(block) => extract(&block, &mut issues),
            None => bail!("{name}: no `template_meta` block found"),
        };
        Ok(finish(name, source, issues, meta)?)
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

/// Emit one `variant` block, indented inside `template_meta`, in the same
/// fixed field order the schema lists.
fn write_variant(out: &mut String, v: &VariantMeta) {
    let _ = writeln!(out, "  variant {} {{", quote(&v.name));
    if v.default {
        let _ = writeln!(out, "    default = true");
    }
    let _ = writeln!(out, "    created = {}", quote(&v.created.to_rfc3339()));
    let _ = writeln!(out, "    sha256 = {}", quote(&v.sha256));
    let _ = writeln!(out, "    base_sha256 = {}", quote(&v.base_sha256));
    if let Some(d) = &v.description {
        let _ = writeln!(out, "    description = {}", quote(d));
    }
    if let Some(s) = &v.first_boot_script {
        let _ = writeln!(out, "    first_boot_script = {}", quote(s));
    }
    if let Some(a) = &v.agent_version {
        let _ = writeln!(out, "    agent_version = {}", quote(a));
    }
    if let Some(w) = v.wscript_surface {
        let _ = writeln!(out, "    wscript_surface = {w}");
    }
    if let Some(p) = &v.profile {
        let _ = writeln!(out, "    profile = {}", quote(p));
    }
    if let Some(c) = v.cpus {
        let _ = writeln!(out, "    cpus = {c}");
    }
    if let Some(m) = v.memory {
        let _ = writeln!(out, "    memory = {}", format_size(m));
    }
    if let Some(f) = &v.firmware {
        let _ = writeln!(out, "    firmware = {}", quote(f));
    }
    if let Some(t) = v.tpm {
        let _ = writeln!(out, "    tpm = {t}");
    }
    if let Some(s) = v.secure_boot {
        let _ = writeln!(out, "    secure_boot = {s}");
    }
    if let Some(k) = v.secure_boot_keys {
        let _ = writeln!(out, "    secure_boot_keys = {}", quote(k.as_str()));
    }
    if let Some(d) = &v.display {
        let _ = writeln!(out, "    display = {}", quote(d));
    }
    if let Some(c) = &v.cpu_model {
        let _ = writeln!(out, "    cpu_model = {}", quote(c));
    }
    if let Some(h) = v.hyperv {
        let _ = writeln!(out, "    hyperv = {h}");
    }
    if let Some(a) = v.accel {
        let _ = writeln!(out, "    accel = {}", quote(a.as_str()));
    }
    let _ = writeln!(out, "  }}");
}

/// The first 12 hex digits of a digest, for messages.
fn short_digest(d: &str) -> &str {
    let d = d.strip_prefix("sha256:").unwrap_or(d);
    &d[..d.len().min(12)]
}

/// Hex digests compare case-insensitively, with or without `sha256:`.
fn digest_eq(a: &str, b: &str) -> bool {
    let bare = |d: &str| d.strip_prefix("sha256:").unwrap_or(d).to_ascii_lowercase();
    bare(a) == bare(b)
}

/// The metadata field mapping. Reading, coercion, spans and wording all
/// come from [`crate::config::block`] (ADR-0006); the emitter above stays
/// hand-written, which is the one asymmetry left in this path.
fn extract(b: &Block, issues: &mut IssueList) -> Option<TemplateMeta> {
    let mut r = Reader::new(b, issues);
    // Read every field before giving up on a missing one, so a single pass
    // reports everything wrong with the file — including when it is the
    // name label that is missing.
    let name = r.label();
    let created = r.required("created", |r, n| {
        r.parsed(n, |s| {
            DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|e| format!("malformed `created` timestamp `{s}`: {e}"))
        })
    });
    let arch = r.required_string("arch");
    let version = r.required_string("version");
    let profile = r.string("profile").unspan();
    // `cpus` only guards its sign here: what an already-built store holds is
    // not this change's business (the lab file's own `cpus` takes 1, §5.1).
    let cpus = r.int_at_least("cpus", 0).unspan();
    let memory = r.size("memory").unspan();
    let disk = r.size("disk").unspan();
    let firmware = r.string("firmware").unspan();
    let tpm = r.bool("tpm").unspan();
    let secure_boot = r.bool("secure_boot").unspan();
    // Lenient, like `firmware`: an unknown key store reads as unset.
    let secure_boot_keys = r
        .string("secure_boot_keys")
        .unspan()
        .and_then(|s| SecureBootKeys::parse(&s));
    let display = r.string("display").unspan();
    let cpu_model = r
        .parsed("cpu_model", crate::config::model::parse_cpu_model)
        .unspan();
    let hyperv = r.bool("hyperv").unspan();
    // Lenient, like `firmware`: a spelling this build does not know reads
    // as unset rather than making the store entry unreadable.
    let accel = r
        .string("accel")
        .unspan()
        .and_then(|s| crate::config::model::AccelMode::parse(&s));
    let origin = r.string("origin").unspan();
    let registry = r.string("registry").unspan();
    let sha256 = r.string("sha256").unspan();
    let first_boot_script = r.string("first_boot_script").unspan();
    let agent_version = r.string("agent_version").unspan();
    let wscript_surface = r
        .int_at_least::<u32>("wscript_surface", 0)
        .map(|version| version.map(Into::into))
        .unspan();
    let variants_expected = r.bool("variants_expected").unspan().unwrap_or(false);
    let variants = extract_variants(&mut r);
    Some(TemplateMeta {
        name: name?,
        arch: arch?.value,
        version: version?.value,
        created: created?.value,
        profile,
        cpus,
        memory,
        disk,
        firmware,
        tpm,
        secure_boot,
        secure_boot_keys,
        display,
        cpu_model,
        hyperv,
        accel,
        origin,
        registry,
        sha256,
        first_boot_script,
        agent_version,
        wscript_surface,
        variants_expected,
        variants: variants?,
    })
}

/// Every `variant` child, then the rules that hold across them (§6.6): names
/// follow the variant grammar, are unique and are never `base`; and exactly
/// one is the default — written `default = true`, or implied for a lone
/// variant. `None` when any of that fails, so a template whose default is
/// ambiguous is never resolved against.
fn extract_variants(r: &mut Reader) -> Option<Vec<VariantMeta>> {
    let mut out = Vec::new();
    let mut spans = Vec::new();
    let mut ok = true;
    for child in r.children().filter(|c| c.kind() == "variant") {
        let span = crate::config::block::span_of(&child);
        match extract_variant(&child, r.issues()) {
            Some(v) => {
                if let Err(e) = crate::config::model::check_variant_name(&v.name) {
                    r.issue_at(span, format!("variant \"{}\": {e}", v.name));
                    ok = false;
                }
                if out.iter().any(|o: &VariantMeta| o.name == v.name) {
                    r.issue_at(span, format!("variant \"{}\" is recorded twice", v.name));
                    ok = false;
                }
                out.push(v);
                spans.push(span);
            }
            None => ok = false,
        }
    }
    let defaults: Vec<&str> = out
        .iter()
        .filter(|v| v.default)
        .map(|v| v.name.as_str())
        .collect();
    if out.len() > 1 && defaults.len() != 1 {
        let names: Vec<&str> = out.iter().map(|v| v.name.as_str()).collect();
        let why = if defaults.is_empty() {
            "none of them is marked `default = true`".to_string()
        } else {
            format!(
                "several are marked `default = true` ({})",
                defaults.join(", ")
            )
        };
        r.issue(format!(
            "template has variants {} but {why} — exactly one variant must be the default",
            names.join(", ")
        ));
        ok = false;
    }
    ok.then_some(out)
}

fn extract_variant(b: &Block, issues: &mut IssueList) -> Option<VariantMeta> {
    let mut r = Reader::new(b, issues);
    let name = r.label();
    let created = r.required("created", |r, n| {
        r.parsed(n, |s| {
            DateTime::parse_from_rfc3339(s)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|e| format!("malformed `created` timestamp `{s}`: {e}"))
        })
    });
    let sha256 = r.required_string("sha256");
    let base_sha256 = r.required_string("base_sha256");
    let default = r.bool("default").unspan().unwrap_or(false);
    let description = r.string("description").unspan();
    let first_boot_script = r.string("first_boot_script").unspan();
    let agent_version = r.string("agent_version").unspan();
    let wscript_surface = r
        .int_at_least::<u32>("wscript_surface", 0)
        .map(|version| version.map(Into::into))
        .unspan();
    let profile = r.string("profile").unspan();
    let cpus = r.int_at_least("cpus", 0).unspan();
    let memory = r.size("memory").unspan();
    let firmware = r.string("firmware").unspan();
    let tpm = r.bool("tpm").unspan();
    let secure_boot = r.bool("secure_boot").unspan();
    let secure_boot_keys = r
        .string("secure_boot_keys")
        .unspan()
        .and_then(|s| SecureBootKeys::parse(&s));
    let display = r.string("display").unspan();
    let cpu_model = r
        .parsed("cpu_model", crate::config::model::parse_cpu_model)
        .unspan();
    let hyperv = r.bool("hyperv").unspan();
    let accel = r
        .string("accel")
        .unspan()
        .and_then(|s| crate::config::model::AccelMode::parse(&s));
    Some(VariantMeta {
        name: name?,
        default,
        created: created?.value,
        sha256: sha256?.value,
        base_sha256: base_sha256?.value,
        description,
        first_boot_script,
        agent_version,
        wscript_surface,
        profile,
        cpus,
        memory,
        firmware,
        tpm,
        secure_boot,
        secure_boot_keys,
        display,
        cpu_model,
        hyperv,
        accel,
    })
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
            secure_boot_keys: Some(SecureBootKeys::Setup),
            display: Some("vnc".into()),
            cpu_model: Some("pentium3".into()),
            hyperv: Some(false),
            accel: Some(crate::config::model::AccelMode::Tcg),
            created: "2026-06-12T10:20:30.123456Z".parse().unwrap(),
            origin: Some("https://example.com/win11.iso".into()),
            registry: Some("ghcr.io/vmlabdev/vmlab-templates/win11".into()),
            sha256: Some("ab".repeat(32)),
            first_boot_script: Some(
                "use vmlab\nfn main(lab) {\n    let vm = lab.this_vm()\n}\n".into(),
            ),
            agent_version: Some("agent=abc123".into()),
            wscript_surface: Some(crate::scripting::WSCRIPT_SURFACE_VERSION),
            variants_expected: false,
            variants: Vec::new(),
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
            secure_boot_keys: None,
            display: None,
            cpu_model: None,
            hyperv: None,
            accel: None,
            created: "2026-01-02T03:04:05Z".parse().unwrap(),
            origin: None,
            registry: None,
            sha256: None,
            first_boot_script: None,
            agent_version: None,
            wscript_surface: None,
            variants_expected: false,
            variants: Vec::new(),
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

    /// Every key store a template records round-trips through the file, a
    /// variant's overrides the base's, and a spelling this build does not
    /// know reads as unset rather than breaking the store entry.
    #[test]
    fn key_store_round_trips_and_is_lenient() {
        for keys in [
            SecureBootKeys::Microsoft,
            SecureBootKeys::Setup,
            SecureBootKeys::Vars,
        ] {
            let mut meta = full_meta();
            meta.secure_boot_keys = Some(keys);
            let mut v = variant("v");
            v.secure_boot_keys = Some(SecureBootKeys::Setup);
            meta.variants = vec![v.clone()];
            let text = meta.to_wcl();
            assert!(
                text.contains(&format!("secure_boot_keys = \"{}\"", keys.as_str())),
                "{text}"
            );
            let back = TemplateMeta::from_wcl(&text, "<test>").unwrap();
            assert_eq!(back, meta);
            assert_eq!(
                back.effective(Some(&v)).secure_boot_keys,
                Some(SecureBootKeys::Setup)
            );
            assert_eq!(back.effective(None).secure_boot_keys, Some(keys));
        }
        let text = full_meta().to_wcl().replace(
            "secure_boot_keys = \"setup\"",
            "secure_boot_keys = \"tpm-sealed\"",
        );
        let back = TemplateMeta::from_wcl(&text, "<test>").unwrap();
        assert_eq!(back.secure_boot_keys, None);
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

    /// The diagnostics a corrupt metadata file now produces — same wording
    /// as a lab file, anchored at the line (ADR-0006).
    fn meta_err(body: &str) -> String {
        let src = format!("{SCHEMA_IMPORT}\n{body}");
        let err = TemplateMeta::from_wcl(&src, "template.wcl").expect_err("should be rejected");
        format!("{err:#}")
    }

    #[test]
    fn rejects_missing_required_field() {
        assert_eq!(
            meta_err("template_meta \"x\" { arch = \"x86_64\" version = \"1\" }\n"),
            "1 error(s) in template.wcl\n  \
             template.wcl:2:1: missing required field `created`"
        );
        assert!(
            meta_err("template_meta \"x\" { created = \"2026-01-02T03:04:05Z\" }\n")
                .contains("missing required field `arch`")
        );
    }

    #[test]
    fn rejects_corrupt_values_with_shared_wording() {
        assert!(
            meta_err(
                "template_meta \"x\" { arch = 7 version = \"1\" \
                 created = \"2026-01-02T03:04:05Z\" }\n"
            )
            .contains("`arch` must be a string, got an integer"),
        );
        assert!(
            meta_err(
                "template_meta \"x\" { arch = \"a\" version = \"1\" \
                 created = \"2026-01-02T03:04:05Z\" memory = -1 }\n"
            )
            .contains("`memory` must be at least 0, got -1"),
        );
        assert!(
            meta_err(
                "template_meta \"x\" { arch = \"a\" version = \"1\" \
                 created = \"2026-01-02T03:04:05Z\" tpm = \"yes\" }\n"
            )
            .contains("`tpm` must be a bool, got a string"),
        );
    }

    #[test]
    fn every_mistake_in_a_metadata_file_is_reported_at_once() {
        let err = meta_err(
            "template_meta \"x\" {\n  arch = 7\n  version = \"1\"\n  \
             created = \"yesterday\"\n}\n",
        );
        // Both mistakes in one pass, each at its own line — the second is
        // no longer hidden behind the first.
        assert!(
            err.contains("template.wcl:3:3: `arch` must be a string"),
            "{err}"
        );
        assert!(
            err.contains("template.wcl:5:3: malformed `created`"),
            "{err}"
        );
    }

    /// A field the metadata schema does not name is the schema's to reject,
    /// not the extractor's — but it must still reach the user, positioned.
    #[test]
    fn rejects_unknown_field() {
        let err = meta_err(
            "template_meta \"x\" { arch = \"a\" version = \"1\" \
             created = \"2026-01-02T03:04:05Z\" bogus = 1 }\n",
        );
        assert!(err.contains("bogus"), "{err}");
        assert!(err.contains("template.wcl:2:"), "unpositioned: {err}");
    }

    /// A metadata block with no name label still reports what else is wrong
    /// with it — the label failure does not swallow the rest of the pass.
    #[test]
    fn a_nameless_block_still_reports_its_other_mistakes() {
        let err = meta_err("template_meta { arch = 7 }\n");
        assert!(
            err.contains("`template_meta` requires a name label"),
            "{err}"
        );
        assert!(err.contains("`arch` must be a string"), "{err}");
        assert!(err.contains("missing required field `version`"), "{err}");
    }

    /// Spans survive as spans, not just as rendered text (ADR-0006, story 19).
    #[test]
    fn a_metadata_issue_carries_a_span_a_surface_can_use() {
        let source = format!("{SCHEMA_IMPORT}\ntemplate_meta \"x\" {{ arch = 7 }}\n");
        let err = TemplateMeta::from_wcl(&source, "template.wcl").unwrap_err();
        let diag = err
            .downcast_ref::<crate::config::block::IssueError>()
            .expect("metadata errors carry their issues")
            .diagnostic();
        let span = diag
            .issues
            .iter()
            .find_map(|i| i.span.filter(|_| i.message.contains("must be a string")))
            .expect("positioned type error");
        assert_eq!(
            &source[span.offset()..span.offset() + span.len()],
            "arch = 7"
        );
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

    // ---- variants (§6.6) -------------------------------------------------

    fn variant(name: &str) -> VariantMeta {
        VariantMeta {
            name: name.into(),
            default: false,
            created: "2026-07-01T00:00:00Z".parse().unwrap(),
            sha256: "cd".repeat(32),
            base_sha256: "ab".repeat(32),
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
            secure_boot_keys: None,
            display: None,
            cpu_model: None,
            hyperv: None,
            accel: None,
        }
    }

    /// `full_meta` with `variants`: its `sha256` is `ab…`, which is what
    /// [`variant`] records as its base.
    fn with_variants(variants: Vec<VariantMeta>) -> TemplateMeta {
        TemplateMeta {
            variants,
            ..full_meta()
        }
    }

    #[test]
    fn round_trip_variants() {
        let mut standard = variant("standard");
        standard.default = true;
        standard.first_boot_script = Some("use vmlab\nfn main(lab) {}\n".into());
        standard.wscript_surface = Some(crate::scripting::WSCRIPT_SURFACE_VERSION);
        standard.agent_version = Some("agent=def456".into());
        standard.description = Some("sysprepped".into());
        let mut win3d = variant("win3d");
        win3d.secure_boot = Some(false);
        win3d.memory = Some(16 << 30);
        win3d.cpus = Some(8);
        win3d.profile = Some("windows11-3d".into());
        win3d.firmware = Some("uefi".into());
        win3d.tpm = Some(false);
        win3d.display = Some("spice".into());
        win3d.cpu_model = Some("host".into());
        win3d.accel = Some(crate::config::model::AccelMode::Kvm);
        let meta = with_variants(vec![standard, win3d]);
        let text = meta.to_wcl();
        assert!(text.contains("  variant \"standard\" {"), "{text}");
        let back = TemplateMeta::from_wcl(&text, "<test>").unwrap();
        assert_eq!(meta, back);
        assert_eq!(back.default_variant().unwrap().name, "standard");
    }

    /// The default rules: one marked default, or a lone variant implicitly;
    /// none or several marked among two or more is invalid metadata.
    #[test]
    fn default_variant_rules() {
        // A lone variant needs no mark.
        let lone = with_variants(vec![variant("standard")]);
        let back = TemplateMeta::from_wcl(&lone.to_wcl(), "<test>").unwrap();
        assert_eq!(back.default_variant().unwrap().name, "standard");
        // No variants: no default.
        assert!(full_meta().default_variant().is_none());

        // Two, neither marked.
        let err = meta_err_of(&with_variants(vec![variant("a"), variant("b")]));
        assert!(
            err.contains("none of them is marked `default = true`"),
            "{err}"
        );
        assert!(err.contains("a, b"), "{err}");
        // Two, both marked.
        let (mut a, mut b) = (variant("a"), variant("b"));
        a.default = true;
        b.default = true;
        let err = meta_err_of(&with_variants(vec![a.clone(), b]));
        assert!(err.contains("several are marked"), "{err}");
        // Exactly one marked among three.
        let meta = with_variants(vec![variant("x"), a, variant("y")]);
        let back = TemplateMeta::from_wcl(&meta.to_wcl(), "<test>").unwrap();
        assert_eq!(back.default_variant().unwrap().name, "a");
    }

    fn meta_err_of(meta: &TemplateMeta) -> String {
        let err =
            TemplateMeta::from_wcl(&meta.to_wcl(), "template.wcl").expect_err("should be rejected");
        format!("{err:#}")
    }

    #[test]
    fn variant_names_are_checked() {
        let err = meta_err_of(&with_variants(vec![variant("base")]));
        assert!(err.contains("reserved"), "{err}");
        let err = meta_err_of(&with_variants(vec![variant("Win3D")]));
        assert!(err.contains("malformed variant name"), "{err}");
        let mut one = variant("dup");
        one.default = true;
        let err = meta_err_of(&with_variants(vec![one, variant("dup")]));
        assert!(err.contains("recorded twice"), "{err}");
        // A variant block missing its digests is reported like any field.
        let src = format!(
            "{SCHEMA_IMPORT}\ntemplate_meta \"x\" {{ arch = \"a\" version = \"1\" \
             created = \"2026-01-02T03:04:05Z\"\n  variant \"v\" {{ created = \
             \"2026-01-02T03:04:05Z\" }}\n}}\n"
        );
        let err = format!("{:#}", TemplateMeta::from_wcl(&src, "t.wcl").unwrap_err());
        assert!(err.contains("missing required field `sha256`"), "{err}");
        assert!(
            err.contains("missing required field `base_sha256`"),
            "{err}"
        );
    }

    /// Effective metadata is the base overridden field by field: what the
    /// variant sets wins, what it leaves alone is the base's, and the result
    /// is flat.
    #[test]
    fn effective_metadata_merges_field_by_field() {
        let mut v = variant("win3d");
        v.secure_boot = Some(false);
        v.memory = Some(16 << 30);
        v.first_boot_script = Some("variant script".into());
        v.agent_version = Some("agent=variant".into());
        v.wscript_surface = Some(1.into());
        let mut base = with_variants(vec![v.clone()]);
        base.secure_boot = Some(true);
        let eff = base.effective(Some(&v));
        assert_eq!(eff.secure_boot, Some(false));
        assert_eq!(eff.memory, Some(16 << 30));
        assert_eq!(eff.first_boot_script.as_deref(), Some("variant script"));
        assert_eq!(eff.agent_version.as_deref(), Some("agent=variant"));
        assert_eq!(eff.wscript_surface, Some(1.into()));
        assert_eq!(eff.sha256.as_deref(), Some(v.sha256.as_str()));
        assert_eq!(eff.created, v.created);
        // Untouched fields are the base's.
        assert_eq!(eff.cpus, base.cpus);
        assert_eq!(eff.profile, base.profile);
        assert_eq!(eff.tpm, base.tpm);
        assert_eq!(eff.disk, base.disk);
        assert_eq!(eff.firmware, base.firmware);
        assert_eq!(eff.version, base.version);
        assert!(eff.variants.is_empty());
        // The base's own effective metadata is itself, flattened.
        let plain = base.effective(None);
        assert_eq!(plain.secure_boot, Some(true));
        assert_eq!(plain.first_boot_script, base.first_boot_script);
        assert!(plain.variants.is_empty());
    }

    /// `variants_expected` writes only when set and reads back; a file
    /// without it is an ordinary base.
    #[test]
    fn round_trip_variants_expected() {
        let mut meta = full_meta();
        assert!(!meta.to_wcl().contains("variants_expected"));
        meta.variants_expected = true;
        let text = meta.to_wcl();
        assert!(text.contains("  variants_expected = true\n"), "{text}");
        let back = TemplateMeta::from_wcl(&text, "<test>").unwrap();
        assert_eq!(back, meta);
        let mut built = with_variants(vec![variant("standard")]);
        built.variants_expected = true;
        let back = TemplateMeta::from_wcl(&built.to_wcl(), "<test>").unwrap();
        assert_eq!(back, built);
        // A variant's effective metadata is sealed: it carries no marker.
        assert!(!built.effective(built.variants.first()).variants_expected);
    }

    /// A variant-bearing base with no default variant installed refuses every
    /// lab reference — bare, `#<name>` and `#base` — naming the build that
    /// fixes it; a build's `#base` and `#<name>` still resolve, its bare ref
    /// does not.
    #[test]
    fn an_unbuilt_variant_bearing_base_is_never_cloned() {
        let base_ref = VariantRef::Base;
        let named = VariantRef::Named("extra".into());
        let refs = [None, Some(&named), Some(&base_ref)];

        let mut none = full_meta();
        none.variants_expected = true;
        for r in refs {
            let err = none.select_variant(r, Purpose::Lab).unwrap_err();
            assert!(err.contains("template x86_64/win11@26100.1 "), "{err}");
            assert!(err.contains("variants are not built yet"), "{err}");
            assert!(
                err.contains("vmlab template build win11@26100.1 --variant <variant>"),
                "{err}"
            );
        }
        assert_eq!(
            none.select_variant(Some(&base_ref), Purpose::Build),
            Ok(None)
        );
        let err = none.select_variant(None, Purpose::Build).unwrap_err();
        assert!(err.contains("variants are not built yet"), "{err}");

        // Two variants left, the default's overlay gone: no default.
        let mut headless = with_variants(vec![variant("extra"), variant("other")]);
        headless.variants_expected = true;
        for r in refs {
            let err = headless.select_variant(r, Purpose::Lab).unwrap_err();
            assert!(err.contains("default variant is not installed"), "{err}");
            assert!(err.contains("extra, other"), "{err}");
            assert!(err.contains("--variant <variant>"), "{err}");
        }
        assert_eq!(
            headless.select_variant(Some(&base_ref), Purpose::Build),
            Ok(None)
        );
        assert_eq!(
            headless
                .select_variant(Some(&named), Purpose::Build)
                .unwrap()
                .map(|v| v.name.as_str()),
            Some("extra")
        );

        // Once the default lands, the marker changes nothing.
        let mut built = with_variants(vec![variant("standard")]);
        built.variants_expected = true;
        assert_eq!(
            built
                .select_variant(None, Purpose::Lab)
                .unwrap()
                .map(|v| v.name.as_str()),
            Some("standard")
        );
        assert!(built.select_variant(Some(&base_ref), Purpose::Lab).is_err());
        assert_eq!(
            built.select_variant(Some(&base_ref), Purpose::Build),
            Ok(None)
        );
    }

    #[test]
    fn stale_detection() {
        let fresh = variant("standard");
        let base = with_variants(vec![fresh.clone()]);
        assert!(!base.is_stale(&fresh));
        // Digest spelling does not matter.
        let mut upper = fresh.clone();
        upper.base_sha256 = format!("sha256:{}", "AB".repeat(32));
        assert!(!base.is_stale(&upper));
        // A rebuilt base leaves the variant behind.
        let mut rebuilt = base.clone();
        rebuilt.sha256 = Some("ef".repeat(32));
        assert!(rebuilt.is_stale(&fresh));
        // A base with no digest vouches for nothing.
        let mut unrecorded = base.clone();
        unrecorded.sha256 = None;
        assert!(unrecorded.is_stale(&fresh));
        let err = rebuilt
            .select_variant(None, Purpose::Lab)
            .expect_err("stale is refused");
        assert!(err.contains("stale"), "{err}");
        assert!(err.contains("rebuild the variant"), "{err}");
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
