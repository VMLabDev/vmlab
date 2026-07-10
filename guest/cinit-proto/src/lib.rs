//! The wire contract between the vmlab host and `vmlab-cinit`, the PID 1 of
//! an OCI-container micro-VM.
//!
//! Two channels use these types:
//!
//! - **`container.json`** on the read-only 9p config share (`vmlab.cfg`):
//!   one [`ContainerSpec`] document, written by the host, read by cinit at
//!   boot.
//! - **The ctl channel** (virtio-serial port `vmlab.ctl.0`): newline-delimited
//!   JSON. cinit emits [`CtlEvent`]s (starting with `boot`), the host sends
//!   [`CtlCommand`]s.
//!
//! Everything is plain serde so this crate builds for the host (a normal
//! dependency of the `vmlab` crate) and for the static-musl guest init.

use serde::{Deserialize, Serialize};

/// Version of this contract. cinit reports it in [`CtlEvent::Boot`]; the host
/// refuses to drive an init speaking a different major version.
pub const PROTO_VERSION: u32 = 1;

/// A 9p volume mounted into the container root.
///
/// The host exports each volume as a 9p share tagged `vol0..volN` (the `tag`
/// field carries the exact tag) and cinit mounts it at `target` inside the
/// container rootfs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMount {
    /// virtio-9p mount tag, e.g. `vol0`.
    pub tag: String,
    /// Absolute path inside the container rootfs.
    pub target: String,
    /// Mount read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// Container healthcheck, mirroring the OCI image / compose semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSpec {
    /// Argv executed inside the container root; exit 0 = healthy.
    pub command: Vec<String>,
    /// Seconds between checks.
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    /// Seconds one check may run before it is killed and counted as failed.
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
    /// Consecutive failures before the container is reported unhealthy.
    #[serde(default = "default_health_retries")]
    pub retries: u32,
    /// Grace period after start before checks begin.
    #[serde(default)]
    pub start_period_secs: u64,
}

fn default_health_interval() -> u64 {
    30
}
fn default_health_timeout() -> u64 {
    30
}
fn default_health_retries() -> u32 {
    3
}

/// The `container.json` document: everything cinit needs to assemble and run
/// one container. The host pre-merges image config with lab overrides (env,
/// entrypoint/cmd), so cinit applies these fields verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSpec {
    /// Guest hostname (also written to /etc/hostname and /etc/hosts).
    pub hostname: String,
    /// OCI entrypoint. The process argv is `entrypoint ++ cmd`; at least one
    /// of the two must be non-empty.
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// OCI cmd (arguments appended to the entrypoint).
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Environment, already merged by the host (image env + lab overrides).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory inside the container root (default `/`).
    #[serde(default)]
    pub workdir: Option<String>,
    /// `name[:group]` or `uid[:gid]`, resolved against the container's
    /// /etc/passwd + /etc/group; absent = root.
    #[serde(default)]
    pub user: Option<String>,
    /// Signal sent on stop, e.g. `"SIGTERM"` (the default).
    #[serde(default)]
    pub stop_signal: Option<String>,
    /// Seconds between the stop signal and SIGKILL.
    #[serde(default = "default_stop_grace")]
    pub stop_grace_secs: u64,
    /// 9p volumes to mount inside the rootfs.
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// Number of virtio NICs (`eth0..`) to bring up via DHCP. 0 = loopback
    /// only.
    #[serde(default = "default_nics")]
    pub nics: u32,
    /// Optional healthcheck.
    #[serde(default)]
    pub healthcheck: Option<HealthSpec>,
}

fn default_stop_grace() -> u64 {
    10
}
fn default_nics() -> u32 {
    1
}

/// Events cinit emits on the ctl channel, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CtlEvent {
    /// First line after the channel opens.
    Boot { proto_version: u32 },
    /// eth0 obtained a DHCP lease.
    NetUp { ip: String },
    /// The container process is running.
    Started { pid: u32 },
    /// Healthcheck state changed (first pass, or `retries` consecutive
    /// failures, or recovery after failures).
    Health { healthy: bool },
    /// The container process exited; cinit powers the VM off right after.
    /// `code` is the exit code, or `128 + signal` if it died from a signal.
    Exited { code: i32 },
}

/// Commands the host sends to cinit, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum CtlCommand {
    /// Graceful stop: send the spec's stop signal, escalate to SIGKILL after
    /// `grace_secs`.
    Stop { grace_secs: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(v: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::from_str(&serde_json::to_string(v).unwrap()).unwrap()
    }

    #[test]
    fn container_spec_roundtrips() {
        let spec = ContainerSpec {
            hostname: "web1".into(),
            entrypoint: vec!["/docker-entrypoint.sh".into()],
            cmd: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            env: vec![("PATH".into(), "/usr/bin".into()), ("A".into(), "b".into())],
            workdir: Some("/srv".into()),
            user: Some("nginx:nginx".into()),
            stop_signal: Some("SIGQUIT".into()),
            stop_grace_secs: 5,
            volumes: vec![VolumeMount {
                tag: "vol0".into(),
                target: "/data".into(),
                read_only: true,
            }],
            nics: 2,
            healthcheck: Some(HealthSpec {
                command: vec!["/bin/true".into()],
                interval_secs: 10,
                timeout_secs: 3,
                retries: 2,
                start_period_secs: 1,
            }),
        };
        assert_eq!(roundtrip(&spec), spec);
    }

    #[test]
    fn container_spec_minimal_defaults() {
        // The host may emit a minimal document; everything else defaults.
        let spec: ContainerSpec =
            serde_json::from_str(r#"{ "hostname": "c1", "cmd": ["/bin/sh"] }"#).unwrap();
        assert_eq!(spec.hostname, "c1");
        assert!(spec.entrypoint.is_empty());
        assert_eq!(spec.cmd, vec!["/bin/sh"]);
        assert_eq!(spec.stop_grace_secs, 10);
        assert_eq!(spec.nics, 1);
        assert!(spec.volumes.is_empty());
        assert!(spec.healthcheck.is_none());
        assert!(spec.user.is_none());
    }

    #[test]
    fn events_use_snake_case_tags() {
        let ev = CtlEvent::Boot {
            proto_version: PROTO_VERSION,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"boot","proto_version":1}"#
        );
        let ev = CtlEvent::NetUp {
            ip: "10.0.0.9".into(),
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"net_up","ip":"10.0.0.9"}"#
        );
        let ev = CtlEvent::Exited { code: 137 };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"exited","code":137}"#
        );
    }

    #[test]
    fn events_roundtrip() {
        for ev in [
            CtlEvent::Boot { proto_version: 1 },
            CtlEvent::NetUp {
                ip: "1.2.3.4".into(),
            },
            CtlEvent::Started { pid: 42 },
            CtlEvent::Health { healthy: false },
            CtlEvent::Exited { code: -1 },
        ] {
            assert_eq!(roundtrip(&ev), ev);
        }
    }

    #[test]
    fn commands_roundtrip() {
        let cmd = CtlCommand::Stop { grace_secs: 7 };
        assert_eq!(
            serde_json::to_string(&cmd).unwrap(),
            r#"{"cmd":"stop","grace_secs":7}"#
        );
        assert_eq!(roundtrip(&cmd), cmd);
    }
}
