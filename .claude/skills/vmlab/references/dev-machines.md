# Dev machines and the workspace syncer

Any lab machine — VM or container, Windows or Linux — can be the lab's development
environment. Mark it `@dev` and vmlab publishes it as an SSH endpoint an editor
attaches *into*: language server, build, debugger and terminal all run guest-side,
against real guest paths, the real toolchain and, where the lab has one, the real
domain identity. The target is parity with what devcontainers give a Linux
developer, for a Windows application on a real domain.

## The `@dev` declaration

`@dev` is a decorator on a `vm` or `container` block, not a child block, because it
states something *about* the machine rather than configuring something inside it;
nothing it carries is a setting the guest sees. Any number of machines may carry it
and zero is normal. Its arguments are all optional, and a bare `@dev` is a
complete, attachable dev machine.

```wcl
# vmlab.wcl
@dev(default = true, workspace = "./src", workspace_guest = "C:\\src")
vm "dev01" {
  template   = "x86_64/windows-server-2025"
  depends_on = ["dc01"]
  nic { segment = "corp" }
  login "dev" { user = "PROBE\\dev" password = "vmlab123!" }
}

@dev(workspace = "./src")
container "buildbox" {
  image = "mcr.microsoft.com/dotnet/sdk:9.0"
  cpus = 4  memory = 4GiB
  nic { segment = "corp" }
}
```

| Argument | Meaning |
| --- | --- |
| `default` | Make this the lab's default dev machine. At most one per lab. The only `@dev` machine in a lab is the default implicitly, even if it wrote `default = false`; with several and none marked, there is no default. |
| `workspace` | Host directory to sync into the guest, relative to the lab root. Without it the machine is still attachable and has no workspace. |
| `workspace_guest` | Guest path the workspace lands at. Resolved `@dev` argument, then the profile's `workspace_guest`, then the floor `/src`. |

Unset arguments resolve **`@dev` argument, then profile, then floor**. The default
is profile-sourced because it is guest-OS-shaped: the shipped Windows profiles say
`C:\src` and the Linux ones `/src`. A profile with no dev keys still hosts a dev
machine; a missing key means the floor applies, never that the profile cannot be a
dev target. This resolver is deliberately separate from the hardware resolver.
"First in file order wins" was rejected for the default because declaration order
already means something in vmlab and a block reorder would silently move it.

Three things were kept off the decorator on purpose:

- Editor hints: the SSH endpoint is the whole contract and vmlab learns no editor.
- Ports: a dev machine's ports are ordinary `port {}` and `forward {}` declarations,
  and `ssh -L` over the facade is the ad-hoc path.
- Toolchain and package lists: that is `provision {}` and `playbook {}`. A
  distributable template is vmlab's answer to devcontainer features, installed once
  at build time and pulled by every developer (see templates.md).

## `attachable` and the failure ladder

The guest needs two things: the agent and the toolchain. There is no sshd to install
and the workspace path is created by the syncer. The agent advertises three feature
strings in its handshake: `tunnel` serves the facade's `direct-tcpip`, `fileops`
serves host-side SFTP and every transfer, and `watch` serves the workspace syncer.

**`attachable` means exactly `tunnel` and `fileops` are both present**: this agent
can serve an attach, never that your attach will succeed, because identity is
declared separately. It does not widen to cover `watch`; the syncer checks
`watch && fileops` for itself. A template built with `agent = false` reports `false`
through the same path as one whose agent is merely old. `vmlab machine capabilities`
reports it and `vmlab status` carries it.

A machine whose agent predates these features fails where it costs least and says
most:

- **`validate` says nothing.** It has no side effects, and the only static signal is
  the template's free-form `agent_version` string. Comparing that would be inference.
- **`up` warns.** The handshake is part of readiness, so by then the features are
  honestly probed. The warning says a shell still works and names both remedies.
- **Attach fails hard.** `vmlab dev attach` refuses, naming what the agent does not
  serve and both remedies: rebuild the template to bake in the shipped agent, or
  push it into the running machine with `vmlab machine repair-agent`. The facade
  itself degrades per channel, so an old agent still serves a shell while `sftp` and
  `direct-tcpip` refuse by name.

