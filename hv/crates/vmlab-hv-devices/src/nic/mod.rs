//! Emulated NICs that are not virtio: the e1000 82540EM and the PCnet
//! Am79C970A of the pc machine (phase 4b), and the link both use.
//!
//! A [`Link`] is a NIC's connection to vmlab's switch: whole Ethernet frames
//! in vmlab's stream framing (a 4-byte big-endian length, then the frame)
//! over a unix stream socket the NIC connects to, as the virtio-net stream
//! attachment does. The NIC model owns its descriptor rings; the link owns
//! the socket and a worker thread. Every guest-memory access happens in the
//! model, under its lock: the worker only offers frames and runs timers
//! through [`LinkClient`], so a quiesced model touches no guest memory.

pub mod e1000;
pub mod pcnet;

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Frames longer than this end the connection: the stream is corrupt.
pub const MAX_FRAME: usize = 65535;
/// Received frames held while the guest has no buffer for them. Past this
/// the link stops reading the socket, which pushes back on the switch.
const MAX_HELD: usize = 64;
/// Frames queued for the switch before new ones are dropped, as a wire
/// without carrier would drop them.
const MAX_QUEUED: usize = 256;
const RECONNECT: Duration = Duration::from_millis(200);

/// What the link's worker asks of the NIC model it serves.
pub trait LinkClient: Send + 'static {
    /// The connection to the switch came up or went down.
    fn link_changed(&mut self, up: bool);
    /// Offer one received frame. `false` keeps it for a later offer: the
    /// guest has no receive buffer, or the model is quiesced. A frame the
    /// model filters out or drops by policy is consumed (`true`).
    fn receive(&mut self, frame: &[u8]) -> bool;
    /// Run the model's timers due at `now`; the next deadline, if any.
    fn poll_timers(&mut self, now: Instant) -> Option<Instant>;
}

struct Shared {
    out: Mutex<VecDeque<Vec<u8>>>,
    wake: UnixStream,
    stop: AtomicBool,
}

/// The NIC model's half of a link: queue frames for the switch and wake
/// the worker when receive buffers or timers change.
pub struct Link {
    shared: Arc<Shared>,
    worker: Option<UnixStream>,
}

impl Link {
    pub fn new() -> io::Result<Self> {
        let (wake, worker) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        worker.set_nonblocking(true)?;
        Ok(Self {
            shared: Arc::new(Shared {
                out: Mutex::new(VecDeque::new()),
                wake,
                stop: AtomicBool::new(false),
            }),
            worker: Some(worker),
        })
    }

    /// Queue one frame for the switch. Dropped when the queue is full.
    pub fn send(&self, frame: Vec<u8>) {
        let mut out = self.shared.out.lock();
        if out.len() < MAX_QUEUED && frame.len() <= MAX_FRAME {
            out.push_back(frame);
        }
        drop(out);
        self.kick();
    }

    /// Wake the worker: receive buffers were posted or a timer changed.
    pub fn kick(&self) {
        let _ = (&self.shared.wake).write(&[1]);
    }

    /// Frames queued and not yet taken by a worker (tests drive models
    /// without one).
    pub fn take_sent(&self) -> Vec<Vec<u8>> {
        self.shared.out.lock().drain(..).collect()
    }

    /// Start the worker for `client` on `socket`. A link without a started
    /// worker keeps its frames for [`Self::take_sent`].
    pub fn start<C: LinkClient>(&mut self, client: &Arc<Mutex<C>>, socket: PathBuf) {
        let Some(wake) = self.worker.take() else {
            return;
        };
        let shared = self.shared.clone();
        let client = Arc::downgrade(client);
        let _ = std::thread::Builder::new()
            .name("nic-link".into())
            .spawn(move || Worker::new(shared, wake, client, socket).run());
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.kick();
    }
}

struct Worker<C> {
    shared: Arc<Shared>,
    wake: UnixStream,
    client: Weak<Mutex<C>>,
    socket: PathBuf,
    stream: Option<UnixStream>,
    next_connect: Instant,
    input: Vec<u8>,
    held: VecDeque<Vec<u8>>,
    output: Vec<u8>,
}

impl<C: LinkClient> Worker<C> {
    fn new(shared: Arc<Shared>, wake: UnixStream, client: Weak<Mutex<C>>, socket: PathBuf) -> Self {
        Self {
            shared,
            wake,
            client,
            socket,
            stream: None,
            next_connect: Instant::now(),
            input: Vec::new(),
            held: VecDeque::new(),
            output: Vec::new(),
        }
    }

