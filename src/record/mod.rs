//! Console recording (PRD §11.1): a VM's console as an MP4 or WebM file.
//!
//! The recorder holds one long-lived, shared RFB connection to the VM's
//! existing VNC socket, receives incremental framebuffer updates and QEMU's
//! cursor shape, composites the pointer and a ripple at every click vmlab
//! sends, and pipes constant-rate frames to host `ffmpeg`. It depends only on
//! a socket path, an input feed and an options value — never on the lab
//! runtime — so the supervisor can host it for template builds later.
//!
//! Split per ADR-0003: [`core`] is the pure part (timestamped events in,
//! frames, cuts and a timeline out, no I/O); [`encoder`] turns a format into
//! an `ffmpeg` argv and a discovery decision as values; [`Recorder`] is the
//! executor that owns the RFB read loop, the ffmpeg child and the clock.

pub mod core;
pub mod encoder;
mod executor;
pub mod input;

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use executor::{Finished, Recorder, RecorderSpec};
pub use input::{InputEvent, InputTap, TimedInput};

/// Where a lab keeps recordings nobody named: `.vmlab/recordings/`, beside
/// `screenshots/`.
pub const RECORDING_DIR: &str = "recordings";

/// Frames per second when the caller names none.
pub const DEFAULT_FPS: u32 = 30;
/// Output size when the caller names none (D17): a recording started at a
/// 640x480 boot screen and carried into a 1920x1080 desktop lands in one
/// frame size, letterboxed.
pub const DEFAULT_SIZE: Size = Size {
    width: 1920,
    height: 1080,
};
/// Idle threshold when `--compress-idle` is given without a figure (D16).
pub const DEFAULT_IDLE_THRESHOLD: Duration = Duration::from_secs(2);
/// What an idle stretch longer than the threshold becomes (D16).
pub const IDLE_KEEP: Duration = Duration::from_millis(500);

/// The container and codec a recording is written in, chosen by the output
/// path's extension (D21). Nothing else is accepted, `.gif` included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// H.264 in MP4, `yuv420p`.
    Mp4,
    /// VP9 in WebM.
    Webm,
}

impl Format {
    /// The format `path`'s extension names, or why it names none.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("mp4") => Ok(Format::Mp4),
            Some("webm") => Ok(Format::Webm),
            Some(other) => Err(format!(
                "recording `{}`: unsupported extension `.{other}`; a recording is written \
                 as `.mp4` (H.264) or `.webm` (VP9) — convert it afterwards for anything else",
                path.display()
            )),
            None => Err(format!(
                "recording `{}`: the path needs an `.mp4` or `.webm` extension",
                path.display()
            )),
        }
    }

    /// The ffmpeg encoder this format needs.
    pub fn encoder(self) -> &'static str {
        match self {
            Format::Mp4 => "libx264",
            Format::Webm => "libvpx-vp9",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::Webm => "webm",
        }
    }
}

/// An output frame size, spelled `WIDTHxHEIGHT` everywhere a user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Why this is not a size a recording can be written at, if it is not:
    /// both dimensions even (4:2:0 chroma subsampling needs it) and within
    /// what an encoder takes.
    pub fn validate(self) -> Result<Self, String> {
        if self.width < 16 || self.height < 16 || self.width > 7680 || self.height > 4320 {
            return Err(format!(
                "size {self}: each dimension must be between 16 and 7680x4320"
            ));
        }
        if !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(format!(
                "size {self}: both dimensions must be even (yuv420p video cannot hold an odd edge)"
            ));
        }
        Ok(self)
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl FromStr for Size {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("size `{s}`: expected WIDTHxHEIGHT, e.g. 1280x720");
        let (w, h) = s.split_once(['x', 'X']).ok_or_else(bad)?;
        let width = w.trim().parse::<u32>().map_err(|_| bad())?;
        let height = h.trim().parse::<u32>().map_err(|_| bad())?;
        Ok(Size { width, height })
    }
}

impl Serialize for Size {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// What a caller may ask of a recording, as every surface spells it: the
/// `vmlab record` flags, the `--record` flags on `script`/`up`/`vm start`/
/// `vm restart`, wscript's `opts` map and the wire. Every field optional;
/// [`Settings::resolve`] fills the defaults and validates.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RecordOptions {
    /// Frames per second; 30 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Output frame size; 1920x1080 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    /// Compress idle stretches longer than this many seconds to half a
    /// second (D16). Absent: no compression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compress_idle: Option<f64>,
    /// Write `<output>.events.json` listing every input vmlab sent (D13).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timeline: bool,
    /// Finalise the recording after this many seconds of video (D14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration: Option<f64>,
}

