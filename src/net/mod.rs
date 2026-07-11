//! The userspace network fabric (PRD §9).
//!
//! - [`frame`]: ethernet/ARP/IPv4/UDP/TCP/ICMP views and builders.
//! - [`framing`]: the 4-byte big-endian length framing QEMU uses for
//!   stream-socket netdevs.
//! - [`switch`]: the per-segment MAC-learning L2 switch with port isolation
//!   and the ingress-hook seam for L3 rules.
//! - [`fastpath`]: opt-in kernel acceleration tiers (eBPF sockmap/XDP) with
//!   empirical probing and silent fallback to the userspace switch.

pub mod dhcp;
pub mod dns;
pub mod fastpath;
pub mod frame;
pub mod framing;
pub mod gateway;
pub mod nat;
pub mod rules;
pub mod switch;
