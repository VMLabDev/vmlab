"""Core coverage on vmlab-hv in labs of at most two VMs.

The provision dependency and scratch boot have separate labs. Lifecycle and
hardware checks share the two-machine core-hv lab. Normal cleanup destroys
each lab before starting the next.
"""

import json
import re
import shutil
from datetime import datetime, timezone

from harness import E2E
from scenarios import core


def provision_order(h):
    with h.lab("core-hv-provision") as lab:
        shutil.copytree(E2E / "labs/core-lab/scripts", lab / "scripts")
        part = h.vmlab("up", "vm01", cwd=lab, timeout=600)
        h.wait_ready(lab, "vm01", "vm03")
        full = h.vmlab("up", cwd=lab, timeout=600)
        end = full.text.find("vm03: e2e-prov vm03 end")
        begin = full.text.find("vm01: e2e-prov vm01 begin")
        h.ok(
            "lab.depends_on_provision",
            'vm03: pulled in \u2014 "vm01" names it in depends_on_provision' in part.text
            and "vm01: waiting for the provisioning of vm03 (depends_on_provision)" in full.text
            and 0 <= end < begin,
            f"vm03 pulled into partial up; full up: vm03 ended at {end}, vm01 began at {begin}",
        )


def scratch(h):
    with h.lab("core-hv-scratch") as lab:
        h.vmlab("up", cwd=lab, timeout=300)

        def boot():
            screen = h.wait_until(
                lambda: (lambda t: t if "bootable" in t else None)(
                    h.vmlab("vm", "ocr", "blank", cwd=lab, check=False).out),
                timeout=90, interval=3, what="the scratch VM's boot failure screen",
            )
            disk = lab / ".vmlab/vms/blank/disk0.qcow2"
            info = h.run(["qemu-img", "info", "-U", "--output=json", str(disk)])
            size = json.loads(info.out)["virtual-size"]
            assert core.state_of(h, lab, "blank") == "running" and size == 1 << 30, info.out
            assert "No bootable device" in screen, screen
            return True

        h.check("vm.scratch", boot, "blank 1GiB disk, no backing template, SeaBIOS boot failure")
        stop = h.vmlab("vm", "stop", "--force", "blank", cwd=lab, timeout=60, check=False)
        return (stop.code == 0 and core.state_of(h, lab, "blank") == "stopped"
                and not core.machine_pid("e2e-core-scratch", "blank"))


def hardware(h, lab):
    core.pinned_tsc(h, lab)
    p = core.kv(core.sh(h, lab, "vm01", core.PROBE_VM01).out)
    memory = int(p.get("memkb", "0") or 0)
    h.ok("vm.hw.cpus-memory", p.get("nproc") == "2" and 600_000 < memory <= 768 * 1024,
         f"nproc={p.get('nproc')} MemTotal={memory}kB (declared 2 cpus, 768MiB)")
    sizes = {k[5:]: int(v) for k, v in p.items() if k.startswith("size_")}
    h.ok("vm.hw.disk", 2 * 1024 * 1024 in sizes.values(),
         f"guest block devices (512-byte sectors): {sizes}; data = 1GiB = 2097152")
    h.ok("vm.hw.disk-from", p.get("payload") == "disk-from payload e2e",
         f"FAT disk {p.get('payload_dev')} carries payload.txt={p.get('payload')!r}")
    h.ok("vm.hw.cdrom", p.get("cdrom") == "cdrom attachment e2e", f"E2ECDROM mounted: {p.get('cdrom')!r}")
    h.ok("vm.media.iso", p.get("media_iso") == "media iso e2e", f"E2EMEDIA mounted: {p.get('media_iso')!r}")
    other = core.sh(h, lab, "vm02", "grep -cwE 'vmx|svm' /proc/cpuinfo; true", check=False).out.strip()
    h.ok("vm.hw.nested", int(p.get("virt", "0") or 0) > 0 and other == "0",
         f"nested vm01: vmx|svm on {p.get('virt')} cpu(s); vm02 without it: on {other or '?'}")


