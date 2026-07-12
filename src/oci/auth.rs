//! Registry authentication (PRD §6.4).
//!
//! vmlab reuses Docker-style credential configuration so a `ghcr.io` login
//! already on the machine just works:
//!
//! - `~/.docker/config.json` (or `$DOCKER_CONFIG/config.json`) — the
//!   `auths[registry].auth` field is base64(`user:pass`); `credHelpers`
//!   and `credsStore` name a `docker-credential-<helper>` binary invoked as
//!   a subprocess (`get`, registry host on stdin → JSON `{Username,
//!   Secret}`).
//! - The Bearer token flow: a registry replies `401` with a
//!   `Www-Authenticate: Bearer realm=…,service=…,scope=…` challenge; we GET
//!   the realm (with basic auth when we have a credential) to obtain a
//!   token, then retry the request with `Authorization: Bearer <token>`.
//!
//! Anonymous pulls of public images must work, so a missing credential is
//! never fatal here — it just yields `Credential::Anonymous`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

/// A resolved registry credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    Anonymous,
    Basic { username: String, password: String },
}

impl Credential {
    /// The `Authorization: Basic …` header value, if any.
    pub fn basic_header(&self) -> Option<String> {
        match self {
            Credential::Anonymous => None,
            Credential::Basic { username, password } => {
                let token = BASE64.encode(format!("{username}:{password}"));
                Some(format!("Basic {token}"))
            }
        }
    }
}

/// The parsed subset of a Docker `config.json` we care about.
#[derive(Debug, Default, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthEntry {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// Locate the Docker config file: `$DOCKER_CONFIG/config.json` if set, else
/// `~/.docker/config.json`.
pub fn docker_config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        return Some(PathBuf::from(dir).join("config.json"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".docker").join("config.json"))
}

/// Resolve a credential for `registry` (a bare host like `ghcr.io` or
/// `localhost:5000`) from the default Docker config location. A missing
/// file or absent entry is not an error — it yields [`Credential::Anonymous`]
/// so anonymous public pulls work.
pub fn resolve(registry: &str) -> Result<Credential> {
    match docker_config_path() {
        Some(path) if path.is_file() => resolve_from_file(registry, &path),
        _ => Ok(Credential::Anonymous),
    }
}

/// Resolve a credential for `registry` from a specific config file.
pub fn resolve_from_file(registry: &str, path: &std::path::Path) -> Result<Credential> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read docker config {}", path.display()))?;
    let config: DockerConfig = serde_json::from_str(&text)
        .with_context(|| format!("malformed docker config {}", path.display()))?;
    resolve_from_config(registry, &config)
}

fn resolve_from_config(registry: &str, config: &DockerConfig) -> Result<Credential> {
    let candidates = candidate_hosts(registry);
    // 1. A per-registry credential helper wins.
    for host in &candidates {
        if let Some(helper) = config.cred_helpers.get(*host) {
            return run_cred_helper(helper, host);
        }
    }
    // 2. A direct `auths` entry: base64 `auth`, or explicit user/pass.
    for host in &candidates {
        if let Some(entry) = lookup_auth(&config.auths, host) {
            if let Some(auth) = &entry.auth {
                return decode_basic_auth(auth);
            }
            if let (Some(u), Some(p)) = (&entry.username, &entry.password) {
                return Ok(Credential::Basic {
                    username: u.clone(),
                    password: p.clone(),
                });
            }
        }
    }
    // 3. A global credsStore helper.
    if let Some(helper) = &config.creds_store {
        // The store may or may not have this registry; missing creds are
        // not fatal (anonymous pull stays possible).
        for host in &candidates {
            if let Ok(cred) = run_cred_helper(helper, host) {
                return Ok(cred);
            }
        }
        return Ok(Credential::Anonymous);
    }
    Ok(Credential::Anonymous)
}