Rebuild is policy; repair is a tool. The agent enters an image once, at build, and
`repair-agent` pushes the host's shipped agent over the agent's own channel and
marks the machine **diverged**, because the template's sealed `agent_version` no
longer describes it. Nothing does this by itself. It is meaningless on a container,
whose agent lives in the initramfs vmlab ships and tracks the installed vmlab; the
verb says so.

Two further preconditions of a dev-capable image: the guest must be
symlink-capable, on Windows through `SeCreateSymbolicLinkPrivilege` or Developer
Mode, and a full Linux VM's kernel must be recent enough that `inotify` survives an
overlayfs copy-up.

## The managed SSH config block

vmlab's whole host-side footprint is one marker-fenced block inside your own
`~/.ssh/config`, between `# BEGIN vmlab managed block` and
`# END vmlab managed block`. A separate file behind an `Include` was rejected on
evidence: JetBrains Toolbox's importer does not follow `Include`, and a private file
reached with `-F` would keep `vmlab ssh` working while every editor saw nothing.
Sharing one path means a broken block breaks `vmlab ssh` too, at a terminal that can
explain it. The `ssh_config` key in the host configuration (see host-profiles.md)
relocates the file vmlab writes into; it is a location knob with one code path
behind it, and the `ssh -G` check below still runs.

Stanzas cover **declared** machines, not running ones, because an empty picker at
the moment you want it helps nobody. Any command that successfully loads a lab
renders the block and writes it only on a real difference, so working inside a lab
directory is enough to register it; a failed write warns, except at `vmlab ssh` and
`vmlab dev attach` where the alias is load-bearing and the command fails with the
reason.

The alias is `vmlab-<lab>-<machine>`, plus `vmlab-<lab>-<machine>-<label>` for each
non-default login, so "attach as admin" is a pick in an editor's host list. Each
stanza sets:

- a `ProxyCommand` running `vmlab ssh-proxy <lab>/<machine>`,
- `User <label>` on a labelled alias,
- vmlab's own `UserKnownHostsFile` with `StrictHostKeyChecking accept-new`,
- `ControlMaster auto` with a `ControlPath` of `$XDG_RUNTIME_DIR/vmlab/ssh/%C` and
  `ControlPersist 10m`. `%C` is OpenSSH's own bounded token, which keeps the socket
  path inside the unix socket limit on any home directory.

No `HostName` is set: the proxy is the connection.

The writer's discipline is the feature, because its failure mode is eating someone's
SSH config: an advisory lock across the read-modify-write, a temp file in the same
directory fsynced and renamed onto the *resolved* path so a dotfiles symlink stays a
symlink, an absent file created `0600`, deterministic ordering by lab, machine and
label so a tracked config diffs only when something changed, and a refusal naming
file and line on markers it cannot read. Each lab's section carries the lab's
canonical root in a comment, and a root that no longer holds a `vmlab.wcl` has its
section dropped on the next write. Every write re-hoists vmlab's region to the top,
because OpenSSH takes the first value for each keyword and an earlier `Host *` would
silently win, then runs `ssh -G <alias>` and errors naming the keyword that beat it
if the resolved `proxycommand` is not vmlab's.

`vmlab ssh-config` refreshes the block by hand and `--print <machine>` emits one
stanza plus the editor settings snippet for a client that will not read the file.

## `dev attach`, `dev use` and which machine is mine

`vmlab dev attach [machine]` is cold-to-editing in one command: it ups the machine,
waits until it is attachable with the wait visible, prints the alias and the editor
settings snippet and the offline-guest notes, and then `exec`s the system `ssh` so
it becomes a shell on the machine. It launches no editor and knows none; you open
your own editor and pick the alias out of its host list. Because it becomes a shell,
the syncer is not tied to it: the syncer is owned by the lab daemon and keeps
running when the shell closes. `vmlab ssh` by contrast refuses on a stopped machine,
and `ssh-proxy` never does lifecycle at all.

A committed `vmlab.wcl` cannot say which dev machine is *yours*, so that is
host-side state. `vmlab dev use <machine>` records it in the lab's own gitignored
`.vmlab/` directory, in a file named `dev-machine`, which makes it per-developer by
construction; `vmlab destroy` clears `.vmlab/` and forgets it.

When a `dev` verb needs a machine and none was named it climbs a fixed ladder and
never guesses:

1. an explicit argument,
2. the `VMLAB_DEV_MACHINE` environment variable,
3. the `vmlab dev use` selection,
4. the lab's default dev machine (`@dev(default = true)`, or the lone `@dev`),
5. otherwise an error listing the candidates.

