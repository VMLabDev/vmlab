"""Run the vmlab end-to-end suite inside the e2e container.

    run.py [--only SCENARIO ...] [--keep] [--list]

Scenarios run in order; each reports against the feature catalogue. The run
fails when any feature failed or no scenario covered it.
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

from harness import RESULTS, Harness, ScenarioFailed  # noqa: E402

# Order matters: later scenarios use the template `template` builds.
SCENARIOS = [
    "template",
    "core",
    "sleep",
    "guest",
    "network",
    "containers",
    "snapshots",
    "scripting",
    "playbooks",
    "dev",
    "peers",
    "admin",
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


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", nargs="*", default=None)
    ap.add_argument("--keep", action="store_true", help="leave labs up for inspection")
    ap.add_argument("--list", action="store_true")
    args = ap.parse_args()

    if args.list:
        print("\n".join(SCENARIOS))
        return 0
    chosen = args.only or SCENARIOS
    unknown = set(chosen) - set(SCENARIOS)
    if unknown:
        raise SystemExit(f"unknown scenario(s): {', '.join(sorted(unknown))}")

    registry = start_registry()
    h = Harness(keep=args.keep)
    try:
        if "template" not in chosen:
            # Every other scenario boots the e2e template; build it without
            # reporting on it.
            h.scenario = "base"
            print("== base template", flush=True)
            importlib.import_module("scenarios.template").ensure_base(h)
        for name in SCENARIOS:
            if name not in chosen:
                continue
            mod = importlib.import_module(f"scenarios.{name}")
            h.scenario = name
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
        from features import FEATURES

        here = pathlib.Path(__file__).resolve().parent / "scenarios"
        named = "\n".join((here / f"{n}.py").read_text() for n in chosen)
        for fid in list(FEATURES):
            if f'"{fid}"' not in named:
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
