//! Queue service on a worker thread, with a snapshot barrier.
//!
//! Each queue has an eventfd, and a kick is a write to it. The transport
//! writes it on a notify that reached userspace. The VMM can also register
//! it with KVM as an ioeventfd, and then the kernel writes it and the vCPU
//! never leaves guest mode for userspace ([`VirtioDevice::queue_events`]).

use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use parking_lot::Mutex;
use virtio_queue::Queue;
use vmm_sys_util::eventfd::EventFd;

use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

enum Command {
    Barrier(mpsc::SyncSender<()>),
    Quiesce(mpsc::SyncSender<()>),
    Resume,
    Stop,
}

/// What the worker thread sleeps on: one eventfd for commands, one per queue.
struct Wake {
    command: EventFd,
    queues: Vec<Arc<EventFd>>,
}

impl Wake {
    /// Serve queue `index` if it was kicked since it was last served.
    fn serve(&self, index: usize, device: &Mutex<Box<dyn VirtioDevice>>) {
        // The read takes every kick so far. It fails when there was none.
        if self.queues[index].read().is_ok() {
            device.lock().queue_notify(index as u16);
        }
    }

    fn run(&self, device: &Mutex<Box<dyn VirtioDevice>>, commands: &mpsc::Receiver<Command>) {
        use std::os::fd::AsRawFd;
        let mut fds: Vec<libc::pollfd> = std::iter::once(self.command.as_raw_fd())
            .chain(self.queues.iter().map(|queue| queue.as_raw_fd()))
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let mut paused = false;
        loop {
            // A paused worker leaves the kicks in the eventfds, which
            // coalesce them, and serves them after the resume.
            let watched = if paused { 1 } else { fds.len() };
            // SAFETY: `fds` holds `watched` initialised entries, and every
            // descriptor is owned by `self` for the whole call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), watched as libc::nfds_t, -1) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                log::error!("virtio queue worker: poll: {error}");
                return;
            }
            for (index, fd) in fds[1..watched].iter().enumerate() {
                if fd.revents & libc::POLLIN != 0 {
                    self.serve(index, device);
                }
            }
            if fds[0].revents & libc::POLLIN == 0 {
                continue;
            }
            let _ = self.command.read();
            while let Ok(command) = commands.try_recv() {
                // A kick that was written before the command was sent is
                // served before it, whatever `poll` reported: a barrier
                // promises that, and a snapshot must not lose a kick.
                if !paused {
                    for index in 0..self.queues.len() {
                        self.serve(index, device);
                    }
                }
                match command {
                    Command::Barrier(done) => {
                        let _ = done.send(());
                    }
                    Command::Quiesce(done) => {
                        paused = true;
                        device.lock().quiesce();
                        let _ = done.send(());
                    }
                    Command::Resume => {
                        device.lock().resume_snapshot();
                        paused = false;
                    }
                    Command::Stop => return,
                }
            }
        }
    }
}

