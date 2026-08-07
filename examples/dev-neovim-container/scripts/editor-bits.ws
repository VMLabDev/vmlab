// dev01, step two — the same guarantee as the Windows twin, on a machine
// with none of its machinery.
//
// PRD §19.8's one stated guarantee:
//
//   A `provision {}` step can address the dev login's home directory
//   **before that user has ever logged on.**
//
// Neovim has no client/server split and no marketplace, so it looks like the
// easy case — and it lands on exactly the same blocker: **everything
// editor-shaped lives in a per-user home directory.** `~/.config/nvim` and
// `~/.local/share/nvim` belong to `dev`, and the agent is root.
//
// `dev01.as_login("dev")` is the fix, and it is the same one line the
// Windows example uses. Here it costs nothing at all: the agent is root, so
// §19.2's container floor makes becoming `dev` free — no PAM, no secret.
// Take it away and every path below is root-owned in the account's home, and
// nvim's first run fails on a directory it cannot write.
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

// One plugin, cloned the way Neovim plugins actually arrive. `pack/*/start`
// is the built-in package path, so nothing here needs a plugin manager to
// have been installed first.
fn clone_plugin(lab: Lab, dev: Machine, home: string, name: string, url: string) -> Result[unit, string] {
    let dest = home + "/.local/share/nvim/site/pack/vmlab/start/" + name
    sh(dev, "test -d " + dest + " || git clone --depth 1 " + url + " " + dest, 600)?
    lab.log("plugin " + name + " is at " + dest)
    Ok(())
}

fn place(lab: Lab, dev01: Machine) -> Result[unit, string] {
    dev01.wait_ready(600)?

    // The rung that makes the rest true. It fails loudly on a login the lab
    // file does not declare, rather than falling back to the agent identity
    // — a silent fallback here is exactly the bug (§19.2).
    let dev = dev01.as_login("dev")?

    // Ask the guest, in that session, where "home" is. On a container this
    // is the answer §19.2's floor gives: a real login, `su -l` where the
    // guest has PAM and a `setuid` session where it does not.
    let home = sh(dev, "echo $HOME", 60)?
    let who = sh(dev, "id -un", 60)?
    lab.log(who + "'s home is " + home)
    if who != "dev" {
        return Err("the editor bits are landing as " + who + ", not as the developer — the `as_login` rung is missing")
    }

    sh(dev, "mkdir -p " + home + "/.config/nvim " + home + "/.local/share/nvim/site/pack/vmlab/start", 60)?

    // Config from the repo: `copy_to` under the same logon, so the file is
    // the developer's and not root's.
    dev.copy_to("config/init.lua", home + "/.config/nvim/init.lua")?
    lab.log("placed " + home + "/.config/nvim/init.lua")

    // Plugins as `git clone`s, over the segment's egress.
    clone_plugin(lab, dev, home, "vim-sensible", "https://github.com/tpope/vim-sensible.git")?

    // Everything above is under the guest home, outside the workspace — so
    // it survives reboot, `down`/`up`, and restore to a snapshot taken after
    // this ran. It dies on a per-machine `destroy` + `up`, and comes back
    // because it is *declared*: this script runs again on the fresh clone.
    lab.log("editor bits placed; they re-apply on every `destroy` + `up`")
    Ok(())
}

fn main(lab: Lab) {
    let Ok(dev01) = lab.container("dev01") else {
        lab.log("dev01 is not defined")
        return
    }
    place(lab, dev01).expect("placing the editor bits failed")
}