def run(h):
    for fid in ("vm.hw.qemu_args", "vm.hw.floppy", "vm.media.floppy"):
        h.skip(fid, h.out_of_scope(fid) or "not declared by the compatible core labs")
    provision_order(h)
    forced_stop = scratch(h)
    with h.lab("core-hv") as lab:
        for folder in ("cd", "payload", "media-iso", "scripts"):
            shutil.copytree(E2E / "labs" / "core-lab" / folder, lab / folder)
        core.build_images(h, lab)
        valid = h.vmlab("validate", cwd=lab)
        h.ok("lab.validate", 'lab "e2e-core"' in valid.out and "2 vm(s)" in valid.out, valid.out.strip())

        # A true partial up leaves the other declared machine stopped.
        part = h.vmlab("up", "vm01", cwd=lab, timeout=600)
        h.wait_ready(lab, "vm01")
        states = {m: core.state_of(h, lab, m) for m in ("vm01", "vm02")}
        h.ok("lab.up.partial", part.code == 0 and states == {"vm01": "running", "vm02": "stopped"},
             f"up vm01: {states}")
        h.vmlab("down", cwd=lab, timeout=300)

        since = datetime.now(timezone.utc).replace(tzinfo=None)
        ordered = h.vmlab("up", "vm02", cwd=lab, timeout=600)
        h.wait_ready(lab, "vm01", "vm02")
        events = core.events_since(h, lab, since)
        ready = core.first(events, "vm.ready", "vm01")
        start = core.first(events, "vm.starting", "vm02")
        h.ok("lab.depends_on", 'vm01: pulled in \u2014 "vm02" names it in depends_on' in ordered.text
             and ready is not None and start is not None and start >= ready,
             f"vm01 ready {ready}, vm02 starting {start}")

        full = h.vmlab("up", cwd=lab, timeout=600)
        states = {m: core.state_of(h, lab, m) for m in ("vm01", "vm02")}
        h.ok("lab.up", 'lab "e2e-core" is up' in full.out and set(states.values()) == {"running"}, f"states {states}")
        at = {f"{m} {edge}": full.text.find(f"{m}: e2e-prov {m} {edge}")
              for m in ("vm01", "vm02") for edge in ("begin", "end")}
        h.ok("lab.provision.parallel", all(i >= 0 for i in at.values())
             and at["vm01 begin"] < at["vm02 end"] and at["vm02 begin"] < at["vm01 end"],
             f"provision offsets in full up: {at}")

        status = h.vmlab("status", cwd=lab).out
        verbose = h.vmlab("status", "-v", cwd=lab).out
        h.ok("lab.status", all(re.search(rf"^\s+{m}\s+vm\s+running", status, re.M) for m in ("vm01", "vm02"))
             and "10.80.0.10" in status and "lan" in status and verbose.count("ready=yes") == 2
             and "state=running" in verbose, "both running, vm01 static IP, verbose state and readiness")
        ip = h.vmlab("vm", "ip", "vm01", cwd=lab).out.strip()
        other_ip = h.vmlab("vm", "ip", "--nic", "0", "vm02", cwd=lab).out.strip()
        h.ok("vm.ip", ip == "10.80.0.10" and other_ip.startswith("10.80.0."), f"vm01 {ip!r}, vm02 --nic 0 {other_ip!r}")
        hardware(h, lab)

        before = core.sh(h, lab, "vm02", "cat /proc/sys/kernel/random/boot_id; touch /root/e2e-marker; sync").out.strip()
        restart = h.vmlab("vm", "restart", "vm02", cwd=lab, check=False)
        h.wait_ready(lab, "vm02")
        after = core.sh(h, lab, "vm02", "cat /proc/sys/kernel/random/boot_id; ls /root/e2e-marker", check=False).out
        h.ok("vm.restart", restart.code == 0 and bool(before) and before not in after and "/root/e2e-marker" in after,
             "new boot_id; marker written before restart survived")

        clone = lab / ".vmlab/vms/vm02"
        destroy = h.vmlab("vm", "destroy", "vm02", cwd=lab, check=False)
        h.ok("vm.destroy", destroy.code == 0 and 'vm "vm02" destroyed' in destroy.out and not clone.exists()
             and core.state_of(h, lab, "vm02") == "stopped" and core.state_of(h, lab, "vm01") == "running",
             "clone removed, vm02 stopped, vm01 still running")
        start = h.vmlab("vm", "start", "vm02", cwd=lab, timeout=300, check=False)
        h.wait_ready(lab, "vm02")
        fresh = core.sh(h, lab, "vm02", "ls /root/e2e-marker", check=False)
        h.ok("vm.start", start.code == 0 and clone.exists() and fresh.code != 0 and "No such file" in fresh.text,
             "fresh clone booted without the pre-destroy marker")
        stop = h.vmlab("vm", "stop", "vm02", cwd=lab, timeout=180, check=False)
        h.ok("vm.stop", forced_stop and stop.code == 0 and core.state_of(h, lab, "vm02") == "stopped"
             and not core.machine_pid(core.LAB, "vm02") and core.state_of(h, lab, "vm01") == "running",
             f"graceful vm02 stop, vm01 untouched; separate scratch forced stop={forced_stop}")

        down = h.vmlab("down", cwd=lab, timeout=300, check=False)
        status = h.vmlab("status", cwd=lab, check=False).out
        running = [m for m in ("vm01", "vm02") if core.machine_pid(core.LAB, m)]
        h.ok("lab.down", down.code == 0 and not running and "not running" in status
             and core.LAB not in h.vmlab("lab", "list").out and (lab / ".vmlab/vms/vm01").exists(),
             f"no processes left ({running}); daemon released ({status.strip()!r}); clones retained")
        core.reload_after_down(h, lab, worker="vm02")
        destroy = h.vmlab("destroy", cwd=lab, timeout=300, check=False)
        status = h.vmlab("status", cwd=lab, check=False).out
        h.ok("lab.destroy", destroy.code == 0 and 'lab "e2e-core" destroyed' in destroy.out
             and not (lab / ".vmlab").exists() and "not running" in status,
             f".vmlab removed; status: {status.strip()}")
