// `vmlab script`: reach the guest and stream a line back.
use vmlab

fn run(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("vm01")?
    let r = vm.exec("/bin/sh", ["-c", "echo guest-$((6*7)) from $(hostname)"])?
    lab.log("e2e-script-out " + r.stdout.trim())
    Ok(())
}

fn main(lab: Lab) {
    match run(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("hello failed: " + e)
        },
    }
}
