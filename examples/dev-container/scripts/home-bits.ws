// dev01, step two: place a dotfile in the dev login's home, as that login.
//
// PRD §19.8's one stated guarantee:
//
//   A `provision {}` step can address the dev login's home directory
//   **before that user has ever logged on.**
//
// Everything personal lives in a per-user home directory. `~/.profile`
// belongs to `dev`, and the agent is root, so a file the agent writes there
// is root-owned in someone else's home.
//
// `dev01.as_login("dev")` is the fix: a second handle onto the same machine
// whose every call lands as `dev`. On a container it costs nothing: the agent
// is root, so §19.2's container floor makes becoming `dev` free, with no PAM
// and no secret. Take it away and every path below is root-owned in the
// account's home.
//
// A `playbook {}` could not do this: no user parameter, no rung on §19.2's
// precedence ladder. **Anything that must land as the developer rather than
// as the machine belongs in `provision {}`.**

use vmlab

fn sh(m: Machine, script: string, timeout: int) -> Result[string, string] {
    let r = m.exec_timeout("/bin/sh", ["-c", script], timeout)?
    if r.exit_code != 0 {
        return Err(fmt("`{}` exited {}: {}", script, r.exit_code, r.stderr))
    }
    Ok(r.stdout.trim())
}

fn place(lab: Lab, dev01: Machine) -> Result[unit, string] {
    dev01.wait_ready(600)?

    // The rung that makes the rest true. It fails loudly on a login the lab
    // file does not declare, rather than falling back to the agent identity;
    // a silent fallback here is exactly the bug (§19.2).
    let dev = dev01.as_login("dev")?

    // Ask the guest, in that session, where "home" is. On a container this
    // is the answer §19.2's floor gives: a real login, `su -l` where the
    // guest has PAM and a `setuid` session where it does not.
    let home = sh(dev, "echo $HOME", 60)?
    let who = sh(dev, "id -un", 60)?
    lab.log(who + "'s home is " + home)
    if who != "dev" {
        return Err("the dotfile is landing as " + who + ", not as the developer; the `as_login` rung is missing")
    }

    // The dotfile beside this script: a relative `copy_to` path resolves
    // against the script's own directory. It runs under the same logon, so
    // the file is the developer's and not root's.
    dev.copy_to("profile", home + "/.profile")?
    lab.log("placed " + home + "/.profile")

    // The file is under the guest home, outside the workspace, so it
    // survives reboot, `down`/`up`, and restore to a snapshot taken after
    // this ran. It dies on a per-machine `destroy` + `up`, and comes back
    // because it is *declared*: this script runs again on the fresh machine.
    Ok(())
}

fn main(lab: Lab) {
    let Ok(dev01) = lab.container("dev01") else {
        lab.log("dev01 is not defined")
        return
    }
    place(lab, dev01).expect("placing the dotfile failed")
}
