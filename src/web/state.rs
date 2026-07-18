//! Shared server state: the proto clients we keep open to the supervisor and
//! each lab daemon, plus auth config and live sessions.
//!
//! Clients are obtained through the same auto-start path the CLI uses
//! (`vmlab::cli::daemon`), cached, and re-established on a dropped connection.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Mutex;

use vmlab::cli::daemon;
use vmlab::proto::ProtoError;
use vmlab::proto::client::Client;

/// Sessions older than this with no activity are dropped. Overridable via
/// `VMLAB_WEB_SESSION_TTL_SECS`.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// Consecutive login failures from one address before throttling kicks in.
const LOGIN_FAILURE_LIMIT: u32 = 5;
/// How long a throttled address must stay quiet before trying again.
const LOGIN_THROTTLE: Duration = Duration::from_secs(30);
/// Failure records idle longer than this are forgotten.
const LOGIN_FAILURE_TTL: Duration = Duration::from_secs(15 * 60);

/// Lab and VM names arrive in URL path segments and become filesystem paths
/// (control sockets, VNC sockets, screenshot files). Accept exactly what the
/// config layer accepts — a DNS label (§9.5) — so nothing can traverse.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// (lab, kind, machine, page) — the web-page proxy target cache key.
pub type WebTargetKey = (String, String, String, String);

pub struct AuthConfig {
    pub enabled: bool,
    pub user: String,
    /// argon2 PHC hash string; empty when auth is disabled.
    pub password_hash: String,
}

pub struct AppState {
    pub auth: AuthConfig,
    /// Trust the nearest reverse proxy's `X-Forwarded-For` when attributing
    /// client addresses (login backoff). Off by default: with no proxy in
    /// front, the header is attacker-controlled.
    pub trust_proxy: bool,
    /// token → last-seen instant.
    sessions: Mutex<HashMap<String, Instant>>,
    session_ttl: Duration,
    /// login-failure backoff: address → (consecutive failures, last attempt).
    login_failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    /// The lab discovered from the server's working directory at startup.
    pub default_lab: Option<(String, PathBuf)>,
    /// Directory holding the host-side config-weave binaries (resolved once
    /// from host config / env / XDG default). Tests point this at a stub.
    pub weave_bin_dir: PathBuf,
    /// Resolved web-page proxy targets, keyed by (lab, kind, machine, page).
    /// Populated lazily from `web.forward`; carries the per-page reqwest
    /// client + upstream auth session. Tests seed this directly.
    web_targets: Mutex<HashMap<WebTargetKey, std::sync::Arc<crate::webpages::WebTarget>>>,
    /// lab name → root directory (seeded from the cwd lab and the supervisor
    /// registry).
    roots: Mutex<HashMap<String, PathBuf>>,
    supervisor: Mutex<Option<Client>>,
    labs: Mutex<HashMap<String, Client>>,
}

impl AppState {
    pub fn new(
        auth: AuthConfig,
        default_lab: Option<(String, PathBuf)>,
        trust_proxy: bool,
    ) -> Self {
        let mut roots = HashMap::new();
        if let Some((name, root)) = &default_lab {
            roots.insert(name.clone(), root.clone());
        }
        let session_ttl = std::env::var("VMLAB_WEB_SESSION_TTL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&secs| secs > 0)
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_SESSION_TTL);
        let host_cfg = vmlab::config::host::HostConfig::load_default().unwrap_or_default();
        let weave_bin_dir =
            vmlab::weave_bin::default_bin_dir(host_cfg.config_weave_bin_dir.as_deref());
        Self {
            auth,
            trust_proxy,
            weave_bin_dir,
            sessions: Mutex::new(HashMap::new()),
            session_ttl,
            login_failures: Mutex::new(HashMap::new()),
            default_lab,
            roots: Mutex::new(roots),
            supervisor: Mutex::new(None),
            labs: Mutex::new(HashMap::new()),
            web_targets: Mutex::new(HashMap::new()),
        }
    }

