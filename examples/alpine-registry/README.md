# alpine-registry — pull a template from an OCI registry on `up`

A one-VM lab whose template is referenced by **OCI registry ref** instead of a
local store ref. It demonstrates vmlab's on-demand template distribution
(PRD §6.4): there is no `vmlab template build` or `vmlab template pull` step —
the template is fetched from the registry the first time you bring the lab up.

The VM block points at a published template:

```wcl
vm "alp" {
  template = "ghcr.io/wiltaylor/alpine-3.23/3.23.4"
  arch     = "x86_64"
  ...
}
```

The ref is `host/owner/name/version` (the version is the final path segment,
not a `:tag`). On `up`, the per-lab daemon checks the local store; if the
template is absent it pulls it from the registry, installs it, and boots from
it. Subsequent `up`s reuse the now-cached store copy.

## Run

```sh
vmlab up                        # first run pulls the template, then boots
ssh vmlab@localhost -p 12222    # password: vmlab
vmlab down
```

The first `up` takes longer than usual because of the pull (a few tens of
seconds for this ~small Alpine template, depending on your connection).
`vmlab status` shows the VM with its registry ref under `TEMPLATE`.

## Cleaning up the cached template

The pull leaves the template in your local store. To force the next `up` to
pull again:

```sh
vmlab template rm x86_64/alpine-3.23@3.23.4 --force
```
