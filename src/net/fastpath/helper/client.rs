//! The daemon's side: start `vmlab-fastpath` once, then ask it for each
//! privileged step. The daemon itself holds no capability (F21).

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::proto::{self, Reply, Request};
use crate::sync::LockRecover;

/// The helper's file name, installed beside the `vmlab` binary.
pub const HELPER_NAME: &str = "vmlab-fastpath";

/// A reply that takes longer than this means the helper is wedged.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// One connection to one helper process.
pub(crate) struct Broker {
    /// `None` only while the broker drops.
    sock: Mutex<Option<OwnedFd>>,
    /// Reaped when the broker goes; `None` for the in-process test server.
    child: Mutex<Option<std::process::Child>>,
}

impl Drop for Broker {
    fn drop(&mut self) {
        // Closing the socket ends the helper's loop; then reap it.
        drop(self.sock.lock_recover().take());
        if let Some(mut child) = self.child.lock_recover().take() {
            let _ = child.wait();
        }
    }
}

static BROKER: Mutex<Option<Arc<Broker>>> = Mutex::new(None);

/// This daemon's helper, started on first use.
pub(crate) fn broker() -> Result<Arc<Broker>> {
    let mut slot = BROKER.lock_recover();
    if let Some(broker) = &*slot {
        return Ok(broker.clone());
    }
    let broker = Arc::new(Broker::start()?);
    *slot = Some(broker.clone());
    Ok(broker)
}

/// Let the helper go once nothing uses it (no kernel tier was selected):
/// it exits when the last engine handle drops.
pub(crate) fn release() {
    BROKER.lock_recover().take();
}

/// Where the helper is: `VMLAB_FASTPATH_HELPER`, else beside this binary.
fn helper_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("VMLAB_FASTPATH_HELPER") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().context("locating the vmlab binary")?;
    let path = exe.with_file_name(HELPER_NAME);
    if !path.is_file() {
        bail!(
            "the fast-path helper {} is not installed (vmlab looks for it beside its own binary)",
            path.display()
        );
    }
    Ok(path)
}

impl Broker {
    /// Spawn the helper with its end of the socket as stdin.
    #[cfg(not(test))]
    fn start() -> Result<Broker> {
        use std::process::{Command, Stdio};
        let path = helper_path()?;
        let (ours, theirs) = proto::pair().context("helper socket")?;
        let child = Command::new(&path)
            .env_clear()
            .stdin(Stdio::from(theirs))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting the fast-path helper {}", path.display()))?;
        Broker::over(ours, Some(child))
    }

    /// Tests serve the same protocol from a thread in this process, so the
    /// privileged tests (run under sudo) exercise it without a second binary.
    #[cfg(test)]
    fn start() -> Result<Broker> {
        let _ = helper_path;
        let (ours, theirs) = proto::pair().context("helper socket")?;
        std::thread::Builder::new()
            .name("fastpath-helper".into())
            .spawn(move || super::server::serve(theirs, super::server::Aya))
            .context("helper thread")?;
        Broker::over(ours, None)
    }

    fn over(sock: OwnedFd, child: Option<std::process::Child>) -> Result<Broker> {
        rustix::net::sockopt::set_socket_timeout(
            &sock,
            rustix::net::sockopt::Timeout::Recv,
            Some(CALL_TIMEOUT),
        )
        .context("helper socket timeout")?;
        Ok(Broker {
            sock: Mutex::new(Some(sock)),
            child: Mutex::new(child),
        })
    }

