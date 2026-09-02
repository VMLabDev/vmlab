# Logins and the SSH facade

A `login {}` block declares an identity on a machine; vmlab logs on as that
identity whenever a person reaches into the guest. The SSH facade is how an
editor or a plain `ssh` reaches in: vmlab terminates SSH on the host and maps
each SSH channel onto the guest agent, so no guest runs an sshd. The facade is
a general capability of every machine, not only of dev machines
(see dev-machines.md). The `login {}` block's field reference is in vm.md; the
verbs are `vmlab ssh`, `vmlab ssh-config`, `vmlab exec` and `vmlab shell`
(see cli-machine.md).

## Declaring a login

Identity is a property of the machine, not of the attach. A repeatable
`login "<label>" { user, password?, elevated?, default? }` block on a `vm` or
`container` names a guest account and, on Windows, its secret.

The label selects the login: an SSH username, `--user` on `exec` and `shell`,
and `as_login` in a script all take the label. The raw account name is accepted
as an alias for the label. Using labels keeps `PROBE\dev` out of an SSH
username and out of a `ControlPath`. One account may be declared twice at
different elevation under two labels.

```wcl
# vmlab.wcl
vm "dev01" {
  template   = "x86_64/windows-server-2025"
  depends_on = ["dc01"]
  nic { segment = "corp" }

  login "dev"   { user = "PROBE\\dev"           password = "vmlab123!" default = true }
  login "admin" { user = "PROBE\\Administrator" password = "vmlab123!" }
}
```

The password is written in the lab file plainly. The account exists because the
lab's own provisioning created it, so the same string already sits in the
provision script beside it; in a synthetic lab the secret is the lab author's.
That decision buys the absence of a credential store, a login verb and a
credential API. vmlab is not a security boundary between you and your guests.

`vmlab validate` adds four rules the schema cannot express:

- A `login` with no `password` on a Windows-family profile is an error. The
  agent is SYSTEM and every credential-free logon route is one Windows
  disqualifies.
- `elevated` on a Linux-family profile is an error. Root is root, and a
  non-root user cannot be elevated without sudo.
- More than one `login` with `default = true` on a machine is an error naming
  both.
- A machine with exactly one `login` has it as the default implicitly.

## Who runs as whom

Precedence: **CLI flag, then wscript, then `login {}`, then the agent
identity.**

- `vmlab exec --user admin` picks a declared label; `--user`/`--password`
  together name an account the lab file never declared, or a rotated password.
- In a script, `m.as_login("dev")` and `m.as_account(user, password)` return a
  second `Machine` handle whose every call runs as that identity.
- With none of those, the machine's default login applies.
- With no `login {}` at all, the floor applies: the agent identity, which is
  SYSTEM on Windows and root on Linux, or on a container the user cinit already
  resolves from the `user` field, the image's `USER`, or root.

The dividing rule is bootstrap. **Everything a person invokes defaults to the
declared login; everything vmlab does on its own behalf keeps the agent
identity.** Person-invoked: `ssh` and the facade, `exec`, `shell`, and
`vmlab cp`. vmlab's own: provisions, playbooks, share mounting, readiness,
metrics, tail and shutdown. `PROBE\dev` does not exist until provisioning
creates it, so provisioning running as the declared login could never stand up
its own domain. The one exception is the workspace syncer, which writes as the
default login because it produces the developer's files (see dev-machines.md).

Warning — a declared login changes what `exec` and `shell` can do: on a machine
that declares a `login {}`, `vmlab exec` and `vmlab shell` stop being SYSTEM or
root and run as that login. Writing into `C:\Windows\System32` starts failing
where it used to work. `--user SYSTEM` on Windows and `--user root` on Linux
name the agent identity explicitly and restore the old behaviour for one
command. `vmlab cp` is the exception: it still runs as the agent identity and
says so in its help.

Failure is loud and never a fallback. A declared account that does not exist,
or a wrong secret, fails naming the account and the machine. Falling back to
the agent identity would leave commands running as SYSTEM and writing into
`systemprofile` with no visible cause.

### Windows: a minted logon

