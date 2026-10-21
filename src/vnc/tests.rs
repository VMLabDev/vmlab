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

/// The client's messages once the server has recorded `n` of them: the last
/// request is written just before `next_update` returns, so the server may
/// not have read it yet.
async fn received(server: &fake::FakeRfb, n: usize) -> Vec<Vec<u8>> {
    for _ in 0..200 {
        let got = server.received.lock().unwrap().clone();
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    server.received.lock().unwrap().clone()
}

/// The incremental `FramebufferUpdateRequest` a stream sends after each
/// update it receives.
fn incremental_request(w: u16, h: u16) -> Vec<u8> {
    let mut m = vec![3, 1, 0, 0, 0, 0];
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    m
}

/// A stream asks for Raw, DesktopSize, AlphaCursor and RichCursor, takes one
/// full update, and then keeps exactly one incremental request outstanding,
/// decoding rectangles and a RichCursor along the way.
#[tokio::test]
async fn stream_requests_incrementally_and_decodes_rects_and_a_rich_cursor() {
    use super::StreamEvent;
    let img = pattern(16, 10);
    let cursor = fake::rich_cursor(
        1,
        0,
        2,
        2,
        &[[255, 0, 0], [0, 255, 0], [0, 0, 255], [9, 9, 9]],
        &[true, false, true, false],
    );
    let server = fake::spawn(
        16,
        10,
        vec![
            update(&[cursor, raw_rect(&img, 0, 0, 16, 10)]),
            update(&[raw_rect(&img, 2, 3, 4, 2)]),
        ],
    );

    let mut c = VncClient::connect(&server.path).await.unwrap();
    c.start_stream().await.unwrap();
    let first = c.next_update().await.unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(
        first[0],
        StreamEvent::Cursor {
            hotspot: (1, 0),
            w: 2,
            h: 2,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 255, 9, 9, 9, 0],
        }
    );
    assert!(
        matches!(&first[1], StreamEvent::Rect { x: 0, y: 0, w: 16, h: 10, bgrx } if bgrx.len() == 16 * 10 * 4)
    );
    let second = c.next_update().await.unwrap();
    assert!(matches!(
        &second[0],
        StreamEvent::Rect {
            x: 2,
            y: 3,
            w: 4,
            h: 2,
            ..
        }
    ));

    let sent = received(&server, 5).await;
    let mut enc = vec![2, 0, 0, 4];
    for e in [0i32, -223, -314, -239] {
        enc.extend_from_slice(&e.to_be_bytes());
    }
    assert_eq!(
        sent[1], enc,
        "SetEncodings: Raw, DesktopSize, AlphaCursor, RichCursor"
    );
    assert_eq!(
        sent[2],
        full_request(16, 10),
        "first request is non-incremental"
    );
    assert_eq!(sent[3], incremental_request(16, 10));
    assert_eq!(sent[4], incremental_request(16, 10));
    assert_eq!(sent.len(), 5, "one request per update received");
}

/// An AlphaCursor is decoded as QEMU sends it: host-order ARGB `u32`s, so
/// the bytes on the wire are `B G R A`, and the pixel comes out as straight
/// RGBA with red and blue in the right places.
#[tokio::test]
async fn stream_decodes_an_alpha_cursor_in_qemus_byte_order() {
    use super::StreamEvent;
    let cursor = fake::alpha_cursor(3, 4, 1, 2, &[[0x80, 0x10, 0x20, 0x30], [0, 0, 0, 0]]);
    let server = fake::spawn(8, 8, vec![update(&[cursor])]);

    let mut c = VncClient::connect(&server.path).await.unwrap();
    c.start_stream().await.unwrap();
    let events = c.next_update().await.unwrap();
    assert_eq!(
        events,
        vec![StreamEvent::Cursor {
            hotspot: (3, 4),
            w: 1,
            h: 2,
            rgba: vec![0x10, 0x20, 0x30, 0x80, 0, 0, 0, 0],
        }]
    );
}

/// A `DesktopSize` resizes the client and is followed by a full request for
/// the new screen; an unrequested pseudo-rectangle is read past.
#[tokio::test]
async fn stream_follows_desktop_size_and_ignores_an_unrequested_pseudo_rect() {
    use super::StreamEvent;
    let img = pattern(20, 12);
    let server = fake::spawn(
        8,
        6,
        vec![
            update(&[fake::unrequested_pseudo(), desktop_size(20, 12)]),
            update(&[raw_rect(&img, 0, 0, 20, 12)]),
        ],
    );

    let mut c = VncClient::connect(&server.path).await.unwrap();
    c.start_stream().await.unwrap();
    assert_eq!(
        c.next_update().await.unwrap(),
        vec![StreamEvent::DesktopSize { w: 20, h: 12 }]
    );
    assert_eq!((c.width, c.height), (20, 12));
    let next = c.next_update().await.unwrap();
    assert!(matches!(&next[0], StreamEvent::Rect { w: 20, h: 12, .. }));
    let sent = received(&server, 5).await;
    assert_eq!(
        sent[3],
        full_request(20, 12),
        "a resize asks for the whole new screen"
    );
    assert_eq!(sent[4], incremental_request(20, 12));
}

/// A server that closes the connection ends the stream with an error, and
/// a new connection to the same socket starts over.
#[tokio::test]
async fn stream_reports_a_dropped_connection_and_reconnects() {
    let img = pattern(4, 4);
    let server = fake::spawn(
        4,
        4,
        vec![update(&[raw_rect(&img, 0, 0, 4, 4)]), fake::CLOSE],
    );

    let mut c = VncClient::connect(&server.path).await.unwrap();
    c.start_stream().await.unwrap();
    c.next_update().await.unwrap();
    assert!(c.next_update().await.is_err(), "closed mid-stream");

    let mut again = VncClient::connect(&server.path).await.unwrap();
    again.start_stream().await.unwrap();
    assert_eq!(again.next_update().await.unwrap().len(), 1);
    assert_eq!(
        server.connections.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

/// Taking the socket away makes a connect fail; bringing it back on the same
/// path lets the next connect through, as across a `vm restart`.
#[tokio::test]
async fn a_vanished_socket_refuses_connections_until_it_reappears() {
    let img = pattern(4, 4);
    let mut server = fake::spawn(4, 4, vec![]);
    server.vanish();
    assert!(VncClient::connect(&server.path).await.is_err());
    server.reappear(6, 6, vec![update(&[raw_rect(&img, 0, 0, 4, 4)])]);
    let c = VncClient::connect(&server.path).await.unwrap();
    assert_eq!((c.width, c.height), (6, 6));
}
