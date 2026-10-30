//! A fake vmlab-hv display socket, for tests of the native client here, in
//! the vmlab-hv adapter and in the recorder.
//!
//! It is vmlab-hv's own server core (`vmlab_hv_display::DisplayServer`) over
//! a frame source the test paints and an input sink that records what it
//! was sent — the wire, the memfds and the release discipline are the real
//! ones, with no machine behind them. [`FakeDisplay::vanish`] and
//! [`FakeDisplay::reappear`] take the socket away and bring it back on the
//! same path, which is what a `vm restart` does to a recorder.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use vmlab_hv_display::protocol::XRGB8888;
use vmlab_hv_display::{
    Cursor, DisplayServer, Frame, FrameSource, InputSink, Message, Pixels, Rect,
};

/// The picture the source currently shows.
struct Painted {
    generation: u64,
    width: u32,
    height: u32,
    pixels: Arc<Vec<u8>>,
    damage: Vec<Rect>,
}

struct Source(Mutex<Painted>);

impl FrameSource for Source {
    fn frame(&self) -> io::Result<Frame> {
        let p = self.0.lock().unwrap();
        Ok(Frame {
            generation: p.generation,
            width: p.width,
            height: p.height,
            stride: p.width * 4,
            drm_fourcc: XRGB8888,
            modifier: 0,
            timestamp_ns: 0,
            damage: p.damage.clone(),
            pixels: Pixels::Shm(p.pixels.clone()),
        })
    }

    fn readback(&self, _: &Frame) -> io::Result<Vec<u8>> {
        Err(io::Error::other("a shm source needs no readback"))
    }
}

struct Sink(Arc<Mutex<Vec<Message>>>);

impl InputSink for Sink {
    fn input(&self, message: Message) {
        self.0.lock().unwrap().push(message);
    }
}

/// A `B G R x` frame where pixel `(x, y)` is `(x, y, x ^ y)` as RGB.
pub(crate) fn pattern(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            out.extend_from_slice(&[(x ^ y) as u8, y as u8, x as u8, 0]);
        }
    }
    out
}

pub(crate) struct FakeDisplay {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    server: DisplayServer,
    source: Arc<Source>,
    /// Every input message the server accepted, in order.
    pub inputs: Arc<Mutex<Vec<Message>>>,
    pub connections: Arc<AtomicUsize>,
    /// Every stream this generation accepted, shut down by `vanish`.
    streams: Arc<Mutex<Vec<UnixStream>>>,
}

/// Serve a `width`x`height` [`pattern`] on a fresh socket.
pub(crate) fn spawn(width: u32, height: u32) -> FakeDisplay {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("display.sock");
    let source = Arc::new(Source(Mutex::new(Painted {
        generation: 1,
        width,
        height,
        pixels: Arc::new(pattern(width, height)),
        damage: vec![Rect {
            x: 0,
            y: 0,
            width,
            height,
        }],
    })));
    let inputs: Arc<Mutex<Vec<Message>>> = Arc::default();
    let server = DisplayServer::new(source.clone(), Arc::new(Sink(inputs.clone()))).unwrap();
    let fake = FakeDisplay {
        _dir: dir,
        path,
        server,
        source,
        inputs,
        connections: Arc::default(),
        streams: Arc::default(),
    };
    fake.listen();
    fake
}

impl FakeDisplay {
    fn listen(&self) {
        let listener = UnixListener::bind(&self.path).expect("bind fake display socket");
        let server = self.server.clone();
        let connections = self.connections.clone();
        let streams = self.streams.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                connections.fetch_add(1, Ordering::SeqCst);
                streams.lock().unwrap().push(stream.try_clone().unwrap());
                let server = server.clone();
                std::thread::spawn(move || server.serve_native(stream));
            }
        });
    }

    /// Paint `pixels` (`B G R x`, `width`x`height`) with `damage` and
    /// publish it; a new size reconfigures every client.
    pub(crate) fn show(&self, width: u32, height: u32, pixels: Vec<u8>, damage: Vec<Rect>) {
        {
            let mut p = self.source.0.lock().unwrap();
            if (p.width, p.height) != (width, height) {
                p.generation += 1;
            }
            *p = Painted {
                generation: p.generation,
                width,
                height,
                pixels: Arc::new(pixels),
                damage,
            };
        }
        self.server.publish().unwrap();
    }

    pub(crate) fn cursor(&self, cursor: Cursor) {
        self.server.set_cursor(cursor).unwrap();
    }

    /// Drop every connection and take the socket away.
    pub(crate) fn vanish(&self) {
        let _ = std::fs::remove_file(&self.path);
        for s in self.streams.lock().unwrap().drain(..) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Serve again on the same path.
    pub(crate) fn reappear(&self) {
        self.listen();
    }
}
