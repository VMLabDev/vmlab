// First-boot provision for sysprep-generalized Windows templates (PRD §6.1).
//
// These templates are sysprep-generalized, so every clone replays the
// specialize/OOBE pass on first boot. The QEMU guest agent survives generalize
// and can answer guest-ping WHILE specialize is still running — too early to
// treat the VM as ready. sysprep-unattend.xml writes the marker
// C:\Windows\Temp\vmlab-firstboot.done from a specialize-pass command (as
// SYSTEM, no logon needed) once that first boot genuinely finishes. vmlab runs
// this script before reporting the VM ready: wait for the marker, delete it,
// return. The VM operated on is the clone this provision gates — reached with
// lab.this_vm().

use vmlab

fn wait_first_boot(lab: Lab, vm: Vm) -> Result[unit, string] {
    let marker = "C:\\Windows\\Temp\\vmlab-firstboot.done"
    // Specialize + OOBE can take a while; poll for up to ~25 minutes (well
    // under vmlab's 30-minute host ceiling, so this clearer error wins). The
    // agent may blip across a specialize reboot, so a failed exec is not fatal.
    for i in 0..300 {
        match vm.exec("cmd.exe", ["/c", "if exist " + marker + " (exit 0) else (exit 1)"]) {
            Ok(r) => {
                if r.exit_code == 0 {
                    lab.log("first-boot: specialize complete; clearing marker")
                    let del = vm.exec("cmd.exe", ["/c", "del /f /q " + marker])
                    return Ok(())
                }
            }
            Err(e) => lab.log("first-boot: agent busy (" + e + "); still waiting"),
        }
        vmlab::sleep_ms(5000)
    }
    Err("first-boot marker never appeared after ~25 minutes")
}

fn main(lab: Lab) {
    let vm = lab.this_vm().expect("first-boot: no target VM")
    wait_first_boot(lab, vm).expect("windows first-boot failed")
}
