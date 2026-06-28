// Provision for the alpine-arm64 lab: wait for the (TCG-emulated) guest,
// then prove it is really aarch64 and reachable. `wait_ready` blocks until
// the QEMU guest agent answers, so a long timeout covers the slow TCG boot.

use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let alp = lab.vm("alp")?

    lab.log("waiting for the aarch64 guest agent (TCG boot is slow)...")
    alp.wait_ready(1800)?
    lab.log("alp is up at " + alp.ip()?)

    let arch = alp.exec("/bin/uname", ["-m"])?
    lab.log("guest reports machine arch: " + arch.stdout.trim())

    let rel = alp.exec("/bin/cat", ["/etc/alpine-release"])?
    lab.log("alpine release: " + rel.stdout.trim())

    lab.log("SSH in with:  ssh vmlab@localhost -p 12222   (password: vmlab)")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("alpine-arm64 setup failed")
}
