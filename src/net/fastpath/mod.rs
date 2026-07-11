//! Network fast-path tier selection (PRD §9.1: the netdev attachment is
//! designed so a faster backend can be substituted without changing lab
//! semantics).
//!
//! Two kernel-accelerated tiers sit above the always-available userspace
//! switch:
//!
//! - **afxdp** — tap netdevs with a per-segment XDP program forwarding known
//!   unicast in-kernel (needs CAP_NET_ADMIN + CAP_BPF).
//! - **sockmap** — sk_skb programs splicing the existing QEMU stream sockets
//!   in-kernel (needs CAP_BPF + CAP_NET_ADMIN, kernel ≥ 5.15).
//!
//! Selection is empirical: each daemon probes the real mechanism once at
//! startup and degrades to the userspace switch on any failure, so the PRD's
//! rootless / no-CAP_NET_ADMIN guarantee (§1.1, §13, §14) holds on hosts
//! without privileges — those simply keep today's path.

#[cfg(feature = "ebpf")]
mod afxdp;
#[cfg(all(test, feature = "ebpf"))]
mod priv_tests;
#[cfg(feature = "ebpf")]
mod probe;
#[cfg(feature = "ebpf")]
mod sockmap;
#[cfg(feature = "ebpf")]
pub use afxdp::{SegmentXdp, TapNic};
#[cfg(feature = "ebpf")]
pub use sockmap::SegmentOffload;
#[cfg(not(feature = "ebpf"))]
mod stub;
#[cfg(not(feature = "ebpf"))]
pub use stub::{SegmentOffload, SegmentXdp, TapNic};

use std::sync::OnceLock;

/// How one VM NIC attaches to its segment.
pub enum NicAttachment {
    /// A unix listener socket QEMU connects to (the default path).
    Stream { sock: std::path::PathBuf },
    /// A pre-opened tap device (afxdp tier). RAII: dropping detaches the
    /// switch port and XDP state and releases the daemon's queue fd.
    Tap(TapNic),
}

/// Which fast path this daemon uses for eligible switch traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastpathTier {
    /// tap netdevs + in-kernel XDP forwarding.
    AfXdp,
    /// sk_skb socket splicing on the stream-socket ports.
    Sockmap,
    /// The plain userspace switch (always available).
    Userspace,
}

impl FastpathTier {
    pub fn as_str(self) -> &'static str {
        match self {
            FastpathTier::AfXdp => "afxdp",
            FastpathTier::Sockmap => "sockmap",
            FastpathTier::Userspace => "userspace",
        }
    }
}

/// The `fastpath` host-config knob; `VMLAB_FASTPATH` overrides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FastpathMode {
    /// Probe afxdp, else fall back to userspace. Never picks sockmap: it
    /// works but measures slower than the userspace fabric (see `select`).
    #[default]
    Auto,
    /// Never use a kernel fast path.
    Off,
    /// Probe only the sockmap tier (evaluation only — measured slower
    /// than the userspace fabric on current kernels).
    Sockmap,
    /// Probe only the afxdp tier.
    AfXdp,
}

impl FastpathMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            "sockmap" => Some(Self::Sockmap),
            "afxdp" => Some(Self::AfXdp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            FastpathMode::Auto => "auto",
            FastpathMode::Off => "off",
            FastpathMode::Sockmap => "sockmap",
            FastpathMode::AfXdp => "afxdp",
        }
    }
}