The agent runs as LocalSystem and logs the declared account on itself with
`LogonUser` in network-cleartext mode. That mode yields a real initial Kerberos
ticket and genuine network credentials — the finding that moved the SSH server
to the host: a key-authenticated Windows sshd logs a domain account on without
credentials, so `\\dc\share` fails while the identity looks right. Batch and
service logons are refused outright and an interactive logon is refused on a
domain controller, so network-cleartext is the one mode that works on a DC and
a member alike.

Before spawning, the agent loads the user's profile with `LoadUserProfileW`,
which creates it on demand for a never-logged-on domain user; without that step
`USERPROFILE` would silently be `C:\Users\Default`. It enables the two
privileges SYSTEM holds disabled, and for `elevated = true` it uses the
account's linked token where one exists. Elevation defaults to true because the
parity bar is a devcontainer, which gives you root; `elevated = false` serves
testing as a standard user, and degrades the workspace in two named ways (see
dev-machines.md).

Logons are cached per (account, secret, machine), not per label, so two labels
naming one account share a session and a changed password mints a fresh logon
rather than failing against a stale token. A cached logon lives while any
channel uses it plus an idle grace, is recycled at idle once older than its
Kerberos ticket lifetime, and never survives the machine stopping; the profile
is unloaded when it goes. The lab's share credential is injected into each
minted logon before anything spawns, so an SMB share mapped by the agent opens
without a password prompt in the attached session.

### Linux: a real session

A Linux session is a real login, not a bare `setuid`. Where the guest has PAM,
the agent runs `su -l`, which opens a PAM session: that registers the login
with logind, gives it `XDG_RUNTIME_DIR`, applies limits and unlocks a keyring.
Where it does not — a BusyBox container or a stripped appliance — the agent
assembles by hand what PAM would have done: `HOME`, `USER`, `LOGNAME`, `SHELL`
and supplementary groups from the passwd entry, the working directory at
`HOME`, a login shell, and a `PATH` taken from `login.defs`. Which route ran is
named in the agent's log and the terminal banner, because "rootless podman does
not work here" is only answerable if you can see which one you got.

The password is not verified: root needs no credential to become an account,
which is why the container floor costs nothing. An account not in the guest's
passwd fails by name.

## The SSH facade

**vmlab terminates SSH on the host. The guest runs no sshd, holds no host key,
and needs no NIC.** The facade lives in the lab daemon beside the agent client
and the cached logon, and maps SSH channels onto agent channels. Its transport
is the agent's virtio-serial channel, so a machine on no segment at all is
attachable, and a VM and a container micro-VM are attached to the same way.

Path: `editor or ssh` → `vmlab ssh-proxy` → `SSH facade (labd)` →
`vmlab-agent (SYSTEM/root)` → your process, as the login.

The endpoint is a stdio `ProxyCommand` and nothing else. The hidden verb
`vmlab ssh-proxy <lab>/<machine>` asks the lab daemon for a unix socket and
connects that socket to its own stdin and stdout; one proxy per `ssh` or `scp`
invocation, nothing listening on the host, no port leased. `ssh-proxy` never
starts a machine: it is spawned by an editor with no TTY and its stderr may
never be shown, so it fails immediately with a diagnostic that survives being
printed into an editor log. `vmlab ssh` refreshes the managed SSH config block
(see dev-machines.md) and then runs the system `ssh` against the alias; it is
not a second SSH client, and it refuses on a stopped machine like `console` and
`exec` do.

### Auth is none, and the username is a selector

There is no network path to the facade, so the trust boundary is already "can
you exec the proxy against this lab socket". The facade offers the `none`
method and nothing else; OpenSSH's opening `none` probe is unconditional, so
`BatchMode`, `PasswordAuthentication=no` and the rest all still authenticate.

The username carries a login label: the generated alias for a non-default login
sets `User <label>`, and `ssh -l admin` picks one by hand. A username that names
no declared login — other than your own local username, which the facade reads
as "I named nobody" — gets a message naming the machine's declared logins and
the floor identity, then the connection ends.

