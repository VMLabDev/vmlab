# dev-container — a dev machine on a Linux container micro-VM

This example is PRD §19.8's worked example. A dev machine is a machine with a
synced workspace. Here that machine is a Linux container, and the example
shows three things:

- **The container identity floor.** `login "dev"` declares an account with no
  password, because the agent is root and root needs no credential to become
  an account.
- **Provisioning into a login's home before first logon.** A `provision {}`
  step writes `~/.profile` as `dev`, through `as_login`, before that account
  has ever logged on.
- **The workspace syncer.** `./workspace` on the host and `/src` in the guest
  stay in step both ways.

## Prerequisites

None. A container is pulled like a docker image and run inside a micro-VM, so
there is no template to build. It is a lab machine in every respect: the same
segments, DNS, snapshots, agent channel and workspace syncer as a VM.
`alpine:3.22` is pulled on the first `vmlab up`.

## Run it

1. Bring the lab up:

   ```sh
   vmlab up
   vmlab status        # dev01 ready
   ```

2. Open a shell in the machine:

   ```sh
   vmlab shell dev01
   ```

   The shell runs as `dev`, the machine's default login, over the agent
   channel. No guest network is involved. The `~/.profile` that the provision
   placed puts you in `/src`.

3. Edit a file on either side and watch it arrive on the other. In the guest:

   ```sh
   echo "edited in the guest" >> hello.txt
   ./build.sh          # writes /src/out.txt as dev
   ```

   Within a second, `./workspace/hello.txt` on the host has the new line, and
   `./workspace/out.txt` exists. Edit `./workspace/hello.txt` on the host and
   `cat /src/hello.txt` in the guest shows the change.

4. Check the syncer's state from the host:

   ```sh
   vmlab dev sync status
   ```

   `dev sync` picks its machine from the command argument, then
   `VMLAB_DEV_MACHINE`, then the lab's default `@dev` machine. This lab has
   one, `dev01`, so no argument is needed.

## What this example demonstrates

### Writing into a home before its owner has logged on

> A `provision {}` step can address the dev login's home directory **before
> that user has ever logged on.**

Everything personal lives in a per-user home directory. `~/.profile` belongs
to `dev`, and the agent is root. A file the agent writes there is root-owned.

`scripts/home-bits.ws` fixes that with one line:

```rust
let dev = dev01.as_login("dev")?
```

`dev` is a second handle onto the same machine, and every call on it (`exec`,
`copy_to`, a terminal) runs as the `dev` login. On a container this costs
nothing. §19.2's **container identity floor** is that the agent is root, so
`login "dev"` in `vmlab.wcl` declares the account alone, with no `password`.
Declaring `elevated` on a Linux login is a validation error.

Remove the line and `~/.profile` is root-owned in `dev`'s home. The script
checks `id -un` and fails if it is not `dev`.

### Provision, never playbook

A `playbook {}` runs config-weave in the guest with no user parameter, and it
has no rung on §19.2's precedence ladder. It would write these paths as root.

> **Anything that must land as the developer rather than as the machine
> belongs in `provision {}`.**

### The durability rule

> **Bake what the lab needs every developer to have. Hand-install what you
> personally want today, and expect to redo it after a rebuild.**

`~/.profile` is under the guest home, outside the workspace. It survives a
reboot, `down`/`up`, and a restore to a snapshot taken after it landed. It is
lost on a per-machine `destroy` + `up`, and comes back because it is a
*declaration*:

```sh
vmlab container destroy dev01
vmlab up                       # the fresh micro-VM is provisioned again
vmlab shell dev01              # the same ~/.profile, the same /src
```

The workspace is intact across that. It is a guest-local copy of a canonical
host tree (ADR-0014), and `./workspace` on the host never moved. Only a
hand-install does not come back.

Baking the same files into a custom image is the other declared placement. Use
it when every developer needs the files without a per-lab provision.

## Guest credentials

`dev`, with no password. That is the container identity floor, not an
oversight. `vmlab shell`, `vmlab exec` and the workspace syncer all run as it,
because it is the machine's only declared login and therefore its default. `--user root` on `shell` or `exec` gives the agent identity.

## What each file is

| Path | What it does |
|---|---|
| `vmlab.wcl` | The lab: one segment with egress, and `@dev` dev01 with one login |
| `scripts/dev-user.ws` | The packages and the account, **as the machine** |
| `scripts/home-bits.ws` | Places `~/.profile` **as `dev`**, through `as_login` |
| `scripts/profile` | The dotfile copied into `dev`'s home |
| `workspace/` | The host side of `/src`, the only thing here that is not a declaration |
