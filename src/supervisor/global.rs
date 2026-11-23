//! Supervisor-owned global segments (PRD §9.2): shared L2 switches that
//! span labs (and hosts). Created on first attach, destroyed on last detach.
//! The supervisor runs the shared segment's DHCP/DNS so registrations span
//! labs coherently. Lab daemons attach via segment trunks (unix sockets);
//! the *same* frame-forwarding trunk protocol over TCP bridges two
//! supervisors for cross-host segments — one mechanism, two transports.
//!
//! Cross-host trunks are tracked per (segment, remote IP) in a [`TrunkTable`]:
//! at most one trunk per remote host. That single-slot rule is what prevents
//! a broadcast storm when both sides declare `connect {}` to each other (two
//! parallel Service ports would re-flood every broadcast back and forth
//! forever — the switch floods broadcasts to all other ports, Service→Service
//! included). The dialer stands down while an inbound trunk from the same IP
//! is active, the listener refuses a second trunk from an IP it already has,
//! and jittered retries break the simultaneous-dial tie. Known limitation:
//! a NAT'd or multi-homed peer can defeat the IP match, so one-sided
//! `connect` remains the documented safe form for exotic topologies.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ipnet::Ipv4Net;
use serde_json::json;
use tokio::sync::Mutex;

use crate::config::model::MacAddr;
use crate::net::dhcp::DhcpConfig;
use crate::net::dns::DnsZone;
use crate::net::framing::{read_frame, write_frame};
use crate::net::gateway::{Gateway, GatewayConfig, GatewayHandle, gateway_mac};
use crate::net::switch::{ChannelPort, PortClass, PortId, Switch};
use crate::proto::Event;
use crate::proto::vocab::GlobalMember;

/// Global segments use a distinct pool from per-lab segments to avoid
/// collisions when both appear on one host.
const GLOBAL_POOL: &str = "10.214.0.0/16";

/// Default trunk TCP port, appended when a `connect { host }` has no `:port`.
const DEFAULT_TRUNK_PORT: u16 = 13947;

/// Retry / stand-down cadence for the dialer loop.
#[cfg(not(test))]
const REDIAL_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const REDIAL_DELAY: Duration = Duration::from_millis(150);

/// Jitter ceiling for the dialer retry (breaks simultaneous-dial ties).
#[cfg(not(test))]
const JITTER_MS: u32 = 2000;
#[cfg(test)]
const JITTER_MS: u32 = 100;

/// How many connections the trunk listener holds that have not finished the
/// handshake. A connection costs a descriptor and a task for up to
/// `trunk_auth::TIMEOUT` before its peer has proved anything, and the
/// listener is open to every host that reaches the port
/// (docs/reviews/networking.md F11). A real peer dials once per segment and
/// finishes in milliseconds, and it dials again after `REDIAL_DELAY` when it
/// is refused.
const PREAUTH_MAX: usize = 32;
/// The share of [`PREAUTH_MAX`] one remote address may hold, so that one
/// address cannot shut the others out.
const PREAUTH_MAX_PER_ADDR: usize = 8;

/// The connections the trunk listener holds before authentication, counted
/// per remote address.
#[derive(Default)]
struct PreAuth {
    held: std::sync::Mutex<HashMap<IpAddr, usize>>,
}

impl PreAuth {
    /// A slot for a new connection from `ip`, or `None` when the address or
    /// the listener holds its limit: the caller drops the connection.
    fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<PreAuthSlot> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let mine = held.get(&ip).copied().unwrap_or(0);
        if mine >= PREAUTH_MAX_PER_ADDR || held.values().sum::<usize>() >= PREAUTH_MAX {
            return None;
        }
        held.insert(ip, mine + 1);
        Some(PreAuthSlot {
            table: self.clone(),
            ip,
        })
    }
}

/// One held connection. Dropping it gives the slot back.
struct PreAuthSlot {
    table: Arc<PreAuth>,
    ip: IpAddr,
}

impl Drop for PreAuthSlot {
    fn drop(&mut self) {
        let mut held = self.table.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = held.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.ip);
            }
        }
    }
}

/// One cross-host trunk slot.
enum TrunkState {
    /// Our dialer is mid-handshake to this IP.
    Dialing,
    /// A live bridge exists (dialed by us or accepted from them).
    Active {
        direction: &'static str, // "dial" | "accept"
        addr: SocketAddr,
        since: chrono::DateTime<chrono::Utc>,
        /// Abort handle for an inbound bridge. Outbound bridges run in the
        /// dialer task and are dropped when that task is aborted.
        task: Option<tokio::task::AbortHandle>,
    },
}

/// Per-segment trunk bookkeeping, shared by the dialer loop, the listener's
/// accept path, `list()`, and segment teardown. Emits `segment.peer.up` /
/// `segment.peer.down` transition events on the supervisor's aggregate
/// stream (host-scoped: `lab` is empty; the UI keys on the segment name).
struct TrunkTable {
    segment: String,
    events: tokio::sync::broadcast::Sender<Event>,
    slots: std::sync::Mutex<HashMap<IpAddr, TrunkState>>,
}

