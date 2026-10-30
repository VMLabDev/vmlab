//! Minimal RFB (VNC) client for scripted input and framebuffer capture.
//!
//! vmlab normally injects input via QMP (`send-key` / `input-send-event`),
//! which reaches whichever keyboard QEMU routes unbound input to — the
//! machine's PS/2 keyboard, or the USB one `input = "usb"` adds (how macOS
//! is driven). Input a real VNC viewer sends takes the same route through
//! QEMU's VNC server, and some guests only behave with it. This client
//! speaks just enough of RFB 3.8 to do the same from a provision/`vmlab script`: it
//! connects to the VM's `vnc.sock`, completes the (auth-less) handshake, and
//! sends `KeyEvent`/`PointerEvent` messages.
//!
//! Selected per VM via `input_transport = "vnc"` (profile). Pointer
//! coordinates are framebuffer pixels — the same space screenshots use, so
//! no scaling is needed (unlike the QMP abs path).
//!
//! The same connection also reads the framebuffer ([`VncClient::capture`]).
//! QMP `screendump` answers `no surface` on a GL display (`gpu.mode` virgl or
//! vulkan under egl-headless), whose scanout lives in a GPU texture; QEMU's
//! VNC server reads that texture back for its clients, so a plain RFB update
//! is the one capture path that works there (PRD §10.3).

use std::path::Path;

use anyhow::{Context, Result, bail};
use image::RgbImage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Button-mask bits for `PointerEvent` (RFB 6.4.4).
pub const BTN_LEFT: u8 = 1;
pub const BTN_MIDDLE: u8 = 1 << 1;
pub const BTN_RIGHT: u8 = 1 << 2;

/// Client-to-server message types (RFB 7.5).
const MSG_SET_PIXEL_FORMAT: u8 = 0;
const MSG_SET_ENCODINGS: u8 = 2;
const MSG_FB_UPDATE_REQUEST: u8 = 3;

/// Server-to-client message types (RFB 7.6).
const MSG_FB_UPDATE: u8 = 0;
const MSG_SET_COLOUR_MAP: u8 = 1;
const MSG_BELL: u8 = 2;
const MSG_SERVER_CUT_TEXT: u8 = 3;

/// Rectangle encodings (RFB 7.7, 7.8).
const ENC_RAW: i32 = 0;
const ENC_DESKTOP_SIZE: i32 = -223;
/// The two cursor pseudo-encodings QEMU acts on (`ui/vnc.c` `set_encodings`):
/// RichCursor carries pixels in the client's format plus a 1 bpp mask,
/// AlphaCursor an s32 sub-encoding then host-order ARGB pixels.
const ENC_RICH_CURSOR: i32 = -239;
const ENC_ALPHA_CURSOR: i32 = -314;
/// Pseudo-rectangles a server may volunteer that carry no payload: LastRect,
/// QEMU's extended key event and audio announcements. Read past, never
/// acted on, so an unrequested one cannot desynchronise the stream.
const ENC_EMPTY_PSEUDO: [i32; 3] = [-224, -258, -259];
/// QEMU's LED-state pseudo-rectangle: one byte of payload.
const ENC_LED_STATE: i32 = -261;

/// The pixel format a capture asks for: 32 bpp, depth 24, little-endian
/// true colour with 8 bits per channel, red in the third byte — so a pixel
/// arrives as the bytes `B G R x`.
const CAPTURE_PIXEL_FORMAT: [u8; 16] = [
    32, // bits-per-pixel
    24, // depth
    0,  // big-endian-flag
    1,  // true-colour-flag
    0, 255, // red-max
    0, 255, // green-max
    0, 255, // blue-max
    16,  // red-shift
    8,   // green-shift
    0,   // blue-shift
    0, 0, 0, // padding
];

/// How long a capture may take end to end before it is given up on.
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct VncClient {
    stream: UnixStream,
    pub width: u16,
    pub height: u16,
}

