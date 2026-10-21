//! An in-process RFB 3.8 server on a unix socket, for tests of the capture
//! path here and in the display module.
//!
//! It completes the auth-less handshake, records every client message, and
//! answers the n-th `FramebufferUpdateRequest` on each connection with the
//! n-th scripted reply — raw server bytes, built with the helpers below. An
//! empty reply closes the connection instead ([`CLOSE`]), the way QEMU drops
//! a client it has given up on; a fresh connection starts the replies over.
//! [`FakeRfb::vanish`] and [`FakeRfb::reappear`] take the socket away and
//! bring it back on the same path, which is what a `vm restart` does to a
//! recorder (§11).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use image::RgbImage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

pub(crate) struct FakeRfb {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    /// Every client message after `ClientInit`, whole, in arrival order.
    pub received: Arc<Mutex<Vec<Vec<u8>>>>,
    /// How many connections the server has accepted so far.
    pub connections: Arc<AtomicUsize>,
    accept: tokio::task::JoinHandle<()>,
    /// Flipped by `vanish` so every open connection of this generation ends.
    closed: tokio::sync::watch::Sender<bool>,
}

/// The reply that closes the connection instead of answering.
pub(crate) const CLOSE: Vec<u8> = Vec::new();

/// Serve a `width`x`height` framebuffer, answering update requests with
/// `replies` in order.
pub(crate) fn spawn(width: u16, height: u16, replies: Vec<Vec<u8>>) -> FakeRfb {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vnc.sock");
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let connections = Arc::new(AtomicUsize::new(0));
    let (accept, closed) = listen(&path, width, height, replies, &received, &connections);
    FakeRfb {
        _dir: dir,
        path,
        received,
        connections,
        accept,
        closed,
    }
}

fn listen(
    path: &std::path::Path,
    width: u16,
    height: u16,
    replies: Vec<Vec<u8>>,
    received: &Arc<Mutex<Vec<Vec<u8>>>>,
    connections: &Arc<AtomicUsize>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Sender<bool>,
) {
    let listener = UnixListener::bind(path).expect("bind fake rfb socket");
    let rec = Arc::clone(received);
    let conns = Arc::clone(connections);
    let (closed, closed_rx) = tokio::sync::watch::channel(false);
    let accept = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            conns.fetch_add(1, Ordering::SeqCst);
            let replies = replies.clone();
            let rec = Arc::clone(&rec);
            let mut closed = closed_rx.clone();
            tokio::spawn(async move {
                tokio::select! {
                    r = serve(stream, width, height, replies, rec) => r,
                    _ = closed.changed() => Ok(()),
                }
            });
        }
    });
    (accept, closed)
}

impl FakeRfb {
    /// Stop listening, close every open connection and remove the socket,
    /// as a QEMU that exited would.
    pub(crate) fn vanish(&self) {
        self.accept.abort();
        let _ = self.closed.send(true);
        let _ = std::fs::remove_file(&self.path);
    }

    /// Listen again on the same path, as the next QEMU would, serving a
    /// `width`x`height` screen with `replies`.
    pub(crate) fn reappear(&mut self, width: u16, height: u16, replies: Vec<Vec<u8>>) {
        let (accept, closed) = listen(
            &self.path,
            width,
            height,
            replies,
            &self.received,
            &self.connections,
        );
        self.accept = accept;
        self.closed = closed;
    }
}

async fn serve(
    mut s: UnixStream,
    width: u16,
    height: u16,
    replies: Vec<Vec<u8>>,
    rec: Arc<Mutex<Vec<Vec<u8>>>>,
) -> std::io::Result<()> {
    s.write_all(b"RFB 003.008\n").await?;
    let mut ver = [0u8; 12];
    s.read_exact(&mut ver).await?;
    s.write_all(&[1, 1]).await?; // one security type: None
    s.read_u8().await?;
    s.write_u32(0).await?; // SecurityResult OK
    s.read_u8().await?; // ClientInit
    let mut init = Vec::new();
    init.extend_from_slice(&width.to_be_bytes());
    init.extend_from_slice(&height.to_be_bytes());
    // A server-native format the client must override: 16 bpp.
    init.extend_from_slice(&[16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0]);
    init.extend_from_slice(&4u32.to_be_bytes());
    init.extend_from_slice(b"fake");
    s.write_all(&init).await?;

    let mut replies = replies.into_iter();
    loop {
        let kind = s.read_u8().await?;
        let rest = match kind {
            0 => 19,
            2 => {
                let mut head = [0u8; 3];
                s.read_exact(&mut head).await?;
                let n = u16::from_be_bytes([head[1], head[2]]) as usize;
                let mut body = vec![0u8; n * 4];
                s.read_exact(&mut body).await?;
                let mut msg = vec![kind];
                msg.extend_from_slice(&head);
                msg.extend_from_slice(&body);
                rec.lock().unwrap().push(msg);
                continue;
            }
            3 => 9,
            4 => 7,
            5 => 5,
            other => panic!("fake rfb: unexpected client message {other}"),
        };
        let mut msg = vec![kind; 1 + rest];
        s.read_exact(&mut msg[1..]).await?;
        rec.lock().unwrap().push(msg);
        if kind == 3
            && let Some(reply) = replies.next()
        {
            if reply.is_empty() {
                return Ok(());
            }
            s.write_all(&reply).await?;
        }
    }
}

