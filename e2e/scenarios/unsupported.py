"""Features that need hardware, a guest family or a display this container
may not have. Each is exercised as far as the container allows: run for real
where it can be, and otherwise skipped with the reason, after the cheapest
check that vmlab handles it gracefully.
"""

import json
import os
import pathlib
import shutil
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
        h.ok("vm.hw.gpu", False, f"validate accepted passthrough with no host GPU: {v.text.strip()[-200:]!r}")
        return
    untested = ("passthrough (needs a VFIO-bound physical GPU) and vulkan are not exercised; validate rejects "
                "passthrough with no host GPU to pick")
    nodes = sorted(pathlib.Path("/dev/dri").glob("renderD*"))
    if not nodes:
        h.skip("vm.hw.gpu", f"no /dev/dri render node in the container for virgl to render on; {untested}")
        return
    # The image's EGL is Mesa's, which cannot drive an NVIDIA node: declare
    # the first node a Mesa driver owns as the host's default GPU, so both
    # hypervisors render there rather than on whichever node sorts first.
    mesa = next((n for n in nodes if render_driver(n) in MESA_DRIVERS), None)
    restore = declare_host_gpu(h, mesa) if mesa else None
    try:
        gpu_boot(h, nodes, mesa, untested)
    finally:
        if restore:
            restore()


# Kernel drivers whose render nodes the image's Mesa EGL can open.
MESA_DRIVERS = ("amdgpu", "radeon", "i915", "xe", "virtio_gpu", "nouveau")


def render_driver(node: pathlib.Path) -> str:
    link = pathlib.Path("/sys/class/drm") / node.name / "device" / "driver"
    return link.resolve().name if link.exists() else ""


def declare_host_gpu(h, node: pathlib.Path):
    """Add `gpu "e2e" { address = <node's PCI address>, default }` to the
    host config, returning what puts the file back."""
    address = (pathlib.Path("/sys/class/drm") / node.name / "device").resolve().name
    path = pathlib.Path(os.environ.get("XDG_CONFIG_HOME", pathlib.Path.home() / ".config")) / "vmlab" / "config.wcl"
    before = path.read_text() if path.exists() else None
    path.parent.mkdir(parents=True, exist_ok=True)
    text = before or "import <vmlab-host.wcl>\n"
    path.write_text(text + f'\ngpu "e2e" {{ address = "{address}"  default = true }}\n')
    h.log.write(f"[unsupported] host GPU e2e = {address} ({node.name}, {render_driver(node)})\n")

    def restore():
        if before is None:
            path.unlink(missing_ok=True)
        else:
            path.write_text(before)
    return restore


def gpu_boot(h, nodes, mesa, untested):
    on = "vmlab-hv" if h.vms_here and h.hypervisor == "vmlab" else "QEMU"
    with h.lab("unsupported-gpu") as lab:
        up = h.vmlab("up", cwd=lab, timeout=300, check=False)
        if up.code != 0:
            # QEMU's own reason is in its log; `up` only says it exited.
            log = h.vmlab("logs", "gpu01", "-n", "50", cwd=lab, check=False).text
            said = [l.strip() for l in log.splitlines()
                    if l.strip() and not l.startswith("==>") and ".log" not in l]
            where = mesa.name if mesa else "no Mesa-driven node, so the first one"
            h.skip("vm.hw.gpu",
                   f"virgl cannot start here although render nodes exist ({', '.join(n.name for n in nodes)}; "
                   f"rendering on {where}): `up` said {up.text.strip()[-120:]!r}, {on} said "
                   f"{' | '.join(said[-2:])!r}; {untested}")
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

        node = mesa.name if mesa else "the first render node"
        h.check("vm.hw.gpu", virgl,
                f"virgl on {on}, rendering on {node}: the guest's virtio-gpu negotiated `[drm] features: +virgl`, "
                f"debugfs virtio-gpu-features says `virgl : yes`, and it exposes a render node; {untested}")


def audio(h):
    """`audio = true` (hv-audio D7): on vmlab-hv an HDA function at 00:1b.0
    — Intel's 8086:293e, which the guest sees on its PCI bus whether or not
    its kernel carries a driver — playing into the node vmlab names; on QEMU
    a validation error naming vmlab-hv. The e2e container has no PipeWire,
    so vmlab-hv plays into its null backend; the host stream is a live check."""
    lab = scratch_lab("unsupported-audio", """lab "e2e-audio" {
  vm "snd01" {
    template = "x86_64/e2e-alpine"
    cpus     = 1
    memory   = 512MiB
    audio    = true
  }
}
""")
    if not (h.hypervisor == "vmlab" and h.vms_here):
        v = h.vmlab("validate", cwd=lab, check=False)
        h.ok("vm.hw.audio", v.code != 0 and 'needs hypervisor = "vmlab"' in v.text,
             f"QEMU: validate exits {v.code}: {next((l.strip() for l in v.text.splitlines() if 'audio' in l), '')!r}")
        return
    try:
        h.vmlab("up", cwd=lab, timeout=600)
        h.wait_ready(lab, "snd01")

        def hda():
            dev = h.vmlab("exec", "snd01", "--", "sh", "-c",
                          "cat /sys/bus/pci/devices/0000:00:1b.0/vendor /sys/bus/pci/devices/0000:00:1b.0/device",
                          cwd=lab).out.split()
            assert dev == ["0x8086", "0x293e"], dev
            desc = json.loads((lab_runtime(lab, "snd01") / "machine.json").read_text())
            assert desc["audio"]["node_name"] == "vmlab.e2e-audio.snd01", desc["audio"]
            assert desc["audio"]["microphone"] is False, desc["audio"]
            cards = h.vmlab("exec", "snd01", "--", "cat", "/proc/asound/cards", cwd=lab, check=False).out.strip()
            return f"00:1b.0 is {dev[0]}:{dev[1]}, node vmlab.e2e-audio.snd01; /proc/asound/cards: {cards[:80]!r}"

        h.check("vm.hw.audio", hda,
                "the guest sees an Intel HDA (8086:293e) at 00:1b.0 and the description names the host node "
                "`vmlab.e2e-audio.snd01`, microphone off; no PipeWire in the container, so the host stream is "
                "checked live")
    finally:
        h.vmlab("destroy", cwd=lab, timeout=300, check=False)


