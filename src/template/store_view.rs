//! What a `store.*` command answers with (ADR-0010), in the shape ADR-0004
//! settled on for lab status.
//!
//! The supervisor builds these values and the CLI renders them. Neither side
//! reads a key out of an untyped map, which is the whole point: rename a field
//! here and every consumer stops compiling, where a hand-built
//! `json!` object and a `v["freed"].as_u64().unwrap_or(0)` on the far side
//! would have rendered a confident zero instead.
//!
//! [`TemplateSummary`] is a published contract as well as an internal one:
//! it is exactly what `vmlab template list --json` prints, so every field is
//! always present and `null` stands for "the template does not record this".

use serde::{Deserialize, Serialize};

use super::meta::TemplateMeta;

/// One exact version in the store, as `<arch>/<name>@<version>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredVersion {
    pub arch: String,
    pub name: String,
    pub version: String,
}

impl StoredVersion {
    pub fn of(meta: &TemplateMeta) -> Self {
        Self {
            arch: meta.arch.clone(),
            name: meta.name.clone(),
            version: meta.version.clone(),
        }
    }
}

impl std::fmt::Display for StoredVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}@{}", self.arch, self.name, self.version)
    }
}

/// A template's recorded metadata, in the fixed shape `--json` prints.
///
/// Field for field what `template.wcl` holds, minus the two a listing has no
/// use for (the first-boot script's whole source, and the baked agent's
/// version).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateSummary {
    pub arch: String,
    pub name: String,
    pub version: String,
    /// `<arch>/<name>@<version>`, so a script can pass one field straight
    /// back to another verb.
    #[serde(rename = "ref")]
    pub reference: String,
    pub profile: Option<String>,
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    pub disk: Option<u64>,
    pub firmware: Option<String>,
    pub tpm: Option<bool>,
    pub secure_boot: Option<bool>,
    pub display: Option<String>,
    /// RFC 3339.
    pub created: String,
    pub origin: Option<String>,
    pub registry: Option<String>,
    pub sha256: Option<String>,
}

impl From<&TemplateMeta> for TemplateSummary {
    fn from(t: &TemplateMeta) -> Self {
        Self {
            arch: t.arch.clone(),
            name: t.name.clone(),
            version: t.version.clone(),
            reference: StoredVersion::of(t).to_string(),
            profile: t.profile.clone(),
            cpus: t.cpus,
            memory: t.memory,
            disk: t.disk,
            firmware: t.firmware.clone(),
            tpm: t.tpm,
            secure_boot: t.secure_boot,
            display: t.display.clone(),
            created: t.created.to_rfc3339(),
            origin: t.origin.clone(),
            registry: t.registry.clone(),
            sha256: t.sha256.clone(),
        }
    }
}

/// Whether the registry a template names already carries this exact version
/// and architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteStatus {
    /// Published: nothing to upload.
    #[serde(rename = "yes")]
    Published,
    /// The registry is reachable and does not have it.
    #[serde(rename = "no")]
    Missing,
    /// The template names no registry, so there is nothing to compare against.
    #[serde(rename = "local")]
    Local,
    /// The registry reference is malformed and could not be asked.
    #[serde(rename = "?")]
    Unknown,
}

impl RemoteStatus {
    /// The one-word spelling the `REMOTE` column shows, which is also the
    /// wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteStatus::Published => "yes",
            RemoteStatus::Missing => "no",
            RemoteStatus::Local => "local",
            RemoteStatus::Unknown => "?",
        }
    }
}

/// One row of `store.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntry {
    pub meta: TemplateSummary,
    /// Size of `disk.qcow2` in bytes; 0 when it cannot be read.
    pub size: u64,
    /// Absent unless the caller asked for the registry check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteStatus>,
}

/// What `store.prune` would do, or did.
///
/// The plan is the answer whether or not it was carried out (ADR-0003): a dry
/// run is the same computation stopped one step earlier, not a second code
/// path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrunePlan {
    /// Superseded builds, oldest first.
    pub remove: Vec<StoredVersion>,
    /// Superseded builds held back because a clone still leans on them.
    pub skipped: Vec<StoredVersion>,
    /// Bytes the `remove` list accounts for.
    pub freed: u64,
    /// Whether the removals actually happened.
    pub applied: bool,
}

/// What `store.push` answers as soon as the upload is claimed and running —
/// everything the caller needs to report the result it is about to wait for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushStarted {
    pub pushing: StoredVersion,
    /// The version-tagged registry reference being written.
    pub target: String,
    /// Source repository the package will be linked to, when the caller
    /// supplied or detected one.
    pub source: Option<String>,
    /// The alias moved on success: `latest`, or `latest-prerelease`.
    pub moving_tag: String,
}
