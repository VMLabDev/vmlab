// Crash handler: grab a screenshot of whatever the guest showed when it
// died. Handler failures are logged, never fatal (PRD §8.2).

use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("crash handler fired for " + event.vm + " (" + event.name + ")")
    let Ok(vm) = lab.vm(event.vm) else { return }
    match vm.screenshot("") {
        Ok(path) => lab.log("saved crash screenshot: " + path),
        Err(e)   => lab.log("could not screenshot: " + e),
    }
}
