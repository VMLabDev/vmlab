//! A Windows guest reads the RTC as UTC (PRD §7.3, #166). QEMU starts the
//! RTC at the host's UTC time, and Windows reads it as local time unless
//! `RealTimeIsUniversal` says otherwise — so without the flag every cold
//! boot is wrong by the guest's UTC offset, and a fixed `clock` instant is
//! read as a different one. The bootstrap installer sets the flag; the agent
//! ensures it at every start as well, which is how a template built before
//! either did gets it, through `up`'s agent update or `repair-agent`.
//!
//! The kernel reads the flag at boot, so one this agent had to write takes
//! effect at the next boot; the host sets the running clock once after the
//! push that brought it (`labd::agent_repair`).
//!
//! Portable so the decision is tested on any host; the Windows half only
//! reads and writes the registry.

/// The key, under `HKLM`.
pub const KEY: &str = r"SYSTEM\CurrentControlSet\Control\TimeZoneInformation";
/// The `REG_DWORD` value.
pub const VALUE: &str = "RealTimeIsUniversal";
/// What it must hold.
pub const WANTED: u32 = 1;

/// Make the flag [`WANTED`], writing it only where it is absent or holds
/// anything else. Returns whether it wrote.
pub fn ensure(
    read: impl FnOnce() -> Result<Option<u32>, String>,
    write: impl FnOnce(u32) -> Result<(), String>,
) -> Result<bool, String> {
    if read()? == Some(WANTED) {
        return Ok(false);
    }
    write(WANTED)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn run(current: Result<Option<u32>, String>) -> (Result<bool, String>, Option<u32>) {
        let written = Cell::new(None);
        let out = ensure(
            || current,
            |v| {
                written.set(Some(v));
                Ok(())
            },
        );
        (out, written.get())
    }

    /// A guest already reading the RTC as UTC is left exactly as it was.
    #[test]
    fn a_set_flag_is_not_written() {
        assert_eq!(run(Ok(Some(1))), (Ok(false), None));
    }

    /// Absent — every Windows install's default — or zero, it is written 1.
    #[test]
    fn an_absent_or_other_flag_is_written_one() {
        assert_eq!(run(Ok(None)), (Ok(true), Some(1)));
        assert_eq!(run(Ok(Some(0))), (Ok(true), Some(1)));
    }

    /// A registry that cannot be read is reported, and nothing is written.
    #[test]
    fn a_failed_read_writes_nothing() {
        assert_eq!(run(Err("denied".into())), (Err("denied".into()), None));
    }

    /// A failed write is reported, not taken for success.
    #[test]
    fn a_failed_write_is_an_error() {
        assert_eq!(
            ensure(|| Ok(None), |_| Err("denied".into())),
            Err("denied".into())
        );
    }

    /// The installer writes the same value the agent ensures.
    #[test]
    fn the_installer_sets_the_same_flag() {
        let install = include_str!("../../../src/template/bootstrap/install.cmd");
        assert!(
            install.contains(&format!(
                r#"reg add "HKLM\{KEY}" /v {VALUE} /t REG_DWORD /d {WANTED} /f"#
            )),
            "install.cmd must set {VALUE}"
        );
    }
}