impl TrunkTable {
    fn new(segment: &str, events: tokio::sync::broadcast::Sender<Event>) -> Arc<Self> {
        Arc::new(Self {
            segment: segment.to_string(),
            events,
            slots: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Reserve a dial slot for `ip`. `false` = a trunk to/from that host
    /// already exists (or is being dialed) — the caller stands down.
    fn try_reserve_dial(&self, ip: IpAddr) -> bool {
        let mut slots = self.slots.lock().expect("trunk slots");
        if slots.contains_key(&ip) {
            return false;
        }
        slots.insert(ip, TrunkState::Dialing);
        true
    }

    /// Whether an inbound trunk from `ip` may be accepted (no existing slot).
    /// Reserves the slot on success so a racing second accept is refused.
    fn try_reserve_accept(&self, addr: SocketAddr, task: tokio::task::AbortHandle) -> bool {
        let mut slots = self.slots.lock().expect("trunk slots");
        if slots.contains_key(&addr.ip()) {
            return false;
        }
        slots.insert(
            addr.ip(),
            TrunkState::Active {
                direction: "accept",
                addr,
                since: chrono::Utc::now(),
                task: Some(task),
            },
        );
        drop(slots);
        self.emit_up(addr, "accept");
        true
    }

    /// Flip a reserved dial slot to active (handshake succeeded).
    fn activate_dial(&self, addr: SocketAddr) {
        let mut slots = self.slots.lock().expect("trunk slots");
        slots.insert(
            addr.ip(),
            TrunkState::Active {
                direction: "dial",
                addr,
                since: chrono::Utc::now(),
                task: None,
            },
        );
        drop(slots);
        self.emit_up(addr, "dial");
    }

    /// Drop the slot for `ip`; emits `segment.peer.down` if it was active.
    fn clear(&self, ip: IpAddr) {
        let removed = self.slots.lock().expect("trunk slots").remove(&ip);
        if let Some(TrunkState::Active {
            direction, addr, ..
        }) = removed
        {
            let _ = self.events.send(Event::new(
                "segment.peer.down",
                "",
                json!({"segment": self.segment, "peer": addr.to_string(), "direction": direction}),
            ));
            tracing::info!(
                "cross-host trunk {addr} for \"{}\" down ({direction})",
                self.segment
            );
        }
    }

    fn emit_up(&self, addr: SocketAddr, direction: &'static str) {
        let _ = self.events.send(Event::new(
            "segment.peer.up",
            "",
            json!({"segment": self.segment, "peer": addr.to_string(), "direction": direction}),
        ));
        tracing::info!(
            "cross-host trunk {addr} for \"{}\" up ({direction})",
            self.segment
        );
    }

    fn has(&self, ip: IpAddr) -> bool {
        self.slots.lock().expect("trunk slots").contains_key(&ip)
    }

    fn connected(&self) -> bool {
        self.slots
            .lock()
            .expect("trunk slots")
            .values()
            .any(|s| matches!(s, TrunkState::Active { .. }))
    }

    fn peers_json(&self) -> Vec<serde_json::Value> {
        self.slots
            .lock()
            .expect("trunk slots")
            .values()
            .filter_map(|s| match s {
                TrunkState::Active {
                    direction,
                    addr,
                    since,
                    ..
                } => Some(json!({
                    "addr": addr.to_string(),
                    "direction": direction,
                    "since": since.to_rfc3339(),
                })),
                TrunkState::Dialing => None,
            })
            .collect()
    }

    /// Abort every inbound bridge and emit down for every active trunk
    /// (segment teardown).
    fn teardown(&self) {
        let slots: Vec<(IpAddr, TrunkState)> = {
            let mut map = self.slots.lock().expect("trunk slots");
            map.drain().collect()
        };
        for (_, state) in slots {
            if let TrunkState::Active {
                direction,
                addr,
                task,
                ..
            } = state
            {
                if let Some(task) = task {
                    task.abort();
                }
                let _ = self.events.send(Event::new(
                    "segment.peer.down",
                    "",
                    json!({"segment": self.segment, "peer": addr.to_string(), "direction": direction}),
                ));
            }
        }
    }
}

/// A lab joining a global segment (PRD §9.2): its name, the NICs it puts
/// there, and the MTU those NICs were started with.
pub struct Joining {
    pub lab: String,
    pub members: Vec<GlobalMember>,
    /// The attaching lab's MTU for the segment — its declared `mtu`, else
    /// 1500. `None` from a caller with no opinion.
    pub mtu: Option<u16>,
}

/// Who is on a global segment, lab by lab, which names the segment's DNS
/// zone currently holds on their behalf, and what its DHCP serves them
/// (PRD §9.2, §9.4, §9.5).
#[derive(Default)]
struct Directory {
    /// Each lab's members, a refused static address already stripped.
    members: HashMap<String, Vec<GlobalMember>>,
    /// Bare names (before the zone suffix) this directory registered.
    registered: std::collections::BTreeSet<String>,
    /// The MTU the segment serves (DHCP option 26) and the lab that set it:
    /// the first lab to attach with one. It stays for the segment's life —
    /// guests already running took it.
    mtu: Option<(u16, String)>,
}

impl Directory {
    /// Admit `joining` onto the segment `name` (subnet `subnet`, gateway
    /// `gateway`), replacing whatever that lab had there before. A declared
    /// static address becomes a DHCP reservation unless it cannot be one —
    /// not a host address of the subnet, the gateway's, another lab's
    /// reservation, or held by another machine's lease — in which case that
    /// NIC gets a dynamic lease and the refusal is returned as a warning. So
    /// is an MTU that differs from the one the segment already serves: the
    /// first lab to attach sets it. Nothing here refuses the attach itself;
    /// the lab still joins the segment.
    fn admit(
        &mut self,
        name: &str,
        subnet: Ipv4Net,
        gateway: Ipv4Addr,
        joining: Joining,
        leases: &[(MacAddr, Ipv4Addr)],
    ) -> Vec<String> {
        let Joining {
            lab,
            mut members,
            mtu,
        } = joining;
        self.members.remove(&lab);
        let mut warnings = Vec::new();
        let others: Vec<(String, &GlobalMember)> = self
            .members
            .iter()
            .flat_map(|(l, ms)| ms.iter().map(move |m| (format!("{}.{l}", m.machine), m)))
            .collect();
        for m in &mut members {
            let Some(ip) = m.ip else { continue };
            let usable = subnet.contains(&ip)
                && ip != subnet.network()
                && ip != subnet.broadcast()
                && ip != gateway;
            let reserved = others
                .iter()
                .find(|(_, o)| o.ip == Some(ip) && o.mac != m.mac);
            let leased = leases.iter().find(|(mac, l)| *l == ip && *mac != m.mac);
            let why = if !usable {
                Some(format!(
                    "it is not a host address on the segment's subnet {subnet} other than the gateway {gateway}"
                ))
            } else if let Some((who, _)) = reserved {
                Some(format!("{who} already has it"))
            } else if let Some((mac, _)) = leased {
                let who = others
                    .iter()
                    .find(|(_, o)| o.mac == *mac)
                    .map_or_else(|| format!("the machine with MAC {mac}"), |(w, _)| w.clone());
                Some(format!("{who} holds it on a lease"))
            } else {
                None
            };
            if let Some(why) = why {
                warnings.push(format!(
                    "global segment \"{name}\": {}.{lab} declares ip {ip}, but {why}; it gets a \
                     dynamic lease instead",
                    m.machine
                ));
                m.ip = None;
            }
        }
        if let Some(mtu) = mtu {
            match &self.mtu {
                None => self.mtu = Some((mtu, lab.clone())),
                Some((served, by)) if *served != mtu => warnings.push(format!(
                    "global segment \"{name}\" serves mtu {served}, set by lab \"{by}\" which \
                     attached first, but this lab's NICs use {mtu}; declare the same `mtu` in \
                     every lab that shares the segment"
                )),
                Some(_) => {}
            }
        }
        self.members.insert(lab, members);
        warnings
    }

    /// Every admitted static address, by MAC: the segment's DHCP
    /// reservations (§9.4).
    fn reservations(&self) -> HashMap<MacAddr, Ipv4Addr> {
        self.members
            .values()
            .flatten()
            .filter_map(|m| Some((m.mac, m.ip?)))
            .collect()
    }

    /// The MTU the segment's DHCP serves: the first attaching lab's, else
    /// the classic 1500.
    fn mtu(&self) -> u16 {
        self.mtu
            .as_ref()
            .map_or(crate::labd::network::STANDARD_MTU, |(m, _)| *m)
    }

    /// Every name the attached labs' machines answer to, from their leases
    /// (or a declared static address before one exists): `<vm>.<lab>`
    /// always, and the short `<vm>` alias only where no other lab on the
    /// segment has a machine of that name (§9.5).
    fn names(&self, leases: &HashMap<MacAddr, Ipv4Addr>) -> HashMap<String, Ipv4Addr> {
        let mut labs_by_machine: HashMap<&str, std::collections::BTreeSet<&str>> = HashMap::new();
        for (lab, members) in &self.members {
            for m in members {
                labs_by_machine.entry(&m.machine).or_default().insert(lab);
            }
        }
        let mut names = HashMap::new();
        for (lab, members) in &self.members {
            for m in members {
                let Some(ip) = leases.get(&m.mac).copied().or(m.ip) else {
                    continue;
                };
                names.insert(format!("{}.{lab}", m.machine), ip);
                if labs_by_machine[m.machine.as_str()].len() == 1 {
                    names.insert(m.machine.clone(), ip);
                }
            }
        }
        names
    }

    /// Bring `zone` in line with [`names`](Self::names): register what is
    /// new or moved, unregister what left.
    fn sync(&mut self, zone: &std::sync::Mutex<DnsZone>, leases: &[(MacAddr, Ipv4Addr)]) {
        let names = self.names(&leases.iter().copied().collect());
        let Ok(mut zone) = zone.lock() else { return };
        for gone in self.registered.iter().filter(|n| !names.contains_key(*n)) {
            zone.unregister(gone);
        }
        for (name, ip) in &names {
            zone.register(name, *ip);
        }
        self.registered = names.into_keys().collect();
    }
}

struct GlobalSeg {
    switch: Arc<Switch>,
    gateway: GatewayHandle,
    directory: Arc<std::sync::Mutex<Directory>>,
    /// Keeps the zone following the leases.
    dns_sync: tokio::task::JoinHandle<()>,
    subnet: Ipv4Net,
    /// Who holds the segment (docs/reviews/lab-lifecycle.md F8). A lab is
    /// one reference however often it attaches, and only a lab that holds
    /// one can give it back.
    holders: Holders,
    sock: PathBuf,
    listener: tokio::task::JoinHandle<()>,
    /// Outbound dialer loop (present once some attach declared `connect`).
    dialer: Option<tokio::task::JoinHandle<()>>,
    /// Cross-host trunk slots (dialer + inbound accepts + list()).
    trunks: Arc<TrunkTable>,
}

/// The references a global segment counts: each attached lab by name, and
/// the anonymous ones (inbound cross-host trunks, a caller naming no lab).
#[derive(Default)]
struct Holders {
    labs: std::collections::BTreeSet<String>,
    anonymous: usize,
}

impl Holders {
    fn take(&mut self, lab: Option<&str>) {
        match lab {
            Some(lab) => {
                self.labs.insert(lab.to_string());
            }
            None => self.anonymous += 1,
        }
    }

    /// Give back `lab`'s reference, or one anonymous one. Returns false
    /// when `lab` holds none, and nothing changes.
    fn give_back(&mut self, lab: Option<&str>) -> bool {
        match lab {
            Some(lab) => self.labs.remove(lab),
            None if self.anonymous > 0 => {
                self.anonymous -= 1;
                true
            }
            None => false,
        }
    }

    fn count(&self) -> usize {
        self.labs.len() + self.anonymous
    }
}

impl GlobalSeg {
    /// Put a lab's machines into the segment's DNS and their static
    /// addresses and MTU into its DHCP. Returns what [`Directory::admit`]
    /// refused, for the lab to say.
    fn join(&self, name: &str, lab: Option<Joining>) -> Vec<String> {
        let Some(joining) = lab else {
            return Vec::new();
        };
        let leases = self.gateway.leases_probe()().unwrap_or_default();
        let gw = self.gateway.gw_ip();
        let mut dir = self.directory.lock().expect("global directory");
        let warnings = dir.admit(name, self.subnet, gw, joining, &leases);
        self.sync_dhcp(&dir);
        self.sync_dns(&mut dir);
        warnings
    }

    /// Take a lab's machines back out of the segment's DNS and DHCP.
    fn leave(&self, lab: Option<&str>) {
        let Some(lab) = lab else { return };
        let mut dir = self.directory.lock().expect("global directory");
        if dir.members.remove(lab).is_some() {
            self.sync_dhcp(&dir);
            self.sync_dns(&mut dir);
        }
    }

    fn sync_dhcp(&self, dir: &Directory) {
        let (reservations, mtu) = (dir.reservations(), dir.mtu());
        self.gateway.reconfigure_dhcp(|c| {
            c.reservations = reservations;
            c.mtu = mtu;
        });
    }

    fn sync_dns(&self, dir: &mut Directory) {
        if let (Some(zone), Some(leases)) = (self.gateway.dns_zone(), self.gateway.leases_probe()())
        {
            dir.sync(&zone, &leases);
        }
    }
}

/// Keep the segment's DNS following its DHCP leases, as a lab segment's
/// lease sync does (PRD §9.5 auto-registration).
fn spawn_dns_sync(
    gateway: &GatewayHandle,
    directory: Arc<std::sync::Mutex<Directory>>,
) -> tokio::task::JoinHandle<()> {
    let zone = gateway.dns_zone().expect("global segments serve DNS");
    let leases = gateway.leases_probe();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Some(leases) = leases() else { break };
            directory
                .lock()
                .expect("global directory")
                .sync(&zone, &leases);
        }
    })
}

