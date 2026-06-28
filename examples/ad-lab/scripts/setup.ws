// Provision script for the AD lab (PRD §10). Runs during `vmlab up` after
// dc01 is ready. Demonstrates the lab/VM API: waits for readiness, runs a
// guest command, and screen-drives a step that has no automation hook.

use vmlab

fn main(lab: Lab) {
    lab.log("setting up " + lab.name())

    let Ok(dc) = lab.vm("dc01") else {
        lab.log("dc01 is not defined")
        return
    }

    match dc.wait_ready(600) {
        Ok(_) => lab.log("dc01 agent is responding"),
        Err(e) => {
            lab.log("dc01 never became ready: " + e)
            return
        }
    }

    // Report the guest's view of its network.
    match dc.exec("ipconfig", ["/all"]) {
        Ok(r) => lab.log(r.stdout),
        Err(e) => lab.log("ipconfig failed: " + e),
    }

    // Screen-driven step: wait for a UI element, then click it (the kind of
    // guest these APIs exist for). Reference images live beside vmlab.wcl
    // under images/.
    match dc.wait_for_image("images/promote-button.png", 120) {
        Ok(m) => {
            let mv = dc.mouse_move(m.cx, m.cy)
            let cl = dc.mouse_click("left")
            lab.log("clicked the promote button")
        }
        Err(e) => lab.log("promote button not found (skipping): " + e),
    }
}
