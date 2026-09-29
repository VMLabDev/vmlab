// Create the guest accounts the lab's logins name. Provisions run as the
// agent identity, which is root here.

use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("g01").expect("no g01")
    vm.wait_ready(300).expect("agent never answered")
    let r = vm.exec("/bin/sh", ["-c", "for u in dev ops temp; do id $u >/dev/null 2>&1 || adduser -D $u; done"]).expect("adduser failed")
    lab.log(fmt("accounts created, exit {}", r.exit_code))
}