pub struct GlobalSegments {
    segs: Mutex<HashMap<String, GlobalSeg>>,
    next_index: Mutex<u32>,
    dns_suffix: String,
    /// Where the segments' DNS forwards names no attached lab answers to.
    upstream_dns: Option<SocketAddr>,
    psk: Option<String>,
    events: tokio::sync::broadcast::Sender<Event>,
    /// Where segment trunk unix sockets live (tests inject a tempdir so two
    /// in-process instances don't collide).
    sock_dir: PathBuf,
}

impl GlobalSegments {
    pub fn new(
        host: &crate::config::host::HostConfig,
        events: tokio::sync::broadcast::Sender<Event>,
    ) -> Arc<Self> {
        Self::new_at(
            crate::paths::runtime_dir().join("global"),
            host.dns_suffix.clone(),
            host.upstream_resolver(),
            host.psk.as_ref().map(|psk| psk.expose().to_string()),
            events,
        )
    }

    pub(crate) fn new_at(
        sock_dir: PathBuf,
        dns_suffix: String,
        upstream_dns: Option<SocketAddr>,
        psk: Option<String>,
        events: tokio::sync::broadcast::Sender<Event>,
    ) -> Arc<Self> {
        Arc::new(Self {
            segs: Mutex::new(HashMap::new()),
            next_index: Mutex::new(0),
            dns_suffix,
            upstream_dns,
            psk,
            events,
            sock_dir,
        })
    }

    /// Attach to (creating if needed) the global segment `name`. Returns the
    /// unix socket the caller's lab daemon connects its trunk to, the
    /// warnings that lab should show (see [`Directory::admit`]) and the
    /// segment's subnet, which the lab names again when it re-attaches (F9).
    ///
    /// `lab` is the attaching lab and the NICs it puts on the segment; their
    /// names join the segment's DNS, and their static addresses its DHCP
    /// reservations, until that lab detaches. An inbound cross-host trunk
    /// attaches with none.
    pub async fn attach(
        self: &Arc<Self>,
        name: &str,
        subnet: Option<Ipv4Net>,
        peer: Option<String>,
        lab: Option<Joining>,
    ) -> Result<(PathBuf, Vec<String>, Ipv4Net)> {
        check_segment_name(name)?;
        let peer = peer
            .map(|peer| self.require_psk().map(|psk| (peer, psk)))
            .transpose()?;
        let mut segs = self.segs.lock().await;
        if let Some(seg) = segs.get_mut(name) {
            seg.holders.take(lab.as_ref().map(|j| j.lab.as_str()));
            let warnings = seg.join(name, lab);
            // A later lab may be the one declaring `connect` — start the
            // dialer on an already-existing segment too.
            if let Some((peer, psk)) = peer
                && seg.dialer.is_none()
            {
                seg.dialer = Some(spawn_tcp_peer_dialer(
                    seg.switch.clone(),
                    peer,
                    psk,
                    seg.trunks.clone(),
                ));
            }
            return Ok((seg.sock.clone(), warnings, seg.subnet));
        }

        let mut next_index = self.next_index.lock().await;
        let automatic_subnet = subnet.is_none();
        // A subnet named explicitly may be one this pool handed out before:
        // an adopted lab re-attaching after a supervisor restart (F9) names
        // the subnet its guests hold leases in. Allocate past it.
        let pool_index = subnet.and_then(pool_index);
        let subnet = match subnet {
            Some(subnet) => subnet,
            None => {
                let pool: Ipv4Net = GLOBAL_POOL.parse().expect("valid global pool");
                pool.subnets(24)
                    .expect("pool splits")
                    .nth(*next_index as usize)
                    .ok_or_else(|| anyhow::anyhow!("global subnet pool exhausted"))?
            }
        };
        std::fs::create_dir_all(&self.sock_dir)?;
        let sock = self.sock_dir.join(format!("{name}.sock"));
        let switch = Switch::new(format!("global/{name}"));
        let listener = switch
            .listen_unix(&sock, PortClass::Service)
            .await
            .with_context(|| format!("listening on {}", sock.display()))?;

        // Commit the allocation only after the last fallible setup step.
        if automatic_subnet {
            *next_index += 1;
        } else if let Some(index) = pool_index {
            *next_index = (*next_index).max(index + 1);
        }

        let gw_ip = Ipv4Addr::from(u32::from(subnet.network()) + 1);
        let gw_mac = gateway_mac("__global", name);

        let mut dhcp = DhcpConfig::new(subnet, gw_ip, gw_mac);
        dhcp.dns_server = Some(gw_ip);
        dhcp.domain = Some(format!("{}.{}", domain_label(name), self.dns_suffix));
        let zone = DnsZone::new(&self.dns_suffix);

        let gateway = Gateway::spawn(
            &switch,
            GatewayConfig {
                segment_name: name.to_string(),
                lab_name: "__global".to_string(),
                gw_ip,
                gw_mac,
                dhcp: Some(dhcp),
                dns: Some(zone),
                upstream_dns: self.upstream_dns,
            },
        );
        let directory: Arc<std::sync::Mutex<Directory>> = Arc::default();
        let dns_sync = spawn_dns_sync(&gateway, directory.clone());

        let trunks = TrunkTable::new(name, self.events.clone());
        let dialer = peer
            .map(|(peer, psk)| spawn_tcp_peer_dialer(switch.clone(), peer, psk, trunks.clone()));

        let seg = GlobalSeg {
            switch,
            gateway,
            directory,
            dns_sync,
            subnet,
            holders: {
                let mut holders = Holders::default();
                holders.take(lab.as_ref().map(|j| j.lab.as_str()));
                holders
            },
            sock: sock.clone(),
            listener,
            dialer,
            trunks,
        };
        let warnings = seg.join(name, lab);
        segs.insert(name.to_string(), seg);
        tracing::info!("global segment \"{name}\" created on {subnet}");
        Ok((sock, warnings, subnet))
    }

    fn require_psk(&self) -> Result<String> {
        self.psk
            .clone()
            .ok_or_else(|| anyhow::anyhow!("cross-host segment needs a `psk` in host config"))
    }

    /// Detach; destroys the segment when the last reference goes. `lab`'s
    /// names leave the segment's DNS with it. A lab that holds no reference
    /// changes nothing: it cannot take the segment from the labs on it.
    pub async fn detach(self: &Arc<Self>, name: &str, lab: Option<&str>) {
        let mut segs = self.segs.lock().await;
        if let Some(seg) = segs.get_mut(name) {
            if !seg.holders.give_back(lab) {
                tracing::debug!(
                    "global segment \"{name}\": {} holds no reference to give back",
                    lab.unwrap_or("an anonymous caller")
                );
                return;
            }
            seg.leave(lab);
            if seg.holders.count() == 0
                && let Some(seg) = segs.remove(name)
            {
                seg.listener.abort();
                seg.dns_sync.abort();
                if let Some(d) = seg.dialer {
                    d.abort();
                }
                seg.trunks.teardown();
                let _ = std::fs::remove_file(&seg.sock);
                tracing::info!("global segment \"{name}\" destroyed");
            }
        }
    }

    /// Segment inventory for `global.list`: name, subnet, refcount, the
    /// labs attached, and the
    /// live cross-host trunk state (drives the web UI's peer LED).
    pub async fn list(&self) -> Vec<serde_json::Value> {
        self.segs
            .lock()
            .await
            .iter()
            .map(|(n, s)| {
                json!({
                    "name": n,
                    "subnet": s.subnet.to_string(),
                    "refcount": s.holders.count(),
                    "labs": s.holders.labs.iter().collect::<Vec<_>>(),
                    "mtu": s.directory.lock().expect("global directory").mtu(),
                    "peer_connected": s.trunks.connected(),
                    "peers": s.trunks.peers_json(),
                })
            })
            .collect()
    }

    /// Cheap pre-check for the listener: is there room for an inbound trunk
    /// from `ip` on segment `name`? (A missing segment says yes — accepting
    /// creates it.) The authoritative check is the atomic reserve inside
    /// [`accept_peer`]; this just lets the listener answer `NO\n` up front.
    pub async fn peer_slot_free(&self, name: &str, ip: IpAddr) -> bool {
        match self.segs.lock().await.get(name) {
            Some(seg) => !seg.trunks.has(ip),
            None => true,
        }
    }