Every rung that names a machine is checked rather than trusted: a rung naming
something that is not a dev machine in this lab is an error at that rung, never a
silent fall-through, so an environment variable left over from another lab cannot
land you somewhere nothing said out loud. The output says which rung answered.

There is no `dev list` or `dev status`; `vmlab status` shows dev-ness and
`attachable` for every machine. There is no `rebuild` verb either:
`vmlab vm destroy <m>` then `vmlab up <m>` is re-clone plus re-provision, and the
workspace survives it.

## The workspace

**The workspace is a guest-local working copy on the machine's own disk; the host
directory is canonical; a vmlab-integrated syncer keeps them in step.**

Neither obvious alternative works. A shared folder cannot carry a watched source
tree: host-side edits do not reach `ReadDirectoryChangesW` over virtiofs or SMB, nor
`inotify` over virtiofs on Linux, and both fail silently, with the watcher armed and
quiet and the language server simply no longer re-analysing. Source kept guest-side
alone fails the other way: `destroy` is a first-class verb on disposable clones and
a snapshot restore would roll uncommitted work back with the machine. With the host
canonical, `destroy` loses nothing and a restore re-converges the guest from the
host. `share {}` stays exactly as useful as it was for datasets, installers and
build output; the line is a watched source tree.

The syncer is a task in the lab daemon, started by `up` **after provisioning**
rather than at machine-ready, and it runs as the machine's default login: the one
exception to vmlab's own machinery keeping the agent identity, because it produces
the developer's files, and the account it writes as does not exist until
provisioning creates it. Ownership always matches whoever will attach. One pass
walks the host, learns what the guest holds, reconciles, applies and saves the
ledger. The seed is simply the first pass.

### The ledger and what a conflict is

The agreement point is a **host-side sync ledger** under the lab's `.vmlab/`, one
per (machine, workspace), so `destroy` wipes it. It holds one record per relative
path: a content digest plus **each side's own** size and mtime as a change detector.
A host mtime is never compared to a guest mtime, only to the host's own recorded
value, because a restored guest resumes with a clock behind the host and every file
it holds would look older; that alone rules out newest-wins. Digest is the truth and
the stat pair is only a pre-filter, since a same-size in-place write is exactly what
the share transports were caught missing. A missing ledger is not a decision: on
first run, or after a wiped `.vmlab/` with a live guest, paths whose digests match
are adopted as agreed and paths that differ take the ordinary conflict path, rather
than a blind host-to-guest seed eating a developer's work.

Per path, each side is unchanged, modified, deleted or replaced by the other kind
relative to the ledger. One side changed: propagate. Both changed: conflict, with
four riders:

- Both modified to identical content is not a conflict and transfers nothing, which
  is common after a host-side `git checkout` lands bytes the guest already had.
- Modified on one side and deleted on the other is a conflict, not delete-wins,
  because deletion is unrecoverable.
- Mode-only changes are not conflicts and are not synced across kinds.
- A file replaced by a directory, or the reverse, is a conflict.

### Ignore rules and the prune list

Ignore rules live in the tree, not in the lab file: a built-in floor, then the
repo's `.gitignore`, then `.vmlabignore` for the delta including `!` negations. What
you do not want to sync is almost exactly what you do not commit, and `.vmlabignore`
covers where "almost" fails: a gitignored `.env`, a local cert or
`appsettings.Development.json` that the app needs guest-side takes a negation.
Precedence is git's own, deepest file first, `.vmlabignore` beating `.gitignore` in
one directory, and a path under an ignored directory staying ignored. The floor
covers the syncer's own scratch names and `.git/**/*.lock`, and no repo rule may
override it.

An ignored path is not skipped, it is **guest-owned**: `node_modules` is the proving
case, where the guest runs its own install and holds guest-native binaries,
diverging permanently and on purpose. Neither direction ever touches one. The host
computes a coarse **prune list**, ignored directory prefixes with no negation below
them, and hands it to the agent when the watch opens, so no watcher is registered
under them. That matters because `inotify` costs one watch descriptor per directory
against a default limit of 8192, so an unpruned registration would be silently
incomplete on Linux. The guest is never asked to decide what is in the synced set;
it is handed a list.