impl VncClient {
    /// Connect to a VM's VNC unix socket and complete the RFB 3.8 handshake
    /// (Security type "None" — QEMU's default on these sockets).
    pub async fn connect(sock: &Path) -> Result<Self> {
        let mut stream = UnixStream::connect(sock)
            .await
            .with_context(|| format!("connecting VNC socket {}", sock.display()))?;

        // ProtocolVersion exchange.
        let mut server_ver = [0u8; 12];
        stream.read_exact(&mut server_ver).await?;
        stream.write_all(b"RFB 003.008\n").await?;

        // Security handshake (3.7+): server lists types, we pick None (1).
        let count = stream.read_u8().await?;
        if count == 0 {
            let reason_len = stream.read_u32().await? as usize;
            let mut reason = vec![0u8; reason_len];
            stream.read_exact(&mut reason).await?;
            bail!(
                "VNC connection refused: {}",
                String::from_utf8_lossy(&reason)
            );
        }
        let mut types = vec![0u8; count as usize];
        stream.read_exact(&mut types).await?;
        if !types.contains(&1) {
            bail!("VNC server requires authentication (unsupported); offered {types:?}");
        }
        stream.write_u8(1).await?; // chosen security: None
        let security_result = stream.read_u32().await?;
        if security_result != 0 {
            bail!("VNC security handshake failed");
        }

        // ClientInit (shared) → ServerInit.
        stream.write_u8(1).await?;
        let width = stream.read_u16().await?;
        let height = stream.read_u16().await?;
        let mut pixel_format = [0u8; 16];
        stream.read_exact(&mut pixel_format).await?;
        let name_len = stream.read_u32().await? as usize;
        let mut name = vec![0u8; name_len];
        stream.read_exact(&mut name).await?;

        Ok(Self {
            stream,
            width,
            height,
        })
    }

    /// Press or release one key (X11 keysym), RFB `KeyEvent` (type 4).
    pub async fn key(&mut self, keysym: u32, down: bool) -> Result<()> {
        let mut msg = [0u8; 8];
        msg[0] = 4;
        msg[1] = u8::from(down);
        msg[4..8].copy_from_slice(&keysym.to_be_bytes());
        self.stream.write_all(&msg).await?;
        Ok(())
    }

