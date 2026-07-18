---
name: verify
description: How to build, launch, and drive vmlab surfaces for runtime verification — the web console (vmlab-web + playwright-cli), the CLI, and scratch labs.
---

# Verifying vmlab changes at runtime

## Web console (web/ + web-ui/)

Build and serve:

```bash
just web-build                 # web-ui dist + help book + vmlab-web binary
cargo build                    # the `vmlab` binary vmlab-web spawns daemons with
cd <lab-dir> && /path/to/target/debug/vmlab-web --port 7899   # loopback ⇒ no auth
```

- Debug builds serve rust-embed assets **from disk** (no `debug-embed`
  feature), so web-ui CSS/JS edits only need `just web-ui-build` — no
  vmlab-web rebuild or restart.
- The supervisor is shared and long-lived: `/api/labs` lists every lab ever
  registered on this host. The UI opens on whatever lab it last had; switch
  labs via the banner lab-picker button → menuitem.

Drive it with `playwright-cli` — **always use a named session**
(`playwright-cli -s=<name> open http://127.0.0.1:7899/`): the `default`
session is shared with other agents on this host and gets navigated away
mid-run. Screenshots land in the CWD (repo root) — delete them before commit.

## Scratch labs

Make a lab dir in the scratchpad with a `vmlab.wcl`, then `vmlab up` from it —
it auto-registers with the supervisor. The cheapest guest is an OCI container
(e.g. `nginx:1.27` micro-VM, ~15 s to ready, no template build); needs the
guest assets in `~/.local/share/vmlab/guest/x86_64/` (usually already
installed). When done: `vmlab destroy` from the lab dir, otherwise the lab
lingers in the supervisor registry forever.

## Gotchas

- `just web-serve` defaults to `examples/mixed-lab`; pass `dir=` for another lab.
- Stopping a container from the UI keeps labd running (status still served,
  machines show stopped); `vmlab down` kills labd (status goes null).
- Seen 2026-07-18: a stray 502 on `/api/labs/<lab>/templates` in the browser
  console right after switching labs — appeared unrelated to the change under
  test; worth a look if it recurs.
