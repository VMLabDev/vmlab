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

/// Branch counters for field diagnosis (dumped by the probe on failure):
/// 0 invoked, 1 no-state (cookie miss), 2 non-aligned passthrough,
/// 3 aligned, 4 classify pass, 5 redirect attempted, 6 redirect returned
/// drop, 7 tx-verdict invoked.
#[map]
static FP_DEBUG: PerCpuArray<u64> = PerCpuArray::with_max_entries(8, 0);

pub const DBG_INVOKED: u32 = 0;
pub const DBG_NO_STATE: u32 = 1;
pub const DBG_PASSTHROUGH: u32 = 2;
pub const DBG_ALIGNED: u32 = 3;
pub const DBG_CLASSIFY_PASS: u32 = 4;
pub const DBG_REDIRECT: u32 = 5;
pub const DBG_REDIRECT_DROP: u32 = 6;
pub const DBG_TX_INVOKED: u32 = 7;

#[inline(always)]
fn dbg(index: u32) {
    if let Some(v) = FP_DEBUG.get_ptr_mut(index) {
        unsafe { *v += 1 };
    }
}

#[stream_verdict]
pub fn verdict_guest(ctx: SkBuffContext) -> u32 {
    dbg(DBG_INVOKED);
    let cookie = unsafe { bpf_get_socket_cookie(ctx.as_ptr()) };
    let Some(state_ptr) = SOCK_STATE.get_ptr_mut(&cookie) else {
        // Not a managed socket: leave the stream alone.
        dbg(DBG_NO_STATE);
        return sk_action::SK_PASS;
    };
    // Copy to the stack, run the logic, write back. Per-socket verdicts are
    // serialized by the kernel, so this is not racy.
    let mut st = unsafe { *state_ptr };
    let len = ctx.len();
    let class = step_skb(&mut st.fs, len, &mut |off, dst| load_exact(&ctx, off, dst));
    unsafe { (*state_ptr).fs = st.fs };

    let SkbClass::Aligned { .. } = class else {
        dbg(DBG_PASSTHROUGH);
        return sk_action::SK_PASS;
    };
    dbg(DBG_ALIGNED);
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
            dbg(DBG_REDIRECT);
            if let Some(stats) = FP_STATS.get_ptr_mut(0) {
                unsafe {
                    (*stats).frames += 1;
                    (*stats).bytes += u64::from(len);
                }
            }
            // Egress redirect (flags 0): transmit out of the target socket.
            let ret = PORT_HASH.redirect_skb(&ctx, dst_port, 0);
            if ret == sk_action::SK_DROP as i64 {
                dbg(DBG_REDIRECT_DROP);
            }
            ret as u32
        }
        FrameVerdict::Pass => {
            dbg(DBG_CLASSIFY_PASS);
            sk_action::SK_PASS
        }
    }
}

/// Copy exactly `dst.len()` skb bytes at `off` into `dst`, dispatching to
/// constant-size loads. `bpf_skb_load_bytes` needs a verifier-provable
/// nonzero length, which a runtime slice length is not — the verifier
/// rejected the naive `ctx.load_bytes(off, dst)` with
/// "R4 invalid zero-sized read". The framing state machine only ever asks
/// for 1..=4 bytes (length-prefix pieces), so four arms cover it; anything
/// else fails the read, which the state machine treats as a safe desync.
#[inline(always)]
fn load_exact(ctx: &SkBuffContext, off: u32, dst: &mut [u8]) -> bool {
    fn copy<const N: usize>(ctx: &SkBuffContext, off: u32, dst: &mut [u8]) -> bool {
        match ctx.load::<[u8; N]>(off as usize) {
            Ok(v) => {
                dst[..N].copy_from_slice(&v);
                true
            }
            Err(_) => false,
        }
    }
    match dst.len() {
        1 => copy::<1>(ctx, off, dst),
        2 => copy::<2>(ctx, off, dst),
        3 => copy::<3>(ctx, off, dst),
        4 => copy::<4>(ctx, off, dst),
        _ => false,
    }
}

#[stream_verdict]
pub fn verdict_tx(ctx: SkBuffContext) -> u32 {
    dbg(DBG_TX_INVOKED);
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
