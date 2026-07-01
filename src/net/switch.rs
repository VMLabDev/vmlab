//! The per-segment userspace L2 switch (PRD §9.1).
//!
//! Every VM NIC attaches as a port over a QEMU stream-socket netdev; daemon
//! services (DHCP/DNS/NAT, trunks) attach as channel ports. The switch does
//! MAC-learning forwarding between ports, honours private-VLAN-style port
//! isolation, and exposes an ingress hook seam for later L3 rule
//! enforcement.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use bytes::Bytes;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::debug;

use super::framing;
use crate::config::model::MacAddr;

/// Per-port egress queue depth. It's ethernet: a full queue drops the frame.
pub const PORT_QUEUE_CAPACITY: usize = 512;

/// Opaque switch-port identifier, unique per switch for its lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PortId(u64);

impl std::fmt::Display for PortId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "port{}", self.0)
    }
}

/// What kind of participant a port is — drives isolation (PRD §9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortClass {
    /// A VM NIC. `isolated` ports exchange frames only with [`Service`]
    /// ports (gateway/NAT/trunk), never with other guests — the
    /// private-VLAN model.
    ///
    /// [`Service`]: PortClass::Service
    Guest { isolated: bool },
    /// A daemon-side participant: DHCP/DNS/NAT, port-forward proxy, trunk.
    Service,
}

/// Verdict of an [`IngressHook`] for one ingress frame.
pub enum HookAction {
    /// Forward the frame unmodified.
    Pass,
    /// Drop the frame. (No in-tree hook returns this today — the L3 rules
    /// hook drops via `Inject { forward: None, .. }` — but it stays part of
    /// the seam contract and the switch tests exercise it.)
    #[allow(dead_code)]
    Drop,
    /// Forward this frame instead of the original.
    Replace(Vec<u8>),
    /// Optionally forward a frame, and queue `reply` frames back out the
    /// ingress port (e.g. a synthesised TCP RST or ICMP unreachable).
    Inject {
        forward: Option<Vec<u8>>,
        reply: Vec<Bytes>,
    },
}

/// Hook invoked on every ingress frame before learning and forwarding.
/// The seam for L3 rule enforcement (block/redirect, PRD §9.9).
pub type IngressHook = Box<dyn Fn(PortId, &PortClass, &[u8]) -> HookAction + Send + Sync>;

/// A channel-backed port handle for in-process participants.
pub struct ChannelPort {
    pub id: PortId,
    /// Frames sent here ingress the switch as if received on this port.
    pub tx: mpsc::Sender<Bytes>,
    /// Frames the switch egresses to this port.
    pub rx: mpsc::Receiver<Bytes>,
}

/// Point-in-time counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SwitchStats {
    pub ports: usize,
    /// Frames delivered to a single learned unicast destination.
    pub frames_forwarded: u64,
    /// Ingress frames flooded (unknown unicast, broadcast, multicast) that
    /// reached at least one port.
    pub frames_flooded: u64,
    /// Frames that died: hook drops, isolation drops, runts, full or closed
    /// egress queues, and floods that reached nobody.
    pub frames_dropped: u64,
}

struct PortEntry {
    class: PortClass,
    tx: mpsc::Sender<Bytes>,
}

#[derive(Default)]
struct Inner {
    ports: HashMap<PortId, PortEntry>,
    macs: HashMap<MacAddr, PortId>,
}

/// A virtual L2 switch for one segment.
pub struct Switch {
    name: String,
    inner: Mutex<Inner>,
    hook: Mutex<Option<IngressHook>>,
    next_port: AtomicU64,
    forwarded: AtomicU64,
    flooded: AtomicU64,
    dropped: AtomicU64,
}

/// May a frame ingressing on a port of class `ingress` egress a port of
/// class `egress`? Isolated guest ports exchange frames only with service
/// ports.
fn delivery_allowed(ingress: &PortClass, egress: &PortClass) -> bool {
    !matches!(
        (ingress, egress),
        (PortClass::Guest { isolated: true }, PortClass::Guest { .. })
            | (PortClass::Guest { .. }, PortClass::Guest { isolated: true })
    )
}

fn is_multicast(mac: &MacAddr) -> bool {
    mac.0[0] & 0x01 != 0
}

