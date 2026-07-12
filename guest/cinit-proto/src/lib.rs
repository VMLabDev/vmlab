//! The wire contract between the vmlab host and `vmlab-cinit`, the PID 1 of
//! an OCI-container micro-VM.
//!
//! One channel uses these types: **the ctl channel** (virtio-serial port
//! `vmlab.ctl.0`), newline-delimited JSON. cinit emits [`CtlEvent`]s
//! (starting with `boot`, repeated until the spec arrives), the host sends
//! [`CtlCommand`]s — the first being [`CtlCommand::Spec`], which carries the
//! [`ContainerSpec`]. The spec itself never touches a filesystem device;
//! volumes attach as snapshot-safe vhost-user-fs devices (or CIFS mounts on
//! hosts without virtiofsd) — never 9p, which would block snapshots
//! (PRD §18).
//!
//! Everything is plain serde so this crate builds for the host (a normal
//! dependency of the `vmlab` crate) and for the static-musl guest init.

use serde::{Deserialize, Serialize};

/// Version of this contract. cinit reports it in [`CtlEvent::Boot`]; the host
/// refuses to drive an init speaking a different major version.
///
/// v2: `tty_resize` command + the `vmlab.tty.0` interactive-shell port.
/// v3: the spec arrives over the ctl channel (`spec` command) instead of a
///     9p config share; volumes are CIFS mounts instead of 9p shares;
///     `resync` command for post-snapshot-restore state replay.
/// v4: volumes may arrive as virtiofs mounts (`tag` set) instead of CIFS —
///     the host spawns one virtiofsd per volume and attaches a
///     vhost-user-fs device; CIFS (`tag` absent + `smb`) remains the
///     fallback when the host has no virtiofsd.
/// v5: idle mode and the `idle` lifecycle event allow an exec-driven
///     container micro-VM to run without an OCI workload process.
pub const PROTO_VERSION: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    #[default]
    Workload,
    Idle,
}

/// A volume mounted into the container root, over virtiofs or CIFS.
///
/// With `tag` set the host has attached a vhost-user-fs device for this
/// volume and cinit mounts it natively (`mount -t virtiofs <tag>`), before
/// the network is even up. Without a `tag` the host serves the volume as an
/// SMB share at the segment gateway (the same mechanism as VM shared
/// folders) and cinit mounts `//<gateway>/<share>` at `target` after DHCP,
/// using the credentials in [`ContainerSpec::smb`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMount {
    /// SMB share name at the gateway, e.g. `vol-data`. Also doubles as the
    /// virtiofs tag when `tag` is set (they are minted from the same name).
    pub share: String,
    /// Absolute path inside the container rootfs.
    pub target: String,
    /// Mount read-only.
    #[serde(default)]
    pub read_only: bool,
    /// virtiofs mount tag of the vhost-user-fs device backing this volume.
    /// `None` = CIFS volume (see [`ContainerSpec::smb`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// How to reach the lab's SMB server for volume mounts. Present whenever the
/// spec declares volumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmbInfo {
    /// Segment gateway serving the shares (dotted IPv4).
    pub gateway: String,
    /// Lab SMB credential.
    pub username: String,
    pub password: String,
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
    /// Whether to start the OCI workload or remain available for exec only.
    #[serde(default)]
    pub mode: RuntimeMode,
    /// OCI entrypoint. In workload mode the process argv is
    /// `entrypoint ++ cmd` and at least one must be non-empty; idle mode
    /// ignores both fields.
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
    /// Volumes to mount inside the rootfs — virtiofs (tagged, pre-network)
    /// or CIFS (untagged, after DHCP). See [`VolumeMount`].
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,
    /// SMB server coordinates; required when any volume is untagged (CIFS).
    #[serde(default)]
    pub smb: Option<SmbInfo>,
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
    /// Rootfs, volumes and networking are prepared in exec-driven idle mode.
    Idle,
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
    /// The container spec. Sent once the host sees `boot`; cinit blocks its
    /// boot sequence until this arrives.
    Spec { spec: ContainerSpec },
    /// Graceful stop: send the spec's stop signal, escalate to SIGKILL after
    /// `grace_secs`.
    Stop { grace_secs: u64 },
    /// Resize the interactive shell's PTY (the `vmlab.tty.0` port). Applies
    /// to the current session and is remembered for future ones.
    TtyResize { cols: u16, rows: u16 },
    /// Re-emit current state as events (`boot`, then `net_up`, `started` or
    /// `idle`, and `health` as applicable). Sent after an online snapshot restore, where
    /// the resumed guest would otherwise never repeat its lifecycle events.
    Resync,
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
            mode: RuntimeMode::Workload,
            entrypoint: vec!["/docker-entrypoint.sh".into()],
            cmd: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            env: vec![("PATH".into(), "/usr/bin".into()), ("A".into(), "b".into())],
            workdir: Some("/srv".into()),
            user: Some("nginx:nginx".into()),
            stop_signal: Some("SIGQUIT".into()),
            stop_grace_secs: 5,
            volumes: vec![
                VolumeMount {
                    share: "vol-data".into(),
                    target: "/data".into(),
                    read_only: true,
                    tag: None,
                },
                VolumeMount {
                    share: "vol-fast".into(),
                    target: "/fast".into(),
                    read_only: false,
                    tag: Some("vol-fast".into()),
                },
            ],
            smb: Some(SmbInfo {
                gateway: "10.0.0.1".into(),
                username: "lab".into(),
                password: "s3cret".into(),
            }),
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
        assert_eq!(spec.mode, RuntimeMode::Workload);
        assert!(spec.entrypoint.is_empty());
        assert_eq!(spec.cmd, vec!["/bin/sh"]);
        assert_eq!(spec.stop_grace_secs, 10);
        assert_eq!(spec.nics, 1);
        assert!(spec.volumes.is_empty());
        assert!(spec.smb.is_none());
        assert!(spec.healthcheck.is_none());
        assert!(spec.user.is_none());
    }

    #[test]
    fn volume_mount_tag_is_optional_on_the_wire() {
        // A v3-era document (no `tag`) still parses: CIFS volume.
        let v: VolumeMount =
            serde_json::from_str(r#"{ "share": "vol-c-0", "target": "/data" }"#).unwrap();
        assert_eq!(v.tag, None);
        assert!(!v.read_only);
        // And an untagged volume serializes without the key at all.
        assert!(!serde_json::to_string(&v).unwrap().contains("tag"));
    }

    #[test]
    fn events_use_snake_case_tags() {
        let ev = CtlEvent::Boot {
            proto_version: PROTO_VERSION,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"event":"boot","proto_version":5}"#
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
        assert_eq!(
            serde_json::to_string(&CtlEvent::Idle).unwrap(),
            r#"{"event":"idle"}"#
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
            CtlEvent::Idle,
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

        let cmd = CtlCommand::TtyResize {
            cols: 132,
            rows: 43,
        };
        assert_eq!(
            serde_json::to_string(&cmd).unwrap(),
            r#"{"cmd":"tty_resize","cols":132,"rows":43}"#
        );
        assert_eq!(roundtrip(&cmd), cmd);

        let cmd = CtlCommand::Resync;
        assert_eq!(serde_json::to_string(&cmd).unwrap(), r#"{"cmd":"resync"}"#);
        assert_eq!(roundtrip(&cmd), cmd);

        let cmd = CtlCommand::Spec {
            spec: serde_json::from_str(r#"{ "hostname": "c1", "cmd": ["/bin/sh"] }"#).unwrap(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.starts_with(r#"{"cmd":"spec","spec":{"#));
        assert_eq!(roundtrip(&cmd), cmd);
    }
}