When the rules change under the syncer, leaving scope is free: a newly guest-owned
path leaves the ledger and both copies stay. Entering scope is a conflict, because
no agreement exists and both sides may hold content, so un-ignoring a populated
directory halts naming every file in it. The rules' own digest is part of the
ledger, so the halt can say these conflict because you just changed the rules.

### Both directions

Host changes arrive from vmlab's own watcher. Guest changes arrive from the agent's
**dirty set**: the agent holds a coalescing set of paths that changed, sends a
single nudge when the set goes from empty to non-empty, and the host drains it. A
drained record is the path plus its current stat, or a tombstone if the path is
gone, so no platform event kind ever crosses the seam and `inotify` and
`ReadDirectoryChangesW` disagreeing about renames never becomes a vocabulary
problem. Both directions get the same per-path debounce, a quiet period before a
path is read, because editors write a temp and rename and compilers write in chunks,
and a torn read guest-to-host would land on the canonical copy. A path that keeps
moving keeps waiting, so a burst under one subtree de-prioritises rather than
starving a single save elsewhere.

Every apply is temp-name-then-rename in the target's own directory, and the ledger
records agreement only after the rename, never after the last write. Renames are
delete plus create at the ledger level; a directory delete expands via the ledger,
not the event stream. Symlinks sync verbatim and are never followed; their target
string is content, untranslated across the seam. Special files — FIFOs, sockets,
device nodes and non-symlink reparse points — are skipped loudly and never enter the
ledger, and so is a drained path the login cannot read, because a build leaving a
root-owned artefact in the tree must not stop the dev machine.

### The stat-walk

The steady state never walks the tree; it probes named paths. A full guest
**stat-walk**, where the guest reports every path's kind, size and mtime and the
host applies the ignore set on receipt and asks for digests only for suspects, runs
on a watch discontinuity and nowhere else: first sync, ledger loss, an overflow, a
dropped channel. That list is exactly the list of watch discontinuities, which is
why there is no resync token. An overflow, on either platform or in the agent's own
capped set, collapses to a single rescan value that warns, forces the walk and never
halts. **The rescan is a barrier in both directions**: between the overflow and the
completed walk the host does not know the guest moved, so propagating host-to-guest
meanwhile would overwrite guest work silently through the ledger with no conflict
raised. It is a deferral, needing no developer action, and it clears itself.

### Windows preconditions

A Windows guest costs the syncer three actions, each a precondition of the
mechanism, so vmlab does them rather than documenting them:

1. **The NTFS case-sensitive flag on every directory the syncer creates, at
   creation**, including the workspace root. The host can hold `Foo.cs` and
   `foo.cs`; a default Windows guest cannot, and the second write would silently
   land on the first. The flag only takes on an empty directory, which the syncer's
   always are, and inheritance is not relied on. Where it cannot be set, a case
   collision at that path is a loud refusal by name. This also makes a shared
   `.git/config` with one `core.ignorecase` right on both sides.
2. **Symlinks attempted, with a warning by name on failure.** A symlink-capable
   image is a documented precondition.
3. **`core.autocrlf = false` in the guest's global git config**, set as the default
   login. Git for Windows ships it `true`, which would rewrite the working tree to
   CRLF on the first guest-side checkout, sync every file back as modified, and halt
   the workspace if the host had touched anything. The syncer translates nothing;
   bytes cross verbatim and git normalises on both sides from settings that agree.

A Windows dev login declared `elevated = false` degrades the workspace in exactly
two ways — no case-sensitive directories and no symlinks — and the syncer says so up
front rather than at a random path hours in. Line-ending policy belongs in each
side's global git config, never the repo's, because the home directory is
guest-local and `.git/config` is shared; `.gitattributes` is the escape for genuine
CRLF needs.

### The conflict halt

The developer authors guest-side; the host is doing durability work, not authorship.
The host-side writer set is small: git operations, occasional tooling, vmlab's own
restore. **A conflict is therefore an anomaly**, which licenses an expensive, loud,
safe policy over a winner rule that must be right thousands of times a day. That
policy is **halt and surface**: the whole workspace stops, both directions, on one
machine, naming every conflicting path in the batch. A pass scans and reconciles
before it applies anything, so the halt is computed from a whole reconciliation, and
a host-side `git pull` that collides in batches is one halt rather than thirty
resolve-and-resume round trips. Ten conflicts do not become a bigger hammer.

