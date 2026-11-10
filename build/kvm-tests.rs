// libtest has no stable runtime skip. Ignore KVM tests at build time instead
// of letting their missing-device guards return a false success.
pub fn configure() {
    println!("cargo:rustc-check-cfg=cfg(vmlab_ci_no_kvm)");
    println!("cargo:rerun-if-env-changed=VMLAB_REQUIRE_KVM");
    println!("cargo:rerun-if-changed=/dev/kvm");
    let available = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    if !available {
        assert!(
            std::env::var("VMLAB_REQUIRE_KVM").as_deref() != Ok("1"),
            "VMLAB_REQUIRE_KVM=1 but /dev/kvm is absent or not accessible read/write"
        );
        println!("cargo:rustc-cfg=vmlab_ci_no_kvm");
        println!("cargo:warning=KVM tests ignored: /dev/kvm is absent or not accessible read/write");
    }
}
