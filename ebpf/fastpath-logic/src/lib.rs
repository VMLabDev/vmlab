//! Pure fast-path decision logic, shared by the BPF programs beside this
//! crate and unit-tested with a plain host `cargo test`. `no_std`, no aya
//! dependencies — every subtle decision (the stream-framing boundary state
//! machine, the redirect verdicts) lives here as functions over bytes and
//! callbacks, so the BPF crates stay thin unsafe shims.
//!
//! # Why a state machine at all
//!
//! QEMU stream netdevs speak 4-byte big-endian length-prefixed ethernet
//! frames over a byte stream. AF_UNIX sockets in a sockmap have no strparser
//! (the kernel gates it on TCP), so the verdict program runs once per skb
//! and must decide *itself* whether an skb is exactly one whole frame — only
//! those may be redirected in-kernel; everything else passes to the
//! userspace switch untouched. One send from QEMU is usually one skb holding
//! one frame, but large frames split (~16 KiB skb cap) and nothing forbids a
//! sender coalescing several frames into one write, so the per-socket state
//! tracks where the next frame boundary lies. Losing track is safe: the
//! sticky desync flag routes everything to userspace from then on.

#![cfg_attr(not(test), no_std)]

/// The 4-byte big-endian length prefix on every frame (net/framing.rs).
pub const PREFIX_LEN: u32 = 4;
/// Minimum parsable ethernet frame: dst + src + ethertype.
pub const ETH_HEADER_LEN: u32 = 14;
/// Largest frame body the framing layer accepts (net/framing.rs
/// `MAX_FRAME_LEN` minus the prefix): 64 KiB payload + ethernet header.
pub const MAX_FRAME_LEN: u32 = 65536 + 14;
/// Frame boundaries walked within a single skb before giving up. More
/// frames per skb than this means a pathological sender; desync (= pass
/// everything to userspace) rather than loop further.
pub const MAX_BOUNDARIES_PER_SKB: u32 = 16;

/// `FrameState::flags`: the stream lost framing sync — sticky pass-all.
pub const FLAG_DESYNC: u8 = 1;

/// Per-socket framing-boundary state (embedded in the BPF `SOCK_STATE` map
/// value). All fields little-endian host order; only ever touched by one
/// verdict invocation at a time (per-socket skb processing is serialized).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameState {
    /// Bytes left of a frame whose start passed in an earlier skb.
    pub remaining: u32,
    /// Partially accumulated length prefix (valid bytes: `prefix_have`).
    pub prefix_buf: [u8; 4],
    /// How many of `prefix_buf` are filled (0..=3 between skbs).
    pub prefix_have: u8,
    /// `FLAG_*` bits.
    pub flags: u8,
    pub _pad: [u8; 2],
}

/// What one skb turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkbClass {
    /// Exactly one whole frame: prefix at offset 0, ethernet header at
    /// [`PREFIX_LEN`], frame body of `frame_len` bytes ending exactly at the
    /// skb's end. The only redirect candidate.
    Aligned { frame_len: u32 },
    /// Consumed by the state machine (continuation bytes, several frames,
    /// or a partial frame); pass to userspace, which reads the byte stream
    /// exactly as before.
    Passthrough,
    /// Framing sync lost (oversized length or boundary-walk budget blown);
    /// the desync flag is now set and everything passes from here on.
    Desync,
}

/// Advance the boundary state machine across one skb of `len` bytes.
///
/// `read` must copy skb bytes `[offset, offset + dst.len())` into `dst`,
/// returning `false` on failure; it is only called with in-bounds ranges.
/// The state is updated to describe the stream position after this skb —
/// including for [`SkbClass::Aligned`], where the position lands back on a
/// boundary whether or not the caller ends up redirecting the skb.
pub fn step_skb(
    st: &mut FrameState,
    len: u32,
    read: &mut impl FnMut(u32, &mut [u8]) -> bool,
) -> SkbClass {
    if st.flags & FLAG_DESYNC != 0 {
        return SkbClass::Passthrough;
    }

    let mut off: u32 = 0;
    if st.remaining > 0 {
        // Continuation of a frame from an earlier skb.
        if len <= st.remaining {
            st.remaining -= len;
            return SkbClass::Passthrough;
        }
        off = st.remaining;
        st.remaining = 0;
    }

    // Fast path: at a clean boundary and the skb is exactly one frame.
    if off == 0 && st.prefix_have == 0 && len > PREFIX_LEN {
        let mut b = [0u8; 4];
        if !read(0, &mut b) {
            st.flags |= FLAG_DESYNC;
            return SkbClass::Desync;
        }
        let flen = u32::from_be_bytes(b);
        if flen > MAX_FRAME_LEN {
            st.flags |= FLAG_DESYNC;
            return SkbClass::Desync;
        }
        if flen >= ETH_HEADER_LEN && flen == len - PREFIX_LEN {
            return SkbClass::Aligned { frame_len: flen };
        }
    }

    walk_boundaries(st, len, off, read)
}

