// Build provision for the windows-server-2025 template (PRD §6.1, §10.4).
// Two human moments to automate: the ISO's "Press any key to boot from CD
// or DVD" prompt right after power-on, and knowing when the unattended
// install is done — autounattend.xml installs the vmlab guest agent as its
// last first-logon command, so "agent responding" means finished.

use vmlab

fn boot_from_dvd(lab: Lab, vm: Vm) -> Result[unit, string] {
    for attempt in 0..4 {
        // Spam enter through the prompt's window.
        for i in 0..30 {
            let k = vm.send_keys("enter")   // bind unused Result
            vmlab::sleep_ms(1000)
        }
        // If we missed it, OVMF falls through to its shell; reset and retry.
        let screen = vm.ocr()?
        if screen.contains("Shell>") || screen.contains("UEFI Interactive Shell") {
            lab.log(fmt("missed the boot prompt (attempt {}), resetting", attempt))
            vm.restart()?
            vmlab::sleep_ms(3000)
        } else {
            return Ok(())
        }
    }
    Err("never got past the press-any-key prompt")
}

fn install(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("build")?
    boot_from_dvd(lab, vm)?

    lab.log("Windows Setup running; unattended install takes 20-40 minutes...")
    vm.wait_ready(5400)?

    match vm.exec("cmd.exe", ["/c", "ver"]) {
        Ok(r)  => lab.log("installed: " + r.stdout.trim()),
        Err(e) => lab.log("version check failed (agent is up though): " + e),
    }
    Ok(())
}

fn main(lab: Lab) {
    install(lab).expect("windows-server-2025 build failed")
}