While halted, the watch keeps running and the host keeps draining the guest's dirty
set into its own pending set, so a long halt costs no rescan and edits made during
it drain normally on resume. No conflict copy is written: the two copies already
exist, one per side, and a halt writes neither and deletes neither. The scope is one
machine's workspace; two dev machines may share one host workspace because the host
is a hub rather than a peer, each with its own ledger, and one halting must not stop
the other. The halt message names the machine.

From inside the guest a halt is otherwise nothing happening, and there is no
guest-to-host control path to say otherwise, so the halt writes a marker file,
`.vmlab-sync-halt`, at the guest's workspace root, listing up to 200 of the halted
paths and saying how many it left out. It is in the ignore floor so it never syncs,
and its `git status` noise is the point: it is the developer noticing. Resolution is
host-side, necessarily, and the routes are:

| Verb | What it does |
| --- | --- |
| `vmlab dev sync status` | What the syncer last decided: halted paths, volume warnings, rescan symptoms, and what it skipped by name. Capped at 500 entries, saying what was dropped. |
| `vmlab dev sync flush` | Run a sync pass now and wait for it, rather than for the next edit. |
| `vmlab dev sync diff [paths]` | Bring the guest's copy of a path host-side beside the host's. With no path it takes every halted one. Neither copy is changed; a copy over 4 MiB or not text is described by size and digest instead. |
| `vmlab dev sync resolve --host \| --guest [paths] \| --all` | Pick which side wins at a halted path and carry it out. The losing copy is overwritten and not recoverable from vmlab. `--all` takes every halted path as the halt stands. |

A free third route needs no verb: make the two sides identical by hand and the next
pass adopts them as agreed. These verbs live under `vmlab dev` because a workspace
exists only for a dev machine, and a `--machine` flag or the selection ladder picks
which. See cli-machine.md.

### Guards that are not halts

**The size guard refuses loudly, per file, before transfer.** A file over the
`workspace_max_file` cap in the host configuration, 256 MiB by default, is never
hashed or sent; the refusal names the file and the cap and states the two ways out,
an ignore rule or a raised cap. It exists to catch the 4 GB `.vhdx` nobody wrote a
rule for.

**Volume warns and never halts**: a pass moving over a thousand paths or 256 MiB,
dominated by one subtree, names that subtree and suggests a `.vmlabignore` rule,
because a build burst into an un-ignored `target/` is wanted work that happens to be
large.

**The guest-to-host bulk-delete guard** is asymmetric on purpose: host-to-guest
deletes are unguarded, and a `git checkout` removing 400 guest files just removes
them, but guest-to-host deletions past a threshold are withheld and halt. The
threshold is a proportion with a floor: more than 20 paths and more than half of
what the ledger had agreed on. A fixed count would punish large repos and a bare
proportion would let a ten-file project lose everything. A single deletion still
propagates immediately. `--guest` on the halt is what releases the withheld
removals.

**`.git` syncs bidirectionally**, because the guest can stay offline while the host
has the network, so a host-side `git fetch` is a first-class operation, and because
a coding agent inside the dev machine commits and branches with no host shell. Most
of `.git` is content-addressed or write-once and syncs freely. The mutable set —
`index`, `HEAD`, `ORIG_HEAD`, `FETCH_HEAD`, `packed-refs`, `config`, and everything
under `refs/` and `logs/` — is **deferred** while a `*.lock` is held on either side.
Lock files themselves never sync. That deferral is timing, not a conflict rule:
nothing is reported, nothing needs resolving, and the loop looks again after a
second so a lock released in the guest does not stall until the next unrelated edit.
Running git on both sides at once can still reach an ordinary halt, where both
copies survive.

### Snapshots bracket the syncer

A restore rewinds the guest by hundreds of files at once, which a naive syncer
cannot tell from the developer having edited them and would push onto the canonical
copy. vmlab performs the restore, so it brackets it.

Capture first flushes, and **refuses with no escape flag** if the guest holds work
the canonical copy has never seen, whether a halt stands or the pass could not
finish.

