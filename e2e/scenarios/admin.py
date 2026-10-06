"""Host-wide administration: the `vmlab lab` verbs against a running lab, the
supervisor's own lifecycle, and an emulated (non-x86) guest — the lab's one
machine is an aarch64 VM under TCG, so it doubles as the thing to administer.
"""

import pathlib
import shutil
import subprocess
import time

from harness import WORK

LAB = "e2e-admin"
QEMU_NAME = f"vmlab:{LAB}/arm01"


def pids(pattern: str) -> list[int]:
    p = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True)
    return [int(x) for x in p.stdout.split()]


def alive(pid: int) -> bool:
    """Running, as opposed to gone or a zombie nobody has reaped yet (the
    container's PID 1 does not reap, so an exited daemon lingers as `Z`)."""
    try:
        stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    return stat.rsplit(")", 1)[1].split()[0] != "Z"


def argv_of(pid: int) -> list[str]:
    return pathlib.Path(f"/proc/{pid}/cmdline").read_text().split("\0")


def listed(h) -> dict[str, str]:
    """`vmlab lab list` as name → state."""
    rows = h.vmlab("lab", "list").out.splitlines()
    return {c[0]: c[1] for c in (r.split() for r in rows[1:]) if len(c) >= 2}


ROOTS = "e2e-roots"


def two_roots(h):
    """Two checkouts declaring one lab name (ADR-0011): only one holds the
    name, the other is refused by name everywhere — `status` included
    (#148) — and a full `down` releases the name so the other can `up` with
    the clones it was handed (#142), or `vmlab lab move` hands them over
    (#143)."""
    with h.lab("admin-roots", under="a") as a, h.lab("admin-roots", under="b") as b:
        h.vmlab("up", cwd=a, timeout=300)
        qemu = f"vmlab:{ROOTS}/blank"

        st = h.vmlab("status", cwd=b, check=False)
        dn = h.vmlab("down", cwd=b, check=False, timeout=120)
        still = bool([p for p in pids(qemu) if alive(p)])
        h.ok(
            "lab.status.conflict",
            st.code == 5 and str(a) in st.text and "blank" not in st.out
            and dn.code == 5 and still,
            f"status in b: exit {st.code} {st.text.strip()[:160]!r}; "
            f"down in b: exit {dn.code}, a's VM still running={still}",
        )

        def release():
            h.vmlab("down", cwd=a, timeout=300)
            # Checked at once: the release waits for the daemon to go.
            assert not [p for p in pids(f"__labd --lab {ROOTS} ") if alive(p)], "a's daemon outlived `down`"
            assert ROOTS not in listed(h), h.vmlab("lab", "list").out
            # b takes over a's provisioned disks, as a moved worktree would.
            shutil.rmtree(b / ".vmlab", ignore_errors=True)
            (a / ".vmlab").rename(b / ".vmlab")
            up = h.vmlab("up", cwd=b, timeout=300, check=False)
            assert up.code == 0, up.text
            rows = h.vmlab("lab", "list").out
            assert listed(h).get(ROOTS) == "running" and str(b) in rows, rows
            back = h.vmlab("status", cwd=a, check=False)
            assert back.code == 5 and str(b) in back.text, back.text
            # By name from anywhere, the other remedy the conflict names.
            h.vmlab("lab", "stop", ROOTS, cwd=WORK, timeout=300)
            assert ROOTS not in listed(h), h.vmlab("lab", "list").out
            return True

        h.check(
            "lab.down.release",
            release,
            "down in a reaped the daemon and unlisted the lab; b came up on a's disks; "
            "status in a was then refused naming b; lab stop by name released it",
        )

        def move():
            # b holds the working data now and the name is released, so the
            # move back to a has to be told where the lab lives (#143).
            disk = b / ".vmlab" / "vms" / "blank" / "disk0.qcow2"
            inode = disk.stat().st_ino
            unknown = h.vmlab("lab", "move", ROOTS, cwd=a, check=False)
            assert unknown.code == 1 and "--from" in unknown.text, unknown.text
            mv = h.vmlab("lab", "move", ROOTS, "--from", str(b), cwd=a, check=False, timeout=120)
            assert mv.code == 0 and f"now lives in {a}" in mv.out, mv.text
            # One filesystem: the clone was renamed, not rebuilt or copied.
            moved = a / ".vmlab" / "vms" / "blank" / "disk0.qcow2"
            assert not (b / ".vmlab").exists() and moved.stat().st_ino == inode, mv.text
            rows = h.vmlab("lab", "list").out
            assert ROOTS in rows and str(a) in rows, rows
            up = h.vmlab("up", cwd=a, timeout=300, check=False)
            assert up.code == 0, up.text
            busy = h.vmlab("lab", "move", ROOTS, cwd=b, check=False)
            assert busy.code == 5 and "still has machines running" in busy.text, busy.text
            assert [p for p in pids(f"vmlab:{ROOTS}/blank") if alive(p)], "the refused move stopped a's VM"
            h.vmlab("down", cwd=a, timeout=300)
            return True

        h.check(
            "lab.move",
            move,
            "lab move without --from on a released lab asked for it; with --from it renamed "
            "b's clone into a and registered a; a came up on it; a move while it ran exited 5",
        )


