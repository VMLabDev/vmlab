"""Playbooks: a config-weave play applied on `up`, listed, then checked for
drift and re-applied by hand.

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
        for f in ("playbook.up", "playbook.list", "playbook.check-apply"):
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
            drift = h.vmlab("playbook", "check", "vm01", cwd=lab, timeout=300)
            assert "1 not_configured" in drift.out, f"check did not report drift: {drift.out.strip()[-300:]!r}"
            assert guest_file(h, lab) == "tampered", "check changed the guest"
            applied = h.vmlab("playbook", "apply", "vm01", cwd=lab, timeout=300)
            assert guest_file(h, lab) == WANT, f"apply left {guest_file(h, lab)!r}: {applied.out.strip()[-300:]!r}"
            clean = h.vmlab("playbook", "check", "vm01", cwd=lab, timeout=300)
            assert "1 already_configured" in clean.out, f"check after apply: {clean.out.strip()[-300:]!r}"
            return True

        h.check("playbook.check-apply", check_apply,
                "check reported 1 not_configured without touching the tampered file; apply restored it; check then 1 already_configured")
