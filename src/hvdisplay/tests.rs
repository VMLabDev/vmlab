//! The native client against vmlab-hv's own server core (`fake`): real
//! sockets, real memfds, real releases.

use std::time::Duration;

use vmlab_hv_display::{Cursor, Message, Rect};

use super::fake::{self, pattern};
use super::*;

/// The next batch that is not empty, within a few seconds.
async fn next_events(stream: &mut NativeStream) -> Vec<StreamEvent> {
    loop {
        let batch = tokio::time::timeout(Duration::from_secs(5), stream.next_update())
            .await
            .expect("an update in time")
            .expect("the stream is up");
        if !batch.is_empty() {
            return batch;
        }
    }
}

/// A screenshot is the whole frame, RGB, whatever the damage said.
#[tokio::test]
async fn a_capture_is_the_whole_frame_as_rgb() {
    let display = fake::spawn(16, 8);
    let img = capture(&display.path).await.unwrap();
    assert_eq!((img.width(), img.height()), (16, 8));
    // `pattern`: (x, y) is RGB (x, y, x ^ y).
    assert_eq!(img.get_pixel(5, 3).0, [5, 3, 5 ^ 3]);
    assert_eq!(img.get_pixel(15, 7).0, [15, 7, 15 ^ 7]);
}

#[tokio::test]
async fn capture_waits_past_placeholder_for_guest_black() {
    let display = fake::spawn(16, 8);
    display.placeholder();
    let path = display.path.clone();
    let capture = tokio::spawn(async move { super::capture(&path).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!capture.is_finished(), "a placeholder is not a screenshot");
    display.show(16, 8, vec![0; 16 * 8 * 4], vec![]);
    let image = tokio::time::timeout(Duration::from_secs(2), capture)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(image.as_raw().iter().all(|b| *b == 0));
}

#[tokio::test]
async fn repeated_placeholders_do_not_extend_capture_timeout() {
    let display = fake::spawn(16, 8);
    display.placeholder();
    let path = display.path.clone();
    let capture =
        tokio::task::spawn_blocking(move || capture_blocking(&path, Duration::from_millis(200)));
    for _ in 0..10 {
        display.placeholder();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        capture.is_finished(),
        "placeholder traffic must not renew the deadline"
    );
    let error = format!("{:#}", capture.await.unwrap().unwrap_err());
    assert!(error.contains("display.sock"), "{error}");
}

#[tokio::test]
async fn stream_releases_placeholders_without_rectangles() {
    let display = fake::spawn(16, 8);
    display.placeholder();
    let mut stream = NativeStream::connect(&display.path).await.unwrap();
    // Publishing more than the ring size also proves buffers are released.
    for _ in 0..5 {
        let events = stream.next_update().await.unwrap();
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Rect { .. })),
            "{events:?}"
        );
        display.placeholder();
    }
    display.show(16, 8, vec![0; 16 * 8 * 4], vec![]);
    let events = next_events(&mut stream).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Rect { bgrx, .. } if bgrx.iter().all(|b| *b == 0))),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_capture_of_a_socket_nobody_serves_fails_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let err = format!("{:#}", capture(&path).await.unwrap_err());
    assert!(err.contains("display.sock"), "{err}");
}

/// The recorder's stream: the size up front, a full first frame, then only
/// the damage, the cursor's shape and position, and a resize.
#[tokio::test]
async fn the_stream_carries_damage_cursor_and_resize() {
    let display = fake::spawn(16, 8);
    let mut stream = NativeStream::connect(&display.path).await.unwrap();
    assert_eq!((stream.width, stream.height), (16, 8));

    // The first frame is full.
    let first = next_events(&mut stream).await;
    let StreamEvent::Rect { x, y, w, h, bgrx } = &first[0] else {
        panic!("{first:?}")
    };
    assert_eq!((*x, *y, *w, *h), (0, 0, 16, 8));
    assert_eq!(bgrx, &pattern(16, 8));

    // A change in one rectangle arrives as that rectangle's pixels.
    let mut pixels = pattern(16, 8);
    for row in 2..4 {
        for col in 4..7 {
            let i = (row * 16 + col) * 4;
            pixels[i..i + 4].copy_from_slice(&[1, 2, 3, 0]);
        }
    }
    display.show(
        16,
        8,
        pixels,
        vec![Rect {
            x: 4,
            y: 2,
            width: 3,
            height: 2,
        }],
    );
    let changed = next_events(&mut stream).await;
    assert_eq!(
        changed,
        vec![StreamEvent::Rect {
            x: 4,
            y: 2,
            w: 3,
            h: 2,
            bgrx: [1, 2, 3, 0].repeat(6),
        }]
    );

    // The cursor: premultiplied B G R A words become R G B A, and the
    // position arrives as its own event.
    display.cursor(Cursor {
        width: 1,
        height: 1,
        hot_x: 0,
        hot_y: 0,
        argb: vec![10, 20, 30, 255],
        x: 7,
        y: 3,
        visible: true,
    });
    let mut cursor = Vec::new();
    while cursor.len() < 2 {
        cursor.extend(next_events(&mut stream).await);
    }
    assert!(
        cursor.contains(&StreamEvent::Cursor {
            hotspot: (0, 0),
            w: 1,
            h: 1,
            rgba: vec![30, 20, 10, 255],
        }),
        "{cursor:?}"
    );
    assert!(
        cursor.contains(&StreamEvent::Pointer {
            x: 7,
            y: 3,
            visible: true
        }),
        "{cursor:?}"
    );

    // A resize: the new size, then the new picture whole.
    display.show(
        8,
        4,
        pattern(8, 4),
        vec![Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 4,
        }],
    );
    let mut resized = Vec::new();
    while !resized
        .iter()
        .any(|e| matches!(e, StreamEvent::Rect { w: 8, h: 4, .. }))
    {
        resized.extend(next_events(&mut stream).await);
    }
    assert_eq!(resized[0], StreamEvent::DesktopSize { w: 8, h: 4 });
}