    /// Press all keysyms down (in order) then release them (reverse) — the
    /// chord semantics of a single QMP `send-key`.
    pub async fn chord(&mut self, keysyms: &[u32]) -> Result<()> {
        for &k in keysyms {
            self.key(k, true).await?;
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        // Hold briefly before release so slow real-mode guests (DOS/9x TUIs
        // polling the BIOS keyboard) reliably latch the keystroke; an 8ms hold
        // was dropped between menu redraws.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        for &k in keysyms.iter().rev() {
            self.key(k, false).await?;
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        Ok(())
    }

    /// Pointer state at (x, y) with the given button mask, RFB `PointerEvent`
    /// (type 5). Coordinates are framebuffer pixels, clamped to the screen.
    pub async fn pointer(&mut self, x: i64, y: i64, mask: u8) -> Result<()> {
        let cx = x.clamp(0, self.width.saturating_sub(1) as i64) as u16;
        let cy = y.clamp(0, self.height.saturating_sub(1) as i64) as u16;
        let mut msg = [0u8; 6];
        msg[0] = 5;
        msg[1] = mask;
        msg[2..4].copy_from_slice(&cx.to_be_bytes());
        msg[4..6].copy_from_slice(&cy.to_be_bytes());
        self.stream.write_all(&msg).await?;
        Ok(())
    }

    /// Move with no buttons pressed.
    pub async fn mouse_move(&mut self, x: i64, y: i64) -> Result<()> {
        self.pointer(x, y, 0).await
    }

    /// Press + release a button at (x, y).
    pub async fn click(&mut self, x: i64, y: i64, button: u8) -> Result<()> {
        self.pointer(x, y, 0).await?;
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        self.pointer(x, y, button).await?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        self.pointer(x, y, 0).await?;
        Ok(())
    }

    /// Switch this connection to streaming: ask for 32 bpp raw pixels and the
    /// encodings a recorder needs (Raw, DesktopSize, AlphaCursor, RichCursor),
    /// then request one non-incremental update of the whole screen so the
    /// first [`Self::next_update`] delivers a complete frame. QEMU sends the
    /// current cursor shape once after `SetEncodings`, so a guest with a
    /// hardware cursor announces it before the first frame.
    pub async fn start_stream(&mut self) -> Result<()> {
        let mut msg = Vec::with_capacity(20);
        msg.extend_from_slice(&[MSG_SET_PIXEL_FORMAT, 0, 0, 0]);
        msg.extend_from_slice(&CAPTURE_PIXEL_FORMAT);
        self.stream.write_all(&msg).await?;

        let encodings = [ENC_RAW, ENC_DESKTOP_SIZE, ENC_ALPHA_CURSOR, ENC_RICH_CURSOR];
        let mut msg = vec![MSG_SET_ENCODINGS, 0];
        msg.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for e in encodings {
            msg.extend_from_slice(&e.to_be_bytes());
        }
        self.stream.write_all(&msg).await?;
        self.request_full_update().await
    }

    /// Read one `FramebufferUpdate` and ask for the next incremental one.
    /// QEMU answers one request with one update, at most every ~30 ms and
    /// backing off to 3 s when nothing changes, so a client keeps exactly one
    /// request outstanding (research 05). Messages that are not updates are
    /// read past, as in [`Self::capture`]. Returns the decoded rectangles in
    /// order; a `DesktopSize` is folded into this client's dimensions and
    /// prompts a fresh non-incremental request for the new screen.
    pub async fn next_update(&mut self) -> Result<Vec<StreamEvent>> {
        loop {
            match self.stream.read_u8().await.context("reading VNC message")? {
                MSG_FB_UPDATE => {
                    let _pad = self.stream.read_u8().await?;
                    let rects = self.stream.read_u16().await?;
                    let mut events = Vec::with_capacity(rects as usize);
                    let mut resized = false;
                    for _ in 0..rects {
                        if let Some(ev) = self.read_stream_rect().await? {
                            if matches!(ev, StreamEvent::DesktopSize { .. }) {
                                resized = true;
                            }
                            events.push(ev);
                        }
                    }
                    if resized {
                        self.request_full_update().await?;
                    } else {
                        self.request_incremental_update().await?;
                    }
                    return Ok(events);
                }
                MSG_SET_COLOUR_MAP => {
                    let mut head = [0u8; 5];
                    self.stream.read_exact(&mut head).await?;
                    let count = u16::from_be_bytes([head[3], head[4]]) as usize;
                    self.skip(count * 6).await?;
                }
                MSG_BELL => {}
                MSG_SERVER_CUT_TEXT => {
                    let mut pad = [0u8; 3];
                    self.stream.read_exact(&mut pad).await?;
                    let len = self.stream.read_u32().await? as usize;
                    self.skip(len).await?;
                }
                other => bail!("VNC server sent unsupported message type {other}"),
            }
        }
    }

    /// One rectangle of a streamed update, decoded. `None` for a pseudo-
    /// rectangle that carries nothing a recorder uses.
    async fn read_stream_rect(&mut self) -> Result<Option<StreamEvent>> {
        let x = self.stream.read_u16().await?;
        let y = self.stream.read_u16().await?;
        let w = self.stream.read_u16().await?;
        let h = self.stream.read_u16().await?;
        let encoding = self.stream.read_i32().await?;
        match encoding {
            ENC_RAW => {
                let mut bgrx = vec![0u8; w as usize * h as usize * 4];
                self.stream.read_exact(&mut bgrx).await?;
                Ok(Some(StreamEvent::Rect { x, y, w, h, bgrx }))
            }
            ENC_DESKTOP_SIZE => {
                self.width = w;
                self.height = h;
                Ok(Some(StreamEvent::DesktopSize { w, h }))
            }
            ENC_ALPHA_CURSOR => {
                // QEMU writes the sub-encoding (always RAW) and then the
                // cursor's `uint32_t` ARGB pixels in host order — on every
                // host vmlab runs on that is the bytes B G R A, not the RGBA
                // rfbproto describes. The alpha is premultiplied as the guest
                // uploaded it to its cursor plane (measured on GNOME and KDE),
                // and the recorder blends it as such.
                let sub = self.stream.read_i32().await?;
                if sub != ENC_RAW {
                    bail!("VNC alpha cursor uses unsupported sub-encoding {sub}");
                }
                let mut px = vec![0u8; w as usize * h as usize * 4];
                self.stream.read_exact(&mut px).await?;
                let rgba = px
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
                    .collect();
                Ok(Some(StreamEvent::Cursor {
                    hotspot: (x, y),
                    w,
                    h,
                    rgba,
                }))
            }
            ENC_RICH_CURSOR => {
                let mut px = vec![0u8; w as usize * h as usize * 4];
                self.stream.read_exact(&mut px).await?;
                let stride = (w as usize).div_ceil(8);
                let mut mask = vec![0u8; stride * h as usize];
                self.stream.read_exact(&mut mask).await?;
                let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
                for row in 0..h as usize {
                    for col in 0..w as usize {
                        let i = (row * w as usize + col) * 4;
                        let bit = mask[row * stride + col / 8] >> (7 - (col % 8)) & 1;
                        rgba.extend_from_slice(&[
                            px[i + 2],
                            px[i + 1],
                            px[i],
                            if bit == 1 { 255 } else { 0 },
                        ]);
                    }
                }
                Ok(Some(StreamEvent::Cursor {
                    hotspot: (x, y),
                    w,
                    h,
                    rgba,
                }))
            }
            ENC_LED_STATE => {
                self.skip(1).await?;
                Ok(None)
            }
            e if ENC_EMPTY_PSEUDO.contains(&e) => Ok(None),
            other => bail!("VNC server sent unrequested encoding {other}"),
        }
    }

    /// `FramebufferUpdateRequest` (type 3), incremental, whole screen.
    async fn request_incremental_update(&mut self) -> Result<()> {
        let mut msg = [0u8; 10];
        msg[0] = MSG_FB_UPDATE_REQUEST;
        msg[1] = 1; // incremental: yes
        msg[6..8].copy_from_slice(&self.width.to_be_bytes());
        msg[8..10].copy_from_slice(&self.height.to_be_bytes());
        self.stream.write_all(&msg).await?;
        Ok(())
    }

    /// Read the whole framebuffer once: ask for 32 bpp raw pixels, request
    /// one non-incremental update of the full screen, and assemble the
    /// rectangles that answer it. Server messages that are not framebuffer
    /// updates (`Bell`, `ServerCutText`, colour maps) are read past. A
    /// `DesktopSize` rectangle resizes the canvas and asks again.
    pub async fn capture(&mut self) -> Result<RgbImage> {
        tokio::time::timeout(CAPTURE_TIMEOUT, self.capture_inner())
            .await
            .context("VNC framebuffer capture timed out")?
    }

    async fn capture_inner(&mut self) -> Result<RgbImage> {
        let mut msg = Vec::with_capacity(20);
        msg.extend_from_slice(&[MSG_SET_PIXEL_FORMAT, 0, 0, 0]);
        msg.extend_from_slice(&CAPTURE_PIXEL_FORMAT);
        self.stream.write_all(&msg).await?;

        let mut msg = vec![MSG_SET_ENCODINGS, 0];
        msg.extend_from_slice(&1u16.to_be_bytes());
        msg.extend_from_slice(&ENC_RAW.to_be_bytes());
        self.stream.write_all(&msg).await?;

        let mut canvas = Canvas::new(self.width, self.height)?;
        self.request_full_update().await?;
        loop {
            match self.stream.read_u8().await.context("reading VNC message")? {
                MSG_FB_UPDATE => {
                    let _pad = self.stream.read_u8().await?;
                    let rects = self.stream.read_u16().await?;
                    for _ in 0..rects {
                        self.read_rect(&mut canvas).await?;
                    }
                    if canvas.complete() {
                        return Ok(canvas.image);
                    }
                    // A resize or a partial answer: ask for the whole
                    // (possibly new) screen again.
                    self.request_full_update().await?;
                }
                MSG_SET_COLOUR_MAP => {
                    let mut head = [0u8; 5];
                    self.stream.read_exact(&mut head).await?;
                    let count = u16::from_be_bytes([head[3], head[4]]) as usize;
                    self.skip(count * 6).await?;
                }
                MSG_BELL => {}
                MSG_SERVER_CUT_TEXT => {
                    let mut pad = [0u8; 3];
                    self.stream.read_exact(&mut pad).await?;
                    let len = self.stream.read_u32().await? as usize;
                    self.skip(len).await?;
                }
                other => bail!("VNC server sent unsupported message type {other}"),
            }
        }
    }

    /// `FramebufferUpdateRequest` (type 3), non-incremental, whole screen.
    async fn request_full_update(&mut self) -> Result<()> {
        let mut msg = [0u8; 10];
        msg[0] = MSG_FB_UPDATE_REQUEST;
        msg[1] = 0; // incremental: no
        msg[6..8].copy_from_slice(&self.width.to_be_bytes());
        msg[8..10].copy_from_slice(&self.height.to_be_bytes());
        self.stream.write_all(&msg).await?;
        Ok(())
    }

    /// One rectangle of a `FramebufferUpdate`, painted onto `canvas`.
    async fn read_rect(&mut self, canvas: &mut Canvas) -> Result<()> {
        let x = self.stream.read_u16().await?;
        let y = self.stream.read_u16().await?;
        let w = self.stream.read_u16().await?;
        let h = self.stream.read_u16().await?;
        match self.stream.read_i32().await? {
            ENC_RAW => {
                let mut px = vec![0u8; w as usize * h as usize * 4];
                self.stream.read_exact(&mut px).await?;
                canvas.paint(x, y, w, h, &px)
            }
            ENC_DESKTOP_SIZE => {
                self.width = w;
                self.height = h;
                *canvas = Canvas::new(w, h)?;
                Ok(())
            }
            other => bail!("VNC server sent unrequested encoding {other}"),
        }
    }

    /// Read and discard `n` bytes.
    async fn skip(&mut self, n: usize) -> Result<()> {
        let mut buf = vec![0u8; n];
        self.stream.read_exact(&mut buf).await?;
        Ok(())
    }
}

/// One decoded rectangle of a streamed `FramebufferUpdate`
/// ([`VncClient::next_update`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// Raw pixels for the region, as `B G R x` bytes.
    Rect {
        x: u16,
        y: u16,
        w: u16,
        h: u16,
        bgrx: Vec<u8>,
    },
    /// The framebuffer is now `w`x`h`; every earlier pixel is stale.
    DesktopSize { w: u16, h: u16 },
    /// The guest's hardware cursor shape: straight-alpha RGBA pixels, with
    /// the hotspot in cursor pixels. Only ever sent by a display device that
    /// has a cursor plane (research 05); std VGA, bochs and cirrus guests
    /// paint their own pointer into the framebuffer and send none.
    Cursor {
        hotspot: (u16, u16),
        w: u16,
        h: u16,
        rgba: Vec<u8>,
    },
}

