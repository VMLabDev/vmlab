# Remote-SSH into a Windows guest: what the editor's backend actually needs

Research for [#49](https://github.com/VMLabDev/vmlab/issues/49) (part of map
[#47](https://github.com/VMLabDev/vmlab/issues/47) — dev machines). Investigated 2026-08-02.

Everything below is sourced from primary material: vendor documentation, vendor source
repositories, the shipped Remote-SSH extension bundle itself, and first-party issue
trackers. Where a claim rests on a user report rather than a vendor statement, it says so.
Where nothing primary could settle a question, it is in
[Unverified / needs testing](#unverified--needs-testing) instead of being guessed at.

Version pins for the artefacts inspected:

- VS Code Remote-SSH extension `0.125.2026072315` (packaged 2026-07-23), pulled from the
  Marketplace gallery endpoint
  `https://marketplace.visualstudio.com/_apis/public/gallery/publishers/ms-vscode-remote/vsextensions/remote-ssh/latest/vspackage`
  ([extension page](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-ssh)).
  Quotes from it are from `extension/package.json`, `extension/package.nls.json` and
  `extension/out/resolver.js` inside that VSIX.
- VS Code docs as of `DateApproved: 7/29/2026`
  ([source](https://github.com/microsoft/vscode-docs/blob/main/docs/remote/ssh.md)).
- IntelliJ IDEA 2026.2 help; Toolbox App help (pages dated to 2026).
- Zed docs from `main`
  ([source](https://github.com/zed-industries/zed/blob/main/docs/src/remote-development.md)).

---

## Bottom line

**1. The attach contract survives contact with Windows, but only for VS Code and JetBrains
Toolbox.** VS Code Remote-SSH officially supports a Windows remote host. JetBrains supports
a Windows *backend* only through the **Toolbox App**, not through Gateway ("only Linux
servers are supported as suitable for the backend" for Gateway). Zed does not support a
Windows remote at all. So "any SSH-capable client attaches" is true in the abstract, but for
the proving case — Windows on a domain — the realistic client set today is **VS Code
Remote-SSH and JetBrains Toolbox App**, plus plain `ssh`.

**2. The "no guest network involved" property holds — but only if vmlab says so explicitly.**
By default Remote-SSH downloads the server *on the guest*, which needs guest internet. But
the client can be told to download locally and push over the SSH connection with `scp`:
`"remote.SSH.localServerDownload": "always"` (there is also `"off"`, which forbids the local
path). This is a **client-side setting on the developer's machine**, not something the guest
can advertise. Design consequence: vmlab must *tell the developer to set it* (or write it
into a workspace/user settings snippet it generates alongside the SSH alias) — a dev machine
cannot make itself work offline unilaterally. The same shape holds for Zed
(`upload_binary_over_ssh: true`, per-connection in the client's settings) and for Toolbox
(`cli_dir_path` in a `ToolboxSshDeploy/env.ps1` override file — that one *is* guest-side).

**3. Extensions are the leak, not the server.** Even with the server pushed over SSH,
installing extensions from the panel wants outbound HTTPS to `marketplace.visualstudio.com`
from *both* client and server, per Microsoft's own connectivity list. A VS Code maintainer
stated in Feb 2025 that extension install now falls back to local download + copy when the
remote can't reach the marketplace, but that fallback is not documented on the docs site.
If vmlab wants a genuinely air-gapped dev machine, extension provisioning is a second,
separate problem from server provisioning.

**4. The guest sshd requirements are small, specific, and mostly defaults.** For a Windows
guest: inbox OpenSSH Server, **`DefaultShell` left unset (i.e. `cmd.exe`)**,
`AllowTcpForwarding yes` (default), the `sftp` subsystem enabled (default), and
`administrators_authorized_keys` handled correctly if the dev user is an admin. `powershell`
must resolve on `PATH` — Remote-SSH literally runs `ssh <host> powershell` and pipes a
PowerShell bootstrap script into it. Windows OpenSSH offers only `password` and `publickey`
authentication, which suits a vmlab-owned keypair.

**5. ProxyCommand over stdio is fine for VS Code and Zed, and unverified for JetBrains.**
Both VS Code and Zed shell out to the system `ssh` binary and pass a bare host alias, so
`ProxyCommand` is resolved entirely inside OpenSSH — neither client ever dials a TCP port
itself. This is the strongest single result for the transport ticket
([#52](https://github.com/VMLabDev/vmlab/issues/52)): **a stdio ProxyCommand is a viable
transport for the dev-machine endpoint**, with two caveats — the clients spawn *multiple*
`ssh`/`scp` processes per session (so the ProxyCommand must be concurrently invocable and
cheap), and the guest sshd must still permit TCP forwarding because VS Code tunnels its
protocol over an SSH dynamic-forward (`ssh -D`) rather than over the session channel.

**6. Nothing here is settled for the *domain* part.** Every claim about domain-joined
Windows in this document is about SSH mechanics (`user@domain@host` syntax, `domain\user`
matching rules, GSSAPI availability). Whether a domain profile, roaming profile, or
domain-scoped `%USERPROFILE%` breaks the server install is untested — see
[Unverified](#unverified--needs-testing).

---

## 1. VS Code Remote-SSH against a Windows remote

### 1.1 Support matrix

The docs list Windows as a supported SSH host under **System requirements**:

> **Remote SSH host**: A running SSH server on:
> […]
> - Windows 10 / Server 2016/2019 (1803+) using the [official OpenSSH Server](https://learn.microsoft.com/windows-server/administration/openssh/openssh_install_firstuse).
> […]
> - 1 GB RAM is required for remote hosts, but at least 2 GB RAM and a 2-core CPU is recommended.

— [Remote Development using SSH](https://code.visualstudio.com/docs/remote/ssh)

Windows-remote support is no longer experimental. The extension manifest still carries the
old opt-in flag, marked deprecated:

> `remote.SSH.windowsRemotes` — **Deprecated**: Enables experimental support for connecting
> to Windows remotes. Add the names of windows remotes to this list.

— `package.nls.json`, Remote-SSH `0.125.2026072315`

Caveats on the matrix itself:

- **The version list is stale relative to Microsoft's own OpenSSH support.** Microsoft states
  OpenSSH "was added to Windows Server and Windows client operating systems starting with
  Windows Server 2019 and Windows 10 (build 1809)"
  ([OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_server_configuration)),
  so the VS Code doc's "Server 2016 (1803+)" implies the out-of-band Win32-OpenSSH build, not
  the inbox one. Windows 11, Server 2022 and Server 2025 are **not named** in the VS Code
  matrix. Beginning with Windows Server 2025 "OpenSSH is now installed by default"
  ([Get started with OpenSSH Server for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse)).
  Treat "modern Windows works" as extremely likely but undocumented.

### 1.2 What is degraded on a Windows remote versus Linux

All from first-party docs:

- **Unix-socket server mode is unavailable.** "Ensure you have a local OpenSSH 6.7+ SSH client
  on Windows, macOS, or Linux and an **OpenSSH 6.7+ Linux or macOS Host (Windows does not
  support this mode)**"
  ([Tips and Tricks → Improving security on multi-user servers](https://code.visualstudio.com/docs/remote/troubleshooting)).
  The extension manifest agrees: `remote.SSH.remoteServerListenOnSocket` is "Only valid for
  Linux and macOS remotes… Requires `AllowStreamLocalForwarding` to be enabled for the SSH
  server." Microsoft lists `AllowStreamLocalForwarding` among the directives that "aren't
  available in the OpenSSH version that ships in Windows Server and Windows"
  ([openssh_server_configuration](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_server_configuration)).
  Consequence: on Windows the server always listens on a **loopback TCP port**, forwarded to
  the client.
- **Dev Containers on top of Remote-SSH is Linux/macOS only.** "If you are using a Linux or
  macOS SSH host, you can use the Remote - SSH and Dev Containers extensions together to open
  a folder on your remote host inside of a container"
  ([Remote Development using SSH](https://code.visualstudio.com/docs/remote/ssh)).
- **A documented Windows-sshd interop bug in host detection.** "Due to a bug in certain
  versions of OpenSSH server for Windows, the default check to determine if the host is
  running Windows may not work properly… A fix has been merged so this problem should be
  resolved in a version of the server greater than 8.1.0.0." The documented workarounds are
  `"remote.SSH.useLocalServer": false` and pinning
  `"remote.SSH.remotePlatform": { "host": "windows" }`
  ([Tips and Tricks → Troubleshooting hanging or failing connections](https://code.visualstudio.com/docs/remote/troubleshooting)).
  `remote.SSH.remotePlatform` is a **client-side** map keyed by the host alias — so vmlab can
  hand the developer a snippet that pins its dev-machine alias to `windows` and skip the
  detection path entirely.
- **Extension architecture caveat**: on ARM64 Windows the artefact is `server-win32-arm64`
  (see §1.3); the general caveat about x86-only native code in extensions applies.

### 1.3 How the server gets into the guest — exactly

The extension generates a PowerShell bootstrap script, pipes it over SSH, and runs it. The
remote command is chosen purely by detected platform (from `out/resolver.js`):

```js
const c = t.platform === h.Platform.Windows ? "powershell"
        : (t.platform === h.Platform.Linux || t.platform === h.Platform.MacOS) ? "bash"
        : undefined;
```

A user-posted client log in
[microsoft/vscode-remote-release#11613](https://github.com/microsoft/vscode-remote-release/issues/11613)
shows the resulting invocation verbatim (user report, not a vendor statement, but it matches
the code above):

```
Running script with connection command: "…\OpenSSH\ssh.exe" -T -D 64325 "<host>" powershell
Generated SSH command: 'type "…\vscode-linux-multi-line-command-<host>-….sh" | "…\ssh.exe" -T -D 64325 "<host>" powershell'
```

Inside that script (all quotes below are the literal template in `out/resolver.js` of
Remote-SSH `0.125.2026072315`):

- **Install root**: `$serverRoot = (Join-Path (Resolve-Path ~) '<serverDataFolderName>')`.
  `serverDataFolderName` comes from the *client's* `product.json`; for Microsoft's stable
  build the docs use `~/.vscode-server`
  ([Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting) uses
  `rm -rf $HOME/.vscode-server # Or ~/.vscode-server-insiders`), and the OSS build's
  [`product.json`](https://github.com/microsoft/vscode/blob/main/product.json) shows the
  mechanism (`"serverDataFolderName": ".vscode-server-oss"`).
- **Per-commit directory**: `$sDir = "$serverRoot\bin\$commitId"`, presence probe
  `$sFile = "$serverRoot\bin\$commitId\bin\<serverApplicationName>.cmd"`.
- **Host-side download**:
  `Uri = "https://update.code.visualstudio.com/commit:$commitId/$serverName/$quality"`
  where `$serverName = "server-win32-$arch"` and `$arch` is derived from
  `$env:PROCESSOR_ARCHITECTURE` (`x64` for `AMD64`/`IA64`, `arm64` for `ARM64`; anything else
  is a hard `UnsupportedArch` failure).
- **The skip-if-present branch**, which is the whole basis of any pre-staging scheme:

  ```powershell
  "Looking for existing server in $sDir"
  if (Test-Path "$sFile") { "Found installed server" } else { … download … }
  ```
- **Client-side download and push**: `DoLocalDownload` emits markers the client watches for
  (`artifact==…==`, `destFolder==$serverRoot\bin\==`,
  `destFolder2==$commitId\vscode-server.zip==`), then blocks:

  ```powershell
  "Waiting for $sDir\vscode-server.zip.done and vscode-server.zip to exist"
  ```

  The client transfers the archive with `scp` — the extension resolves an `scp` binary
  (`ScpNotFoundError`, `generateScpCommand`) and its own setting description says so:

  > `remote.SSH.localServerDownload` — Whether the extension can download the VS Code Server
  > on the client and transfer it to the host **with scp**, instead of downloading it on the
  > host.

  — `package.nls.json`. The setting is an enum `['auto','always','off']`, default `auto`.
- **Newer default path uses the CLI first.** `remote.SSH.useExecServer` defaults to `true`
  ("Uses the a new bootstrapping mode when connecting to a server. Can be toggled off in the
  event of connection issues"). In that mode the first artefact fetched is
  `cli-win32-$arch`, installed as `$serverRoot\code-<commitId>.exe` and launched as
  `command-shell --cli-data-dir … --parent-process-id $sshdPID --on-host … --on-port
  --require-token …`. The same `allowLocalDownload` / `forceLocalDownload` flags govern it,
  with the transfer marker `vscode-cli-$commitId.zip.done` in `$serverRoot`.
- **The server is started detached** via
  `Start-Process powershell.exe -WindowStyle hidden -ArgumentList -ExecutionPolicy Unrestricted -NoLogo -NoProfile -NonInteractive -c "<serverfile> --start-server --host=… --server-data-dir … --accept-server-license-terms --enable-remote-auto-shutdown --port=0 --connection-token-file … "`
  and it self-terminates when its watched sshd parent PID disappears.
- **The connection token is ACL'd with `icacls`** to the connecting identity
  (`[System.Security.Principal.WindowsIdentity]::GetCurrent().Name`) — relevant to the domain
  case, since that name will be `DOMAIN\user`.

The vendor-level summary of the same behaviour:

> Installation of VS Code Server requires that your local machine have outbound HTTPS (port
> 443) connectivity to: `update.code.visualstudio.com`, `vscode.download.prss.microsoft.com`.
>
> By default, the Remote - SSH will attempt to download on the remote host, and fail back to
> downloading VS Code Server locally and transferring it remotely once a connection is
> established. You can change this behavior with the `remote.SSH.localServerDownload` setting
> to always download locally and then transfer it, or to never download locally.

— [Remote Development FAQ](https://code.visualstudio.com/docs/remote/faq)

Note the docs contradict themselves in one place. The troubleshooting page still says flatly:

> **Ensure the remote machine has internet access** — The remote machine must have internet
> access to be able to download the VS Code Server and extensions from the Marketplace.

— [Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting)

The FAQ text and the shipped bootstrap script are the more specific and more recent
statements; treat the troubleshooting line as describing the default, not a hard requirement.

**Answer to the ticket's question 2, in one line: the download happens on the guest by
default, and can be moved entirely to the client with a documented client-side setting.**

### 1.4 Offline / air-gapped

There is **no documented air-gapped install path**. The canonical feature request,
[microsoft/vscode-remote-release#9454](https://github.com/microsoft/vscode-remote-release/issues/9454)
("Preinstalled server+extensions environment for SSH hosts (similar to 'offline'/airgapped
host)"), is **open** and in the backlog; the air-gapped ticket
[#11133](https://github.com/microsoft/vscode-remote-release/issues/11133) was closed as a
duplicate of it by a VS Code team member.

What *is* on the record from Microsoft, in that thread:

> To follow up here, we have several installation modes for the server itself, but extension
> installation doesn't have the same fallbacks in these contrained environments

— [joshspicer, 2024-12-16](https://github.com/microsoft/vscode-remote-release/issues/9454#issuecomment-2546143419)

> The latest insiders will now fallback to installing extensions locally and copying to the
> remote in cases where it's impossible to do so on the remote directly

— [joshspicer, 2025-02-21](https://github.com/microsoft/vscode-remote-release/issues/9454#issuecomment-2675217954)

So the honest position is a three-tier one:

| Tier | Mechanism | Support status |
|---|---|---|
| Client downloads, pushes over SSH with `scp` | `"remote.SSH.localServerDownload": "always"` | **Documented and supported.** Guest needs no internet. Client does. |
| Pre-stage the server in the guest image | Drop the unpacked `server-win32-<arch>` tree at `%USERPROFILE%\.vscode-server\bin\<commit>\` so the `Test-Path "$sFile"` probe succeeds | **Mechanically real, officially unsupported** (that's exactly what #9454 asks for). |
| Relocate the install root | `remote.SSH.serverInstallPath` — "A map of remote host to absolute path where the VS Code server will be installed" | Documented setting; still per-commit inside that path. |

**Survival across client updates: it does not.** The artefact URL and the on-disk directory
are both keyed by `$commitId`, which is the client's build commit. Every VS Code client
update invalidates a pre-staged tree, and the extension will fall straight back to
downloading. Any vmlab pre-staging design must either pin the client version or accept
re-staging per update — which argues strongly for the `localServerDownload: always` tier
instead, since that one is version-agnostic by construction.

Extensions remain a separate hole:

> You can install extensions manually without an internet connection using the
> **Extensions: Install from VSIX...** command, but if you use the extension panel or
> `devcontainer.json` to install extensions, your local machine and VS Code Server will need
> outbound HTTPS (port 443) access to: `marketplace.visualstudio.com`,
> `*.gallerycdn.vsassets.io` (Azure CDN)

— [Remote Development FAQ](https://code.visualstudio.com/docs/remote/faq)

Also worth knowing for the "vmlab does not ship editor servers" boundary already settled in
#47: Microsoft explicitly forbids redistribution-style use of the server.

> **Can VS Code Server be installed or used on its own?** No. The VS Code Server is a
> component of the Remote Development extensions and is managed by a VS Code client. It is
> installed and updated automatically by VS Code when it connects to an endpoint and if
> installed separately could become quickly out of date. It is not intended or licensed for
> use by other clients.

— [Remote Development FAQ](https://code.visualstudio.com/docs/remote/faq)

That sentence is a good reason for vmlab to stop at "here is an SSH endpoint" and let the
client push its own server, rather than baking one into a template.

### 1.5 What the guest's sshd must look like

| Requirement | Source | Notes for vmlab |
|---|---|---|
| Inbox OpenSSH Server (`OpenSSH.Server~~~~0.0.1.0` capability; installed by default on Server 2025) | [Get started with OpenSSH Server for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse) | Also creates/enables firewall rule `OpenSSH-Server-In-TCP` on port 22. |
| Default shell **must effectively be `cmd.exe`** (leave `DefaultShell` unset) | [openssh_server_configuration](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_server_configuration): "The initial default in Windows is the Windows command prompt (cmd.exe)"; overridden by the string value `DefaultShell` under `HKEY_LOCAL_MACHINE\SOFTWARE\OpenSSH` | Remote-SSH runs the remote command `powershell`; a non-`cmd` default shell changes how that command is parsed/escaped. See the known-issue row below. |
| `powershell.exe` (Windows PowerShell 5.1) resolvable on `PATH` | The bootstrap script is Windows PowerShell and uses `Expand-Archive`, `Invoke-RestMethod`, `Start-Process`, `Get-CimInstance`, `icacls` | Not `pwsh`. The script also re-invokes `powershell.exe` by name to start the server. |
| `AllowTcpForwarding yes` | [Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting): "Remote - SSH extension makes use of an SSH tunnel… Add the setting `AllowTcpForwarding yes`" in `C:\ProgramData\ssh\sshd_config`. Windows' shipped default config leaves it at the OpenSSH default of yes ([`contrib/win32/openssh/sshd_config`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/openssh/sshd_config): `#AllowTcpForwarding yes`) | Symptom if missing: `open failed: administratively prohibited: open failed`. |
| `sftp` subsystem enabled | Windows' shipped `sshd_config` has `Subsystem sftp sftp-server.exe` ([same file](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/openssh/sshd_config)) | Needed for the `scp` push path (modern OpenSSH `scp` speaks SFTP). |
| Authorized keys in the right file for the user's group | [openssh_server_configuration](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_server_configuration): "If the user belongs to the administrator group, `%programdata%/ssh/administrators_authorized_keys` is used instead", and that file must be ACL'd to `NT Authority\SYSTEM` + `BUILTIN\Administrators` only | VS Code's own docs give both variants of the key-copy command ([Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting)). vmlab provisioning must branch on whether the dev user is a local admin. |
| Auth methods | "For Windows OpenSSH, the only available authentication methods are `password` and `publickey`. […] Authentication via a Microsoft Entra account isn't currently supported." | A vmlab-owned keypair is the natural fit. |
| Nothing exotic in the login path | [Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting): "Some users launch a different shell from their `.bash_profile` or other startup script on their SSH host… This can break VS Code's remote server install script and isn't recommended." | Stated for Unix, but the principle (the bootstrap needs a clean, non-interactive stdout) applies to a Windows profile that prints banners too. |

Known shell incompatibilities, from the first-party tracker (open issues, user-reported, no
vendor diagnosis yet):

- [#11388](https://github.com/microsoft/vscode-remote-release/issues/11388) — "Cannot connect
  to windows host with git bash as default shell" (open).
- [#11613](https://github.com/microsoft/vscode-remote-release/issues/11613) — "…hangs at
  'Connecting with SSH timed out' on Windows host with PowerShell DefaultShell when
  `useLocalServer=false`" (open). Comments in that thread report the same hang with
  `DefaultShell` *unset*, and report `"remote.SSH.useLocalServer": true` (the shipped default)
  as the workaround.

Win32-OpenSSH's own knobs for non-default shells, for completeness:
`DefaultShellCommandOption` ("switch that the configured default shell requires to execute a
command… By default this is `-c`") and `DefaultShellEscapeArguments`
([Win32-OpenSSH wiki: DefaultShell](https://github.com/PowerShell/Win32-OpenSSH/wiki/DefaultShell)).
Both are reasons to not touch `DefaultShell` on a vmlab dev machine.

Windows-side scope limits that could bite: the Win32-OpenSSH project lists
"Authentication forwarding" (agent forwarding) and "Client ControlMaster" as scoped out
([Project Scope](https://github.com/PowerShell/Win32-OpenSSH/wiki/Project-Scope)). Agent
forwarding into the guest therefore should not be assumed; `ControlMaster` is a *client*-side
feature and the vmlab host is Linux, so multiplexing on the host side is unaffected.

### 1.6 Domain-joined specifics that are on the record

- Login syntax: "`ssh user@domain@hostname` — Or for Windows when using a domain / AAD
  account" ([Remote Development using SSH](https://code.visualstudio.com/docs/remote/ssh));
  Microsoft's own quickstart uses `ssh domain\username@servername`
  ([openssh_install_firstuse](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse)).
- sshd allow/deny rules for domain principals: "When configuring user/group-based rules with
  a domain user or group, use the following format: `user?domain*`… Domain users and groups
  are strictly resolved to NameSamCompatible format `domain_short_name\user_name`. […] All
  account names must be specified in lower case."
  ([openssh_server_configuration](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_server_configuration))
- Kerberos: "GSSAPI is only available starting in Windows Server 2022, Windows 11, and
  Windows 10 (May 2021 Update)", and `GSSAPIAuthentication` defaults to `no` (same page).
  Note the tension with "the only available authentication methods are `password` and
  `publickey`" earlier on that page — Microsoft's page states both. Do not plan on Kerberos
  SSO into the dev machine without testing it.

---

## 2. JetBrains

JetBrains has **two** remote-development front ends and they differ on exactly the question
this ticket asks.

### 2.1 Gateway: Linux backends only

> **What are the current limitations of the implementation?** If you use the JetBrains Gateway
> connection, only Linux servers are supported as suitable for the backend. If you use the
> JetBrains Toolbox App, macOS and Windows servers are also supported.

— [FAQ about remote development, IntelliJ IDEA 2026.2](https://www.jetbrains.com/help/idea/faq-about-remote-development.html)

Rider's requirements page states the remote-host prerequisites for the Gateway path plainly:

> **Prerequisites for the remote host** — You have installed a compatible SSH server on the
> Linux platform. […] You need to have the `sftp` subsystem enabled on the remote host.

— [System requirements for remote development (Rider)](https://www.jetbrains.com/help/rider/Prerequisites.html)

Backend delivery and connectivity for Gateway:

> By default, the downloaded IntelliJ IDEA is located in the following folder on the remote
> server: `~/.cache/JetBrains/RemoteDev/dist`. However, you can change it… Click **Upload IDE
> and Connect**. JetBrains Gateway downloads the IDE, and opens your remote project in
> JetBrains Client.

— [Connect and work with JetBrains Gateway](https://www.jetbrains.com/help/idea/remote-development-a.html)

> Both remote server and local computer, **or only the local one**, must have a network
> connection to JetBrains' URLs from this list: `code-with-me.jetbrains.com`,
> `download.jetbrains.com`, `download-cf.jetbrains.com`, `download-cdn.jetbrains.com`,
> `cache-redirector.jetbrains.com`, `data.services.jetbrains.com/products`

— [FAQ about remote development](https://www.jetbrains.com/help/idea/faq-about-remote-development.html)

The "or only the local one" clause is JetBrains stating that a guest with no internet is a
supported configuration for Gateway. There is also a documented air-gapped procedure —
[Fully offline mode](https://www.jetbrains.com/help/idea/fully-offline-mode.html) — using the
JetBrains Client Downloader plus `productsInfoUrl` / `clientDownloadUrl` / `jreDownloadUrl` /
`pgpPublicKeyUrl` override files, and note two limits stated there: "The script supports Linux
only" and "creating separate files is applicable for macOS and Linux only". And a
pre-registration path exists:

> **Can I point Remote Development to use an existing IDE on my remote server?** Since version
> 221.5481, you can manually register an existing backend IDE on the remote server and make it
> visible for the Gateway.

— [FAQ about remote development](https://www.jetbrains.com/help/idea/faq-about-remote-development.html)

None of that helps a Windows guest, because Gateway will not run a backend there at all.

### 2.2 Toolbox App: Windows backends supported, with a documented offline path

> **SSH server connection requirements** — You can connect to a remote project running on
> macOS, Linux, or Windows. […] If you use the Windows server, refer to requirements suggested
> in the OpenSSH for Windows documentation. […] OpenSSH server, version 7.9p1 or later is
> recommended. Other SSH servers fully implementing RFC 4254 may work too, yet are not
> supported. **SSH port forwarding must be enabled in the server configuration.**

— [Toolbox App: Installation](https://www.jetbrains.com/help/toolbox-app/installation.html)

Two things to flag on that page. First, it defers *entirely* to Microsoft for what a Windows
remote needs — there is no JetBrains-authored Windows guest checklist. Second, it contains
this line, listed under the Windows section of *client* system requirements but written as if
about a login shell:

> In Toolbox App version 2.6, PowerShell Core is not supported as the default login shell on
> Windows.

Its placement makes it ambiguous whether it constrains the client or the remote host. Either
way it is one more vote for leaving `DefaultShell` at `cmd.exe`.

Delivery and the offline path are documented in detail, including Windows paths:

> The proper distribution of the **Toolbox Agent** must be manually provided into the proper
> directory or must be accessible from either public or internal server. Currently, we require
> the Toolbox Agent to be exactly the same version as the Toolbox App. […] The URL to Toolbox
> Agent has the format:
> `https://download.jetbrains.com/toolbox/agent/tbcli-{os}-{arch}-{build_number}.tar.gz`
> — os: **win32**, linux, mac; arch: x64, aarch64
>
> **Manual provision** — In order to provision the artifacts manually, you have to override
> only `cli_dir_path`. The Agent distribution contains the Java runtime which will be
> automatically detected.
>
> **How to provide variables** — The only way to provide those environment variables is by
> using an env override file. […] `%ALLUSERSAPPDATA%/JetBrains/ToolboxSshDeploy/env.ps1` […]
> `%LOCALAPPDATA%/JetBrains/ToolboxSshDeploy/env.ps1` […] We recommend using the PowerShell
> variables and not environment variables on Windows.
>
> **IDEs** — Besides the Toolbox App and Toolbox Agent, in an air-gapped environment one also
> has to provide the IDE on the remote side and the compatible client on the local side.

— [Offline remote development in the Toolbox App](https://www.jetbrains.com/help/toolbox-app/offline-remote-development-in-the-toolbox-app.html)

This is materially better than VS Code's story for vmlab's purposes: it is a **guest-side**
configuration (`%LOCALAPPDATA%\JetBrains\ToolboxSshDeploy\env.ps1` plus a preinstalled-tools
JSON under `%LOCALAPPDATA%/JetBrains/Toolbox/preinstalled`), which a vmlab template or
provision script could bake in without the developer configuring anything. The same page
documents `environment.json` for locking down installs/updates on the remote, further
described in
[Add remotely installed IDE to the Toolbox App](https://www.jetbrains.com/help/toolbox-app/add-remote-ide-to-tba.html).

Note that the Toolbox SSH connection flow is currently in flux: "Starting with the Toolbox App
3.6 Early Access Program (EAP), SSH connections to remote environments use the JetBrains
Daemon (jetbrainsd) by default"
([Connect to SSH environment through jetbrainsd](https://www.jetbrains.com/help/toolbox-app/connect-to-ssh-environment-through-jetbrainsd.html)).
Any vmlab guidance pinned to Toolbox behaviour should expect churn.

### 2.3 JetBrains and ProxyCommand

Gateway parses the SSH config only if you opt in — "**Parse config file `~/.ssh/config`**:
select this option if you want JetBrains Gateway to use the `.ssh/config` file"
([Connect and work with JetBrains Gateway](https://www.jetbrains.com/help/idea/remote-development-a.html))
— and JetBrains documents no `ProxyCommand` support. Historical tickets
([GTW-811](https://youtrack.jetbrains.com/issue/GTW-811),
[CWM-6654](https://youtrack.jetbrains.com/issue/CWM-6654)) are closed as **Obsolete** and
**Duplicate** respectively, not as Fixed, so they establish nothing either way about the
current build.

For Toolbox the FAQ says it uses "the integrated OpenSSH for secure and customizable SSH
connections"
([FAQ about remote development](https://www.jetbrains.com/help/idea/faq-about-remote-development.html)),
which would imply full `ssh_config` semantics including `ProxyCommand` — but JetBrains does
not spell that out, and "integrated" leaves open whether it is the system `ssh` binary or a
bundled one. **Unverified**; see below.

---

## 3. Zed

Zed is the clearest and the least useful for the Windows case.

> **Supported platforms** — The remote machine must be able to run Zed's server. The following
> platforms should work…
> - macOS Catalina or later (Intel or Apple Silicon)
> - Linux (x86_64 or arm64, we do not yet support 32-bit platforms)
> - **Windows is not yet supported as a remote server**, but Windows can be used as a local
>   machine to connect to remote servers.

— [Zed docs: Remote Development](https://zed.dev/docs/remote-development)
([source](https://github.com/zed-industries/zed/blob/main/docs/src/remote-development.md))

For completeness, since Linux dev machines are in scope for #47 too:

- Backend delivery: "Zed will check to see if the remote server binary is present in
  `~/.zed_server` on the remote… If it is not there or the version mismatches, Zed will try to
  download the latest version. By default, it will download from `https://zed.dev` directly,
  but if you set `{"upload_binary_over_ssh":true}` in your settings for that server, it will
  download the binary to your local machine and then upload it to the remote server."
- Offline: manual placement is documented — "you must upload it to
  `~/.zed_server/zed-remote-server-{RELEASE_CHANNEL}-{VERSION}` on the server… The version must
  exactly match the version of Zed itself you are using."
- Transport: "Once you provide the SSH options, Zed shells out to `ssh` on your local machine
  to create a ControlMaster connection… Zed shells out to the `ssh` on your path, and so it
  will inherit any configuration you have in `~/.ssh/config` for the given host." Permitted
  extra args explicitly include `-o`, `-F`, `-J`, `-w`; `-t`/`-T` are deliberately disallowed
  because Zed sets them.

So Zed's design is the closest match to what vmlab wants (client-side push, host `ssh`
binary, ssh_config inherited) — it just can't target Windows.

---

## 4. The ProxyCommand / stdio-transport question (#52's blocker)

**What OpenSSH itself promises.** `ProxyCommand`:

> Specifies the command to use to connect to the server. […] The command can be basically
> anything, and should read from its standard input and write to its standard output. It
> should eventually connect an sshd(8) server running on some machine, or execute `sshd -i`
> somewhere. […] Note that `CheckHostIP` is not available for connects with a proxy command.

— [ssh_config(5)](https://man.openbsd.org/ssh_config#ProxyCommand)

A stdio-speaking helper is therefore the *documented* contract, not a hack. (`ProxyUseFdpass`
exists as an alternative where the helper hands back a connected fd instead.)

**VS Code: yes.** The extension resolves and invokes the *system* `ssh` binary — the
`remote.SSH.path` setting is "An absolute path to the SSH executable. When empty, it will use
`ssh` on the path or in common install locations", and the code throws `SshNotFoundError` /
`ScpNotFoundError` if it can't find them. The generated argv (from `out/resolver.js`) is
essentially:

```
-T [-D <socksPort>] [-o ClearAllForwardings=true] [-o ConnectTimeout=N]
   [-o RemoteCommand=none] [-F <configFile>] [-p <port>] [user@]<host>
```

with `<host>` being the alias the user picked, validated only against leading `-` and shell
metacharacters (`assertValidHost`). Nothing resolves a hostname to an address or opens a
socket inside the extension. The docs also treat `ProxyCommand` as an ordinary supported knob:
"you may need to use the `ProxyCommand` parameter for your host in a local SSH config file"
([Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting)). The extension
even parses `ProxyCommand` itself when reading the config (its bundled ssh-config parser has
`ProxyCommand` in its case-preserving/argument-joining rules).

Three caveats that matter to vmlab's transport design:

1. **Multiple concurrent ssh processes.** The docs describe the shape: "it makes two
   connections to open a remote window: the first to install or start the VS Code Server… and
   the second to create the SSH port tunnel" ([Tips and Tricks](https://code.visualstudio.com/docs/remote/troubleshooting)),
   plus a separate `scp` process for the local-download path. Each spawns its own
   `ProxyCommand`. The documented mitigation, `ControlMaster`, is noted as "macOS/Linux clients
   only" — fine for a Linux vmlab host, and worth recommending in whatever ssh_config stanza
   vmlab generates.
2. **The guest still needs TCP forwarding**, because the tunnel is a client-side dynamic
   forward (`-D`) rather than the session channel. A stdio ProxyCommand does not remove that
   requirement; it only replaces how the client reaches sshd.
3. **`ClearAllForwardings=true` is injected** on the calls that don't want forwarding, which
   will override `LocalForward`/`RemoteForward` directives in a vmlab-generated stanza for
   those particular invocations.

**Zed: yes.** "Zed shells out to the `ssh` binary… We read settings from your SSH config
file", and `-o` and `-F` are on the allowed-argument list
([Zed docs](https://zed.dev/docs/remote-development)).

**JetBrains: unverified.** See §2.3.

**Plain `ssh`: trivially yes** — that's the OpenSSH feature itself.

---

## Unverified / needs testing

Ordered by how much a vmlab decision hangs on it.

1. **Whether the whole VS Code flow actually completes against a domain-joined Windows guest.**
   No primary source covers domain profiles + the server install. Specific risks: `Resolve-Path ~`
   resolving to a roaming/redirected profile on a UNC path (the bootstrap does `cd $sDir` and
   file-locks `vscode-remote-lock.<commit>` there — the extension's own
   `remote.SSH.lockfilesInTmp` setting exists precisely because "hosts with a home directory
   using NFS or another distributed filesystem" have locking problems, though that setting is
   described for Linux); `icacls … /setowner "DOMAIN\user"` on the token file; Group Policy
   `ExecutionPolicy` blocking the `Start-Process powershell.exe -ExecutionPolicy Unrestricted`
   launch; and a domain logon banner or profile script polluting stdout.
   **Test that settles it**: a domain-joined Windows guest with a domain dev user, a
   vmlab-provisioned keypair, `remote.SSH.remotePlatform` pinned to `windows`, and
   `remote.SSH.localServerDownload: "always"`; connect and confirm the
   `Extension host agent listening on <port>` line appears in
   `%USERPROFILE%\.vscode-server\.<commit>.log`. Repeat with the dev user both in and out of
   the local Administrators group (different `authorized_keys` file).
2. **Whether Windows 11 / Server 2022 / Server 2025 are actually fine.** They are absent from
   the VS Code support matrix. **Test**: same as above on each target the vmlab profiles ship.
3. **Whether `remote.SSH.localServerDownload: "always"` works end to end on a Windows guest**,
   i.e. whether the `scp` push of `vscode-server.zip` into
   `%USERPROFILE%\.vscode-server\bin\<commit>\` and the `.done` marker handshake behave. There
   are user reports in [#9454](https://github.com/microsoft/vscode-remote-release/issues/9454)
   of that handshake hanging specifically against "remote: Windows Server OpenSSH". **Test**:
   block the guest's egress entirely, set the setting, connect, and watch the
   `Remote - SSH` output channel for `didLocalDownload==True==`.
4. **Whether pre-staging the server tree makes the download disappear.** The code path says
   yes (`if (Test-Path "$sFile") { "Found installed server" }`), and #9454 says it is not a
   supported feature. **Test**: unpack `server-win32-x64` for the client's exact commit into
   `%USERPROFILE%\.vscode-server\bin\<commit>\` in the template, connect with the guest
   offline, and confirm `Found installed server` in the output channel.
5. **Whether VS Code, Zed, and Toolbox tolerate a *slow* ProxyCommand.** `remote.SSH.connectTimeout`
   defaults to 15 seconds and #11613 shows what a stalled bootstrap looks like. If vmlab's
   ProxyCommand has to start or wake a machine, it can exceed that. **Test**: a deliberately
   slowed stdio ProxyCommand against a working guest, bisecting the tolerated delay.
6. **Whether JetBrains Toolbox honours `ProxyCommand`.** The IDEA FAQ's "integrated OpenSSH"
   is suggestive but not a statement about `ssh_config` semantics, and the SSH flow is being
   rewritten around `jetbrainsd`. **Test**: a Toolbox SSH connection to a host alias whose only
   route is a stdio `ProxyCommand`.
7. **Whether the Toolbox App can drive a Windows backend from a Linux client in practice**, and
   what the Windows-side agent layout is. The offline page documents Windows override files
   (`env.ps1`) and a `win32` agent artefact, which strongly implies yes, but no JetBrains page
   walks the Linux→Windows case. **Test**: Toolbox on Linux, `New SSH Connection` to a Windows
   guest, install an IDE backend.
8. **The Toolbox "PowerShell Core is not supported as the default login shell on Windows" line
   — client or remote?** Ambiguous placement on the Installation page. Cheap to settle by
   setting `DefaultShell` to `pwsh.exe` on a test guest and connecting.
9. **Whether VS Code's extension-install-locally-and-copy fallback is in stable.** Announced
   for Insiders in Feb 2025 by a maintainer; not in the docs. **Test**: block the guest's
   egress and install an extension into the remote from the Extensions panel on current
   stable.
10. **Agent forwarding into a Windows guest.** Win32-OpenSSH scopes out "Authentication
    forwarding"; whether that means `ssh -A` into a Windows guest fails outright matters if a
    dev machine is expected to reach a git remote using the developer's key. **Test**:
    `ssh -A` to the guest and `ssh-add -l` inside it.

---

## Source index

VS Code / Microsoft
- <https://code.visualstudio.com/docs/remote/ssh> (source: <https://github.com/microsoft/vscode-docs/blob/main/docs/remote/ssh.md>)
- <https://code.visualstudio.com/docs/remote/faq>
- <https://code.visualstudio.com/docs/remote/troubleshooting>
- <https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-ssh> (VSIX `0.125.2026072315`: `package.json`, `package.nls.json`, `out/resolver.js`)
- <https://github.com/microsoft/vscode/blob/main/product.json>
- <https://github.com/microsoft/vscode-remote-release/issues/9454>, <https://github.com/microsoft/vscode-remote-release/issues/11133>, <https://github.com/microsoft/vscode-remote-release/issues/11388>, <https://github.com/microsoft/vscode-remote-release/issues/11613>
- <https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_server_configuration>
- <https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse>
- <https://github.com/PowerShell/Win32-OpenSSH/wiki/DefaultShell>, <https://github.com/PowerShell/Win32-OpenSSH/wiki/Project-Scope>
- <https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/openssh/sshd_config>

JetBrains
- <https://www.jetbrains.com/help/idea/faq-about-remote-development.html>
- <https://www.jetbrains.com/help/idea/remote-development-a.html>
- <https://www.jetbrains.com/help/idea/prerequisites.html>, <https://www.jetbrains.com/help/rider/Prerequisites.html>
- <https://www.jetbrains.com/help/idea/fully-offline-mode.html>
- <https://www.jetbrains.com/help/toolbox-app/installation.html>
- <https://www.jetbrains.com/help/toolbox-app/offline-remote-development-in-the-toolbox-app.html>
- <https://www.jetbrains.com/help/toolbox-app/gettings-started-with-ssh.html>, <https://www.jetbrains.com/help/toolbox-app/add-remote-ide-to-tba.html>, <https://www.jetbrains.com/help/toolbox-app/connect-to-ssh-environment-through-jetbrainsd.html>
- <https://youtrack.jetbrains.com/issue/GTW-811>, <https://youtrack.jetbrains.com/issue/CWM-6654>

Zed / OpenSSH
- <https://zed.dev/docs/remote-development> (source: <https://github.com/zed-industries/zed/blob/main/docs/src/remote-development.md>)
- <https://man.openbsd.org/ssh_config#ProxyCommand>
