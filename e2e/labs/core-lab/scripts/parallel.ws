// Used only for core's measured full up. Both independent provisions must
// begin before either can end. Serial provisioning fails within two minutes.
use vmlab
use fs

fn stamp(lab: Lab) -> Result[unit, string] {
    let me = lab.this_vm()?
    let name = me.name()
    lab.log("e2e-prov " + name + " begin")
    if name == "vm02" || name == "vm03" {
        let dir = "BARRIER_DIR"
        fs::write(dir + "/" + name, "arrived")?
        let peer = if name == "vm02" { "vm03" } else { "vm02" }
        let attempts = 0
        while !fs::exists(dir + "/" + peer) {
            if attempts >= 1200 {
                return Err("parallel provision timed out waiting for " + peer)
            }
            vmlab::sleep_ms(100)
            attempts = attempts + 1
        }
    }
    lab.log("e2e-prov " + name + " end")
    Ok(())
}

fn main(lab: Lab) {
    stamp(lab).expect("provision rendezvous failed")
}