/// A framebuffer being assembled from update rectangles, with a record of
/// which pixels have arrived so a capture knows when it has the whole screen.
struct Canvas {
    image: RgbImage,
    covered: Vec<bool>,
    remaining: usize,
}

impl Canvas {
    fn new(width: u16, height: u16) -> Result<Self> {
        if width == 0 || height == 0 {
            bail!("VNC framebuffer is empty ({width}x{height})");
        }
        let n = width as usize * height as usize;
        Ok(Self {
            image: RgbImage::new(width.into(), height.into()),
            covered: vec![false; n],
            remaining: n,
        })
    }

    /// Paint a raw rectangle of `B G R x` pixels at `(x, y)`.
    fn paint(&mut self, x: u16, y: u16, w: u16, h: u16, px: &[u8]) -> Result<()> {
        let (fw, fh) = (self.image.width(), self.image.height());
        if u32::from(x) + u32::from(w) > fw || u32::from(y) + u32::from(h) > fh {
            bail!("VNC rectangle {w}x{h}+{x}+{y} lies outside the {fw}x{fh} framebuffer");
        }
        for row in 0..u32::from(h) {
            for col in 0..u32::from(w) {
                let i = ((row * u32::from(w) + col) * 4) as usize;
                let (px_x, px_y) = (u32::from(x) + col, u32::from(y) + row);
                self.image
                    .put_pixel(px_x, px_y, image::Rgb([px[i + 2], px[i + 1], px[i]]));
                let seen = &mut self.covered[(px_y * fw + px_x) as usize];
                if !*seen {
                    *seen = true;
                    self.remaining -= 1;
                }
            }
        }
        Ok(())
    }

    fn complete(&self) -> bool {
        self.remaining == 0
    }
}

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;
