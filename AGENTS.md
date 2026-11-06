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
- Slots: N = 2. At most two validation runs at once on the target; a third
  queues (`validation:queued`) until one ends.
- Pass: every recipe of `ci::check` exits 0. Record the exit code and the
  pass and fail counts on the card, and push the results with
  `terrarium test push`.
- A known flaky test that fails once is rerun alone before the run counts as
  failed; name it on the card.

## Branches

- Card branches start from `next` and are named after the card (`vml-<n>`).
- Only the lead merges onto `next`, `release/*` and `main`. No force pushes.