/// The stream ends when the server goes, so the recorder can reconnect.
#[tokio::test]
async fn the_stream_ends_when_the_socket_goes() {
    let display = fake::spawn(4, 4);
    let mut stream = NativeStream::connect(&display.path).await.unwrap();
    next_events(&mut stream).await;
    display.vanish();
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if stream.next_update().await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(end.is_ok(), "the stream noticed the server going");
}

/// Input arrives in order, and survives the server going and coming back.
#[test]
fn input_arrives_in_order_and_reconnects() {
    let display = fake::spawn(4, 4);
    let mut input = InputConn::new(display.path.clone());
    let key = |down| Message::Key {
        qcode: "a".into(),
        down,
    };
    input.send(&[key(true), key(false)]).unwrap();
    input
        .send(&[Message::PointerAbs {
            x: 1,
            y: 2,
            width: 4,
            height: 4,
        }])
        .unwrap();
    let wait = |n: usize| {
        for _ in 0..200 {
            if display.inputs.lock().unwrap().len() >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "{n} inputs never arrived: {:?}",
            display.inputs.lock().unwrap()
        );
    };
    wait(3);
    assert_eq!(
        display.inputs.lock().unwrap()[..3],
        [
            key(true),
            key(false),
            Message::PointerAbs {
                x: 1,
                y: 2,
                width: 4,
                height: 4
            }
        ]
    );

    display.vanish();
    display.reappear();
    // The first write after the drop may still land in the dead socket's
    // buffer; the connection is replaced once the write fails.
    for _ in 0..20 {
        input.send(&[Message::Buttons { mask: 1 }]).unwrap();
        if display
            .connections
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    wait(4);
    assert!(
        display
            .inputs
            .lock()
            .unwrap()
            .contains(&Message::Buttons { mask: 1 })
    );
}

#[test]
fn buttons_have_the_protocols_bits() {
    assert_eq!(button_bit("left").unwrap(), 1);
    assert_eq!(button_bit("middle").unwrap(), 2);
    assert_eq!(button_bit("right").unwrap(), 4);
    assert!(button_bit("fourth").is_err());
}

/// A shared-memory surface configure of the given geometry.
fn configure_of(surface_id: u64, width: u32, height: u32, stride: u32) -> Message {
    Message::SurfaceConfigure {
        surface_id,
        width,
        height,
        drm_fourcc: XRGB8888,
        stride,
        modifier: 0,
        kind: SurfaceKind::Shm,
        buffers: 1,
    }
}

/// A buffer of `len` bytes to pass as a surface's descriptor.
fn buffer_of(len: u64) -> File {
    let file = tempfile::tempfile().expect("a buffer file");
    file.set_len(len).expect("sizing the buffer file");
    file
}

/// Review F1 (docs/reviews/display.md): the peer is a hypervisor process a
/// guest may have taken over, so a configure is refused before any of its
/// sizes is used. 1048576 squared would ask the lab daemon for 4 TiB; zero
/// and 70000 are outside what the recorder's 16-bit events carry; and
/// `0x4000_0001 * 4` wraps to 4 in 32 bits.
#[test]
fn configure_refuses_a_surface_larger_than_the_protocol_limit() {
    for (width, height, stride) in [
        (1_048_576, 1_048_576, 4_194_304),
        (0, 1, 4),
        (1, 0, 4),
        (70_000, 1, 280_000),
        (0x4000_0001, 1, 4),
        // Each side is in range and the buffer is over 256 MiB.
        (16_384, 16_384, 65_536),
    ] {
        let refused = Surface::configure(
            &configure_of(1, width, height, stride),
            vec![buffer_of(0).into()],
        );
        assert!(
            refused.is_err(),
            "{width}x{height} stride {stride} was accepted"
        );
    }
    let largest = Surface::configure(
        &configure_of(1, 16_384, 4_096, 65_536),
        vec![buffer_of(0).into()],
    );
    assert!(largest.is_ok(), "the largest surface is still accepted");
}

/// The same refusal at the socket: a server that reconfigures to a width of
/// 70000 ends the stream, rather than the recorder getting a `DesktopSize`
/// and rows cut to 65535.
#[tokio::test]
async fn the_stream_refuses_a_configure_wider_than_the_limit() {
    use vmlab_hv_display::{receive, send};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        use std::os::fd::AsRawFd;
        let (stream, _) = listener.accept().unwrap();
        let Message::Hello { capabilities, .. } = receive(&stream).unwrap().message else {
            panic!("the client opens with a Hello");
        };
        let hello = Message::Hello {
            version: vmlab_hv_display::protocol::VERSION,
            capabilities,
        };
        send(&stream, &hello, &[]).unwrap();
        let small = buffer_of(16);
        send(&stream, &configure_of(1, 4, 1, 16), &[small.as_raw_fd()]).unwrap();
        let wide = buffer_of(280_000);
        // The client may have gone by the time these are written.
        let _ = send(
            &stream,
            &configure_of(2, 70_000, 1, 280_000),
            &[wide.as_raw_fd()],
        );
        let _ = send(
            &stream,
            &Message::FrameReady {
                flags: FrameFlags::default(),
                surface_id: 2,
                buffer: 0,
                seq: 1,
                timestamp_ns: 1,
                damage: vec![Rect {
                    x: 0,
                    y: 0,
                    width: 70_000,
                    height: 1,
                }],
            },
            &[],
        );
        // Hold the connection until the client leaves.
        while receive(&stream).is_ok() {}
    });
    let mut stream = NativeStream::connect(&path).await.unwrap();
    assert_eq!((stream.width, stream.height), (4, 1));
    let refused = stream.next_update().await;
    assert!(refused.is_err(), "the wide configure gave {refused:?}");
    drop(stream);
    server.join().unwrap();
}

