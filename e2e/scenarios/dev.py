"""Dev machines: the workspace syncer both ways, its halt and the verbs that
clear one, the machine-selection ladder, and the snapshot bracket.

Two container dev machines and no default between them, so every `dev sync`
verb here names its machine — which is itself the ladder's first rung.
"""

import os
import time

from harness import ScenarioFailed

M = "dev01"


def gx(h, lab, script: str, **kw):
    """A guest shell command on dev01, as its default login."""
    return h.vmlab("exec", M, "--", "sh", "-c", script, cwd=lab, **kw)


def guest_cat(h, lab, path: str) -> str:
    r = gx(h, lab, f"cat /src/{path} 2>/dev/null", check=False)
    return r.out if r.code == 0 else ""


def status(h, lab) -> str:
    return h.vmlab("dev", "sync", "status", M, cwd=lab, check=False).text


def in_step(h, lab) -> bool:
    return "is in step" in status(h, lab)


def halted(h, lab) -> str:
    s = status(h, lab)
    return s if "has stopped, both directions" in s else ""


def conflict(h, lab, name: str, tag: str) -> str:
    """Edit `name` on both sides inside one debounce window, so a pass sees
    both changed since they last agreed. The guest keeps rewriting it for three
    seconds — a path still moving keeps waiting — and the host writes once in
    the middle. Returns the halt report."""
    for attempt in range(3):
        loop = h.background(
            ["vmlab", "exec", M, "--", "sh", "-c",
             f"for i in $(seq 1 30); do echo guest-{tag} > /src/{name}; sleep 0.1; done"],
            cwd=lab,
        )
        time.sleep(1)
        (lab / "ws1" / name).write_text(f"host-{tag}\n")
        loop.wait(timeout=60)
        try:
            return h.wait_until(lambda: halted(h, lab), timeout=15, what=f"a halt on {name}")
        except ScenarioFailed:
            # The race went one way and the sides agreed; try again.
            h.wait_until(lambda: in_step(h, lab), timeout=30, what="the workspace to settle")
    raise ScenarioFailed(f"three attempts at a both-sides edit of {name} never halted the workspace")


