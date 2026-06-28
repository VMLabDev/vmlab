// Build provision for the freebsd-15 template (PRD §6.1, §10.4). The cloud
// image boots to a login; FreeBSD's nuageinit (from the CIDATA seed) creates
// the vmlab user and installs + enables the QEMU guest agent on first boot.
// There's no installer to drive — we wait for the agent to come up (which
// proves the install landed) and return. The build then powers the VM off
// gracefully and seals the disk.

use vmlab

fn build(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("build")?

    lab.log("waiting for nuageinit to provision (pkg-installs qemu-guest-agent)...")
    vm.wait_ready(1800)?
    lab.log("guest agent is up; provisioned")

    lab.log("the build will seal once the VM powers off")
    Ok(())
}

fn main(lab: Lab) {
    build(lab).expect("freebsd-15 build failed")
}
