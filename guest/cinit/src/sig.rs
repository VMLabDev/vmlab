//! Stop-signal parsing: OCI images carry `StopSignal` as a name ("SIGTERM",
//! sometimes "TERM") or occasionally a number.

use nix::sys::signal::Signal;

use crate::util::Result;

// Docker's Linux StopSignal numbering on x86_64 and aarch64. cinit's own
// libc can reserve a different number of signals than the workload's libc.
const RT_MIN: libc::c_int = 34;
const RT_MAX: libc::c_int = 64;

/// Parse a case-insensitive signal name or a decimal signal number.
pub fn parse_signal(name: &str) -> Result<libc::c_int> {
    let trimmed = name.trim();
    if let Ok(num) = trimmed.parse::<i32>() {
        if (RT_MIN..=RT_MAX).contains(&num) {
            return Ok(num);
        }
        return Signal::try_from(num)
            .map(|signal| signal as libc::c_int)
            .map_err(|e| format!("bad signal number {trimmed}: {e}"));
    }
    let upper = trimmed.to_ascii_uppercase();
    let short = upper.strip_prefix("SIG").unwrap_or(&upper);
    match short {
        "RTMIN" => return Ok(RT_MIN),
        "RTMAX" => return Ok(RT_MAX),
        _ => {}
    }
    if let Some(offset) = short
        .strip_prefix("RTMIN+")
        .or_else(|| short.strip_prefix("RTMAX-"))
        && let Ok(offset) = offset.parse::<u32>()
        && offset <= (RT_MAX - RT_MIN) as u32
    {
        return Ok(if short.starts_with("RTMIN+") {
            RT_MIN + offset as libc::c_int
        } else {
            RT_MAX - offset as libc::c_int
        });
    }
    Signal::iterator()
        .find(|s| s.as_str().strip_prefix("SIG") == Some(short))
        .map(|signal| signal as libc::c_int)
        .ok_or(format!("unknown signal {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_numbers_and_short_forms() {
        assert_eq!(parse_signal("SIGTERM").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("term").unwrap(), libc::SIGTERM);
        assert_eq!(parse_signal("QUIT").unwrap(), libc::SIGQUIT);
        assert_eq!(parse_signal("9").unwrap(), libc::SIGKILL);
        assert_eq!(parse_signal(" SIGHUP ").unwrap(), libc::SIGHUP);
    }

    #[test]
    fn parses_realtime_signals() {
        for (name, number) in [
            ("SIGRTMIN+3", 37),
            ("RTMIN+3", 37),
            ("37", 37),
            ("sigrtmin", 34),
            ("RTMAX-1", 63),
            ("SIGRTMAX", 64),
        ] {
            assert_eq!(parse_signal(name).unwrap(), number, "{name}");
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_signal("SIGBOGUS").is_err());
        assert!(parse_signal("").is_err());
        assert!(parse_signal("999").is_err());
        assert!(parse_signal("0").is_err());
        assert!(parse_signal("65").is_err());
        assert!(parse_signal("RTMIN+31").is_err());
        assert!(parse_signal("RTMAX-31").is_err());
    }
}
