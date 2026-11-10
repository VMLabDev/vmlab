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
  full checkout (no `--depth`: `win3d-check` needs the history). Every heavy
  run starts with `terrarium run` through `/var/build/cache/vmlab/bin/heavy`,
  which admits two shared slots, sets `CARGO_BUILD_JOBS=8`, and applies the
  24 GiB scope and low scheduling priority. Set `VMLAB_SKIP_UPDATES=1`.
- Who runs it (operator, 2026-11-07): the worker runs it itself on its
  target against its pushed commit before it moves the card to `review`, and
  puts the command, the commit, the exit code and the pass count in the move
  comment. There is no `validating` column and no `terrarium card validate`
  for card work. Persistent target dirs: set
  `VMLAB_TARGET_ROOT=/var/build/cache/vmlab/<you>` so a rerun is fast. Validate
  from a vmlab-owned copy of the pushed checkout at `$VMLAB_TARGET_ROOT/src`.
  Update it with `rsync -a --delete --exclude target/` and restore the
  executable bits of tracked mode-100755 files before running the gate.
  This also redirects CI reports, virgl, hv iPXE and rebuilt BPF objects.
  A read-only checkout owned by another user needs command-local Git trust
  (inherited by Git subprocesses), not a global safe.directory change:

  ```sh
  GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory \
    GIT_CONFIG_VALUE_0=/absolute/path/to/checkout \
    VMLAB_TARGET_ROOT=/var/build/cache/vmlab/<you> just ci::check
  ```
- The merge queue runs the same command on each batch merged onto `next`
  (one `terrarium card merge A B C ...` job), sharing the heavy wrapper slots.
- Pass: every recipe of `ci::check` exits 0. Record the log's pass, fail and
  ignored counts. Upload native machine-readable reports with
  `terrarium test push --card <id>`. Do not fabricate reports or convert counts
  into test cases. Retain the full gate log for steps without reports.
  The main crate and hv workspace write raw libtest JSON to
  `$VMLAB_TARGET_ROOT/test-results/main.json` and `hv.json` (under
  `target/test-results` if unset). Each test step prints its path even on
  failure, and a successful check prints both paths at the end. Push both:
  `terrarium test push <path> --format libtest-json --name <main-or-hv> --card <id>`.
  Push failed runs before rerunning too. Copy evidence from the target to
  your workspace when it is outside the shared mount. These two files cover
  the main and hv test steps, not the separate guest/conformance checks.
  The target runner enables JSON only for test binaries. Doctests still run
  separately on stable rustdoc without the runner, with text output included in cargo's exit status but not
  in the uploaded case counts.
  VML-114 tracks native reports for the remaining steps.
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

## Release version

- `scripts/release-version.sh` currently derives the bump from conventional
  commits, not the `bump:` trailer. Do not rely on that trailer to select a
  version. Until trailer support lands, the lead must check the script's
  calculated version against the intended version before publishing.

## Release gate

- `scripts/release-version.sh` honors HEAD's `bump: major|minor|patch`
  trailer, falling back to commit subjects without it. A 0.x major bumps
  minor. The `release: true` promotion of an alpha keeps its version,
  regardless of a bump trailer.
- Run `ci::hv-gate` first, then one full green e2e on the exact release
  commit that will merge to `main`. The main merge must have the same tree.
  Otherwise record a written waiver for each failed scenario on the release
  card. A partial run or a green run on another tree is not release evidence.
- Do not cut another version while a cut version waits for its e2e.
- Reserve the host during release e2e. No other heavy vmlab runs may overlap.
  Use the heavy wrapper hold file and drain both slots as described in
  `docs/agents/release-gate.md`. All heavy runs start with `terrarium run`
  through `/var/build/cache/vmlab/bin/heavy`.
- Follow the exact-tree evidence checklist in `docs/agents/release-gate.md`
  before merging. Signing and GitHub publication remain main-only.