impl Switch {
    pub fn new(name: String) -> Arc<Switch> {
        Arc::new(Switch {
            name,
            inner: Mutex::new(Inner::default()),
            hook: Mutex::new(None),
            next_port: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            flooded: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    /// Install (or replace) the ingress hook. The hook runs synchronously on
    /// every ingress frame before MAC learning and forwarding.
    pub fn set_ingress_hook(&self, hook: IngressHook) {
        *self.hook.lock().expect("hook lock poisoned") = Some(hook);
    }

    /// Diagnostics counters; consumed by the switch tests only so far.
    #[allow(dead_code)]
    pub fn stats(&self) -> SwitchStats {
        SwitchStats {
            ports: self.inner.lock().expect("switch lock poisoned").ports.len(),
            frames_forwarded: self.forwarded.load(Ordering::Relaxed),
            frames_flooded: self.flooded.load(Ordering::Relaxed),
            frames_dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    /// Attach a QEMU stream-socket connection as a port. Spawns a reader
    /// task (deframe → ingress) and a writer task (egress queue → frame).
    /// The port is removed automatically on EOF or stream error.
    pub async fn add_stream_port(self: &Arc<Self>, stream: UnixStream, class: PortClass) -> PortId {
        let id = self.alloc_port(class);
        let mut rx = self.attach(id, class);
        let (mut read_half, mut write_half) = stream.into_split();

        let name = self.name.clone();
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if let Err(error) = framing::write_frame(&mut write_half, &frame).await {
                    debug!(switch = %name, %id, %error, "stream port write failed");
                    break;
                }
            }
        });

        let switch = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match framing::read_frame(&mut read_half).await {
                    Ok(Some(frame)) => switch.ingress(id, frame),
                    Ok(None) => {
                        debug!(switch = %switch.name, %id, "stream port EOF");
                        break;
                    }
                    Err(error) => {
                        debug!(switch = %switch.name, %id, %error, "stream port read failed");
                        break;
                    }
                }
            }
            switch.remove_port(id);
        });

