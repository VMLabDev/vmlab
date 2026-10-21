//! `vmlab record` (PRD §11.1): start, stop and list console recordings, and
//! the `--record <vm>=<path>` flags `script`, `up`, `vm start` and
//! `vm restart` share.
//!
//! The CLI resolves a relative output path against its own working
//! directory and sends an absolute one (the daemon stands somewhere else),
//! turns the flags into the one [`RecordOptions`] value every surface
//! carries, and renders what the daemon answers. Nothing here decides
//! anything about a recording; the lab daemon owns it.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use super::daemon::{abs_path, remote};
use super::emit;
use super::lab::{lab_client_for, rt, split_vm_ref};
use crate::proto::LabRequest;
use crate::record::{RecordOptions, RecordRequest, RecordStopped, RecordingStatus, Size};

/// The recording options as flags, shared by `record start` and every verb
/// that takes `--record`.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct RecordOptionFlags {
    /// Frames per second (default 30)
    #[arg(long, value_name = "N")]
    pub fps: Option<u32>,
    /// Output frame size; every frame is scaled and letterboxed into it
    /// (default 1920x1080)
    #[arg(long, value_name = "WxH")]
    pub size: Option<Size>,
    /// Cut idle stretches longer than SECS (default 2) to half a second
    #[arg(
        long,
        value_name = "SECS",
        num_args = 0..=1,
        default_missing_value = "2",
        require_equals = true
    )]
    pub compress_idle: Option<f64>,
    /// Write <output>.events.json listing every input vmlab sent, in video time
    #[arg(long)]
    pub timeline: bool,
    /// Finalise the recording after this long (e.g. 90s, 5m, 1h30m)
    #[arg(long, value_name = "DURATION")]
    pub max_duration: Option<String>,
}

impl RecordOptionFlags {
    /// The options value these flags spell.
    pub fn options(&self) -> Result<RecordOptions> {
        let max_duration = match &self.max_duration {
            Some(d) => Some(crate::record::parse_duration(d).map_err(|e| anyhow!(e))?),
            None => None,
        };
        Ok(RecordOptions {
            fps: self.fps,
            size: self.size,
            compress_idle: self.compress_idle,
            timeline: self.timeline,
            max_duration,
        })
    }
}

/// One `--record <vm>=<path>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordTarget {
    pub machine: String,
    pub path: PathBuf,
}

/// Parse `vm=path`. The path may itself contain `=`; the machine name may
/// not.
pub fn parse_record_target(s: &str) -> Result<RecordTarget, String> {
    let (machine, path) = s
        .split_once('=')
        .ok_or_else(|| format!("`{s}`: expected <vm>=<path>, e.g. vm01=demo.mp4"))?;
    if machine.is_empty() || path.is_empty() {
        return Err(format!("`{s}`: expected <vm>=<path>, e.g. vm01=demo.mp4"));
    }
    Ok(RecordTarget {
        machine: machine.to_string(),
        path: PathBuf::from(path),
    })
}

/// `--record <vm>=<path>` (repeatable) plus the option flags, which apply to
/// every recording named on the command line.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct RecordFlags {
    /// Record a VM's console to PATH for as long as this command's work
    /// lasts (repeatable)
    #[arg(long = "record", value_name = "VM=PATH", value_parser = parse_record_target)]
    pub record: Vec<RecordTarget>,
    #[command(flatten)]
    pub options: RecordOptionFlags,
}

impl RecordFlags {
    /// The requests to send, paths made absolute against this process's
    /// working directory and extensions checked before anything is sent.
    pub fn requests(&self) -> Result<Vec<RecordRequest>> {
        let options = self.options.options()?;
        let mut out = Vec::with_capacity(self.record.len());
        for t in &self.record {
            crate::record::Format::from_path(&t.path).map_err(|e| anyhow!(e))?;
            out.push(RecordRequest {
                machine: t.machine.clone(),
                path: Some(abs_path(&t.path)?.display().to_string()),
                options: options.clone(),
            });
        }
        Ok(out)
    }
}