    /// Accept an inbound cross-host peer trunk (after PSK auth) and bridge it
    /// onto the named global segment, creating the segment if necessary.
    /// Refuses (returns `false`) when a trunk to/from that remote host
    /// already exists — the single-slot rule that stops mutual-connect from
    /// creating a broadcast loop.
    pub async fn accept_peer(
        self: &Arc<Self>,
        name: &str,
        stream: tokio::net::TcpStream,
    ) -> Result<bool> {
        let remote = stream.peer_addr().context("peer_addr")?;
        // The inbound trunk counts as a segment reference until it dies.
        let _ = self.attach(name, None, None, None).await?;
        let (switch, trunks) = {
            let segs = self.segs.lock().await;
            let seg = segs.get(name).expect("just attached");
            (seg.switch.clone(), seg.trunks.clone())
        };
        tune_trunk_socket(&stream);
        let bridge = tokio::spawn(bridge_tcp_to_switch(switch, stream));
        if !trunks.try_reserve_accept(remote, bridge.abort_handle()) {
            bridge.abort();
            self.detach(name, None).await;
            return Ok(false);
        }
        // Waiter: on bridge death clear the slot (emitting peer.down) and
        // release the segment reference.
        let me = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let _ = bridge.await;
            trunks.clear(remote.ip());
            me.detach(&name, None).await;
        });
        Ok(true)
    }
}

struct TrunkPort {
    switch: Arc<Switch>,
    id: PortId,
}

impl Drop for TrunkPort {
    fn drop(&mut self) {
        self.switch.remove_port(self.id);
    }
}

/// Bridge a TCP stream onto a switch. Both directions and the port belong to
/// the returned future, so dropping it closes the trunk and removes the port.
fn bridge_tcp_to_switch(
    switch: Arc<Switch>,
    mut stream: tokio::net::TcpStream,
) -> impl Future<Output = ()> {
    let ChannelPort { id, tx, mut rx } = switch.add_channel_port(PortClass::Service);
    let port = TrunkPort { switch, id };

    async move {
        let _port = port;
        let (mut read_half, mut write_half) = stream.split();
        tokio::select! {
            () = async {
                while let Some(frame) = rx.recv().await {
                    if write_frame(&mut write_half, &frame).await.is_err() {
                        break;
                    }
                }
            } => {}
            () = async {
                while let Ok(Some(frame)) = read_frame(&mut read_half).await {
                    if tx.try_send(frame).is_err() {
                        tracing::debug!("global trunk ingress queue full");
                    }
                }
            } => {}
        }
    }
}

/// Keepalive-tune a trunk socket so a silently dead peer surfaces as a read
/// error in ~25s (10s idle + 3×5s probes) instead of hanging forever — the
/// framed-L2 protocol itself has no heartbeat.
fn tune_trunk_socket(stream: &tokio::net::TcpStream) {
    use socket2::{SockRef, TcpKeepalive};
    let ka = TcpKeepalive::new()
        .with_time(Duration::from_secs(10))
        .with_interval(Duration::from_secs(5))
        .with_retries(3);
    let sock = SockRef::from(stream);
    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        tracing::debug!("trunk keepalive: {e}");
    }
    let _ = stream.set_nodelay(true);
}

/// `host[:port]` → a dialable address string (default trunk port appended).
fn peer_addr_string(peer: &str) -> String {
    if peer.contains(':') {
        peer.to_string()
    } else {
        format!("{peer}:{DEFAULT_TRUNK_PORT}")
    }
}

/// Sub-second jitter so two supervisors dialing each other don't retry in
/// lockstep forever (the tie-breaker for the simultaneous-dial race).
fn dial_jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis(u64::from(nanos % JITTER_MS))
}

/// Dial a remote supervisor's trunk TCP port, authenticate with the PSK, and
/// bridge the segment. The loop owns the whole trunk lifecycle: it redials
/// after failures AND after an established bridge dies (one bridge per
/// iteration — never duplicate ports), and stands down while an inbound
/// trunk from the same host is active (see the module docs' storm guard).
fn spawn_tcp_peer_dialer(
    switch: Arc<Switch>,
    peer: String,
    psk: String,
    trunks: Arc<TrunkTable>,
) -> tokio::task::JoinHandle<()> {
    let target = peer_addr_string(&peer);
    tokio::spawn(async move {
        loop {
            // Re-resolve each round (DNS may change); the resolved IP is the
            // dedupe key, so it must match what the connection will use.
            let addr = match tokio::net::lookup_host(&target).await {
                Ok(mut addrs) => match addrs.next() {
                    Some(a) => a,
                    None => {
                        tracing::warn!("cross-host trunk: {target} resolves to nothing");
                        tokio::time::sleep(REDIAL_DELAY).await;
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!("cross-host trunk: resolving {target}: {e}");
                    tokio::time::sleep(REDIAL_DELAY).await;
                    continue;
                }
            };
            if !trunks.try_reserve_dial(addr.ip()) {
                // An inbound trunk from that host is already active (or a
                // dial is in flight) — one trunk is enough.
                tokio::time::sleep(REDIAL_DELAY).await;
                continue;
            }
            match dial_peer(addr, &trunks.segment, &psk).await {
                Ok(stream) => {
                    tune_trunk_socket(&stream);
                    let bridge = bridge_tcp_to_switch(switch.clone(), stream);
                    trunks.activate_dial(addr);
                    // Wait for the bridge to die, then loop around to redial.
                    bridge.await;
                    trunks.clear(addr.ip());
                }
                Err(e) => {
                    trunks.clear(addr.ip()); // was Dialing → no down event
                    tracing::warn!(
                        "cross-host trunk to {target} for \"{}\" failed: {e:#}; retrying",
                        trunks.segment
                    );
                }
            }
            tokio::time::sleep(REDIAL_DELAY + dial_jitter()).await;
        }
    })
}

/// Refuse a global segment name that is not one file name. The name becomes
/// `<sock_dir>/<name>.sock`, which is unlinked and bound, and a trunk peer
/// sends its own: a separator, `..` or an absolute path would aim that at
/// another file. A lab's name is a DNS label, or `<label>@<worktree>` with a
/// directory's name after the `@` (PRD §21.4), so nothing a lab sends is
/// refused here.
/// Where `subnet` sits among the automatic pool's /24s, if it is one.
fn pool_index(subnet: Ipv4Net) -> Option<u32> {
    let pool: Ipv4Net = GLOBAL_POOL.parse().expect("valid global pool");
    (subnet.prefix_len() == 24 && pool.contains(&subnet))
        .then(|| (u32::from(subnet.network()) - u32::from(pool.network())) >> 8)
}

fn check_segment_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c == '/' || c.is_control());
    if !ok {
        bail!("invalid global segment name {name:?}");
    }
    Ok(())
}

/// A global segment's name as a DNS domain label. A worktree lab's global
/// segment is named `<segment>@<worktree>` (PRD §21.4), and a worktree is
/// named by its directory: `@`, and anything else a segment name never
/// holds, becomes `-`. A plain segment name passes through unchanged.
fn domain_label(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '-',
        })
        .collect()
}

