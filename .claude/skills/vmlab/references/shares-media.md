# Shared folders, attached media and file movement

## Shared folders

A `share` block on a VM maps a host directory to a guest path. Two transports
sit behind that one surface, virtiofs and SMB. vmlab picks between them, serves
them, and mounts them in the guest once it is ready. The full field list for
`share {}` and for the VM's `media` children is in vm.md.

```wcl
vm "dev01" {
  template = "x86_64/ubuntu-24.04"
  nic { segment = "lan" }
  share { host = "./src"      guest = "/mnt/src" }
  share { host = "~/datasets" guest = "D:\\data"  readonly = true }
}
```

### Two transports

**virtiofs** is the fast path. vmlab runs one `virtiofsd` per share and attaches
it as a vhost-user-fs device, which the guest mounts natively with
`mount -t virtiofs`. No guest network is involved and no credential exists. It
needs a virtiofs client in the guest: Linux kernels from 5.4 have one, and a
Windows guest needs the virtio-win driver and WinFsp baked into its template. A
guest OS profile declares whether its guests mount virtiofs with the `virtiofs`
capability flag; the shipped `linux-modern` profile sets it (see
host-profiles.md).

virtiofs is compatible with online snapshots. vmlab runs `virtiofsd` with
migration mode enabled, so its session state, open handles included, rides the
snapshot's migration stream via QEMU's device-state transfer. The cost is that a
VM carrying a virtiofs share has its RAM moved to a shared memory backend, and
one daemon per share.

**SMB** is the universal fallback. The lab daemon serves each share at the
segment gateway as `\\<gateway>\<share>`, so a Windows guest needs nothing extra
and a Linux guest needs only `cifs-utils`. An XP-era guest can be served too:
`smb1 = true` on a share enables the SMB1 dialect and the NTLMv1 authentication
those guests require. It is off unless asked for, and harmless on an isolated lab
segment. SMB carries no device state at all, so a restored VM's sessions are
stale TCP that the guest's SMB client re-establishes transparently.

### How the transport is chosen

Each share carries `transport = "auto" | "virtiofs" | "smb"`, defaulting to
`auto`. The choice is made once, before the lab starts, as the **share plan**:
which shares ride virtiofs, which fall back to SMB, which segments need a gateway
rule to reach the SMB server, and which host port it takes. Two host facts feed
it — whether a `virtiofsd` binary exists and whether a localhost port is free —
and one guest fact, the profile's capability flag.

- `auto` takes virtiofs only when the host can serve it *and* the guest can mount
  it. SMB is the fallback, never the preference.
- `virtiofs` always rides virtiofs. A host with no `virtiofsd` errors when the
  machine starts rather than degrading silently.
- `smb` always rides the bundled SMB server.

Container volumes join the same plan. They have no per-guest question, since the
micro-VM's own guest always mounts virtiofs, so they ride virtiofs whenever the
host has a `virtiofsd` and SMB otherwise.

### The bundled smbd

No mature embeddable SMB server exists in Rust, so vmlab runs Samba's `smbd` as
an unprivileged process. Three things make that work without root: it listens on
a localhost high port, which any user may bind, and the switch proxies the
segment gateway's port 445 onto it; every Samba state directory is relocated
under the lab's `.vmlab/smb`; and each share is served with `force user` as the
invoking user, so `smbd` never switches uid. Samba is a documented host package,
and `smb1` shares depend on the installed build retaining NT1 support, which
vmlab checks.

vmlab mints per-lab SMB credentials automatically, and a share is mappable only
with its owning VM's credential. That scopes shares to the VM that declared them
even on a shared segment. Authenticated NTLMv2 with SMB signing is the baseline,
because current Windows hardening rejects unauthenticated shares. The credentials
are plumbed by vmlab and are not user-visible.

### Mounting in the guest

Once a VM is ready, the lab daemon runs the share plan's **mount steps** through
the agent. The steps are a value computed per guest OS before anything runs, so
what a Windows guest will be told to do can be read without booting one.

- **Linux, virtiofs:** `mount -t virtiofs <tag> <guest_path>`.
- **Linux, SMB:** `mount -t cifs //<gateway>/<share> <guest_path>` with the
  generated credential.
- **Windows:** the credential is stored once per lab with `cmdkey`. A drive-letter
  target such as `S:` is mapped with `net use`. A folder-path target becomes a
  directory symbolic link to the UNC path with `mklink /D`, because a junction
  cannot target a UNC path.

The agent's mounts run as the agent identity, SYSTEM on Windows, and a drive
letter is visible in every session while each logon authenticates separately. So
the agent also writes the lab's share credential into every logon it mints,
before spawning anything. Without that, a developer attaching through the SSH
facade (logins-and-ssh.md) would see the mapped drive and be unable to open it.

**XP-era guests mount by screen.** The agent does not target XP or 2003-era
guests, so automatic mounting does not apply there. A provision script maps the
share instead, by typing
`net use X: \\<gateway>\<share> /user:… /persistent:yes` through the
screen-driven API (snapshots-vision.md).

### What a share is not

**Share contents are outside every snapshot.** A share's files are host state. A
snapshot restore never rolls them back, on either transport, and a `destroy`
never deletes them. To have a directory rolled back with the VM, put it on the
VM's disk.

A share needs a segment to reach the gateway on when it rides SMB, so a VM with
an SMB share and no NIC is a validation error. A port-isolated NIC can still
reach the gateway, so shares work on isolated ports by design.

