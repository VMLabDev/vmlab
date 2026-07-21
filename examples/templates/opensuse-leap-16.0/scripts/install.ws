// Build provision for the opensuse-leap template (PRD §6.1, §10.4). The
// cloud image boots straight to a login; cloud-init (from the CIDATA seed)
// creates the vmlab user and installs the vmlab guest agent on first boot.
// There's no installer to drive — we wait for the agent to come up (which
// proves the install landed), let cloud-init finish, and return. The build
// then powers the VM off gracefully and seals the disk.

use vmlab

fn build(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("build")?

    lab.log("waiting for cloud-init to provision (installs the vmlab guest agent)...")
    vm.wait_ready(1800)?
    lab.log("guest agent is up; waiting for cloud-init to finish")

    // Ensure every module ran (incl. the cloud-init self-disable) before seal.
    match vm.exec("cloud-init", ["status", "--wait"]) {
        Ok(r) => lab.log("cloud-init " + r.stdout),
        Err(e) => lab.log("cloud-init status unavailable, continuing: " + e),
    }

    lab.log("provisioned; the build will seal once the VM powers off")
    Ok(())
}

fn main(lab: Lab) {
    build(lab).expect("opensuse-leap build failed")
}
