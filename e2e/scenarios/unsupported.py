"""Features that need hardware, a guest family or a display this container
may not have. Each is exercised as far as the container allows: run for real
where it can be, and otherwise skipped with the reason, after the cheapest
check that vmlab handles it gracefully.
"""

import os
import pathlib
import shutil
import subprocess

from harness import WORK, ScenarioFailed

# A stand-in for a GUI VNC viewer: vmlab finds `vncviewer` on PATH and hands
# it a localhost display; this one dials it, reads the RFB banner and records
# both, which is everything a real viewer would do before drawing a window.
FAKE_VIEWER = """#!/usr/bin/env python3
import re, socket, sys
target = sys.argv[-1]
host, num = re.match(r"(.+):(\\d+)$", target).groups()
port = int(num) + 5900 if int(num) < 5900 else int(num)
banner = socket.create_connection((host, port), timeout=10).recv(12)
with open(sys.argv[0] + ".seen", "a") as f:
    f.write(f"{target} {banner.decode(errors='replace')}\\n")
"""


def scratch_lab(name: str, body: str) -> pathlib.Path:
    d = WORK / name
    shutil.rmtree(d, ignore_errors=True)
    d.mkdir(parents=True)
    (d / "vmlab.wcl").write_text("import <vmlab.wcl>\n\n" + body)
    return d


def fastpath(h):
    auto = h.vmlab("fastpath").out
    if "afxdp" not in auto.splitlines()[0]:
        h.skip("net.fastpath.kernel", f"the daemon did not select a kernel tier here: {auto.strip()!r}")
        return

    def ping(lab) -> bool:
        r = h.vmlab("exec", "fp01", "--", "ping", "-c", "3", "-W", "2", "10.89.0.12", cwd=lab, check=False)
        return r.code == 0 and " 0% packet loss" in r.out

    def netdevs() -> list[str]:
        out = []
        for pid in subprocess.run(["pgrep", "-f", "vmlab:e2e-unsupported-fastpath/"],
                                  capture_output=True, text=True).stdout.split():
            argv = pathlib.Path(f"/proc/{pid}/cmdline").read_text().split("\0")
            out += [argv[i + 1] for i, a in enumerate(argv) if a == "-netdev"]
        return out

    with h.lab("unsupported-fastpath") as lab:
        # afxdp: auto's pick. Each VM NIC is a tap carrying the segment's XDP
        # program, and two guests reach each other across it.
        def afxdp():
            h.vmlab("up", cwd=lab, timeout=600)
            h.wait_ready(lab, "fp01", "fp02")
            devs = netdevs()
            assert len(devs) == 2 and all(d.startswith("tap,") for d in devs), devs
            links = h.run(["ip", "-d", "link"]).out
            assert "xdp_switch" in links, "no tap carries the xdp_switch program"
            assert ping(lab), "fp01 could not ping fp02 over the afxdp tier"
            return True

        ok = h.check("net.fastpath.kernel", afxdp, "afxdp")

        # sockmap: never picked by auto, so forced for a fresh supervisor.
        def sockmap():
            h.vmlab("down", cwd=lab, timeout=300)
            h.vmlab("daemon", "stop")
            h.run(["vmlab", "daemon", "start"], env={"VMLAB_FASTPATH": "sockmap"})
            said = h.vmlab("fastpath").out
            assert said.startswith("network fast path: sockmap"), said
            h.vmlab("up", cwd=lab, timeout=600)
            h.wait_ready(lab, "fp01", "fp02")
            devs = netdevs()
            assert len(devs) == 2 and all(d.startswith("stream,") for d in devs), devs
            assert ping(lab), "fp01 could not ping fp02 over the sockmap tier"
            return True

        try:
            ok = h.check("net.fastpath.kernel", sockmap, "sockmap") and ok
        finally:
            h.vmlab("destroy", cwd=lab, timeout=300, check=False)
            h.wait_until(lambda: subprocess.run(["pgrep", "-f", "__labd"], capture_output=True).returncode != 0,
                         timeout=60, what="the lab daemon to exit")
            h.vmlab("daemon", "stop", check=False)
            h.vmlab("daemon", "start")
        if ok:
            h.ok("net.fastpath.kernel", True,
                 "auto selected afxdp: both VM NICs were `-netdev tap` with xdp_switch attached and fp01 pinged fp02; "
                 "VMLAB_FASTPATH=sockmap selected sockmap (stream NICs) and the ping crossed it too")