def run(h):
    with h.lab("admin-lab") as lab:
        h.vmlab("up", cwd=lab, timeout=300)

        # -- an aarch64 guest under TCG ----------------------------------------
        def emulated():
            qemu = [p for p in pids(QEMU_NAME) if alive(p)]
            assert qemu, "no QEMU process for arm01"
            argv = argv_of(qemu[0])
            assert argv[0].endswith("qemu-system-aarch64"), argv[0]
            assert "tcg" in argv[argv.index("-accel") + 1], argv
            # vmlab's own AAVMF: the image carries no qemu-efi-aarch64.
            assert any(a.startswith("if=pflash") and "/usr/share/vmlab/guest/firmware/aarch64/AAVMF_CODE.fd" in a
                       for a in argv), [a for a in argv if "pflash" in a]
            # The firmware is aarch64 code running under emulation: it has to
            # get as far as drawing its shell before the screen means anything.
            h.wait_until(lambda: "UEFI" in h.vmlab("logs", "arm01", "-n", "200", cwd=lab, check=False).out,
                         timeout=120, what="the aarch64 firmware on the serial console")
            png = WORK / "arm01.png"
            png.unlink(missing_ok=True)
            h.vmlab("vm", "screenshot", "arm01", str(png), cwd=lab)
            data = png.read_bytes()
            assert data[:8] == b"\x89PNG\r\n\x1a\n" and len(data) > 1000, f"{len(data)} bytes"
            return True

        h.check("arch.emulated", emulated,
                "qemu-system-aarch64 -accel tcg; the bundled AAVMF reached its UEFI shell on serial; vm screenshot wrote a PNG")

        # -- lab verbs -------------------------------------------------------
        h.check("lab.list",
                lambda: listed(h).get(LAB) == "running" and str(lab) in h.vmlab("lab", "list").out,
                "e2e-admin listed running with its directory")
        h.check("lab.info",
                lambda: "arm01" in (i := h.vmlab("lab", "info", LAB).out) and str(lab) in i,
                "names arm01 and the lab directory")

        refused = h.vmlab("lab", "restart", LAB, check=False)
        refused_ok = refused.code != 0 and "still has machines running" in refused.text

        def stop():
            h.vmlab("lab", "stop", LAB, timeout=300)
            h.wait_until(lambda: not [p for p in pids(QEMU_NAME) if alive(p)], timeout=60,
                         what="arm01's QEMU to exit")
            # A full stop by name is a full `down`: the daemon is reaped and
            # the name released (#142), checked at once since the release
            # waits for the daemon.
            assert not [p for p in pids(f"__labd --lab {LAB} ") if alive(p)], "the lab daemon outlived `lab stop`"
            assert LAB not in listed(h), h.vmlab("lab", "list").out
            assert (lab / ".vmlab" / "vms" / "arm01").exists(), "clones were not retained"
            return True

        h.check("lab.stop", stop, "QEMU exited, the lab daemon reaped and the lab unlisted, clone retained")

        def restart():
            before = [p for p in pids(f"__labd --lab {LAB}") if alive(p)]
            # From the lab's directory: the stop released the registration,
            # so the directory is what says where the lab lives.
            r = h.vmlab("lab", "restart", LAB, cwd=lab)
            assert "restarted" in r.out, r.out
            after = [p for p in pids(f"__labd --lab {LAB}") if alive(p)]
            assert after and set(after) != set(before), f"labd pids {before} -> {after}"
            assert "arm01" in h.vmlab("lab", "info", LAB).out
            return True

        h.ok("lab.restart", refused_ok and h.check("lab.restart", restart),
             f"refused while running ({refused.text.strip()[:90]!r}); after stop a new lab daemon answers")

        # `destroy` returns only once the lab daemon and everything it owns
        # have gone, so an `up` straight after it starts clean.
        def destroy_then_up():
            h.vmlab("destroy", cwd=lab, timeout=300)
            left = [p for p in pids(f"__labd --lab {LAB} ") if alive(p)]
            assert not left, f"destroy returned with the lab daemon still running: {left}"
            assert LAB not in listed(h), h.vmlab("lab", "list").out
            h.vmlab("up", cwd=lab, timeout=300)
            assert listed(h).get(LAB) == "running", h.vmlab("lab", "list").out
            return True

        h.check("lab.destroy", destroy_then_up,
                "destroy returned with the lab daemon gone and the lab unlisted; an immediate up succeeded")

        def destroy_named():
            h.vmlab("lab", "destroy", LAB, cwd=WORK, timeout=300)
            # Checked at once, not polled: the release waits for the daemon.
            assert LAB not in listed(h), h.vmlab("lab", "list").out
            assert not (lab / ".vmlab").exists(), "the lab's .vmlab survived"
            return True

        h.check("lab.destroy.named", destroy_named, "destroyed by name from outside the lab: gone from list, .vmlab removed")

    two_roots(h)

    # -- the supervisor ---------------------------------------------------------
    def daemon():
        # Give the destroyed lab's daemon time to go, so the stop is the
        # supervisor's alone.
        h.wait_until(lambda: not [p for p in pids("__labd") if alive(p)], timeout=60, what="lab daemons to exit")
        h.vmlab("daemon", "start")
        s = h.vmlab("daemon", "status").out
        assert "vmlabd" in s and "not running" not in s, s
        sup = [p for p in pids("__supervisord") if alive(p)]
        assert len(sup) == 1, f"supervisor pids {sup}"
        h.vmlab("daemon", "stop")
        # Checked at once, not polled: `stop` returns only once it is gone.
        assert not alive(sup[0]), "the supervisor outlived `vmlab daemon stop`"
        assert "not running" in h.vmlab("daemon", "status").out
        h.vmlab("daemon", "start")
        s = h.vmlab("daemon", "status").out
        assert "not running" not in s and [p for p in pids("__supervisord") if alive(p)], s
        return True

    h.check("daemon.lifecycle", daemon,
            "status running; stop returned with the supervisor already gone; status not running; start brought it back")