def run(h):
    with h.lab("dev-sync") as lab:
        h.vmlab("up", cwd=lab, timeout=600)
        h.wait_ready(lab, "dev01", "dev02")
        h.wait_until(lambda: in_step(h, lab), timeout=60, what="dev01's first sync pass")

        # -- selection ladder ------------------------------------------------
        def select():
            none = h.vmlab("dev", "sync", "status", cwd=lab, check=False)
            assert none.code != 0 and "none of them is the default" in none.text \
                and '"dev01", "dev02"' in none.text, f"no-default error: {none.text!r}"
            arg = h.vmlab("dev", "sync", "status", "dev01", cwd=lab)
            assert '"dev01"' in arg.out, arg.out
            env = h.vmlab("dev", "sync", "status", cwd=lab, env={"VMLAB_DEV_MACHINE": "dev02"})
            assert '"dev02"' in env.out, env.out
            both = h.vmlab("dev", "sync", "status", "dev01", cwd=lab, env={"VMLAB_DEV_MACHINE": "dev02"})
            assert '"dev01"' in both.out, f"argument must beat the environment: {both.out!r}"
            bad = h.vmlab("dev", "sync", "status", cwd=lab, check=False, env={"VMLAB_DEV_MACHINE": "ghost"})
            assert bad.code != 0 and 'names "ghost"' in bad.text and '"dev01", "dev02"' in bad.text, bad.text
            return True

        h.check("dev.select", select,
                "no default lists dev01+dev02; argument picks dev01; VMLAB_DEV_MACHINE picks dev02; "
                "argument beats env; VMLAB_DEV_MACHINE=ghost refused naming both")

        # -- both directions -------------------------------------------------
        h.check(
            "dev.sync.host-to-guest",
            lambda: "seeded by the host" in guest_cat(h, lab, "hello.txt")
            and (lab / "ws1" / "hello.txt").write_text("edited on the host\n")
            and h.wait_until(lambda: "edited on the host" in guest_cat(h, lab, "hello.txt"),
                             timeout=30, what="the host edit in the guest"),
            "first pass seeded /src/hello.txt; a host edit reached it",
        )

        def guest_to_host():
            gx(h, lab, "echo written in the guest > /src/out.txt")
            h.wait_until(lambda: (lab / "ws1" / "out.txt").exists(), timeout=30, what="out.txt on the host")
            owner = gx(h, lab, "stat -c %U /src/out.txt").out.strip()
            text = (lab / "ws1" / "out.txt").read_text()
            assert "written in the guest" in text, text
            assert owner == "dev", f"guest file owned by {owner!r}, not the default login"
            return True

        h.check("dev.sync.guest-to-host", guest_to_host, "guest-written /src/out.txt (owned by dev) reached ./ws1")

        def modes():
            run_sh = lab / "ws1" / "run.sh"
            run_sh.write_text("#!/bin/sh\necho ran-on-guest\n")
            run_sh.chmod(0o755)
            gx(h, lab, "printf '#!/bin/sh\\necho ran-on-host\\n' > /src/build.sh; chmod 755 /src/build.sh")
            h.wait_until(
                lambda: gx(h, lab, "test -x /src/run.sh", check=False).code == 0,
                timeout=30, what="run.sh executable in the guest")
            built = lab / "ws1" / "build.sh"
            h.wait_until(lambda: built.exists() and os.access(built, os.X_OK), timeout=30,
                         what="build.sh executable on the host")
            assert "ran-on-guest" in gx(h, lab, "/src/run.sh").out
            assert "ran-on-host" in h.run([str(built)], cwd=lab).out
            return True

        h.check("dev.sync.modes", modes, "host 0755 run.sh runs in the guest; guest chmod +x build.sh runs on the host")

        def status_flush():
            # No wait before the flush: a completed flush carries every write
            # made before it, the host one here still inside its debounce.
            gx(h, lab, "echo from-guest > /src/flushed-g.txt")
            (lab / "ws1" / "flushed.txt").write_text("flush me\n")
            f = h.vmlab("dev", "sync", "flush", M, cwd=lab)
            assert "is in step" in f.out, f.out
            assert "flush me" in guest_cat(h, lab, "flushed.txt")
            assert (lab / "ws1" / "flushed-g.txt").read_text().strip() == "from-guest"
            s = h.vmlab("dev", "sync", "status", M, cwd=lab)
            assert "passes" in s.out, s.out
            return True

        h.check("dev.sync.status-flush", status_flush, "flush reports in step with both new files carried; status counts passes")

        # -- snapshot bracket, part one: clean capture, restore re-seeds -------
        def capture_and_reseed():
            cap = h.vmlab("snapshot", "create", "clean", "--vm", M, cwd=lab)
            assert "created" in cap.out and "not a workspace backup" in cap.out, cap.out
            gx(h, lab, "echo after-snapshot > /src/after.txt; echo scratch > /tmp/outside-workspace")
            h.wait_until(lambda: (lab / "ws1" / "after.txt").exists(), timeout=30, what="after.txt on the host")
            res = h.vmlab("snapshot", "restore", "clean", "--vm", M, cwd=lab)
            assert "not a workspace backup" in res.out, res.out
            # Rewound: the guest-only scratch file outside the workspace is gone,
            # while the re-seed carries after.txt back from the host.
            gone = gx(h, lab, "test -e /tmp/outside-workspace", check=False).code != 0
            assert gone, "the restore did not rewind the guest"
            h.wait_until(lambda: "after-snapshot" in guest_cat(h, lab, "after.txt"), timeout=30,
                         what="the re-seed to put after.txt back")
            assert (lab / "ws1" / "after.txt").exists(), "the restore reached the host tree"
            logs = h.vmlab("logs", "-o", "jsonl", "-n", "500", cwd=lab).out
            assert "workspace.reconverged" in logs, "no workspace.reconverged event"
            return True

        h.check("dev.snapshot-bracket", capture_and_reseed,
                "clean capture; a restore rewound the guest and re-seeded after.txt from the host; both said snapshots are not a workspace backup")

        # Unsynced guest work, the slow way: a guest file still being written
        # sits inside its debounce window, so the pre-flight flush cannot carry it.
        def refuse_busy():
            loop = h.background(
                ["vmlab", "exec", M, "--", "sh", "-c",
                 "for i in $(seq 1 40); do echo $i > /src/busy.txt; sleep 0.1; done"], cwd=lab)
            time.sleep(1)
            r = h.vmlab("snapshot", "create", "busy", "--vm", M, cwd=lab, check=False)
            loop.wait(timeout=60)
            assert r.code != 0 and "busy.txt" in r.text and "no flag" in r.text, r.text
            assert "not a workspace backup" in r.text, r.text
            return True

        h.check("dev.snapshot-bracket", refuse_busy,
                "capture refused while the guest was still writing busy.txt, naming it")
        h.wait_until(lambda: in_step(h, lab), timeout=30, what="busy.txt to settle")

        # -- conflict --------------------------------------------------------
        def conflict_cycle():
            said = conflict(h, lab, "both.txt", "a")
            assert "both.txt" in said, said
            marker = gx(h, lab, "cat /src/.vmlab-sync-halt", check=False).out
            assert "both.txt" in marker, f"guest marker file: {marker!r}"
            d = h.vmlab("dev", "sync", "diff", "--machine", M, cwd=lab).out
            assert "-host-a" in d and "+guest-a" in d, d
            # A halt refuses capture, and the refusal names the path.
            cap = h.vmlab("snapshot", "create", "halted", "--vm", M, cwd=lab, check=False)
            assert cap.code != 0 and "both.txt" in cap.text, cap.text
            h.vmlab("dev", "sync", "resolve", "--machine", M, "--host", "both.txt", cwd=lab)
            h.wait_until(lambda: in_step(h, lab), timeout=30, what="the halt to clear")
            assert guest_cat(h, lab, "both.txt").strip() == "host-a"
            assert (lab / "ws1" / "both.txt").read_text().strip() == "host-a"
            assert gx(h, lab, "test -e /src/.vmlab-sync-halt", check=False).code != 0, "marker outlived the halt"
            return True

        h.check("dev.sync.conflict", conflict_cycle,
                "both-sides edit halted naming both.txt (status + guest marker); diff showed -host/+guest; "
                "resolve --host cleared it and the guest matches the host")

        # -- snapshot bracket, part two: restore over a halt ------------------
        def restore_over_halt():
            conflict(h, lab, "both.txt", "b")
            r = h.vmlab("snapshot", "restore", "clean", "--vm", M, cwd=lab, check=False)
            assert r.code != 0 and "--discard-guest-changes" in r.text and "both.txt" in r.text, r.text
            h.vmlab("snapshot", "restore", "clean", "--vm", M, "--discard-guest-changes", cwd=lab)
            h.wait_until(lambda: guest_cat(h, lab, "both.txt").strip() == "host-b", timeout=30,
                         what="the re-seed to carry the host's both.txt")
            h.wait_until(lambda: in_step(h, lab), timeout=30, what="the workspace to be in step")
            assert (lab / "ws1" / "both.txt").read_text().strip() == "host-b", "host copy changed"
            return True

        h.check("dev.snapshot-bracket", restore_over_halt,
                "restore refused over a halt naming --discard-guest-changes; with it the guest re-seeded to the host copy")
