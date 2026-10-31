//! The recorder's executor: the RFB read loop, the ffmpeg child and the
//! clock, around the pure [`Core`].
//!
//! Three concerns, three tasks. The *reader* owns the display connection —
//! RFB on QEMU's VNC socket, or vmlab-hv's native frame protocol on its
//! `display.sock` ([`StreamSource`]) — and feeds every decoded rectangle to
//! the core; when the connection drops it marks the gap and retries the
//! socket until it is back (a `vm restart` or a snapshot restore replaces the
//! hypervisor under it, D6). The *input* task drains
//! the display's feed into the core. The *clock* — the recorder task itself
//! — ticks at the frame rate, takes the frames due and writes them to
//! ffmpeg. Reading is never blocked by encoding: the reader only ever takes
//! a short lock on the core, so QEMU, which drops a client about five frames
//! behind (research 05), always finds this one keeping up.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{mpsc, watch};

use super::core::{Core, CursorShape};
use super::encoder::{Encoder, EncoderPlan};
use super::input::TimedInput;
use super::{Settings, timeline_path};
use crate::hvdisplay::NativeStream;
use crate::vnc::{StreamEvent, VncClient};

/// How often a dropped connection is retried.
const RECONNECT_EVERY: Duration = Duration::from_millis(250);

/// Where a recording's frames come from: a socket path, so a reconnect finds
/// the hypervisor a restart put in place of the old one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamSource {
    /// An RFB server: QEMU's `-vnc` socket.
    Vnc(PathBuf),
    /// vmlab-hv's native frame protocol (§22, phase 2b): damage, cursor
    /// shape and position, resizes, with no RFB in between (HV phase-2 D8).
    Native(PathBuf),
}

impl StreamSource {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::Vnc(p) | Self::Native(p) => p,
        }
    }
}

/// One live connection to a [`StreamSource`].
enum Stream {
    Vnc(VncClient),
    Native(NativeStream),
}

impl Stream {
    async fn connect(source: &StreamSource) -> Result<Self> {
        Ok(match source {
            StreamSource::Vnc(sock) => {
                let mut client = VncClient::connect(sock).await?;
                client
                    .start_stream()
                    .await
                    .context("starting the VNC stream")?;
                Self::Vnc(client)
            }
            StreamSource::Native(sock) => Self::Native(NativeStream::connect(sock).await?),
        })
    }

    fn size(&self) -> (u16, u16) {
        match self {
            Self::Vnc(c) => (c.width, c.height),
            Self::Native(c) => (c.width, c.height),
        }
    }

    async fn next_update(&mut self) -> Result<Vec<StreamEvent>> {
        match self {
            Self::Vnc(c) => c.next_update().await,
            Self::Native(c) => c.next_update().await,
        }
    }
}

/// What a recorder needs: nothing about a lab (D18).
pub struct RecorderSpec {
    /// Where the machine's frames come from.
    pub source: StreamSource,
    /// Where the video goes; its extension chose the format.
    pub output: PathBuf,
    pub settings: Settings,
    /// The ffmpeg to run, already checked for the format's encoder.
    pub ffmpeg: PathBuf,
    /// The display's input feed.
    pub inputs: mpsc::UnboundedReceiver<TimedInput>,
}

/// A finalised recording.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub path: PathBuf,
    /// Length of the video written.
    pub duration: Duration,
    /// The timeline sidecar, when one was asked for.
    pub timeline: Option<PathBuf>,
    /// Why it ended, when it was not asked to: the max duration, or an error.
    pub ended_by: Option<String>,
}

/// A live recording: stop it and get the finished file.
pub struct Recorder {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<Finished>>,
    pub started: Instant,
}

impl Recorder {
    /// Connect to the socket, start ffmpeg and begin the clock. Returns once
    /// the first connection is up and the encoder is running, so a caller
    /// holding a paused VM (D20) knows the first frame will be caught; a
    /// socket that refuses or an ffmpeg that fails to start are errors here,
    /// not later.
    pub async fn start(spec: RecorderSpec) -> Result<Self> {
        let client = Stream::connect(&spec.source)
            .await
            .with_context(|| format!("connecting to {}", spec.source.path().display()))?;
        let plan = EncoderPlan::new(&spec.ffmpeg, &spec.settings, &spec.output);
        let encoder = Encoder::spawn(&plan, &spec.output)?;

        let started = Instant::now();
        let mut initial = Core::new(spec.settings.clone());
        if matches!(spec.source, StreamSource::Native(_)) {
            initial.wait_for_frame();
        }
        let core = Arc::new(Mutex::new(initial));
        let (w, h) = client.size();
        core.lock()
            .unwrap_or_else(|e| e.into_inner())
            .connected(Duration::ZERO, w, h);
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(run(spec, client, encoder, core, started, stopped));
        Ok(Self {
            stop,
            task,
            started,
        })
    }

