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
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok: lab"));
}
