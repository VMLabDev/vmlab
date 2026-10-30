//! vmlab-hv's native frame protocol (PRD §22, phase 2b; vmlab-hv's
//! phase-2 D7/D8) as vmlab consumes it: the client side of a vmlab-hv
//! machine's `display.sock`.
//!
//! The wire, the handshake and the descriptor passing are
//! [`vmlab_hv_display`]'s; this is what vmlab does with them. A frame arrives
//! as an index into a ring of shared-memory buffers the server configured,
//! with the rectangles that changed; the pixels are read straight out of that
//! buffer (`B G R x`, the RFB raw layout the recorder already consumes) and
//! the buffer is released at once, so the server never waits on vmlab. The
//! cursor arrives as its own shape and position rather than painted into the
//! frame.
//!
//! Three consumers, as vmlab has over RFB for QEMU (`crate::vnc`): a one-shot
//! [`capture`] for screenshots and vision, an [`InputConn`] for scripted keys
//! and pointer, and a [`NativeStream`] the console recorder reads. The
//! protocol client is blocking, so each runs on a thread of its own or on
//! the blocking pool.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use image::RgbImage;
use vmlab_hv_display::protocol::{ARGB8888, XRGB8888};
use vmlab_hv_display::{Capabilities, Client, Message, Packet, SurfaceKind};

use crate::vnc::StreamEvent;

/// How long a connect waits for a socket that is about to appear: vmlab-hv
/// binds `display.sock` just after its control socket.
const CONNECT_PATIENCE: Duration = Duration::from_secs(3);

/// How long a capture waits for the frame the server owes every new client.
const FIRST_FRAME: Duration = Duration::from_secs(10);

/// Connect and handshake, retrying a socket that is not there yet.
fn connect(path: &Path, capabilities: u32) -> Result<Client> {
    let deadline = Instant::now() + CONNECT_PATIENCE;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => {
                return Client::from_stream(stream, Capabilities(capabilities))
                    .with_context(|| format!("vmlab-hv display handshake on {}", path.display()));
            }
            Err(e)
                if Instant::now() < deadline
                    && matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("connecting to {}", path.display()));
            }
        }
    }
}

/// The shared-memory surface the server last configured.
struct Surface {
    id: u64,
    width: u32,
    height: u32,
    stride: u32,
    buffers: Vec<File>,
}

impl Surface {
    /// A `SurfaceConfigure` and the buffers that came with it.
    fn configure(message: &Message, fds: Vec<std::os::fd::OwnedFd>) -> Result<Self> {
        let Message::SurfaceConfigure {
            surface_id,
            width,
            height,
            drm_fourcc,
            stride,
            kind,
            ..
        } = message
        else {
            bail!("not a surface configure: {message:?}");
        };
        if *kind != SurfaceKind::Shm {
            bail!("vmlab-hv offered a dma-buf surface vmlab did not ask for");
        }
        if *drm_fourcc != XRGB8888 && *drm_fourcc != ARGB8888 {
            bail!("vmlab-hv surface format {drm_fourcc:#x} is not XRGB8888");
        }
        if *stride < width * 4 || fds.is_empty() {
            bail!("vmlab-hv surface {width}x{height} stride {stride} is malformed");
        }
        Ok(Self {
            id: *surface_id,
            width: *width,
            height: *height,
            stride: *stride,
            buffers: fds.into_iter().map(File::from).collect(),
        })
    }

    /// The `w`x`h` region at `(x, y)` of buffer `index`, as `B G R x` rows.
    fn read(&self, index: u32, x: u32, y: u32, w: u32, h: u32) -> io::Result<Vec<u8>> {
        let file = self
            .buffers
            .get(index as usize)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no such buffer"))?;
        let row = (w * 4) as usize;
        let mut out = vec![0u8; row * h as usize];
        if w == self.width && x == 0 && self.stride == self.width * 4 {
            file.read_exact_at(&mut out, u64::from(y) * u64::from(self.stride))?;
            return Ok(out);
        }
        for r in 0..h {
            let offset = u64::from(y + r) * u64::from(self.stride) + u64::from(x) * 4;
            let at = r as usize * row;
            file.read_exact_at(&mut out[at..at + row], offset)?;
        }
        Ok(out)
    }
}

