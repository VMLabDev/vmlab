use serde::{Deserialize, Serialize};

/// One revision of vmlab's host API exposed to wscript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WscriptSurfaceVersion(u32);

impl From<u32> for WscriptSurfaceVersion {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for WscriptSurfaceVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Script source embedded in a template and the host surface it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmbeddedWscript {
    pub source: String,
    pub surface_version: Option<WscriptSurfaceVersion>,
}

/// Surface written into templates built by this host, and the newest one it
/// compiles. Version zero predates the explicit contract; one is the first
/// stamped surface. Two adds, on the machine handle, `logins()`,
/// `as_login()`/`as_account()` (§19.2's wscript rung), `eject_cdrom()`,
/// `boot_id()` and `wait_rebooted()`; and makes `seg.route_to()` /
/// `seg.unroute_to()` work rather than fail. Three adds host USB passthrough
/// (§5.2.1): `lab.usb_devices()` and, on the machine handle, `usb_attach()`,
/// `usb_attach_force()`, `usb_detach()`, `usb_detach_all()` and
/// `usb_attached()`. Four adds console recording (§11.1): `record()` on the
/// machine handle and the `Recording` handle it returns, with `stop()` and
/// `path()`. Five adds OCR options (§10.3): `ocr_opts()` and
/// `wait_for_text_opts()` on the machine handle.
pub(crate) const WSCRIPT_SURFACE_VERSION: WscriptSurfaceVersion = WscriptSurfaceVersion(5);
/// Oldest stamped surface this host promises to compile. Unstamped templates
/// are legacy and remain accepted through compatibility aliases.
const MIN_WSCRIPT_SURFACE_VERSION: WscriptSurfaceVersion = WscriptSurfaceVersion(1);

/// Refuse a template whose embedded scripts this host cannot promise to
/// compile, by name and before any of them is compiled: below the floor it
/// wants rebuilding, above `WSCRIPT_SURFACE_VERSION` it wants a newer vmlab.
pub(crate) fn ensure_wscript_surface_supported(
    template: &str,
    recorded: Option<WscriptSurfaceVersion>,
) -> Result<(), String> {
    if let Some(version) = recorded
        && version < MIN_WSCRIPT_SURFACE_VERSION
    {
        return Err(format!(
            "template `{template}` records wscript surface {version}, below this host's supported \
             floor {MIN_WSCRIPT_SURFACE_VERSION}; rebuild the template with this vmlab version"
        ));
    }
    if let Some(version) = recorded
        && version > WSCRIPT_SURFACE_VERSION
    {
        return Err(format!(
            "template `{template}` records wscript surface {version}, newer than this host's \
             {WSCRIPT_SURFACE_VERSION}; upgrade vmlab to use it"
        ));
    }
    Ok(())
}
