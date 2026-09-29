// Provision: create the two accounts the identity script lands files as, and
// leave a marker the scenario reads back.
use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let vm = lab.this_vm()?
    vm.wait_ready(300)?
    let r = vm.exec("/bin/sh", ["-c", "id dev >/dev/null 2>&1 || adduser -D dev; id audit >/dev/null 2>&1 || adduser -D audit; echo provisioned-by-setup > /etc/e2e-provisioned"])?
    if r.exit_code != 0 {
        return Err("setup exited " + fmt("{}", r.exit_code) + ": " + r.stderr)
    }
    lab.log("e2e-provision done on " + vm.name())
    Ok(())
}

fn main(lab: Lab) {
    match setup(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("setup failed: " + e)
        },
    }
}
