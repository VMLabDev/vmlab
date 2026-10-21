//! The pure core against scripted event sequences: frames, cuts, timeline.

use std::time::Duration;

use super::{Core, CursorShape, Frame, IDLE_AREA, RIPPLE_RGB};
use crate::record::input::InputEvent;
use crate::record::{Format, Settings, Size};

fn settings(fps: u32, size: Size, compress_idle: Option<f64>) -> Settings {
    Settings {
        format: Format::Mp4,
        fps,
        size,
        compress_idle: compress_idle.map(Duration::from_secs_f64),
        timeline: true,
        max_duration: None,
    }
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// A `w`x`h` Raw payload of one colour, `B G R x`.
fn solid(w: u16, h: u16, rgb: [u8; 3]) -> Vec<u8> {
    [rgb[2], rgb[1], rgb[0], 0].repeat(w as usize * h as usize)
}

fn pixel(frame: &Frame, size: Size, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * size.width + x) * 3) as usize;
    [frame.rgb[i], frame.rgb[i + 1], frame.rgb[i + 2]]
}

fn all(frame: &Frame, rgb: [u8; 3]) -> bool {
    frame.rgb.chunks_exact(3).all(|p| p == rgb)
}

const RED: [u8; 3] = [255, 0, 0];
const GREEN: [u8; 3] = [0, 255, 0];
const BLACK: [u8; 3] = [0, 0, 0];

/// With nothing arriving, one frame per interval still comes out, black
/// before any screen is known and repeating the last picture after.
#[test]
fn frames_come_out_at_the_constant_rate_whatever_arrives() {
    let size = Size::new(8, 4);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 8, 4);
    let frames = core.tick(secs(1.0));
    assert_eq!(frames.len(), 10);
    assert!(frames.iter().all(|f| all(f, BLACK)));

    core.rect(secs(1.02), 0, 0, 8, 4, &solid(8, 4, RED));
    let frames = core.tick(secs(1.55));
    assert_eq!(frames.len(), 5, "intervals ending at 1.1 .. 1.5");
    assert!(frames.iter().all(|f| all(f, RED)), "the picture repeats");
    assert_eq!(core.tick(secs(1.55)).len(), 0, "no interval ended");
    assert_eq!(core.video_duration(), secs(1.5));
}

/// A source narrower than the output is scaled up with its aspect kept and
/// centred, black above and below; a `DesktopSize` keeps the old picture
/// until the new one paints, then the new size is fitted on its own.
#[test]
fn the_source_is_scaled_and_letterboxed_into_one_output_size() {
    let size = Size::new(8, 8);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 4, 2);
    core.rect(secs(0.0), 0, 0, 4, 2, &solid(4, 2, RED));
    let frame = core.tick(secs(0.1)).pop().unwrap();
    for y in 0..8 {
        let expect = if (2..6).contains(&y) { RED } else { BLACK };
        assert_eq!(pixel(&frame, size, 3, y), expect, "row {y}");
    }

    core.desktop_size(secs(0.15), 2, 2);
    let frame = core.tick(secs(0.2)).pop().unwrap();
    assert_eq!(
        pixel(&frame, size, 3, 3),
        RED,
        "old picture holds until pixels arrive"
    );
    core.rect(secs(0.25), 0, 0, 2, 2, &solid(2, 2, GREEN));
    let frame = core.tick(secs(0.3)).pop().unwrap();
    assert!(all(&frame, GREEN), "a square source fills a square output");
}

/// An explicit output size letterboxes the same way.
#[test]
fn an_explicit_size_is_honoured() {
    let size = Size::new(16, 8);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 4, 4);
    core.rect(secs(0.0), 0, 0, 4, 4, &solid(4, 4, GREEN));
    let frame = core.tick(secs(0.1)).pop().unwrap();
    assert_eq!(frame.rgb.len(), 16 * 8 * 3);
    assert_eq!(pixel(&frame, size, 3, 4), BLACK, "left bar");
    assert_eq!(pixel(&frame, size, 8, 4), GREEN, "picture");
    assert_eq!(pixel(&frame, size, 12, 4), BLACK, "right bar");
}

fn arrow() -> CursorShape {
    // 2x2, opaque white except the bottom-right pixel; hotspot (1, 1).
    CursorShape {
        hotspot: (1, 1),
        width: 2,
        height: 2,
        rgba: vec![
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 0, 0, 0, 0,
        ],
    }
}

