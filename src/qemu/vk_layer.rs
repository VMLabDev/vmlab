//! The Vulkan layer that leaves a `vulkan` guest only its own host GPU
//! (PRD §5.2).
//!
//! The loader's driver filter cannot tell two identical cards apart, so the
//! lab daemon gives every `vulkan` emulator with a picked host GPU an
//! implicit layer, `VK_LAYER_VMLAB_pci_filter`, which hides every physical
//! device but the one at that GPU's PCI address. It is always on rather than
//! only for a host with identical cards: it costs a property query per
//! device at enumeration, and the guest then sees the same thing on every
//! host. The layer is `src/qemu/vk_pci_filter.c`, compiled by `build.rs` and
//! embedded here; at VM start [`install`] writes it and its manifest into a
//! content-addressed directory under the data directory, which the
//! emulator's `VK_ADD_IMPLICIT_LAYER_PATH` names. The data directory rather
//! than the runtime one because the loader maps the library executable, and
//! the runtime directory's fallback under `/tmp` is mounted `noexec` on
//! hardened hosts.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// The compiled layer.
pub const LIBRARY: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/libVkLayer_vmlab_pci_filter.so"));

/// The layer's name, as the loader lists it.
pub const NAME: &str = "VK_LAYER_VMLAB_pci_filter";

/// Names the PCI address the layer keeps, `0000:01:00.0`.
pub const ADDRESS_VAR: &str = "VMLAB_VK_PCI_ADDRESS";

/// The manifest's `disable_environment`: `1` stops the loader loading it.
pub const DISABLE_VAR: &str = "VMLAB_VK_PCI_FILTER_DISABLE";

/// The loader variable that adds a directory of implicit layer manifests.
pub const PATH_VAR: &str = "VK_ADD_IMPLICIT_LAYER_PATH";

const LIBRARY_FILE: &str = "libVkLayer_vmlab_pci_filter.so";
const MANIFEST_FILE: &str = "VkLayer_vmlab_pci_filter.json";

/// The directory this build's layer lives in under `data`: named by the
/// library's digest, so a directory once written never changes and two vmlab
/// builds running side by side never write over each other's.
pub fn dir(data: &Path) -> PathBuf {
    let digest = hex::encode(Sha256::digest(LIBRARY));
    data.join("vk-layer").join(&digest[..16])
}

/// The layer manifest for a library at `library`: an implicit layer of the
/// loader's interface version 2 that the loader skips while
/// [`DISABLE_VAR`] is `1`.
pub fn manifest(library: &Path) -> String {
    let manifest = serde_json::json!({
        "file_format_version": "1.1.2",
        "layer": {
            "name": NAME,
            "type": "GLOBAL",
            "library_path": library,
            "api_version": "1.1.0",
            "implementation_version": "1",
            "description": "vmlab: hide every Vulkan physical device but the one at VMLAB_VK_PCI_ADDRESS",
            "functions": {
                "vkNegotiateLoaderLayerInterfaceVersion": "vkNegotiateLoaderLayerInterfaceVersion"
            },
            "disable_environment": { DISABLE_VAR: "1" }
        }
    });
    serde_json::to_string_pretty(&manifest).expect("a JSON value serialises")
}

/// Write the layer and its manifest into `dir` unless both are already
/// there, each through a temporary file and a rename, so an emulator
/// starting beside this never reads half a file.
pub fn install(dir: &Path) -> Result<()> {
    let library = dir.join(LIBRARY_FILE);
    let manifest_path = dir.join(MANIFEST_FILE);
    if library.is_file() && manifest_path.is_file() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    write_new(&library, LIBRARY, 0o755)?;
    write_new(&manifest_path, manifest(&library).as_bytes(), 0o644)?;
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().expect("a file in a directory");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a file in {}", dir.display()))?;
    std::io::Write::write_all(&mut tmp, bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded layer is a shared object exporting the one entry point
    /// its manifest names.
    #[test]
    fn the_embedded_layer_is_a_shared_object_with_the_negotiation_entry_point() {
        assert_eq!(&LIBRARY[..4], b"\x7fELF");
        let needle = b"vkNegotiateLoaderLayerInterfaceVersion";
        assert!(LIBRARY.windows(needle.len()).any(|w| w == needle));
    }

    /// The manifest names the library by absolute path, the layer by name,
    /// its entry point, and the variable that disables it.
    #[test]
    fn the_manifest_names_the_library_and_the_disable_variable() {
        let m: serde_json::Value = serde_json::from_str(&manifest(Path::new(
            "/run/x/libVkLayer_vmlab_pci_filter.so",
        )))
        .unwrap();
        let layer = &m["layer"];
        assert_eq!(layer["name"], NAME);
        assert_eq!(layer["type"], "GLOBAL");
        assert_eq!(
            layer["library_path"],
            "/run/x/libVkLayer_vmlab_pci_filter.so"
        );
        assert_eq!(
            layer["functions"]["vkNegotiateLoaderLayerInterfaceVersion"],
            "vkNegotiateLoaderLayerInterfaceVersion"
        );
        assert_eq!(layer["disable_environment"][DISABLE_VAR], "1");
    }

    /// The directory is named by the library's digest, under `vk-layer/`.
    #[test]
    fn the_layer_directory_is_content_addressed_under_the_data_dir() {
        let d = dir(Path::new("/home/u/.local/share/vmlab"));
        assert_eq!(
            d.parent().unwrap(),
            Path::new("/home/u/.local/share/vmlab/vk-layer")
        );
        let name = d.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 16);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(dir(Path::new("/home/u/.local/share/vmlab")), d);
    }

    /// Install writes the library byte for byte and a manifest pointing at
    /// it, and a second install leaves both as they are.
    #[test]
    fn install_writes_the_library_and_a_manifest_pointing_at_it() {
        let tmp = tempfile::tempdir().unwrap();
        let d = dir(tmp.path());
        install(&d).unwrap();
        let library = d.join(LIBRARY_FILE);
        assert_eq!(std::fs::read(&library).unwrap(), LIBRARY);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&library).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.join(MANIFEST_FILE)).unwrap()).unwrap();
        assert_eq!(m["layer"]["library_path"], library.to_str().unwrap());
        let before = std::fs::metadata(&library).unwrap().modified().unwrap();
        install(&d).unwrap();
        let after = std::fs::metadata(&library).unwrap().modified().unwrap();
        assert_eq!(before, after);
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        assert_eq!(names.len(), 2, "no temporary files left behind");
    }
}
