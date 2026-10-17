//! An in-process RFB 3.8 server on a unix socket, for tests of the capture
//! path here and in the display module.
//!
//! It completes the auth-less handshake, records every client message, and
//! answers the n-th `FramebufferUpdateRequest` on each connection with the
//! n-th scripted reply — raw server bytes, built with the helpers below.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use image::RgbImage;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

pub(crate) struct FakeRfb {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
    /// Every client message after `ClientInit`, whole, in arrival order.
    pub received: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// Serve a `width`x`height` framebuffer, answering update requests with
/// `replies` in order.
pub(crate) fn spawn(width: u16, height: u16, replies: Vec<Vec<u8>>) -> FakeRfb {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("vnc.sock");
    let listener = UnixListener::bind(&path).expect("bind fake rfb socket");
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let rec = Arc::clone(&received);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let replies = replies.clone();
            let rec = Arc::clone(&rec);
            tokio::spawn(serve(stream, width, height, replies, rec));
        }
    });
    FakeRfb {
        _dir: dir,
        path,
        received,
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