/// No pointer is drawn until a cursor shape arrives; then it sits at the last
/// position vmlab sent, offset by the hotspot; a `DesktopSize` and a
/// reconnect forget the shape until the guest defines it again.
#[test]
fn the_pointer_is_drawn_only_with_a_known_shape_at_the_sent_position() {
    let size = Size::new(8, 8);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 8, 8);
    core.rect(secs(0.0), 0, 0, 8, 8, &solid(8, 8, BLACK));
    core.input(secs(0.01), InputEvent::Move { x: 4, y: 4 });
    let frame = core.tick(secs(0.1)).pop().unwrap();
    assert!(
        all(&frame, BLACK),
        "no cursor rect yet: the pointer is the guest's business"
    );
    assert!(!core.draws_pointer());

    core.cursor(secs(0.11), arrow());
    let frame = core.tick(secs(0.2)).pop().unwrap();
    assert_eq!(
        pixel(&frame, size, 3, 3),
        [255, 255, 255],
        "hotspot (1,1) puts the top-left at (3,3)"
    );
    assert_eq!(pixel(&frame, size, 4, 3), [255, 255, 255]);
    assert_eq!(pixel(&frame, size, 4, 4), BLACK, "transparent cursor pixel");
    assert_eq!(pixel(&frame, size, 5, 5), BLACK);

    core.input(secs(0.21), InputEvent::Move { x: 1, y: 1 });
    let frame = core.tick(secs(0.3)).pop().unwrap();
    assert_eq!(
        pixel(&frame, size, 0, 0),
        [255, 255, 255],
        "moved with the pointer"
    );
    assert_eq!(pixel(&frame, size, 3, 3), BLACK);

    core.desktop_size(secs(0.31), 8, 8);
    assert!(!core.draws_pointer(), "a mode change forgets the shape");
    core.cursor(secs(0.32), arrow());
    core.disconnected(secs(0.33));
    assert!(!core.draws_pointer(), "so does losing the connection");
    core.connected(secs(0.34), 8, 8);
    assert!(!core.draws_pointer());
}

/// A click paints ripple pixels around the click point in the frame that
/// follows it, and they are gone half a second later.
#[test]
fn a_click_ripples_at_its_point() {
    let size = Size::new(64, 64);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 64, 64);
    core.rect(secs(0.0), 0, 0, 64, 64, &solid(64, 64, BLACK));
    core.input(
        secs(0.05),
        InputEvent::Click {
            button: "left".into(),
            x: Some(32),
            y: Some(32),
        },
    );
    let frame = core.tick(secs(0.1)).pop().unwrap();
    let centre = pixel(&frame, size, 32, 32);
    assert!(
        centre[0] > 90 && centre[1] > 50 && centre[2] == 0,
        "amber at the click point: {centre:?}"
    );
    assert_eq!(pixel(&frame, size, 2, 2), BLACK, "far corner untouched");
    let _ = RIPPLE_RGB;

    let frames = core.tick(secs(0.7));
    assert!(all(frames.last().unwrap(), BLACK), "faded out after 0.5 s");
    assert!(!all(&frames[0], BLACK), "still visible at 0.2 s");
}

/// With compression on, a run of idle intervals longer than the threshold is
/// cut to half a second; small changes stay idle, a bigger change or any
/// input ends the run.
#[test]
fn idle_stretches_are_cut_and_activity_is_not() {
    let size = Size::new(8, 8);
    // 10 fps, threshold 1 s.
    let mut core = Core::new(settings(10, size, Some(1.0)));
    core.connected(secs(0.0), 100, 100);
    core.rect(secs(0.0), 0, 0, 100, 100, &solid(100, 100, RED));
    let frames = core.tick(secs(0.1));
    assert_eq!(frames.len(), 1, "a full repaint is activity");

    // 3 s of nothing but a caret-sized blink every interval.
    for i in 1..=30 {
        core.rect(
            secs(0.1 + 0.1 * f64::from(i) - 0.05),
            0,
            0,
            8,
            16,
            &solid(8, 16, GREEN),
        );
    }
    let frames = core.tick(secs(3.1));
    assert_eq!(frames.len(), 0, "an open idle run emits nothing yet");

    // Input closes the run: 0.5 s kept, then the active frame.
    core.input(
        secs(3.15),
        InputEvent::Keys {
            chord: "ret".into(),
        },
    );
    let frames = core.tick(secs(3.2));
    assert_eq!(frames.len(), 5 + 1);
    assert_eq!(core.cuts().len(), 1);
    let cut = &core.cuts()[0];
    assert!(
        (cut.wall_start - 0.1).abs() < 1e-6 && (cut.wall_end - 3.1).abs() < 1e-6,
        "{cut:?}"
    );
    assert!((cut.kept - 0.5).abs() < 1e-6);
    assert_eq!(core.video_duration(), secs(0.7));

    // A run shorter than the threshold is kept whole.
    let frames = core.tick(secs(3.9));
    assert_eq!(frames.len(), 0);
    core.rect(secs(3.95), 0, 0, 100, 100, &solid(100, 100, GREEN));
    let frames = core.tick(secs(4.0));
    assert_eq!(
        frames.len(),
        7 + 1,
        "0.7 s of idle below the threshold is kept whole"
    );
    assert_eq!(core.cuts().len(), 1);

    // Finishing closes an open run too.
    let frames = core.finish(secs(6.0));
    assert_eq!(frames.len(), 5, "2 s idle at the end becomes 0.5 s");
    assert_eq!(core.cuts().len(), 2);
    assert!(IDLE_AREA >= 8 * 16);
}

