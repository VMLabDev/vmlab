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
use crate::template::meta::VariantMeta;

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
    /// QEMU `-cpu` model (§5.2).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cpu_model: Option<String>,
    /// QEMU accelerator (§5.2), by its schema spelling. Free text so a
    /// spelling a newer build wrote reads as unset rather than failing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub accel: Option<String>,
    /// RFC 3339 creation timestamp.
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sha256: Option<String>,
    /// Embedded first-boot wscript script source (PRD §6.1).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub first_boot_script: Option<String>,
    /// vmlab-agent stamp baked into the image by the template build.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_version: Option<String>,
    /// Host wscript surface used by embedded scripts.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub wscript_surface: Option<crate::scripting::WscriptSurfaceVersion>,
    /// A base built from a definition with variants (§6.6): with no default
    /// variant installed, a lab never clones it. A client that predates the
    /// marker ignores it and pulls a base-only template.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub variants_expected: bool,
    /// Set on a variant's config blob only (§6.6): which variant the
    /// effective metadata above belongs to. A client that predates variants
    /// ignores it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub variant: Option<VariantConfig>,
}

/// What a variant's config blob adds to its effective metadata: the
/// variant's identity and the base it was built on. Its `created` and
/// `sha256` are the config's own, the variant's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariantConfig {
    pub name: String,
    /// `sha256` of the base the overlay was built on.
    pub base_sha256: String,
    #[serde(default)]
    pub default: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
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
            cpu_model: meta.cpu_model.clone(),
            accel: meta.accel.map(|a| a.as_str().to_string()),
            created: meta.created.to_rfc3339(),
            origin: meta.origin.clone(),
            registry: meta.registry.clone(),
            sha256: meta.sha256.clone(),
            first_boot_script: meta.first_boot_script.clone(),
            agent_version: meta.agent_version.clone(),
            wscript_surface: meta.wscript_surface,
            variants_expected: meta.variants_expected,
            variant: None,
        }
    }

    /// A variant's config blob (§6.6): `base`'s metadata as the variant
    /// sees it ([`TemplateMeta::effective`]), plus the variant's name,
    /// default flag, description and the base digest it was built on.
    pub fn from_variant(base: &TemplateMeta, variant: &VariantMeta) -> Self {
        let mut config = Self::from_meta(&base.effective(Some(variant)));
        config.variant = Some(VariantConfig {
            name: variant.name.clone(),
            base_sha256: variant.base_sha256.clone(),
            default: variant.default,
            description: variant.description.clone(),
        });
        config
    }

    /// The `variant` block a variant's config blob stands for, against the
    /// `base` it is pulled onto. The blob carries effective metadata, so a
    /// field is an override exactly where it differs from the base's — which
    /// resolves to the same effective metadata as the block it was pushed
    /// from.
    pub fn into_variant(self, base: &TemplateMeta) -> Result<VariantMeta> {
        let Some(v) = self.variant.clone() else {
            anyhow::bail!("the config blob is a base's, not a variant's");
        };
        let sha256 = self
            .sha256
            .clone()
            .ok_or_else(|| anyhow::anyhow!("variant `{}`'s config blob has no sha256", v.name))?;
        let m = self.into_meta(None)?;
        fn over<T: PartialEq + Clone>(effective: &Option<T>, base: &Option<T>) -> Option<T> {
            (effective != base).then(|| effective.clone()).flatten()
        }
        Ok(VariantMeta {
            name: v.name,
            default: v.default,
            created: m.created,
            sha256,
            base_sha256: v.base_sha256,
            description: v.description,
            first_boot_script: over(&m.first_boot_script, &base.first_boot_script),
            agent_version: over(&m.agent_version, &base.agent_version),
            wscript_surface: over(&m.wscript_surface, &base.wscript_surface),
            profile: over(&m.profile, &base.profile),
            cpus: over(&m.cpus, &base.cpus),
            memory: over(&m.memory, &base.memory),
            firmware: over(&m.firmware, &base.firmware),
            tpm: over(&m.tpm, &base.tpm),
            secure_boot: over(&m.secure_boot, &base.secure_boot),
            display: over(&m.display, &base.display),
            cpu_model: over(&m.cpu_model, &base.cpu_model),
            accel: over(&m.accel, &base.accel),
        })
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
            cpu_model: self.cpu_model,
            accel: self
                .accel
                .as_deref()
                .and_then(crate::config::model::AccelMode::parse),
            created,
            origin: origin_override.or(self.origin),
            registry: self.registry,
            sha256: self.sha256,
            first_boot_script: self.first_boot_script,
            agent_version: self.agent_version,
            wscript_surface: self.wscript_surface,
            variants_expected: self.variants_expected,
            // Variants travel as their own manifests (§6.6), not in the
            // base's config blob.
            variants: Vec::new(),
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
            cpu_model: Some("pentium3".into()),
            accel: Some(crate::config::model::AccelMode::Tcg),
            created: "2026-06-12T10:20:30.123456Z".parse().unwrap(),
            origin: Some("https://example.com/win11.iso".into()),
            registry: Some("ghcr.io/vmlabdev/vmlab-templates/win11".into()),
            sha256: Some("ab".repeat(32)),
            first_boot_script: Some("use vmlab\nfn main(lab) { let vm = lab.this_vm() }\n".into()),
            agent_version: Some("agent=abc123".into()),
            wscript_surface: Some(crate::scripting::WSCRIPT_SURFACE_VERSION),
            variants_expected: false,
            variants: Vec::new(),
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

    #[test]
    fn legacy_config_without_a_surface_version_is_accepted() {
        let mut value = serde_json::to_value(TemplateConfig::from_meta(&meta())).unwrap();
        value.as_object_mut().unwrap().remove("wscript_surface");
        let config: TemplateConfig = serde_json::from_value(value).unwrap();
        assert_eq!(config.wscript_surface, None);
        assert_eq!(config.into_meta(None).unwrap().wscript_surface, None);
    }

    /// The base's blob carries `variants_expected` as a JSON key only when
    /// set, and back; a variant's blob — sealed, effective metadata — never
    /// does. A blob without the key reads as an ordinary base.
    #[test]
    fn variants_expected_rides_the_base_blob() {
        let mut m = meta();
        let plain = TemplateConfig::from_meta(&m).to_json().unwrap();
        assert!(!String::from_utf8_lossy(&plain).contains("variants_expected"));
        m.variants_expected = true;
        let json = TemplateConfig::from_meta(&m).to_json().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["variants_expected"], serde_json::Value::Bool(true));
        let back = TemplateConfig::from_json(&json)
            .unwrap()
            .into_meta(None)
            .unwrap();
        assert_eq!(back, m);
        let v = VariantMeta {
            name: "standard".into(),
            default: true,
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
            display: None,
            cpu_model: None,
            accel: None,
        };
        assert!(!TemplateConfig::from_variant(&m, &v).variants_expected);
    }

    /// A variant's config blob carries its effective metadata; turned back
    /// into a block against the same base, it resolves to the same effective
    /// metadata, and keeps the variant's identity.
    #[test]
    fn variant_config_round_trip() {
        let base = meta();
        let v = VariantMeta {
            name: "win3d".into(),
            default: false,
            created: "2026-07-01T00:00:00Z".parse().unwrap(),
            sha256: "cd".repeat(32),
            base_sha256: "ab".repeat(32),
            description: Some("3D".into()),
            first_boot_script: Some("variant script".into()),
            agent_version: None,
            wscript_surface: None,
            profile: None,
            cpus: Some(4), // the base's own value: no override
            memory: Some(16 << 30),
            firmware: None,
            tpm: None,
            secure_boot: Some(false),
            display: None,
            cpu_model: None,
            accel: None,
        };
        let config = TemplateConfig::from_variant(&base, &v);
        assert_eq!(config.sha256.as_deref(), Some(v.sha256.as_str()));
        assert_eq!(config.memory, Some(16 << 30));
        assert_eq!(config.secure_boot, Some(false));
        let json = config.to_json().unwrap();
        let back = TemplateConfig::from_json(&json)
            .unwrap()
            .into_variant(&base)
            .unwrap();
        assert_eq!(base.effective(Some(&back)), base.effective(Some(&v)));
        assert_eq!(back.name, "win3d");
        assert_eq!(back.base_sha256, v.base_sha256);
        assert_eq!(back.description.as_deref(), Some("3D"));
        assert_eq!(back.cpus, None, "equal to the base's, so inherited");
        assert_eq!(back.memory, Some(16 << 30));

        // A base's blob is no variant's.
        assert!(
            TemplateConfig::from_meta(&base)
                .into_variant(&base)
                .is_err()
        );
        // Read as a client that predates variants reads it, it is a template.
        let mut value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        value.as_object_mut().unwrap().remove("variant");
        assert!(serde_json::from_value::<TemplateConfig>(value).is_ok());
    }
}