/// A `FramebufferUpdate` carrying already-encoded rectangles.
pub(crate) fn update(rects: &[Vec<u8>]) -> Vec<u8> {
    let mut m = vec![0, 0];
    m.extend_from_slice(&(rects.len() as u16).to_be_bytes());
    for r in rects {
        m.extend_from_slice(r);
    }
    m
}

/// The region `(x, y, w, h)` of `img` as a Raw rectangle in the capture's
/// pixel format (`B G R x`).
pub(crate) fn raw_rect(img: &RgbImage, x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
    let mut r = rect_header(x, y, w, h, 0);
    for row in y..y + h {
        for col in x..x + w {
            let p = img.get_pixel(col.into(), row.into());
            r.extend_from_slice(&[p[2], p[1], p[0], 0]);
        }
    }
    r
}

/// A `DesktopSize` pseudo-rectangle announcing a `w`x`h` framebuffer.
pub(crate) fn desktop_size(w: u16, h: u16) -> Vec<u8> {
    rect_header(0, 0, w, h, -223)
}

/// An AlphaCursor pseudo-rectangle as QEMU sends it: hotspot `(hx, hy)`,
/// `w`x`h`, a RAW sub-encoding, then one host-order ARGB `u32` per pixel —
/// the bytes `B G R A` on a little-endian host. `argb` is one `[a, r, g, b]`
/// per pixel.
pub(crate) fn alpha_cursor(hx: u16, hy: u16, w: u16, h: u16, argb: &[[u8; 4]]) -> Vec<u8> {
    assert_eq!(argb.len(), w as usize * h as usize);
    let mut r = rect_header(hx, hy, w, h, -314);
    r.extend_from_slice(&0i32.to_be_bytes());
    for [a, red, g, b] in argb {
        r.extend_from_slice(&u32::from_be_bytes([*a, *red, *g, *b]).to_le_bytes());
    }
    r
}

/// A RichCursor pseudo-rectangle: `w`x`h` pixels in the capture's pixel
/// format (`B G R x`) then a 1 bpp mask, rows padded to a byte, MSB first.
/// `rgb` is one `[r, g, b]` per pixel and `opaque` one flag per pixel.
pub(crate) fn rich_cursor(
    hx: u16,
    hy: u16,
    w: u16,
    h: u16,
    rgb: &[[u8; 3]],
    opaque: &[bool],
) -> Vec<u8> {
    assert_eq!(rgb.len(), w as usize * h as usize);
    assert_eq!(opaque.len(), rgb.len());
    let mut r = rect_header(hx, hy, w, h, -239);
    for [red, g, b] in rgb {
        r.extend_from_slice(&[*b, *g, *red, 0]);
    }
    let stride = (w as usize).div_ceil(8);
    for row in 0..h as usize {
        let mut bytes = vec![0u8; stride];
        for col in 0..w as usize {
            if opaque[row * w as usize + col] {
                bytes[col / 8] |= 0x80 >> (col % 8);
            }
        }
        r.extend_from_slice(&bytes);
    }
    r
}

/// A pseudo-rectangle with no payload and an encoding the client never asked
/// for (QEMU's extended key event announcement).
pub(crate) fn unrequested_pseudo() -> Vec<u8> {
    rect_header(0, 0, 0, 0, -258)
}

fn rect_header(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
    let mut r = Vec::new();
    for v in [x, y, w, h] {
        r.extend_from_slice(&v.to_be_bytes());
    }
    r.extend_from_slice(&encoding.to_be_bytes());
    r
}

/// A `Bell` message.
pub(crate) fn bell() -> Vec<u8> {
    vec![2]
}

/// A `ServerCutText` message carrying `text`.
pub(crate) fn cut_text(text: &str) -> Vec<u8> {
    let mut m = vec![3, 0, 0, 0];
    m.extend_from_slice(&(text.len() as u32).to_be_bytes());
    m.extend_from_slice(text.as_bytes());
    m
}

/// A `w`x`h` image where every pixel differs from its neighbours, so a
/// misplaced or channel-swapped pixel shows up.
pub(crate) fn pattern(w: u32, h: u32) -> RgbImage {
    RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([
            (x * 7 + y) as u8,
            (y * 13 + x * 3) as u8,
            (x ^ y) as u8 | 0x40,
        ])
    })
}