def gpu(h):
    good = scratch_lab("unsupported-gpu-validate", """lab "e2e-gpu-validate" {
  vm "a" {
    template = "scratch"
    arch = "x86_64"
    profile = "linux-modern"
    disk = 1GiB
    gpu { mode = "virgl" }
  }
  vm "b" {
    template = "scratch"
    arch = "x86_64"
    profile = "linux-modern"
    disk = 1GiB
    gpu { mode = "passthrough" }
  }
}
""")
    v = h.vmlab("validate", cwd=good, check=False)
    if v.code == 0 or "address" not in v.text:
        h.ok("vm.hw.gpu", False, f"validate accepted passthrough with no address: {v.text.strip()[-200:]!r}")
        return
    untested = ("passthrough (needs a VFIO-bound physical GPU) and vulkan are not exercised; validate rejects "
                "passthrough without `address`")
    nodes = sorted(pathlib.Path("/dev/dri").glob("renderD*"))
    if not nodes:
        h.skip("vm.hw.gpu", f"no /dev/dri render node in the container for virgl to render on; {untested}")
        return

    with h.lab("unsupported-gpu") as lab:
        up = h.vmlab("up", cwd=lab, timeout=300, check=False)
        if up.code != 0:
            # QEMU's own reason is in its log; `up` only says it exited.
            log = h.vmlab("logs", "gpu01", "-n", "50", cwd=lab, check=False).text
            said = [l.strip() for l in log.splitlines()
                    if l.strip() and not l.startswith("==>") and "qemu.log" not in l]
            h.skip("vm.hw.gpu",
                   f"virgl cannot start here although render nodes exist ({', '.join(n.name for n in nodes)}): "
                   f"`up` said {up.text.strip()[-120:]!r}, QEMU said {' | '.join(said[-2:])!r}; {untested}")
            return
        h.wait_ready(lab, "gpu01")

        def virgl():
            dmesg = h.vmlab("exec", "gpu01", "--user", "root", "--", "dmesg", cwd=lab).out
            line = next((l for l in dmesg.splitlines() if "[drm] features:" in l), "")
            assert "+virgl" in line, f"virtio-gpu features line: {line!r}"
            feats = h.vmlab("exec", "gpu01", "--user", "root", "--", "sh", "-c",
                            "mount -t debugfs none /sys/kernel/debug 2>/dev/null; "
                            "cat /sys/kernel/debug/dri/*/virtio-gpu-features", cwd=lab, check=False).out
            assert any(l.split() == ["virgl", ":", "yes"] for l in feats.splitlines()), feats
            drm = h.vmlab("exec", "gpu01", "--", "ls", "/sys/class/drm", cwd=lab).out
            assert "renderD128" in drm, drm
            return line.split("]", 1)[-1].strip()

        h.check("vm.hw.gpu", virgl,
                "virgl: the guest's virtio-gpu negotiated `[drm] features: +virgl`, debugfs virtio-gpu-features "
                f"says `virgl : yes`, and it exposes a render node; {untested}")