/// One full frame from the machine's display, as RGB.
pub async fn capture(path: &Path) -> Result<RgbImage> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || capture_blocking(&path))
        .await
        .context("vmlab-hv capture task")?
}

fn capture_blocking(path: &Path) -> Result<RgbImage> {
    let client = connect(path, Capabilities::FRAMES)?;
    client.stream().set_read_timeout(Some(FIRST_FRAME))?;
    let mut surface: Option<Surface> = None;
    loop {
        let Packet { message, fds } = client
            .receive()
            .with_context(|| format!("waiting for a frame on {}", path.display()))?;
        match &message {
            Message::SurfaceConfigure { .. } => surface = Some(Surface::configure(&message, fds)?),
            Message::FrameReady {
                surface_id,
                buffer,
                seq,
                ..
            } => {
                let current = surface.as_ref().filter(|s| s.id == *surface_id);
                let Some(s) = current else {
                    client.release(*surface_id, *buffer, *seq)?;
                    continue;
                };
                // Shm pixels are complete whatever the damage says.
                let bgrx = s.read(*buffer, 0, 0, s.width, s.height)?;
                client.release(*surface_id, *buffer, *seq)?;
                return Ok(rgb_from_bgrx(s.width, s.height, &bgrx));
            }
            _ => {}
        }
    }
}

/// `B G R x` pixels as an RGB image.
fn rgb_from_bgrx(width: u32, height: u32, bgrx: &[u8]) -> RgbImage {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for px in bgrx.as_chunks::<4>().0 {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    RgbImage::from_raw(width, height, rgb).expect("one pixel per four bytes")
}

/// A pointer button's bit in the protocol's held-button mask.
pub fn button_bit(button: &str) -> Result<u32> {
    match button {
        "left" => Ok(1),
        "middle" => Ok(2),
        "right" => Ok(4),
        other => Err(anyhow!("unknown mouse button `{other}`")),
    }
}

/// An input connection: keys to the guest's PS/2 keyboard, the pointer to
/// its USB tablet. One per machine, kept open so input arrives in the order
/// it was sent; vmlab-hv's LED reports on it are not read.
pub struct InputConn {
    path: PathBuf,
    client: Option<Client>,
}

impl InputConn {
    pub fn new(path: PathBuf) -> Self {
        Self { path, client: None }
    }

    /// Send `messages` in order, reconnecting once if the connection went
    /// away under us — a restart or a restore replaced the hypervisor.
    pub fn send(&mut self, messages: &[Message]) -> Result<()> {
        for attempt in 0..2 {
            if self.client.is_none() {
                self.client = Some(connect(&self.path, Capabilities::INPUT)?);
            }
            let client = self.client.as_ref().expect("connected above");
            match messages.iter().try_for_each(|m| client.send(m)) {
                Ok(()) => return Ok(()),
                Err(e) if attempt == 0 => {
                    tracing::debug!("vmlab-hv input connection dropped ({e}); reconnecting");
                    self.client = None;
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("sending input on {}", self.path.display()));
                }
            }
        }
        unreachable!("the second attempt returns")
    }
}

/// The recorder's stream: every frame's damage, the cursor and resizes, as
/// the events the RFB stream yields (`crate::vnc::StreamEvent`).
pub struct NativeStream {
    events: tokio::sync::mpsc::Receiver<io::Result<Vec<StreamEvent>>>,
    /// A second handle on the socket, shut down on drop so the reader thread
    /// blocked in a receive ends with it.
    shutdown: UnixStream,
    pub width: u16,
    pub height: u16,
}

