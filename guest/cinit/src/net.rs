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

/// Bring up `eth<n>` and run one DHCP round. Returns the leased IP, or None
/// when no lease was obtained (logged, non-fatal: a NIC-less segment or a
/// static-only fabric should not stop the container).
pub fn dhcp_up(n: u32) -> Result<Option<String>> {
    let ifname = format!("eth{n}");
    fs::create_dir_all(STATE_DIR).ctx("mkdir /run/vmlab-net")?;
    busybox(&["ip", "link", "set", &ifname, "up"])?;
    // -f foreground, -q quit once bound, -n give up after the retries below
    // instead of looping forever (6 tries x 3s).
    let st = Command::new(BUSYBOX)
        .args([
            "udhcpc",
            "-i",
            &ifname,
            "-f",
            "-q",
            "-n",
            "-t",
            "6",
            "-T",
            "3",
            "-s",
            UDHCPC_SCRIPT,
        ])
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
    fs::write("/rootfs/etc/resolv.conf", out).ctx("write /etc/resolv.conf")
}
