//! Framebuffer capture against the in-process fake RFB server.

use super::VncClient;
use super::fake::{self, bell, cut_text, desktop_size, pattern, raw_rect, update};

/// The `FramebufferUpdateRequest` a capture sends: non-incremental, whole
/// `w`x`h` screen.
fn full_request(w: u16, h: u16) -> Vec<u8> {
    let mut m = vec![3, 0, 0, 0, 0, 0];
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    m
}

/// A known pattern delivered as two rectangles comes back pixel for pixel,
/// after the client asked for 32 bpp true colour, Raw only, and one full
/// non-incremental update.
#[tokio::test]
async fn capture_assembles_a_framebuffer_sent_as_two_rectangles() {
    let img = pattern(16, 10);
    let reply = update(&[raw_rect(&img, 0, 0, 16, 4), raw_rect(&img, 0, 4, 16, 6)]);
    let server = fake::spawn(16, 10, vec![reply]);

    let mut c = VncClient::connect(&server.path).await.unwrap();
    let got = c.capture().await.unwrap();

    assert_eq!(got.dimensions(), (16, 10));
    assert_eq!(got, img);
    let sent = server.received.lock().unwrap().clone();
    let mut set_pf = vec![0, 0, 0, 0];
    set_pf.extend_from_slice(&super::CAPTURE_PIXEL_FORMAT);
    assert_eq!(sent[0], set_pf, "SetPixelFormat 32bpp true colour");
    assert_eq!(
        sent[1],
        vec![2, 0, 0, 1, 0, 0, 0, 0],
        "SetEncodings: Raw only"
    );
    assert_eq!(sent[2], full_request(16, 10));
    assert_eq!(sent.len(), 3, "one request answered by one update");
}

/// `Bell` and `ServerCutText` arriving before the update are read past
/// without desynchronising the stream.
#[tokio::test]
async fn capture_reads_past_bell_and_cut_text_before_the_update() {
    let img = pattern(8, 6);
    let mut reply = bell();
    reply.extend(cut_text("clipboard"));
    reply.extend(bell());
    reply.extend(update(&[raw_rect(&img, 0, 0, 8, 6)]));
    let server = fake::spawn(8, 6, vec![reply]);

    let mut c = VncClient::connect(&server.path).await.unwrap();
    assert_eq!(c.capture().await.unwrap(), img);
}

/// An update that leaves part of the screen unpainted is followed by
/// another full request, and the two answers are assembled together.
#[tokio::test]
async fn capture_asks_again_when_an_update_covers_part_of_the_screen() {
    let img = pattern(12, 8);
    let server = fake::spawn(
        12,
        8,
        vec![
            update(&[raw_rect(&img, 0, 0, 12, 3)]),
            update(&[raw_rect(&img, 0, 3, 12, 5)]),
        ],
    );

    let mut c = VncClient::connect(&server.path).await.unwrap();
    assert_eq!(c.capture().await.unwrap(), img);
    let requests = server
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m[0] == 3)
        .count();
    assert_eq!(requests, 2);
}

/// A `DesktopSize` pseudo-rectangle resizes the canvas, and the capture
/// asks for the new whole screen and returns it at the new size.
#[tokio::test]
async fn capture_follows_a_desktop_size_change() {
    let img = pattern(20, 12);
    let server = fake::spawn(
        8,
        6,
        vec![
            update(&[desktop_size(20, 12)]),
            update(&[raw_rect(&img, 0, 0, 20, 12)]),
        ],
    );

    let mut c = VncClient::connect(&server.path).await.unwrap();
    assert_eq!(c.capture().await.unwrap(), img);
    assert_eq!((c.width, c.height), (20, 12));
    let sent = server.received.lock().unwrap().clone();
    assert_eq!(sent.last().unwrap(), &full_request(20, 12));
}

/// A rectangle reaching past the framebuffer is an error, not a panic.
#[tokio::test]
async fn capture_rejects_a_rectangle_outside_the_framebuffer() {
    let img = pattern(8, 8);
    let server = fake::spawn(4, 4, vec![update(&[raw_rect(&img, 0, 0, 8, 8)])]);

    let mut c = VncClient::connect(&server.path).await.unwrap();
    let err = c.capture().await.unwrap_err();
    assert!(err.to_string().contains("outside"), "{err:#}");
}
