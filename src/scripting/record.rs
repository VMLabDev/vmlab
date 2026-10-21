//! `m.record(path, opts)` (PRD §11.1, D7): a console recording a script
//! starts, as a handle whose `stop()` finalises it. A recording the script
//! never stops is finalised by the lab daemon when the run ends, success or
//! failure — the daemon, not the language, provides the guarantee, so a host
//! method that cannot take a closure still gives a scoped recording.

use std::collections::HashMap;
use std::sync::Arc;

use wscript::Script;

use crate::labd::lab::LabRuntime;
use crate::record::RecordOptions;

/// A recording a script started (wscript surface 4).
#[derive(Script)]
#[script(name = "Recording")]
#[script(opaque)]
pub struct RecordingHandle {
    pub(crate) machine: String,
    pub(crate) path: String,
    pub(crate) runtime: Arc<LabRuntime>,
    pub(crate) rt: tokio::runtime::Handle,
}

impl RecordingHandle {
    /// Where the video is being written.
    pub fn path(&self) -> String {
        self.path.clone()
    }

    /// Finalise the recording and return the file's path. Stopping twice, or
    /// after the daemon ended it, is an error naming the machine.
    pub fn stop(&self) -> Result<String, String> {
        let runtime = self.runtime.clone();
        let machine = self.machine.clone();
        let stopped = self
            .rt
            .block_on(async move { runtime.record_stop(&machine).await })
            .map_err(|e| format!("{e:#}"))?;
        Ok(stopped.path.display().to_string())
    }
}

/// The `opts` map's spellings, the same as the CLI flags': `fps`, `size`
/// (`WxH`), `compress_idle` (seconds, or `true` for the default threshold),
/// `timeline` (`true`/`false`) and `max_duration` (`90`, `90s`, `5m`,
/// `1h30m`). wscript maps are homogeneous, so every value is a string.
pub fn parse_opts(opts: &HashMap<String, String>) -> Result<RecordOptions, String> {
    let mut out = RecordOptions::default();
    for (key, value) in opts {
        let value = value.trim();
        match key.as_str() {
            "fps" => {
                out.fps = Some(
                    value
                        .parse()
                        .map_err(|_| format!("record opts: fps `{value}` is not a whole number"))?,
                )
            }
            "size" => out.size = Some(value.parse().map_err(|e| format!("record opts: {e}"))?),
            "compress_idle" => out.compress_idle = Some(match value {
                "true" | "yes" | "on" => crate::record::DEFAULT_IDLE_THRESHOLD.as_secs_f64(),
                v => v.parse().map_err(|_| {
                    format!(
                        "record opts: compress_idle `{v}` is not a number of seconds (or `true`)"
                    )
                })?,
            }),
            "timeline" => {
                out.timeline = match value {
                    "true" | "yes" | "on" => true,
                    "false" | "no" | "off" => false,
                    v => return Err(format!("record opts: timeline `{v}` is not true or false")),
                }
            }
            "max_duration" => {
                out.max_duration = Some(
                    crate::record::parse_duration(value)
                        .map_err(|e| format!("record opts: {e}"))?,
                )
            }
            other => {
                return Err(format!(
                    "record opts: unknown key `{other}`; the keys are fps, size, compress_idle, \
                     timeline and max_duration"
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn opts_take_the_cli_spellings() {
        let o = parse_opts(&map(&[
            ("fps", "24"),
            ("size", "1280x720"),
            ("compress_idle", "2"),
            ("timeline", "true"),
            ("max_duration", "5m"),
        ]))
        .unwrap();
        assert_eq!(o.fps, Some(24));
        assert_eq!(o.size, Some(crate::record::Size::new(1280, 720)));
        assert_eq!(o.compress_idle, Some(2.0));
        assert!(o.timeline);
        assert_eq!(o.max_duration, Some(300.0));
        assert_eq!(
            parse_opts(&map(&[("compress_idle", "true")]))
                .unwrap()
                .compress_idle,
            Some(2.0)
        );
        assert_eq!(
            parse_opts(&HashMap::new()).unwrap(),
            RecordOptions::default()
        );
    }

    #[test]
    fn opts_refuse_unknown_keys_and_bad_values() {
        assert!(
            parse_opts(&map(&[("fsp", "24")]))
                .unwrap_err()
                .contains("unknown key")
        );
        assert!(
            parse_opts(&map(&[("fps", "fast")]))
                .unwrap_err()
                .contains("fps")
        );
        assert!(
            parse_opts(&map(&[("timeline", "maybe")]))
                .unwrap_err()
                .contains("timeline")
        );
        assert!(
            parse_opts(&map(&[("size", "big")]))
                .unwrap_err()
                .contains("size")
        );
    }
}