    // --- web-page proxy targets -------------------------------------------

    /// Cached proxy target for a page, if resolved.
    pub async fn web_target(
        &self,
        key: &WebTargetKey,
    ) -> Option<std::sync::Arc<crate::webpages::WebTarget>> {
        self.web_targets.lock().await.get(key).cloned()
    }

    pub async fn set_web_target(
        &self,
        key: WebTargetKey,
        target: std::sync::Arc<crate::webpages::WebTarget>,
    ) {
        self.web_targets.lock().await.insert(key, target);
    }

    /// Drop a cached target (connect failure → force a fresh `web.forward`).
    pub async fn invalidate_web_target(&self, key: &(String, String, String, String)) {
        self.web_targets.lock().await.remove(key);
    }

    // --- sessions ---------------------------------------------------------

    pub async fn create_session(&self, token: String) {
        let mut s = self.sessions.lock().await;
        prune(&mut s, self.session_ttl);
        s.insert(token, Instant::now());
    }

    pub async fn valid_session(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().await;
        prune(&mut s, self.session_ttl);
        match s.get_mut(token) {
            Some(seen) => {
                *seen = Instant::now();
                true
            }
            None => false,
        }
    }

    pub async fn drop_session(&self, token: &str) {
        self.sessions.lock().await.remove(token);
    }

    // --- login backoff ------------------------------------------------------

    /// Is this address currently locked out of `/api/login`?
    pub async fn login_throttled(&self, addr: IpAddr) -> bool {
        let mut f = self.login_failures.lock().await;
        let now = Instant::now();
        f.retain(|_, (_, last)| now.duration_since(*last) < LOGIN_FAILURE_TTL);
        matches!(f.get(&addr),
            Some((count, last)) if *count >= LOGIN_FAILURE_LIMIT
                && now.duration_since(*last) < LOGIN_THROTTLE)
    }

    pub async fn login_failed(&self, addr: IpAddr) {
        let mut f = self.login_failures.lock().await;
        let entry = f.entry(addr).or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = Instant::now();
    }

    pub async fn login_succeeded(&self, addr: IpAddr) {
        self.login_failures.lock().await.remove(&addr);
    }

    // --- daemon clients ---------------------------------------------------

    /// A live supervisor client, auto-starting `vmlabd` if needed.
    pub async fn supervisor(&self) -> Result<Client, String> {
        let mut guard = self.supervisor.lock().await;
        if let Some(c) = guard.as_ref()
            && c.call("ping", Value::Null).await.is_ok()
        {
            return Ok(c.clone());
        }
        let client = daemon::ensure_supervisor()
            .await
            .map_err(|e| format!("{e:#}"))?;
        *guard = Some(client.clone());
        Ok(client)
    }

    /// A supervisor call, surfacing remote errors as plain strings.
    pub async fn supervisor_call(&self, cmd: &str, args: Value) -> Result<Value, String> {
        let client = self.supervisor().await?;
        client.call(cmd, args).await.map_err(proto_err)
    }

    /// Resolve a lab's root directory (public wrapper over `root_for`), used by
    /// the config read/write handlers.
    pub async fn lab_root(&self, lab: &str) -> Result<PathBuf, String> {
        self.root_for(lab).await
    }

    /// Drop the cached lab-daemon client so the next `lab_call` reconnects.
    /// Used after a daemon restart (config reload), where the old socket is
    /// gone and a fresh daemon now owns the lab.
    pub async fn drop_lab_client(&self, lab: &str) {
        self.labs.lock().await.remove(lab);
    }

    /// Make a freshly created lab resolvable before any daemon knows about
    /// it (the "New Lab" flow registers the directory it just wrote).
    pub async fn register_root(&self, lab: &str, root: PathBuf) {
        self.roots.lock().await.insert(lab.to_string(), root);
    }