/// Adapt a device whose queue service can block to non-blocking vCPU kicks.
/// Configuration and snapshot bytes remain those of the wrapped device.
/// Device type, offered features, config length, and queue limits must be fixed.
pub struct QueueWorker {
    device: Arc<Mutex<Box<dyn VirtioDevice>>>,
    sizes: Vec<u16>,
    kind: u32,
    features: u64,
    config_len: u64,
    wake: Arc<Wake>,
    sender: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl QueueWorker {
    pub fn new(device: Box<dyn VirtioDevice>) -> Result<Self, DeviceError> {
        // Capability reads can hold the transport's interrupt lock. Cache
        // them so they never wait for a queue issuing an interrupt.
        let sizes = device.queue_max_sizes().to_vec();
        let kind = device.device_type();
        let features = device.features();
        let config_len = device.config_len();
        let name = format!("{}-queues", device.state_id());
        let device = Arc::new(Mutex::new(device));
        let event = || EventFd::new(libc::EFD_NONBLOCK | libc::EFD_CLOEXEC);
        let wake = Arc::new(Wake {
            command: event()?,
            queues: sizes
                .iter()
                .map(|_| event().map(Arc::new))
                .collect::<Result<_, _>>()?,
        });
        let (sender, receiver) = mpsc::channel();
        let work = wake.clone();
        let inner = device.clone();
        let worker = std::thread::Builder::new()
            .name(name)
            .spawn(move || work.run(&inner, &receiver))?;
        Ok(Self {
            device,
            sizes,
            kind,
            features,
            config_len,
            wake,
            sender,
            worker: Some(worker),
        })
    }

    fn send(&self, command: Command) {
        self.sender
            .send(command)
            .expect("virtio queue worker is alive");
        self.wake
            .command
            .write(1)
            .expect("virtio queue worker command event is writable");
    }

    fn barrier(&self, quiesce: bool) {
        let (done, wait) = mpsc::sync_channel(0);
        self.send(if quiesce {
            Command::Quiesce(done)
        } else {
            Command::Barrier(done)
        });
        wait.recv()
            .expect("virtio queue worker completed its barrier");
    }
}

impl Drop for QueueWorker {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Stop);
        let _ = self.wake.command.write(1);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl VirtioDevice for QueueWorker {
    fn device_type(&self) -> u32 {
        self.kind
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.sizes
    }
    fn features(&self) -> u64 {
        self.features
    }
    fn ack_features(&mut self, features: u64) {
        self.device.lock().ack_features(features);
    }
    fn config_len(&self) -> u64 {
        self.config_len
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.device.lock().read_config(offset, data);
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        self.device.lock().write_config(offset, data);
    }
    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        self.device.lock().activate(mem, interrupt, queues)
    }
    fn queue_notify(&mut self, index: u16) {
        if let Some(queue) = self.wake.queues.get(index as usize) {
            // The write fails only when the counter is full, and then a
            // kick is pending already.
            let _ = queue.write(1);
        }
    }
    fn queue_events(&self) -> Option<Vec<Arc<EventFd>>> {
        Some(self.wake.queues.clone())
    }
    fn reset(&mut self) {
        self.barrier(false);
        for queue in &self.wake.queues {
            let _ = queue.read();
        }
        self.device.lock().reset();
    }
}