/// A recording's resolved parameters: the options with defaults applied and
/// every figure checked, plus the format the path chose.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub format: Format,
    pub fps: u32,
    pub size: Size,
    /// Idle stretches longer than this are cut to [`IDLE_KEEP`].
    pub compress_idle: Option<Duration>,
    pub timeline: bool,
    pub max_duration: Option<Duration>,
}

impl Settings {
    /// Resolve `options` against `output`'s extension, refusing anything an
    /// encoder cannot take before any socket is opened.
    pub fn resolve(output: &Path, options: &RecordOptions) -> Result<Self, String> {
        let format = Format::from_path(output)?;
        let fps = options.fps.unwrap_or(DEFAULT_FPS);
        if !(1..=120).contains(&fps) {
            return Err(format!("fps {fps}: must be between 1 and 120"));
        }
        let size = options.size.unwrap_or(DEFAULT_SIZE).validate()?;
        let compress_idle = match options.compress_idle {
            None => None,
            Some(secs) if secs.is_finite() && secs > 0.0 => Some(Duration::from_secs_f64(secs)),
            Some(secs) => {
                return Err(format!(
                    "compress_idle {secs}: the idle threshold must be a positive number of seconds"
                ));
            }
        };
        let max_duration = match options.max_duration {
            None => None,
            Some(secs) if secs.is_finite() && secs > 0.0 => Some(Duration::from_secs_f64(secs)),
            Some(secs) => {
                return Err(format!(
                    "max_duration {secs}: must be a positive number of seconds"
                ));
            }
        };
        Ok(Settings {
            format,
            fps,
            size,
            compress_idle,
            timeline: options.timeline,
            max_duration,
        })
    }

    /// The options that resolve to these settings, for status.
    pub fn options(&self) -> RecordOptions {
        RecordOptions {
            fps: Some(self.fps),
            size: Some(self.size),
            compress_idle: self.compress_idle.map(|d| d.as_secs_f64()),
            timeline: self.timeline,
            max_duration: self.max_duration.map(|d| d.as_secs_f64()),
        }
    }
}

/// One recording a request asks a lab daemon to start alongside what it is
/// doing: `--record <vm>=<path>` on `script`, `up`, `vm start` and
/// `vm restart` (D8, D20). The one shape every such request carries.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RecordRequest {
    /// The machine to record.
    pub machine: String,
    /// Absolute output path; absent means the lab's default
    /// (`.vmlab/recordings/<machine>-<stamp>.mp4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub options: RecordOptions,
}

/// A live recording as status reports it (ADR-0004) and as
/// `machine.record_start` / `machine.record_status` answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub machine: String,
    pub path: std::path::PathBuf,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Wall-clock seconds since it started.
    pub elapsed_secs: f64,
    /// The options in force, defaults filled in.
    pub options: RecordOptions,
}

// The status projection is `Eq` (ADR-0004) and these ride in it. Their only
// non-`Eq` members are finite seconds (an elapsed time, a threshold, a cap)
// that never hold a NaN, so equality is reflexive in practice.
impl Eq for RecordingStatus {}
impl Eq for RecordOptions {}

/// What `machine.record_stop` answers: the finalised file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordStopped {
    pub machine: String,
    pub path: std::path::PathBuf,
    /// Length of the video written, in seconds.
    pub duration_secs: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<std::path::PathBuf>,
    /// Why it had already ended, when it was not this stop: the maximum
    /// duration, or an encoder failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_by: Option<String>,
}

/// The sidecar a `timeline` recording writes beside its video.
pub fn timeline_path(output: &Path) -> std::path::PathBuf {
    let mut name = output
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".events.json");
    output.with_file_name(name)
}

/// The default output path for `machine`: `<lab_local>/recordings/<machine>-<UTC stamp>.mp4`.
pub fn default_output(lab_local: &Path, machine: &str) -> std::path::PathBuf {
    lab_local.join(RECORDING_DIR).join(format!(
        "{machine}-{}.mp4",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f")
    ))
}

