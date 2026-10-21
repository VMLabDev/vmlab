//! The recorder's pure core (ADR-0003): timestamped events in, output
//! frames, idle cuts and timeline entries out. No I/O, no clock of its own —
//! the executor tells it the time.
//!
//! Events are framebuffer rectangles, `DesktopSize` changes, cursor shapes,
//! the inputs vmlab sent, and the connection going away and coming back.
//! From them the core keeps one source framebuffer and yields frames on a
//! constant clock (D10): the frame due at the end of every `1/fps` interval,
//! repeating the last picture when nothing arrived. Each frame is the source
//! scaled with aspect preserved and letterboxed into the one output size
//! (D11, D17), with the guest's cursor shape drawn at the last position vmlab
//! sent (D12, research 05) and a ripple at each click (D3).
//!
//! With idle compression on (D4, D16) an interval is *idle* when the
//! rectangles it received changed no more than [`IDLE_AREA`] pixels and no
//! input was sent; a run of idle intervals longer than the threshold is cut
//! to [`IDLE_KEEP`] of output. Because idle frames repeat one picture, the
//! core holds back nothing but a duration while a run is open and emits the
//! kept frames when it closes, so a caret or a clock never defeats the cut
//! and a click on a static screen is never cut.

use std::time::Duration;

use image::{RgbImage, imageops};
use serde::{Deserialize, Serialize};

use super::input::InputEvent;
use super::{IDLE_KEEP, Settings, Size};

/// The largest area, in source pixels, an interval's rectangles may cover and
/// still count as idle: a blinking caret, a clock with seconds in a taskbar
/// (QEMU reports dirt in 16-pixel cells, so even a one-pixel change arrives
/// as at least a 16-wide strip).
pub const IDLE_AREA: u64 = 64 * 64;

/// How long a click ripple stays on screen.
pub const RIPPLE_DURATION: Duration = Duration::from_millis(500);
/// Ripple geometry, in output pixels: it starts as a disc of this radius...
const RIPPLE_RADIUS_START: f64 = 10.0;
/// ...and grows to this radius as it fades.
const RIPPLE_RADIUS_END: f64 = 44.0;
/// Ripple colour (an amber that reads on light and dark desktops alike).
pub const RIPPLE_RGB: [u8; 3] = [255, 170, 0];

/// One output frame: packed `R G B` bytes, `size.width * size.height * 3`.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub rgb: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({} bytes)", self.rgb.len())
    }
}

/// An idle stretch that was shortened: wall-clock bounds and what it became.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cut {
    pub wall_start: f64,
    pub wall_end: f64,
    pub kept: f64,
}

/// One input in the timeline: when it happened in the output video (through
/// any cuts), when on the wall clock, and what it was.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// Seconds into the output video.
    pub at: f64,
    /// Seconds since the recording started, wall clock.
    pub wall: f64,
    #[serde(flatten)]
    pub event: InputEvent,
}

/// The guest's hardware cursor as QEMU last described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub hotspot: (u16, u16),
    pub width: u16,
    pub height: u16,
    /// Premultiplied-alpha RGBA: what a Linux guest uploads to the virtio-gpu
    /// cursor plane (DRM's default plane blend mode) and what QEMU relays,
    /// measured live on GNOME and KDE (acceptance F2). A RichCursor's 1-bit
    /// mask yields alpha 0 or 255, which is premultiplied and straight alike.
    pub rgba: Vec<u8>,
}

impl CursorShape {
    /// Whether `other` is QEMU re-announcing this shape rather than the guest
    /// defining one: the same pixels with the hotspot zeroed. QEMU 11 sends
    /// exactly that at a virtio-gpu reset and alongside every `DesktopSize`
    /// through a reboot (acceptance F1), long after the guest that drew the
    /// shape is gone. A guest redefining its cursor after a mode change sends
    /// its real hotspot, so it passes; a shape whose own hotspot is (0,0)
    /// cannot be told apart and is treated as stale, hidden rather than
    /// drawn wrongly, until the guest sends a different shape.
    fn is_stale_resend_of(&self, other: &CursorShape) -> bool {
        other.hotspot == (0, 0)
            && other.width == self.width
            && other.height == self.height
            && other.rgba == self.rgba
    }
}

#[derive(Debug, Clone)]
struct Ripple {
    /// Source (framebuffer) coordinates.
    x: i64,
    y: i64,
    start: Duration,
}

