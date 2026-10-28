"""Worktree labs (§21.2): a main checkout and a linked worktree of a scratch
git repository. The main lab is provisioned, snapshotted offline and frozen;
`up` in the worktree clones each machine from the freeze, and both labs then
run at once."""

import json

from harness import WORK

LAB = "worktree-clone"
MARK = "/root/e2e-provisioned"


def sh(h, lab, machine, script, container=False, **kw):
    verb = ["container", "exec"] if container else ["exec"]
    return h.vmlab(*verb, machine, "--", "/bin/sh", "-c", script, cwd=lab, **kw)


def answer(h, lab, machine, script, container=False) -> str:
    """`script`'s output, once the machine answers exec."""

    def read():
        r = sh(h, lab, machine, script, container=container, check=False)
        return r.out.strip() if r.code == 0 else None

    return h.wait_until(read, timeout=180, interval=2, what=f"{machine} to answer exec")


def guest_mac(h, lab, machine, container=False) -> str:
    return answer(h, lab, machine, "cat /sys/class/net/eth0/address", container=container)


def recorded_macs(lab, machine) -> list:
    state = json.loads((lab / ".vmlab" / "state.json").read_text())
    return state["machines"][machine]["macs"]


def state(h, lab, machine) -> str:
    line = h.machine_line(lab, machine)
    return line.split()[0] if line else ""


def run(h):
    with h.lab(LAB, under="worktree") as main:
        wt = WORK / "worktree" / f"{LAB}-feat"
        git = ["git", "-c", "user.name=e2e", "-c", "user.email=e2e@vmlab.invalid"]
        h.run(["git", "init", "-q", "-b", "main"], cwd=main)
        (main / ".gitignore").write_text(".vmlab/\n")
        h.run(["git", "add", "-A"], cwd=main)
        h.run([*git, "commit", "-q", "-m", "lab"], cwd=main)
        h.run(["rm", "-rf", str(wt)], check=False)
        h.run(["git", "worktree", "add", "-q", "-b", "feat", str(wt)], cwd=main)
        try:
            body(h, main, wt)
        finally:
            if not h.keep:
                # The worktree first: the main checkout refuses `destroy`
                # while a worktree disk backs onto its freeze (§21.5).
                h.destroy(wt)


def body(h, main, wt):
    # -- the main lab, provisioned, then frozen at an offline snapshot ------
    h.vmlab("up", cwd=main, timeout=900)
    h.wait_ready(main, "vm01", "c01")
    sh(h, main, "vm01", f"echo from-main > {MARK}; sync")
    sh(h, main, "c01", f"echo from-main > {MARK}; sync", container=True)
    sh(h, main, "c01", "echo volume-from-main > /data/v; sync", container=True)
    main_macs = {
        "vm01": guest_mac(h, main, "vm01"),
        "c01": guest_mac(h, main, "c01", container=True),
    }
    h.vmlab("down", cwd=main, timeout=300)
    h.vmlab("snapshot", "create", "base", cwd=main, timeout=300)
    h.vmlab("worktree", "freeze", "base", cwd=main, timeout=600)
    # The main lab carries on after the freeze; what it does next is its own.
    h.vmlab("up", cwd=main, timeout=900)
    h.wait_ready(main, "vm01", "c01")
    sh(h, main, "vm01", f"echo after-freeze > {MARK}; sync")

    # -- worktree.clone: the worktree's `up` clones the freeze --------------
    def clone():
        r = h.vmlab("up", cwd=wt, timeout=900)
        assert "cloning from freeze generation 1" in r.out, r.text
        h.wait_ready(wt, "vm01", "c01")
        vm = answer(h, wt, "vm01", f"cat {MARK}")
        ct = answer(h, wt, "c01", f"cat {MARK}", container=True)
        vol = answer(h, wt, "c01", "cat /data/v", container=True)
        assert vm == "from-main" and ct == "from-main", f"vm01={vm!r} c01={ct!r}"
        assert vol == "volume-from-main", f"volume={vol!r}"
        local = wt / ".vmlab"
        gen = (local / "vms" / "vm01" / "freeze-generation").read_text().strip()
        assert gen == "1", f"freeze-generation={gen!r}"
        info = h.run(["qemu-img", "info", "-U", "--output=json",
                      str(local / "vms" / "vm01" / "disk0.qcow2")]).out
        backing = json.loads(info).get("backing-filename", "")
        assert backing == str(main / ".vmlab" / "freeze" / "1" / "vm01" / "disk0.qcow2"), backing
        listed = h.vmlab("snapshot", "list", "vm01", cwd=wt).out
        assert "base" in listed, listed
        # The worktree's guest writes its own overlay, never the main's.
        sh(h, wt, "vm01", f"echo from-worktree > {MARK}; sync")
        again = answer(h, main, "vm01", f"cat {MARK}")
        assert again == "after-freeze", f"main vm01={again!r}"
        return True

    h.check("worktree.clone", clone,
            "provisioned state and named volume carried; overlay backs onto generation 1; "
            "the freeze point listed")

    # -- worktree.clone.macs: the frozen MACs, both labs up at once ---------
    def macs():
        wt_macs = {
            "vm01": guest_mac(h, wt, "vm01"),
            "c01": guest_mac(h, wt, "c01", container=True),
        }
        assert wt_macs == main_macs, f"worktree {wt_macs} main {main_macs}"
        for m in ("vm01", "c01"):
            assert recorded_macs(wt, m) == recorded_macs(main, m), m
        running = [state(h, lab, m) for lab in (main, wt) for m in ("vm01", "c01")]
        assert all(s == "state=running" for s in running), running
        return True

    h.check("worktree.clone.macs", macs,
            "both machines keep the frozen MACs; main and worktree labs running together")
