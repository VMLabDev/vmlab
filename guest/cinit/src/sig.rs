//! Stop-signal parsing: OCI images carry `StopSignal` as a name ("SIGTERM",
//! sometimes "TERM") or occasionally a number.

use nix::sys::signal::Signal;

use crate::util::Result;

/// Parse `"SIGTERM"`, `"TERM"` or `"15"` (case-insensitive) into a signal.
pub fn parse_signal(name: &str) -> Result<Signal> {
    let trimmed = name.trim();
    if let Ok(num) = trimmed.parse::<i32>() {
        return Signal::try_from(num).map_err(|e| format!("bad signal number {trimmed}: {e}"));
    }
    let upper = trimmed.to_ascii_uppercase();
    let full = if upper.starts_with("SIG") {
        upper.clone()
    } else {
        format!("SIG{upper}")
    };
    Signal::iterator()
        .find(|s| s.as_str() == full)
        .ok_or(format!("unknown signal {name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_numbers_and_short_forms() {
        assert_eq!(parse_signal("SIGTERM").unwrap(), Signal::SIGTERM);
        assert_eq!(parse_signal("term").unwrap(), Signal::SIGTERM);
        assert_eq!(parse_signal("QUIT").unwrap(), Signal::SIGQUIT);
        assert_eq!(parse_signal("9").unwrap(), Signal::SIGKILL);
        assert_eq!(parse_signal(" SIGHUP ").unwrap(), Signal::SIGHUP);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_signal("SIGBOGUS").is_err());
        assert!(parse_signal("").is_err());
        assert!(parse_signal("999").is_err());
    }
}