    /// One exchange. A refusal or a kernel failure is an error.
    fn call(&self, request: &Request, fds: &[BorrowedFd<'_>]) -> Result<(Reply, Vec<OwnedFd>)> {
        let guard = self.sock.lock_recover();
        let sock = guard
            .as_ref()
            .ok_or_else(|| anyhow!("the fast-path helper is gone"))?;
        proto::send(sock.as_fd(), request, fds).context("asking the fast-path helper")?;
        let (bytes, fds) = proto::recv(sock.as_fd())
            .context("reading the fast-path helper's reply")?
            .ok_or_else(|| anyhow!("the fast-path helper exited"))?;
        let reply: Reply =
            serde_json::from_slice(&bytes).context("the fast-path helper's reply")?;
        match reply {
            Reply::Refused { reason } => bail!("the fast-path helper refused: {reason}"),
            Reply::Failed { reason } => bail!("{reason}"),
            reply => Ok((reply, fds)),
        }
    }

    fn engine(&self, request: Request) -> Result<u32> {
        match self.call(&request, &[])?.0 {
            Reply::Engine { engine } => Ok(engine),
            other => bail!("unexpected helper reply {other:?}"),
        }
    }

    fn done(&self, request: Request, fds: &[BorrowedFd<'_>]) -> Result<()> {
        match self.call(&request, fds)?.0 {
            Reply::Done => Ok(()),
            other => bail!("unexpected helper reply {other:?}"),
        }
    }
}

/// A tap the helper created: name, ifindex and its queues.
pub(crate) struct HelperTap {
    pub name: String,
    pub ifindex: u32,
    pub queues: Vec<OwnedFd>,
}

/// One xdp_switch object held by the helper; dropping it unloads it.
pub(crate) struct RemoteXdp {
    broker: Arc<Broker>,
    engine: u32,
}

impl RemoteXdp {
    pub(crate) fn load() -> Result<RemoteXdp> {
        let broker = broker()?;
        let engine = broker.engine(Request::XdpLoad {})?;
        Ok(RemoteXdp { broker, engine })
    }

    /// Create a tap carrying this engine's program.
    pub(crate) fn tap(&self, queue_pairs: Option<u16>) -> Result<HelperTap> {
        let request = Request::XdpTap {
            engine: self.engine,
            queue_pairs,
        };
        match self.broker.call(&request, &[])? {
            (Reply::Tap { name, ifindex }, queues) if !queues.is_empty() => Ok(HelperTap {
                name,
                ifindex,
                queues,
            }),
            (other, _) => bail!("unexpected helper reply {other:?}"),
        }
    }

    pub(crate) fn set_port(&self, tag: u32, ifindex: u32, flags: u32) -> Result<()> {
        self.broker.done(
            Request::XdpPort {
                engine: self.engine,
                ifindex,
                tag,
                flags,
            },
            &[],
        )
    }

    pub(crate) fn release(&self, ifindex: u32) {
        let _ = self.broker.done(
            Request::XdpRelease {
                engine: self.engine,
                ifindex,
            },
            &[],
        );
    }

    pub(crate) fn insert_mac(&self, mac: [u8; 6], tag: u32) -> Result<()> {
        self.broker.done(
            Request::XdpMac {
                engine: self.engine,
                mac,
                tag: Some(tag),
            },
            &[],
        )
    }

    pub(crate) fn remove_mac(&self, mac: [u8; 6]) {
        let _ = self.broker.done(
            Request::XdpMac {
                engine: self.engine,
                mac,
                tag: None,
            },
            &[],
        );
    }

    pub(crate) fn stats(&self) -> [u64; 4] {
        match self.broker.call(
            &Request::XdpStats {
                engine: self.engine,
            },
            &[],
        ) {
            Ok((Reply::XdpStats { counters }, _)) => counters,
            _ => [0; 4],
        }
    }
}

impl Drop for RemoteXdp {
    fn drop(&mut self) {
        let _ = self.broker.done(
            Request::Unload {
                engine: self.engine,
            },
            &[],
        );
    }
}

/// One fastpath_sockmap object held by the helper; dropping it unloads it.
pub(crate) struct RemoteSockmap {
    broker: Arc<Broker>,
    engine: u32,
}

impl RemoteSockmap {
    pub(crate) fn load() -> Result<RemoteSockmap> {
        let broker = broker()?;
        let engine = broker.engine(Request::SockmapLoad {})?;
        Ok(RemoteSockmap { broker, engine })
    }

    pub(crate) fn register_stream(&self, id: u64, sock: impl AsFd) -> Result<()> {
        self.broker.done(
            Request::SockmapStream {
                engine: self.engine,
                id,
            },
            &[sock.as_fd()],
        )
    }

    /// `map_sock` is the kernel end of the loopback, `cookie_sock` the end
    /// the daemon writes; returns the cookie TX_TARGET is keyed by.
    pub(crate) fn register_tx(
        &self,
        id: u64,
        map_sock: impl AsFd,
        cookie_sock: impl AsFd,
    ) -> Result<u64> {
        let request = Request::SockmapTx {
            engine: self.engine,
            id,
        };
        match self
            .broker
            .call(&request, &[map_sock.as_fd(), cookie_sock.as_fd()])?
            .0
        {
            Reply::Cookie { cookie } => Ok(cookie),
            other => bail!("unexpected helper reply {other:?}"),
        }
    }

    pub(crate) fn insert_mac(&self, mac: [u8; 6], id: u64) -> Result<()> {
        self.broker.done(
            Request::SockmapMac {
                engine: self.engine,
                mac,
                id: Some(id),
            },
            &[],
        )
    }

    pub(crate) fn remove_mac(&self, mac: [u8; 6]) {
        let _ = self.broker.done(
            Request::SockmapMac {
                engine: self.engine,
                mac,
                id: None,
            },
            &[],
        );
    }

    pub(crate) fn unregister(&self, id: u64, tx_cookie: u64, macs: &[[u8; 6]]) {
        let _ = self.broker.done(
            Request::SockmapUnregister {
                engine: self.engine,
                id,
                tx_cookie,
                macs: macs.to_vec(),
            },
            &[],
        );
    }

    pub(crate) fn stats(&self) -> (u64, u64) {
        match self.broker.call(
            &Request::SockmapStats {
                engine: self.engine,
            },
            &[],
        ) {
            Ok((Reply::SockmapStats { frames, bytes }, _)) => (frames, bytes),
            _ => (0, 0),
        }
    }

    pub(crate) fn debug_counters(&self) -> String {
        match self.broker.call(
            &Request::SockmapDebug {
                engine: self.engine,
            },
            &[],
        ) {
            Ok((Reply::Debug { text }, _)) => text,
            Ok((other, _)) => format!("unexpected helper reply {other:?}"),
            Err(error) => format!("{error:#}"),
        }
    }
}

impl Drop for RemoteSockmap {
    fn drop(&mut self) {
        let _ = self.broker.done(
            Request::Unload {
                engine: self.engine,
            },
            &[],
        );
    }
}
