// A script that records nothing itself and fails: the recording
// `vmlab script --record` started around it must still be finalised.
use vmlab

fn main(lab: Lab) {
    let vm = lab.vm("vm01").expect("vm01")
    vm.send_keys("ret").expect("keys")
    let _missing = lab.vm("no-such-machine").expect("this script fails on purpose")
}
