"""The end-to-end harness: run vmlab, record what each feature did.

Every scenario reports against the catalogue in `features.py`. A feature is
passed, failed or skipped (with a reason); one no scenario touched is reported
as missing, and a missing feature fails the run as surely as a failed one.

A run is on one hypervisor (PRD §22). On any but QEMU, a feature outside the
phases that hypervisor has delivered is skipped with the phase as its reason,
whatever the scenario reported, and is not missing when no scenario ran it.
"""

from __future__ import annotations

import contextlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import time
from dataclasses import dataclass

from features import FEATURES, HOST, PHASE_ORDER, PHASES, QEMU_SCENARIOS, VMLAB_DELIVERED, out_of_scope

E2E = pathlib.Path(__file__).resolve().parent
WORK = pathlib.Path(os.environ.get("E2E_WORK", "/work"))
RESULTS = pathlib.Path(os.environ.get("E2E_RESULTS", "/results"))


def redact_osk(text: str) -> str:
    """Hide device options, including keys containing printable whitespace."""
    return re.sub(r"osk=[^\r\n\x00]*", "osk=[REDACTED]", text)


class ScenarioFailed(Exception):
    """A step a scenario cannot continue past."""


@dataclass
class Result:
    argv: list[str]
    code: int
    out: str
    err: str

    @property
    def text(self) -> str:
        return self.out + self.err


