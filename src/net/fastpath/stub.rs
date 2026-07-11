//! Inert stand-ins for the `ebpf`-feature types, so switch.rs carries no
//! feature cfg. `SegmentOffload::for_segment` is the only constructor and
//! always returns `None`; the uninhabited field makes every other method
//! statically unreachable.

use std::convert::Infallible;
use std::sync::Arc;

use crate::config::model::MacAddr;
use crate::net::switch::PortId;

pub struct SegmentOffload {
    never: Infallible,
}

pub struct PortTx {
    never: Infallible,
}

impl SegmentOffload {
    pub fn for_segment(_name: &str) -> Option<Arc<SegmentOffload>> {
        None
    }

    pub fn add_port(
        &self,
        _id: PortId,
        _stream: &tokio::net::UnixStream,
    ) -> anyhow::Result<PortTx> {
        match self.never {}
    }

    pub fn adopt_write_half(&self, _id: PortId, _half: tokio::net::unix::OwnedWriteHalf) {
        match self.never {}
    }

    pub fn relearn(&self, _mac: MacAddr, _port: PortId) {
        match self.never {}
    }

    pub fn remove_port(&self, _id: PortId, _purged: &[MacAddr]) {
        match self.never {}
    }

    pub fn stats(&self) -> (u64, u64) {
        match self.never {}
    }
}

impl PortTx {
    pub async fn send_frame(&self, _frame: &[u8]) -> std::io::Result<()> {
        match self.never {}
    }
}

pub struct SegmentXdp {
    never: Infallible,
}

pub struct TapNic {
    never: Infallible,
}

impl SegmentXdp {
    pub fn new(_segment: &str, _mtu: u16) -> anyhow::Result<Arc<SegmentXdp>> {
        anyhow::bail!("vmlab was built without the `ebpf` feature")
    }

    pub fn add_nic(
        self: &Arc<Self>,
        _switch: &Arc<crate::net::switch::Switch>,
        _mac: MacAddr,
        _isolated: bool,
    ) -> anyhow::Result<TapNic> {
        match self.never {}
    }
}

impl TapNic {
    pub fn qemu_fd(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        match self.never {}
    }
}
