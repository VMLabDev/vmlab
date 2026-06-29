//! Shared server state: the proto clients we keep open to the supervisor and
//! each lab daemon, plus auth config and live sessions.
//!
//! Clients are obtained through the same auto-start path the CLI uses
//! (`vmlab::cli::daemon`), cached, and re-established on a dropped connection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Mutex;

use vmlab::cli::daemon;
use vmlab::proto::ProtoError;
use vmlab::proto::client::Client;

/// Sessions older than this with no activity are dropped.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

pub struct AuthConfig {
    pub enabled: bool,
    pub user: String,
    /// argon2 PHC hash string; empty when auth is disabled.
    pub password_hash: String,
}

pub struct AppState {
    pub auth: AuthConfig,
    /// token → last-seen instant.
    sessions: Mutex<HashMap<String, Instant>>,
    /// The lab discovered from the server's working directory at startup.
    pub default_lab: Option<(String, PathBuf)>,
    /// lab name → root directory (seeded from the cwd lab and the supervisor
    /// registry).
    roots: Mutex<HashMap<String, PathBuf>>,
    supervisor: Mutex<Option<Client>>,
    labs: Mutex<HashMap<String, Client>>,
}

impl AppState {
    pub fn new(auth: AuthConfig, default_lab: Option<(String, PathBuf)>) -> Self {
        let mut roots = HashMap::new();
        if let Some((name, root)) = &default_lab {
            roots.insert(name.clone(), root.clone());
        }
        Self {
            auth,
            sessions: Mutex::new(HashMap::new()),
            default_lab,
            roots: Mutex::new(roots),
            supervisor: Mutex::new(None),
            labs: Mutex::new(HashMap::new()),
        }
    }

    // --- sessions ---------------------------------------------------------

    pub async fn create_session(&self, token: String) {
        let mut s = self.sessions.lock().await;
        prune(&mut s);
        s.insert(token, Instant::now());
    }

    pub async fn valid_session(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().await;
        prune(&mut s);
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

    /// Resolve a lab's root: the cwd lab, a cached entry, or the supervisor
    /// registry.
    async fn root_for(&self, lab: &str) -> Result<PathBuf, String> {
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
            .map(PathBuf::from)
            .ok_or_else(|| format!("unknown lab `{lab}`"))?;
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

    /// Lab names to subscribe to / list: the cwd lab plus every registry entry.
    pub async fn lab_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        if let Some((name, _)) = &self.default_lab {
            names.push(name.clone());
        }
        if let Ok(labs) = self.supervisor_call("status", Value::Null).await {
            for l in labs.as_array().into_iter().flatten() {
                if let Some(n) = l["name"].as_str()
                    && !names.iter().any(|x| x == n)
                {
                    names.push(n.to_string());
                }
            }
        }
        names
    }

    /// A live lab client for the events/VNC bridges (public wrapper).
    pub async fn lab_client_pub(&self, lab: &str) -> Result<Client, String> {
        self.lab_client(lab).await
    }
}

fn prune(sessions: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    sessions.retain(|_, seen| now.duration_since(*seen) < SESSION_TTL);
}

fn proto_err(e: ProtoError) -> String {
    e.to_string()
}