    /// Every cached name → root entry (the cwd lab, registry hits, and labs
    /// created this session). `list_labs` merges these into its output so
    /// custom-path labs show up before their daemon ever runs.
    pub async fn known_roots(&self) -> Vec<(String, PathBuf)> {
        self.roots
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Resolve a lab's root: the cwd lab, a cached entry, or the supervisor
    /// registry. Every lab-addressed call funnels through here, so this is
    /// also where URL-supplied lab names are rejected before they can reach
    /// a socket path.
    async fn root_for(&self, lab: &str) -> Result<PathBuf, String> {
        if !valid_name(lab) {
            return Err(format!("invalid lab name `{lab}`"));
        }
        if let Some(p) = self.roots.lock().await.get(lab) {
            return Ok(p.clone());
        }
        let labs = self.supervisor_call("status", Value::Null).await?;
        let root = labs
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| l["name"].as_str() == Some(lab))
            .and_then(|l| l["root"].as_str())
            .map(PathBuf::from);
        // Labs created under the managed labs home aren't in the supervisor
        // registry until their daemon first runs; resolve them from disk
        // (`lab` is a validated DNS label, so it can't traverse).
        let root = match root {
            Some(r) => r,
            None => {
                let managed = vmlab::paths::labs_home().join(lab);
                if !managed.join(vmlab::paths::LAB_FILE).is_file() {
                    return Err(format!("unknown lab `{lab}`"));
                }
                managed
            }
        };
        self.roots
            .lock()
            .await
            .insert(lab.to_string(), root.clone());
        Ok(root)
    }

    /// A live client for a lab daemon, starting it (and the supervisor) if
    /// needed.
    async fn lab_client(&self, lab: &str) -> Result<Client, String> {
        if let Some(c) = self.labs.lock().await.get(lab) {
            return Ok(c.clone());
        }
        let root = self.root_for(lab).await?;
        let client = daemon::ensure_lab_daemon(lab, &root)
            .await
            .map_err(|e| format!("{e:#}"))?;
        self.labs
            .lock()
            .await
            .insert(lab.to_string(), client.clone());
        Ok(client)
    }

    /// A lab-daemon call with one reconnect on a dropped connection.
    pub async fn lab_call(&self, lab: &str, cmd: &str, args: Value) -> Result<Value, String> {
        let client = self.lab_client(lab).await?;
        match client.call(cmd, args.clone()).await {
            Ok(v) => Ok(v),
            Err(ProtoError::Closed) => {
                self.labs.lock().await.remove(lab);
                let client = self.lab_client(lab).await?;
                client.call(cmd, args).await.map_err(proto_err)
            }
            Err(e) => Err(proto_err(e)),
        }
    }
}

fn prune(sessions: &mut HashMap<String, Instant>, ttl: Duration) {
    let now = Instant::now();
    sessions.retain(|_, seen| now.duration_since(*seen) < ttl);
}

fn proto_err(e: ProtoError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_is_a_dns_label() {
        for good in ["web", "dc01", "my-lab", "a", "x2-y3"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a b",
            "-lead",
            "trail-",
            "a.b",
            "x\u{0}y",
            &"a".repeat(64),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn login_backoff_locks_after_repeated_failures() {
        let state = AppState::new(
            AuthConfig {
                enabled: true,
                user: "u".into(),
                password_hash: String::new(),
            },
            None,
            false,
        );
        let addr: IpAddr = "192.0.2.7".parse().unwrap();
        assert!(!state.login_throttled(addr).await);
        for _ in 0..LOGIN_FAILURE_LIMIT {
            state.login_failed(addr).await;
        }
        assert!(state.login_throttled(addr).await);
        // Another address is unaffected; success clears the record.
        assert!(!state.login_throttled("192.0.2.8".parse().unwrap()).await);
        state.login_succeeded(addr).await;
        assert!(!state.login_throttled(addr).await);
    }
}
