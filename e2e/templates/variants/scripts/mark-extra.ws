// Leave the variant's marker on its disk.

use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("build").expect("no build vm")
    vm.wait_ready(300).expect("agent never answered")
    let r = vm.exec("/bin/sh", ["-c", "echo extra > /etc/e2e-variant && sync"]).expect("mark failed")
    lab.log(fmt("marked extra, exit {}", r.exit_code))
}
