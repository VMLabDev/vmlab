// dev01, step one: become a member of PROBE.
//
// This one runs as the **agent identity** and should: joining is done to the
// machine, not as a developer. Its sibling, editor-bits.ws, is the other
// case — and getting the two the wrong way round is the mistake PRD §19.8
// exists to head off.

use vmlab

fn stage() -> string { "C:\\vmlab" }

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

fn joined(dev: Machine) -> bool {
    match dev.exec("powershell", ["-NoProfile", "-Command", "(Get-CimInstance Win32_ComputerSystem).PartOfDomain"]) {
        Ok(r) => r.stdout.trim().to_lower() == "true",
        Err(e) => false,
    }
}

fn join(lab: Lab, dev: Machine) -> Result[unit, string] {
    let out = run_ps(dev, "join-domain.ps1", 900)?
    lab.log(out.trim())
    if !out.contains("REBOOT-REQUIRED") {
        return Ok(())
    }

    lab.log("rebooting dev01 to finish the domain join…")
    dev.restart()?
    dev.wait_ready(1800)?
    if joined(dev) {
        lab.log("dev01 is a member of probe.local")
        return Ok(())
    }
    Err("dev01 came back but is not a domain member")
}

fn setup(lab: Lab) -> Result[unit, string] {
    let dev = lab.vm("dev01")?
    dev.wait_ready(1800)?
    join(lab, dev)
}

fn main(lab: Lab) {
    setup(lab).expect("dev01 domain join failed")
}