/// A raw server for the damage tests: it answers the Hello, configures one
/// `width` x `height` surface with one buffer, sends one `FrameReady` with
/// `damage`, and holds the connection until the client leaves.
fn serve_one_frame(
    listener: std::os::unix::net::UnixListener,
    width: u32,
    height: u32,
    damage: Vec<Rect>,
) -> std::thread::JoinHandle<()> {
    use vmlab_hv_display::{receive, send};

    std::thread::spawn(move || {
        use std::os::fd::AsRawFd;
        let (stream, _) = listener.accept().unwrap();
        let Message::Hello { capabilities, .. } = receive(&stream).unwrap().message else {
            panic!("the client opens with a Hello");
        };
        let hello = Message::Hello {
            version: vmlab_hv_display::protocol::VERSION,
            capabilities,
        };
        send(&stream, &hello, &[]).unwrap();
        let stride = width * 4;
        let buffer = buffer_of(u64::from(stride) * u64::from(height));
        send(
            &stream,
            &configure_of(1, width, height, stride),
            &[buffer.as_raw_fd()],
        )
        .unwrap();
        send(
            &stream,
            &Message::FrameReady {
                flags: FrameFlags::default(),
                surface_id: 1,
                buffer: 0,
                seq: 1,
                timestamp_ns: 1,
                damage,
            },
            &[],
        )
        .unwrap();
        // Hold the connection until the client leaves.
        while receive(&stream).is_ok() {}
    })
}

/// What `next_update` gave, without printing a gigabyte of pixels.
fn outcome(update: &Result<Vec<StreamEvent>>) -> String {
    match update {
        Ok(events) => format!("Ok with {} events", events.len()),
        Err(e) => format!("{e:#}"),
    }
}

