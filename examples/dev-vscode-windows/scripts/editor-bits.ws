// dev01, step two — what this whole example exists to demonstrate.
//
// PRD §19.8's one stated guarantee:
//
//   A `provision {}` step can address the dev login's home directory
//   **before that user has ever logged on.**
//
// `dev01.as_login("dev")` is that guarantee spelled out. It resolves the
// `login "dev"` block the lab file declares, mints PROBE\dev's logon
// (LogonUser + LoadUserProfileW, §19.2) and hands back a second handle onto
// the same machine. Every call on that handle — `exec`, `copy_to`,
// `terminal` — lands inside the profile the mint just created.
//
// Take the `.as_login("dev")` away and this script still runs, still
// succeeds, and writes every byte into
// C:\Windows\system32\config\systemprofile. That silent failure is why §19.8
// bothers to state the guarantee: "provision runs as the machine, full stop"
// is the natural reading of §19.2's headline, and it is wrong here.
//
// A `playbook {}` could not do this at all — no user parameter, no rung on
// the precedence ladder. **Anything that must land as the developer rather
// than as the machine belongs in `provision {}`.**

use vmlab

// One inline PowerShell command as this handle's identity.
fn ps(m: Machine, command: string, timeout: int) -> Result[string, string] {
    let r = m.exec_timeout("powershell", ["-NoProfile", "-NonInteractive", "-Command", command], timeout)?
    if r.exit_code != 0 {
        return Err(fmt("`{}` exited {}: {}", command, r.exit_code, r.stderr))
    }
    Ok(r.stdout.trim())
}

// Push a script from this directory and run it as this handle's identity.
fn run_ps(m: Machine, file: string, timeout: int) -> Result[string, string] {
    let remote = "C:\\vmlab\\" + file
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

// The staged VSIX, if this checkout has one. §19.8 keeps the two halves
// apart on purpose: vmlab moves bytes it is told to move and never
// interprets them, so a `.vsix` is a file to place, not an extension to
// install. Installing it is `code --install-extension` in the attached
// terminal — see README.md, which records what that does over the facade.
// The payload is optional, so an absent one is not a failure — but the
// reason is always said, because "no payload" and "the push failed" are
// different facts and only one of them is fine.
fn stage_vsix(lab: Lab, dev: Machine, home: string) {
    match dev.copy_to("payload/extension.vsix", home + "\\vsix\\extension.vsix") {
        Ok(_) => lab.log("staged payload/extension.vsix in " + home + "\\vsix"),
        Err(e) => lab.log("payload/extension.vsix not staged: " + e + " (optional — see payload/README.md)"),
    }
}

fn place(lab: Lab, dev01: Machine) -> Result[unit, string] {
    dev01.wait_ready(1800)?

    // The rung that makes the rest true. It fails loudly on a login the lab
    // file does not declare, rather than falling back to the agent identity
    // — a silent fallback here is exactly the bug (§19.2).
    let dev = dev01.as_login("dev")?

    let made = run_ps(dev, "editor-bits.ps1", 900)?
    lab.log(made.trim())

    // Ask the guest where that actually landed, in the same session. This is
    // the assertion the example is built around: a domain profile that did
    // not exist when `vmlab up` started.
    let home = ps(dev, "$env:USERPROFILE", 300)?
    lab.log("PROBE\\dev's profile: " + home)
    if home.to_lower().contains("systemprofile") {
        return Err("the editor bits landed in the machine's profile, not the developer's — the `as_login` rung is missing")
    }

    // The server's per-user settings on the remote side. Copied under the
    // same logon, so the file is the developer's, not SYSTEM's.
    dev.copy_to("config/settings.json", home + "\\.vscode-server\\data\\Machine\\settings.json")?
    lab.log("placed .vscode-server\\data\\Machine\\settings.json")

    stage_vsix(lab, dev, home)

    // Every path above is under the guest home, outside the workspace — so
    // it survives reboot, `down`/`up`, and restore to a snapshot taken after
    // this ran. It dies on a per-machine `destroy` + `up`, and comes back
    // because it is *declared*: this script runs again on the fresh clone.
    lab.log("editor bits placed; they re-apply on every `destroy` + `up`")
    Ok(())
}

fn main(lab: Lab) {
    let Ok(dev01) = lab.vm("dev01") else {
        lab.log("dev01 is not defined")
        return
    }
    place(lab, dev01).expect("placing the editor bits failed")
}
