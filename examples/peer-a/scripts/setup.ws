// Peering demo, side A: give a1 a deterministic address on the shared
// segment (DHCP leases can race when both instances' supervisors serve the
// bridged segment), then it is pingable from side B at 10.99.0.10.

use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let a1 = lab.vm("a1")?
    a1.wait_ready(600)?
    // Errors ignored: re-running `up` re-adds an address that already exists.
    let _res = a1.exec("/sbin/ip", ["addr", "add", "10.99.0.10/24", "dev", "eth0"])
    lab.log("a1 is up — 10.99.0.10 on the shared \"wan\" segment")
    lab.log("once side B is up too:  vmlab exec a1 -- ping -c 3 10.99.0.20")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("peer-a setup failed")
}
