//! Serde model of the OCI / Docker **image config blob** — the JSON document
//! a manifest's `config` descriptor points at
//! (`application/vnd.oci.image.config.v1+json` or Docker's
//! `application/vnd.docker.container.image.v1+json`).
//!
//! Only the subset vmlab consumes is modelled: the platform fields (to
//! reject non-linux images), the runtime defaults the container runtime
//! seeds a container with, and `rootfs.diff_ids` (the uncompressed layer
//! digests the flatten step verifies). Docker emits explicit `null` for
//! absent runtime fields, so every field tolerates both a missing key and
//! JSON null.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer};

/// Deserialise a field treating JSON `null` as the type's default (plain
/// `#[serde(default)]` only covers a *missing* key, and docker configs are
/// full of `"Cmd": null`).
fn null_default<'de, D, T>(de: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

/// The image config blob.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageConfig {
    /// OCI architecture string (`amd64`, `arm64`, …) — see [`oci_arch`].
    #[serde(default, deserialize_with = "null_default")]
    pub architecture: String,
    /// Target OS; vmlab only runs `linux` images.
    #[serde(default, deserialize_with = "null_default")]
    pub os: String,
    /// The runtime defaults baked into the image.
    #[serde(default, deserialize_with = "null_default")]
    pub config: RuntimeDefaults,
    /// The layer DiffIDs (digests of the *uncompressed* layer tars).
    pub rootfs: RootFs,
}

/// The `config` object: what `docker run` would default to. JSON keys use
/// Docker's capitalised casing (`Env`, `Cmd`, `WorkingDir`, …).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuntimeDefaults {
    /// `NAME=value` environment entries.
    #[serde(default, rename = "Env", deserialize_with = "null_default")]
    pub env: Vec<String>,
    #[serde(default, rename = "Entrypoint", deserialize_with = "null_default")]
    pub entrypoint: Vec<String>,
    #[serde(default, rename = "Cmd", deserialize_with = "null_default")]
    pub cmd: Vec<String>,
    #[serde(default, rename = "WorkingDir", deserialize_with = "null_default")]
    pub working_dir: String,
    /// `user`, `user:group`, or numeric uid[:gid]; empty means root.
    #[serde(default, rename = "User", deserialize_with = "null_default")]
    pub user: String,
    /// Signal that stops the container (e.g. `SIGTERM`, the runtime default).
    #[serde(default, rename = "StopSignal")]
    pub stop_signal: Option<String>,
    /// Keys like `80/tcp`; the values are always empty objects.
    #[serde(default, rename = "ExposedPorts", deserialize_with = "null_default")]
    pub exposed_ports: BTreeMap<String, serde_json::Value>,
    /// Declared volume mount points, keyed by container path.
    #[serde(default, rename = "Volumes", deserialize_with = "null_default")]
    pub volumes: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "Healthcheck")]
    pub healthcheck: Option<ImageHealthcheck>,
}

/// A `HEALTHCHECK` baked into the image. Durations are in **nanoseconds**
/// (Docker serialises Go `time.Duration` values raw).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageHealthcheck {
    /// The probe: `["CMD", …]`, `["CMD-SHELL", …]`, or `["NONE"]`.
    #[serde(default, rename = "Test", deserialize_with = "null_default")]
    pub test: Vec<String>,
    #[serde(default, rename = "Interval")]
    pub interval: Option<i64>,
    #[serde(default, rename = "Timeout")]
    pub timeout: Option<i64>,
    #[serde(default, rename = "Retries")]
    pub retries: Option<u32>,
    #[serde(default, rename = "StartPeriod")]
    pub start_period: Option<i64>,
}

/// The `rootfs` object: the ordered uncompressed-layer digests.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RootFs {
    /// Always `layers` in practice.
    #[serde(default, rename = "type")]
    pub kind: String,
    /// `sha256:<hex>` of each layer's **decompressed** tar, lowest first —
    /// what [`super::flatten::flatten_to_squashfs`] verifies.
    #[serde(default, deserialize_with = "null_default")]
    pub diff_ids: Vec<String>,
}

