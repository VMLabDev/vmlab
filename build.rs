//! Compile `VK_LAYER_VMLAB_pci_filter` (PRD §5.2) into a shared object for
//! `src/qemu/vk_layer.rs` to embed.
//!
//! Why C through `cc` rather than a Rust `cdylib`: a layer is loaded by the
//! Vulkan loader into QEMU's process, so it has to be a shared object for
//! the host the binary runs on, built by the same `cargo build` with no
//! release artefact of its own. A `cdylib` crate would need a second cargo
//! invocation from here (cargo has no stable artifact dependencies) and an
//! `unsafe` exception from the crate's lint. One C file, compiled by the
//! target's C compiler that ring, zstd-sys and aws-lc-sys already require,
//! needs neither — and declares its own Vulkan types, so no Vulkan SDK
//! either.

use std::path::PathBuf;
use std::process::Command;

const SOURCE: &str = "src/qemu/vk_pci_filter.c";

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let so = out.join("libVkLayer_vmlab_pci_filter.so");
    let compiler = cc::Build::new()
        .pic(true)
        .opt_level(2)
        .debug(false)
        .cargo_metadata(false)
        .get_compiler();
    let mut cmd: Command = compiler.to_command();
    cmd.args([
        "-std=c11",
        "-shared",
        "-fvisibility=hidden",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-s",
        "-o",
    ])
    .arg(&so)
    .arg(SOURCE)
    .arg("-lpthread");
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("running the C compiler for {SOURCE}: {e}"));
    assert!(status.success(), "compiling {SOURCE} failed: {status}");
}
