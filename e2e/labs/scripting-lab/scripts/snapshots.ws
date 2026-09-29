// snapshot / restore / snapshots / delete_snapshot, with a guest file that
// changes between the capture and the restore.
use vmlab

fn mark(vm: Machine, text: string) -> Result[unit, string] {
    let r = vm.exec("/bin/sh", ["-c", "echo " + text + " > /root/snap-mark; sync"])?
    Ok(())
}

fn run(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("vm01")?
    mark(vm, "captured")?
    vm.snapshot("e2e-script-snap")?
    mark(vm, "overwritten")?
    for n in vm.snapshots()? {
        lab.log("e2e-snap-listed " + n)
    }
    vm.restore("e2e-script-snap")?
    vm.wait_ready(300)?
    let r = vm.exec("/bin/cat", ["/root/snap-mark"])?
    lab.log("e2e-snap-restored " + r.stdout.trim())
    vm.delete_snapshot("e2e-script-snap")?
    let left = vm.snapshots()?
    lab.log("e2e-snap-left " + fmt("{}", left.len()))
    Ok(())
}

fn main(lab: Lab) {
    match run(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("snapshots failed: " + e)
        },
    }
}
