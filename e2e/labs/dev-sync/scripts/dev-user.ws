// Create the account the machine's `login {}` declares, as the agent (root).
use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let m = lab.this_vm()?
    m.wait_ready(600)?
    for login in m.logins() {
        let r = m.exec("/bin/sh", ["-c", "id -u " + login.user + " >/dev/null 2>&1 || adduser -D -s /bin/sh " + login.user])?
        if r.exit_code != 0 {
            return Err(fmt("adduser {} exited {}: {}", login.user, r.exit_code, r.stderr))
        }
        lab.log("account " + login.user + " exists")
    }
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("account setup failed")
}
