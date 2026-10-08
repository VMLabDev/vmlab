"""Playbooks: a config-weave play applied on `up`, listed, then checked for
drift and re-applied by hand; and a run that outlives its `timeout`, which
holds the next run off until what it started has finished.

The guest binary comes from VMLAB_CONFIG_WEAVE_DIR, which the e2e run mounts
from the host's config-weave install."""

import os
import pathlib

LAB = "e2e-playbooks"
MARKER = "/etc/e2e-weave.txt"
WANT = "converged-by-vmlab"


def guest_file(h, lab) -> str:
    r = h.vmlab("exec", "vm01", "--", "/bin/sh", "-c", f"cat {MARKER} 2>/dev/null || echo MISSING", cwd=lab)
    return r.out.strip()


def run(h):
    bin_dir = pathlib.Path(os.environ.get("VMLAB_CONFIG_WEAVE_DIR", pathlib.Path.home() / ".local/share/config-weave/bin"))
    if not (bin_dir / "config-weave-linux-x86_64").exists():
        reason = f"no config-weave-linux-x86_64 in {bin_dir}: mount the host's config-weave bin and set VMLAB_CONFIG_WEAVE_DIR"
        for f in ("playbook.up", "playbook.list", "playbook.check-apply", "playbook.timeout"):
            h.skip(f, reason)
        return

    with h.lab("playbooks-lab") as lab:
        up = h.vmlab("up", cwd=lab, timeout=900, check=False)
        if up.code == 0:
            h.wait_ready(lab, "vm01")
        got = guest_file(h, lab) if up.code == 0 else up.text.strip()[-300:]
        h.ok("playbook.up", up.code == 0 and got == WANT,
             f"`up` applied the play and its var: {MARKER}={got!r}")
        if up.code != 0:
            return

        listed = h.vmlab("playbook", "list", cwd=lab)
        h.ok(
            "playbook.list",
            "vm01 → playbooks/marker play marker" in listed.out and f"var marker_content={WANT}" in listed.out,
            listed.out.strip()[:200],
        )

        def check_apply():
            # Drift: the file's content changed behind the play's back.
            h.vmlab("exec", "vm01", "--", "/bin/sh", "-c", f"echo tampered > {MARKER}", cwd=lab)
            drift = h.vmlab("playbook", "check", "vm01", "--playbook", "playbooks/marker", cwd=lab, timeout=300)
            assert "1 not_configured" in drift.out, f"check did not report drift: {drift.out.strip()[-300:]!r}"
            assert guest_file(h, lab) == "tampered", "check changed the guest"
            applied = h.vmlab("playbook", "apply", "vm01", "--playbook", "playbooks/marker", cwd=lab, timeout=300)
            assert guest_file(h, lab) == WANT, f"apply left {guest_file(h, lab)!r}: {applied.out.strip()[-300:]!r}"
            clean = h.vmlab("playbook", "check", "vm01", "--playbook", "playbooks/marker", cwd=lab, timeout=300)
            assert "1 already_configured" in clean.out, f"check after apply: {clean.out.strip()[-300:]!r}"
            return True

        h.check("playbook.check-apply", check_apply,
                "check reported 1 not_configured without touching the tampered file; apply restored it; check then 1 already_configured")

        def timeout():
            slow = ("playbook", "apply", "vm01", "--playbook", "playbooks/slow")
            h.vmlab("exec", "vm01", "--", "/bin/sh", "-c", "touch /tmp/e2e-slow", cwd=lab)
            first = h.vmlab(*slow, cwd=lab, timeout=120, check=False)
            assert first.code != 0, f"the slow run did not fail: {first.text.strip()[-300:]!r}"
            assert "timed out after 5s" in first.text, f"no timeout: {first.text.strip()[-300:]!r}"
            assert "still running in the guest" in first.text, f"no orphan warning: {first.text.strip()[-300:]!r}"

            # The step's `sleep 20` outlived config-weave. Running again now
            # would start a second one over it.
            second = h.vmlab(*slow, cwd=lab, timeout=120, check=False)
            assert second.code != 0, f"the rerun was not refused: {second.text.strip()[-300:]!r}"
            assert "still running in the guest" in second.text, f"refusal: {second.text.strip()[-300:]!r}"
            listed = h.vmlab("playbook", "list", cwd=lab)
            assert "still running, and a new run is refused" in listed.out, f"list: {listed.out.strip()[-400:]!r}"

            # Once it has finished the next run goes ahead, and converges
            # without the trigger.
            h.vmlab("exec", "vm01", "--", "/bin/sh", "-c", "rm -f /tmp/e2e-slow", cwd=lab)
            h.wait_until(
                lambda: "everything it started has finished" in h.vmlab("playbook", "list", cwd=lab).out,
                timeout=90, interval=3, what="the timed-out run's processes to finish",
            )
            third = h.vmlab(*slow, cwd=lab, timeout=120, check=False)
            assert third.code == 0, f"the run after the drain failed: {third.text.strip()[-300:]!r}"
            listed = h.vmlab("playbook", "list", cwd=lab)
            assert "timed out at" not in listed.out, f"the record outlived the run: {listed.out.strip()[-400:]!r}"
            return True

        h.check("playbook.timeout", timeout,
                "a 5 s timeout failed the run naming the orphan; the rerun was refused and `playbook list` said why; after the orphan finished the run converged and the record cleared")