    fn run(mut self) {
        while !self.shared.stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            if self.stream.is_none() && now >= self.next_connect {
                self.connect();
            }
            let Some(client) = self.client.upgrade() else {
                return;
            };
            let deadline = {
                let mut client = client.lock();
                while let Some(frame) = self.held.front() {
                    if !client.receive(frame) {
                        break;
                    }
                    self.held.pop_front();
                }
                client.poll_timers(now)
            };
            drop(client);
            self.fill_output();
            if self.flush().is_err() {
                self.disconnect();
            }
            let mut timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if self.stream.is_none() {
                let retry = self.next_connect.saturating_duration_since(Instant::now());
                timeout = Some(timeout.map_or(retry, |t| t.min(retry)));
            }
            if self
                .wait(timeout.unwrap_or(Duration::from_secs(1)))
                .is_err()
            {
                self.disconnect();
            }
        }
    }

    fn connect(&mut self) {
        match UnixStream::connect(&self.socket).and_then(|s| {
            s.set_nonblocking(true)?;
            Ok(s)
        }) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.input.clear();
                self.output.clear();
                self.notify(true);
            }
            Err(_) => self.next_connect = Instant::now() + RECONNECT,
        }
    }

    fn disconnect(&mut self) {
        if self.stream.take().is_some() {
            self.notify(false);
        }
        self.next_connect = Instant::now() + RECONNECT;
    }

    fn notify(&self, up: bool) {
        if let Some(client) = self.client.upgrade() {
            client.lock().link_changed(up);
        }
    }

    fn fill_output(&mut self) {
        let mut out = self.shared.out.lock();
        if self.stream.is_none() {
            // No carrier: what the guest sends is lost.
            out.clear();
            return;
        }
        while self.output.len() < 1 << 20
            && let Some(frame) = out.pop_front()
        {
            self.output
                .extend_from_slice(&(frame.len() as u32).to_be_bytes());
            self.output.extend_from_slice(&frame);
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        while !self.output.is_empty() {
            match stream.write(&self.output) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.output.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        let mut fds = vec![libc::pollfd {
            fd: self.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(stream) = &self.stream {
            let mut events = 0;
            if self.held.len() < MAX_HELD {
                events |= libc::POLLIN;
            }
            if !self.output.is_empty() {
                events |= libc::POLLOUT;
            }
            fds.push(libc::pollfd {
                fd: stream.as_raw_fd(),
                events,
                revents: 0,
            });
        }
        let millis = timeout.as_millis().clamp(0, 1000) as i32;
        // SAFETY: `fds` is a valid array of initialised pollfds.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(error)
            };
        }
        if fds[0].revents != 0 {
            let mut drain = [0u8; 64];
            while matches!((&self.wake).read(&mut drain), Ok(n) if n > 0) {}
        }
        if let Some(fd) = fds.get(1)
            && fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
        {
            self.read_frames()?;
        }
        Ok(())
    }

    fn read_frames(&mut self) -> io::Result<()> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        let mut chunk = [0u8; 16384];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.input.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
            if self.input.len() > 4 * (MAX_FRAME + 4) {
                break;
            }
        }
        while self.input.len() >= 4 {
            let length = u32::from_be_bytes(self.input[..4].try_into().unwrap()) as usize;
            if length > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame longer than the stream allows",
                ));
            }
            if self.input.len() < 4 + length {
                break;
            }
            let frame = self.input[4..4 + length].to_vec();
            self.input.drain(..4 + length);
            if self.held.len() < MAX_HELD * 2 {
                self.held.push_back(frame);
            }
        }
        Ok(())
    }
}

/// The Internet checksum's running ones-complement sum of `bytes`, added to
/// `sum`, not yet folded.
pub(crate) fn ones_sum(mut sum: u32, bytes: &[u8]) -> u32 {
    let (pairs, rest) = bytes.as_chunks::<2>();
    for pair in pairs {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes(*pair)));
    }
    if let [last] = rest {
        sum = sum.wrapping_add(u32::from(*last) << 8);
    }
    sum
}

/// Fold a running sum to 16 bits.
pub(crate) fn fold(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum as u16
}

/// The IEEE 802.3 frame check sequence of `frame`, as it follows the frame
/// on the wire (little-endian).
pub(crate) fn fcs(frame: &[u8]) -> [u8; 4] {
    let mut crc = 0xffff_ffffu32;
    for &byte in frame {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    (!crc).to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_helpers_match_known_values() {
        // RFC 1071's example words.
        let bytes = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(fold(ones_sum(0, &bytes)), 0xddf2);
        assert_eq!(fold(ones_sum(0, &[0xab])), 0xab00);
        // CRC-32 of "123456789" is 0xcbf43926.
        assert_eq!(fcs(b"123456789"), 0xcbf4_3926u32.to_le_bytes());
    }

    struct Probe {
        frames: Vec<Vec<u8>>,
        accept: bool,
        up: Vec<bool>,
    }

    impl LinkClient for Probe {
        fn link_changed(&mut self, up: bool) {
            self.up.push(up);
        }
        fn receive(&mut self, frame: &[u8]) -> bool {
            if self.accept {
                self.frames.push(frame.to_vec());
            }
            self.accept
        }
        fn poll_timers(&mut self, _: Instant) -> Option<Instant> {
            None
        }
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < until, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn link_frames_both_ways_and_holds_until_the_model_has_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switch.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let probe = Arc::new(Mutex::new(Probe {
            frames: vec![],
            accept: false,
            up: vec![],
        }));
        let mut link = Link::new().unwrap();
        link.start(&probe, path);
        let (mut switch, _) = listener.accept().unwrap();
        wait_for(|| probe.lock().up == [true]);

        link.send(vec![1, 2, 3]);
        let mut header = [0; 4];
        switch.read_exact(&mut header).unwrap();
        assert_eq!(u32::from_be_bytes(header), 3);
        let mut body = [0; 3];
        switch.read_exact(&mut body).unwrap();
        assert_eq!(body, [1, 2, 3]);

        // Two frames arrive split across writes while the model has no
        // buffer; both wait, then arrive in order once it does.
        switch.write_all(&[0, 0, 0, 2, 9]).unwrap();
        switch.write_all(&[8, 0, 0, 0, 1, 7]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(probe.lock().frames.is_empty());
        probe.lock().accept = true;
        link.kick();
        wait_for(|| probe.lock().frames.len() == 2);
        assert_eq!(probe.lock().frames, vec![vec![9, 8], vec![7]]);

        drop(switch);
        wait_for(|| probe.lock().up == [true, false]);
    }
}