/// Walk frame boundaries from `off` to the skb's end, leaving the state at
/// the stream position after the skb. Never a redirect: this path only
/// exists to keep the boundary bookkeeping true while userspace consumes
/// the bytes.
fn walk_boundaries(
    st: &mut FrameState,
    len: u32,
    mut off: u32,
    read: &mut impl FnMut(u32, &mut [u8]) -> bool,
) -> SkbClass {
    let mut walked: u32 = 0;
    while walked < MAX_BOUNDARIES_PER_SKB {
        walked += 1;
        if off >= len {
            // Ended exactly on a boundary.
            return SkbClass::Passthrough;
        }
        // Accumulate length-prefix bytes (possibly resuming a partial
        // prefix from the previous skb). The `& 3` is redundant with the
        // invariant (prefix_have < 4 here) but keeps the BPF verifier able
        // to bound the slice below.
        let start = (st.prefix_have & 3) as u32;
        let need = PREFIX_LEN - start;
        let avail = len - off;
        let take = if need < avail { need } else { avail };
        if !read(
            off,
            &mut st.prefix_buf[start as usize..(start + take) as usize],
        ) {
            st.flags |= FLAG_DESYNC;
            return SkbClass::Desync;
        }
        st.prefix_have = (start + take) as u8;
        off += take;
        if u32::from(st.prefix_have) < PREFIX_LEN {
            // skb ended mid-prefix; resume in the next one.
            return SkbClass::Passthrough;
        }
        let flen = u32::from_be_bytes(st.prefix_buf);
        st.prefix_have = 0;
        if flen > MAX_FRAME_LEN {
            // Not a plausible length — the stream is not speaking the
            // framing protocol (userspace read_frame would error out too).
            st.flags |= FLAG_DESYNC;
            return SkbClass::Desync;
        }
        let rest = len - off;
        if flen >= rest {
            st.remaining = flen - rest;
            return SkbClass::Passthrough;
        }
        off += flen;
    }
    st.flags |= FLAG_DESYNC;
    SkbClass::Desync
}

/// Forwarding decision for one [`SkbClass::Aligned`] frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameVerdict {
    /// Redirect the skb out of the destination port's socket.
    Redirect { dst_port: u64 },
    /// Let the userspace switch handle it.
    Pass,
}

/// Redirect only when both MACs are known offloaded ports and the
/// destination is a *different* port. Everything else — broadcast/multicast,
/// unknown or moved source (userspace must observe it to learn), unknown
/// destination (includes the gateway MAC, isolated ports, service ports:
/// none of those are ever in the MAC map), self-addressed — passes to the
/// userspace switch, which keeps today's semantics.
pub fn classify_frame(
    dst_mac: &[u8; 6],
    src_mac: &[u8; 6],
    ingress_port: u64,
    lookup: impl Fn(&[u8; 6]) -> Option<u64>,
) -> FrameVerdict {
    if dst_mac[0] & 0x01 != 0 {
        return FrameVerdict::Pass;
    }
    match lookup(src_mac) {
        Some(p) if p == ingress_port => {}
        _ => return FrameVerdict::Pass,
    }
    match lookup(dst_mac) {
        Some(p) if p != ingress_port => FrameVerdict::Redirect { dst_port: p },
        _ => FrameVerdict::Pass,
    }
}

// --- afxdp tier -----------------------------------------------------------

/// `PortConf::flags`: every ingress frame punts to the daemon (isolation is
/// enforced by the userspace switch's delivery matrix).
pub const XDP_FLAG_ISOLATED: u32 = 1;
/// `PortConf::flags`: this is the daemon's host tap — ingress carries
/// `[4-byte BE tag][frame]` to inject toward the tagged guest tap.
pub const XDP_FLAG_HOST: u32 = 2;
/// The host tap's fixed slot in the `PORTS` devmap.
pub const HOST_TAG: u32 = 0;
/// The punt/inject header: one big-endian `u32` port tag.
pub const TAG_LEN: u32 = 4;

/// Forwarding decision for one frame arriving on a guest tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdpVerdict {
    /// In-kernel devmap redirect to the destination tap.
    ForwardTo { tag: u32 },
    /// Prepend this port's tag and redirect to the host tap (the daemon
    /// feeds it into the userspace switch).
    Punt,
    /// Self-addressed known unicast: drop, matching the userspace switch's
    /// never-echo rule.
    Drop,
}