class Harness:
    def __init__(self, keep: bool = False, hypervisor: str = "qemu", delivered: str = VMLAB_DELIVERED):
        self.keep = keep
        # Which hypervisor the run's machines are on, and on vmlab-hv the
        # last phase it has delivered.
        self.hypervisor = hypervisor
        # Whether the scenario running now has its VMs on the hypervisor
        # under test (`run.py` sets it per scenario).
        self.vms_here = hypervisor == "qemu"
        self.delivered = delivered
        self.results: dict[str, tuple[str, str]] = {}
        RESULTS.mkdir(parents=True, exist_ok=True)
        WORK.mkdir(parents=True, exist_ok=True)
        self.log = open(RESULTS / "e2e.log", "a", buffering=1)
        self.scenario = "-"

    # -- running things ------------------------------------------------------

    def run(
        self,
        argv: list[str],
        cwd: pathlib.Path | None = None,
        timeout: float = 600,
        check: bool = True,
        input: str | None = None,
        env: dict[str, str] | None = None,
    ) -> Result:
        """Run a command, log it and its output, and fail the step on a
        nonzero exit when `check` is set."""
        self.log.write(redact_osk(f"\n[{self.scenario}] $ {' '.join(argv)}  (cwd={cwd or WORK})\n"))
        full_env = dict(os.environ, **(env or {}))
        try:
            p = subprocess.run(
                argv,
                cwd=cwd or WORK,
                input=input,
                capture_output=True,
                text=True,
                timeout=timeout,
                env=full_env,
            )
            r = Result(argv, p.returncode, p.stdout, p.stderr)
        except subprocess.TimeoutExpired as e:
            out = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
            err = (e.stderr or b"").decode() if isinstance(e.stderr, bytes) else (e.stderr or "")
            r = Result(argv, 124, out, err + f"\n[timed out after {timeout}s]")
        r.out = redact_osk(r.out)
        r.err = redact_osk(r.err)
        r.argv = [redact_osk(arg) for arg in r.argv]
        self.log.write(r.out)
        if r.err:
            self.log.write("[stderr] " + r.err)
        self.log.write(f"[exit {r.code}]\n")
        if check and r.code != 0:
            raise ScenarioFailed(f"`{' '.join(r.argv)}` exited {r.code}: {r.text.strip()[-800:]}")
        return r

    def vmlab(self, *args: str, **kw) -> Result:
        return self.run(["vmlab", *args], **kw)

    def pty(self, command: str, keys: str, cwd: pathlib.Path | None = None, timeout: float = 120) -> Result:
        """Run an interactive command under a pseudo-terminal, typing `keys`."""
        return self.run(
            ["script", "-qec", command, "/dev/null"],
            cwd=cwd,
            input=keys,
            timeout=timeout,
            check=False,
        )

    def background(self, argv: list[str], cwd: pathlib.Path | None = None) -> subprocess.Popen:
        self.log.write(redact_osk(f"\n[{self.scenario}] $ {' '.join(argv)} &\n"))
        return subprocess.Popen(
            argv,
            cwd=cwd or WORK,
            stdout=self.log,
            stderr=subprocess.STDOUT,
            text=True,
        )

    def wait_until(self, check, timeout: float = 120, interval: float = 1.0, what: str = "condition"):
        """Poll `check` until it returns something truthy, and return that."""
        deadline = time.monotonic() + timeout
        while True:
            got = check()
            if got:
                return got
            if time.monotonic() > deadline:
                raise ScenarioFailed(f"timed out after {timeout}s waiting for {what}")
            time.sleep(interval)

    def machine_line(self, lab: pathlib.Path, machine: str) -> str:
        """The `status -v` detail line for one machine (`state=… ready=…`)."""
        lines = self.vmlab("status", "-v", cwd=lab, check=False).out.splitlines()
        for i, line in enumerate(lines):
            cols = line.split()
            if cols and cols[0] == machine and i + 1 < len(lines):
                return lines[i + 1]
        return ""

    def wait_ready(self, lab: pathlib.Path, *machines: str, timeout: float = 300) -> None:
        """`up` returns once machines are started; readiness is the agent
        handshake, which a caller waits for the way a user would."""
        for m in machines:
            self.wait_until(
                lambda m=m: "ready=yes" in self.machine_line(lab, m),
                timeout=timeout,
                interval=2,
                what=f"{m} to report ready",
            )

    # -- labs ----------------------------------------------------------------

    def destroy(self, lab: pathlib.Path) -> None:
        """`vmlab destroy`, which returns only once the lab's daemon is gone."""
        self.run(["vmlab", "destroy"], cwd=lab, check=False, timeout=300)

    @contextlib.contextmanager
    def lab(self, name: str, under: str = ""):
        """A fresh copy of `labs/<name>` under the work directory (inside
        `under`, when given), destroyed afterwards unless the run keeps its
        labs."""
        src = E2E / "labs" / name
        dst = WORK / under / name
        if dst.exists():
            self.destroy(dst)
            shutil.rmtree(dst)
        shutil.copytree(src, dst)
        try:
            yield dst
        finally:
            if not self.keep:
                self.destroy(dst)

    # -- hypervisor ----------------------------------------------------------

    def out_of_scope(self, feature: str) -> str | None:
        """Why `feature` does not apply to this run's hypervisor, or None."""
        return out_of_scope(feature, self.hypervisor, self.delivered)

    def seabios_template(self) -> bool:
        """Whether the suite's VM template is built for SeaBIOS: on vmlab-hv
        delivered through phase 2a or 2b, which boot SeaBIOS and not yet OVMF.
        From 2c it is the UEFI one, as on QEMU."""
        return self.hypervisor != "qemu" and self.delivered in ("2a", "2b")

    def vms_follow(self, features, scenario: str = "") -> bool:
        """Whether a scenario's VMs run on vmlab-hv: it covers a VM feature of
        a delivered phase (2a onwards). A scenario whose VM features are all
        later keeps its VMs on QEMU, where a lab declaring OVMF or a TPM still
        validates, and its features are skipped by phase."""
        if self.hypervisor == "qemu" or scenario in QEMU_SCENARIOS:
            return False
        lo, hi = PHASE_ORDER.index("2a"), PHASE_ORDER.index(self.delivered)
        return any(
            f in PHASES and lo <= PHASE_ORDER.index(PHASES[f]) <= hi and not self.out_of_scope(f)
            for f in features
        )

    def host_config(self, vms: bool = False) -> str:
        """The `host {}` lines that put this run's machines on its hypervisor:
        containers always, VMs where `vms` says (`vms_follow`)."""
        if self.hypervisor == "qemu":
            return ""
        kinds = ["container"]
        if vms:
            kinds.append("vm")
        pins = " ".join(f'{k} = "{self.hypervisor}"' for k in kinds)
        return f"  hypervisor {{ {pins} }}\n"

    # -- recording -----------------------------------------------------------

    def _record(self, feature: str, status: str, detail: str) -> None:
        if feature not in FEATURES:
            raise KeyError(f"feature {feature!r} is not in the catalogue")
        why = self.out_of_scope(feature)
        if why:
            # The scenario ran it, but not on the hypervisor under test.
            status, detail = "skip", why
        elif (self.hypervisor != "qemu" and not self.vms_here
              and PHASES.get(feature, HOST) not in (HOST, "1")):
            # A VM feature from a scenario whose VMs ran on QEMU.
            status, detail = "skip", f"not on vmlab-hv yet: {QEMU_SCENARIOS.get(self.scenario, 'VMs on QEMU')}"
        prior = self.results.get(feature)
        # A failure is never overwritten by a later pass of the same feature,
        # nor a pass by a skip from a scenario that could not run it.
        if prior and prior[0] == "fail" and status != "fail":
            return
        if prior and prior[0] == "pass" and status == "skip":
            return
        self.results[feature] = (status, detail)
        self.log.write(f"[{self.scenario}] {status.upper()} {feature}: {detail}\n")

    def ok(self, feature: str, cond: bool, detail: str = "") -> bool:
        self._record(feature, "pass" if cond else "fail", detail)
        return cond

    def skip(self, feature: str, reason: str) -> None:
        self._record(feature, "skip", reason)

    def check(self, feature: str, step, detail: str = "") -> bool:
        """Run `step`; the feature passes when it returns without raising and
        its result is not False."""
        try:
            got = step()
        except (ScenarioFailed, AssertionError, subprocess.SubprocessError, OSError) as e:
            return self.ok(feature, False, f"{detail}: {e}" if detail else str(e))
        return self.ok(feature, got is not False, detail)

    # -- report --------------------------------------------------------------

    def report(self) -> int:
        rows = []
        failed = 0
        for fid, desc in FEATURES.items():
            why = self.out_of_scope(fid)
            unreported = ("skip", why) if why else ("missing", "no scenario exercised it")
            status, detail = self.results.get(fid, unreported)
            if status in ("fail", "missing"):
                failed += 1
            rows.append({"feature": fid, "description": desc, "hypervisor": self.hypervisor,
                         "status": status, "detail": detail})
        (RESULTS / "results.json").write_text(json.dumps(rows, indent=2))
        width = max((len(r["feature"]) for r in rows), default=0)
        counts: dict[str, int] = {}
        for r in rows:
            counts[r["status"]] = counts.get(r["status"], 0) + 1
            if r["status"] != "pass":
                print(f"{r['status'].upper():8} {r['feature']:<{width}}  {r['detail'][:160]}")
        scope = f"vmlab-hv through phase {self.delivered}" if self.hypervisor == "vmlab" else self.hypervisor
        print("\n" + ", ".join(f"{k}: {v}" for k, v in sorted(counts.items())) + f" (of {len(rows)}) on {scope}")
        return 1 if failed else 0
