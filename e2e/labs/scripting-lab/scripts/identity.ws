// as_login and as_account: a second handle whose exec and copy_to land as
// that user.
use vmlab

fn run(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("vm01")?
    let dev = vm.as_login("dev")?
    let r = dev.exec("/bin/sh", ["-c", "echo via-login > $HOME/from-login; id -un"])?
    lab.log("e2e-login-whoami " + r.stdout.trim())
    dev.copy_to("payload.txt", "/home/dev/login-payload.txt")?
    let audit = vm.as_account("audit", "unused-on-linux")?
    let a = audit.exec("/bin/sh", ["-c", "echo via-account > $HOME/from-account; id -un"])?
    lab.log("e2e-account-whoami " + a.stdout.trim())
    audit.copy_to("payload.txt", "/home/audit/account-payload.txt")?
    Ok(())
}

fn main(lab: Lab) {
    match run(lab) {
        Ok(u) => u,
        Err(e) => {
            let failed: Result[unit, string] = Err(e)
            failed.expect("identity failed: " + e)
        },
    }
}