vmlab owns a per-(lab, machine) host key and its own `known_hosts` under its
state directory, so your `~/.ssh/known_hosts` is never touched, a rebuilt
machine never triggers a host-key warning, and the key survives `destroy`.

### What it answers

| SSH request | Served by | Used by |
| --- | --- | --- |
| `pty-req`, `shell` | an agent terminal | plain `ssh`, an editor's terminal |
| `exec` | an agent exec, or a terminal hosting the command when a PTY was asked for | an editor's bootstrap script |
| `window-change` | a terminal resize | |
| `subsystem sftp` | host-side SFTP version 3, transcoded packet for request onto the agent's file session | `scp`, `sftp`, an editor's server push and file explorer |
| `env` | applied over the logon's environment, minus a deny list | `SendEnv LANG LC_*` |
| `direct-tcpip` | a TCP connection the guest dials | `ssh -D`, `ssh -L`, `ssh -W`, VS Code's whole protocol |
| `exit-status` | always sent, from the agent's exit code | `ssh` and `scp` exit codes |

The `env` deny list drops `HOME`, `USERPROFILE`, `USERNAME`, `LOGNAME` and
`SSH_AUTH_SOCK`, because a client-sent `USERPROFILE` would silently undo the
profile load that gave a never-logged-on domain user a home. `exit-signal` is
never sent: the agent reports `128 + signal` as a status, so a command killed
with signal 9 reports 137. `keepalive@openssh.com` gets a request failure,
which is the correct answer and what makes `ServerAliveInterval` work. Many
session channels per connection are expected, since `ControlMaster` puts them
there.

`direct-tcpip` is what makes editors work rather than a convenience: VS Code
runs `ssh -T -D <port>` and rides its entire protocol over that SOCKS forward.
The agent dials the address inside the guest, resolution included, so a domain
name in a SOCKS request works; there is no destination policy, and a failed
dial answers `SSH_OPEN_CONNECT_FAILED` rather than the prohibited code, so a
SOCKS client can tell "nothing is listening" from "vmlab refused you". SFTP
runs under the connection's own logon, the same cached logon as the shell, so
`scp` lands files owned by the login you attached as.

### What it refuses

One invariant decides every refusal: **the facade only ever answers a channel
open; it never initiates one.** The agent protocol has no guest-initiated
channel open, and vmlab does not add one. `forwarded-tcpip`,
`auth-agent@openssh.com` and `x11` are channel types the facade can never open,
which is why `ssh -R`, agent forwarding and X11 forwarding are refused. Each
would need a listener inside the guest with its own lifetime against a
multiplexer that outlives its client. Everything else — another subsystem, a
signal, a break — is refused because nothing in the client set sends it.

A channel request refusal carries no text in the SSH protocol, so those
refusals are narrated by the client: `-R` warns, `-X` warns, and agent
forwarding is refused in total silence, with `SSH_AUTH_SOCK` simply empty in
the guest. Every refused channel is also recorded on the lab event log as
`ssh.refused`, naming the machine, the request and the reason, so a refusal is
visible somewhere other than one developer's terminal.

The facade degrades per channel: an agent missing `fileops` still serves a
shell while `subsystem sftp` refuses by name, and one missing `tunnel` refuses
`direct-tcpip` the same way, each naming the rebuild and the repair verb. See
`attachable` and the failure ladder in dev-machines.md.

`ssh -R` is refused; NAT egress is the answer. An offline guest that needs a
host-side mirror, proxy or licence server does not need a reverse tunnel. Give
the machine a NIC on a segment with `nat = true`: the NAT engine terminates
guest flows in-process, so anything addressed off-segment reaches the host's own
address. `vmlab dev attach` and `vmlab ssh-config --print` print this note.

## Throughput and flow control

Everything rides one virtio-serial channel, and that is enough: measured into a
Windows guest, a one gigabyte push sustains around 80 MiB/s while `exec` round
trips on the same port grow by tens of milliseconds and never stall. The facade
never grants SSH window it cannot back with agent credit. Client bytes travel
one chunk at a time towards the agent and the SSH window is only re-granted once
the agent accepted the chunk, so a tens-of-megabytes editor server push is
throttled by the guest rather than buffered inside the lab daemon.
