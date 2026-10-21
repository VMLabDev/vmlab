//! Host `ffmpeg` (D2, D15): where it is, whether it has the encoder a format
//! needs, the argv a recording runs it with, and the child itself.
//!
//! Discovery and the encoder check are decisions over data (ADR-0003): the
//! configured path, what is on `PATH` and what `ffmpeg -encoders` printed go
//! in, a path or a refusal comes out. Only [`probe`] and [`Encoder`] touch
//! the host.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};

use super::{Format, Settings};

/// What a recording start knows about the host's ffmpeg.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Availability {
    /// The binary, from the host config's `ffmpeg` key or `PATH`.
    pub ffmpeg: Option<PathBuf>,
    /// Encoder names `ffmpeg -encoders` listed.
    pub encoders: BTreeSet<String>,
}

/// The `ffmpeg` to run: the host config's `ffmpeg` key when set, else the
/// first `ffmpeg` on `PATH` as `on_path` finds it.
pub fn locate(
    configured: Option<&Path>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    configured
        .map(Path::to_path_buf)
        .or_else(|| on_path("ffmpeg"))
}

/// The encoder names in `ffmpeg -encoders` output: each entry is a flags
/// column, the name, then a description.
pub fn parse_encoders(listing: &str) -> BTreeSet<String> {
    listing
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let flags = cols.next()?;
            let name = cols.next()?;
            (flags.len() == 6 && flags.starts_with('V')).then(|| name.to_string())
        })
        .collect()
}

/// Whether a recording in `format` can start on this host, and with which
/// binary. The refusal names both discovery routes and the missing encoder.
pub fn check(avail: &Availability, format: Format) -> Result<PathBuf, String> {
    let Some(ffmpeg) = avail.ffmpeg.as_ref() else {
        return Err(
            "recording needs ffmpeg: none found on PATH, and the host config (config.wcl) \
             names none under `ffmpeg`"
                .to_string(),
        );
    };
    let needed = format.encoder();
    if !avail.encoders.contains(needed) {
        return Err(format!(
            "recording a .{} needs the `{needed}` encoder, which `{} -encoders` does not list; \
             install an ffmpeg built with it, or point the host config's `ffmpeg` key at one",
            format.extension(),
            ffmpeg.display()
        ));
    }
    Ok(ffmpeg.clone())
}

/// Find ffmpeg (host config, then `PATH`) and ask it for its encoders.
pub async fn probe(configured: Option<&Path>) -> Availability {
    let ffmpeg = locate(configured, on_path);
    let encoders = match &ffmpeg {
        Some(bin) => match Command::new(bin)
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .output()
            .await
        {
            Ok(out) => parse_encoders(&String::from_utf8_lossy(&out.stdout)),
            Err(_) => BTreeSet::new(),
        },
        None => BTreeSet::new(),
    };
    Availability { ffmpeg, encoders }
}

fn on_path(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|p| p.is_file())
    })
}

/// The `ffmpeg` invocation a recording runs, as a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl EncoderPlan {
    /// Raw `rgb24` frames of `settings.size` at `settings.fps` on stdin,
    /// encoded into `output` in the settings' format.
    pub fn new(ffmpeg: &Path, settings: &Settings, output: &Path) -> Self {
        let mut args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-s",
            &settings.size.to_string(),
            "-r",
            &settings.fps.to_string(),
            "-i",
            "pipe:0",
            "-an",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        match settings.format {
            Format::Mp4 => args.extend(
                [
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-crf",
                    "23",
                    "-pix_fmt",
                    "yuv420p",
                    "-movflags",
                    "+faststart",
                ]
                .iter()
                .map(|s| s.to_string()),
            ),
            Format::Webm => args.extend(
                [
                    "-c:v",
                    "libvpx-vp9",
                    "-deadline",
                    "realtime",
                    "-cpu-used",
                    "6",
                    "-row-mt",
                    "1",
                    "-b:v",
                    "0",
                    "-crf",
                    "33",
                    "-pix_fmt",
                    "yuv420p",
                ]
                .iter()
                .map(|s| s.to_string()),
            ),
        }
        args.push("-r".into());
        args.push(settings.fps.to_string());
        args.push(output.display().to_string());
        Self {
            program: ffmpeg.to_path_buf(),
            args,
        }
    }
}

/// A running ffmpeg taking frames on stdin.
pub struct Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    output: PathBuf,
}

impl Encoder {
    /// Spawn the plan. The output's parent directory is created.
    pub fn spawn(plan: &EncoderPlan, output: &Path) -> Result<Self> {
        if let Some(parent) = output.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut child = Command::new(&plan.program)
            .args(&plan.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", plan.program.display()))?;
        let stdin = child.stdin.take();
        Ok(Self {
            child,
            stdin,
            output: output.to_path_buf(),
        })
    }

    /// Feed one frame. An error here means ffmpeg went away; the recording
    /// is over.
    pub async fn write_frame(&mut self, rgb: &[u8]) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("ffmpeg stdin already closed"))?;
        stdin
            .write_all(rgb)
            .await
            .map_err(|e| anyhow!("writing a frame to ffmpeg: {e}"))
    }

    /// Close stdin so ffmpeg writes its trailer, and wait for it.
    pub async fn finish(mut self) -> Result<PathBuf> {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.shutdown().await;
            drop(stdin);
        }
        let stderr = self.child.stderr.take();
        let status = self.child.wait().await.context("waiting for ffmpeg")?;
        let mut err = String::new();
        if let Some(mut s) = stderr {
            use tokio::io::AsyncReadExt;
            let _ = s.read_to_string(&mut err).await;
        }
        if !status.success() {
            bail!(
                "ffmpeg exited with {status} while writing {}: {}",
                self.output.display(),
                err.trim()
            );
        }
        Ok(self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{RecordOptions, Size};

    const LISTING: &str = "Encoders:
 V..... = Video
 A..... = Audio
 ------
 V....D libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)
 V....D libvpx-vp9           libvpx VP9 (codec vp9)
 A....D aac                  AAC (Advanced Audio Coding)
