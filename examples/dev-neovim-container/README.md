# dev-neovim-container — Neovim on a Linux container micro-VM

The second of PRD §19.8's two worked examples. Its twin is
[`../dev-vscode-windows`](../dev-vscode-windows), and the pair is split by
machine kind on purpose: *one contract, every machine kind* is shown rather
than stated. Two Windows examples would exercise the same code path twice; a
Windows VM and a Linux container exercise both identity floors, both home
conventions, and both machine kinds.

This is the structurally opposite half. No client/server split, no
marketplace, no minted domain logon — a TUI over the facade's own `session`
channel, plugins that are `git clone`s, and the container identity floor.

## Prerequisites

None. A container is pulled like a docker image and run inside a micro-VM, so
there is no template to build — and it is a lab machine in every respect: same
segments, DNS, snapshots, agent channel, SSH facade and workspace syncer as a
VM. `alpine:3.22` is pulled on first `vmlab up`.

## Run it

```sh
vmlab up
vmlab status        # dev01 ready, attachable
vmlab dev attach    # waits for `attachable`, prints the alias, opens a shell
```

Then, in that shell:

```sh
cd /src
nvim hello.lua      # a TUI, over the facade's own session channel
./build.sh          # writes /src/out.txt — it appears in ./workspace on the host
```

Or without attaching first, straight off the alias:

```sh
ssh vmlab-dev-neovim-container-dev01 -t nvim /src
```

`vmlab dev attach` launches no editor. Neovim *is* the terminal here, so the
handover is the shell itself — and the alias is still what any other client
(`scp`, `sftp`, an editor's file explorer) picks out of `~/.ssh/config`.

## What this example is actually demonstrating

### The same guarantee, on a machine with none of the machinery

> A `provision {}` step can address the dev login's home directory **before
> that user has ever logged on.**

Neovim looks like the easy case — nothing to download, nothing to install
remotely — and it lands on exactly the same blocker: **everything
editor-shaped lives in a per-user home directory.** `~/.config/nvim` and
`~/.local/share/nvim` belong to `dev`, and the agent is root.

`scripts/editor-bits.ws` is the same one line the Windows example uses:

```rust
let dev = dev01.as_login("dev")?
```

Here it costs nothing at all. §19.2's **container identity floor** is that the
agent is root, and root needs no credential to become an account — which is
why `login "dev"` in `vmlab.wcl` declares the account alone, with no
`password`, and why declaring `elevated` on this side is a validation error
rather than a default. (Its Windows twin cannot do that: every
credential-free route there is the one Windows OpenSSH's S4U logon already
disqualified.)

Take the line away and every path the script writes is root-owned inside
`dev`'s home, and nvim's first run fails on a directory it cannot write. The
script asserts on `id -un` for exactly that reason.

### Provision, never playbook

A `playbook {}` runs config-weave in-guest with no user parameter and has no
rung on §19.2's precedence ladder — it would write these paths as root and
half-work.

> **Anything that must land as the developer rather than as the machine
> belongs in `provision {}`.**

### The durability rule

> **Bake what the lab needs every developer to have; hand-install what you
> personally want today, and expect to redo it after a rebuild.**

`~/.config/nvim/init.lua` and `~/.local/share/nvim/site/pack/vmlab/start/` are
under the guest home, outside the workspace. So they survive reboot,
`down`/`up`, and restore to a snapshot taken after they landed. They die on a
per-machine `destroy` + `up` — and come back, because they are a
*declaration*:

```sh
vmlab container destroy dev01
vmlab up                       # the fresh micro-VM is provisioned again
vmlab dev attach
nvim /src/hello.lua            # same config, same plugin
```

The workspace is intact across that: it is a guest-local copy of a canonical
host tree (ADR-0014), and `./workspace` on the host never moved. Only a
hand-install does not come back — install a plugin by hand today and you redo
it after a rebuild. That is the rule, not a limitation to work around.

Baking the same bits into a custom image is the other declared placement, and
the right one when the lab wants every developer to have them without a
per-lab provision. This example uses the provision route so that the two
worked examples use the *same* route on both machine kinds.

### Where the bytes come from

Two routes, both in `scripts/editor-bits.ws`:

- **From the repo** — `config/init.lua` is `copy_to`'d under the `dev` logon,
  so the file is the developer's and not root's.
- **From the network** — the plugin is a `git clone`, over the segment's
  `nat = true` egress.

A lab that wants neither bakes the same bits into an image. vmlab does not
care which: **it moves bytes it is told to move and never interprets them.**

## Two things an offline guest has not taken away

Both are printed by `vmlab dev attach` and `vmlab ssh-config --print`, because
a developer who has just been handed an offline guest assumes both are gone.

**Personal config copies over the alias.** The facade answers `subsystem sftp`
host-side (§19.3), so this works with nothing built for it:

```sh
scp -r ~/.config/nvim vmlab-dev-neovim-container-dev01:.config/nvim
```

That is also the escape hatch for the durability rule: the declaration carries
what the lab needs everyone to have, and this line carries what is yours.

**A host-side service is reachable without a reverse tunnel.** `ssh -R` is
refused (§19.3) and is not what this needs. This lab's segment already has
`nat = true`; the NAT engine terminates guest flows in-process and proxies
them over ordinary host sockets, so anything addressed off-segment reaches the
host's own address — an `apk` mirror on the host, a proxy, a licence server.
The same one line is what makes the plugin clone reach GitHub.

## Guest credentials

`dev`, with no password — which is the container identity floor, not an
oversight. `vmlab dev attach`, `ssh vmlab-dev-neovim-container-dev01`, `scp`
and `vmlab exec` all land on it, because it is the machine's only declared
login and therefore its default.

## What each file is

| Path | What it does |
|---|---|
| `vmlab.wcl` | The lab: one segment with egress, and `@dev` dev01 with one login |
| `scripts/dev-user.ws` | The toolchain and the account — **as the machine**, which is right |
| `scripts/editor-bits.ws` | The point: places editor bits **as `dev`** |
| `config/init.lua` | The Neovim config, copied into `dev`'s home |
| `workspace/` | The host side of `/src` — the only thing here that is not a declaration |
