"""Run the vmlab end-to-end suite inside the e2e container.

    run.py [--only SCENARIO ...] [--keep] [--list]
           [--hypervisor qemu|vmlab] [--vmlab-phase PHASE]

Scenarios run in order; each reports against the feature catalogue. The run
fails when any feature failed or no scenario covered it.

`--hypervisor vmlab` puts the suite's machines on vmlab-hv through the host
config's default (PRD §22). A scenario with no feature inside the phases it
has delivered (`--vmlab-phase`, default in `features.py`) does not run, and
its features are skipped naming their phase.
"""

from __future__ import annotations

import argparse
import importlib
import os
import pathlib
import subprocess
import sys
import time
import traceback

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from features import FEATURES, PHASE_ORDER, VMLAB_DELIVERED  # noqa: E402
from harness import RESULTS, Harness, ScenarioFailed  # noqa: E402

# Order matters: later scenarios use the template `template` builds.
SCENARIOS = [
    "template",
    "variants",
    "core",
    "firmware",
    "sleep",
    "guest",
    "network",
    "containers",
    "snapshots",
    "worktree",
    "scripting",
    "record",
    "playbooks",
    "dev",
    "worktree_clone",
    "apps",
    "peers",
    "admin",
    "fastpath",
    "unsupported",
]


def start_registry() -> subprocess.Popen:
    """A plain registry on localhost:5000, which vmlab talks to over HTTP."""
    config = pathlib.Path("/tmp/registry.yml")
    config.write_text(
        "version: 0.1\n"
        "storage:\n  filesystem:\n    rootdirectory: /var/lib/registry\n"
        "http:\n  addr: 127.0.0.1:5000\n"
    )
    log = open(RESULTS / "registry.log", "w")
    proc = subprocess.Popen(["registry", "serve", str(config)], stdout=log, stderr=subprocess.STDOUT)
    for _ in range(50):
        if subprocess.run(["curl", "-sf", "http://127.0.0.1:5000/v2/"], capture_output=True).returncode == 0:
            return proc
        time.sleep(0.2)
    raise SystemExit("registry did not start")


def scenario_features(name: str) -> set[str]:
    """The features a scenario names, which are the ones it reports."""
    text = (pathlib.Path(__file__).resolve().parent / "scenarios" / f"{name}.py").read_text()
    return {fid for fid in FEATURES if f'"{fid}"' in text}


def write_host_config(h: Harness, vms: bool = False) -> None:
    """The host config the run's supervisor and lab daemons read: only what
    selects the hypervisor, and nothing at all on QEMU, which is the default.
    Rewritten per scenario, since whether VMs follow is the scenario's."""
    lines = h.host_config(vms)
    if not lines:
        return
    path = pathlib.Path(os.environ.get("XDG_CONFIG_HOME", pathlib.Path.home() / ".config")) / "vmlab" / "config.wcl"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"import <vmlab-host.wcl>\n\nhost {{\n{lines}}}\n")
    h.log.write(f"[setup] {path}:\n{path.read_text()}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", nargs="*", default=None)
    ap.add_argument("--keep", action="store_true", help="leave labs up for inspection")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--hypervisor", choices=["qemu", "vmlab"], default="qemu",
                    help="the hypervisor the suite's machines run on (default qemu)")
    ap.add_argument("--vmlab-phase", choices=PHASE_ORDER[1:], default=VMLAB_DELIVERED,
                    help=f"the last vmlab-hv phase delivered (default {VMLAB_DELIVERED})")
    args = ap.parse_args()

    if args.list:
        print("\n".join(SCENARIOS))
        return 0
    chosen = args.only or SCENARIOS
    unknown = set(chosen) - set(SCENARIOS)
    if unknown:
        raise SystemExit(f"unknown scenario(s): {', '.join(sorted(unknown))}")

    h = Harness(keep=args.keep, hypervisor=args.hypervisor, delivered=args.vmlab_phase)
    # On vmlab-hv, a scenario none of whose features apply yet does not run;
    # the report skips its features by phase.
    runnable = [n for n in chosen if any(not h.out_of_scope(f) for f in scenario_features(n))]
    for name in chosen:
        if name not in runnable:
            print(f"== {name}\n   skipped: no feature in scope on {args.hypervisor}", flush=True)
    write_host_config(h)
    registry = start_registry()
    try:
        if "template" not in runnable:
            # Every other scenario boots the e2e template; build it without
            # reporting on it.
            h.scenario = "base"
            print("== base template", flush=True)
            importlib.import_module("scenarios.template").ensure_base(h)
        for name in SCENARIOS:
            if name not in runnable:
                continue
            mod = importlib.import_module(f"scenarios.{name}")
            h.scenario = name
            h.vms_here = h.hypervisor == "qemu" or h.vms_follow(scenario_features(name), name)
            write_host_config(h, vms=h.vms_here)
            started = time.monotonic()
            print(f"== {name}", flush=True)
            try:
                mod.run(h)
                verdict = "done"
            except ScenarioFailed as e:
                verdict = f"aborted: {e}"
            except Exception:  # a harness bug must not hide the rest
                verdict = "crashed:\n" + traceback.format_exc()
            h.log.write(f"[{name}] {verdict}\n")
            print(f"   {verdict.splitlines()[0]} ({time.monotonic() - started:.0f}s)", flush=True)
    finally:
        registry.terminate()
        subprocess.run(["vmlab", "daemon", "stop"], capture_output=True)
    if args.only:
        # A partial run expects exactly the features its scenarios name, so a
        # feature one of them skipped still reports as missing.
        named = set().union(*(scenario_features(n) for n in chosen))
        for fid in list(FEATURES):
            if fid not in named:
                FEATURES.pop(fid)
    code = h.report()
    give_back_results()
    return code


def give_back_results() -> None:
    """The container runs as root; hand the results to whoever owns the
    mounted results directory, so a run leaves nothing root-owned behind."""
    owner = RESULTS.stat()
    for path in [RESULTS, *RESULTS.rglob("*")]:
        try:
            os.chown(path, owner.st_uid, owner.st_gid)
        except OSError:
            pass


if __name__ == "__main__":
    sys.exit(main())