    /// Whether the recording ended on its own (max duration, or ffmpeg died).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Finalise: close ffmpeg's stdin, let it write its trailer, wait.
    pub async fn stop(self) -> Result<Finished> {
        let _ = self.stop.send(true);
        self.task
            .await
            .map_err(|e| anyhow!("recorder task failed: {e}"))?
    }
}

async fn run(
    spec: RecorderSpec,
    client: Stream,
    mut encoder: Encoder,
    core: Arc<Mutex<Core>>,
    started: Instant,
    mut stopped: watch::Receiver<bool>,
) -> Result<Finished> {
    let RecorderSpec {
        source,
        output,
        settings,
        inputs,
        ..
    } = spec;
    let reader_stop = stopped.clone();
    let reader = tokio::spawn(read_loop(
        source,
        client,
        core.clone(),
        started,
        reader_stop,
    ));
    let feed = tokio::spawn(input_loop(inputs, core.clone(), started));

    let dt = Duration::from_nanos(1_000_000_000 / u64::from(settings.fps));
    let mut ticks = tokio::time::interval(dt);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ended_by = None;
    let outcome: Result<()> = async {
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                _ = stopped.changed() => break,
            }
            let frames = core
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .tick(started.elapsed());
            for f in &frames {
                encoder.write_frame(&f.rgb).await?;
            }
            if let Some(max) = settings.max_duration {
                let video = core
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .video_duration();
                if video >= max {
                    ended_by = Some(format!(
                        "reached the maximum duration of {:.1} s",
                        max.as_secs_f64()
                    ));
                    break;
                }
            }
        }
        Ok(())
    }
    .await;
    reader.abort();
    feed.abort();

    // Flush what the clock owes (an open idle run, the last interval), then
    // the trailer. A write error means ffmpeg is gone; finish still reports
    // what it left behind.
    let (frames, duration, timeline) = {
        let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
        let frames = if outcome.is_ok() {
            c.finish(started.elapsed())
        } else {
            Vec::new()
        };
        (
            frames,
            c.video_duration(),
            settings
                .timeline
                .then(|| (c.timeline().to_vec(), c.cuts().to_vec())),
        )
    };
    let mut write_err = outcome.err();
    if write_err.is_none() {
        for f in &frames {
            if let Err(e) = encoder.write_frame(&f.rgb).await {
                write_err = Some(e);
                break;
            }
        }
    }
    let path = encoder.finish().await?;
    if let Some(e) = write_err {
        ended_by = Some(format!("{e:#}"));
    }

    let timeline = match timeline {
        Some((events, cuts)) => {
            let side = timeline_path(&output);
            let doc = serde_json::json!({
                "video": path.file_name().map(|n| n.to_string_lossy().into_owned()),
                "fps": settings.fps,
                "size": settings.size,
                "duration": duration.as_secs_f64(),
                "events": events,
                "cuts": cuts,
            });
            std::fs::write(&side, serde_json::to_vec_pretty(&doc)?)
                .with_context(|| format!("writing {}", side.display()))?;
            Some(side)
        }
        None => None,
    };
    Ok(Finished {
        path,
        duration,
        timeline,
        ended_by,
    })
}