def guest(h):
    """One Linux VM: an SMB1 share (served for real), `eventlog` refused by
    name, and the viewer launch through `vmlab console` and `gui = true`."""
    fake = WORK / "fake-viewer"
    shutil.rmtree(fake, ignore_errors=True)
    fake.mkdir()
    (fake / "vncviewer").write_text(FAKE_VIEWER)
    (fake / "vncviewer").chmod(0o755)
    seen = fake / "vncviewer.seen"
    path = {"PATH": f"{fake}:{os.environ['PATH']}"}

    with h.lab("unsupported-guest") as lab:
        up = h.run(["vmlab", "up"], cwd=lab, env=path, timeout=600)
        h.wait_ready(lab, "lin01")

        # console.viewer: `gui = true` opened the viewer on `up`, and
        # `vmlab console` opens another.
        def viewer():
            assert "opened a viewer for lin01" in up.text, up.text
            h.wait_until(lambda: seen.exists() and "RFB" in seen.read_text(), timeout=20, what="the gui viewer")
            first = seen.read_text().count("RFB")
            c = h.run(["vmlab", "console", "lin01"], cwd=lab, env=path, timeout=60)
            assert "in a viewer" in c.out, c.out
            h.wait_until(lambda: seen.read_text().count("RFB") > first, timeout=20, what="the console viewer")
            return True

        h.check("console.viewer", viewer,
                "a stand-in `vncviewer` on PATH was launched by `up` (gui = true) and by `vmlab console`, "
                "and read an RFB banner from the display it was given (no real GUI display in the container)")

        # share.smb1: the SMB1 dialect, mounted by vmlab with vers=1.0.
        def smb1():
            h.wait_until(
                lambda: "served over smb1" in h.vmlab("exec", "lin01", "--", "cat", "/mnt/legacy/hello.txt",
                                                      cwd=lab, check=False).out,
                timeout=90, what="the smb1 share to mount")
            line = h.vmlab("exec", "lin01", "--", "sh", "-c", "mount | grep /mnt/legacy", cwd=lab).out
            assert "type cifs" in line and "vers=1.0" in line, line
            conf = (lab / ".vmlab" / "smb" / "smb.conf").read_text()
            assert "server min protocol = NT1" in conf, "smbd not configured for NT1"
            return True

        h.check("share.smb1", smb1,
                "vmlab mounted the smb1 share in a Linux guest with vers=1.0 (smbd min protocol NT1) and it read "
                "the host file, vmlab creating the mount point itself; nat=true on the segment "
                "(gateway:445 unreachable without it); an XP-era client itself was not exercised")

        ev = h.vmlab("eventlog", "lin01", cwd=lab, check=False, timeout=30)
        caps = h.vmlab("machine", "capabilities", "lin01", cwd=lab, check=False).out
        if ev.code != 0 and "Windows-only" in ev.text and "eventlog" not in caps:
            h.skip("agent.eventlog",
                   "needs a Windows guest (building one is a full OS install). Checked: `vmlab eventlog lin01` "
                   f"on the Linux guest exits {ev.code} with {ev.text.strip()!r}, and its capabilities omit eventlog")
        else:
            h.ok("agent.eventlog", False, f"eventlog on a Linux guest: exit {ev.code} {ev.text.strip()!r}")

    # login.elevated: a Linux declaration of it must be refused by name.
    bad = scratch_lab("unsupported-elevated", """lab "e2e-elevated" {
  vm "lin" {
    template = "x86_64/e2e-alpine"
    login "admin" { user = "root" elevated = true }
  }
}
""")
    v = h.vmlab("validate", cwd=bad, check=False)
    if v.code != 0 and "elevated" in v.text:
        h.skip("login.elevated",
               "`elevated` is a Windows logon property (a linked, elevated token) and needs a Windows guest, "
               f"which is a full OS install. Checked: validate refuses it on a Linux login: {next((l.strip() for l in v.text.splitlines() if 'declares' in l), '')[:200]!r}")
    else:
        h.ok("login.elevated", False, f"validate accepted `elevated` on a Linux login: {v.text.strip()!r}")


def run(h):
    # The parts are independent: one aborting leaves its own features missing
    # (which fails the run) without costing the others theirs.
    failed = []
    for part in (fastpath, gpu, guest):
        try:
            part(h)
        except ScenarioFailed as e:
            h.log.write(f"[unsupported] {part.__name__} aborted: {e}\n")
            failed.append(f"{part.__name__}: {e}")
    if failed:
        raise ScenarioFailed("; ".join(failed))