/// The outcome of tier selection, kept for the CLI/web surface.
#[derive(Debug, Clone)]
pub struct FastpathStatus {
    pub tier: FastpathTier,
    pub mode: FastpathMode,
    /// Why each skipped kernel tier is unavailable, keyed by tier name.
    pub reasons: Vec<(&'static str, String)>,
}

impl Default for FastpathStatus {
    fn default() -> Self {
        Self {
            tier: FastpathTier::Userspace,
            mode: FastpathMode::Auto,
            reasons: Vec::new(),
        }
    }
}

static SELECTION: OnceLock<FastpathStatus> = OnceLock::new();

/// Probe and select the fast-path tier, once per daemon process. Called at
/// supervisor and lab-daemon startup before any switch is built; idempotent
/// (the first call wins).
pub fn init(mode: FastpathMode) -> FastpathTier {
    SELECTION
        .get_or_init(|| {
            let status = select(env_mode(mode));
            tracing::info!(
                tier = status.tier.as_str(),
                mode = status.mode.as_str(),
                "network fast path selected"
            );
            for (tier, reason) in &status.reasons {
                tracing::info!("fast-path tier {tier} unavailable: {reason}");
            }
            status
        })
        .tier
}

/// The selected tier; `Userspace` when [`init`] was never called (unit
/// tests, the plain CLI process) — switches then never offload.
pub fn tier() -> FastpathTier {
    SELECTION
        .get()
        .map(|s| s.tier)
        .unwrap_or(FastpathTier::Userspace)
}

/// Selection outcome as JSON, the shape the `fastpath` proto command (and
/// through it the CLI and web UI) reports.
pub fn status_json() -> serde_json::Value {
    let status = SELECTION.get().cloned().unwrap_or_default();
    let reasons: serde_json::Map<String, serde_json::Value> = status
        .reasons
        .iter()
        .map(|(tier, reason)| (tier.to_string(), serde_json::json!(reason)))
        .collect();
    serde_json::json!({
        "tier": status.tier.as_str(),
        "mode": status.mode.as_str(),
        "reasons": reasons,
    })
}

/// `VMLAB_FASTPATH` beats the host-config mode; a malformed value is
/// ignored with a warning (labs must always come up).
fn env_mode(fallback: FastpathMode) -> FastpathMode {
    match std::env::var("VMLAB_FASTPATH") {
        Ok(s) => FastpathMode::parse(&s).unwrap_or_else(|| {
            tracing::warn!("ignoring VMLAB_FASTPATH=`{s}` (want auto|off|sockmap|afxdp)");
            fallback
        }),
        Err(_) => fallback,
    }
}

/// Pure selection over already-resolved `mode` (no env, no logging).
/// A forced tier that fails its probe degrades to userspace rather than
/// failing daemon startup.
fn select(mode: FastpathMode) -> FastpathStatus {
    fn probe(tier: FastpathTier, reasons: &mut Vec<(&'static str, String)>) -> bool {
        let result = match tier {
            FastpathTier::AfXdp => probe_afxdp(),
            FastpathTier::Sockmap => probe_sockmap(),
            FastpathTier::Userspace => Ok(()),
        };
        match result {
            Ok(()) => true,
            Err(reason) => {
                reasons.push((tier.as_str(), reason));
                false
            }
        }
    }
    let mut reasons: Vec<(&'static str, String)> = Vec::new();
    let tier = match mode {
        FastpathMode::Off => FastpathTier::Userspace,
        FastpathMode::Auto => {
            if probe(FastpathTier::AfXdp, &mut reasons) {
                FastpathTier::AfXdp
            } else {
                // The sockmap tier works but measured ~8x SLOWER than the
                // userspace fabric (every af_unix redirect rides the psock
                // backlog workqueue, ~30µs/frame), so auto never picks it —
                // it stays available for explicit evaluation.
                reasons.push((
                    FastpathTier::Sockmap.as_str(),
                    "not used in auto mode: af_unix kernel splicing measures slower than \
                     the userspace fabric (psock backlog workqueue); force with \
                     `fastpath = \"sockmap\"` to evaluate it"
                        .into(),
                ));
                FastpathTier::Userspace
            }
        }
        FastpathMode::Sockmap => {
            if probe(FastpathTier::Sockmap, &mut reasons) {
                FastpathTier::Sockmap
            } else {
                FastpathTier::Userspace
            }
        }
        FastpathMode::AfXdp => {
            if probe(FastpathTier::AfXdp, &mut reasons) {
                FastpathTier::AfXdp
            } else {
                FastpathTier::Userspace
            }
        }
    };
    FastpathStatus {
        tier,
        mode,
        reasons,
    }
}

#[cfg(not(feature = "ebpf"))]
fn probe_afxdp() -> Result<(), String> {
    Err("vmlab was built without the `ebpf` feature".into())
}

#[cfg(not(feature = "ebpf"))]
fn probe_sockmap() -> Result<(), String> {
    Err("vmlab was built without the `ebpf` feature".into())
}

#[cfg(feature = "ebpf")]
fn probe_afxdp() -> Result<(), String> {
    probe::afxdp()
}

#[cfg(feature = "ebpf")]
fn probe_sockmap() -> Result<(), String> {
    probe::sockmap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_round_trips() {
        for mode in [
            FastpathMode::Auto,
            FastpathMode::Off,
            FastpathMode::Sockmap,
            FastpathMode::AfXdp,
        ] {
            assert_eq!(FastpathMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(FastpathMode::parse("fast"), None);
        assert_eq!(FastpathMode::parse(""), None);
    }

    #[test]
    fn off_skips_probing() {
        let status = select(FastpathMode::Off);
        assert_eq!(status.tier, FastpathTier::Userspace);
        assert!(status.reasons.is_empty());
    }

    #[test]
    fn forced_tier_selects_or_degrades_with_reason() {
        // The real probe runs here: on a host/user with BPF privileges the
        // forced tier is selected; anywhere else it degrades to userspace
        // with a recorded reason. Both are correct.
        let status = select(FastpathMode::Sockmap);
        match status.tier {
            FastpathTier::Sockmap => assert!(status.reasons.is_empty()),
            FastpathTier::Userspace => {
                assert_eq!(status.reasons.len(), 1);
                assert_eq!(status.reasons[0].0, "sockmap");
            }
            FastpathTier::AfXdp => panic!("forced sockmap must never select afxdp"),
        }
    }

    #[test]
    fn auto_records_every_skipped_tier() {
        let status = select(FastpathMode::Auto);
        let skipped: Vec<&str> = status.reasons.iter().map(|(t, _)| *t).collect();
        match status.tier {
            FastpathTier::AfXdp => assert!(skipped.is_empty()),
            // Sockmap measured slower than the userspace fabric; auto must
            // never select it (explicit `fastpath = "sockmap"` only).
            FastpathTier::Sockmap => panic!("auto selected the demoted sockmap tier"),
            FastpathTier::Userspace => assert_eq!(skipped, vec!["afxdp", "sockmap"]),
        }
    }
}
