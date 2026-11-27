#!/usr/bin/env python3
"""Fail when the latest run regressed beyond measured noise (D2).

For every (arch, backend, guest, vcpus, workload) with at least two history
entries, the latest median must not move beyond the measured noise in the
workload's worse direction: the larger of the two runs' spreads, or 5 % of
the previous median. Rate workloads (iops, MiB/s, Mbit/s) regress downward;
seconds and percentages regress upward. Comparisons are within one backend;
the phase bars (D1 TCG, D28 x86_64) are reported by `just bench-report`,
not gated here.

The aarch64 rows of vmlab-hv are compared within their JIT mode (3e
DESIGN.md card J8): a JIT row with the JIT row before it, an interpreter
row (`VMLAB_HV_JIT=0`, and every row from before the JIT) with the
interpreter row before it. A row of K3 (`VMLAB_HV_JIT_CHECK`) measures the
check and is not gated. A JIT row also fails when more of its blocks ran in
the interpreter for a missing form (H2b) than in the JIT row before it.
"""

import argparse
import sys

import lib
ARCHES = ("aarch64", "riscv64", "x86_64")


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--arch", default="all", help="all, or aarch64,riscv64")
    p.add_argument("--backend", default="all", help="all, qemu, or vmlab-hv")
    p.add_argument("--guest", default=None, help="filter by template name")
    args = p.parse_args(argv)
    if args.guest == "all":
        args.guest = None
    return args


def compare(arch, args):
    """(regressions, notes): one line per config with two entries or fewer."""
    records = lib.read_history(arch)
    if args.backend != "all":
        records = [r for r in records if r["backend"] == args.backend]
    if args.guest:
        records = [r for r in records if r["guest"] == args.guest]
    grouped = {}
    for rec in records:
        mode = lib.jit_mode(rec)
        if mode == "jit-check":
            continue
        grouped.setdefault(
            (rec["backend"], rec["guest"], rec["vcpus"], rec["workload"], mode), []
        ).append(rec)

    regressions, notes = [], []
    for key in sorted(grouped):
        entries = grouped[key]
        name = f"{arch} {key[0]}{f' ({key[4]})' if key[4] else ''} {key[1]} vcpus={key[2]} {key[3]}"
        if len(entries) < 2:
            notes.append(f"{name}: only {len(entries)} entry, nothing to compare")
            continue
        previous, latest = entries[-2], entries[-1]
        verdict, allowed = lib.regression_verdict(previous, latest)
        unit = latest.get("unit", "s")
        line = (
            f"{name}: {previous['median']} -> {latest['median']} {unit} "
            f"(noise <= {allowed}) {verdict}"
        )
        if verdict == "worse":
            regressions.append(line)
        else:
            notes.append(line)
        if key[4] == "jit":
            before, after = lib.interpreted_blocks(previous), lib.interpreted_blocks(latest)
            if before is not None and after is not None and after > before:
                regressions.append(
                    f"{name}: {before} -> {after} blocks in the interpreter for a missing form (H2b)"
                )
    return regressions, notes


def main(argv=None):
    args = parse_args(argv)
    arches = list(ARCHES) if args.arch == "all" else [a.strip() for a in args.arch.split(",")]
    regressions = []
    for arch in arches:
        arch_regressions, notes = compare(arch, args)
        for note in notes:
            print(f"  ok   {note}")
        regressions.extend(arch_regressions)
        for line in arch_regressions:
            print(f"  FAIL {line}", file=sys.stderr)
    if regressions:
        print(
            f"\nbench-check: {len(regressions)} regression(s) beyond measured noise",
            file=sys.stderr,
        )
        return 1
    print("\nbench-check: no regression beyond measured noise")
    return 0


if __name__ == "__main__":
    sys.exit(main())
