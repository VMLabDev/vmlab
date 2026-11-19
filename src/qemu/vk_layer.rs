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
//!
//! The same layer carries a second rule, off by default (PRD §5.2, a host
//! GPU's `hide_uncached_memory`): with [`HIDE_UNCACHED_VAR`] set to `1` it
//! reports an uncached host-visible memory type as not host-visible when the
//! same heap has a cached counterpart, so no guest driver maps memory the
//! host's CPU and GPU do not see alike.

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

/// `1` turns the layer's memory rule on: an uncached host-visible memory
/// type with a cached counterpart in the same heap is reported as not
/// host-visible.
pub const HIDE_UNCACHED_VAR: &str = "VMLAB_VK_HIDE_UNCACHED";

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

    const LOCAL: u32 = 0x1;
    const VISIBLE: u32 = 0x2;
    const COHERENT: u32 = 0x4;
    const CACHED: u32 = 0x8;

    /// Run the layer's memory rule on `(heap, flags)` types, through the
    /// layer's own source built as a program by `build.rs`: the types after
    /// the rule, and for each hidden type the index of its counterpart.
    fn rule(types: &[(u32, u32)]) -> (Vec<(u32, u32)>, Vec<(usize, usize)>) {
        let out = std::process::Command::new(concat!(env!("OUT_DIR"), "/vk-rule-check"))
            .args(types.iter().map(|(heap, flags)| format!("{heap}:{flags:x}")))
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let mut after = Vec::new();
        let mut hidden = Vec::new();
        for (i, line) in String::from_utf8(out.stdout).unwrap().lines().enumerate() {
            let mut words = line.split(' ');
            let (heap, flags) = words.next().unwrap().split_once(':').unwrap();
            after.push((
                heap.parse().unwrap(),
                u32::from_str_radix(flags, 16).unwrap(),
            ));
            if let Some(to) = words.next() {
                hidden.push((i, to.parse().unwrap()));
            }
        }
        (after, hidden)
    }

    /// The NVIDIA list of #136: type 2, host-visible and coherent but not
    /// cached, has type 3 in its heap, and is the one type hidden. The
    /// device-local host-visible type 4 stays, as does everything else.
    #[test]
    fn the_rule_hides_nvidias_uncached_type_and_names_the_cached_one() {
        let types = [
            (1, 0),
            (0, LOCAL),
            (1, VISIBLE | COHERENT),
            (1, VISIBLE | COHERENT | CACHED),
            (0, LOCAL | VISIBLE | COHERENT),
        ];
        let (after, hidden) = rule(&types);
        assert_eq!(hidden, [(2, 3)]);
        let mut want = types;
        want[2].1 = 0;
        assert_eq!(after, want);
    }

    /// A cached type in another heap is no counterpart: the uncached type
    /// is the only way to map that heap, so it stays.
    #[test]
    fn the_rule_keeps_an_uncached_type_whose_heap_has_no_cached_one() {
        let types = [
            (0, LOCAL),
            (1, VISIBLE | COHERENT),
            (2, VISIBLE | COHERENT | CACHED),
        ];
        assert_eq!(rule(&types), (types.to_vec(), vec![]));
    }

    /// The counterpart has every flag of the type it stands in for: a
    /// protected or otherwise flagged uncached type is not hidden behind a
    /// cached type that lacks the flag, and is behind one that has it.
    #[test]
    fn the_rule_wants_every_flag_of_the_hidden_type_on_its_counterpart() {
        const OTHER: u32 = 0x40;
        let types = [
            (0, VISIBLE | COHERENT | OTHER),
            (0, VISIBLE | COHERENT | CACHED),
        ];
        assert_eq!(rule(&types), (types.to_vec(), vec![]));
        let types = [
            (0, VISIBLE | COHERENT | OTHER),
            (0, VISIBLE | COHERENT | CACHED),
            (0, VISIBLE | COHERENT | CACHED | OTHER),
        ];
        let (after, hidden) = rule(&types);
        assert_eq!(hidden, [(0, 2)]);
        assert_eq!(after[0], (0, OTHER));
        assert_eq!(after[1..], types[1..]);
    }

    /// What the rule leaves alone: a cached type, a device-local one even
    /// when uncached, a host-visible type that is not coherent, and a list
    /// with nothing host-visible at all. The counterpart may come first.
    #[test]
    fn the_rule_touches_only_coherent_uncached_types_that_are_not_device_local() {
        let types = [
            (0, VISIBLE | COHERENT | CACHED),
            (0, LOCAL | VISIBLE | COHERENT),
            (0, VISIBLE),
            (0, LOCAL),
        ];
        assert_eq!(rule(&types), (types.to_vec(), vec![]));
        assert_eq!(rule(&[]), (vec![], vec![]));
        let types = [(0, VISIBLE | COHERENT | CACHED), (0, VISIBLE | COHERENT)];
        let (after, hidden) = rule(&types);
        assert_eq!(hidden, [(1, 0)]);
        assert_eq!(after, [(0, VISIBLE | COHERENT | CACHED), (0, 0)]);
    }

    /// Two uncached types of one heap are both hidden, each judged on the
    /// driver's own answer, and all 32 types are read.
    #[test]
    fn the_rule_hides_every_uncached_type_with_a_counterpart() {
        let mut types = vec![(0, VISIBLE | COHERENT); 31];
        types.push((0, VISIBLE | COHERENT | CACHED));
        let (after, hidden) = rule(&types);
        assert_eq!(hidden, (0..31).map(|i| (i, 31)).collect::<Vec<_>>());
        assert!(after[..31].iter().all(|t| *t == (0, 0)));
        assert_eq!(after[31], types[31]);
    }

    /// The embedded layer answers the memory queries and reads its switch.
    #[test]
    fn the_embedded_layer_carries_the_memory_rule() {
        for needle in [
            "vkGetPhysicalDeviceMemoryProperties2KHR",
            HIDE_UNCACHED_VAR,
        ] {
            assert!(
                LIBRARY
                    .windows(needle.len())
                    .any(|w| w == needle.as_bytes()),
                "{needle}"
            );
        }
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
