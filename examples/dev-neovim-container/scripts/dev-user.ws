// dev01, step one: the toolchain and the account, both as the machine.
//
// This runs as the **agent identity** — root inside the micro-VM — and
// should: installing packages and creating an account are done *to* the
// machine. Its sibling, editor-bits.ws, is the other case, and getting the
// two the wrong way round is the mistake PRD §19.8 exists to head off.

use vmlab

fn sh(m: Machine, script: string, timeout: int) -> Result[string, string] {
    let r = m.exec_timeout("/bin/sh", ["-c", script], timeout)?
    if r.exit_code != 0 {
        return Err(fmt("`{}` exited {}: {}", script, r.exit_code, r.stderr))
    }
    Ok(r.stdout)
}

// The editor, and the two things a plugin clone needs. The segment's egress
// is what makes this reachable; a lab that wants none of it bakes the same
// packages into an image instead — the other half of the durability rule.
fn install(lab: Lab, dev01: Machine) -> Result[unit, string] {
    lab.log("installing neovim, git and a shell…")
    sh(dev01, "apk add --no-cache neovim git ca-certificates shadow", 900)?
    lab.log("toolchain installed")
    Ok(())
}

// The account the lab file declares. It is created here, with exactly the
// name the declaration names — `m.logins()` reads the declaration rather
// than the account existing in two places that drift (§19.2).
fn create_accounts(lab: Lab, dev01: Machine) -> Result[unit, string] {
    for login in dev01.logins() {
        // `adduser -D` on busybox: no password, which is the container
        // identity floor working as intended — root becomes the account
        // without one, so a Linux `login {}` need not declare a secret.
        sh(dev01, "id -u " + login.user + " >/dev/null 2>&1 || adduser -D -s /bin/sh " + login.user, 120)?
        lab.log("account " + login.user + " exists")

        match login.password {
            Some(secret) => {
                sh(dev01, "echo '" + login.user + ":" + secret + "' | chpasswd", 120)?
                lab.log("set " + login.user + "'s declared password")
            }
            None => lab.log("no secret declared for " + login.user + " — none is needed here"),
        }

        if login.default {
            lab.log(login.label + " is the identity every surface attaches as")
        }
    }
    Ok(())
}

fn setup(lab: Lab) -> Result[unit, string] {
    let dev01 = lab.container("dev01")?
    dev01.wait_ready(600)?
    install(lab, dev01)?
    create_accounts(lab, dev01)
}

fn main(lab: Lab) {
    setup(lab).expect("dev01 toolchain setup failed")
}
