# Logins

A `login {}` block declares an identity on a machine; vmlab logs on as that
identity whenever a person reaches into the guest. The `login {}` block's
field reference is in vm.md; the verbs that take a login are `vmlab exec` and
`vmlab shell` (see cli-machine.md), and the wscript rung is `as_login` and
`as_account` (see wscript-machine-api.md).

## Declaring a login

Identity is a property of the machine, not of the command that reaches in. A repeatable
`login "<label>" { user, password?, elevated?, default? }` block on a `vm` or
`container` names a guest account and, on Windows, its secret.

The label selects the login: `--user` on `exec` and `shell` and `as_login` in a
script all take the label, and with neither the machine's default login
applies. The raw account name is accepted as an alias for the label. One
account may be declared twice at different elevation under two labels.

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
identity.** Person-invoked: `exec`, `shell`, and `vmlab cp`. vmlab's own:
provisions, playbooks, share mounting, readiness, metrics, tail and shutdown. `PROBE\dev` does not exist until provisioning
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
ticket and genuine network credentials, so `\\dc\share` works under the
minted identity; a credential-free logon would leave the identity looking right
while network access fails. Batch and
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
without a password prompt in the minted session.

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