";

    fn avail(ffmpeg: Option<&str>, encoders: &[&str]) -> Availability {
        Availability {
            ffmpeg: ffmpeg.map(PathBuf::from),
            encoders: encoders.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn the_host_config_wins_over_path() {
        let cfg = PathBuf::from("/opt/ffmpeg/bin/ffmpeg");
        assert_eq!(
            locate(Some(&cfg), |_| Some(PathBuf::from("/usr/bin/ffmpeg"))),
            Some(cfg)
        );
        assert_eq!(
            locate(None, |b| Some(PathBuf::from("/usr/bin").join(b))),
            Some(PathBuf::from("/usr/bin/ffmpeg"))
        );
        assert_eq!(locate(None, |_| None), None);
    }

    #[test]
    fn encoders_are_read_from_the_listing() {
        let enc = parse_encoders(LISTING);
        assert!(enc.contains("libx264") && enc.contains("libvpx-vp9"));
        assert!(!enc.contains("aac"), "video encoders only");
    }

    #[test]
    fn the_refusal_names_both_routes_or_the_missing_encoder() {
        let none = check(&Availability::default(), Format::Mp4).unwrap_err();
        assert!(
            none.contains("PATH") && none.contains("`ffmpeg`") && none.contains("config.wcl"),
            "{none}"
        );
        let no_vp9 =
            check(&avail(Some("/usr/bin/ffmpeg"), &["libx264"]), Format::Webm).unwrap_err();
        assert!(
            no_vp9.contains("libvpx-vp9") && no_vp9.contains("-encoders"),
            "{no_vp9}"
        );
        assert_eq!(
            check(&avail(Some("/usr/bin/ffmpeg"), &["libx264"]), Format::Mp4),
            Ok(PathBuf::from("/usr/bin/ffmpeg"))
        );
    }

    fn settings(path: &str, opts: RecordOptions) -> Settings {
        Settings::resolve(Path::new(path), &opts).unwrap()
    }

    #[test]
    fn the_argv_reads_raw_rgb_frames_and_encodes_per_format() {
        let s = settings(
            "/tmp/out.mp4",
            RecordOptions {
                fps: Some(24),
                size: Some(Size::new(1280, 720)),
                ..Default::default()
            },
        );
        let plan = EncoderPlan::new(Path::new("/usr/bin/ffmpeg"), &s, Path::new("/tmp/out.mp4"));
        let joined = plan.args.join(" ");
        assert!(
            joined.contains("-f rawvideo -pix_fmt rgb24 -s 1280x720 -r 24 -i pipe:0"),
            "{joined}"
        );
        assert!(
            joined.contains("-c:v libx264") && joined.contains("-pix_fmt yuv420p"),
            "{joined}"
        );
        assert!(joined.ends_with("/tmp/out.mp4"));
        let s = settings("/tmp/out.webm", RecordOptions::default());
        let plan = EncoderPlan::new(Path::new("ffmpeg"), &s, Path::new("/tmp/out.webm"));
        assert!(plan.args.join(" ").contains("-c:v libvpx-vp9"));
        assert!(plan.args.join(" ").contains("-s 1920x1080 -r 30"));
    }

    /// With a real ffmpeg on PATH, a few frames become a file ffprobe reads
    /// back with the right codec, size and length. Skipped cleanly without.
    #[tokio::test]
    async fn real_ffmpeg_writes_a_probeable_file() {
        let avail = probe(None).await;
        let Ok(ffmpeg) = check(&avail, Format::Mp4) else {
            eprintln!("skipping: no ffmpeg with libx264 on this host");
            return;
        };
        if on_path("ffprobe").is_none() {
            eprintln!("skipping: no ffprobe on this host");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("t.mp4");
        let s = settings(
            "t.mp4",
            RecordOptions {
                fps: Some(10),
                size: Some(Size::new(64, 32)),
                ..Default::default()
            },
        );
        let plan = EncoderPlan::new(&ffmpeg, &s, &out);
        let mut enc = Encoder::spawn(&plan, &out).unwrap();
        let frame = vec![200u8; 64 * 32 * 3];
        for _ in 0..20 {
            enc.write_frame(&frame).await.unwrap();
        }
        enc.finish().await.unwrap();

        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,width,height:format=duration",
                "-of",
                "json",
            ])
            .arg(&out)
            .output()
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
        assert_eq!(v["streams"][0]["codec_name"], "h264");
        assert_eq!(v["streams"][0]["width"], 64);
        assert_eq!(v["streams"][0]["height"], 32);
        let dur: f64 = v["format"]["duration"].as_str().unwrap().parse().unwrap();
        assert!((dur - 2.0).abs() < 0.15, "20 frames at 10 fps: {dur}");
    }
}