impl Snapshot for QueueWorker {
    fn quiesce(&mut self) {
        self.barrier(true);
    }
    fn resume_snapshot(&mut self) {
        self.send(Command::Resume);
    }
    fn state_id(&self) -> String {
        self.device.lock().state_id()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.device.lock().save_state()
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.device.lock().restore_state(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::{block::Block, testing::MemDisk};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    struct Slow {
        inner: Block,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        completed: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Snapshot for Slow {
        fn state_id(&self) -> String {
            self.inner.state_id()
        }
        fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
            self.inner.save_state()
        }
        fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
            self.inner.restore_state(state)
        }
    }
    impl VirtioDevice for Slow {
        fn device_type(&self) -> u32 {
            self.inner.device_type()
        }
        fn queue_max_sizes(&self) -> &[u16] {
            self.inner.queue_max_sizes()
        }
        fn features(&self) -> u64 {
            self.inner.features()
        }
        fn ack_features(&mut self, features: u64) {
            self.inner.ack_features(features);
        }
        fn config_len(&self) -> u64 {
            self.inner.config_len()
        }
        fn read_config(&self, offset: u64, data: &mut [u8]) {
            self.inner.read_config(offset, data);
        }
        fn write_config(&mut self, offset: u64, data: &[u8]) {
            self.inner.write_config(offset, data);
        }
        fn activate(
            &mut self,
            mem: GuestMemory,
            interrupt: Arc<dyn VirtioInterrupt>,
            queues: Vec<Queue>,
        ) -> Result<(), DeviceError> {
            self.inner.activate(mem, interrupt, queues)
        }
        fn queue_notify(&mut self, _: u16) {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(2)).unwrap();
            self.completed.fetch_add(1, Ordering::SeqCst);
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
    }

    #[test]
    fn kicks_do_not_wait_for_io_and_quiesce_waits_for_completion() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let block = Block::new("test", Box::new(MemDisk::new(4096)));
        let expected = block.save_state().unwrap();
        let capabilities = (block.device_type(), block.features(), block.config_len());
        let sizes = block.queue_max_sizes().to_vec();
        let mut device = QueueWorker::new(Box::new(Slow {
            inner: block,
            entered,
            release: released,
            completed: completed.clone(),
        }))
        .unwrap();
        device.queue_notify(0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        assert_eq!(
            (device.device_type(), device.features(), device.config_len()),
            capabilities,
            "transport capability reads must not wait for blocked queue I/O"
        );
        assert_eq!(device.queue_max_sizes(), sizes);
        let (quiesced, parked) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            device.quiesce();
            quiesced
                .send(device)
                .unwrap_or_else(|_| panic!("quiesce receiver is alive"));
        });
        assert!(parked.recv_timeout(Duration::from_millis(20)).is_err());
        release.send(()).unwrap();
        let mut device = parked.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.join().unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(device.save_state().unwrap(), expected);
        device.queue_notify(0);
        assert!(started.recv_timeout(Duration::from_millis(20)).is_err());
        device.resume_snapshot();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        release.send(()).unwrap();
        device.quiesce();
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        device.restore_state(&expected).unwrap();
        assert_eq!(device.save_state().unwrap(), expected);
        device.reset();
    }

    #[test]
    fn paused_kicks_are_coalesced_without_losing_queues() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = Block::new("test", Box::new(MemDisk::new(4096)))
            .with_num_queues(4)
            .unwrap();
        let mut device = QueueWorker::new(Box::new(Slow {
            inner,
            entered,
            release: released,
            completed: completed.clone(),
        }))
        .unwrap();
        device.quiesce();
        for _ in 0..1000 {
            for index in [0, 2, 3] {
                device.queue_notify(index);
            }
        }
        device.queue_notify(4);
        assert!(started.recv_timeout(Duration::from_millis(20)).is_err());
        device.resume_snapshot();
        for _ in 0..3 {
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            release.send(()).unwrap();
        }
        device.quiesce();
        assert_eq!(completed.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_write_to_a_queue_event_is_a_kick() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = Block::new("test", Box::new(MemDisk::new(4096)))
            .with_num_queues(2)
            .unwrap();
        let mut device = QueueWorker::new(Box::new(Slow {
            inner,
            entered,
            release: released,
            completed: completed.clone(),
        }))
        .unwrap();
        let events = device.queue_events().unwrap();
        assert_eq!(events.len(), 2);
        // What KVM does for a registered ioeventfd: no call into the device.
        events[1].write(1).unwrap();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        release.send(()).unwrap();
        device.quiesce();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        // A kick the kernel delivers while the worker is paused is kept.
        events[0].write(1).unwrap();
        events[0].write(1).unwrap();
        assert!(started.recv_timeout(Duration::from_millis(20)).is_err());
        device.resume_snapshot();
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        release.send(()).unwrap();
        device.quiesce();
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        device.resume_snapshot();
        // A reset leaves no kick behind.
        device.reset();
        assert!(started.recv_timeout(Duration::from_millis(20)).is_err());
    }

    #[test]
    fn a_barrier_serves_the_kicks_written_before_it() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner = Block::new("test", Box::new(MemDisk::new(4096)));
        let mut device = QueueWorker::new(Box::new(Slow {
            inner,
            entered,
            release: released,
            completed: completed.clone(),
        }))
        .unwrap();
        let events = device.queue_events().unwrap();
        for round in 1..=50 {
            events[0].write(1).unwrap();
            release.send(()).unwrap();
            device.quiesce();
            assert_eq!(completed.load(Ordering::SeqCst), round);
            started.try_recv().unwrap();
            device.resume_snapshot();
        }
    }
}