/// Guest-tap ingress: forward known non-isolated unicast between taps;
/// punt everything with semantic weight (broadcast/multicast, unknown or
/// gateway-bound destinations, isolated ports) to the daemon.
pub fn classify_xdp_guest(
    conf_flags: u32,
    my_tag: u32,
    dst_mac: &[u8; 6],
    lookup: impl Fn(&[u8; 6]) -> Option<u32>,
) -> XdpVerdict {
    if conf_flags & XDP_FLAG_ISOLATED == 0 && dst_mac[0] & 0x01 == 0 {
        if let Some(tag) = lookup(dst_mac) {
            return if tag == my_tag {
                XdpVerdict::Drop
            } else {
                XdpVerdict::ForwardTo { tag }
            };
        }
    }
    XdpVerdict::Punt
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Back on a frame boundary with no residue — `prefix_buf` may hold
    /// stale bytes (only `prefix_have` gives them meaning), so full struct
    /// equality would be over-strict.
    fn in_sync(st: &FrameState) -> bool {
        st.remaining == 0 && st.prefix_have == 0 && st.flags == 0
    }

    /// Drive `step_skb` over a byte buffer standing in for one skb.
    fn step(st: &mut FrameState, skb: &[u8]) -> SkbClass {
        let len = skb.len() as u32;
        step_skb(st, len, &mut |off, dst| {
            let off = off as usize;
            if off + dst.len() > skb.len() {
                return false;
            }
            dst.copy_from_slice(&skb[off..off + dst.len()]);
            true
        })
    }

    /// One length-prefixed frame of `body_len` bytes.
    fn frame(body_len: usize) -> Vec<u8> {
        let mut v = (body_len as u32).to_be_bytes().to_vec();
        v.extend(std::iter::repeat_n(0xAB, body_len));
        v
    }

    #[test]
    fn aligned_single_frame() {
        let mut st = FrameState::default();
        assert_eq!(
            step(&mut st, &frame(1500)),
            SkbClass::Aligned { frame_len: 1500 }
        );
        // State stays at a boundary whether or not the caller redirects.
        assert!(in_sync(&st));
        // And the next frame is aligned again.
        assert_eq!(
            step(&mut st, &frame(14)),
            SkbClass::Aligned { frame_len: 14 }
        );
    }

    #[test]
    fn runt_frame_passes_without_desync() {
        // A sub-ethernet frame is legal on the wire; it must pass (never
        // redirect — no MACs to read) but keep the stream in sync.
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &frame(6)), SkbClass::Passthrough);
        assert!(in_sync(&st));
        assert_eq!(
            step(&mut st, &frame(60)),
            SkbClass::Aligned { frame_len: 60 }
        );
    }

    #[test]
    fn split_jumbo_across_skbs() {
        // A 9000-byte frame arriving as 3 skbs: header+start, middle, tail.
        let f = frame(9000);
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &f[..4000]), SkbClass::Passthrough);
        assert_eq!(st.remaining, 9004 - 4000);
        assert_eq!(step(&mut st, &f[4000..8000]), SkbClass::Passthrough);
        assert_eq!(st.remaining, 9004 - 8000);
        assert_eq!(step(&mut st, &f[8000..]), SkbClass::Passthrough);
        assert!(in_sync(&st));
        // Back in sync: next whole frame is aligned.
        assert_eq!(
            step(&mut st, &frame(100)),
            SkbClass::Aligned { frame_len: 100 }
        );
    }

    #[test]
    fn tail_plus_next_frame_in_one_skb() {
        // skb = [tail of frame A][whole frame B]: consumed, in sync after.
        let a = frame(2000);
        let b = frame(300);
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &a[..1000]), SkbClass::Passthrough);
        let mut skb = a[1000..].to_vec();
        skb.extend_from_slice(&b);
        assert_eq!(step(&mut st, &skb), SkbClass::Passthrough);
        assert!(in_sync(&st));
    }

    #[test]
    fn several_frames_in_one_skb() {
        let mut skb = Vec::new();
        for len in [64, 128, 256] {
            skb.extend_from_slice(&frame(len));
        }
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &skb), SkbClass::Passthrough);
        assert!(in_sync(&st));
    }

    #[test]
    fn split_prefix_resumes() {
        // The 4-byte length prefix itself split across two skbs.
        let f = frame(500);
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &f[..2]), SkbClass::Passthrough);
        assert_eq!(st.prefix_have, 2);
        assert_eq!(step(&mut st, &f[2..]), SkbClass::Passthrough);
        assert!(in_sync(&st));
    }

    #[test]
    fn prefix_split_at_every_byte() {
        for cut in 1..4 {
            let f = frame(64);
            let mut st = FrameState::default();
            assert_eq!(step(&mut st, &f[..cut]), SkbClass::Passthrough);
            assert_eq!(step(&mut st, &f[cut..]), SkbClass::Passthrough);
            assert!(in_sync(&st), "cut at {cut}");
        }
    }

    #[test]
    fn oversized_length_desyncs_stickily() {
        let mut skb = ((MAX_FRAME_LEN + 1).to_be_bytes()).to_vec();
        skb.extend_from_slice(&[0u8; 32]);
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &skb), SkbClass::Desync);
        assert_ne!(st.flags & FLAG_DESYNC, 0);
        // Sticky: even a perfectly aligned frame now passes.
        assert_eq!(step(&mut st, &frame(64)), SkbClass::Passthrough);
    }

    #[test]
    fn boundary_walk_budget_desyncs() {
        // More zero-length frames than the walk budget in one skb.
        let mut skb = Vec::new();
        for _ in 0..(MAX_BOUNDARIES_PER_SKB + 1) {
            skb.extend_from_slice(&frame(0));
        }
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &skb), SkbClass::Desync);
    }

    #[test]
    fn exact_prefix_only_skb() {
        // An skb holding exactly one prefix and nothing else.
        let f = frame(1000);
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &f[..4]), SkbClass::Passthrough);
        assert_eq!(st.remaining, 1000);
        assert_eq!(step(&mut st, &f[4..]), SkbClass::Passthrough);
        assert!(in_sync(&st));
    }

    #[test]
    fn continuation_ending_mid_prefix() {
        // skb = [tail of A][2 bytes of B's prefix].
        let a = frame(100);
        let b = frame(200);
        let mut st = FrameState::default();
        assert_eq!(step(&mut st, &a[..50]), SkbClass::Passthrough);
        let mut skb = a[50..].to_vec();
        skb.extend_from_slice(&b[..2]);
        assert_eq!(step(&mut st, &skb), SkbClass::Passthrough);
        assert_eq!(st.prefix_have, 2);
        assert_eq!(st.remaining, 0);
        assert_eq!(step(&mut st, &b[2..]), SkbClass::Passthrough);
        assert!(in_sync(&st));
    }

    const MAC_A: [u8; 6] = [0x52, 0x54, 0, 0, 0, 1];
    const MAC_B: [u8; 6] = [0x52, 0x54, 0, 0, 0, 2];
    const MAC_X: [u8; 6] = [0x52, 0x54, 0, 0, 0, 99];
    const BCAST: [u8; 6] = [0xFF; 6];
    const MCAST: [u8; 6] = [0x01, 0, 0x5E, 0, 0, 1];

    fn table(mac: &[u8; 6]) -> Option<u64> {
        match *mac {
            MAC_A => Some(1),
            MAC_B => Some(2),
            _ => None,
        }
    }

    #[test]
    fn frame_verdicts() {
        use FrameVerdict::*;
        // Known unicast between two offloaded ports redirects.
        assert_eq!(
            classify_frame(&MAC_B, &MAC_A, 1, table),
            Redirect { dst_port: 2 }
        );
        // Broadcast/multicast always passes (flood is userspace's job).
        assert_eq!(classify_frame(&BCAST, &MAC_A, 1, table), Pass);
        assert_eq!(classify_frame(&MCAST, &MAC_A, 1, table), Pass);
        // Unknown source must pass so userspace learning still sees it.
        assert_eq!(classify_frame(&MAC_B, &MAC_X, 1, table), Pass);
        // Source known on a *different* port (MAC moved): pass to relearn.
        assert_eq!(classify_frame(&MAC_B, &MAC_A, 7, table), Pass);
        // Unknown destination (gateway/service/isolated/foreign): pass.
        assert_eq!(classify_frame(&MAC_X, &MAC_A, 1, table), Pass);
        // Self-addressed: pass (userspace drops it, counting it).
        assert_eq!(classify_frame(&MAC_A, &MAC_A, 1, table), Pass);
    }

    fn xdp_table(mac: &[u8; 6]) -> Option<u32> {
        match *mac {
            MAC_A => Some(1),
            MAC_B => Some(2),
            _ => None,
        }
    }

    #[test]
    fn xdp_verdicts() {
        use XdpVerdict::*;
        assert_eq!(
            classify_xdp_guest(0, 1, &MAC_B, xdp_table),
            ForwardTo { tag: 2 }
        );
        // Self-addressed known unicast drops (never echo).
        assert_eq!(classify_xdp_guest(0, 1, &MAC_A, xdp_table), Drop);
        // Broadcast, multicast, unknown: punt to the daemon.
        assert_eq!(classify_xdp_guest(0, 1, &BCAST, xdp_table), Punt);
        assert_eq!(classify_xdp_guest(0, 1, &MCAST, xdp_table), Punt);
        assert_eq!(classify_xdp_guest(0, 1, &MAC_X, xdp_table), Punt);
        // Isolated ports punt everything, even known unicast.
        assert_eq!(
            classify_xdp_guest(XDP_FLAG_ISOLATED, 1, &MAC_B, xdp_table),
            Punt
        );
    }
}
