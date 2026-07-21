// Provision for the alpine-registry lab: wait for the guest (which boots from
// a template pulled on-demand from the OCI registry), then prove it is up and
// reachable. `wait_ready` blocks until the vmlab guest agent answers.

use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let alp = lab.vm("alp")?

    lab.log("waiting for the guest agent (template pulled from the registry on first up)...")
    alp.wait_ready(600)?
    lab.log("alp is up at " + alp.ip()?)

    let rel = alp.exec("/bin/cat", ["/etc/alpine-release"])?
    lab.log("alpine release: " + rel.stdout.trim())

    lab.log("SSH in with:  ssh vmlab@localhost -p 12222   (password: vmlab)")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("alpine-registry setup failed")
}