/// The docker-config keys to try for `registry`. Docker Hub's registry
/// answers as `registry-1.docker.io`, but docker tooling keys its credential
/// as `docker.io` or the legacy `https://index.docker.io/v1/` — try those
/// aliases too so an existing `docker login` just works.
fn candidate_hosts(registry: &str) -> Vec<&str> {
    if registry == "registry-1.docker.io" {
        vec![
            registry,
            "docker.io",
            "index.docker.io",
            "https://index.docker.io/v1/",
        ]
    } else {
        vec![registry]
    }
}

/// Look up an `auths` entry tolerant of Docker's habit of keying on full
/// URLs (`https://ghcr.io/v2/`, `https://index.docker.io/v1/`) for a bare
/// host registry.
fn lookup_auth<'a>(auths: &'a HashMap<String, AuthEntry>, registry: &str) -> Option<&'a AuthEntry> {
    if let Some(e) = auths.get(registry) {
        return Some(e);
    }
    auths.iter().find_map(|(k, v)| {
        let host = k
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(k);
        (host == registry).then_some(v)
    })
}

/// Decode a base64 `user:pass` string into a basic credential.
fn decode_basic_auth(auth: &str) -> Result<Credential> {
    let decoded = BASE64
        .decode(auth.trim())
        .context("docker config `auth` field is not valid base64")?;
    let s = String::from_utf8(decoded).context("docker config `auth` is not UTF-8")?;
    let (user, pass) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("docker config `auth` is not `user:pass`"))?;
    Ok(Credential::Basic {
        username: user.to_string(),
        password: pass.to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct CredHelperReply {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

/// Invoke `docker-credential-<helper> get` with the registry host on stdin.
fn run_cred_helper(helper: &str, registry: &str) -> Result<Credential> {
    use std::io::Write as _;
    let bin = format!("docker-credential-{helper}");
    let mut child = Command::new(&bin)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run credential helper `{bin}`"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("cannot pipe to `{bin}`"))?
        .write_all(registry.as_bytes())
        .with_context(|| format!("cannot write to `{bin}`"))?;
    let output = child
        .wait_with_output()
        .with_context(|| format!("credential helper `{bin}` failed"))?;
    if !output.status.success() {
        bail!(
            "credential helper `{bin}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let reply: CredHelperReply = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("credential helper `{bin}` returned invalid JSON"))?;
    // Identity-token style replies use Username "<token>"; we only support
    // basic here, which covers GHCR PATs and Docker Hub.
    Ok(Credential::Basic {
        username: reply.username,
        password: reply.secret,
    })
}

/// A parsed `Www-Authenticate: Bearer …` challenge.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BearerChallenge {
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

/// Parse a `Www-Authenticate` header value. Returns `None` if it is not a
/// Bearer challenge with a realm.
pub fn parse_bearer_challenge(header: &str) -> Option<BearerChallenge> {
    let rest = header.trim();
    let rest = rest
        .strip_prefix("Bearer ")
        .or_else(|| rest.strip_prefix("bearer "))?;
    let mut challenge = BearerChallenge::default();
    for part in split_params(rest) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        match key {
            "realm" => challenge.realm = value.to_string(),
            "service" => challenge.service = Some(value.to_string()),
            "scope" => challenge.scope = Some(value.to_string()),
            _ => {}
        }
    }
    if challenge.realm.is_empty() {
        return None;
    }
    Some(challenge)
}

/// Split `realm="…",service="…",scope="…"` on commas that are not inside
/// quotes.
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Persist a credential for `registry` into the Docker config at the
/// default location (creating the file/dir as needed). This is the storage
/// half of `vmlab template login`; the caller validates the credential
/// against the registry first.
pub fn store_login(registry: &str, username: &str, password: &str) -> Result<PathBuf> {
    let path =
        docker_config_path().ok_or_else(|| anyhow!("cannot determine docker config path"))?;
    store_login_at(&path, registry, username, password)?;
    Ok(path)
}

/// Persist a credential into a specific config file (the testable core of
/// [`store_login`]).
pub fn store_login_at(
    path: &std::path::Path,
    registry: &str,
    username: &str,
    password: &str,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let mut root: serde_json::Value = if path.is_file() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let auth = BASE64.encode(format!("{username}:{password}"));
    root["auths"][registry] = serde_json::json!({ "auth": auth });
    let text = serde_json::to_string_pretty(&root).context("cannot serialise docker config")?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, format!("{text}\n"))
        .with_context(|| format!("cannot write {}", temp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode())
            .unwrap_or(0o600);
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("cannot protect {}", temp.display()))?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_base64_auth_and_builds_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // base64("alice:s3cr3t")
        let auth = BASE64.encode("alice:s3cr3t");
        let json = serde_json::json!({ "auths": { "ghcr.io": { "auth": auth } } });
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let cred = resolve_from_file("ghcr.io", &path).unwrap();
        assert_eq!(
            cred,
            Credential::Basic {
                username: "alice".into(),
                password: "s3cr3t".into()
            }
        );
        assert_eq!(
            cred.basic_header().unwrap(),
            format!("Basic {}", BASE64.encode("alice:s3cr3t"))
        );
    }

    #[test]
    fn explicit_username_password_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let json = serde_json::json!({
            "auths": { "reg.example.com": { "username": "bob", "password": "pw" } }
        });
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let cred = resolve_from_file("reg.example.com", &path).unwrap();
        assert_eq!(
            cred,
            Credential::Basic {
                username: "bob".into(),
                password: "pw".into()
            }
        );
    }

    #[test]
    fn full_url_key_matches_bare_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let auth = BASE64.encode("u:p");
        let json = serde_json::json!({ "auths": { "https://ghcr.io/v2/": { "auth": auth } } });
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let cred = resolve_from_file("ghcr.io", &path).unwrap();
        assert!(matches!(cred, Credential::Basic { .. }));
    }

    #[test]
    fn docker_hub_registry_tries_alias_keys() {
        // `docker login` keys Docker Hub as `docker.io` or the legacy
        // `https://index.docker.io/v1/`; both must satisfy a lookup for the
        // actual registry host `registry-1.docker.io`.
        for key in ["docker.io", "https://index.docker.io/v1/"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.json");
            let auth = BASE64.encode("hubuser:hubpass");
            let json = serde_json::json!({ "auths": { key: { "auth": auth } } });
            std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
            let cred = resolve_from_file("registry-1.docker.io", &path).unwrap();
            assert_eq!(
                cred,
                Credential::Basic {
                    username: "hubuser".into(),
                    password: "hubpass".into()
                },
                "key {key} should resolve for registry-1.docker.io"
            );
            // …and the alias never leaks onto other registries.
            assert_eq!(
                resolve_from_file("ghcr.io", &path).unwrap(),
                Credential::Anonymous
            );
        }
    }

    #[test]
    fn missing_entry_is_anonymous() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(
            resolve_from_file("ghcr.io", &path).unwrap(),
            Credential::Anonymous
        );
    }

    #[test]
    fn bearer_challenge_parsing() {
        let h = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:owner/name:pull""#;
        let c = parse_bearer_challenge(h).unwrap();
        assert_eq!(c.realm, "https://ghcr.io/token");
        assert_eq!(c.service.as_deref(), Some("ghcr.io"));
        assert_eq!(c.scope.as_deref(), Some("repository:owner/name:pull"));
    }

    #[test]
    fn non_bearer_challenge_is_none() {
        assert!(parse_bearer_challenge("Basic realm=\"x\"").is_none());
        assert!(parse_bearer_challenge("Bearer service=\"x\"").is_none()); // no realm
    }

    #[test]
    fn store_login_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        store_login_at(&path, "ghcr.io", "carol", "tok").unwrap();
        assert!(path.is_file());
        let cred = resolve_from_file("ghcr.io", &path).unwrap();
        assert_eq!(
            cred,
            Credential::Basic {
                username: "carol".into(),
                password: "tok".into()
            }
        );
        // merging a second registry preserves the first
        store_login_at(&path, "reg.example.com", "dave", "pw").unwrap();
        assert!(matches!(
            resolve_from_file("ghcr.io", &path).unwrap(),
            Credential::Basic { .. }
        ));
        assert!(matches!(
            resolve_from_file("reg.example.com", &path).unwrap(),
            Credential::Basic { .. }
        ));
    }
}
