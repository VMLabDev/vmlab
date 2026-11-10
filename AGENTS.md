# AGENTS.md

Instructions for agents that work in this repo. Project context, design rules
and conventions are in `CLAUDE.md`; read it too. Where the two differ, this
file wins for the board and validation flow.

## Validation

The one validation command for a card branch, a merge onto `next`, and a
release branch is:

```
just ci::check
```

- Target: `vmlab-helios-vmlab` (the vmlab account on helios). Run it in a
  full checkout (no `--depth`: `win3d-check` needs the history), under
  `systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0`, with
  `CARGO_BUILD_JOBS=4` (or less) and `VMLAB_SKIP_UPDATES=1`.
- Who runs it (operator, 2026-11-07): the worker runs it itself on its
  target against its pushed commit before it moves the card to `review`, and
  puts the command, the commit, the exit code and the pass count in the move
  comment. There is no `validating` column and no `terrarium card validate`
  for card work. Persistent target dirs: set
  `VMLAB_TARGET_ROOT=/var/build/cache/vmlab/<you>` so a rerun is fast.
- The merge queue runs the same command on each batch merged onto `next`
  (one `terrarium card merge A B C ...` job); its two slots serve merges only.
- Pass: every recipe of `ci::check` exits 0. Push the test results with
  `terrarium test push --card <id>`.
  The main crate and hv workspace write raw libtest JSON to
  `$VMLAB_TARGET_ROOT/test-results/main.json` and `hv.json` (under
  `target/test-results` if unset). Each test step prints its path even on
  failure, and a successful check prints both paths at the end. Push both:
  `terrarium test push <path> --format libtest-json --name <main-or-hv> --card <id>`.
  Push failed runs before rerunning too. Copy evidence from the target to
  your workspace when it is outside the shared mount. These two files cover
  the main and hv test steps, not the separate guest/conformance checks.
  The target runner enables JSON only for test binaries. Doctests still run
  on stable rustdoc with text output, included in cargo's exit status but not
  in the uploaded case counts.
- A known flaky test that fails once is rerun alone before the run counts as
  failed; name it on the card. Red again: fix it, or move the card to
  `blocked` with the output tail.
  The quarantine register and reporting rules are in
  `docs/agents/flaky-tests.md`. Quarantined failures stay in the evidence.
  A rerun without a named flaky test needs a reason comment first.
- Every shell script you write or run: `set -eu`, and refuse an empty or `/`
  path before `cp`, `rsync`, `mv` or `rm` (`just ci::shell-paths` checks the
  tracked ones).

## Branches

- Card branches start from `next` and are named after the card (`vml-<n>`).
- Only the lead merges onto `next`, `release/*` and `main`. No force pushes.
