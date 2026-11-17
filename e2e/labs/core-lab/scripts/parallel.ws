// Used only for core's measured full up. Both independent provisions must
// begin before either can end. Serial provisioning fails within two minutes.
use vmlab

fn stamp(lab: Lab) -> Result[unit, string] {
    let me = lab.this_vm()?
    let name = me.name()
    lab.log("e2e-prov " + name + " begin")
    if name == "vm02" || name == "vm03" {
        let marker = "BARRIER_PATH"
        let arrived = me.exec("/bin/touch", [marker])?
        if arrived.exit_code != 0 {
            return Err("could not mark provision arrival: " + arrived.stderr)
        }
        let peer_name = if name == "vm02" { "vm03" } else { "vm02" }
        let peer = lab.vm(peer_name)?
        let attempts = 0
        while peer.exec("/bin/test", ["-f", marker])?.exit_code != 0 {
            if attempts >= 120 {
                return Err("parallel provision timed out waiting for " + peer_name)
            }
            vmlab::sleep_ms(1000)
            attempts = attempts + 1
        }
    }
    lab.log("e2e-prov " + name + " end")
    Ok(())
}

fn main(lab: Lab) {
    stamp(lab).expect("provision rendezvous failed")
}
