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
    stamp_build();
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

/// `VMLAB_BUILD`: which build this binary is, for `src/version.rs` — the
/// crate version, plus the git revision it was built from where there is
/// one (`0.31.0+1a2b3c4`, or `0.31.0` from a tarball). The CLI refuses to
/// drive a daemon of another build, so this has to tell two builds of the
/// same version apart: every developer build is `0.1.0`.
///
/// Rebuilt when `HEAD` or the ref it points at moves, which is what cargo
/// can watch; a dirty tree rebuilt without a commit keeps its revision.
fn stamp_build() {
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let build = match git_revision() {
        Some(rev) => format!("{version}+{rev}"),
        None => version,
    };
    println!("cargo:rustc-env=VMLAB_BUILD={build}");
}

fn git_revision() -> Option<String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let rev = git(&["rev-parse", "--short=7", "HEAD"])?;
    // `--git-path` answers through a worktree's `.git` file to the real files.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={path}");
    }
    Some(rev)
}
