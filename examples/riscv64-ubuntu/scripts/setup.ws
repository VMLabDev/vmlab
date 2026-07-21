// Provision for the riscv64-ubuntu lab: wait for the (TCG-emulated) guest,
// then prove it is really riscv64 and reachable. `wait_ready` blocks until
// the vmlab guest agent answers, so a long timeout covers the slow TCG boot.

use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let ubu = lab.vm("ubu")?

    lab.log("waiting for the riscv64 guest agent (TCG boot is slow)...")
    ubu.wait_ready(1800)?
    lab.log("ubu is up at " + ubu.ip()?)

    let arch = ubu.exec("/usr/bin/uname", ["-m"])?
    lab.log("guest reports machine arch: " + arch.stdout.trim())

    let rel = ubu.exec("/usr/bin/cat", ["/etc/os-release"])?
    lab.log("os-release: " + rel.stdout.trim())

    lab.log("SSH in with:  ssh vmlab@localhost -p 12322   (password: vmlab)")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("riscv64-ubuntu setup failed")
}