/// Parse a duration a user typed: a bare number of seconds, or a figure
/// with `s`, `m` or `h` units, chained (`90`, `90s`, `5m`, `1h30m`).
pub fn parse_duration(s: &str) -> Result<f64, String> {
    let s = s.trim();
    let bad = || {
        format!(
            "duration `{s}`: expected seconds, or a figure with s/m/h units such as 90s, 5m or 1h30m"
        )
    };
    if s.is_empty() {
        return Err(bad());
    }
    if let Ok(secs) = s.parse::<f64>() {
        return if secs.is_finite() && secs > 0.0 {
            Ok(secs)
        } else {
            Err(bad())
        };
    }
    let mut total = 0.0;
    let mut num = String::new();
    for ch in s.chars() {
        match ch {
            '0'..='9' | '.' => num.push(ch),
            'h' | 'm' | 's' => {
                let n: f64 = num.parse().map_err(|_| bad())?;
                num.clear();
                total += n * match ch {
                    'h' => 3600.0,
                    'm' => 60.0,
                    _ => 1.0,
                };
            }
            _ => return Err(bad()),
        }
    }
    if !num.is_empty() || total <= 0.0 {
        return Err(bad());
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_follows_the_extension_and_refuses_everything_else() {
        assert_eq!(Format::from_path(Path::new("a/b.mp4")), Ok(Format::Mp4));
        assert_eq!(Format::from_path(Path::new("b.WEBM")), Ok(Format::Webm));
        let gif = Format::from_path(Path::new("out.gif")).unwrap_err();
        assert!(
            gif.contains(".gif") && gif.contains(".mp4") && gif.contains(".webm"),
            "{gif}"
        );
        assert!(
            Format::from_path(Path::new("out"))
                .unwrap_err()
                .contains("extension")
        );
    }

    #[test]
    fn size_parses_and_validates() {
        assert_eq!("1280x720".parse::<Size>(), Ok(Size::new(1280, 720)));
        assert_eq!("1280X720".parse::<Size>(), Ok(Size::new(1280, 720)));
        assert!("1280".parse::<Size>().is_err());
        assert!("ax720".parse::<Size>().is_err());
        assert!(
            Size::new(1281, 720)
                .validate()
                .unwrap_err()
                .contains("even")
        );
        assert!(Size::new(8, 8).validate().is_err());
        assert_eq!(
            serde_json::to_string(&Size::new(640, 480)).unwrap(),
            "\"640x480\""
        );
        assert_eq!(
            serde_json::from_str::<Size>("\"640x480\"").unwrap(),
            Size::new(640, 480)
        );
    }

    #[test]
    fn settings_apply_defaults_and_refuse_bad_figures() {
        let s = Settings::resolve(Path::new("x.mp4"), &RecordOptions::default()).unwrap();
        assert_eq!((s.fps, s.size, s.format), (30, DEFAULT_SIZE, Format::Mp4));
        assert_eq!(s.compress_idle, None);
        let opts = RecordOptions {
            fps: Some(0),
            ..Default::default()
        };
        assert!(
            Settings::resolve(Path::new("x.mp4"), &opts)
                .unwrap_err()
                .contains("fps")
        );
        let opts = RecordOptions {
            compress_idle: Some(-1.0),
            ..Default::default()
        };
        assert!(
            Settings::resolve(Path::new("x.webm"), &opts)
                .unwrap_err()
                .contains("compress_idle")
        );
        let opts = RecordOptions {
            compress_idle: Some(2.0),
            max_duration: Some(90.0),
            timeline: true,
            ..Default::default()
        };
        let s = Settings::resolve(Path::new("x.webm"), &opts).unwrap();
        assert_eq!(s.compress_idle, Some(Duration::from_secs(2)));
        assert_eq!(
            s.options(),
            RecordOptions {
                fps: Some(30),
                size: Some(DEFAULT_SIZE),
                ..opts
            }
        );
    }

    #[test]
    fn options_omit_what_was_not_asked_on_the_wire() {
        let json = serde_json::to_value(RecordOptions::default()).unwrap();
        assert_eq!(json, serde_json::json!({}));
        let back: RecordOptions =
            serde_json::from_value(serde_json::json!({"fps": 24, "size": "640x480"})).unwrap();
        assert_eq!(back.fps, Some(24));
        assert_eq!(back.size, Some(Size::new(640, 480)));
    }

    #[test]
    fn timeline_sits_beside_the_video() {
        assert_eq!(
            timeline_path(Path::new("/r/out.mp4")),
            Path::new("/r/out.mp4.events.json")
        );
    }

    #[test]
    fn durations_take_seconds_or_units() {
        assert_eq!(parse_duration("90"), Ok(90.0));
        assert_eq!(parse_duration("90s"), Ok(90.0));
        assert_eq!(parse_duration("5m"), Ok(300.0));
        assert_eq!(parse_duration("1h30m"), Ok(5400.0));
        assert_eq!(parse_duration("1.5s"), Ok(1.5));
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("5m3").is_err());
    }
}
