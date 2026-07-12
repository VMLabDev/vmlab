//! Host-level OCI catalog settings shared by the CLI and web console.
//!
//! Registry namespaces are non-secret search roots. Credentials remain in
//! Docker's standard config/credential helpers and are only referenced by
//! registry host.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "registries.json";
const DOCKER_HUB: &str = "registry-1.docker.io";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum RegistryUse {
    Vms,
    Containers,
    Both,
}

impl RegistryUse {
    pub fn vms(self) -> bool {
        matches!(self, Self::Vms | Self::Both)
    }

    pub fn containers(self) -> bool {
        matches!(self, Self::Containers | Self::Both)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub namespace: String,
    pub use_for: RegistryUse,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    entries: Vec<RegistryEntry>,
    #[serde(default)]
    removed: Vec<String>,
}

pub fn defaults() -> Vec<RegistryEntry> {
    vec![
        RegistryEntry {
            namespace: "ghcr.io/vmlabdev/vmlab-templates".into(),
            use_for: RegistryUse::Vms,
        },
        RegistryEntry {
            namespace: "ghcr.io/vmlabdev".into(),
            use_for: RegistryUse::Containers,
        },
        RegistryEntry {
            namespace: format!("{DOCKER_HUB}/library"),
            use_for: RegistryUse::Containers,
        },
    ]
}

pub fn settings_path() -> PathBuf {
    crate::paths::config_dir().join(FILE_NAME)
}

pub fn normalise_namespace(value: &str) -> Result<String> {
    let trimmed = value
        .trim()
        .strip_prefix("https://")
        .or_else(|| value.trim().strip_prefix("http://"))
        .unwrap_or(value.trim())
        .trim_end_matches('/');
    let (host, path) = trimmed
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("registry namespace needs an owner, e.g. ghcr.io/owner"))?;
    if path.is_empty()
        || !(host == "localhost" || host.contains('.') || host.contains(':'))
        || host.chars().any(char::is_whitespace)
        || path.chars().any(char::is_whitespace)
    {
        bail!("invalid OCI registry namespace `{value}`");
    }
    let host = match host.to_ascii_lowercase().as_str() {
        "docker.io" | "index.docker.io" => DOCKER_HUB,
        _ => host,
    };
    Ok(format!("{host}/{path}"))
}

pub fn host_of(namespace: &str) -> Result<&str> {
    namespace
        .split_once('/')
        .map(|(host, _)| host)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| anyhow::anyhow!("invalid registry namespace `{namespace}`"))
}

fn read_at(path: &Path) -> Result<SettingsFile> {
    if !path.is_file() {
        return Ok(SettingsFile::default());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read registry settings {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("cannot parse registry settings {}", path.display()))
}

fn write_at(path: &Path, settings: &SettingsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(settings).context("serialising registry settings")?;
    std::fs::write(&temp, format!("{text}\n"))
        .with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

fn list_at(path: &Path) -> Result<Vec<RegistryEntry>> {
    let settings = read_at(path)?;
    let removed: std::collections::HashSet<String> = settings.removed.into_iter().collect();
    let mut entries = std::collections::BTreeMap::new();
    for entry in defaults().into_iter().chain(settings.entries) {
        let namespace = normalise_namespace(&entry.namespace)?;
        if !removed.contains(&namespace) {
            entries.insert(namespace.clone(), RegistryEntry { namespace, ..entry });
        }
    }
    Ok(entries.into_values().collect())
}

pub fn list() -> Result<Vec<RegistryEntry>> {
    list_at(&settings_path())
}

pub fn removed() -> Result<Vec<String>> {
    Ok(read_at(&settings_path())?.removed)
}

fn add_at(path: &Path, namespace: &str, use_for: RegistryUse) -> Result<RegistryEntry> {
    let namespace = normalise_namespace(namespace)?;
    let mut settings = read_at(path)?;
    settings
        .entries
        .retain(|entry| match normalise_namespace(&entry.namespace) {
            Ok(existing) => existing != namespace,
            Err(_) => true,
        });
    settings.removed.retain(|removed| removed != &namespace);
    let entry = RegistryEntry { namespace, use_for };
    settings.entries.push(entry.clone());
    write_at(path, &settings)?;
    Ok(entry)
}

pub fn add(namespace: &str, use_for: RegistryUse) -> Result<RegistryEntry> {
    add_at(&settings_path(), namespace, use_for)
}

fn remove_at(path: &Path, namespace: &str) -> Result<()> {
    let namespace = normalise_namespace(namespace)?;
    let mut settings = read_at(path)?;
    settings
        .entries
        .retain(|entry| match normalise_namespace(&entry.namespace) {
            Ok(existing) => existing != namespace,
            Err(_) => true,
        });
    if !settings.removed.contains(&namespace) {
        settings.removed.push(namespace);
    }
    write_at(path, &settings)
}

pub fn remove(namespace: &str) -> Result<()> {
    remove_at(&settings_path(), namespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_and_defaults_share_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(list_at(&path).unwrap().len(), defaults().len());

        add_at(
            &path,
            "https://registry.example.com/team/",
            RegistryUse::Both,
        )
        .unwrap();
        let entries = list_at(&path).unwrap();
        assert!(entries.iter().any(|entry| {
            entry.namespace == "registry.example.com/team" && entry.use_for == RegistryUse::Both
        }));

        remove_at(&path, "ghcr.io/vmlabdev/vmlab-templates").unwrap();
        assert!(
            !list_at(&path)
                .unwrap()
                .iter()
                .any(|entry| entry.namespace == "ghcr.io/vmlabdev/vmlab-templates")
        );
    }

    #[test]
    fn docker_aliases_normalise() {
        assert_eq!(
            normalise_namespace("docker.io/library").unwrap(),
            "registry-1.docker.io/library"
        );
        assert!(normalise_namespace("ghcr.io").is_err());
    }
}
