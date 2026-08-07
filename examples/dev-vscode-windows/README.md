# dev-vscode-windows — VS Code on a Windows domain member

One of PRD §19.8's two worked examples. Its twin is
[`../dev-neovim-container`](../dev-neovim-container), and the pair is split by
machine kind on purpose: *one contract, every machine kind* is shown rather
than stated. Two Windows examples would exercise the same code path twice; a
Windows VM and a Linux container exercise both identity floors, both home
conventions, and both machine kinds.

This is the hard half. It has a client/server split, extensions under a
per-user profile that does not exist until someone logs on, a minted domain
logon, and a guest with no route off the segment.

## Prerequisites

Build the Windows template first (the example ships none of its own):

```sh
(cd ../templates/windows-server-2025 && ./fetch-deps.sh && vmlab template build)
```

Two 4-vCPU / 8 GiB Windows VMs boot here. The domain promotion and join each
reboot once, so first `vmlab up` takes a while; later ones do neither.

## Run it

```sh
vmlab up            # dc01 becomes probe.local; dev01 joins it and is provisioned
vmlab status        # dev01 ready, attachable
vmlab dev attach    # waits for `attachable`, prints the alias, opens a shell
```

`vmlab dev attach` launches no editor — it prints the alias your client picks
out of its own host list, plus the two client-side VS Code settings below. Own
nothing the editor will still need is the rule; the alias is the handover.

In VS Code: **Remote-SSH: Connect to Host…** → `vmlab-dev-vscode-windows-dev01`,
then open `C:\src`. Or from a shell:

```sh
code --remote ssh-remote+vmlab-dev-vscode-windows-dev01 C:\src
```

### The client-side half

The guest here has **no egress at all** — the `corp` segment declares no
`nat`. Remote-SSH still works, because this setting makes the *client*
download the ~12 MB server and push it over `scp`:

```jsonc
// VS Code settings.json
{
  "remote.SSH.localServerDownload": "always",
  "remote.SSH.remotePlatform": { "vmlab-dev-vscode-windows-dev01": "windows" }
}
```

Both keys are client-side, which is why vmlab hands them over rather than
configuring them: a dev machine cannot make itself offline-capable
unilaterally. `vmlab ssh-config --print dev01` and `vmlab dev attach` both
print this snippet with your own alias already in it.
`remote.SSH.remotePlatform` is the documented workaround for VS Code's
Windows host-detection bug.

Pre-staging the server into the template works mechanically but is keyed to
the *client's* build commit, so it dies on every editor update. The push route
is version-agnostic by construction.

### Edit, build, reach the domain share

`./workspace` on the host is `C:\src` in the guest, syncing both ways (§19.6).
From the editor's integrated terminal:

```powershell
cd C:\src
.\build.ps1
```

That prints the session's identity (`PROBE\dev`, not `SYSTEM`), reads
`\\dc01\team\README.txt` with no credential prompt, and writes `out.txt` —
which appears in `./workspace` on the host within a second.

The share needs no credential because the SSH facade's shell, its `sftp`
subsystem and `vmlab exec` all run under **one** minted logon for
`login "dev"` (§19.2). A session that had kept the agent's SYSTEM identity
would fail there, and fail confusingly.

## What this example is actually demonstrating

### The guarantee

> A `provision {}` step can address the dev login's home directory **before
> that user has ever logged on.**

`scripts/editor-bits.ws` is one line of this:

```rust
let dev = dev01.as_login("dev")?
```

That resolves the `login "dev"` block, mints `PROBE\dev`'s logon
(`LogonUser` + `LoadUserProfileW`), and hands back a second handle onto the
same machine. Every call on it — `exec`, `copy_to`, `terminal` — lands inside
the profile the mint just created.

