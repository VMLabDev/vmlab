// Wait for the agent cloud-init installs, then for cloud-init itself, so the
// image is sealed only once first boot has finished.

use vmlab

fn provision(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("build")?
    vm.wait_ready(600)?
    vm.exec_timeout("cloud-init", ["status", "--wait"], 600)?
    Ok(())
}

fn main(lab: Lab) {
    match provision(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("e2e-alpine build failed: " + e)
        },
    }
}