Restore refuses while a halt stands, and `--discard-guest-changes` is the one
escape: it throws the guest copy of every halted path away, by name. Restore then
takes the syncer off the workspace, rewinds, and puts it back owing a **re-seed**: a
host-only, digest-based reconcile that overwrites anything differing from host
truth, deletes anything the ledger does not hold, transfers nothing else, and emits
no guest-to-host action at all. It compares by digest because a restored guest's
clock runs behind the host. It completes before the watch reopens, and it replaces
the stat-walk rather than following one. Both the owed re-seed and the halt a
restore refuses on ride the ledger, because a restore does not need a running
machine. Every surface that takes or restores one says the same sentence.

> **Warning — snapshots are not a workspace backup.** A dev machine's source lives
> on the host, which is what survives `destroy` and what a restore re-converges the
> guest from. A snapshot holds the guest's copy at that moment and nothing more; do
> not rely on one to keep uncommitted work.

## Editors and the offline guest

vmlab publishes SSH and nothing else, so any SSH-capable client attaches, but for a
Windows dev machine the set that works is narrower: plain `ssh`, `scp`, `sftp` and
VS Code Remote-SSH work on both guest families; JetBrains Toolbox, JetBrains Gateway
and Zed serve a Linux dev machine only.

The guest can stay offline. The settings snippet `dev attach` and
`ssh-config --print` print sets VS Code's `remote.SSH.localServerDownload` to
`always`, so the client downloads its server and pushes it over `scp`, and
`remote.SSH.remotePlatform` to `windows` for a Windows alias.

Extensions and plugins live in the guest home, outside the workspace, so they
survive reboot, `down`/`up` and restore to a later snapshot, and die on `destroy`
plus `up`. Bake what the lab needs every developer to have; hand-install what you
personally want today, and expect to redo it after a rebuild.

The two worked examples, `examples/dev-vscode-windows` and
`examples/dev-neovim-container`, place editor bits into the dev login's home from a
`provision {}` using `as_login`, before that user has ever logged on (see
examples.md).

## Walkthrough: cold to editing

A trimmed version of the `dev-neovim-container` example — the parts every dev
machine has.

### Before you start

- vmlab is installed with the micro-VM guest asset in place, so containers boot (see
  start-here.md).
- Internet access from the host, to pull `alpine:3.22`.
- An `ssh` client on the host. vmlab does not ship one; it writes a block into
  `~/.ssh/config` and hands over to yours.
- Optionally, an editor with remote SSH support, such as VS Code with Remote-SSH or
  any editor that opens files over `ssh`.

### Write the lab file

In a new directory, create the lab file and the workspace directory it names.

```sh
mkdir -p workspace scripts
printf 'print("hello from the workspace")\n' > workspace/hello.lua
```

```wcl
# vmlab.wcl
import <vmlab.wcl>

lab "first-dev" {

  segment "lan" {
    subnet = "10.62.0.0/24"
    nat    = true
  }

  @dev(default = true, workspace = "./workspace")
  container "dev01" {
    image   = "alpine:3.22"
    profile = "container"
    cpus    = 2
    memory  = 1GiB
    mode    = :idle
    nic { segment = "lan" }

    login "dev" { user = "dev" default = true }

    provision "scripts/dev-user.ws" { }
  }
}
```

- `@dev(...)` is a decorator on the machine. `workspace = "./workspace"` names the
  host directory to sync, relative to the lab root. The guest path defaults from the
  profile, `/src` on Linux and `C:\src` on Windows; set `workspace_guest` to change
  it. `default = true` makes this the lab's default dev machine, which matters once
  a lab has more than one.
- `mode = :idle` keeps the micro-VM up without running the image's entrypoint. A dev
  container has no service to be.
- `login "dev" { user = "dev" default = true }` declares the account every surface
  attaches as: `ssh`, `vmlab exec`, `vmlab shell` and the syncer itself. On a Linux
  machine the agent is root and needs no credential to become an account, so no
  password is declared. A Windows login needs one.
- The provision creates that account. vmlab declares logins but does not create
  accounts; the lab's own provisioning does.

The provision script runs as the agent identity, root, which is right for installing
packages and creating an account.

