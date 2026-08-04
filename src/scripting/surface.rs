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

/// Surface written into templates built by this host. Version zero predates
/// the explicit contract; one is the first stamped surface.
pub(crate) const WSCRIPT_SURFACE_VERSION: WscriptSurfaceVersion = WscriptSurfaceVersion(1);
/// Oldest stamped surface this host promises to compile. Unstamped templates
/// are legacy and remain accepted through compatibility aliases.
const MIN_WSCRIPT_SURFACE_VERSION: WscriptSurfaceVersion = WscriptSurfaceVersion(1);

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
    Ok(())
}