        id
    }

    /// Attach an in-process participant (DHCP/DNS/NAT service, trunk) as a
    /// port. The port is removed automatically when the returned `tx` is
    /// dropped.
    pub fn add_channel_port(self: &Arc<Self>, class: PortClass) -> ChannelPort {
        let id = self.alloc_port(class);
        let egress_rx = self.attach(id, class);
        let (ingress_tx, mut ingress_rx) = mpsc::channel::<Bytes>(PORT_QUEUE_CAPACITY);

        let switch = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(frame) = ingress_rx.recv().await {
                switch.ingress(id, frame);
            }
            switch.remove_port(id);
        });

        ChannelPort {
            id,
            tx: ingress_tx,
            rx: egress_rx,
        }
    }

    /// Detach a port and purge every MAC learned on it. Idempotent.
    pub fn remove_port(&self, id: PortId) {
        let mut inner = self.inner.lock().expect("switch lock poisoned");
        if inner.ports.remove(&id).is_some() {
            inner.macs.retain(|_, p| *p != id);
            debug!(switch = %self.name, %id, "port removed");
        }
    }

    /// Bind a unix listener and accept QEMU stream-netdev connections as
    /// ports of the given class until the returned task is aborted. A stale
    /// socket file at the path is replaced.
    pub async fn listen_unix(
        self: &Arc<Self>,
        socket_path: &Path,
        class: PortClass,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        match std::fs::remove_file(socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("removing stale socket {}", socket_path.display()));
            }
        }
        let listener = tokio::net::UnixListener::bind(socket_path)
            .with_context(|| format!("binding switch socket {}", socket_path.display()))?;
        let switch = Arc::clone(self);
        Ok(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let id = switch.add_stream_port(stream, class).await;
                        debug!(switch = %switch.name, %id, "accepted stream port");
                    }
                    Err(error) => {
                        debug!(switch = %switch.name, %error, "unix accept failed");
                        break;
                    }
                }
            }
        }))
    }

    // -- internals ----------------------------------------------------------

    fn alloc_port(&self, class: PortClass) -> PortId {
        let id = PortId(self.next_port.fetch_add(1, Ordering::Relaxed));
        debug!(switch = %self.name, %id, ?class, "port allocated");
        id
    }

    /// Register the port in the table and return its egress receiver.
    fn attach(&self, id: PortId, class: PortClass) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(PORT_QUEUE_CAPACITY);
        self.inner
            .lock()
            .expect("switch lock poisoned")
            .ports
            .insert(id, PortEntry { class, tx });
        rx
    }

    /// Process one frame received on `port`: hook, learn, forward.
    fn ingress(&self, port: PortId, frame: Bytes) {
        let ingress_class = {
            let inner = self.inner.lock().expect("switch lock poisoned");
            match inner.ports.get(&port) {
                Some(p) => p.class,
                None => return, // port already gone
            }
        };

        // Ingress hook (rule enforcement seam).
        let action = {
            let hook = self.hook.lock().expect("hook lock poisoned");
            match hook.as_ref() {
                Some(h) => h(port, &ingress_class, &frame),
                None => HookAction::Pass,
            }
        };
        let forward = match action {
            HookAction::Pass => Some(frame),
            HookAction::Drop => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                None
            }
            HookAction::Replace(f) => Some(Bytes::from(f)),
            HookAction::Inject { forward, reply } => {
                if !reply.is_empty() {
                    let inner = self.inner.lock().expect("switch lock poisoned");
                    if let Some(p) = inner.ports.get(&port) {
                        for r in reply {
                            if p.tx.try_send(r).is_err() {
                                self.dropped.fetch_add(1, Ordering::Relaxed);
                                debug!(switch = %self.name, %port, "reply queue full, frame dropped");
                            }
                        }
                    }
                }
                forward.map(Bytes::from)
            }
        };
        let Some(frame) = forward else { return };

        if frame.len() < super::frame::ETH_HEADER_LEN {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            debug!(switch = %self.name, %port, len = frame.len(), "runt frame dropped");
            return;
        }
        let mut dst = [0u8; 6];
        dst.copy_from_slice(&frame[0..6]);
        let dst = MacAddr(dst);
        let mut src = [0u8; 6];
        src.copy_from_slice(&frame[6..12]);
        let src = MacAddr(src);

        let inner = &mut *self.inner.lock().expect("switch lock poisoned");

        // Learn the source MAC; relearning moves it (VM migrated ports).
        if !is_multicast(&src) {
            inner.macs.insert(src, port);
        }

        let unicast_target = if is_multicast(&dst) {
            None
        } else {
            inner.macs.get(&dst).copied()
        };

        match unicast_target {
            // Known unicast destination on another port.
            Some(target) if target != port => {
                let Some(entry) = inner.ports.get(&target) else {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                if !delivery_allowed(&ingress_class, &entry.class) {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                if entry.tx.try_send(frame).is_ok() {
                    self.forwarded.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    debug!(switch = %self.name, egress = %target, "egress queue full, frame dropped");
                }
            }
            // Destination learned on the ingress port itself: never echo.
            Some(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            // Unknown unicast / broadcast / multicast: flood.
            None => {
                let mut delivered = 0u64;
                for (id, entry) in &inner.ports {
                    if *id == port || !delivery_allowed(&ingress_class, &entry.class) {
                        continue;
                    }
                    if entry.tx.try_send(frame.clone()).is_ok() {
                        delivered += 1;
                    } else {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                        debug!(switch = %self.name, egress = %id, "egress queue full, frame dropped");
                    }
                }
                if delivered > 0 {
                    self.flooded.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt;
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::net::frame::{ETHERTYPE_IPV4, MAC_BROADCAST, eth_build};

    fn mac(n: u8) -> MacAddr {
        MacAddr([0x02, 0x00, 0x00, 0x00, 0x00, n])
    }

    fn frame(dst: MacAddr, src: MacAddr, tag: &[u8]) -> Bytes {
        Bytes::from(eth_build(dst, src, ETHERTYPE_IPV4, tag))
    }

    async fn recv(rx: &mut mpsc::Receiver<Bytes>) -> Bytes {
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for frame")
            .expect("port closed")
    }

    async fn assert_no_frame(rx: &mut mpsc::Receiver<Bytes>) {
        let got = timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            got.is_err(),
            "unexpected frame received: {:?}",
            got.unwrap()
        );
    }

    async fn wait_for<F: Fn() -> bool>(cond: F, what: &str) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    const GUEST: PortClass = PortClass::Guest { isolated: false };
    const GUEST_ISOLATED: PortClass = PortClass::Guest { isolated: true };

    #[tokio::test]
    async fn flood_then_unicast_after_learning() {
        let sw = Switch::new("seg0".into());
        let mut a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        let mut c = sw.add_channel_port(GUEST);

        // A -> B before B's MAC is known: flooded to B and C.
        let f1 = frame(mac(2), mac(1), b"a2b");
        a.tx.send(f1.clone()).await.unwrap();
        assert_eq!(recv(&mut b.rx).await, f1);
        assert_eq!(recv(&mut c.rx).await, f1);
        let s = sw.stats();
        assert_eq!(s.frames_flooded, 1);
        assert_eq!(s.frames_forwarded, 0);

        // B -> A: A's MAC was learned from the first frame, so unicast.
        let f2 = frame(mac(1), mac(2), b"b2a");
        b.tx.send(f2.clone()).await.unwrap();
        assert_eq!(recv(&mut a.rx).await, f2);
        assert_no_frame(&mut c.rx).await;
        let s = sw.stats();
        assert_eq!(s.frames_forwarded, 1);
        assert_eq!(s.frames_flooded, 1);

        // A -> B is now unicast too (B's MAC learned from f2... no, from
        // B's send of f2 whose src is mac(2)).
        let f3 = frame(mac(2), mac(1), b"a2b again");
        a.tx.send(f3.clone()).await.unwrap();
        assert_eq!(recv(&mut b.rx).await, f3);
        assert_no_frame(&mut c.rx).await;
        assert_eq!(sw.stats().frames_forwarded, 2);
    }

    #[tokio::test]
    async fn broadcast_floods_all_three_ports() {
        let sw = Switch::new("seg0".into());
        let mut a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        let mut c = sw.add_channel_port(GUEST);
        let mut d = sw.add_channel_port(GUEST);

        let f = frame(MAC_BROADCAST, mac(1), b"bcast");
        a.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut b.rx).await, f);
        assert_eq!(recv(&mut c.rx).await, f);
        assert_eq!(recv(&mut d.rx).await, f);
        assert_no_frame(&mut a.rx).await; // never echo to ingress
        assert_eq!(sw.stats().frames_flooded, 1);
    }

    #[tokio::test]
    async fn never_echoes_to_ingress_even_for_own_mac() {
        let sw = Switch::new("seg0".into());
        let mut a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);

        // Teach the switch A's MAC, then have A address itself.
        a.tx.send(frame(MAC_BROADCAST, mac(1), b"hello"))
            .await
            .unwrap();
        recv(&mut b.rx).await;
        a.tx.send(frame(mac(1), mac(1), b"to self")).await.unwrap();
        assert_no_frame(&mut a.rx).await;
        assert_no_frame(&mut b.rx).await;
        wait_for(|| sw.stats().frames_dropped == 1, "self-addressed drop").await;
    }

    #[tokio::test]
    async fn isolation_matrix() {
        let sw = Switch::new("seg0".into());
        let mut gi = sw.add_channel_port(GUEST_ISOLATED);
        let mut g = sw.add_channel_port(GUEST);
        let mut s = sw.add_channel_port(PortClass::Service);
        let (m_gi, m_g, m_s) = (mac(1), mac(2), mac(3));

        // Isolated guest broadcasts: only the service port hears it.
        let f = frame(MAC_BROADCAST, m_gi, b"from gi");
        gi.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut s.rx).await, f);
        assert_no_frame(&mut g.rx).await;

        // Plain guest broadcasts: service hears it, isolated guest does not.
        let f = frame(MAC_BROADCAST, m_g, b"from g");
        g.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut s.rx).await, f);
        assert_no_frame(&mut gi.rx).await;

        // Service broadcasts: everyone hears it.
        let f = frame(MAC_BROADCAST, m_s, b"from s");
        s.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut gi.rx).await, f);
        assert_eq!(recv(&mut g.rx).await, f);

        // All MACs are now learned. Unicast checks both directions:
        let dropped_before = sw.stats().frames_dropped;
        // isolated guest -> guest: blocked.
        gi.tx.send(frame(m_g, m_gi, b"gi2g")).await.unwrap();
        assert_no_frame(&mut g.rx).await;
        // guest -> isolated guest: blocked.
        g.tx.send(frame(m_gi, m_g, b"g2gi")).await.unwrap();
        assert_no_frame(&mut gi.rx).await;
        wait_for(
            || sw.stats().frames_dropped == dropped_before + 2,
            "isolation drops",
        )
        .await;
        // isolated guest -> service: unicast passes.
        let f = frame(m_s, m_gi, b"gi2s");
        gi.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut s.rx).await, f);
        // service -> isolated guest: unicast passes.
        let f = frame(m_gi, m_s, b"s2gi");
        s.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut gi.rx).await, f);
        // guest -> guest unaffected requires a second plain guest.
        let mut g2 = sw.add_channel_port(GUEST);
        let f = frame(MAC_BROADCAST, mac(4), b"from g2");
        g2.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut g.rx).await, f);
        let f = frame(mac(4), m_g, b"g2g2");
        g.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut g2.rx).await, f);
    }

    #[tokio::test]
    async fn hook_drop_blocks_forwarding() {
        let sw = Switch::new("seg0".into());
        let a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        sw.set_ingress_hook(Box::new(|_, _, _| HookAction::Drop));

        a.tx.send(frame(MAC_BROADCAST, mac(1), b"doomed"))
            .await
            .unwrap();
        assert_no_frame(&mut b.rx).await;
        wait_for(|| sw.stats().frames_dropped == 1, "hook drop counted").await;
        assert_eq!(sw.stats().frames_flooded, 0);
    }

    #[tokio::test]
    async fn hook_replace_forwards_modified_frame() {
        let sw = Switch::new("seg0".into());
        let a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        let replacement = eth_build(MAC_BROADCAST, mac(9), ETHERTYPE_IPV4, b"rewritten");
        let r2 = replacement.clone();
        sw.set_ingress_hook(Box::new(move |_, _, _| HookAction::Replace(r2.clone())));

        a.tx.send(frame(MAC_BROADCAST, mac(1), b"original"))
            .await
            .unwrap();
        assert_eq!(recv(&mut b.rx).await, Bytes::from(replacement));
    }

    #[tokio::test]
    async fn hook_inject_replies_out_ingress_without_forwarding() {
        let sw = Switch::new("seg0".into());
        let mut a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        let reply = frame(mac(1), mac(9), b"synthesised reply");
        let r2 = reply.clone();
        sw.set_ingress_hook(Box::new(move |_, _, _| HookAction::Inject {
            forward: None,
            reply: vec![r2.clone()],
        }));

        a.tx.send(frame(MAC_BROADCAST, mac(1), b"probe"))
            .await
            .unwrap();
        // Reply comes back out the ingress port; nothing reaches B.
        assert_eq!(recv(&mut a.rx).await, reply);
        assert_no_frame(&mut b.rx).await;
    }

    #[tokio::test]
    async fn hook_inject_can_also_forward() {
        let sw = Switch::new("seg0".into());
        let mut a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        let reply = frame(mac(1), mac(9), b"reply");
        let forwarded = eth_build(MAC_BROADCAST, mac(1), ETHERTYPE_IPV4, b"mutated");
        let (r2, f2) = (reply.clone(), forwarded.clone());
        sw.set_ingress_hook(Box::new(move |_, _, _| HookAction::Inject {
            forward: Some(f2.clone()),
            reply: vec![r2.clone()],
        }));

        a.tx.send(frame(MAC_BROADCAST, mac(1), b"probe"))
            .await
            .unwrap();
        assert_eq!(recv(&mut a.rx).await, reply);
        assert_eq!(recv(&mut b.rx).await, Bytes::from(forwarded));
    }

    #[tokio::test]
    async fn hook_sees_port_and_class() {
        let sw = Switch::new("seg0".into());
        let a = sw.add_channel_port(GUEST_ISOLATED);
        let seen = Arc::new(Mutex::new(None));
        let seen2 = Arc::clone(&seen);
        sw.set_ingress_hook(Box::new(move |id, class, frame| {
            *seen2.lock().unwrap() = Some((id, *class, frame.len()));
            HookAction::Pass
        }));

        let f = frame(MAC_BROADCAST, mac(1), b"peek");
        let flen = f.len();
        a.tx.send(f).await.unwrap();
        wait_for(|| seen.lock().unwrap().is_some(), "hook invocation").await;
        assert_eq!(*seen.lock().unwrap(), Some((a.id, GUEST_ISOLATED, flen)));
    }

    #[tokio::test]
    async fn stream_port_carries_frames_and_is_removed_on_close() {
        let sw = Switch::new("seg0".into());
        let (qemu_side, switch_side) = UnixStream::pair().unwrap();
        let port_id = sw.add_stream_port(switch_side, GUEST).await;
        let mut svc = sw.add_channel_port(PortClass::Service);
        assert_eq!(sw.stats().ports, 2);

        // "QEMU" sends a frame in: deframed and flooded to the service port.
        let (mut qemu_read, mut qemu_write) = qemu_side.into_split();
        let f_in = frame(MAC_BROADCAST, mac(1), b"hello from guest");
        framing::write_frame(&mut qemu_write, &f_in).await.unwrap();
        assert_eq!(recv(&mut svc.rx).await, f_in);

        // Service replies unicast to the learned guest MAC; the stream port
        // writer frames it back out the socket.
        let f_out = frame(mac(1), mac(9), b"hello from service");
        svc.tx.send(f_out.clone()).await.unwrap();
        let got = framing::read_frame(&mut qemu_read).await.unwrap().unwrap();
        assert_eq!(got, f_out);

        // Closing the QEMU side removes the port and its learned MACs.
        qemu_write.shutdown().await.unwrap();
        drop(qemu_write);
        drop(qemu_read);
        wait_for(|| sw.stats().ports == 1, "stream port removal").await;

        // The guest MAC was purged: unicast to it floods (and reaches no
        // one but existing ports — here, nobody else), so it drops.
        let dropped_before = sw.stats().frames_dropped;
        svc.tx.send(frame(mac(1), mac(9), b"ghost")).await.unwrap();
        wait_for(
            || sw.stats().frames_dropped > dropped_before,
            "flood to nobody drops",
        )
        .await;
        let _ = port_id;
    }

    #[tokio::test]
    async fn channel_port_removed_when_sender_dropped() {
        let sw = Switch::new("seg0".into());
        let a = sw.add_channel_port(GUEST);
        let _b = sw.add_channel_port(GUEST);
        assert_eq!(sw.stats().ports, 2);
        drop(a); // drops both tx and rx
        wait_for(|| sw.stats().ports == 1, "channel port removal").await;
    }

    #[tokio::test]
    async fn listen_unix_accepts_qemu_connections() {
        let dir = std::env::temp_dir().join(format!("vmlab-net-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seg0.sock");

        let sw = Switch::new("seg0".into());
        let handle = sw.listen_unix(&path, GUEST).await.unwrap();
        let mut svc = sw.add_channel_port(PortClass::Service);

        let stream = UnixStream::connect(&path).await.unwrap();
        let (_qemu_read, mut qemu_write) = stream.into_split();
        let f = frame(MAC_BROADCAST, mac(1), b"via listener");
        framing::write_frame(&mut qemu_write, &f).await.unwrap();
        assert_eq!(recv(&mut svc.rx).await, f);
        wait_for(|| sw.stats().ports == 2, "accepted port").await;

        handle.abort();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[tokio::test]
    async fn full_egress_queue_drops_frames() {
        let sw = Switch::new("seg0".into());
        let a = sw.add_channel_port(GUEST);
        let b = sw.add_channel_port(GUEST);
        // Nobody reads b.rx: its 512-deep egress queue fills, then drops.
        let total = PORT_QUEUE_CAPACITY + 50;
        for _ in 0..total {
            a.tx.send(frame(MAC_BROADCAST, mac(1), b"flood"))
                .await
                .unwrap();
        }
        wait_for(
            || sw.stats().frames_dropped >= 50,
            "queue-full drops counted",
        )
        .await;
        drop(b);
    }

    #[tokio::test]
    async fn runt_frames_are_dropped() {
        let sw = Switch::new("seg0".into());
        let a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        a.tx.send(Bytes::from_static(b"tooshort")).await.unwrap();
        assert_no_frame(&mut b.rx).await;
        wait_for(|| sw.stats().frames_dropped == 1, "runt drop").await;
    }

    #[tokio::test]
    async fn relearning_moves_a_mac() {
        let sw = Switch::new("seg0".into());
        let mut a = sw.add_channel_port(GUEST);
        let mut b = sw.add_channel_port(GUEST);
        let mut c = sw.add_channel_port(GUEST);

        // mac(7) first appears on port B...
        b.tx.send(frame(MAC_BROADCAST, mac(7), b"on b"))
            .await
            .unwrap();
        recv(&mut a.rx).await;
        recv(&mut c.rx).await;
        // ...then moves to port C.
        c.tx.send(frame(MAC_BROADCAST, mac(7), b"moved to c"))
            .await
            .unwrap();
        recv(&mut a.rx).await;
        recv(&mut b.rx).await;

        // Unicast to mac(7) now goes to C only.
        let f = frame(mac(7), mac(1), b"find me");
        a.tx.send(f.clone()).await.unwrap();
        assert_eq!(recv(&mut c.rx).await, f);
        assert_no_frame(&mut b.rx).await;
    }
}