async fn dial_peer(addr: SocketAddr, segment: &str, psk: &str) -> Result<tokio::net::TcpStream> {
    use tokio::io::AsyncReadExt;
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .with_context(|| format!("connecting to peer {addr}"))?;
    tokio::time::timeout(super::trunk_auth::TIMEOUT, async {
        super::trunk_auth::client(&mut stream, segment, psk).await?;
        let mut buf = [0u8; 3];
        stream.read_exact(&mut buf).await?;
        if &buf != b"OK\n" {
            bail!("peer {addr} rejected the trunk");
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("trunk handshake timed out")??;
    Ok(stream)
}

/// Bind the inbound cross-host trunk listener and serve it in a background
/// task. Binding happens before the task spawns so callers (and tests, which
/// bind `127.0.0.1:0`) see bind errors — and the real listen address.
pub async fn bind_peer_listener(
    globals: Arc<GlobalSegments>,
    bind: std::net::SocketAddr,
    psk: String,
) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("cross-host trunk listener bind {bind}"))?;
    let addr = listener.local_addr()?;
    tracing::info!("cross-host trunk listener on {addr}");
    let handle = tokio::spawn(async move {
        let preauth = Arc::new(PreAuth::default());
        let mut errors = crate::net::accept::AcceptErrors::default();
        loop {
            let (mut stream, remote) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    errors.failed("cross-host trunk listener", &e).await;
                    continue;
                }
            };
            errors.accepted();
            // Past the limit the connection is closed here, before it gets a
            // task or a greeting.
            let Some(slot) = preauth.admit(remote.ip()) else {
                tracing::debug!(
                    "trunk listener: too many unauthenticated connections, {remote} dropped"
                );
                continue;
            };
            let globals = globals.clone();
            let psk = psk.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let Ok(Ok(segment)) = tokio::time::timeout(
                    super::trunk_auth::TIMEOUT,
                    super::trunk_auth::server(&mut stream, &psk),
                )
                .await
                else {
                    return;
                };
                // The peer holds the PSK: it no longer counts.
                drop(slot);
                // The peer chose this name, and it names a socket file.
                if let Err(e) = check_segment_name(&segment) {
                    tracing::warn!("refusing a peer trunk: {e:#}");
                    let _ = stream.write_all(b"NO\n").await;
                    return;
                }
                // Single-slot rule: refuse up front when a trunk to/from this
                // host already exists (mutual connect, duplicate dial). The
                // atomic reserve inside accept_peer catches the race window.
                if let Ok(remote) = stream.peer_addr()
                    && !globals.peer_slot_free(&segment, remote.ip()).await
                {
                    let _ = stream.write_all(b"NO\n").await;
                    return;
                }
                if stream.write_all(b"OK\n").await.is_err() {
                    return;
                }
                match globals.accept_peer(&segment, stream).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::debug!(
                            "refused duplicate trunk for \"{segment}\" (single-slot rule)"
                        );
                    }
                    Err(e) => tracing::warn!("accepting peer trunk for {segment}: {e:#}"),
                }
            });
        }
    });
    Ok((addr, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::frame::{ETHERTYPE_IPV4, MAC_BROADCAST, eth_build};
    use crate::net::framing::{read_frame, write_frame};
    use bytes::Bytes;
    use tokio::time::{Duration, sleep, timeout};

    fn globals_without_psk(sock_dir: PathBuf) -> Arc<GlobalSegments> {
        let (events, _) = tokio::sync::broadcast::channel(64);
        GlobalSegments::new_at(sock_dir, "test.internal".into(), None, None, events)
    }

    #[tokio::test]
    async fn a_refused_attach_leaves_nothing_on_existing_segment() {
        let dir = tempfile::tempdir().unwrap();
        let globals = globals_without_psk(dir.path().join("global"));
        globals.attach("wan", None, None, None).await.unwrap();
        let lab = joining("refused", None, vec![member("vm", 1, None)]);
        assert!(
            globals
                .attach("wan", None, Some("127.0.0.1:1".into()), Some(lab))
                .await
                .is_err()
        );
        globals.detach("wan", None).await;
        assert!(
            globals.list().await.is_empty(),
            "refused attach added a reference"
        );
    }

    #[tokio::test]
    async fn a_refused_attach_leaves_nothing_on_new_segment() {
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("global");
        let globals = globals_without_psk(sock_dir.clone());
        assert!(
            globals
                .attach("wan", None, Some("127.0.0.1:1".into()), None)
                .await
                .is_err()
        );
        assert!(
            !sock_dir.join("wan.sock").exists(),
            "refused attach left a socket"
        );
        assert!(globals.list().await.is_empty());
        assert_eq!(*globals.next_index.lock().await, 0);
    }

    #[tokio::test]
    async fn a_refused_attach_leaves_nothing_when_socket_setup_fails() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wan.sock");
        std::fs::create_dir(&socket).unwrap();
        let globals = globals_without_psk(dir.path().to_path_buf());
        assert!(globals.attach("wan", None, None, None).await.is_err());
        assert!(globals.list().await.is_empty());
        assert_eq!(*globals.next_index.lock().await, 0);

        std::fs::remove_dir(socket).unwrap();
        globals.attach("wan", None, None, None).await.unwrap();
        globals.detach("wan", None).await;
    }

    /// F8 (docs/reviews/lab-lifecycle.md): a lab that never attached gives
    /// back nothing, so the segment stays for the lab that is on it.
    #[tokio::test]
    async fn a_lab_that_never_attached_cannot_destroy_a_segment() {
        let dir = tempfile::tempdir().unwrap();
        let globals = globals_without_psk(dir.path().join("global"));
        let b = joining("b", None, vec![member("vm", 1, None)]);
        globals.attach("corp", None, None, Some(b)).await.unwrap();
        globals.detach("corp", Some("a")).await;
        let list = globals.list().await;
        assert_eq!(list.len(), 1, "lab a destroyed lab b's segment");
        assert_eq!(list[0]["name"], "corp");
        assert_eq!(list[0]["refcount"], 1);
        assert_eq!(list[0]["labs"], json!(["b"]));
    }

    /// F8: a lab that attaches again (a restarted lab daemon, an adopted lab
    /// re-attaching) still holds one reference, and its one detach frees it.
    #[tokio::test]
    async fn a_second_attach_by_one_lab_is_one_reference() {
        let dir = tempfile::tempdir().unwrap();
        let globals = globals_without_psk(dir.path().join("global"));
        for _ in 0..2 {
            let a = joining("a", None, vec![member("vm", 1, None)]);
            globals.attach("corp", None, None, Some(a)).await.unwrap();
        }
        globals.detach("corp", Some("a")).await;
        assert!(globals.list().await.is_empty(), "the segment leaked");
    }

    /// F9: a lab re-attaching with the subnet it was given keeps it, and the
    /// next automatic allocation does not hand that subnet out again.
    #[tokio::test]
    async fn a_reattached_pool_subnet_is_not_allocated_again() {
        let dir = tempfile::tempdir().unwrap();
        let globals = globals_without_psk(dir.path().join("global"));
        let held: Ipv4Net = "10.214.0.0/24".parse().unwrap();
        let a = joining("a", None, Vec::new());
        globals
            .attach("corp", Some(held), None, Some(a))
            .await
            .unwrap();
        let b = joining("b", None, Vec::new());
        globals.attach("lan", None, None, Some(b)).await.unwrap();
        let list = globals.list().await;
        let subnet = |name: &str| {
            list.iter()
                .find(|s| s["name"] == name)
                .map(|s| s["subnet"].clone())
                .unwrap()
        };
        assert_eq!(subnet("corp"), "10.214.0.0/24");
        assert_eq!(subnet("lan"), "10.214.1.0/24");
    }

    struct Instance {
        globals: Arc<GlobalSegments>,
        addr: SocketAddr,
        events: tokio::sync::broadcast::Receiver<Event>,
        _dir: tempfile::TempDir,
    }

    /// A supervisor-shaped half: GlobalSegments + a live trunk listener on
    /// 127.0.0.1:0, sockets in a private tempdir.
    async fn instance(psk: &str) -> Instance {
        let dir = tempfile::tempdir().unwrap();
        let (tx, events) = tokio::sync::broadcast::channel(64);
        let globals = GlobalSegments::new_at(
            dir.path().join("global"),
            "test.internal".into(),
            None,
            Some(psk.into()),
            tx,
        );
        let (addr, _task) =
            bind_peer_listener(globals.clone(), "127.0.0.1:0".parse().unwrap(), psk.into())
                .await
                .unwrap();
        Instance {
            globals,
            addr,
            events,
            _dir: dir,
        }
    }

    /// A lab-daemon-shaped trunk client on the segment's unix socket.
    async fn trunk_client(sock: &std::path::Path) -> tokio::net::UnixStream {
        tokio::net::UnixStream::connect(sock).await.unwrap()
    }

    fn bcast(tag: &[u8]) -> Bytes {
        Bytes::from(eth_build(
            MAC_BROADCAST,
            crate::config::model::MacAddr([0x02, 0, 0, 0, 0, 0x42]),
            ETHERTYPE_IPV4,
            tag,
        ))
    }

    async fn wait_event(rx: &mut tokio::sync::broadcast::Receiver<Event>, name: &str) -> Event {
        timeout(Duration::from_secs(10), async {
            loop {
                let ev = rx.recv().await.expect("event channel open");
                if ev.event == name {
                    return ev;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {name}"))
    }

    async fn wait_connected(g: &Arc<GlobalSegments>, name: &str, want: bool) {
        for _ in 0..200 {
            let list = g.list().await;
            let got = list
                .iter()
                .find(|e| e["name"] == name)
                .map(|e| e["peer_connected"].as_bool().unwrap_or(false))
                .unwrap_or(false);
            if got == want {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
        panic!("segment {name} never reached peer_connected={want}");
    }

    /// A segment name from a trunk peer names a socket file under `sock_dir`:
    /// one that leaves the directory is refused before anything is unlinked
    /// (docs/reviews/networking.md F1).
    #[tokio::test]
    async fn a_peer_cannot_name_a_segment_outside_the_socket_directory() {
        use tokio::io::AsyncReadExt;
        let i = instance("k").await;
        let victim = i._dir.path().join("victim.sock");
        let absolute = i._dir.path().join("absolute.sock");
        std::fs::write(&victim, b"victim").unwrap();
        std::fs::write(&absolute, b"victim").unwrap();
        let absolute_name = absolute.with_extension("");
        for name in ["../victim", absolute_name.to_str().unwrap(), "..", "a/b"] {
            // The handshake carries at most 128 bytes: a long TMPDIR drops
            // the absolute case, and the others still prove the fault.
            if name.len() > 128 {
                continue;
            }
            let mut s = tokio::net::TcpStream::connect(i.addr).await.unwrap();
            crate::supervisor::trunk_auth::client(&mut s, name, "k")
                .await
                .unwrap();
            let mut answer = [0u8; 3];
            timeout(Duration::from_secs(10), s.read_exact(&mut answer))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(&answer, b"OK\n", "the peer's name {name:?} was accepted");
            assert!(
                i.globals.attach(name, None, None, None).await.is_err(),
                "attach took {name:?}"
            );
        }
        // An accepted trunk is attached after the answer: give a wrong one
        // the time to reach the file.
        sleep(Duration::from_millis(200)).await;
        for file in [&victim, &absolute] {
            assert!(
                std::fs::symlink_metadata(file)
                    .unwrap()
                    .file_type()
                    .is_file(),
                "{} was replaced",
                file.display()
            );
        }
        assert!(i.globals.list().await.is_empty());
    }

    #[test]
    fn a_segment_name_is_one_file_name() {
        for ok in ["wan", "wan@feat-x", "wan@my tree", "a.b_c", "wan@..x"] {
            assert!(check_segment_name(ok).is_ok(), "{ok:?}");
        }
        let long = "a".repeat(129);
        for bad in [
            "",
            ".",
            "..",
            "../vmlabd",
            "/run/x",
            "a/b",
            "a\0b",
            "a\nb",
            &long,
        ] {
            assert!(check_segment_name(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn two_instances_bridge_and_reconnect() {
        let mut a = instance("s3kr1t").await;
        let mut b = instance("s3kr1t").await;

        // A dials B; B's listener creates its side of the segment on accept.
        let a_sock = a
            .globals
            .attach(
                "wan",
                None,
                Some(format!("127.0.0.1:{}", b.addr.port())),
                None,
            )
            .await
            .unwrap();
        let up_a = wait_event(&mut a.events, "segment.peer.up").await;
        assert_eq!(up_a.data["segment"], "wan");
        assert_eq!(up_a.data["direction"], "dial");
        let up_b = wait_event(&mut b.events, "segment.peer.up").await;
        assert_eq!(up_b.data["direction"], "accept");
        wait_connected(&a.globals, "wan", true).await;
        wait_connected(&b.globals, "wan", true).await;
        let listed = a.globals.list().await;
        assert_eq!(listed[0]["peers"][0]["direction"], "dial");

        // L2 passthrough: a broadcast injected on A's trunk socket arrives at
        // a client on B's (and only once).
        let b_sock = {
            let segs = b.globals.segs.lock().await;
            segs.get("wan")
                .expect("accept created the segment")
                .sock
                .clone()
        };
        let mut a_client = trunk_client(&a_sock.0).await;
        let mut b_client = trunk_client(&b_sock).await;
        let f = bcast(b"hello-b");
        write_frame(&mut a_client, &f).await.unwrap();
        let got = timeout(Duration::from_secs(5), read_frame(&mut b_client))
            .await
            .expect("frame crossed the trunk")
            .unwrap()
            .expect("stream open");
        assert_eq!(got, f);
        // …and back the other way.
        let f2 = bcast(b"hello-a");
        write_frame(&mut b_client, &f2).await.unwrap();
        let got2 = timeout(Duration::from_secs(5), read_frame(&mut a_client))
            .await
            .expect("frame crossed back")
            .unwrap()
            .expect("stream open");
        assert_eq!(got2, f2);

        // Kill B's side entirely: A must emit down and then redial into the
        // fresh accept, coming back up without duplicate ports.
        b.globals.detach("wan", None).await;
        wait_event(&mut a.events, "segment.peer.down").await;
        wait_event(&mut a.events, "segment.peer.up").await;
        wait_connected(&a.globals, "wan", true).await;
        wait_connected(&b.globals, "wan", true).await;
    }

    #[tokio::test]
    async fn a_trunk_forms_again_after_the_dialer_detaches() {
        let mut a = instance("secret").await;
        let mut b = instance("secret").await;
        let target = b.addr.to_string();
        b.globals.attach("wan", None, None, None).await.unwrap();
        a.globals
            .attach("wan", None, Some(target.clone()), None)
            .await
            .unwrap();
        assert_eq!(a.events.recv().await.unwrap().event, "segment.peer.up");
        assert_eq!(b.events.recv().await.unwrap().event, "segment.peer.up");

        let (old_switch, dialer, gateway_sender) = {
            let segs = a.globals.segs.lock().await;
            let seg = &segs["wan"];
            (
                seg.switch.clone(),
                seg.dialer.as_ref().unwrap().abort_handle(),
                seg.gateway.sender(),
            )
        };
        assert_eq!(old_switch.stats().ports, 2);
        a.globals.detach("wan", None).await;
        while !dialer.is_finished() {
            tokio::task::yield_now().await;
        }
        // Keep the gateway port registered so the count isolates the TCP port.
        assert_eq!(
            old_switch.stats().ports,
            1,
            "dialed trunk port survived detach"
        );
        drop(gateway_sender);
        assert_eq!(a.events.recv().await.unwrap().event, "segment.peer.down");
        assert_eq!(b.events.recv().await.unwrap().event, "segment.peer.down");

        let (a_sock, _, _) = a
            .globals
            .attach("wan", None, Some(target), None)
            .await
            .unwrap();
        assert_eq!(a.events.recv().await.unwrap().event, "segment.peer.up");
        assert_eq!(b.events.recv().await.unwrap().event, "segment.peer.up");

        // Register B's test port before sending so the frame cannot pass it by.
        let b_switch = b.globals.segs.lock().await["wan"].switch.clone();
        let mut b_port = b_switch.add_channel_port(PortClass::Service);
        let mut a_client = trunk_client(&a_sock).await;
        let frame = bcast(b"after reattach");
        write_frame(&mut a_client, &frame).await.unwrap();
        assert_eq!(b_port.rx.recv().await.unwrap(), frame);
    }

    #[tokio::test]
    async fn mutual_dial_converges_to_one_trunk() {
        let mut a = instance("s3kr1t").await;
        let mut b = instance("s3kr1t").await;

        // Both sides declare connect at each other — exactly what the web
        // UI's remote-vmlab node writes on both canvases.
        let a_sock = a
            .globals
            .attach(
                "wan",
                None,
                Some(format!("127.0.0.1:{}", b.addr.port())),
                None,
            )
            .await
            .unwrap();
        let _ = b
            .globals
            .attach(
                "wan",
                None,
                Some(format!("127.0.0.1:{}", a.addr.port())),
                None,
            )
            .await
            .unwrap();
        wait_event(&mut a.events, "segment.peer.up").await;
        wait_event(&mut b.events, "segment.peer.up").await;
        // Let the losing dialer take a few more swings at the storm guard.
        sleep(Duration::from_millis(600)).await;

        // Exactly one active trunk per side.
        for g in [&a.globals, &b.globals] {
            let list = g.list().await;
            let peers = list[0]["peers"].as_array().unwrap();
            assert_eq!(peers.len(), 1, "single-slot rule violated: {list:?}");
        }

        // The storm test: a broadcast from A arrives at B exactly once (two
        // parallel trunks would loop it forever).
        let b_sock = {
            let segs = b.globals.segs.lock().await;
            segs.get("wan").unwrap().sock.clone()
        };
        let mut a_client = trunk_client(&a_sock.0).await;
        let mut b_client = trunk_client(&b_sock).await;
        let f = bcast(b"once");
        write_frame(&mut a_client, &f).await.unwrap();
        let got = timeout(Duration::from_secs(5), read_frame(&mut b_client))
            .await
            .expect("broadcast crossed")
            .unwrap()
            .expect("stream open");
        assert_eq!(got, f);
        let extra = timeout(Duration::from_millis(400), read_frame(&mut b_client)).await;
        assert!(
            extra.is_err(),
            "broadcast duplicated across parallel trunks"
        );
    }

    #[tokio::test]
    async fn bad_psk_rejected() {
        let mut a = instance("right").await;
        let b = instance("wrong").await;
        let _ = a
            .globals
            .attach(
                "wan",
                None,
                Some(format!("127.0.0.1:{}", b.addr.port())),
                None,
            )
            .await
            .unwrap();
        // The dial must never come up; give it a few retry rounds.
        let up = timeout(Duration::from_millis(800), async {
            wait_event(&mut a.events, "segment.peer.up").await
        })
        .await;
        assert!(up.is_err(), "trunk came up despite a PSK mismatch");
        let list = b.globals.list().await;
        assert!(list.is_empty(), "rejected peer still created the segment");
    }

    #[tokio::test]
    async fn silent_trunk_peer_is_disconnected_after_handshake_timeout() {
        use tokio::io::AsyncReadExt;
        let host = instance("secret").await;
        let mut stream = tokio::net::TcpStream::connect(host.addr).await.unwrap();
        let mut greeting = [0; 44];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting[..12], b"VMLABTRUNK2\n");
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(7), stream.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(host.globals.list().await.is_empty());
    }

    // ---- the shared segment's DNS (PRD §9.2, §9.5) ---------------------------

    fn member(machine: &str, last: u8, ip: Option<[u8; 4]>) -> GlobalMember {
        GlobalMember {
            machine: machine.into(),
            mac: MacAddr([0x02, 0, 0, 0, 0, last]),
            ip: ip.map(Ipv4Addr::from),
        }
    }

    async fn lookup(g: &Arc<GlobalSegments>, seg: &str, name: &str) -> crate::net::dns::DnsAnswer {
        let segs = g.segs.lock().await;
        let zone = segs[seg].gateway.dns_zone().expect("zone");
        zone.lock().expect("zone lock").lookup(name)
    }

    /// Every lab on a global segment is named there — `<vm>.<lab>` always,
    /// the short `<vm>` only while no other lab has a machine of that name —
    /// and a lab's names leave with it.
    #[tokio::test]
    async fn a_global_segment_names_every_attached_labs_machines() {
        use crate::net::dns::DnsAnswer;
        let dir = tempfile::tempdir().unwrap();
        let (tx, _events) = tokio::sync::broadcast::channel(8);
        let g = GlobalSegments::new_at(dir.path().into(), "test.internal".into(), None, None, tx);
        let subnet = Some("10.82.9.0/24".parse().unwrap());
        g.attach(
            "wan",
            subnet,
            None,
            Some(Joining {
                lab: "lab-a".into(),
                mtu: None,
                members: vec![
                    member("ga", 1, Some([10, 82, 9, 10])),
                    member("web", 2, Some([10, 82, 9, 11])),
                ],
            }),
        )
        .await
        .unwrap();
        g.attach(
            "wan",
            subnet,
            None,
            Some(Joining {
                lab: "lab-b".into(),
                mtu: None,
                members: vec![
                    member("gb", 3, Some([10, 82, 9, 20])),
                    member("web", 4, Some([10, 82, 9, 21])),
                ],
            }),
        )
        .await
        .unwrap();

        let a = |ip: [u8; 4]| DnsAnswer::A(Ipv4Addr::from(ip));
        assert_eq!(
            lookup(&g, "wan", "gb.lab-b.test.internal").await,
            a([10, 82, 9, 20])
        );
        assert_eq!(
            lookup(&g, "wan", "gb.test.internal").await,
            a([10, 82, 9, 20])
        );
        assert_eq!(
            lookup(&g, "wan", "ga.test.internal").await,
            a([10, 82, 9, 10])
        );
        assert_eq!(
            lookup(&g, "wan", "web.lab-a.test.internal").await,
            a([10, 82, 9, 11])
        );
        assert_eq!(
            lookup(&g, "wan", "web.lab-b.test.internal").await,
            a([10, 82, 9, 21])
        );
        assert_eq!(
            lookup(&g, "wan", "web.test.internal").await,
            DnsAnswer::Nxdomain,
            "two labs have a \"web\": the short name is ambiguous"
        );

        g.detach("wan", Some("lab-b")).await;
        assert_eq!(
            lookup(&g, "wan", "gb.lab-b.test.internal").await,
            DnsAnswer::Nxdomain
        );
        assert_eq!(
            lookup(&g, "wan", "gb.test.internal").await,
            DnsAnswer::Nxdomain
        );
        assert_eq!(
            lookup(&g, "wan", "web.test.internal").await,
            a([10, 82, 9, 11]),
            "unambiguous again once lab-b left"
        );
    }

    /// A lease wins over a declared address, and a machine with neither is
    /// not named at all.
    /// A worktree lab's segment name still makes a domain (§21.4).
    #[test]
    fn a_worktree_segment_name_makes_a_domain_label() {
        assert_eq!(domain_label("corp"), "corp");
        assert_eq!(domain_label("corp@feat-x"), "corp-feat-x");
        assert_eq!(domain_label("corp@my tree"), "corp-my-tree");
    }

    #[test]
    fn a_lease_names_a_machine() {
        let mut dir = Directory::default();
        dir.members.insert(
            "lab".into(),
            vec![
                member("dyn", 1, None),
                member("idle", 2, None),
                member("st", 3, Some([10, 0, 0, 3])),
            ],
        );
        let leases = HashMap::from([
            (MacAddr([0x02, 0, 0, 0, 0, 1]), Ipv4Addr::new(10, 0, 0, 50)),
            (MacAddr([0x02, 0, 0, 0, 0, 3]), Ipv4Addr::new(10, 0, 0, 51)),
        ]);
        let names = dir.names(&leases);
        assert_eq!(names["dyn.lab"], Ipv4Addr::new(10, 0, 0, 50));
        assert_eq!(names["dyn"], Ipv4Addr::new(10, 0, 0, 50));
        assert_eq!(names["st.lab"], Ipv4Addr::new(10, 0, 0, 51));
        assert!(!names.contains_key("idle.lab"));
    }

    fn joining(lab: &str, mtu: Option<u16>, members: Vec<GlobalMember>) -> Joining {
        Joining {
            lab: lab.into(),
            members,
            mtu,
        }
    }

    /// Every attached lab's static addresses are the segment's DHCP
    /// reservations while it stays, and the first lab's MTU is what the
    /// segment serves (PRD §9.2, §9.4).
    #[tokio::test]
    async fn a_global_segment_serves_static_addresses_and_the_first_labs_mtu() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _events) = tokio::sync::broadcast::channel(8);
        let g = GlobalSegments::new_at(dir.path().into(), "test.internal".into(), None, None, tx);
        let subnet = Some("10.82.9.0/24".parse().unwrap());
        let (_, warnings, _) = g
            .attach(
                "wan",
                subnet,
                None,
                Some(joining(
                    "lab-a",
                    Some(1400),
                    vec![
                        member("ga", 1, Some([10, 82, 9, 50])),
                        member("dyn", 2, None),
                    ],
                )),
            )
            .await
            .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let (_, warnings, _) = g
            .attach(
                "wan",
                subnet,
                None,
                Some(joining(
                    "lab-b",
                    Some(1400),
                    vec![member("gb", 3, Some([10, 82, 9, 60]))],
                )),
            )
            .await
            .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");

        let config = |g: Arc<GlobalSegments>| async move {
            let segs = g.segs.lock().await;
            segs["wan"]
                .gateway
                .dhcp_config()
                .expect("global segments serve DHCP")
        };
        let c = config(g.clone()).await;
        assert_eq!(c.mtu, 1400);
        assert_eq!(
            c.reservations,
            HashMap::from([
                (MacAddr([0x02, 0, 0, 0, 0, 1]), Ipv4Addr::new(10, 82, 9, 50)),
                (MacAddr([0x02, 0, 0, 0, 0, 3]), Ipv4Addr::new(10, 82, 9, 60)),
            ])
        );
        assert_eq!(g.list().await[0]["mtu"], 1400);

        g.detach("wan", Some("lab-b")).await;
        let c = config(g.clone()).await;
        assert_eq!(
            c.reservations,
            HashMap::from([(MacAddr([0x02, 0, 0, 0, 0, 1]), Ipv4Addr::new(10, 82, 9, 50))]),
            "lab-b's reservation leaves with it"
        );
        assert_eq!(c.mtu, 1400, "the MTU stays for the segment's life");
    }

    /// A static address that cannot be reserved is refused by name, and that
    /// NIC falls back to a lease; the lab still joins. A lab re-attaching is
    /// not in conflict with itself.
    #[test]
    fn a_conflicting_static_address_is_refused_with_a_warning() {
        let subnet: Ipv4Net = "10.82.9.0/24".parse().unwrap();
        let gw = Ipv4Addr::new(10, 82, 9, 1);
        let mut dir = Directory::default();
        let a = || joining("lab-a", None, vec![member("ga", 1, Some([10, 82, 9, 50]))]);
        assert!(dir.admit("wan", subnet, gw, a(), &[]).is_empty());
        assert!(
            dir.admit("wan", subnet, gw, a(), &[]).is_empty(),
            "re-attach"
        );

        let leases = [
            (MacAddr([0x02, 0, 0, 0, 0, 1]), Ipv4Addr::new(10, 82, 9, 50)),
            (MacAddr([0x02, 0, 0, 0, 0, 9]), Ipv4Addr::new(10, 82, 9, 70)),
        ];
        let warnings = dir.admit(
            "wan",
            subnet,
            gw,
            joining(
                "lab-b",
                None,
                vec![
                    member("taken", 2, Some([10, 82, 9, 50])),
                    member("gw", 3, Some([10, 82, 9, 1])),
                    member("outside", 4, Some([10, 99, 0, 5])),
                    member("leased", 5, Some([10, 82, 9, 70])),
                    member("fine", 6, Some([10, 82, 9, 80])),
                ],
            ),
            &leases,
        );
        assert_eq!(warnings.len(), 4, "{warnings:#?}");
        assert!(
            warnings[0].contains("taken.lab-b declares ip 10.82.9.50, but ga.lab-a already has it"),
            "{}",
            warnings[0]
        );
        assert!(
            warnings[1].contains("gw.lab-b") && warnings[1].contains("not a host address"),
            "{}",
            warnings[1]
        );
        assert!(
            warnings[2].contains("outside.lab-b") && warnings[2].contains("10.82.9.0/24"),
            "{}",
            warnings[2]
        );
        assert!(
            warnings[3].contains("leased.lab-b")
                && warnings[3].contains("02:00:00:00:00:09 holds it on a lease"),
            "{}",
            warnings[3]
        );
        assert!(warnings.iter().all(|w| w.contains("dynamic lease instead")));
        assert_eq!(
            dir.reservations(),
            HashMap::from([
                (MacAddr([0x02, 0, 0, 0, 0, 1]), Ipv4Addr::new(10, 82, 9, 50)),
                (MacAddr([0x02, 0, 0, 0, 0, 6]), Ipv4Addr::new(10, 82, 9, 80)),
            ])
        );
    }

    /// The first lab to attach with an MTU sets the segment's; a later lab
    /// that differs is told, and changes nothing.
    #[test]
    fn a_differing_mtu_is_warned_about_and_the_first_wins() {
        let subnet: Ipv4Net = "10.82.9.0/24".parse().unwrap();
        let gw = Ipv4Addr::new(10, 82, 9, 1);
        let mut dir = Directory::default();
        assert_eq!(dir.mtu(), 1500);
        assert!(
            dir.admit("wan", subnet, gw, joining("trunk", None, vec![]), &[])
                .is_empty()
        );
        assert!(
            dir.admit("wan", subnet, gw, joining("lab-a", Some(9000), vec![]), &[])
                .is_empty()
        );
        assert!(
            dir.admit("wan", subnet, gw, joining("lab-b", Some(9000), vec![]), &[])
                .is_empty()
        );
        let warnings = dir.admit("wan", subnet, gw, joining("lab-c", Some(1500), vec![]), &[]);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("serves mtu 9000, set by lab \"lab-a\"")
                && warnings[0].contains("this lab's NICs use 1500"),
            "{}",
            warnings[0]
        );
        assert_eq!(dir.mtu(), 9000);
    }

    /// Names no attached lab answers to go upstream, as on a lab segment.
    #[tokio::test]
    async fn a_global_segment_forwards_what_it_cannot_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _events) = tokio::sync::broadcast::channel(8);
        let g = GlobalSegments::new_at(
            dir.path().into(),
            "test.internal".into(),
            Some("192.0.2.53:53".parse().unwrap()),
            None,
            tx,
        );
        g.attach("wan", None, None, None).await.unwrap();
        assert_eq!(
            lookup(&g, "wan", "example.com").await,
            crate::net::dns::DnsAnswer::Forward
        );
    }

    // ---- the listener before authentication (docs/reviews/networking.md F11) ----

    /// A TCP connection to the trunk listener from a chosen loopback address,
    /// so one test can be several peers.
    async fn connect_from(from: [u8; 4], to: SocketAddr) -> tokio::net::TcpStream {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind((Ipv4Addr::from(from), 0).into()).unwrap();
        socket.connect(to).await.unwrap()
    }

    /// Whether the listener started a handshake on this connection: it sends
    /// its greeting first. A connection it refused is closed without one.
    async fn greeted(stream: &mut tokio::net::TcpStream, wait: Duration) -> bool {
        use tokio::io::AsyncReadExt;
        let mut magic = [0u8; 12];
        matches!(
            timeout(wait, stream.read_exact(&mut magic)).await,
            Ok(Ok(_)) if &magic == crate::supervisor::trunk_auth::MAGIC
        )
    }

    /// One address that opens connections and says nothing holds its own
    /// share of the listener and no more: the next one from it is closed at
    /// once, and another address is still served.
    #[tokio::test]
    async fn a_silent_address_holds_only_its_share_of_the_listener() {
        let i = instance("k").await;
        let mut held = Vec::new();
        for n in 0..PREAUTH_MAX_PER_ADDR {
            let mut s = connect_from([127, 0, 0, 1], i.addr).await;
            assert!(
                greeted(&mut s, Duration::from_secs(2)).await,
                "connection {n} is inside the share"
            );
            held.push(s);
        }
        let mut over = connect_from([127, 0, 0, 1], i.addr).await;
        assert!(
            !greeted(&mut over, Duration::from_secs(2)).await,
            "a connection past the address's share is held"
        );
        let mut other = connect_from([127, 0, 0, 2], i.addr).await;
        assert!(
            greeted(&mut other, Duration::from_secs(2)).await,
            "one silent address shut another address out"
        );
        // A slot comes back when its connection ends.
        held.pop();
        let mut again = false;
        for _ in 0..80 {
            let mut s = connect_from([127, 0, 0, 1], i.addr).await;
            if greeted(&mut s, Duration::from_millis(200)).await {
                again = true;
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        assert!(again, "a closed connection never gave its slot back");
    }

    /// Many addresses together hold a limited number of connections that
    /// have proved nothing.
    #[tokio::test]
    async fn silent_connections_are_limited_in_total() {
        let i = instance("k").await;
        let mut held = Vec::new();
        for n in 0..PREAUTH_MAX {
            let from = [127, 0, 1, 1 + (n / PREAUTH_MAX_PER_ADDR) as u8];
            let mut s = connect_from(from, i.addr).await;
            assert!(
                greeted(&mut s, Duration::from_secs(2)).await,
                "connection {n} is inside the limit"
            );
            held.push(s);
        }
        let mut over = connect_from([127, 0, 2, 1], i.addr).await;
        assert!(
            !greeted(&mut over, Duration::from_secs(2)).await,
            "a connection past the total limit is held"
        );
    }

    /// A handshake that finished no longer counts: more trunks than the
    /// per-address share authenticate one after another from one address.
    #[tokio::test]
    async fn an_authenticated_connection_gives_its_slot_back() {
        use tokio::io::AsyncReadExt;
        let i = instance("k").await;
        let mut kept = Vec::new();
        for n in 0..PREAUTH_MAX_PER_ADDR + 2 {
            let mut s = connect_from([127, 0, 0, 1], i.addr).await;
            // A name the listener refuses after authentication, so that no
            // segment is made: the answer is `NO`, and it is an answer.
            crate::supervisor::trunk_auth::client(&mut s, "a/b", "k")
                .await
                .unwrap_or_else(|e| panic!("handshake {n}: {e:#}"));
            let mut answer = [0u8; 3];
            s.read_exact(&mut answer).await.unwrap();
            assert_eq!(&answer, b"NO\n");
            kept.push(s);
        }
    }

    const LISTENER_CHILD: &str = "VMLAB_TEST_TRUNK_LISTENER_CHILD";

    /// The other half of the two descriptor-limit tests: a process whose
    /// descriptor limit the parent set, serving the trunk listener. It prints
    /// the address and stays.
    #[tokio::test]
    async fn trunk_listener_child() {
        let Ok(mode) = std::env::var(LISTENER_CHILD) else {
            return;
        };
        let i = instance("k").await;
        // "full": leave three free descriptors, so the listener itself meets
        // the limit however few connections it holds.
        let mut filler = Vec::new();
        if mode == "full" {
            while let Ok(f) = std::fs::File::open("/dev/null") {
                filler.push(f);
            }
            filler.truncate(filler.len().saturating_sub(3));
        }
        // libtest has written `test <name> ... ` on this line already.
        println!("\nTRUNK-LISTENER {}", i.addr);
        sleep(Duration::from_secs(60)).await;
    }

    /// Start [`trunk_listener_child`] with a descriptor limit of 64, open 80
    /// connections to it that send nothing, and give back the child, its
    /// address, the connections, and its CPU use over one second as a percent
    /// of one core.
    async fn besiege_listener_child(
        mode: &str,
    ) -> (
        tokio::process::Child,
        SocketAddr,
        Vec<tokio::net::TcpStream>,
        f64,
    ) {
        use tokio::io::AsyncBufReadExt;
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", r#"ulimit -n 64 && exec "$0" "$@""#])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "supervisor::global::tests::trunk_listener_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(LISTENER_CHILD, mode)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let addr: SocketAddr = timeout(Duration::from_secs(30), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("the child ended before it listened");
                if let Some(addr) = line.strip_prefix("TRUNK-LISTENER ") {
                    return addr.parse().unwrap();
                }
            }
        })
        .await
        .expect("the child never listened");
        let mut silent = Vec::new();
        for _ in 0..80 {
            silent.push(connect_from([127, 0, 0, 1], addr).await);
        }
        sleep(Duration::from_millis(300)).await;
        let cpu = crate::labd::host_cpu::sample(child.id().unwrap())
            .await
            .expect("the child is there to measure");
        (child, addr, silent, cpu)
    }

    /// 80 silent connections against a process that may hold 64 descriptors:
    /// the listener stays under the limit, idle, and serves another address
    /// while the 80 are still open.
    #[tokio::test]
    async fn silent_connections_do_not_take_the_process_to_its_descriptor_limit() {
        let (_child, addr, _silent, cpu) = besiege_listener_child("limit").await;
        assert!(
            cpu < 10.0,
            "the listener used {cpu:.0}% of a core for one second"
        );
        let mut other = connect_from([127, 0, 0, 2], addr).await;
        assert!(
            greeted(&mut other, Duration::from_secs(3)).await,
            "silent connections shut another address out"
        );
    }

    /// A process that is at its descriptor limit for another reason: `accept`
    /// fails at once and the listener waits instead of spinning, and it
    /// serves again when descriptors come back.
    #[tokio::test]
    async fn a_listener_at_the_descriptor_limit_waits() {
        let (_child, addr, silent, cpu) = besiege_listener_child("full").await;
        assert!(
            cpu < 10.0,
            "the listener used {cpu:.0}% of a core for one second"
        );
        drop(silent);
        let mut later = connect_from([127, 0, 0, 2], addr).await;
        assert!(
            greeted(&mut later, Duration::from_secs(20)).await,
            "the listener never served again"
        );
    }
}