def lab_runtime(lab: pathlib.Path, vm: str) -> pathlib.Path:
    """Where a running VM's machine description sits, which `vmlab machine
    info` does not print: the lab's runtime directory, by its machine."""
    base = pathlib.Path(os.environ.get("XDG_RUNTIME_DIR", f"/tmp/vmlab-{os.getuid()}")) / "vmlab" / "labs"
    found = sorted(base.glob(f"*/vms/{vm}/machine.json"))
    assert found, f"no machine.json for {vm} under {base}"
    return found[0].parent


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
                "the host file, vmlab creating the mount point itself, on a segment without nat; an XP-era "
                "client itself was not exercised")

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


def usb(h):
    """Every USB passthrough path that needs no host device (§5.2.1). The
    container sees the host's sysfs, but no device in it is one this run may
    take from the host, so the attach itself is left to a live check."""
    listed = h.vmlab("usb", "list")
    as_json = h.vmlab("usb", "list", "--json")
    rows = json.loads(as_json.out)
    h.ok(
        "usb.list",
        isinstance(rows, list)
        and (listed.out.startswith("PORT") if rows else "no USB devices" in listed.out),
        f"{len(rows)} device(s)",
    )

    with h.lab("unsupported-usb") as lab:
        h.vmlab("up", "usb01", "plain01", cwd=lab, timeout=600)
        h.wait_ready(lab, "usb01", "plain01")

        def root_hubs(vm: str) -> str:
            return h.vmlab("exec", vm, "--", "sh", "-c",
                           "cat /sys/bus/usb/devices/usb*/idProduct", cwd=lab).out

        hv = h.hypervisor == "vmlab" and h.vms_here

        def controller():
            # An xHCI's USB 3 root hub is 1d6b:0003; `-usb` alone has none.
            assert "0003" in root_hubs("usb01"), "no xHCI root hub in usb01"
            if not hv:
                assert "0003" not in root_hubs("plain01"), "plain01 has an xHCI it was not asked for"
                return True
            # vmlab-hv's tablet always rides an xHCI (HV phase-2 D6); `usb =
            # true` makes that function the passthrough controller too.
            usb01 = json.loads((lab_runtime(lab, "usb01") / "machine.json").read_text())
            plain = json.loads((lab_runtime(lab, "plain01") / "machine.json").read_text())
            assert usb01.get("usb") and usb01["usb"] == usb01["tablet"], usb01.get("usb")
            assert "usb" not in plain, plain.get("usb")
            return "usb01's description names its tablet xHCI as the passthrough controller; plain01's names none"

        h.check("usb.controller", controller)

        def refusals():
            said = {}
            for vm, sel in [("plain01", "dead:beef"), ("web", "dead:beef"),
                            ("usb01", "mouse"), ("usb01", "dead:beef")]:
                r = h.vmlab("usb", "attach", vm, sel, cwd=lab, check=False)
                assert r.code != 0, f"attach {vm} {sel} succeeded"
                said[(vm, sel)] = r.text
            assert "set `usb = true` on plain01 and restart it" in said[("plain01", "dead:beef")], said
            assert "USB passthrough is for VMs" in said[("web", "dead:beef")], said
            assert "is not a USB selector" in said[("usb01", "mouse")], said
            assert "no USB device matches `dead:beef`" in said[("usb01", "dead:beef")], said
            r = h.vmlab("usb", "detach", "usb01", "--all", cwd=lab)
            assert "holds no USB devices" in r.out, r.out
            return True

        h.check("usb.refusals", refusals)
        h.skip("usb.attach", "the e2e container has no host USB device it may take from the host; "
               "attach and detach are verified live against a real device"
               + (" (vmlab-hv's device_add/device_del/device_deleted round trip is unit-tested against "
                  "its protocol server)" if hv else ""))


def run(h):
    # The parts are independent: one aborting leaves its own features missing
    # (which fails the run) without costing the others theirs.
    failed = []
    for part in (gpu, audio, guest, usb):
        try:
            part(h)
        except ScenarioFailed as e:
            h.log.write(f"[unsupported] {part.__name__} aborted: {e}\n")
            failed.append(f"{part.__name__}: {e}")
    if failed:
        raise ScenarioFailed("; ".join(failed))