/// Feed every update to the core; on a drop, mark the gap and reconnect.
async fn read_loop(
    source: StreamSource,
    mut client: Stream,
    core: Arc<Mutex<Core>>,
    started: Instant,
    mut stopped: watch::Receiver<bool>,
) {
    loop {
        // Stream until the connection fails.
        loop {
            let next = tokio::select! {
                r = client.next_update() => r,
                _ = stopped.changed() => return,
            };
            match next {
                Ok(events) => {
                    let at = started.elapsed();
                    let mut c = core.lock().unwrap_or_else(|e| e.into_inner());
                    for ev in events {
                        match ev {
                            StreamEvent::Rect { x, y, w, h, bgrx } => c.rect(at, x, y, w, h, &bgrx),
                            StreamEvent::DesktopSize { w, h } => c.desktop_size(at, w, h),
                            StreamEvent::Cursor {
                                hotspot,
                                w,
                                h,
                                rgba,
                            } => c.cursor(
                                at,
                                CursorShape {
                                    hotspot,
                                    width: w,
                                    height: h,
                                    rgba,
                                },
                            ),
                            StreamEvent::Pointer { x, y, visible } => {
                                c.pointer_state(at, i64::from(x), i64::from(y), visible)
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("recorder: display stream ended: {e:#}");
                    break;
                }
            }
        }
        core.lock()
            .unwrap_or_else(|e| e.into_inner())
            .disconnected(started.elapsed());
        // Retry until the socket is back (a restart's new hypervisor) or we
        // stop.
        loop {
            tokio::select! {
                _ = tokio::time::sleep(RECONNECT_EVERY) => {}
                _ = stopped.changed() => return,
            }
            if let Ok(c) = Stream::connect(&source).await {
                let (w, h) = c.size();
                core.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .connected(started.elapsed(), w, h);
                client = c;
                break;
            }
        }
    }
}

async fn input_loop(
    mut inputs: mpsc::UnboundedReceiver<TimedInput>,
    core: Arc<Mutex<Core>>,
    started: Instant,
) {
    while let Some(input) = inputs.recv().await {
        let at = input.at.saturating_duration_since(started);
        core.lock()
            .unwrap_or_else(|e| e.into_inner())
            .input(at, input.event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::encoder::{check, probe};
    use crate::record::input::InputEvent;
    use crate::record::{Format, RecordOptions, Size};
    use crate::vnc::fake::{self, pattern, raw_rect, update};

    /// Against the fake server: the recorder starts only once connected,
    /// survives the socket vanishing and coming back, takes inputs, and
    /// finalises to a file with a timeline beside it.
    #[tokio::test]
    async fn records_across_a_vanished_socket_and_finalises() {
        let avail = probe(None).await;
        let Ok(ffmpeg) = check(&avail, Format::Mp4) else {
            eprintln!("skipping: no ffmpeg with libx264 on this host");
            return;
        };
        let img = pattern(32, 16);
        let mut server = fake::spawn(32, 16, vec![update(&[raw_rect(&img, 0, 0, 32, 16)])]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("rec.mp4");
        let settings = Settings::resolve(
            &output,
            &RecordOptions {
                fps: Some(20),
                size: Some(Size::new(64, 32)),
                timeline: true,
                ..Default::default()
            },
        )
        .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let rec = Recorder::start(RecorderSpec {
            source: StreamSource::Vnc(server.path.clone()),
            output: output.clone(),
            settings,
            ffmpeg,
            inputs: rx,
        })
        .await
        .unwrap();
        assert_eq!(
            server.connections.load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send(TimedInput {
            at: Instant::now(),
            event: InputEvent::Click {
                button: "left".into(),
                x: Some(5),
                y: Some(5),
            },
        })
        .unwrap();
        server.vanish();
        tokio::time::sleep(Duration::from_millis(400)).await;
        server.reappear(32, 16, vec![update(&[raw_rect(&img, 0, 0, 32, 16)])]);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            server.connections.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "reconnected once the socket was back"
        );
        assert!(!rec.is_finished());

        let done = rec.stop().await.unwrap();
        assert_eq!(done.path, output);
        assert!(
            done.duration >= Duration::from_millis(1000),
            "{:?}",
            done.duration
        );
        assert!(output.metadata().unwrap().len() > 0);
        let side = done.timeline.unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&side).unwrap()).unwrap();
        assert_eq!(doc["events"][0]["kind"], "click");
        assert_eq!(doc["fps"], 20);
    }

    /// A socket nobody listens on is refused at start, not later.
    #[tokio::test]
    async fn start_fails_when_the_socket_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        let output = dir.path().join("x.mp4");
        let err = Recorder::start(RecorderSpec {
            source: StreamSource::Vnc(dir.path().join("missing.sock")),
            output: output.clone(),
            settings: Settings::resolve(&output, &RecordOptions::default()).unwrap(),
            ffmpeg: PathBuf::from("/nonexistent/ffmpeg"),
            inputs: rx,
        })
        .await
        .err()
        .expect("a missing socket refuses the start");
        assert!(format!("{err:#}").contains("missing.sock"), "{err:#}");
    }

    /// The recording stops itself at the maximum duration with a reason.
    #[tokio::test]
    async fn max_duration_ends_the_recording() {
        let avail = probe(None).await;
        let Ok(ffmpeg) = check(&avail, Format::Webm) else {
            eprintln!("skipping: no ffmpeg with libvpx-vp9 on this host");
            return;
        };
        let img = pattern(8, 8);
        let server = fake::spawn(8, 8, vec![update(&[raw_rect(&img, 0, 0, 8, 8)])]);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("short.webm");
        let settings = Settings::resolve(
            &output,
            &RecordOptions {
                fps: Some(20),
                size: Some(Size::new(32, 32)),
                max_duration: Some(0.5),
                ..Default::default()
            },
        )
        .unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        let rec = Recorder::start(RecorderSpec {
            source: StreamSource::Vnc(server.path.clone()),
            output,
            settings,
            ffmpeg,
            inputs: rx,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(rec.is_finished());
        let done = rec.stop().await.unwrap();
        assert!(
            done.ended_by
                .as_deref()
                .unwrap_or("")
                .contains("maximum duration"),
            "{done:?}"
        );
        assert!(done.duration >= Duration::from_millis(500));
    }

    /// The native source: vmlab-hv's frame protocol, across the socket going
    /// away and coming back (a `vm restart`), with the guest's own cursor.
    #[tokio::test]
    async fn records_the_native_stream_across_a_restart() {
        let avail = probe(None).await;
        let Ok(ffmpeg) = check(&avail, Format::Mp4) else {
            eprintln!("skipping: no ffmpeg with libx264 on this host");
            return;
        };
        let display = crate::hvdisplay::fake::spawn(32, 16);
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("native.mp4");
        let settings = Settings::resolve(
            &output,
            &RecordOptions {
                fps: Some(20),
                size: Some(Size::new(64, 32)),
                ..Default::default()
            },
        )
        .unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        let rec = Recorder::start(RecorderSpec {
            source: StreamSource::Native(display.path.clone()),
            output: output.clone(),
            settings,
            ffmpeg,
            inputs: rx,
        })
        .await
        .unwrap();
        display.cursor(vmlab_hv_display::Cursor {
            width: 2,
            height: 2,
            hot_x: 0,
            hot_y: 0,
            argb: vec![255; 16],
            x: 3,
            y: 3,
            visible: true,
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        display.vanish();
        tokio::time::sleep(Duration::from_millis(300)).await;
        display.reappear();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            display
                .connections
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 2,
            "reconnected once the socket was back"
        );
        assert!(!rec.is_finished());
        let done = rec.stop().await.unwrap();
        assert!(done.duration >= Duration::from_millis(1000), "{done:?}");
        assert!(output.metadata().unwrap().len() > 0);
    }

    #[tokio::test]
    async fn native_recording_excludes_placeholders_and_compresses_the_gap() {
        let avail = probe(None).await;
        let Ok(ffmpeg) = check(&avail, Format::Mp4) else {
            eprintln!("skipping: no ffmpeg with libx264 on this host");
            return;
        };
        let display = crate::hvdisplay::fake::spawn(128, 64);
        display.placeholder();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("placeholder.mp4");
        let settings = Settings::resolve(
            &output,
            &RecordOptions {
                fps: Some(20),
                size: Some(Size::new(128, 64)),
                compress_idle: Some(0.2),
                timeline: true,
                ..Default::default()
            },
        )
        .unwrap();
        let (_tx, rx) = mpsc::unbounded_channel();
        let rec = Recorder::start(RecorderSpec {
            source: StreamSource::Native(display.path.clone()),
            output: output.clone(),
            settings,
            ffmpeg,
            inputs: rx,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let green = [0, 255, 0, 0].repeat(128 * 64);
        display.show(
            128,
            64,
            green.clone(),
            vec![vmlab_hv_display::Rect {
                x: 0,
                y: 0,
                width: 128,
                height: 64,
            }],
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        for _ in 0..8 {
            display.placeholder();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        display.show(128, 64, green, vec![]);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let done = rec.stop().await.unwrap();
        let timeline: serde_json::Value =
            serde_json::from_slice(&std::fs::read(done.timeline.unwrap()).unwrap()).unwrap();
        assert!(
            timeline["cuts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|cut| cut["wall_start"].as_f64().unwrap() >= 0.35
                    && cut["wall_end"].as_f64().unwrap() - cut["wall_start"].as_f64().unwrap()
                        > 0.5),
            "{timeline}"
        );
        let decoded = tokio::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&output)
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"])
            .output()
            .await
            .unwrap();
        assert!(decoded.status.success());
        assert!(!decoded.stdout.is_empty());
        assert!(
            decoded
                .stdout
                .as_chunks::<3>()
                .0
                .iter()
                .all(|p| p[0] < 10 && p[1] > 240 && p[2] < 10),
            "every encoded frame is guest green, including the first and reset gap"
        );
    }
}
