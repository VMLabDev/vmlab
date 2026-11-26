//! Guest networking.
//!
//! DHCP decision: rather than a hand-rolled AF_PACKET client, cinit shells
//! out to busybox `udhcpc` with a small hook script shipped in the initramfs
//! (/etc/udhcpc/default.script). udhcpc already handles the awkward parts —
//! raw-socket DISCOVER/OFFER before the interface has an address, the
//! broadcast-flag dance, retries — in a few KB we ship anyway for the shell
//! and mkfs, so a native Rust client would be a lot of protocol code for no
//! robustness gain. The hook applies the lease (busybox ifconfig/route) and
//! records ip + dns under /run/vmlab-net/<ifname>.{ip,dns}, which cinit reads
//! back to write resolv.conf and emit `net_up`.

use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::util::{Ctx, Result};

const BUSYBOX: &str = "/bin/busybox";
const UDHCPC_SCRIPT: &str = "/etc/udhcpc/default.script";
const STATE_DIR: &str = "/run/vmlab-net";

fn busybox(args: &[&str]) -> Result<()> {
    let st = Command::new(BUSYBOX)
        .args(args)
        .status()
        .ctx(&format!("run busybox {}", args.join(" ")))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("busybox {} failed ({st})", args.join(" ")))
    }
}

/// Bring up loopback (always, even with 0 NICs).
pub fn loopback_up() -> Result<()> {
    busybox(&["ip", "link", "set", "lo", "up"])
}

fn udhcpc_argv<'a>(ifname: &'a str, pidfile: &'a str) -> [&'a str; 13] {
    [
        "-i",
        ifname,
        "-n",
        "-t",
        "6",
        "-T",
        "3",
        "-s",
        UDHCPC_SCRIPT,
        "-p",
        pidfile,
        "-O",
        "staticroutes",
    ]
}

/// Bring up `eth<n>` and start a DHCP client. Returns the initial IP, or None
/// when no lease was obtained (logged, non-fatal: a NIC-less segment or a
/// static-only fabric should not stop the container).
pub fn dhcp_up(n: u32) -> Result<Option<String>> {
    let ifname = format!("eth{n}");
    fs::create_dir_all(STATE_DIR).ctx("mkdir /run/vmlab-net")?;
    busybox(&["ip", "link", "set", &ifname, "up"])?;
    // BusyBox backgrounds itself after the bound hook returns. Waiting for
    // its launcher gives us the hook's state while the client stays to renew.
    // -n makes the launcher exit if the initial acquisition fails.
    let pidfile = format!("{STATE_DIR}/{ifname}.pid");
    let st = Command::new(BUSYBOX)
        .arg0("udhcpc")
        .args(udhcpc_argv(&ifname, &pidfile))
        .status()
        .ctx("run udhcpc")?;
    if !st.success() {
        eprintln!("vmlab-cinit: warning: no DHCP lease on {ifname} ({st})");
        return Ok(None);
    }
    let ip = fs::read_to_string(format!("{STATE_DIR}/{ifname}.ip"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(ip)
}

/// Collect DHCP-provided DNS servers (all interfaces) and write the container
/// rootfs resolv.conf. No servers → no file, matching "no DNS configured".
/// The caller treats an error as a warning.
pub fn write_resolv_conf(nics: u32) -> Result<()> {
    let mut servers: Vec<String> = Vec::new();
    for n in 0..nics {
        let path = format!("{STATE_DIR}/eth{n}.dns");
        if let Ok(content) = fs::read_to_string(&path) {
            for s in content.split_whitespace() {
                if !servers.iter().any(|have| have == s) {
                    servers.push(s.to_string());
                }
            }
        }
    }
    if servers.is_empty() {
        return Ok(());
    }
    let mut out = String::new();
    for s in &servers {
        out.push_str(&format!("nameserver {s}\n"));
    }
    // The image's file, resolved inside the rootfs (F56): an absolute
    // `resolv.conf` symlink must not aim this write at the micro-VM.
    crate::util::write_in_root(crate::mounts::ROOTFS, "/etc/resolv.conf", &out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dhcp_client_stays_to_renew() {
        let args = udhcpc_argv("eth0", "/run/vmlab-net/eth0.pid");
        assert!(!args.contains(&"-q"));
        assert!(!args.contains(&"-f"));
        assert!(!args.contains(&"-b"));
        assert!(args.contains(&"-n"));
    }

    #[test]
    fn dhcp_hook_installs_classless_routes() {
        let state = tempfile::tempdir().unwrap();
        let build = include_str!("../../build-asset.sh");
        let (_, hook) = build.split_once("default.script\" <<'EOF'\n").unwrap();
        let (hook, _) = hook.split_once("\nEOF\n").unwrap();
        let hook = hook
            .replace("BB=/bin/busybox", "BB=busybox")
            .replace("STATE=/run/vmlab-net", "STATE=$TEST_STATE");
        let run = |event, staticroutes: Option<&str>| {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(format!("busybox() {{ printf '%s\\n' \"$*\"; }}\n{hook}"))
                .args(["udhcpc", event])
                .env_clear()
                .env("TEST_STATE", state.path())
                .env("interface", "eth0")
                .env("ip", "10.0.0.10")
                .env("router", "10.0.0.1");
            if let Some(routes) = staticroutes {
                command.env("staticroutes", routes);
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{:?}", output);
            String::from_utf8(output.stdout).unwrap()
        };
        let commands = run("bound", Some("10.99.0.0/24 10.0.0.5 0.0.0.0/0 10.0.0.2"));
        assert!(
            commands.contains("ip route replace 10.99.0.0/24 via 10.0.0.5 dev eth0"),
            "{commands}"
        );
        assert!(
            commands.contains("ip route replace 0.0.0.0/0 via 10.0.0.2 dev eth0"),
            "{commands}"
        );
        assert!(!commands.contains("10.0.0.1"), "{commands}");

        let commands = run("renew", None);
        assert!(
            commands.contains("ip route del 10.99.0.0/24 via 10.0.0.5 dev eth0"),
            "{commands}"
        );
        assert!(
            commands.contains("ip route del 0.0.0.0/0 via 10.0.0.2 dev eth0"),
            "{commands}"
        );
        assert!(
            commands.contains("ip route replace default via 10.0.0.1 dev eth0"),
            "{commands}"
        );

        let commands = run("deconfig", None);
        assert!(
            commands.contains("ip route del default via 10.0.0.1 dev eth0"),
            "{commands}"
        );
        assert_eq!(
            fs::read_to_string(state.path().join("eth0.ip")).unwrap(),
            ""
        );
    }
}
