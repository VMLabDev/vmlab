// Peering demo, side B: deterministic address (see side A's setup.ws).

use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let b1 = lab.vm("b1")?
    b1.wait_ready(600)?
    // Errors ignored: re-running `up` re-adds an address that already exists.
    let _res = b1.exec("/sbin/ip", ["addr", "add", "10.99.0.20/24", "dev", "eth0"])
    lab.log("b1 is up — 10.99.0.20 on the shared \"wan\" segment")
    lab.log("once side A is up too:  vmlab exec b1 -- ping -c 3 10.99.0.10")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("peer-b setup failed")
}
