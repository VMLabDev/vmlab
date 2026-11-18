use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

pub(super) struct SocketBackend {
    listener: UnixListener,
    client: Option<UnixStream>,
    path: PathBuf,
    identity: (u64, u64),
}

impl SocketBackend {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        let metadata = fs::symlink_metadata(path)?;
        Ok(Self {
            listener,
            client: None,
            path: path.to_path_buf(),
            identity: (metadata.dev(), metadata.ino()),
        })
    }

    fn accept(&mut self) {
        if self.client.is_none()
            && let Ok((client, _)) = self.listener.accept()
            && client.set_nonblocking(true).is_ok()
        {
            self.client = Some(client);
        }
    }

    pub fn read_input(&mut self, bytes: &mut [u8]) -> usize {
        self.accept();
        if bytes.is_empty() {
            return 0;
        }
        let Some(client) = self.client.as_mut() else {
            return 0;
        };
        match readiness(client) {
            Readiness::Idle => return 0,
            Readiness::HungUp => {
                // QEMU's socket chardev disconnects on hangup and discards
                // what the guest has not read yet. The legacy agent relies
                // on it: a hello left by a host that gave up must not reach
                // COM1 later.
                self.client = None;
                return 0;
            }
            Readiness::Input => {}
        }
        match client.read(bytes) {
            Ok(0) => {
                self.client = None;
                0
            }
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                0
            }
            Err(_) => {
                self.client = None;
                0
            }
        }
    }

    fn write_output(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.accept();
        let Some(client) = self.client.as_mut() else {
            // An unattached serial cable discards output without stopping TX.
            return Ok(bytes.len());
        };
        match client.write(bytes) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::NotConnected
                ) =>
            {
                self.client = None;
                Ok(bytes.len())
            }
            result => result,
        }
    }
}

enum Readiness {
    Idle,
    Input,
    HungUp,
}

/// One poll in place of a speculative read: input waiting, nothing, or a
/// host end that has closed, whatever it left unread. A half-close is
/// not a hangup, as in QEMU, which watches for `G_IO_HUP` only.
fn readiness(client: &UnixStream) -> Readiness {
    let mut fd = libc::pollfd {
        fd: std::os::fd::AsRawFd::as_raw_fd(client),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the duration of the call, no wait.
    let ready = unsafe { libc::poll(&mut fd, 1, 0) };
    if ready <= 0 {
        Readiness::Idle
    } else if fd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
        Readiness::HungUp
    } else {
        Readiness::Input
    }
}

impl Drop for SocketBackend {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) struct SocketOut(pub Arc<Mutex<SocketBackend>>);

impl Write for SocketOut {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().write_output(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