Take that line away and the script still runs, still succeeds, and writes
every byte into `C:\Windows\system32\config\systemprofile`. §19.8 states the
guarantee precisely because nobody implementing §19 would infer it: §19.2's
headline is that vmlab's own machinery keeps the agent identity, which makes
"provisioning runs as the machine, full stop" the natural implementation — and
then this example silently fails. The script asserts on the profile path for
exactly that reason.

### Provision, never playbook

A `playbook {}` runs config-weave in-guest with no user parameter and has no
rung on §19.2's precedence ladder. That is not a total block, which is what
makes it dangerous: the agent identity *can* write into a profile directory
that already exists, but cannot create one or set ownership. So a playbook
half-works on an existing profile and fails on a fresh domain user — which is
the first-run case, and the only case this lab has.

> **Anything that must land as the developer rather than as the machine
> belongs in `provision {}`.**

### The durability rule

> **Bake what the lab needs every developer to have; hand-install what you
> personally want today, and expect to redo it after a rebuild.**

Everything `editor-bits.ws` places is under `%USERPROFILE%`, outside the
workspace. So it survives reboot, `down`/`up`, and restore to a snapshot taken
after it landed. It dies on a per-machine `destroy` + `up` — and comes back,
because it is a *declaration*:

```sh
vmlab vm destroy dev01
vmlab up                       # the fresh clone boots, joins, and is provisioned again
vmlab dev attach
```

The workspace is intact across that, because it is a guest-local copy of a
canonical host tree (ADR-0014) and `./workspace` on the host never moved.
Only a hand-install does not come back. Baking into `C:\Users\Default` at
template build time is the other declared placement, and the right one for
shipping a template to a team — but a template is lab-independent, so a
*domain* user's profile cannot exist at build time, which is why this example
uses the provision route.

## Two riders, recorded

§19.8 left two questions open until §19 existed. Neither is decisive — §19's
answer rests on the provision path above, so a negative on either loses an
option and changes no decision. Both are recorded here either way.

**How these were established.** Both findings below are derived from the
documented behaviour of the components involved and from what this lab's own
code paths can and cannot do at provision time; the run that would confirm
them on a booted domain member has not been performed in this repository's CI
or development environment, which has no Windows Server media and no VS Code
client. Each finding says what would change it. Neither changes a §19
decision either way, which is why they are riders and not blockers.

### Install-from-VSIX over the facade

**Finding: it works, but only after a client has attached at least once.**

`code --install-extension <path>.vsix` needs no marketplace: the VSIX is a
local zip and the CLI unpacks it into the extensions directory. Over the
facade the constraint is not the network, it is the *binary*. The remote
`code` shim lives at
`%USERPROFILE%\.vscode-server\bin\<commit>\bin\remote-cli\code.cmd`, and that
tree arrives with the Remote-SSH server — which, with
`localServerDownload: always`, is pushed by the client on first attach. So:

- A provision **can** stage the `.vsix` into the developer's home before
  anyone logs on (this example does, from `payload/`), and
- installing it is a one-liner in the attached terminal
  (`code --install-extension $env:USERPROFILE\vsix\extension.vsix`), after
  which the extension is a normal per-user extension and lives as long as the
  profile does.

