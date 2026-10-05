"""Sleep: a guest that suspends itself to RAM (ACPI S3) is reported as
`suspended` — QEMU alive, nothing answering — until `vm start` wakes it, and a
VM declaring `prevent_sleep` is woken by the lab daemon the moment it sleeps.

The guest is put to sleep from inside, the way an idle Windows client edition
does it on its own: `echo mem > /sys/power/state`, with `mem_sleep` set to
`deep` so the kernel takes ACPI S3 rather than suspend-to-idle (which QEMU
never sees)."""

import re
import time
from datetime import datetime, timezone

from harness import ScenarioFailed
from scenarios.core import events_since, state_of

LAB = "e2e-sleep"
OWNED = ["vm.sleep", "vm.prevent_sleep"]

# Backgrounded and detached so the exec returns before the guest sleeps; the
# delay gives the agent time to send the exec's reply.
SLEEP_NOW = (
    "echo deep > /sys/power/mem_sleep 2>/dev/null; "
    "setsid sh -c 'sleep 2; echo mem > /sys/power/state' </dev/null >/dev/null 2>&1 &"
)


def sh(h, lab, vm, script, **kw):
    return h.vmlab("exec", "--user", "root", vm, "--", "sh", "-c", script, cwd=lab, **kw)


def boot_id(h, lab, vm) -> str:
    return sh(h, lab, vm, "cat /proc/sys/kernel/random/boot_id", timeout=60).out.strip()


def events(h, lab, since, name, vm):
    return [e for e in events_since(h, lab, since)
            if e.get("event") == name and (e.get("data") or {}).get("vm") == vm]


def run(h):
    try:
        _run(h)
    finally:
        for f in OWNED:
            if f not in h.results:
                h.ok(f, False, "not reached: the scenario stopped before this step")


def _run(h):
    with h.lab("sleep-lab") as lab:
        h.vmlab("up", cwd=lab, timeout=600)
        h.wait_ready(lab, "sleepy", "awake")

        # What the guest kernel offers. Without `mem` there is no S3 to take,
        # and both features say so rather than pass on a guest that never slept.
        probe = sh(h, lab, "sleepy", "echo state=$(cat /sys/power/state); "
                   "echo mem_sleep=$(cat /sys/power/mem_sleep)", check=False).out
        if "mem" not in (re.search(r"state=(.*)", probe) or [None, ""])[1]:
            for f in OWNED:
                h.skip(f, f"the guest kernel offers no S3: {probe.strip()!r}")
            return

        # -- vm.sleep: suspended, refused by name, woken by `vm start` -------
        def sleep_and_wake():
            before = boot_id(h, lab, "sleepy")
            t0 = datetime.now(timezone.utc).replace(tzinfo=None)
            sh(h, lab, "sleepy", SLEEP_NOW, timeout=60)
            h.wait_until(lambda: state_of(h, lab, "sleepy") == "suspended",
                         timeout=60, interval=1, what="sleepy to report suspended")
            row = next((l for l in h.vmlab("status", cwd=lab).out.splitlines()
                        if l.split()[:1] == ["sleepy"]), "")
            refused = sh(h, lab, "sleepy", "true", check=False, timeout=60)
            slept = events(h, lab, t0, "vm.suspended", "sleepy")
            assert "suspended" in row, f"status row: {row!r}"
            assert refused.code == 5 and "suspended" in refused.text and "vm start" in refused.text \
                and "prevent_sleep" in refused.text, f"exec on a sleeping guest: exit {refused.code}: {refused.text.strip()}"
            assert slept, "no vm.suspended event"

            started = h.vmlab("vm", "start", "sleepy", cwd=lab, check=False, timeout=60)
            assert started.code == 0, started.text
            st = state_of(h, lab, "sleepy")
            after = h.wait_until(lambda: (lambda r: r.out.strip() if r.code == 0 else None)(
                sh(h, lab, "sleepy", "cat /proc/sys/kernel/random/boot_id", check=False, timeout=60)),
                timeout=60, interval=2, what="sleepy to answer exec after the wake")
            woke = events(h, lab, t0, "vm.woken", "sleepy")
            assert st == "running", f"after vm start: state={st}"
            assert after == before, f"boot_id {before} -> {after}: rebooted, not woken"
            assert woke and woke[0]["data"].get("cause") == "start", f"vm.woken: {woke}"
            return (f"status row {' '.join(row.split())!r}; exec refused with exit 5: "
                    f"{refused.text.strip()[:90]!r}...; `vm start` woke it (same boot_id, vm.woken cause=start)")

        try:
            detail = sleep_and_wake()
            h.ok("vm.sleep", True, detail)
        except (AssertionError, ScenarioFailed) as e:
            h.ok("vm.sleep", False, str(e))

        # -- vm.prevent_sleep: woken on the spot, and recorded ----------------
        def kept_awake():
            before = boot_id(h, lab, "awake")
            t0 = datetime.now(timezone.utc).replace(tzinfo=None)
            sh(h, lab, "awake", SLEEP_NOW, timeout=60)
            woke = h.wait_until(lambda: events(h, lab, t0, "vm.woken", "awake"),
                                timeout=60, interval=1, what="awake to be woken")
            slept = events(h, lab, t0, "vm.suspended", "awake")
            gap = (woke[0]["_when"] - slept[0]["_when"]).total_seconds() if slept else None
            time.sleep(1)
            st = state_of(h, lab, "awake")
            after = h.wait_until(lambda: (lambda r: r.out.strip() if r.code == 0 else None)(
                sh(h, lab, "awake", "cat /proc/sys/kernel/random/boot_id", check=False, timeout=60)),
                timeout=30, interval=2, what="awake to answer exec")
            assert slept and slept[0]["data"].get("prevent_sleep") is True, f"vm.suspended: {slept}"
            assert woke[0]["data"].get("cause") == "prevent_sleep", f"vm.woken: {woke}"
            assert gap is not None and gap < 5, f"woken {gap}s after it slept"
            assert st == "running", f"state={st}"
            assert after == before, f"boot_id {before} -> {after}"
            return (f"vm.suspended (prevent_sleep=true) then vm.woken {gap:.2f}s later: "
                    f"{woke[0]['data'].get('message')!r}; running, same boot_id")

        try:
            h.ok("vm.prevent_sleep", True, kept_awake())
        except (AssertionError, ScenarioFailed) as e:
            h.ok("vm.prevent_sleep", False, str(e))
