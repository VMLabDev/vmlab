//! Windows event-log tailing: EvtSubscribe on the System channel (or a
//! caller-supplied XPath), rendering each event to XML and streaming it as
//! session data.

use std::ffi::c_void;
use std::sync::Arc;

use windows_sys::Win32::System::EventLog::{
    EVT_HANDLE, EvtClose, EvtRender, EvtRenderEventXml, EvtSubscribe, EvtSubscribeActionDeliver,
    EvtSubscribeToFutureEvents,
};

use vmlab_agent_proto::{AgentMsg, FrameKind};

use super::port::wide;
use crate::mux::{Credit, Mux};

struct SubCtx {
    mux: Mux,
    id: u32,
    credit: Arc<Credit>,
}

/// The subscription handle, closed on session close.
struct Subscription(EVT_HANDLE, *mut SubCtx);
// SAFETY: EvtClose is thread-safe; the ctx box is freed exactly once.
unsafe impl Send for Subscription {}

pub fn open(mux: &Mux, id: u32, filter: Option<String>) {
    let query = wide(filter.as_deref().unwrap_or("*"));
    let channel = wide("System");

    // The callback context: freed when the subscription is closed.
    let Some((_input, credit)) = mux.register(id, None, None) else {
        return;
    };
    let ctx = Box::into_raw(Box::new(SubCtx {
        mux: mux.clone(),
        id,
        credit,
    }));

    // SAFETY: callback + context stay valid until EvtClose (the kill hook).
    let sub = unsafe {
        EvtSubscribe(
            0,
            std::ptr::null_mut(),
            channel.as_ptr(),
            query.as_ptr(),
            0,
            ctx as *const c_void,
            Some(callback),
            EvtSubscribeToFutureEvents,
        )
    };
    if sub == 0 {
        let e = std::io::Error::last_os_error();
        // SAFETY: subscription never started; reclaim the context.
        drop(unsafe { Box::from_raw(ctx) });
        mux.remove_finished(id);
        mux.send_error(Some(id), format!("eventlog: {e}"));
        return;
    }
    let subscription = Subscription(sub, ctx);
    mux.set_kill(
        id,
        Box::new(move || {
            // Rebind the whole struct: precise closure capture would
            // otherwise capture the raw-pointer field alone, sidestepping
            // Subscription's Send impl.
            let subscription = subscription;
            // SAFETY: close stops callbacks, then the context can go.
            unsafe {
                EvtClose(subscription.0);
                drop(Box::from_raw(subscription.1));
            }
        }),
    );
    mux.send_ctrl(&AgentMsg::Opened { id });
}

/// EVT_SUBSCRIBE_CALLBACK: render the event as XML and ship it.
unsafe extern "system" fn callback(action: i32, context: *const c_void, event: EVT_HANDLE) -> u32 {
    if action != EvtSubscribeActionDeliver || context.is_null() {
        return 0;
    }
    // SAFETY: context outlives the subscription (freed only after EvtClose).
    let ctx = unsafe { &*(context as *const SubCtx) };
    // SAFETY: size-probe then render into a properly sized buffer.
    let xml = unsafe {
        let mut used: u32 = 0;
        let mut props: u32 = 0;
        EvtRender(
            0,
            event,
            EvtRenderEventXml,
            0,
            std::ptr::null_mut(),
            &mut used,
            &mut props,
        );
        if used == 0 {
            return 0;
        }
        let mut buf = vec![0u16; used.div_ceil(2) as usize + 1];
        if EvtRender(
            0,
            event,
            EvtRenderEventXml,
            (buf.len() * 2) as u32,
            buf.as_mut_ptr() as *mut c_void,
            &mut used,
            &mut props,
        ) == 0
        {
            return 0;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    };
    let mut line = xml.into_bytes();
    line.push(b'\n');
    // Respect the credit window like every other data source.
    let mut off = 0;
    while off < line.len() {
        let take = ctx.credit.take(line.len() - off);
        if take == 0 {
            return 0; // session closed
        }
        ctx.mux
            .send_data(FrameKind::Data, ctx.id, &line[off..off + take]);
        off += take;
    }
    0
}
