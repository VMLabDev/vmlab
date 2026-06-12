//! ICMP echo handling — deliberately degraded.
//!
//! Unprivileged ICMP datagram sockets (`SOCK_DGRAM`/`IPPROTO_ICMP`) are not
//! reachable from our dependency set (the `nix` crate is compiled without
//! its socket feature, and raw sockets need CAP_NET_RAW, which the
//! no-privileges mandate of PRD §9.7 forbids). Reachability is instead
//! probed by spawning the system `ping` binary — setuid or cap-enabled on
//! virtually every distribution — once per destination, with the result
//! cached for 10 seconds so a guest flood ping cannot become a subprocess
//! storm. While a probe is in flight, further requests for the same
//! destination are dropped (the guest retransmits).
//!
//! On success the guest receives a synthesized echo reply (identifier,
//! sequence and payload faithfully mirrored); on failure an ICMP
//! host-unreachable from the gateway. RTT observed by the guest is the
//! probe/cache latency, not the real path RTT — only *reachability* is
//! meaningful.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{debug, trace};

use crate::config::model::MacAddr;
use crate::net::frame::{self, IPPROTO_ICMP};

use super::NatEngine;

/// Reachability results are reused for this long.
const CACHE_TTL: Duration = Duration::from_secs(10);

/// Reachability probe. The default implementation shells out to `ping`;
/// tests substitute a stub.
#[async_trait]
pub trait Pinger: Send + Sync {
    /// `true` when `dst` answered an echo.
    async fn ping(&self, dst: Ipv4Addr) -> bool;
}

/// Probes by spawning `ping -c 1 -W <timeout> <dst>`.
#[derive(Debug, Clone, Copy)]
pub struct SubprocessPinger {
    /// Per-probe timeout handed to `ping -W`, in seconds.
    pub timeout_secs: u32,
}

impl Default for SubprocessPinger {
    fn default() -> Self {
        Self { timeout_secs: 1 }
    }
}

#[async_trait]
impl Pinger for SubprocessPinger {
    async fn ping(&self, dst: Ipv4Addr) -> bool {
        let status = tokio::process::Command::new("ping")
            .arg("-c")
            .arg("1")
            .arg("-W")
            .arg(self.timeout_secs.to_string())
            .arg("-n")
            .arg(dst.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        match status {
            Ok(s) => s.success(),
            Err(e) => {
                debug!(%dst, error = %e, "nat: failed to spawn ping");
                false
            }
        }
    }
}

/// Per-engine probe cache + in-flight dedup.
pub(super) struct IcmpState {
    cache: StdMutex<HashMap<Ipv4Addr, (bool, Instant)>>,
    pending: StdMutex<HashSet<Ipv4Addr>>,
}

impl IcmpState {
    pub fn new() -> Self {
        Self {
            cache: StdMutex::new(HashMap::new()),
            pending: StdMutex::new(HashSet::new()),
        }
    }

    fn cached(&self, dst: Ipv4Addr) -> Option<bool> {
        let cache = self.cache.lock().expect("icmp cache lock");
        match cache.get(&dst) {
            Some((ok, at)) if at.elapsed() < CACHE_TTL => Some(*ok),
            _ => None,
        }
    }

    /// Try to claim the probe for `dst`; `false` means one is in flight.
    fn claim(&self, dst: Ipv4Addr) -> bool {
        self.pending.lock().expect("icmp pending lock").insert(dst)
    }

    fn complete(&self, dst: Ipv4Addr, ok: bool) {
        self.cache
            .lock()
            .expect("icmp cache lock")
            .insert(dst, (ok, Instant::now()));
        self.pending.lock().expect("icmp pending lock").remove(&dst);
    }
}

/// Handle one guest echo request (`pkt` = the full IPv4 packet).
pub(super) async fn handle_echo(engine: Arc<NatEngine>, guest_mac: MacAddr, pkt: Vec<u8>) {
    let Some(ip) = frame::Ipv4View::parse(&pkt) else {
        return;
    };
    let dst = ip.dst();
    let reachable = match engine.icmp.cached(dst) {
        Some(ok) => ok,
        None => {
            if !engine.icmp.claim(dst) {
                trace!(%dst, "nat: probe already in flight, dropping echo");
                return;
            }
            let ok = engine.pinger.ping(dst).await;
            engine.icmp.complete(dst, ok);
            ok
        }
    };
    respond(&engine, guest_mac, &pkt, reachable).await;
}

/// Synthesize the guest-facing answer: a mirrored echo reply on success, an
/// ICMP host-unreachable from the gateway on failure.
async fn respond(engine: &Arc<NatEngine>, guest_mac: MacAddr, pkt: &[u8], reachable: bool) {
    if reachable {
        if let Some(reply) = frame::icmp_echo_reply_for(pkt) {
            engine.send_frame(guest_mac, &reply).await;
        }
        return;
    }
    let Some(ip) = frame::Ipv4View::parse(pkt) else {
        return;
    };
    let unreachable = frame::icmp_unreachable_for(pkt, 1); // host unreachable
    let out = frame::ipv4_build(
        engine.config().gw_ip,
        ip.src(),
        IPPROTO_ICMP,
        64,
        &unreachable,
        engine.next_ip_id(),
    );
    engine.send_frame(guest_mac, &out).await;
}