/// Review F8 (docs/reviews/display-2.md): the codec takes any damage count
/// that fits a 16 MiB message, and every rectangle becomes memory the lab
/// daemon keeps. vmlab-hv's server core sends 65535 rectangles at most, so
/// 70000 end the stream — and so does damage whose sum is far past the
/// surface it describes: 16 times the whole of a 64 MiB surface is 1 GiB.
#[tokio::test]
async fn the_stream_refuses_more_damage_than_a_server_may_send() {
    let full = |width, height| Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    for (width, height, damage) in [
        (4, 4, vec![full(4, 4); 70_000]),
        (4096, 4096, vec![full(4096, 4096); 16]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("display.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let rectangles = damage.len();
        let server = serve_one_frame(listener, width, height, damage);
        let mut stream = NativeStream::connect(&path).await.unwrap();
        let update = stream.next_update().await;
        assert!(
            update.is_err(),
            "{rectangles} rectangles of {width}x{height} gave {}",
            outcome(&update)
        );
        drop(update);
        drop(stream);
        server.join().unwrap();
    }
}

/// The limit leaves an honest frame alone: the server core's own maximum of
/// 65535 rectangles, here one pixel each, arrives whole.
#[tokio::test]
async fn the_stream_takes_the_most_damage_a_server_may_send() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let pixel = Rect {
        x: 1,
        y: 2,
        width: 1,
        height: 1,
    };
    let server = serve_one_frame(listener, 4, 4, vec![pixel; 65_535]);
    let mut stream = NativeStream::connect(&path).await.unwrap();
    let events = stream.next_update().await.unwrap();
    assert_eq!(events.len(), 65_535);
    drop(stream);
    server.join().unwrap();
}

/// A server that takes the connection, reads the client's Hello and never
/// answers: a vmlab-hv that is stopped or hung. `heard` is told once the
/// Hello is read; the thread ends when the client closes.
fn serve_silence(
    listener: std::os::unix::net::UnixListener,
    heard: std::sync::mpsc::Sender<()>,
) -> std::thread::JoinHandle<()> {
    use vmlab_hv_display::receive;

    std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let Message::Hello { .. } = receive(&stream).unwrap().message else {
            panic!("the client opens with a Hello");
        };
        let _ = heard.send(());
        while receive(&stream).is_ok() {}
    })
}

/// Review F11 (docs/reviews/display-2.md): a unix socket connects as soon as
/// its listener exists, so the capture's deadline has to cover the Hello
/// exchange too. No clock is asserted on: without the deadline this test
/// does not end.
#[test]
fn a_capture_gives_up_on_a_server_that_never_answers_the_hello() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (heard, hello) = std::sync::mpsc::channel();
    let server = serve_silence(listener, heard);
    let refused = capture_blocking(&path, Duration::from_millis(50));
    let error = format!("{:#}", refused.unwrap_err());
    assert!(error.contains("display.sock"), "{error}");
    hello.recv().expect("the server read the Hello");
    // The client closed its end: the server's second read ends.
    server.join().unwrap();
}

/// The same for scripted input, whose sender holds the machine's input lock
/// while it waits: the send gives up and keeps no connection.
#[test]
fn input_gives_up_on_a_server_that_never_answers_the_hello() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (heard, hello) = std::sync::mpsc::channel();
    let server = serve_silence(listener, heard);
    let mut input = InputConn::with_patience(path, Duration::from_millis(50));
    let refused = input.send(&[Message::Buttons { mask: 1 }]);
    let error = format!("{:#}", refused.unwrap_err());
    assert!(error.contains("display.sock"), "{error}");
    hello.recv().expect("the server read the Hello");
    server.join().unwrap();
}

/// A server that answers the Hello and then reads nothing more fills the
/// socket: the write that finds it full gives up too, instead of holding
/// the input lock for as long as the hypervisor is stopped.
#[test]
fn input_gives_up_on_a_server_that_stops_reading() {
    use vmlab_hv_display::{receive, send};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        // Every connection gets its Hello and is then left unread: the
        // client reconnects once after a failed write.
        let mut held = Vec::new();
        listener.set_nonblocking(true).unwrap();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let Message::Hello { capabilities, .. } = receive(&stream).unwrap().message
                    else {
                        panic!("the client opens with a Hello");
                    };
                    let hello = Message::Hello {
                        version: vmlab_hv_display::protocol::VERSION,
                        capabilities,
                    };
                    send(&stream, &hello, &[]).unwrap();
                    held.push(stream);
                }
                Err(_) => match stopped.try_recv() {
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    _ => return,
                },
            }
        }
    });
    let mut input = InputConn::with_patience(path, Duration::from_millis(50));
    // More than a socket holds unread, so the first try fills the socket and
    // the one retry on a new connection fills that one.
    let batch = vec![Message::Buttons { mask: 1 }; 1 << 18];
    let error = format!("{:#}", input.send(&batch).unwrap_err());
    assert!(error.contains("display.sock"), "{error}");
    drop(stop);
    server.join().unwrap();
}
