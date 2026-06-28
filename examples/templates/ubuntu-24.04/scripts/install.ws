// Build provision for the ubuntu-24.04 template (PRD §6.1, §10.4).
// Subiquity finds the autoinstall config on the CIDATA ISO but, without
// `autoinstall` on the kernel command line, asks for confirmation first —
// answer it, then wait for the installer to power the VM off
// (`shutdown: poweroff` in cloudinit/user-data). The sealed image carries
// qemu-guest-agent, so lab clones come up "ready".

use vmlab

fn install(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("build")?

    // Nudge GRUB past its menu timeout if it is on screen.
    match vm.wait_for_text("(?i)install ubuntu", 180) {
        Ok(_) => {
            vm.send_keys("enter")?
            lab.log("selected the installer GRUB entry")
        }
        Err(e) => lab.log("no GRUB menu seen, continuing: " + e),
    }

    // Subiquity: "Continue with autoinstall? (yes|no)".
    match vm.wait_for_text("(?i)continue with autoinstall", 900) {
        Ok(_) => {
            vm.type_text("yes\n")?
            lab.log("autoinstall confirmed")
        }
        Err(e) => lab.log("no confirmation prompt seen, assuming unattended boot: " + e),
    }

    lab.log("installing (takes several minutes)...")
    vm.wait_shutdown(3600)?
    lab.log("installer powered the VM off; ready to seal")
    Ok(())
}

fn main(lab: Lab) {
    install(lab).expect("ubuntu-24.04 build failed")
}
