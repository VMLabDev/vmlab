// Provision for mixed-lab (PRD §10): wait for both guests, stand up nginx
// on the Ubuntu box (reachable from the host via the segment's 18080
// forward), prove the SMB share landed on the Windows side, then enable
// autologon and reboot winsrv once — the fresh console session logs in
// AFTER the daemon registered the share-credential logon hook, so the
// desktop opens with S: already working.

use vmlab

fn share_visible(lab: Lab, win: Vm) -> Result[unit, string] {
    // The daemon mounts shares as soon as the agent responds — S: is
    // normally there within seconds; the window is just safety margin.
    for i in 0..60 {
        match win.exec("cmd.exe", ["/c", "dir S:"]) {
            Ok(s) => {
                if s.exit_code == 0 {
                    lab.log("winsrv sees the share:\n" + s.stdout)
                    return Ok(())
                }
            }
            Err(e) => lab.log("share check failed: " + e),
        }
        if i % 6 == 5 {
            lab.log(fmt("still waiting for S: on winsrv ({}s)", (i + 1) * 5))
        }
        vmlab::sleep_ms(5000)
    }
    lab.log("S: never appeared on winsrv — check `vmlab logs`")
    Ok(())
}

fn autologon_enabled(win: Vm) -> bool {
    let winlogon = "HKLM\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon"
    match win.exec("reg", ["query", winlogon, "/v", "AutoAdminLogon"]) {
        Ok(r) => {
            for line in r.stdout.split("\n") {
                if line.contains("AutoAdminLogon") && line.trim().ends_with("1") {
                    return true
                }
            }
            false
        }
        Err(e) => false,
    }
}

fn setup(lab: Lab) -> Result[unit, string] {
    let win = lab.vm("winsrv")?
    let nix = lab.vm("nix01")?

    win.wait_ready(900)?
    nix.wait_ready(900)?
    lab.log("winsrv at " + win.ip()? + ", nix01 at " + nix.ip()?)

    let cmd = "apt-get update && apt-get install -y nginx && echo '<h1>mixed-lab: hello from nix01</h1>' > /var/www/html/index.html"
    let r = nix.exec_timeout("/bin/sh", ["-c", cmd], 600)?
    if r.exit_code != 0 {
        return Err("nginx install failed: " + r.stderr)
    }
    lab.log("nginx serving on nix01 — try: curl http://localhost:18080")

    share_visible(lab, win)?

    // One-time: autologon + reboot so the console session starts after the
    // logon hook exists. Skipped on later `up`s (autologon already set).
    if autologon_enabled(win) {
        lab.log("autologon already configured; no reboot needed")
        return Ok(())
    }
    lab.log("enabling autologon and rebooting winsrv (one-time)...")
    let winlogon = "HKLM\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Winlogon"
    let a = win.exec("reg", ["add", winlogon, "/v", "AutoAdminLogon", "/t", "REG_SZ", "/d", "1", "/f"])?
    let u = win.exec("reg", ["add", winlogon, "/v", "DefaultUserName", "/t", "REG_SZ", "/d", "Administrator", "/f"])?
    let p = win.exec("reg", ["add", winlogon, "/v", "DefaultPassword", "/t", "REG_SZ", "/d", "vmlab123!", "/f"])?
    win.restart()?
    win.wait_ready(900)?
    lab.log("winsrv rebooted; the desktop logs in with shares connected")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("mixed-lab setup failed")
}