/// Map a vmlab arch (QEMU naming) onto the OCI platform architecture string
/// used in image indexes and config blobs.
pub fn oci_arch(vmlab_arch: &str) -> Result<&'static str> {
    match vmlab_arch {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        "riscv64" => Ok("riscv64"),
        other => bail!(
            "no container-image architecture corresponds to vmlab arch `{other}` \
             (containers support x86_64, aarch64, riscv64)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_style_config_with_nulls() {
        // Shaped like real `docker inspect` output: capitalised keys,
        // explicit nulls, ports/volumes as maps of empty objects.
        let json = serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Env": ["PATH=/usr/bin", "LANG=C.UTF-8"],
                "Entrypoint": ["/docker-entrypoint.sh"],
                "Cmd": ["nginx", "-g", "daemon off;"],
                "WorkingDir": "",
                "User": null,
                "StopSignal": "SIGQUIT",
                "ExposedPorts": { "80/tcp": {} },
                "Volumes": { "/var/cache": {} },
                "Healthcheck": {
                    "Test": ["CMD-SHELL", "curl -f http://localhost/ || exit 1"],
                    "Interval": 30_000_000_000i64,
                    "Timeout": 3_000_000_000i64,
                    "Retries": 3,
                    "StartPeriod": 0
                },
                "Labels": { "ignored": "unknown keys are fine" }
            },
            "rootfs": {
                "type": "layers",
                "diff_ids": ["sha256:aa", "sha256:bb"]
            },
            "history": [{ "created_by": "ignored" }]
        });
        let cfg: ImageConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.architecture, "amd64");
        assert_eq!(cfg.os, "linux");
        assert_eq!(cfg.config.env.len(), 2);
        assert_eq!(cfg.config.entrypoint, vec!["/docker-entrypoint.sh"]);
        assert_eq!(cfg.config.cmd[0], "nginx");
        assert_eq!(cfg.config.user, "", "null User becomes empty");
        assert_eq!(cfg.config.stop_signal.as_deref(), Some("SIGQUIT"));
        assert!(cfg.config.exposed_ports.contains_key("80/tcp"));
        assert!(cfg.config.volumes.contains_key("/var/cache"));
        let hc = cfg.config.healthcheck.unwrap();
        assert_eq!(hc.test[0], "CMD-SHELL");
        assert_eq!(hc.interval, Some(30_000_000_000));
        assert_eq!(hc.retries, Some(3));
        assert_eq!(cfg.rootfs.kind, "layers");
        assert_eq!(cfg.rootfs.diff_ids, vec!["sha256:aa", "sha256:bb"]);
    }

    #[test]
    fn minimal_config_defaults() {
        // A spartan OCI config: no `config` object at all.
        let json = serde_json::json!({
            "architecture": "arm64",
            "os": "linux",
            "rootfs": { "type": "layers", "diff_ids": ["sha256:cc"] }
        });
        let cfg: ImageConfig = serde_json::from_value(json).unwrap();
        assert!(cfg.config.env.is_empty());
        assert!(cfg.config.cmd.is_empty());
        assert!(cfg.config.healthcheck.is_none());
        assert_eq!(cfg.rootfs.diff_ids.len(), 1);
    }

    #[test]
    fn missing_rootfs_is_an_error() {
        let json = serde_json::json!({ "architecture": "amd64", "os": "linux" });
        assert!(serde_json::from_value::<ImageConfig>(json).is_err());
    }

    #[test]
    fn arch_mapping() {
        assert_eq!(oci_arch("x86_64").unwrap(), "amd64");
        assert_eq!(oci_arch("aarch64").unwrap(), "arm64");
        assert_eq!(oci_arch("riscv64").unwrap(), "riscv64");
        let err = oci_arch("s390x").unwrap_err();
        assert!(err.to_string().contains("s390x"), "{err}");
    }
}