A share is a passthrough view of the host directory, and that is the wrong tool
for a watched source tree: file-change notification does not cross virtiofs or
SMB, on either guest family, and it fails silently. A dev machine's source is
therefore a **workspace**, a guest-local copy kept in step by vmlab's syncer
(dev-machines.md). Use a share for datasets, build caches and artefacts, and a
workspace for the code you edit.

## vmlab cp

`vmlab cp` copies a file or a tree between the host and a guest over the vmlab
agent's file session, with no guest network and no share involved. Either side
may be a guest reference spelled `<vm>:<path>`; the other is a host path. Parent
directories on the receiving side are created.

```sh
vmlab cp <SRC> <DEST>
```

| Option | Meaning |
| --- | --- |
| `<SRC>` | Source: a host path, or `<vm>:<path>` to pull from the guest. |
| `<DEST>` | Destination: `<vm>:<path>` to push, or a host path when pulling. |
| `-h`, `--help` | Print help. |

The guest side is recognised by splitting on the first colon, so a Windows path
keeps its drive letter: `box:C:/weave` is the machine `box` and the guest path
`C:/weave`. The `<vm>` part accepts the `[lab/]name` form. When neither side is a
guest reference the command is refused locally. A push whose host source does not
exist is refused before any request is sent.

A push sends one file, or walks a directory and sends every file under it to the
matching path below the guest destination, one request per file, keeping each
file's mode. The daemon opens the host file itself, so the path is made absolute
first, and verifies the digest end to end. It prints
`pushed <bytes> bytes to <vm>:<path>` for a file and
`pushed <n> file(s), <bytes> bytes to <vm>:<path>` for a tree.

A pull copies one guest file to the host path. When the host destination is an
existing directory the guest file's name is kept under it. It prints
`pulled <bytes> bytes to <path>`.

**Transfers run as the agent identity.** `cp` runs as SYSTEM or root even on a
machine that declares a `login {}`, unlike `exec` and `shell`. A pushed file is
owned by the agent identity, not by the login you would attach as. To write into
a login's home as that login, use `scp` over the SSH alias, or the `as_login`
handle in a provision script (logins-and-ssh.md).

```sh
vmlab cp ./tools/ dc01:C:/tools
vmlab cp dc01:C:/Windows/debug/netsetup.log ./logs/
vmlab cp mixed-lab/nix01:/etc/nixos/configuration.nix ./configuration.nix
```

Exit status is 0 on success. `not_found` (4) means the lab declares no machine by
that name. `failed` (1) covers a machine that is not running, an agent that does
not answer, a guest path that cannot be opened, and a digest mismatch. A
malformed pair of arguments or a missing host source exits 1 before any request
is sent.

## vmlab clipboard

`vmlab clipboard` reads or writes a guest's clipboard over the guest agent
channel (architecture.md). No guest network is involved, so it works on a machine
with no NIC at all.

```sh
vmlab clipboard <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `get` | Write the guest clipboard to stdout, with no trailing newline added. |
| `set` | Set the guest clipboard from TEXT, or from stdin when TEXT is omitted. |
| `-h`, `--help` | Print help. |

Both subcommands take a machine reference of the form `[lab/]machine`. A bare
name is resolved against the lab in the current directory, and the lab daemon is
started if none is running. The qualified form addresses a lab that is already
running from any directory and never starts one. The machine must be running with
an agent that advertises the `clipboard` feature (`vmlab machine capabilities`,
cli-machine.md).

### vmlab clipboard get

```sh
vmlab clipboard get [OPTIONS] <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The machine, as `[lab/]name`. |
| `--json` | Emit the raw JSON string instead of the bare text. |
| `-h`, `--help` | Print help. |

The guest's clipboard text is written to stdout byte for byte. No newline is
appended, so what you pipe on is exactly what the guest held. With `--json` the
text is printed as one JSON string literal instead. The agent is given 10 seconds
to answer.

### vmlab clipboard set

```sh
vmlab clipboard set [OPTIONS] <MACHINE> [TEXT]
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | The machine, as `[lab/]name`. |
| `[TEXT]` | The text to copy. Omit it to read stdin. |
| `--json` | Emit the raw JSON reply instead of a confirmation. |
| `-h`, `--help` | Print help. |

With a `TEXT` argument that text becomes the guest's clipboard. Without one,
stdin is read to end of file and passed through verbatim, trailing newline
included, so `vmlab clipboard get a | vmlab clipboard set b` round-trips exactly.
Use `echo -n` when you do not want the newline `echo` adds. On success the verb
prints `copied N bytes to "<machine>" clipboard`; with `--json` it prints the
daemon's reply, which is `true`.

### Examples

Move a password from one guest to another without typing it:

```sh
vmlab clipboard get dc01 | vmlab clipboard set client01
```

Paste a host file's content into a guest:

```sh
vmlab clipboard set client01 < notes.txt
```

Read a guest's clipboard into a shell variable:

```sh
token=$(vmlab clipboard get buildbox)
```

### Exit status

Exit status is 0 when the clipboard was read or written. Exit 4 (`not_found`)
means the lab declares no machine by that name. A machine that is not running, an
agent that is not answering or lacks the `clipboard` feature, and a request that
times out all exit 1 (`failed`). A qualified reference to a lab that is not
running exits 1. A usage error exits 2.
