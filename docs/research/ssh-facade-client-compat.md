# Do real editor clients accept the SSH facade?

Findings for [#68](https://github.com/VMLabDev/vmlab/issues/68), on the map
[Map: dev machines](https://github.com/VMLabDev/vmlab/issues/47).

Everything below was observed against a **stand-in facade** built for this
ticket, not against vmlab (§19 is unimplemented). The stand-in is in
`ssh-facade-client-compat/facade/`; logs and screenshots are in
`ssh-facade-client-compat/evidence/`.

## The rig

The stand-in reproduces the seam [#52](https://github.com/VMLabDev/vmlab/issues/52)
and [#60](https://github.com/VMLabDev/vmlab/issues/60) settled, and nothing else:

| Piece | Stands in for | Shape |
|---|---|---|
| `facade.py` (asyncssh) | `labd` terminating SSH | listens on a **Unix socket**, `none` auth, per-machine host key |
| `ssh-proxy.py` | `vmlab ssh-proxy` | stdio ↔ Unix socket **byte pipe**; parses no SSH |
| `vmlab-ssh-config` | `~/.config/vmlab/ssh/config` | `Host vmlab-<lab>-<machine>` + `ProxyCommand` + `ControlMaster` |

The "guest" behind the facade is a local `bash`, so this exercises the *client
half* of the contract only. It says nothing about the agent channel.

`facade.py` hooks asyncssh's `_packet_handlers` for `MSG_CHANNEL_OPEN`,
`MSG_CHANNEL_REQUEST` and `MSG_GLOBAL_REQUEST`, so every request a client sends
is logged whether it is answered or refused. That is the instrument behind the
request-set table.

## 1. `none` auth — CONFIRMED, and it cannot be talked out of

Stock OpenSSH 10.4p1 through the stdio `ProxyCommand`:

```
debug1: Executing proxy command: exec .../ssh-proxy.py --socket .../lab.sock vmlab-probe-dev01 22
debug1: Remote protocol version 2.0, remote software version AsyncSSH_2.24.0
debug1: Authenticating to vmlab-probe-dev01:22 as 'dev'
Authenticated to vmlab-probe-dev01 (via proxy) using "none".
```

The worry in #68 was a client injecting `PreferredAuthentications` and refusing
`none`. It cannot: OpenSSH's opening `none` probe is unconditional — it is how
the client *enumerates* methods — so a server that answers `SUCCESS` has
authenticated the session before any preference is consulted. Every one of
these still logged in:

| Client option | Result |
|---|---|
| `PreferredAuthentications=publickey` | authenticated |
| `PreferredAuthentications=publickey,password,keyboard-interactive` | authenticated |
| `PreferredAuthentications=password` | authenticated |
| `BatchMode=yes` | authenticated |
| `NumberOfPasswordPrompts=0` | authenticated |
| `PubkeyAuthentication=yes` + `PasswordAuthentication=no` | authenticated |

**VS Code, specifically:** Remote-SSH 0.124.0 (VS Code 1.131.0) shells out to
the system `ssh` and its own log records the result —

```
[11:59:23.347] stderr> debug1: Server host key: ssh-ed25519 SHA256:v1ActMon5FrK9RR1Dh7cdYhnD5CWTZ6Dk4rObcKajZM
[11:59:23.373] stderr> Authenticated to vmlab-probe-dev01 (via proxy) using "none".
```

It injects nothing that interferes. #52's choice stands.

## 2. VS Code attaches end to end — CONFIRMED

The editor reached "connected", started its extension host and opened a
workspace on the guest (`evidence/vscode-attached.png`; status bar reads
`SSH: vmlab-probe-dev01`).

Two details worth carrying into §19:

**`localServerDownload: "always"` behaves exactly as #49 said.** The 12 MB
server payload is fetched by the *client* and pushed over the facade:

```
Got request to download on client for {"artifact":"cli-alpine-x64", ...}
Downloading VS Code server locally...
Downloaded VS Code server to /tmp/...
Preparing to scp to host vmlab-probe-dev01
Copying file to remote with scp -o ConnectTimeout=90 -F '<vmlab config>' ...
```

That is the offline-guest story working, and it is the same transport an
extension install uses — so #61's "bake it, or push it from the client" holds at
the transport level.

**VS Code drives the connection over a `-D` SOCKS forward**, as #49 warned:

```
Running ssh connection command: ssh -v -T -D 45709 -o ConnectTimeout=90 -F <vmlab config> vmlab-probe-dev01
```

`-T`, so no PTY is ever requested; the protocol rides `direct-tcpip`. The
facade **must** serve `direct-tcpip` or VS Code does not work at all — this is
not an optional convenience for `ssh -L`.

## 3. The request set — matches #60 exactly

Observed across OpenSSH (interactive, exec, sftp, scp, `-L`, `-R`, `-X`, `-A`)
and a full VS Code attach:

| Request | Facade | Seen from |
|---|---|---|
| `channel-open session` | answer | both |
| `pty-req` | answer | OpenSSH interactive only |
| `shell` | answer | both (VS Code: no pty) |
| `exec` | answer | both |
| `subsystem` (`sftp`) | answer | both |
| `env` | answer | OpenSSH `SendEnv` |
| `channel-open direct-tcpip` | answer | both (VS Code: the `-D` protocol channel) |
| `x11-req` | refuse | OpenSSH `-X` |
| `auth-agent-req@openssh.com` | refuse | OpenSSH `-A` |
| `global tcpip-forward` | refuse | OpenSSH `-R` |

Nothing outside #60's set was requested by either client. `window-change`
never appeared because no client resized mid-session; it is in OpenSSH's
vocabulary and the facade should still answer it.

## 4. Refusals are narrated by the client — and the narration is uneven

#60 already says request-level refusals are the client's words, not vmlab's.
What the rig adds is *how much* the client says, at default `LogLevel`, and the
three cases are not alike:

| Refused | What the developer sees | Exit |
|---|---|---|
| `-R` (`tcpip-forward`) | `Warning: remote port forwarding failed for listen port 18200` | 0 — the session continues (255 only with `ExitOnForwardFailure=yes`) |
| `-X` (`x11-req`) | `X11 forwarding request failed on channel 0` | 0 — the session continues |
| `-A` (`auth-agent-req`) | **nothing at all** | 0 — the session continues |

Agent forwarding is the dangerous one: the request is sent, refused, and no
client prints a word about it. `SSH_AUTH_SOCK` is simply empty in the guest.
A developer who forwards their key to `git push` from a dev machine gets no
signal — only a later authentication failure with an unrelated-looking message.

(An earlier run appeared to show `-A` sending nothing; that was an artefact of
having no agent to forward. With a real `ssh-agent`, `auth-agent-req@openssh.com`
is sent, refused, and silent.)

**Suggestion for §19:** the one refusal worth spending vmlab's own words on is
agent forwarding, and the only place it can say them is the channel it *does*
open — a `USERAUTH_BANNER`, or a line from `vmlab dev attach`.

## 5. JetBrains Toolbox — `ProxyCommand` is honoured by delegation; `Include` is not read

Toolbox 3.6.3 (build 86383) with IDEs 2026.1.

**It reads `~/.ssh/config`**, live, logged on every start:

```
SshImportedConfigsProviderImpl Plugin ssh > Importing OpenSSH configs from: /home/wil/.ssh/config
```

**It connects by spawning the system OpenSSH binary.** `daemon-openssh-impl.jar`
contains `OpenSshProcess`, `SshExecutable`, `SshExecutableType`, `OpenSshConfig`,
`CommonOpenSshPathsProviderImpl` (locate the `ssh` binary), `OpenSshAskpassRunner`
+ `SSH_ASKPASS`/`SSH_ASKPASS_REQUIRE`, `SshAgentConflictDetector`; `gateway.jar`
has `ClientOverSshTunnelConnector`. **No JetBrains jar — Toolbox or IDE —
contains the string `ProxyCommand` anywhere.** That is the signature of a client
that hands the host alias to `ssh` and lets OpenSSH resolve the config, which is
how it gets `ProxyCommand` for free.

**But its config *importer* does not follow `Include`.** Tested with four
stanzas at once and a restart between:

| Host | Where | `ProxyCommand`? | In Toolbox's list |
|---|---|---|---|
| `vmlab-direct-proxy` | directly in `~/.ssh/config` | yes | **listed** |
| `vmlab-direct-plain` | directly in `~/.ssh/config` | no | **listed** |
| `vmlab-probe-dev01` | inside the `Include`d file | yes | **absent** |
| `vmlab-include-plain` | inside the `Include`d file | no | **absent** |

(`evidence/toolbox-probe.png`.) A `ProxyCommand` host imports fine, so
`ProxyCommand` is not the filter — `Include` is. The `Include` sat at the very
top of `~/.ssh/config`, so this is not an ordering artefact.

This bears directly on [#56](https://github.com/VMLabDev/vmlab/issues/56), which
chose "vmlab owns `~/.config/vmlab/ssh/config` and injects the `Include` at the
top of `~/.ssh/config`". **For JetBrains that makes every vmlab machine
invisible in the picker.** Note the split, because it is not a total failure:

- **Discovery** (Toolbox listing hosts) — needs `Include` support. Broken.
- **Connection** (Toolbox spawning `ssh <alias>`) — OpenSSH resolves the
  `Include` itself. Unaffected.

So a developer who types the alias by hand into "New SSH Connection" should
still connect. It is the "a never-started lab still fills the editor's picker"
half of #56 that `Include` does not deliver for JetBrains.

**There is a supported escape hatch.** Toolbox's ssh plugin has a `configPath`
setting (`Toolbox/plugins/ssh/settings.json`). Pointing it at vmlab's own file
works first try — both vmlab hosts appear, sourced
`From /run/user/1000/vf/vmlab-ssh-config` (`evidence/toolbox-configpath.png`).
The catch: it **replaces** `~/.ssh/config` rather than adding to it, so the
developer's own hosts vanish from the list. vmlab must not set it silently.

### What is *not* settled here

The final step — Toolbox actually opening a connection through the
`ProxyCommand` — was **not observed**. Toolbox is a Compose Desktop app and
exposes no accessibility tree on this KDE/Wayland session
(`orca computer get-app-state` → `app_not_found` for its pid; `list-apps` sees
only GTK apps), there is no input-injection tool installed, and the
`jetbrains://gateway/ssh?...` deep link only *navigates* to the SSH provider
page — `jetbrains://gateway/connect?...` is rejected outright
(`GatewayProtocolHandlerKt Cannot find a plugin for connect`). Every remaining
route needs one human click.

The evidence short of that click is strong but circumstantial: Toolbox reads the
config, spawns the system `ssh`, and nowhere implements `ProxyCommand` itself.
See the checklist at the end to close it in about two minutes.

## 6. Incidental: `ControlPath` and the 108-byte limit

`ControlPath` is a Unix socket path, so it is bounded by `sun_path` — 108 bytes
on Linux. The first rig hit it:

```
ControlPath too long ('/tmp/claude-.../facade/mux/vmlab-probe-dev01' >= 108 bytes)
```

#56 already disqualified `<lab>/<machine>` as an alias because a slash breaks
`ControlPath` via `%n`. The length ceiling is the same class of problem and is
not yet written down: `~/.config/vmlab/ssh/mux/%n` plus
`vmlab-<lab>-<machine>` leaves roughly 60 characters for lab + machine names
under a normal home directory, and less under a long one. Worth a bounded
`ControlPath` (a short hash rather than `%n`), or a named length limit.

## Closing the JetBrains gap by hand

Rebuild the rig and take the one click:

```bash
# 1. stand the facade up
mkdir -p /run/user/$(id -u)/vf/{run,mux,guest}
cp docs/research/ssh-facade-client-compat/facade/* /run/user/$(id -u)/vf/
python3 -m venv /run/user/$(id -u)/vf/venv
/run/user/$(id -u)/vf/venv/bin/pip install asyncssh
/run/user/$(id -u)/vf/ctl.sh start          # edit paths in ctl.sh/vmlab-ssh-config first

# 2. paste the vmlab-probe-dev01 stanza DIRECTLY into ~/.ssh/config
#    (not via Include — see §5), then restart Toolbox.

# 3. Toolbox -> SSH -> Import "vmlab-probe-dev01" -> pick an IDE -> connect.

# 4. the answer is in the proxy log; each line records the parent process:
cat /run/user/$(id -u)/vf/run/proxy.log
```

If a line appears whose `parent=` is a JetBrains-spawned `ssh`, `ProxyCommand`
is honoured and the claim is confirmed. If Toolbox instead reports it cannot
resolve the host `vmlab-probe-dev01`, it is refuted.
