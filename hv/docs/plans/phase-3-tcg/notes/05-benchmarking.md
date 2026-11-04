---
id: "05"
decision: D2
---

# Continuous benchmarking from the start

## Question
How is performance tracked so the JIT can be iterated on toward and past D1's bar?

## Answer
Benchmarking is built in from phase 3's first milestone, not added at the end:
- A benchmark suite in the repo (`bench/` plus a `just` recipe) that runs the same guest workloads under vmlab-hv and under QEMU TCG on the same host: boot-to-login, the in-guest CPU benchmarks from notes/03, and micro-benchmarks of the emulator itself (interpretation and translation throughput, TLB hit rate, block cache size and invalidations, JIT compile time).
- Every run writes a machine-readable result (commit, host, guest, workload, vCPUs, numbers, QEMU baseline) to an append-only history, so a change's effect is a diff against the previous run and regressions are visible.
- A comparison report and a regression check: a change that slows any workload beyond a noise threshold is flagged before merge (run on this host, since GitHub runners lack the templates and stable timing).
- Profiling hooks in vmlab-hv: per-block execution counts and hot-block dumps, JIT code exported to `perf` via perf map files, and a switch to run interpreter-only for comparison.

## Why
Owner's requirement (2026-10-31): D1's 100% bar is met by iteration, which needs repeatable measurements recorded over time.

## Consequences
- Benchmark noise control (CPU pinning, governor, repeats, statistics) is part of the suite's design.
- Where the history lives (in the repo vs a separate store) is decided in the spec; recommendation: a JSON-lines file in the repo under `bench/history/`, small enough to commit.