#[derive(clap::Subcommand)]
pub enum RecordCmd {
    /// Start recording a VM's console to an MP4 or WebM file
    Start {
        /// VM, as `vm` or `lab/vm`
        vm: String,
        /// Output file; the extension picks the format (.mp4 or .webm).
        /// Default: .vmlab/recordings/<vm>-<timestamp>.mp4 in the lab
        #[arg(short = 'o', long, value_name = "PATH")]
        output: Option<PathBuf>,
        #[command(flatten)]
        options: RecordOptionFlags,
        /// Print the daemon's reply as JSON
        #[arg(long)]
        json: bool,
    },
    /// Stop a VM's recording and finalise the file
    Stop {
        /// VM, as `vm` or `lab/vm`
        vm: String,
        /// Print the daemon's reply as JSON
        #[arg(long)]
        json: bool,
    },
    /// List live recordings, for one VM or the whole lab
    Status {
        /// VM, as `vm` or `lab/vm`; omit for every machine in the lab
        vm: Option<String>,
        /// Print the daemon's reply as JSON
        #[arg(long)]
        json: bool,
    },
}

pub fn cmd_record(cmd: RecordCmd) -> Result<()> {
    match cmd {
        RecordCmd::Start {
            vm,
            output,
            options,
            json,
        } => cmd_start(&vm, output, &options, json),
        RecordCmd::Stop { vm, json } => cmd_stop(&vm, json),
        RecordCmd::Status { vm, json } => cmd_status(vm.as_deref(), json),
    }
}

fn cmd_start(
    vm_ref: &str,
    output: Option<PathBuf>,
    flags: &RecordOptionFlags,
    json: bool,
) -> Result<()> {
    let options = flags.options()?;
    let path = match output {
        Some(p) => {
            // Refuse a bad extension here, before a daemon is even reached.
            crate::record::Format::from_path(&p).map_err(|e| anyhow!(e))?;
            Some(abs_path(&p)?.display().to_string())
        }
        None => None,
    };
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(vm_ref)?;
        let (lab, client) = lab_client_for(lab).await?;
        let reply = client
            .send(LabRequest::MachineRecordStart {
                machine,
                path,
                options,
            })
            .await
            .map_err(remote)?;
        emit(json, &reply, |v| {
            match serde_json::from_value::<RecordingStatus>(v.clone()) {
                Ok(s) => format!("{}\n", render_started(&lab, &s)),
                Err(_) => format!("{v}\n"),
            }
        })
    })
}

fn cmd_stop(vm_ref: &str, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = split_vm_ref(vm_ref)?;
        let (lab, client) = lab_client_for(lab).await?;
        let reply = client
            .send(LabRequest::MachineRecordStop { machine })
            .await
            .map_err(remote)?;
        emit(json, &reply, |v| {
            match serde_json::from_value::<RecordStopped>(v.clone()) {
                Ok(s) => format!("{}\n", render_stopped(&lab, &s)),
                Err(_) => format!("{v}\n"),
            }
        })
    })
}

fn cmd_status(vm_ref: Option<&str>, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, machine) = match vm_ref {
            Some(r) => {
                let (lab, machine) = split_vm_ref(r)?;
                (lab, Some(machine))
            }
            None => (None, None),
        };
        let (_lab, client) = lab_client_for(lab).await?;
        let reply = client
            .send(LabRequest::MachineRecordStatus { machine })
            .await
            .map_err(remote)?;
        emit(json, &reply, |v| {
            match serde_json::from_value::<Vec<RecordingStatus>>(v.clone()) {
                Ok(list) => render_status(&list),
                Err(_) => format!("{v}\n"),
            }
        })
    })
}

/// `fps` and `size` and whatever else is on, in one bracket.
pub fn render_options(o: &RecordOptions) -> String {
    let mut parts = Vec::new();
    if let Some(fps) = o.fps {
        parts.push(format!("{fps} fps"));
    }
    if let Some(size) = o.size {
        parts.push(size.to_string());
    }
    if let Some(secs) = o.compress_idle {
        parts.push(format!("compress-idle {secs}s"));
    }
    if o.timeline {
        parts.push("timeline".to_string());
    }
    if let Some(secs) = o.max_duration {
        parts.push(format!("max {}", render_secs(secs)));
    }
    parts.join(", ")
}

