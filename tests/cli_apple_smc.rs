//! Exercise validation and the pre-daemon refusal through the actual CLI.

use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, verb: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vmlab"))
        .arg(verb)
        .current_dir(root)
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env_remove("VMLAB_INSTANCE")
        .output()
        .unwrap()
}

fn assert_valid_or_missing_kvm(output: &Output) {
    let error = String::from_utf8_lossy(&output.stderr);
    if vmlab::config::hypervisor::HostFacts::probe().kvm {
        assert!(output.status.success(), "{error}");
    } else {
        assert!(!output.status.success());
        assert!(!error.contains("requires host.apple_smc_osk"), "{error}");
        assert!(error.contains("needs KVM"), "{error}");
    }
}

fn lab(root: &Path) {
    std::fs::write(
        root.join("vmlab.wcl"),
        r#"import <vmlab.wcl>
lab "osk-check" {
  vm "linux" { template = "scratch" arch = "x86_64" profile = "linux-generic" disk = 1GiB }
  vm "mac" { template = "scratch" arch = "x86_64" profile = "macos" disk = 1GiB }
}
"#,
    )
    .unwrap();
}

#[test]
fn missing_osk_refuses_validate_and_up_before_daemons_or_disks() {
    let scratch = tempfile::tempdir().unwrap();
    lab(scratch.path());
    for verb in ["validate", "up"] {
        let output = run(scratch.path(), verb);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        for part in [
            "apple_smc = true requires host.apple_smc_osk",
            "obtain it from your own Apple hardware.",
            "host-config.wcl (Apple SMC OSK)",
        ] {
            assert!(error.contains(part), "missing {part:?}: {error}");
        }
        assert!(!scratch.path().join("runtime/vmlab/vmlabd.sock").exists());
        assert!(!scratch.path().join(".vmlab").exists());
    }
}

#[test]
fn a_fake_host_key_allows_macos_validation() {
    let scratch = tempfile::tempdir().unwrap();
    lab(scratch.path());
    let config = scratch.path().join("config/vmlab");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.wcl"),
        "import <vmlab-host.wcl>\nhost { apple_smc_osk = \"TESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTESTTEST\" }\n"
    ).unwrap();
    let output = run(scratch.path(), "validate");
    assert_valid_or_missing_kvm(&output);
    if output.status.success() {
        assert!(String::from_utf8_lossy(&output.stdout).contains("ok: lab"));
    }
}

#[test]
fn a_template_profile_requires_the_osk_and_refuses_qemu_at_validation() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    let version = root.join("data/vmlab/templates/x86_64/macos-test/1");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::write(version.join("disk.qcow2"), b"").unwrap();
    vmlab::template::TemplateMeta::from_wcl(
        "import <vmlab-meta.wcl>\ntemplate_meta \"macos-test\" { arch = \"x86_64\" version = \"1\" created = \"2026-01-01T00:00:00Z\" profile = \"macos\" }", "<test>")
        .unwrap().write_to(&version.join("template.wcl")).unwrap();
    let source = "import <vmlab.wcl>\nlab \"template-osk-check\" { vm \"mac\" { template = \"x86_64/macos-test\" } }";
    std::fs::write(root.join("vmlab.wcl"), source).unwrap();
    let missing = run(root, "validate");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("requires host.apple_smc_osk"));
    assert!(!root.join(".vmlab").exists());

    let config = root.join("config/vmlab");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.wcl"),
        "import <vmlab-host.wcl>\nhost { apple_smc_osk = \"FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE\" }\n").unwrap();
    let configured = run(root, "validate");
    assert_valid_or_missing_kvm(&configured);
    std::fs::write(
        root.join("vmlab.wcl"),
        source.replace("template =", "hypervisor = \"qemu\" template ="),
    )
    .unwrap();
    let qemu = run(root, "validate");
    assert!(!qemu.status.success());
    let error = String::from_utf8_lossy(&qemu.stderr);
    assert!(error.contains("requires vmlab-hv"), "{error}");
    assert!(!error.contains(&"FAKE".repeat(16)));
}