impl NativeStream {
    /// Connect and wait for the server's first configure, so the caller
    /// knows the screen size — the server sends one to every new client,
    /// paused machine or not.
    pub async fn connect(path: &Path) -> Result<Self> {
        let at = path.to_path_buf();
        let (client, surface) = tokio::task::spawn_blocking(move || {
            let client = connect(&at, Capabilities::FRAMES | Capabilities::CURSOR)?;
            client.stream().set_read_timeout(Some(FIRST_FRAME))?;
            loop {
                let Packet { message, fds } = client
                    .receive()
                    .with_context(|| format!("waiting for a surface on {}", at.display()))?;
                if let Message::SurfaceConfigure { .. } = message {
                    let surface = Surface::configure(&message, fds)?;
                    client.stream().set_read_timeout(None)?;
                    return Ok::<_, anyhow::Error>((client, surface));
                }
            }
        })
        .await
        .context("vmlab-hv stream connect task")??;
        let shutdown = client.stream().try_clone()?;
        let (width, height) = (clamp16(surface.width), clamp16(surface.height));
        let (tx, events) = tokio::sync::mpsc::channel(8);
        std::thread::Builder::new()
            .name("vmlab-hv-frames".into())
            .spawn(move || read_frames(client, surface, tx))
            .context("spawning the vmlab-hv frame reader")?;
        Ok(Self {
            events,
            shutdown,
            width,
            height,
        })
    }

    /// The next batch of events; an error once the connection has gone.
    pub async fn next_update(&mut self) -> Result<Vec<StreamEvent>> {
        match self.events.recv().await {
            Some(Ok(events)) => Ok(events),
            Some(Err(e)) => Err(anyhow!(e).context("vmlab-hv frame stream")),
            None => bail!("vmlab-hv frame stream closed"),
        }
    }
}

impl Drop for NativeStream {
    fn drop(&mut self) {
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }
}

fn clamp16(v: u32) -> u16 {
    v.min(u32::from(u16::MAX)) as u16
}

/// The reader thread: decode until the connection or the receiver goes.
fn read_frames(
    client: Client,
    mut surface: Surface,
    tx: tokio::sync::mpsc::Sender<io::Result<Vec<StreamEvent>>>,
) {
    loop {
        let batch = client
            .receive()
            .and_then(|packet| decode(&client, &mut surface, packet));
        let failed = batch.is_err();
        if tx.blocking_send(batch).is_err() || failed {
            return;
        }
    }
}

/// One packet as the recorder's events, releasing any frame it carried.
fn decode(client: &Client, surface: &mut Surface, packet: Packet) -> io::Result<Vec<StreamEvent>> {
    let Packet { message, fds } = packet;
    Ok(match &message {
        Message::SurfaceConfigure { .. } => {
            *surface = Surface::configure(&message, fds)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:#}")))?;
            vec![StreamEvent::DesktopSize {
                w: clamp16(surface.width),
                h: clamp16(surface.height),
            }]
        }
        Message::FrameReady {
            surface_id,
            buffer,
            seq,
            damage,
            ..
        } => {
            let mut events = Vec::new();
            // A frame from a superseded configure is released unread: the
            // configure that replaced it promises a full frame of its own.
            if *surface_id == surface.id {
                for r in damage {
                    let (x, y) = (r.x.min(surface.width), r.y.min(surface.height));
                    let w = r.width.min(surface.width - x);
                    let h = r.height.min(surface.height - y);
                    if w == 0 || h == 0 {
                        continue;
                    }
                    events.push(StreamEvent::Rect {
                        x: clamp16(x),
                        y: clamp16(y),
                        w: clamp16(w),
                        h: clamp16(h),
                        bgrx: surface.read(*buffer, x, y, w, h)?,
                    });
                }
            }
            client.release(*surface_id, *buffer, *seq)?;
            events
        }
        Message::CursorShape {
            width,
            height,
            hot_x,
            hot_y,
            argb,
        } => vec![StreamEvent::Cursor {
            hotspot: (clamp16(*hot_x), clamp16(*hot_y)),
            w: clamp16(*width),
            h: clamp16(*height),
            // Premultiplied ARGB words, `B G R A` in memory, to the
            // premultiplied RGBA the recorder composites.
            rgba: argb
                .as_chunks::<4>()
                .0
                .iter()
                .flat_map(|p| [p[2], p[1], p[0], p[3]])
                .collect(),
        }],
        Message::CursorState { x, y, visible } => vec![StreamEvent::Pointer {
            x: *x,
            y: *y,
            visible: *visible,
        }],
        _ => Vec::new(),
    })
}

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;
