// dc01: stand up the PROBE forest, create PROBE\dev, publish \\dc01\team.
//
// Nothing here is dev-machine-specific. It exists so the dev machine has a
// real domain to be a member of — and a *domain* user's profile cannot exist
// at template build time, which is exactly why PRD §19.8's guarantee is
// about `provision {}` rather than about baking a template.
//
// The PowerShell lives in .ps1 files beside this one, pushed and run rather
// than pasted into a string. vmlab moves the bytes and never interprets them
// (§19.8) — and a shipped file is what you can read, lint and edit.

use vmlab

// wscript has no top-level constants, so the two strings this script
// repeats are one-line functions.
fn domain() -> string { "probe.local" }
fn stage() -> string { "C:\\vmlab" }

// Push one script from this directory and run it, returning its stdout.
fn run_ps(m: Machine, file: string, timeout: int) -> Result[string, string] {
    let remote = stage() + "\\" + file
    m.copy_to("scripts/" + file, remote)?
    let r = m.exec_timeout(
        "powershell",
        ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", remote],
        timeout,
    )?
    if r.exit_code != 0 {
        return Err(fmt("{} exited {}: {}", file, r.exit_code, r.stderr))
    }
    Ok(r.stdout)
}

fn in_domain(dc: Machine) -> bool {
    match dc.exec("powershell", ["-NoProfile", "-Command", "(Get-CimInstance Win32_ComputerSystem).Domain"]) {
        Ok(r) => r.stdout.trim().to_lower() == domain(),
        Err(e) => false,
    }
}

fn promote(lab: Lab, dc: Machine) -> Result[unit, string] {
    let out = run_ps(dc, "domain.ps1", 2400)?
    lab.log(out.trim())
    if !out.contains("REBOOT-REQUIRED") {
        return Ok(())
    }

    lab.log("rebooting dc01 to finish the promotion…")
    dc.restart()?
    dc.wait_ready(1800)?

    // The directory service answers a little after the agent does.
    for i in 0..60 {
        if in_domain(dc) {
            lab.log("dc01 is " + domain())
            return Ok(())
        }
        vmlab::sleep_ms(10000)
    }
    Err("dc01 never reported itself as " + domain())
}

fn setup(lab: Lab) -> Result[unit, string] {
    let dc = lab.vm("dc01")?
    dc.wait_ready(1800)?
    promote(lab, dc)?
    let made = run_ps(dc, "directory.ps1", 600)?
    lab.log(made.trim())
    lab.log("dc01 ready: " + domain() + ", PROBE\\dev, \\\\dc01\\team")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("dc01 provisioning failed")
}
