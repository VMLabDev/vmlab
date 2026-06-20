//! The template-metadata config blob (PRD §6.4).
//!
//! The OCI config blob carries the template's recorded hardware / profile /
//! provenance as JSON (media type
//! [`media_types::CONFIG_TEMPLATE_JSON`]). `TemplateMeta` itself is rendered
//! as WCL on disk and is not `serde`-derivable, so this module owns a
//! flat JSON mirror that round-trips to and from it. The JSON encoding is
//! deliberately independent of the on-disk WCL form so generic registry
//! tooling can introspect the config.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::template::TemplateMeta;

/// JSON form of [`TemplateMeta`] for the config blob. Field names are
/// stable wire keys; absent optionals are omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateConfig {
    pub name: String,
    pub arch: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cpus: Option<u32>,
    /// RAM in bytes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub memory: Option<u64>,
    /// Primary disk virtual size in bytes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub disk: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tpm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub secure_boot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sha256: Option<String>,
    /// Embedded first-boot wisp script source (PRD §6.1).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_boot_script: Option<String>,
}

impl TemplateConfig {
    pub fn from_meta(meta: &TemplateMeta) -> Self {
        Self {
            name: meta.name.clone(),
            arch: meta.arch.clone(),
            version: meta.version.clone(),
            profile: meta.profile.clone(),
            cpus: meta.cpus,
            memory: meta.memory,
            disk: meta.disk,
            firmware: meta.firmware.clone(),
            tpm: meta.tpm,
            secure_boot: meta.secure_boot,
            display: meta.display.clone(),
            created: meta.created.to_rfc3339(),
            origin: meta.origin.clone(),
            registry: meta.registry.clone(),
            sha256: meta.sha256.clone(),
            first_boot_script: meta.first_boot_script.clone(),
        }
    }

    /// Convert back into a [`TemplateMeta`]. `origin_override`, when set,
    /// replaces the recorded origin (pull records the originating registry
    /// reference, PRD §6.4).
    pub fn into_meta(self, origin_override: Option<String>) -> Result<TemplateMeta> {
        let created = chrono::DateTime::parse_from_rfc3339(&self.created)
            .map_err(|e| anyhow::anyhow!("malformed `created` timestamp `{}`: {e}", self.created))?
            .with_timezone(&chrono::Utc);
        Ok(TemplateMeta {
            name: self.name,
            arch: self.arch,
            version: self.version,
            profile: self.profile,
            cpus: self.cpus,
            memory: self.memory,
            disk: self.disk,
            firmware: self.firmware,
            tpm: self.tpm,
            secure_boot: self.secure_boot,
            display: self.display,
            created,
            origin: origin_override.or(self.origin),
            registry: self.registry,
            sha256: self.sha256,
            first_boot_script: self.first_boot_script,
        })
    }

    /// Serialise to canonical JSON bytes.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(Into::into)
    }

    /// Parse from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| anyhow::anyhow!("malformed template config blob: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> TemplateMeta {
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
            first_boot_script: Some("use vmlab\nfn main(lab) { let vm = lab.this_vm() }\n".into()),
        }
    }

    #[test]
    fn meta_json_round_trip() {
        let m = meta();
        let cfg = TemplateConfig::from_meta(&m);
        let json = cfg.to_json().unwrap();
        let back = TemplateConfig::from_json(&json).unwrap();
        assert_eq!(cfg, back);
        let m2 = back.into_meta(None).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn origin_override_records_reference() {
        let m = meta();
        let cfg = TemplateConfig::from_meta(&m);
        let m2 = cfg
            .into_meta(Some("ghcr.io/owner/win11:26100.1".into()))
            .unwrap();
        assert_eq!(m2.origin.as_deref(), Some("ghcr.io/owner/win11:26100.1"));
    }
}
