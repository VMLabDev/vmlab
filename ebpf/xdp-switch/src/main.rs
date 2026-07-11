//! The afxdp-tier XDP switch (vmlab PRD §9.1 substitutable backend).
//!
//! One instance (program + maps) is loaded per segment by
//! src/net/fastpath/afxdp.rs and attached in SKB (generic) mode to every tap
//! of that segment plus the daemon's host tap. Guest-tap ingress forwards
//! known non-isolated unicast tap-to-tap via the `PORTS` devmap and punts
//! everything else to the daemon as `[4-byte BE tag][frame]` on the host
//! tap; host-tap ingress strips that tag and injects toward the tagged tap.
//! The fall-through is XDP_DROP: guest frames never touch the host stack.
//!
//! Map contents are written only by the daemon: `MAC_TABLE` is a static
//! projection of configured NIC MACs, never a learning cache.

#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    helpers::bpf_xdp_adjust_head,
    macros::{map, xdp},
    maps::{DevMap, HashMap, PerCpuArray},
    programs::XdpContext,
};
use fastpath_logic::{HOST_TAG, TAG_LEN, XDP_FLAG_HOST, XdpVerdict, classify_xdp_guest};

/// Per-tap configuration, keyed by ifindex.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PortConf {
    /// This tap's slot in `PORTS` (0 is reserved for the host tap).
    pub tag: u32,
    /// `fastpath_logic::XDP_FLAG_*` bits.
    pub flags: u32,
}

/// Configured MAC -> destination tag, non-isolated guest taps only.
#[map]
static MAC_TABLE: HashMap<[u8; 6], u32> = HashMap::with_max_entries(1024, 0);

/// tag -> tap ifindex; slot `HOST_TAG` (0) is the daemon's host tap.
#[map]
static PORTS: DevMap = DevMap::with_max_entries(256, 0);

/// ifindex -> PortConf for every tap the program is attached to.
#[map]
static PORT_CONF: HashMap<u32, PortConf> = HashMap::with_max_entries(256, 0);

/// Counters: 0 = forwarded, 1 = punted, 2 = dropped, 3 = host-injected.
#[map]
static STATS: PerCpuArray<u64> = PerCpuArray::with_max_entries(4, 0);

const STAT_FORWARDED: u32 = 0;
const STAT_PUNTED: u32 = 1;
const STAT_DROPPED: u32 = 2;
const STAT_INJECTED: u32 = 3;

fn bump(index: u32) {
    if let Some(v) = STATS.get_ptr_mut(index) {
        unsafe { *v += 1 };
    }
}

/// Bounds-checked pointer into the packet.
fn ptr_at<T>(ctx: &XdpContext, offset: usize) -> Option<*const T> {
    let start = ctx.data();
    let end = ctx.data_end();
    if start + offset + core::mem::size_of::<T>() > end {
        return None;
    }
    Some((start + offset) as *const T)
}

#[xdp]
pub fn xdp_switch(ctx: XdpContext) -> u32 {
    let ifindex = ctx.ingress_ifindex() as u32;
    let Some(conf) = (unsafe { PORT_CONF.get(&ifindex) }) else {
        // Not one of ours (should be unreachable: the program is only
        // attached to configured taps). Never leak into the host stack.
        return xdp_action::XDP_DROP;
    };
    let conf = *conf;

    if conf.flags & XDP_FLAG_HOST != 0 {
        return host_ingress(&ctx);
    }

    let Some(dst) = (unsafe { ptr_at::<[u8; 6]>(&ctx, 0).map(|p| *p) }) else {
        bump(STAT_DROPPED);
        return xdp_action::XDP_DROP; // runt
    };
    let verdict = classify_xdp_guest(conf.flags, conf.tag, &dst, |mac| unsafe {
        MAC_TABLE.get(mac).copied()
    });
    match verdict {
        XdpVerdict::ForwardTo { tag } => {
            bump(STAT_FORWARDED);
            PORTS.redirect(tag, 0).unwrap_or(xdp_action::XDP_DROP)
        }
        XdpVerdict::Drop => {
            bump(STAT_DROPPED);
            xdp_action::XDP_DROP
        }
        XdpVerdict::Punt => punt(&ctx, conf.tag),
    }
}

/// Daemon -> guest: strip the `[4-byte BE tag]` header and redirect to the
/// tagged tap.
fn host_ingress(ctx: &XdpContext) -> u32 {
    let Some(tag_be) = (unsafe { ptr_at::<[u8; 4]>(ctx, 0).map(|p| *p) }) else {
        return xdp_action::XDP_DROP;
    };
    let tag = u32::from_be_bytes(tag_be);
    if tag == HOST_TAG {
        // Never hairpin back onto the host tap.
        return xdp_action::XDP_DROP;
    }
    if unsafe { bpf_xdp_adjust_head(ctx.ctx, TAG_LEN as i32) } != 0 {
        return xdp_action::XDP_DROP;
    }
    bump(STAT_INJECTED);
    PORTS.redirect(tag, 0).unwrap_or(xdp_action::XDP_DROP)
}

/// Guest -> daemon: prepend this tap's tag and redirect to the host tap.
fn punt(ctx: &XdpContext, tag: u32) -> u32 {
    if unsafe { bpf_xdp_adjust_head(ctx.ctx, -(TAG_LEN as i32)) } != 0 {
        bump(STAT_DROPPED);
        return xdp_action::XDP_DROP;
    }
    // adjust_head invalidated all previous pointers; re-derive and bounds-
    // check before writing the tag.
    let start = ctx.data();
    let end = ctx.data_end();
    if start + TAG_LEN as usize > end {
        bump(STAT_DROPPED);
        return xdp_action::XDP_DROP;
    }
    let header = start as *mut [u8; 4];
    unsafe { *header = tag.to_be_bytes() };
    bump(STAT_PUNTED);
    PORTS.redirect(HOST_TAG, 0).unwrap_or(xdp_action::XDP_DROP)
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
