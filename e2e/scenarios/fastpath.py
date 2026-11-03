"""Kernel TAP attachment and stream fallback on both hypervisors."""

import json
import os
import pathlib


def nic_attachments(hypervisor, runtime=None, proc=pathlib.Path("/proc")):
    """Report the configured backend and count actual open TAP descriptors."""
    if runtime is None:
        runtime = pathlib.Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/vmlab-{os.getuid()}")) / "vmlab" / "labs"
    result = []
    for machine in ("fp01", "fp02"):
        description = None
        if hypervisor == "vmlab":
            found = list(runtime.glob(f"*/vms/{machine}/machine.json"))
            assert len(found) == 1, (machine, found)
            description = found[0]
        matching = []
        for process in proc.iterdir():
            if not process.name.isdigit():
                continue
            try:
                argv = (process / "cmdline").read_bytes().decode(errors="surrogateescape").split("\0")
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                continue
            if description is not None:
                owns = any(arg == "--machine" and index + 1 < len(argv) and argv[index + 1] == str(description)
                           for index, arg in enumerate(argv))
            else:
                owns = f"vmlab:e2e-unsupported-fastpath/{machine}" in argv
            if owns:
                matching.append((process, argv))
        assert len(matching) == 1, (machine, matching)
        process, argv = matching[0]
        if description is not None:
            nics = json.loads(description.read_text())["nics"]
            assert len(nics) == 1, nics
            attachment = nics[0]["attachment"]
            kind = attachment["kind"]
            pairs = attachment.get("queue_pairs", 1)
        else:
            backends = [argv[index + 1] for index, arg in enumerate(argv) if arg == "-netdev"]
            assert len(backends) == 1, backends
            kind, pairs = backends[0].split(",", 1)[0], 1
        taps = 0
        for fd in (process / "fd").iterdir():
            try:
                if fd.readlink() == pathlib.Path("/dev/net/tun"):
                    taps += 1
            except (FileNotFoundError, PermissionError):
                continue
        result.append((kind, pairs, taps))
    return result


def fastpath(h):
    auto = h.vmlab("fastpath").out
    if "afxdp" not in auto.splitlines()[0]:
        h.skip("net.fastpath.kernel", f"the daemon did not select a kernel tier here: {auto.strip()!r}")
        return

    def ping(lab) -> bool:
        r = h.vmlab("exec", "fp01", "--", "ping", "-c", "3", "-W", "2", "10.89.0.12", cwd=lab, check=False)
        return r.code == 0 and " 0% packet loss" in r.out

    with h.lab("unsupported-fastpath") as lab:
        # afxdp: auto's pick. Each VM NIC is a tap carrying the segment's XDP
        # program, and two guests reach each other across it.
        def afxdp():
            h.vmlab("up", cwd=lab, timeout=600)
            h.wait_ready(lab, "fp01", "fp02")
            devs = nic_attachments(h.hypervisor)
            expected = "tap_raw" if h.hypervisor == "vmlab" else "tap"
            assert len(devs) == 2 and all(kind == expected and pairs > 0 and taps >= pairs for kind, pairs, taps in devs), devs
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
            devs = nic_attachments(h.hypervisor)
            assert len(devs) == 2 and all(kind in ("stream", "stream_vnet") and taps == 0 for kind, _, taps in devs), devs
            assert ping(lab), "fp01 could not ping fp02 over the sockmap tier"
            return True

        try:
            ok = h.check("net.fastpath.kernel", sockmap, "sockmap") and ok
        finally:
            h.vmlab("destroy", cwd=lab, timeout=300, check=False)
            h.vmlab("daemon", "stop", check=False)
            h.vmlab("daemon", "start")
        if ok:
            stream = "QEMU's raw streams used sockmap" if h.hypervisor == "qemu" else "vmlab-hv's vnet streams used the compatible userspace path with sockmap selected"
            h.ok("net.fastpath.kernel", True,
                 "auto selected afxdp: both VMs held TAP queue descriptors with xdp_switch attached and fp01 pinged fp02; "
                 f"{stream}, and the ping crossed that path too")


def run(h):
    fastpath(h)