/// The pure core. Every method taking an `at` or `now` is a wall-clock
/// offset from the recording's start; the core tolerates them arriving a
/// little out of order by never stepping backwards.
pub struct Core {
    settings: Settings,
    /// Frame interval.
    dt: Duration,
    /// The source framebuffer as last painted; `None` before the first
    /// `DesktopSize` or connection told us a size.
    fb: Option<RgbImage>,
    /// Whether `fb` changed since `scaled` was computed.
    fb_dirty: bool,
    /// The letterboxed, scaled source — the frame without overlays.
    scaled: Option<Vec<u8>>,
    /// Source pixels painted in the current interval.
    dirty_area: u64,
    input_in_interval: bool,
    cursor: Option<CursorShape>,
    /// The last shape the guest itself defined, kept while `cursor` is
    /// hidden so a stale re-announcement of it can be recognised.
    defined: Option<CursorShape>,
    /// Last pointer position vmlab sent, source coordinates.
    pointer: Option<(i64, i64)>,
    ripples: Vec<Ripple>,
    /// Intervals whose end has been processed.
    intervals: u64,
    /// Output video length so far.
    video: Duration,
    /// The open idle run, if any: wall start and accumulated length.
    idle: Option<(Duration, Duration)>,
    cuts: Vec<Cut>,
    timeline: Vec<TimelineEntry>,
    out: Vec<Frame>,
}

