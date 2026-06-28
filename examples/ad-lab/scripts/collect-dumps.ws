// Event handler (PRD §8.2): runs when any VM crashes. Handlers receive
// (event, lab); failures here are logged, never fatal.

use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("crash handler fired for " + event.vm + " (" + event.name + ")")
    let Ok(vm) = lab.vm(event.vm) else { return }
    // Best-effort: grab a final screenshot for post-mortem.
    match vm.screenshot("") {
        Ok(path) => lab.log("saved crash screenshot: " + path),
        Err(e) => lab.log("could not screenshot: " + e),
    }
}