fn render_secs(secs: f64) -> String {
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn render_started(lab: &str, s: &RecordingStatus) -> String {
    format!(
        "recording {lab}/{} to {} ({})",
        s.machine,
        s.path.display(),
        render_options(&s.options)
    )
}

fn render_stopped(lab: &str, s: &RecordStopped) -> String {
    let mut line = format!(
        "stopped recording {lab}/{}: {} ({} of video)",
        s.machine,
        s.path.display(),
        render_secs(s.duration_secs)
    );
    if let Some(t) = &s.timeline {
        line.push_str(&format!("\ntimeline: {}", t.display()));
    }
    if let Some(why) = &s.ended_by {
        line.push_str(&format!("\nit had already ended: {why}"));
    }
    line
}

pub fn render_status(list: &[RecordingStatus]) -> String {
    if list.is_empty() {
        return "no recordings\n".to_string();
    }
    let name_w = list
        .iter()
        .map(|r| r.machine.len())
        .max()
        .unwrap_or(0)
        .max("MACHINE".len());
    let mut out = format!(
        "{:<name_w$} {:>8}  {:<19}  PATH  (OPTIONS)\n",
        "MACHINE", "ELAPSED", "STARTED"
    );
    for r in list {
        out.push_str(&format!(
            "{:<name_w$} {:>8}  {}  {}  ({})\n",
            r.machine,
            render_secs(r.elapsed_secs),
            r.started_at.format("%Y-%m-%d %H:%M:%S"),
            r.path.display(),
            render_options(&r.options),
        ));
    }
    out
}

/// The sentence a verb that took `--record` prints once its work is done.
pub fn finished_line(requests: &[RecordRequest]) -> Option<String> {
    if requests.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    for r in requests {
        lines.push(format!(
            "recorded {} to {}",
            r.machine,
            r.path.as_deref().unwrap_or("its default path")
        ));
    }
    Some(lines.join("\n"))
}

/// Refuse `--record` flags that cannot be right before any request is sent.
pub fn check_targets(requests: &[RecordRequest]) -> Result<()> {
    for (i, r) in requests.iter().enumerate() {
        if requests[..i].iter().any(|o| o.machine == r.machine) {
            bail!("--record names `{}` more than once", r.machine);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_target_is_vm_equals_path() {
        assert_eq!(
            parse_record_target("vm01=out/demo.mp4").unwrap(),
            RecordTarget {
                machine: "vm01".into(),
                path: PathBuf::from("out/demo.mp4"),
            }
        );
        assert_eq!(
            parse_record_target("vm01=/x/a=b.mp4").unwrap().path,
            PathBuf::from("/x/a=b.mp4")
        );
        assert!(parse_record_target("vm01").is_err());
        assert!(parse_record_target("=demo.mp4").is_err());
        assert!(parse_record_target("vm01=").is_err());
    }

    #[test]
    fn flags_become_one_options_value_and_absolute_paths() {
        let flags = RecordFlags {
            record: vec![parse_record_target("a=demo.mp4").unwrap()],
            options: RecordOptionFlags {
                fps: Some(24),
                size: Some(Size::new(1280, 720)),
                compress_idle: Some(2.0),
                timeline: true,
                max_duration: Some("5m".into()),
            },
        };
        let reqs = flags.requests().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].machine, "a");
        assert!(PathBuf::from(reqs[0].path.as_deref().unwrap()).is_absolute());
        assert_eq!(reqs[0].options.fps, Some(24));
        assert_eq!(reqs[0].options.max_duration, Some(300.0));
        assert!(reqs[0].options.timeline);

        let gif = RecordFlags {
            record: vec![parse_record_target("a=demo.gif").unwrap()],
            ..Default::default()
        };
        assert!(gif.requests().unwrap_err().to_string().contains(".gif"));
        let bad = RecordOptionFlags {
            max_duration: Some("soon".into()),
            ..Default::default()
        };
        assert!(bad.options().is_err());
        check_targets(&reqs).unwrap();
        let twice = [reqs[0].clone(), reqs[0].clone()];
        assert!(check_targets(&twice).is_err());
    }

    #[test]
    fn status_renders_a_table_or_says_none() {
        assert_eq!(render_status(&[]), "no recordings\n");
        let r = RecordingStatus {
            machine: "vm01".into(),
            path: PathBuf::from("/r/demo.mp4"),
            started_at: chrono::DateTime::parse_from_rfc3339("2026-10-21T10:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            elapsed_secs: 95.0,
            options: RecordOptions {
                fps: Some(30),
                size: Some(Size::new(1920, 1080)),
                compress_idle: Some(2.0),
                timeline: false,
                max_duration: None,
            },
        };
        let out = render_status(std::slice::from_ref(&r));
        assert!(out.contains("vm01"), "{out}");
        assert!(out.contains("1m35s"), "{out}");
        assert!(
            out.contains("/r/demo.mp4  (30 fps, 1920x1080, compress-idle 2s)"),
            "{out}"
        );
        assert_eq!(
            render_started("lab", &r),
            "recording lab/vm01 to /r/demo.mp4 (30 fps, 1920x1080, compress-idle 2s)"
        );
    }
}