impl Core {
    pub fn new(settings: Settings) -> Self {
        let dt = Duration::from_nanos(1_000_000_000 / u64::from(settings.fps));
        Self {
            settings,
            dt,
            fb: None,
            fb_dirty: false,
            scaled: None,
            dirty_area: 0,
            input_in_interval: false,
            cursor: None,
            defined: None,
            pointer: None,
            ripples: Vec::new(),
            intervals: 0,
            video: Duration::ZERO,
            idle: None,
            cuts: Vec::new(),
            timeline: Vec::new(),
            out: Vec::new(),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Output video length so far, cuts applied.
    pub fn video_duration(&self) -> Duration {
        self.video
    }

    pub fn cuts(&self) -> &[Cut] {
        &self.cuts
    }

    pub fn timeline(&self) -> &[TimelineEntry] {
        &self.timeline
    }

    /// Whether a cursor shape is known, so the pointer would be drawn.
    pub fn draws_pointer(&self) -> bool {
        self.cursor.is_some()
    }

    // --- events ---------------------------------------------------------

    /// The connection is up and the server reports a `w`x`h` screen. Also
    /// the first event of a recording. The cursor shape is unknown again
    /// (research 05: a new QEMU, or a reboot, may or may not redefine it).
    pub fn connected(&mut self, at: Duration, w: u16, h: u16) {
        self.advance(at);
        self.cursor = None;
        self.resize(w, h);
    }

    /// The connection dropped; the last picture holds until it is back.
    pub fn disconnected(&mut self, at: Duration) {
        self.advance(at);
        self.cursor = None;
    }

    /// A `DesktopSize` pseudo-rectangle: the screen is now `w`x`h`. The
    /// previous picture stays on screen until the first rectangle of the new
    /// one arrives.
    pub fn desktop_size(&mut self, at: Duration, w: u16, h: u16) {
        self.advance(at);
        self.cursor = None;
        self.resize(w, h);
    }

    fn resize(&mut self, w: u16, h: u16) {
        let same = self
            .fb
            .as_ref()
            .is_some_and(|fb| fb.width() == u32::from(w) && fb.height() == u32::from(h));
        if !same {
            self.fb = Some(RgbImage::new(u32::from(w.max(1)), u32::from(h.max(1))));
            // Keep `scaled` as it is: the old picture holds until new pixels
            // arrive (D6's gap rule for a restart or a mode switch).
        }
    }

    /// A Raw rectangle of `B G R x` pixels painted at `(x, y)`. Pixels
    /// outside the current size are dropped rather than failing.
    pub fn rect(&mut self, at: Duration, x: u16, y: u16, w: u16, h: u16, bgrx: &[u8]) {
        self.advance(at);
        let Some(fb) = self.fb.as_mut() else {
            return;
        };
        let (fw, fh) = (fb.width(), fb.height());
        let mut painted = 0u64;
        for row in 0..u32::from(h) {
            let py = u32::from(y) + row;
            if py >= fh {
                break;
            }
            for col in 0..u32::from(w) {
                let px = u32::from(x) + col;
                if px >= fw {
                    break;
                }
                let i = ((row * u32::from(w) + col) * 4) as usize;
                if i + 3 >= bgrx.len() {
                    break;
                }
                fb.put_pixel(px, py, image::Rgb([bgrx[i + 2], bgrx[i + 1], bgrx[i]]));
                painted += 1;
            }
        }
        if painted > 0 {
            self.fb_dirty = true;
            self.dirty_area += painted;
        }
    }

    /// A cursor shape rectangle arrived. A genuinely new shape is drawn from
    /// here on; QEMU's re-announcement of the shape already known (see
    /// [`CursorShape::is_stale_resend_of`]) hides the pointer instead, since
    /// QEMU only re-announces when the display that drew it was reset.
    pub fn cursor(&mut self, at: Duration, shape: CursorShape) {
        self.advance(at);
        if self
            .defined
            .as_ref()
            .is_some_and(|d| d.is_stale_resend_of(&shape))
        {
            self.cursor = None;
            return;
        }
        self.defined = Some(shape.clone());
        self.cursor = Some(shape);
    }

    /// vmlab sent `event` to the display.
    pub fn input(&mut self, at: Duration, event: InputEvent) {
        self.advance(at);
        let at = at.max(self.interval_start());
        match &event {
            InputEvent::Move { x, y } => self.pointer = Some((*x, *y)),
            InputEvent::Click { x, y, .. } => {
                if let (Some(x), Some(y)) = (x, y) {
                    self.pointer = Some((*x, *y));
                }
                if let Some((px, py)) = self.pointer {
                    self.ripples.push(Ripple {
                        x: px,
                        y: py,
                        start: at,
                    });
                }
            }
            InputEvent::Drag { x1, y1, x2, y2 } => {
                self.ripples.push(Ripple {
                    x: *x1,
                    y: *y1,
                    start: at,
                });
                self.ripples.push(Ripple {
                    x: *x2,
                    y: *y2,
                    start: at,
                });
                self.pointer = Some((*x2, *y2));
            }
            InputEvent::Keys { .. } | InputEvent::Text { .. } => {}
        }
        self.input_in_interval = true;
        // Video time of this moment: what has been emitted, plus what the
        // open idle run will contribute when this input closes it, plus the
        // way into the current interval.
        let pending = self.idle.map(|(_, len)| self.kept(len)).unwrap_or_default();
        let into = at.saturating_sub(self.interval_start());
        self.timeline.push(TimelineEntry {
            at: (self.video + pending + into).as_secs_f64(),
            wall: at.as_secs_f64(),
            event,
        });
    }

    // --- the clock ------------------------------------------------------

    /// Process every interval that ended by `now` and return the frames due.
    pub fn tick(&mut self, now: Duration) -> Vec<Frame> {
        self.advance(now);
        std::mem::take(&mut self.out)
    }

    /// Process every interval that ended by `now`, close any open idle run,
    /// and return the frames due. The last call. A recording stopped before
    /// its first interval ended still gets one frame, so the file it leaves
    /// is a playable video rather than a container with no stream in it.
    pub fn finish(&mut self, now: Duration) -> Vec<Frame> {
        self.advance(now);
        self.close_idle();
        if self.video.is_zero() {
            let frame = self.compose(now);
            self.out.push(frame);
            self.video += self.dt;
        }
        std::mem::take(&mut self.out)
    }

    fn interval_start(&self) -> Duration {
        self.due(self.intervals)
    }

    fn due(&self, interval: u64) -> Duration {
        Duration::from_nanos(interval * 1_000_000_000 / u64::from(self.settings.fps))
    }

    /// Process intervals whose end is at or before `now`.
    fn advance(&mut self, now: Duration) {
        while self.due(self.intervals + 1) <= now {
            let end = self.due(self.intervals + 1);
            self.step(end);
        }
    }

    /// Close the interval ending at `end`.
    fn step(&mut self, end: Duration) {
        let start = self.interval_start();
        let ripple_live = self
            .ripples
            .iter()
            .any(|r| r.start < end && end < r.start + RIPPLE_DURATION);
        let active = self.dirty_area > IDLE_AREA || self.input_in_interval || ripple_live;
        if self.settings.compress_idle.is_none() || active {
            self.close_idle();
            let frame = self.compose(end);
            self.out.push(frame);
            self.video += self.dt;
        } else {
            match &mut self.idle {
                Some((_, len)) => *len += self.dt,
                None => self.idle = Some((start, self.dt)),
            }
        }
        self.ripples.retain(|r| end < r.start + RIPPLE_DURATION);
        self.dirty_area = 0;
        self.input_in_interval = false;
        self.intervals += 1;
    }

    /// What an idle run of `len` becomes in the output.
    fn kept(&self, len: Duration) -> Duration {
        match self.settings.compress_idle {
            Some(threshold) if len > threshold => IDLE_KEEP.min(len),
            _ => len,
        }
    }

    /// Emit the frames an open idle run owes and record the cut if it was
    /// shortened.
    fn close_idle(&mut self) {
        let Some((start, len)) = self.idle.take() else {
            return;
        };
        let kept = self.kept(len);
        let frames = (kept.as_nanos() / self.dt.as_nanos().max(1)) as usize;
        if frames > 0 {
            // Idle frames show the picture as it stands; no ripple can be
            // live, since a ripple keeps an interval active.
            let frame = self.compose(start + len);
            self.out.extend(std::iter::repeat_n(frame, frames));
            self.video += self.dt * frames as u32;
        }
        if kept < len {
            self.cuts.push(Cut {
                wall_start: start.as_secs_f64(),
                wall_end: (start + len).as_secs_f64(),
                kept: (self.dt * frames as u32).as_secs_f64(),
            });
        }
    }

    // --- compositing ----------------------------------------------------

    /// How the source maps into the output: scale factor and the offset of
    /// the scaled picture.
    fn placement(&self) -> Option<Placement> {
        let fb = self.fb.as_ref()?;
        Some(Placement::fit(
            Size::new(fb.width(), fb.height()),
            self.settings.size,
        ))
    }

    /// The scaled, letterboxed source, recomputed only when pixels changed.
    fn base(&mut self) -> &[u8] {
        if self.scaled.is_none() || self.fb_dirty {
            let size = self.settings.size;
            let mut out = vec![0u8; (size.width * size.height * 3) as usize];
            if let (Some(fb), Some(p)) = (self.fb.as_ref(), self.placement()) {
                let picture = if p.width == fb.width() && p.height == fb.height() {
                    std::borrow::Cow::Borrowed(fb)
                } else {
                    std::borrow::Cow::Owned(imageops::resize(
                        fb,
                        p.width,
                        p.height,
                        imageops::FilterType::Triangle,
                    ))
                };
                let row_bytes = (p.width * 3) as usize;
                for row in 0..p.height {
                    let src = (row * p.width * 3) as usize;
                    let dst = (((row + p.y) * size.width + p.x) * 3) as usize;
                    out[dst..dst + row_bytes]
                        .copy_from_slice(&picture.as_raw()[src..src + row_bytes]);
                }
            }
            self.scaled = Some(out);
            self.fb_dirty = false;
        }
        self.scaled.as_deref().unwrap_or(&[])
    }

    /// The output frame at wall time `t`: base, pointer, ripples.
    fn compose(&mut self, t: Duration) -> Frame {
        let placement = self.placement();
        let mut rgb = self.base().to_vec();
        let size = self.settings.size;
        if let Some(p) = placement {
            if let (Some(cursor), Some((px, py))) = (self.cursor.as_ref(), self.pointer) {
                draw_cursor(&mut rgb, size, &p, cursor, px, py);
            }
            for r in &self.ripples {
                if r.start <= t && t < r.start + RIPPLE_DURATION {
                    let progress = (t - r.start).as_secs_f64() / RIPPLE_DURATION.as_secs_f64();
                    let (cx, cy) = p.map(r.x as f64, r.y as f64);
                    draw_ripple(&mut rgb, size, cx, cy, progress);
                }
            }
        }
        Frame { rgb }
    }
}

/// Where a source picture lands in the output frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub scale: f64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Placement {
    /// Fit `source` into `output` with aspect preserved, centred.
    pub fn fit(source: Size, output: Size) -> Self {
        let scale = (f64::from(output.width) / f64::from(source.width.max(1)))
            .min(f64::from(output.height) / f64::from(source.height.max(1)));
        let width = ((f64::from(source.width) * scale).round() as u32).clamp(1, output.width);
        let height = ((f64::from(source.height) * scale).round() as u32).clamp(1, output.height);
        Self {
            scale,
            x: (output.width - width) / 2,
            y: (output.height - height) / 2,
            width,
            height,
        }
    }

    /// A source coordinate in output pixels.
    pub fn map(&self, x: f64, y: f64) -> (f64, f64) {
        (
            f64::from(self.x) + x * self.scale,
            f64::from(self.y) + y * self.scale,
        )
    }
}

/// Straight-alpha blend of `colour` over the pixel at `(x, y)`.
fn blend(rgb: &mut [u8], size: Size, x: i64, y: i64, colour: [u8; 3], alpha: f64) {
    if x < 0 || y < 0 || x >= i64::from(size.width) || y >= i64::from(size.height) {
        return;
    }
    let i = ((y as u32 * size.width + x as u32) * 3) as usize;
    for c in 0..3 {
        let base = f64::from(rgb[i + c]);
        rgb[i + c] = (base + (f64::from(colour[c]) - base) * alpha)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
}

/// Premultiplied-alpha blend: `colour` already carries its alpha, so it is
/// added to what is left of the pixel beneath (`src + dst * (1 - a)`).
fn blend_premultiplied(rgb: &mut [u8], size: Size, x: i64, y: i64, colour: [u8; 3], alpha: f64) {
    if x < 0 || y < 0 || x >= i64::from(size.width) || y >= i64::from(size.height) {
        return;
    }
    let i = ((y as u32 * size.width + x as u32) * 3) as usize;
    for c in 0..3 {
        let base = f64::from(rgb[i + c]);
        rgb[i + c] = (f64::from(colour[c]) + base * (1.0 - alpha))
            .round()
            .clamp(0.0, 255.0) as u8;
    }
}

/// Draw the cursor with its hotspot at source `(px, py)`, scaled like the
/// picture (nearest neighbour; cursors are small).
fn draw_cursor(rgb: &mut [u8], size: Size, p: &Placement, cursor: &CursorShape, px: i64, py: i64) {
    let (ox, oy) = p.map(
        px as f64 - f64::from(cursor.hotspot.0),
        py as f64 - f64::from(cursor.hotspot.1),
    );
    let w = (f64::from(cursor.width) * p.scale).ceil().max(1.0) as i64;
    let h = (f64::from(cursor.height) * p.scale).ceil().max(1.0) as i64;
    let (x0, y0) = (ox.round() as i64, oy.round() as i64);
    for dy in 0..h {
        let sy = ((dy as f64 / p.scale) as u32).min(u32::from(cursor.height) - 1);
        for dx in 0..w {
            let sx = ((dx as f64 / p.scale) as u32).min(u32::from(cursor.width) - 1);
            let i = ((sy * u32::from(cursor.width) + sx) * 4) as usize;
            let a = f64::from(cursor.rgba[i + 3]) / 255.0;
            if a > 0.0 {
                blend_premultiplied(
                    rgb,
                    size,
                    x0 + dx,
                    y0 + dy,
                    [cursor.rgba[i], cursor.rgba[i + 1], cursor.rgba[i + 2]],
                    a,
                );
            }
        }
    }
}

/// A ripple centred at output `(cx, cy)`, `progress` in `0..1`: a translucent
/// disc that grows and fades, with a brighter rim.
fn draw_ripple(rgb: &mut [u8], size: Size, cx: f64, cy: f64, progress: f64) {
    let radius = RIPPLE_RADIUS_START + (RIPPLE_RADIUS_END - RIPPLE_RADIUS_START) * progress;
    let fade = 1.0 - progress;
    let r = radius.ceil() as i64 + 1;
    for dy in -r..=r {
        for dx in -r..=r {
            let d = ((dx * dx + dy * dy) as f64).sqrt();
            if d > radius {
                continue;
            }
            let rim = if radius - d < 3.0 { 1.0 } else { 0.8 };
            blend(
                rgb,
                size,
                cx.round() as i64 + dx,
                cy.round() as i64 + dy,
                RIPPLE_RGB,
                (0.95 * fade * rim).clamp(0.0, 1.0),
            );
        }
    }
}

#[cfg(test)]
mod tests;
