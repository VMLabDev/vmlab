//! Kernel command line: the host passes the root and scratch block devices as
//! `vmlab.root=<dev>` and `vmlab.scratch=<dev>`.

use crate::util::{Ctx, Result};

/// The two vmlab parameters from /proc/cmdline.
#[derive(Debug, PartialEq, Eq)]
pub struct VmlabCmdline {
    /// Squashfs image device (read-only lower layer), e.g. `/dev/vda`.
    pub root: String,
    /// Scratch device for the overlay upper layer, e.g. `/dev/vdb`.
    pub scratch: String,
}

pub fn read() -> Result<VmlabCmdline> {
    let raw = std::fs::read_to_string("/proc/cmdline").ctx("read /proc/cmdline")?;
    parse(&raw)
}

/// Parse a kernel command line. Values are plain device paths, so no quoting
/// rules apply — whitespace splitting is exact.
pub fn parse(raw: &str) -> Result<VmlabCmdline> {
    let lookup = |key: &str| -> Option<String> {
        raw.split_whitespace()
            .find_map(|w| w.strip_prefix(key).map(str::to_string))
            .filter(|v| !v.is_empty())
    };
    let root = lookup("vmlab.root=").ok_or("kernel cmdline: missing vmlab.root=<dev>")?;
    let scratch = lookup("vmlab.scratch=").ok_or("kernel cmdline: missing vmlab.scratch=<dev>")?;
    Ok(VmlabCmdline { root, scratch })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_params() {
        let got = parse("console=ttyS0 vmlab.root=/dev/vda quiet vmlab.scratch=/dev/vdb").unwrap();
        assert_eq!(got.root, "/dev/vda");
        assert_eq!(got.scratch, "/dev/vdb");
    }

    #[test]
    fn missing_root_is_an_error() {
        let err = parse("console=ttyS0 vmlab.scratch=/dev/vdb").unwrap_err();
        assert!(err.contains("vmlab.root"), "{err}");
    }

    #[test]
    fn missing_scratch_is_an_error() {
        let err = parse("vmlab.root=/dev/vda").unwrap_err();
        assert!(err.contains("vmlab.scratch"), "{err}");
    }

    #[test]
    fn empty_value_counts_as_missing() {
        assert!(parse("vmlab.root= vmlab.scratch=/dev/vdb").is_err());
    }
}
