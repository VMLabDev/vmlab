//! How a VM's UEFI variable store was last booted (PRD §5.2).
//!
//! A VM's `OVMF_VARS.fd` is copied once and kept, so it outlives an edit of
//! `secure_boot`. The secure-boot firmware trusts whatever store it is given:
//! one copied from the blank template leaves it in setup mode, where it
//! verifies nothing, and one a guest ran on without secure boot may hold keys
//! that guest wrote, because the flash is only protected while secure boot is
//! on. Neither can be told from an enrolled store by its size.
//!
//! So each start writes, beside the store, whether it boots with secure boot.
//! A secure-boot start on a store whose last start had none is refused. The
//! record travels with the store: an offline snapshot keeps it, a freeze
//! carries it.
//!
//! A store with no record is one from before vmlab kept it. It is taken at
//! its word, unless it is byte for byte the blank template under a pairing
//! that asked for enrolled keys, which no enrolled store is.

use std::path::Path;

use anyhow::{Context, Result};

/// The record's file name, beside `OVMF_VARS.fd`.
pub(super) const FILE: &str = "OVMF_VARS.boot";

/// What the store's last start was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LastBoot {
    /// Without secure boot: the guest could write every variable.
    Plain,
    /// With secure boot, from the first start on or since the store was made.
    Secure,
}

impl LastBoot {
    pub(super) fn of(secure_boot: bool) -> Self {
        if secure_boot {
            LastBoot::Secure
        } else {
            LastBoot::Plain
        }
    }

    fn word(self) -> &'static str {
        match self {
            LastBoot::Plain => "plain",
            LastBoot::Secure => "secure",
        }
    }
}

/// The record at `path`, or `None` where there is none. A record that does
/// not say `secure` vouches for nothing, whatever else it says.
pub(super) fn read(path: &Path) -> Result<Option<LastBoot>> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim() == LastBoot::Secure.word() => Ok(Some(LastBoot::Secure)),
        Ok(_) => Ok(Some(LastBoot::Plain)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write the record, by a rename, so a reader never sees half of one.
pub(super) fn write(path: &Path, boot: LastBoot) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let staged = path.with_file_name(name);
    std::fs::write(&staged, format!("{}\n", boot.word()))
        .and_then(|()| std::fs::rename(&staged, path))
        .with_context(|| format!("writing {}", path.display()))
}

/// Why a secure-boot start must not use the store at `store`, or `None`
/// when it may. `blank` is the blank VARS template of the store's layout,
/// asked for only under a pairing that wants enrolled keys.
pub(super) fn refusal(
    vm: &str,
    store: &Path,
    record: Option<LastBoot>,
    blank: Option<&Path>,
) -> Result<Option<String>> {
    let why = match record {
        Some(LastBoot::Secure) => return Ok(None),
        Some(LastBoot::Plain) => {
            "was last booted without secure boot, when the guest could write every variable in \
             it, its keys included"
        }
        None => {
            let Some(blank) = blank else {
                return Ok(None);
            };
            let same = std::fs::read(store)
                .with_context(|| format!("reading {}", store.display()))?
                == std::fs::read(blank).with_context(|| format!("reading {}", blank.display()))?;
            if !same {
                return Ok(None);
            }
            "is the blank template with no keys enrolled, which leaves the firmware in setup \
             mode, verifying nothing"
        }
    };
    Ok(Some(format!(
        "vm \"{vm}\": secure_boot = true, but its UEFI variable store {} {why}. vmlab will not \
         boot the secure-boot firmware on it. Delete that OVMF_VARS.fd to start with a freshly \
         enrolled store (the VM loses its UEFI boot entries and variables), or set \
         secure_boot = false",
        store.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_round_trips_and_anything_else_reads_as_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(FILE);
        assert_eq!(read(&path).unwrap(), None);
        for boot in [LastBoot::Secure, LastBoot::Plain] {
            write(&path, boot).unwrap();
            assert_eq!(read(&path).unwrap(), Some(boot));
        }
        std::fs::write(&path, "secure, honest").unwrap();
        assert_eq!(read(&path).unwrap(), Some(LastBoot::Plain));
    }

    #[test]
    fn a_store_with_no_record_is_refused_only_when_it_is_the_blank_template() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("OVMF_VARS.fd");
        let blank = tmp.path().join("blank.fd");
        std::fs::write(&blank, b"blank").unwrap();

        std::fs::write(&store, b"blank").unwrap();
        let said = refusal("dc01", &store, None, Some(&blank))
            .unwrap()
            .expect("the blank template is refused");
        assert!(said.contains("dc01") && said.contains("setup mode"), "{said}");
        // `secure_boot_keys = "setup"` asks for exactly that store.
        assert_eq!(refusal("dc01", &store, None, None).unwrap(), None);

        std::fs::write(&store, b"keyed").unwrap();
        assert_eq!(refusal("dc01", &store, None, Some(&blank)).unwrap(), None);
        assert!(
            refusal("dc01", &store, Some(LastBoot::Plain), Some(&blank))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            refusal("dc01", &store, Some(LastBoot::Secure), Some(&blank)).unwrap(),
            None
        );
    }
}
