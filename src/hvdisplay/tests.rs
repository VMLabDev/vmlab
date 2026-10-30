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