What does **not** work is a provision that installs the extension: at
provision time there is no server, no `code`, and no commit hash to guess.
Unpacking a VSIX into `.vscode-server\extensions\<publisher>.<name>-<version>\`
by hand also works and is what the CLI does, but the layout is undocumented
and version-specific, so this example stages rather than unpacks.

Consequence for §19: none. The declared placement is the *bytes*, which is
what §19.8 says vmlab moves; the install verb is the developer's, in a
terminal vmlab already gives them.

To confirm on a real run: attach once, then in the integrated terminal check
that `Get-Command code` resolves under `.vscode-server\bin\`, run
`code --install-extension $env:USERPROFILE\vsix\extension.vsix`, and check
`code --list-extensions` after a reconnect. A negative would mean the staging
step in `editor-bits.ws` is decoration and the payload belongs in
`C:\Users\Default` instead — an option lost, not a decision changed.

### Does the Windows default profile seed a freshly created domain profile?

**Finding: yes, and it is still the wrong tool here.**

`C:\Users\Default` is copied into a new profile at creation time by
`CreateProfile`/`LoadUserProfileW`, and that is domain-account-agnostic — a
first-logon domain profile picks up whatever is in the default profile,
including `.vscode-server\` and its contents. So baking editor bits into
`C:\Users\Default` in a **template build** is a real, working second placement,
and §19.8 names it as the option for shipping a template to a team.

Two limits keep it from replacing the provision route:

1. It is per-*template*, not per-lab. Anything lab-specific (a domain name, a
   registry, a licence server address) cannot be in it, because a template is
   lab-independent by construction.
2. It only reaches profiles created *after* the bake. A developer who has
   logged on once already gets nothing, and there is no re-seed.

Consequence for §19: none. It is an option gained, not a decision changed.

To confirm on a real run: put a marker file in `C:\Users\Default\` during the
template build, then check for it under `%USERPROFILE%` in the very first
`as_login("dev")` session on a freshly joined member. A negative would mean
the template-bake placement §19.8 names for teams does not reach domain
accounts at all, leaving `provision {}` as the only route — which is the
route this example already takes.

## Two things an offline guest has not taken away

Both are printed by `vmlab dev attach` and `vmlab ssh-config --print`, because
a developer who has just been handed an offline guest assumes both are gone.

**Personal config copies over the alias.** The facade answers `subsystem sftp`
host-side (§19.3), so `scp`, `sftp` and an editor's file explorer all work
against the alias:

```sh
scp ~/.gitconfig vmlab-dev-vscode-windows-dev01:.gitconfig
scp -r ~/vimfiles vmlab-dev-vscode-windows-dev01:vimfiles
```

A `dotfiles` argument is dead by §19.1's third clause, and devcontainers
itself puts dotfiles in a *client* setting rather than in `devcontainer.json`.

**A host-side service is reachable without a reverse tunnel.** `ssh -R` is
refused (§19.3) and is not what this needs. Give the machine a NIC on a
segment with egress:

```wcl
segment "services" { subnet = "10.61.0.0/24" nat = true }
// …and on dev01:
nic { segment = "services" }
```

The NAT engine terminates guest flows in-process and proxies them over
ordinary host sockets, so anything addressed off-segment reaches the host's
own address — a package mirror, a proxy, a licence server.

## Guest credentials

`PROBE\dev` / `vmlab123!` (the default login, and a Domain Admin),
`PROBE\Administrator` / `vmlab123!` (the `admin` login, selected as
`vmlab-dev-vscode-windows-dev01-admin` or `vmlab exec --user admin`). The
secrets are in `vmlab.wcl` plainly because the accounts exist only because
`scripts/directory.ps1` created them, with the same strings.

## What each file is

| Path | What it does |
|---|---|
| `vmlab.wcl` | The lab: one offline domain segment, dc01, and `@dev` dev01 with two logins |
| `scripts/domain.ws` | dc01's provision: promote, then create the account and the share |
| `scripts/domain.ps1` | The forest promotion (idempotent; asks the caller to reboot) |
| `scripts/directory.ps1` | `PROBE\dev` and `\\dc01\team` |
| `scripts/join-domain.ws` | dev01 joins PROBE — **as the machine**, which is right |
| `scripts/join-domain.ps1` | The join itself (idempotent; asks the caller to reboot) |
| `scripts/editor-bits.ws` | The point: places editor bits **as `PROBE\dev`** |
| `scripts/editor-bits.ps1` | The directories, run under the minted logon |
| `config/settings.json` | Machine-scoped VS Code settings, copied into the profile |
| `payload/` | Where a `.vsix` goes; see its README |
| `workspace/` | The host side of `C:\src` — the only thing here that is not a declaration |