/// Timeline timestamps are in output-video time, so an input after a cut is
/// stamped where it appears in the video, not where it happened on the wall.
#[test]
fn the_timeline_maps_wall_time_through_cuts() {
    let size = Size::new(8, 8);
    let mut core = Core::new(settings(10, size, Some(1.0)));
    core.connected(secs(0.0), 8, 8);
    core.rect(secs(0.0), 0, 0, 8, 8, &solid(8, 8, RED));
    core.tick(secs(0.1));
    core.input(secs(0.15), InputEvent::Text { text: "a".into() });
    // Idle from 0.2 to 5.2, then a click at 5.25.
    core.tick(secs(5.2));
    core.input(
        secs(5.25),
        InputEvent::Click {
            button: "left".into(),
            x: Some(1),
            y: Some(1),
        },
    );
    core.finish(secs(5.3));
    let tl = core.timeline();
    assert_eq!(tl.len(), 2);
    assert!((tl[0].at - 0.15).abs() < 1e-6, "{:?}", tl[0]);
    assert!((tl[0].wall - 0.15).abs() < 1e-6);
    // Video: 0.2 s active, 5 s idle kept as 0.5 s, then 0.05 into the next.
    assert!((tl[1].at - 0.75).abs() < 1e-6, "{:?}", tl[1]);
    assert!((tl[1].wall - 5.25).abs() < 1e-6);
    assert_eq!(
        tl[1].event,
        InputEvent::Click {
            button: "left".into(),
            x: Some(1),
            y: Some(1)
        }
    );

    let json = serde_json::to_value(&tl[1]).unwrap();
    assert_eq!(json["kind"], "click");
    assert_eq!(json["x"], 1);
}

/// Without compression nothing is ever cut and the timeline is wall time.
#[test]
fn without_compression_video_time_is_wall_time() {
    let size = Size::new(8, 8);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 8, 8);
    core.tick(secs(4.0));
    core.input(secs(4.05), InputEvent::Move { x: 0, y: 0 });
    assert_eq!(core.finish(secs(4.1)).len(), 1);
    assert!(core.cuts().is_empty());
    assert!((core.timeline()[0].at - 4.05).abs() < 1e-6);
    assert_eq!(core.video_duration(), secs(4.1));
}

/// Across a disconnect and reconnect the last picture holds, and the new
/// server's size is taken up when its pixels arrive.
#[test]
fn the_last_frame_holds_across_a_gap() {
    let size = Size::new(8, 8);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 8, 8);
    core.rect(secs(0.0), 0, 0, 8, 8, &solid(8, 8, RED));
    core.tick(secs(0.1));
    core.disconnected(secs(0.12));
    let frames = core.tick(secs(1.0));
    assert_eq!(frames.len(), 9);
    assert!(
        frames.iter().all(|f| all(f, RED)),
        "the gap shows the last picture"
    );
    core.connected(secs(1.05), 4, 4);
    let frame = core.tick(secs(1.1)).pop().unwrap();
    assert!(all(&frame, RED), "still held until the new screen paints");
    core.rect(secs(1.15), 0, 0, 4, 4, &solid(4, 4, GREEN));
    let frame = core.tick(secs(1.2)).pop().unwrap();
    assert!(all(&frame, GREEN));
}

/// A click with no coordinates ripples where the pointer last was, and a
/// drag ripples at both ends.
#[test]
fn clicks_without_a_point_and_drags_use_the_pointer() {
    let size = Size::new(64, 64);
    let mut core = Core::new(settings(10, size, None));
    core.connected(secs(0.0), 64, 64);
    core.rect(secs(0.0), 0, 0, 64, 64, &solid(64, 64, BLACK));
    core.input(
        secs(0.01),
        InputEvent::Click {
            button: "left".into(),
            x: None,
            y: None,
        },
    );
    let frame = core.tick(secs(0.1)).pop().unwrap();
    assert!(
        all(&frame, BLACK),
        "no pointer position known: nothing to ripple at"
    );

    core.input(secs(0.11), InputEvent::Move { x: 10, y: 10 });
    core.input(
        secs(0.12),
        InputEvent::Click {
            button: "right".into(),
            x: None,
            y: None,
        },
    );
    let frame = core.tick(secs(0.2)).pop().unwrap();
    assert_ne!(pixel(&frame, size, 10, 10), BLACK);

    core.input(
        secs(0.81),
        InputEvent::Drag {
            x1: 20,
            y1: 20,
            x2: 50,
            y2: 50,
        },
    );
    let frame = core.tick(secs(0.9)).pop().unwrap();
    assert_ne!(pixel(&frame, size, 20, 20), BLACK);
    assert_ne!(pixel(&frame, size, 50, 50), BLACK);
    assert_eq!(pixel(&frame, size, 35, 35), BLACK);
}
