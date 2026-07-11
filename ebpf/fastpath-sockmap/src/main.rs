//! The sockmap fast-path programs (vmlab PRD §9.1 substitutable backend).
//!
//! Loaded once per switch by src/net/fastpath/sockmap.rs, which owns all map
//! contents. Two sk_skb stream-verdict programs:
//!
//! - `verdict_guest`, attached to `PORT_HASH` (the QEMU-facing unix stream
//!   sockets): redirects skbs that are exactly one length-prefixed frame of
//!   known unicast between two offloaded ports; everything else SK_PASSes to
//!   the daemon's reader task unchanged.
//! - `verdict_tx`, attached to `TX_HASH` (per-port SOCK_DGRAM loopbacks):
//!   forwards the daemon's egress frames into the destination QEMU socket,
//!   preserving the single-writer invariant — once a port is offloaded, only
//!   the kernel writes its socket.
//!
//! All decisions live in `fastpath-logic` (host-unit-tested); this crate is
//! the unsafe shim binding them to maps and skbs.

#![no_std]
#![no_main]

use aya_ebpf::{
    EbpfContext,
    bindings::sk_action,
    helpers::bpf_get_socket_cookie,
    macros::{map, stream_verdict},
    maps::{HashMap, PerCpuArray, SockHash},
    programs::SkBuffContext,
};
use fastpath_logic::{FrameState, FrameVerdict, PREFIX_LEN, SkbClass, classify_frame, step_skb};

/// Per-socket state: which port the socket is, plus the framing-boundary
/// state machine. Keyed by socket cookie.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockState {
    pub port_id: u64,
    pub fs: FrameState,
}

/// Kernel-forwarded traffic counters (index 0), merged into Switch::stats.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FpStats {
    pub frames: u64,
    pub bytes: u64,
}

/// QEMU-facing stream sockets, keyed by port id; redirect target and the
/// attach point of `verdict_guest`.
#[map]
static PORT_HASH: SockHash<u64> = SockHash::with_max_entries(256, 0);

/// The daemon-egress dgram loopbacks, keyed by port id; attach point of
/// `verdict_tx`.
#[map]
static TX_HASH: SockHash<u64> = SockHash::with_max_entries(256, 0);

/// Learned MAC -> offloaded port id. Only non-isolated guest stream ports
/// ever appear here (the userspace switch is the sole writer).
#[map]
static MAC_MAP: HashMap<[u8; 6], u64> = HashMap::with_max_entries(1024, 0);

/// Socket cookie -> SockState for the QEMU-facing sockets.
#[map]
static SOCK_STATE: HashMap<u64, SockState> = HashMap::with_max_entries(512, 0);

/// Socket cookie of a TX loopback -> the port id whose QEMU socket its
/// dgrams are written to.
#[map]
static TX_TARGET: HashMap<u64, u64> = HashMap::with_max_entries(256, 0);

#[map]
static FP_STATS: PerCpuArray<FpStats> = PerCpuArray::with_max_entries(1, 0);

#[stream_verdict]
pub fn verdict_guest(ctx: SkBuffContext) -> u32 {
    let cookie = unsafe { bpf_get_socket_cookie(ctx.as_ptr()) };
    let Some(state_ptr) = SOCK_STATE.get_ptr_mut(&cookie) else {
        // Not a managed socket: leave the stream alone.
        return sk_action::SK_PASS;
    };
    // Copy to the stack, run the logic, write back. Per-socket verdicts are
    // serialized by the kernel, so this is not racy.
    let mut st = unsafe { *state_ptr };
    let len = ctx.len();
    let class = step_skb(&mut st.fs, len, &mut |off, dst| {
        ctx.load_bytes(off as usize, dst)
            .map(|n| n == dst.len())
            .unwrap_or(false)
    });
    unsafe { (*state_ptr).fs = st.fs };

    let SkbClass::Aligned { .. } = class else {
        return sk_action::SK_PASS;
    };
    let Ok(dst_mac) = ctx.load::<[u8; 6]>(PREFIX_LEN as usize) else {
        return sk_action::SK_PASS;
    };
    let Ok(src_mac) = ctx.load::<[u8; 6]>(PREFIX_LEN as usize + 6) else {
        return sk_action::SK_PASS;
    };
    let verdict = classify_frame(&dst_mac, &src_mac, st.port_id, |mac| unsafe {
        MAC_MAP.get(mac).copied()
    });
    match verdict {
        FrameVerdict::Redirect { dst_port } => {
            if let Some(stats) = FP_STATS.get_ptr_mut(0) {
                unsafe {
                    (*stats).frames += 1;
                    (*stats).bytes += u64::from(len);
                }
            }
            // Egress redirect (flags 0): transmit out of the target socket.
            PORT_HASH.redirect_skb(&ctx, dst_port, 0) as u32
        }
        FrameVerdict::Pass => sk_action::SK_PASS,
    }
}

#[stream_verdict]
pub fn verdict_tx(ctx: SkBuffContext) -> u32 {
    let cookie = unsafe { bpf_get_socket_cookie(ctx.as_ptr()) };
    let Some(port) = (unsafe { TX_TARGET.get(&cookie) }) else {
        // Orphaned loopback: nothing sane to do with the frame.
        return sk_action::SK_DROP;
    };
    PORT_HASH.redirect_skb(&ctx, *port, 0) as u32
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
