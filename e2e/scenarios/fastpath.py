"""The kernel fast-path tiers (afxdp, sockmap) a VM NIC can ride. Kept apart
from `unsupported` because it proves itself from QEMU's tap netdevs, which
vmlab-hv does not take, so this scenario's VMs stay on QEMU on a vmlab run.
"""

import pathlib
import subprocess


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
            h.vmlab("daemon", "stop", check=False)
            h.vmlab("daemon", "start")
        if ok:
            h.ok("net.fastpath.kernel", True,
                 "auto selected afxdp: both VM NICs were `-netdev tap` with xdp_switch attached and fp01 pinged fp02; "
                 "VMLAB_FASTPATH=sockmap selected sockmap (stream NICs) and the ping crossed it too")


def run(h):
    fastpath(h)