```rust
// scripts/dev-user.ws
// Create the account the lab file declares, as the machine.

use vmlab

fn sh(m: Machine, script: string, timeout: int) -> Result[string, string] {
    let r = m.exec_timeout("/bin/sh", ["-c", script], timeout)?
    if r.exit_code != 0 {
        return Err(fmt("`{}` exited {}: {}", script, r.exit_code, r.stderr))
    }
    Ok(r.stdout)
}

fn setup(lab: Lab) -> Result[unit, string] {
    let dev01 = lab.container("dev01")?
    dev01.wait_ready(600)?
    for login in dev01.logins() {
        sh(dev01, "id -u " + login.user + " >/dev/null 2>&1 || adduser -D -s /bin/sh " + login.user, 120)?
        lab.log("account " + login.user + " exists")
    }
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("dev01 account setup failed")
}
```

`dev01.logins()` reads the `login {}` declarations from the lab file, so the account
name lives in one place. The `id -u ... ||` guard makes the script safe to run on
every `up`.

```sh
vmlab validate
```

`vmlab validate` reports one container. It also writes the managed block into
`~/.ssh/config`: any command that loads a lab file does.

### Attach in one command

```sh
vmlab dev attach
```

`dev attach` brings the machine up if it is down, runs the provision, waits until
the machine is **attachable**, prints the SSH alias and an editor settings snippet,
and then becomes a shell on the machine as the `dev` login. With no argument it
picks the machine from the fixed ladder.

A container is always attachable because its agent is the one vmlab ships. On a VM
built from an old template, `vmlab machine capabilities dev01` shows what is missing
and `vmlab machine repair-agent` pushes the current agent in.

In the shell that opens:

```sh
id -un
ls /src
cat /src/hello.lua
```

`id -un` prints `dev`. `/src` holds `hello.lua`, placed there by the syncer's first
pass. Leave the shell with `exit`. The machine keeps running and the syncer keeps
running: it belongs to the lab daemon, not to the shell. `vmlab status` shows
`dev01` ready and attachable; `dev attach` printed the alias `vmlab-first-dev-dev01`.

### Reach it with plain ssh

The stanza's `ProxyCommand` is a hidden vmlab verb, so the system `ssh` connects
through the facade with no guest network and no sshd in the guest. Anything that
reads `~/.ssh/config` can use it.

```sh
ssh vmlab-first-dev-dev01 id -un
vmlab ssh dev01 -- uname -a
```

`vmlab ssh` is not a second SSH client. It refreshes the managed block and then
hands over to the system `ssh` against the alias. Unlike `dev attach` it refuses if
the machine is down and never starts one, the way `exec` and `console` behave.
Copying files works the same way, because the facade answers `sftp` on the host.

```sh
scp workspace/hello.lua vmlab-first-dev-dev01:/tmp/hello.lua
```

To see the stanza and the editor snippet again, for a client that will not read the
config file:

```sh
vmlab ssh-config --print dev01
```

### Open your editor

`dev attach` launches no editor and knows none. Open yours and pick
`vmlab-first-dev-dev01` out of its SSH host list, then open `/src`. The snippet
`dev attach` printed sets `remote.SSH.localServerDownload` to `always` for VS Code,
so a segment without egress does not block the server install. For a terminal
editor, run it over the alias directly.

```sh
ssh vmlab-first-dev-dev01 -t vi /src/hello.lua
```

### Edit on either side

```sh
printf 'print("edited on the host")\n' > workspace/hello.lua
vmlab ssh dev01 -- cat /src/hello.lua
vmlab ssh dev01 -- sh -c 'echo built > /src/out.txt'
cat workspace/out.txt
```

The host edit reaches `/src/hello.lua`, and the file written in the guest appears in
`./workspace/`.

### Read what the syncer is doing

```sh
vmlab dev sync status
vmlab dev sync flush
```

### Rebuild without losing the workspace

There is no rebuild verb because two existing verbs are one. Destroying the machine
takes its clone and the guest working copy; the next `up` provisions a fresh one and
the syncer seeds it again from the host.

```sh
vmlab container destroy dev01
vmlab dev attach
```

Everything the provision declared is back, and `/src` holds the host directory's
current contents. Anything installed by hand in the guest is gone. That is the
durability rule: bake what every developer needs into the image or a provision,
hand-install what you want today, and expect to redo the latter after a rebuild.

### Clean up

```sh
vmlab down
vmlab destroy
```

`destroy` also forgets a `vmlab dev use` selection, which lives in the lab's
gitignored `.vmlab/`. The `./workspace` directory on the host is untouched, and the
alias leaves `~/.ssh/config` the next time a command loads a lab and finds this
directory no longer has a `vmlab.wcl`.
