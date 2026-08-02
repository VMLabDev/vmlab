# Windows OpenSSH Server: install, domain identity, key auth

Research for [VMLabDev/vmlab#50](https://github.com/VMLabDev/vmlab/issues/50), a child of the
dev-machines map [#47](https://github.com/VMLabDev/vmlab/issues/47). Investigated 2026-08-02.

**Sources used.** Microsoft Learn (Windows Server / Windows Hardware / Win32 API / Open
Specifications), the `PowerShell/Win32-OpenSSH` wiki and release list, and the actual shipping
source in `PowerShell/openssh-portable` (branch `latestw_all` — the branch Microsoft builds the
in-box Windows OpenSSH from). Source-code citations are to `latestw_all` as of 2026-08-02; line
numbers may drift, function names should not. No secondary blog posts were used as evidence for
any claim.

---

## Bottom line

**1. Key-authenticated SSH as a domain user works, and gives you a real domain identity — but a
domain identity with no network credentials.** This is not a bug or a misconfiguration; it is
documented and it is baked into the source. Microsoft states it plainly:

> "A remote session opened via key-based authentication doesn't have associated user credentials.
> As a result, the session isn't capable of outbound authentication as the user. This behavior is
> by design."
> — [Key-Based Authentication in OpenSSH for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement)

So: the token in the session is genuinely `CONTOSO\dev`, group memberships are real, ACLs on local
files resolve correctly, and `whoami /groups` looks right. But `\\fileserver\share`, integrated-auth
SQL, a domain-authenticated NuGet/Artifactory feed, and `git` over a Kerberos-protected transport
will all fail. `klist` will show no TGT.

**For the §19 spec this is the single load-bearing fact.** "The dev box is on the domain" is real
for *authorization* and *identity*, and cosmetic for *reaching domain resources*, unless vmlab does
something extra. The spec must say which of these it promises.

**2. There are exactly three credible ways to close the gap, and all three cost something:**

| Option | What it buys | What it costs |
| --- | --- | --- |
| **Password auth instead of key auth** | Full outbound credentials — Windows sshd uses `LOGON32_LOGON_NETWORK_CLEARTEXT`, which Microsoft documents as preserving the credential so the server "can make connections to other network servers while impersonating the client" | vmlab must hold the developer's domain password. Breaks the settled "vmlab owns the keypair" seam in #47. |
| **GSSAPI (`gssapi-with-mic`) with credential delegation** | A forwarded TGT lands in the session's LSA; `klist` populated; file shares work | Needs Win11/WS2022/Win10-21H1+; the *host* must hold a Kerberos TGT for the developer (MIT krb5 `kinit` on the Linux host); the guest computer account must be trusted for unconstrained delegation, which Microsoft says "should only be used for testing scenarios". Also abandons the keypair seam. |
| **Per-command explicit credentials inside the session** | Targeted access only | `runas /netonly`, `New-PSSession -Credential`, `net use /user:` — awkward, per-tool, and does not help a language server or MSBuild that just opens `\\server\share`. |

My recommendation for the spec: **treat "key-authenticated dev session has no network credentials"
as a declared property of a vmlab dev machine**, document it, and offer an explicit opt-in for
password auth (or GSSAPI) for the labs that need domain resources. Silently defaulting to key auth
and letting the developer discover that `\\dc01\share` 403s is the worst outcome.

**3. `authorized_keys` for a domain user has a chicken-and-egg problem vmlab must solve at
provision time, not at connect time.** `sshd` resolves the home directory from
`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\<SID>\ProfileImagePath` — and if
that key does not exist (i.e. the domain user has never logged on to this machine),
**it silently falls back to `C:\Windows`**, so `AuthorizedKeysFile .ssh/authorized_keys` resolves
to `C:\Windows\.ssh\authorized_keys`. See [§2](#2-key-authentication-as-a-domain-user) — this is
read straight out of `pwd.c`, and it is the most likely way a vmlab dev-machine provision silently
fails on a fresh domain join.

**4. Install is easy and can be fully offline** on every version in vmlab's range. Windows Server
2025 ships it enabled-able out of the box; everything else is one `Add-WindowsCapability` against a
mounted FoD ISO with `-LimitAccess`. No internet needed. See [§1](#1-install).

**5. Host keys are plain files in `C:\ProgramData\ssh` and are therefore cloned with the template.**
Every VM built from one vmlab template will present the *same* SSH host key unless the template
build deletes them. Snapshot restore restores whatever keys existed at snapshot time, which is
actually the behaviour you want. See [§5](#5-host-keys). Separately, and more dangerous for a
domain dev box: **reverting a snapshot older than ~30 days breaks the machine account password**
and the guest falls off the domain entirely.

---

## 1. Install

### Availability per Windows version

| Version | OpenSSH **Client** | OpenSSH **Server** |
| --- | --- | --- |
| Windows 10 (1809+) | Preinstalled FoD | Not installed; add capability |
| Windows 11 | Preinstalled FoD | Not installed; add capability |
| Windows Server 2019 | Preinstalled FoD | Not installed; add capability |
| Windows Server 2022 | Preinstalled FoD | Not installed; add capability |
| Windows Server 2025 | Preinstalled | **Installed by default**, service off |

The client/server split is documented in Microsoft's FoD catalogue: **OpenSSH Client** is listed
under "Preinstalled FODs" (`OpenSSH.Client~~~~0.0.1.0`, "Availability: Windows 10, version 1709 and
later"), while **OpenSSH Server** is listed under "FODs that are not preinstalled" with the
capability name `OpenSSH.Server~~~~0.0.1.0` and the explicit note *"Recommendation: Don't include
this Feature on Demand on your image."*
([Available features on demand](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/features-on-demand-non-language-fod))

The floor is Windows Server 2019 / Windows 10 build 1809: *"OpenSSH is open-source and was added to
Windows Server and Windows client operating systems starting with Windows Server 2019 and Windows 10
(build 1809)."*
([OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration))

Windows Server 2025 changes the story:

> "In earlier versions of Windows Server, the OpenSSH connectivity tool required manual installation
> before use. The OpenSSH server-side component is installed by default in Windows Server 2025. The
> Server Manager UI also includes a one-step option under **Remote SSH Access** that enables or
> disables the `sshd.exe` service. Also, you can add users to the **OpenSSH Users** group to allow
> or restrict access to your devices."
> — [What's new in Windows Server 2025](https://learn.microsoft.com/en-us/windows-server/get-started/whats-new-windows-server-2025)

Note the **OpenSSH Users** group: on Server 2025 that group is the intended access-control surface,
not `AllowGroups` in `sshd_config`.
([Get started with OpenSSH Server for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse))

### Mechanism A — Optional Feature / capability (recommended for vmlab)

Online, from Windows Update:

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Start-Service sshd
Set-Service -Name sshd -StartupType 'Automatic'
```

Installing the capability also creates the firewall rule: *"Installing OpenSSH Server creates and
enables a firewall rule named `OpenSSH-Server-In-TCP`. This rule allows inbound SSH traffic on port
22."*
([Get started with OpenSSH Server for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse))

On Server 2025 the capability is already there; you only start the service.

### Can install be staged offline? — Yes, two ways

**Offline against a mounted image** (vmlab could do this on the template disk without booting it,
if it ever grows an offline-servicing path):

```
DISM.exe /image:C:\mount\Windows /add-capability /CapabilityName:OpenSSH.Server~~~~0.0.1.0 /Source:E:
```

**Online inside the build VM, from local media** — this is the shape that fits vmlab's existing
boot-a-VM template build:

```powershell
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 -Source "E:\" -LimitAccess
```

Microsoft documents the source-resolution order and the offline guarantee:

> "If you do not specify a *Source*, the default location set by Group Policy is used. If that
> fails, Windows Update is also used for online images, unless *LimitAccess* is specified."
> — [`Add-WindowsCapability`](https://learn.microsoft.com/en-us/powershell/module/dism/add-windowscapability)

> "If you're adding a FOD to an online image, `/add-capability` downloads features from Windows
> Update and adds them to the image. If you don't want to install from Windows Update, you can use
> `/LimitAccess`, which tells DISM to not check Windows Update or Windows Server Update Services for
> the capability source files."
> — [Features On Demand](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/features-on-demand-v2--capabilities)

The media you need, per the same page's table:

| Windows Version | Media |
| --- | --- |
| Windows 11 | Windows 11 Languages and Optional Features ISO |
| Windows Server 2022 | Windows Server 2022 Languages and Optional Features ISO |
| Windows 10, version 2004, and later | Windows 10, version 2004 Features on Demand ISO |
| Windows 10, version 1809 | Windows 10 Features on Demand, version 1809 ISO |

The table has **no Windows Server 2019 row**. See "Unverified" below.

`OpenSSH.Server~~~~0.0.1.0` is a **non-satellite** FoD ("Satellites: No", install size ~5.61 MB),
which is the easy case: a single `.cab`, no repository metadata required. That matters — for
satellite FoDs, the docs warn *"Don't hand-copy .cab files to a folder and try to use it as a
repository. DISM requires additional metadata in the repository."* OpenSSH Server escapes that.

One deployment caveat worth recording because vmlab labs often have no internet at all:
*"starting with Windows 10, version 1809, FOD and language packs can only be installed from Windows
Update"* when WSUS is in play — hence `-Source` + `-LimitAccess` is not just an optimisation, it is
the only reliable path in a disconnected lab.
([`Add-WindowsCapability`, Notes](https://learn.microsoft.com/en-us/powershell/module/dism/add-windowscapability))

### Mechanism B — standalone GitHub release (MSI or zip)

`PowerShell/Win32-OpenSSH` ships releases with `OpenSSH-Win64.zip` / `OpenSSH-Win64.msi` (also
Win32, ARM, ARM64). Most recent at time of writing: **10.0.0.0p2-Preview** (Oct 2025, upstream
OpenSSH 10.0p2). ([Releases](https://github.com/PowerShell/Win32-OpenSSH/releases))

MSI: ([Install Win32 OpenSSH Using MSI](https://github.com/PowerShell/Win32-OpenSSH/wiki/Install-Win32-OpenSSH-Using-MSI))

```
msiexec /i <path to openssh.msi> ADDLOCAL=Server
```

installs to `ProgramFiles\OpenSSH`.

Zip: extract to `C:\Program Files\OpenSSH`, then
`powershell.exe -ExecutionPolicy Bypass -File install-sshd.ps1`, add the firewall rule, `net start
sshd` (which *"automatically generates host keys in %programdata%\ssh if needed"*).
([Install Win32 OpenSSH](https://github.com/PowerShell/Win32-OpenSSH/wiki/Install-Win32-OpenSSH))
The wiki states these releases *"can be installed on Windows 7 and up"*.

Microsoft warns off the GitHub build for supported scenarios: *"If you downloaded the OpenSSH beta
from the GitHub repo … follow the instructions listed there, not the ones in this article. Some
information in the Win32-OpenSSH repository relates to prerelease product…"*
([Get started with OpenSSH Server for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse))

### What the vmlab template build has to do

1. Attach the matching FoD / "Languages and Optional Features" ISO to the build VM (vmlab already
   models template disks and ISOs).
2. `Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 -Source <isodrive>\ -LimitAccess`
   (skip on Server 2025).
3. Write `HKLM\SOFTWARE\OpenSSH\DefaultShell` (§4).
4. Start `sshd` once so it generates `sshd_config` and host keys, then **delete the host keys again
   before capture** (§5).
5. `Set-Service sshd -StartupType Automatic`.
6. Verify `OpenSSH-Server-In-TCP` exists and is enabled.

Everything after step 2 is a lab-time / provision-time concern, not a template concern, if vmlab
would rather keep the template generic.

---

## 2. Key authentication as a domain user

Key auth against domain accounts is explicitly supported:

> "Key-based authentication in OpenSSH for Windows works with local Windows accounts and Active
> Directory (domain) accounts. Microsoft Entra ID accounts don't support key-based authentication."
> — [Key-Based Authentication](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement)

The client-side username spelling is `user@domain@host`:

```powershell
ssh username@domain@hostname
```
(same page; the Get-started page also shows `ssh domain\username@servername`).

### Where `authorized_keys` must live

Default `AuthorizedKeysFile` is `.ssh/authorized_keys`, relative to the home directory:

> "The default is `.ssh/authorized_keys`. If you don't specify an absolute path, OpenSSH looks for
> the file relative to your home directory, such as `C:\Users\username`. If the user belongs to the
> administrator group, `%programdata%/ssh/administrators_authorized_keys` is used instead."
> — [OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration)

So for a **standard** domain user `CONTOSO\dev`, the file is
`C:\Users\<profile-folder>\.ssh\authorized_keys`.

### How the home directory is actually resolved — and the trap

This is the important part and it is only visible in the source. `get_passwd()` in
[`contrib/win32/win32compat/pwd.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/pwd.c):

```c
/* if one of below fails, set profile path to Windows directory */
if (swprintf_s(reg_path, PATH_MAX, L"SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\ProfileList\\%ls", sid_string) == -1 ||
    RegOpenKeyExW(HKEY_LOCAL_MACHINE, reg_path, 0, ..., &reg_key) != 0 ||
    RegQueryValueExW(reg_key, L"ProfileImagePath", 0, NULL, (LPBYTE)profile_home, &reg_path_len) != 0 ||
    ...)
        if (GetWindowsDirectoryW(profile_home_exp, PATH_MAX) == 0) {
```

Three consequences:

1. **The home directory is `ProfileImagePath` from the `ProfileList` registry key, keyed by SID.**
   Not `C:\Users\<samaccountname>` by convention — whatever the registry says. If Windows had to
   disambiguate a colliding profile folder name, the real path is whatever it picked, and `sshd`
   follows the registry, so vmlab must read the registry too rather than compute the path.
2. **If the user has never logged on, there is no `ProfileList` entry, and `pw_dir` silently becomes
   `C:\Windows`.** `authorized_keys` is then looked for at `C:\Windows\.ssh\authorized_keys`.
   Authentication does not error out with anything obviously diagnostic — the key file simply isn't
   found.
3. The account name is normalised to lowercase SAM-compatible `domain\user`
   (`_wcslwr_s(user_resolved, ...)` in the same function), which is why Microsoft says *"All account
   names must be specified in lower case"* and *"Domain users and groups are strictly resolved to
   NameSamCompatible format `domain_short_name\user_name`"* for `AllowGroups`/`Match`.

### When is the profile created?

The design wiki says: *"In order to login via ssh, one needs to have a valid Windows account on the
target. If user profile does not exist, it gets created upon first logon."*
([About Win32 OpenSSH and Design Details](https://github.com/PowerShell/Win32-OpenSSH/wiki/About-Win32-OpenSSH-and-Design-Details))

Confirmed in source — `load_user_profile()` in `win32_usertoken_utils.c` calls `LoadUserProfileW`,
which creates the profile if absent, and `__posix_spawn_asuser()` in
[`spawn-ext.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/spawn-ext.c)
calls it before spawning the session process.

**But the ordering is fatal for provisioning.** `sshd` reads `authorized_keys` during
*authentication*, before any token exists and therefore before the profile is created. So the very
first key-authenticated login as a never-logged-on domain user cannot work: the profile that would
hold the key does not exist yet, and `pw_dir` is `C:\Windows`.

**Three workarounds, in order of how much I like them for vmlab:**

- **Use an absolute `AuthorizedKeysFile`.** `sshd` honours absolute paths
  (`expand_authorized_keys()` in `auth.c` returns early on `is_absolute_path(file)`), so
  `AuthorizedKeysFile __PROGRAMDATA__/ssh/keys/%u.pub` puts the key somewhere that exists regardless
  of profile state. `%u` expands to the (lowercased `domain\user`) name — note the backslash in a
  filename, so a `Match User` block with a literal path is probably cleaner.
- **Force profile creation during provisioning** — e.g. a one-shot `runas`/scheduled task as the
  domain user, or a WinRM/agent-driven logon, before dropping the key. vmlab already has an in-guest
  agent that could do this.
- **Pre-create the profile and the `ProfileList` entry offline.** Fragile; not recommended.

### Administrative users — `administrators_authorized_keys`

If the user is in the local Administrators group, the per-user file is **ignored** and
`%programdata%\ssh\administrators_authorized_keys` is used instead. This is not a fallback, it is a
redirect. It comes from the `Match Group administrators` block at the bottom of the shipped
`sshd_config`
([`contrib/win32/openssh/sshd_config`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/openssh/sshd_config)):

```
Match Group administrators
       AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
```

Microsoft's note is emphatic: *"This file only applies to administrator accounts. You must use it
instead of the user-specific file within the user's profile location."*
([Key-Based Authentication](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement))

**For vmlab this is a branch the provisioner must take.** A domain user who is in
`CONTOSO\Domain Admins` (and therefore in `BUILTIN\Administrators` on a domain member) needs the key
in `administrators_authorized_keys`, not in their profile. A dev machine where the developer is a
local admin — which is the normal case for a dev box — hits this. And note the file is *shared*:
every admin's key goes in the same file, so vmlab must append/reconcile, not overwrite.

Required ACL:

> "The *administrators_authorized_keys* file must only have permission entries for the
> `NT Authority\SYSTEM` account and `BUILTIN\Administrators` security group. The NT Authority\SYSTEM
> account must be granted full control."
> — [OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration)

```powershell
icacls.exe "C:\ProgramData\ssh\administrators_authorized_keys" /inheritance:r /grant "Administrators:F" /grant "SYSTEM:F"
```

For non-English images, use the SID with a leading asterisk: `/grant "*S-1-5-32-544:F"`.
([Key-Based Authentication](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement))
**vmlab should always use the SID form** — the guest's locale is not vmlab's to assume.

### The ACL rules OpenSSH actually enforces

Documentation aside, here is the enforcement, from
[`w32-sshfileperm.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/w32-sshfileperm.c),
called from `auth2-pubkeyfile.c`:

```c
if (strict_modes && check_secure_file_permission(file, pw, 1) != 0) {
        fclose(f);
        logit("Authentication refused.");
```

`check_secure_file_permission(path, pw, read_ok=1)` enforces:

- **Owner** must be one of: `BUILTIN\Administrators`, `NT AUTHORITY\SYSTEM`, the authenticating user
  themselves, or `NT SERVICE\TrustedInstaller`. Anything else → `"Bad owner on %S"` → refused.
- **DACL**: for every *allow* ACE whose trustee is not one of those four principals, the ACE must
  grant no write-class right. Write-class is defined as:
  ```c
  #define SSH_SECURE_WRITE_MASK \
      (FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA | FILE_APPEND_DATA | \
       WRITE_DAC | WRITE_OWNER | DELETE)
  ```
  Read access by other principals **is** tolerated (`read_ok=1`), unlike upstream Unix behaviour.
  A violation logs `"Bad permissions. Try removing permissions for user: ..."` and refuses the key.
- There is SID-history handling: if the offending trustee reverse-resolves to the same
  domain\name\type as the authenticating user, it is allowed.

Note the gate is `options.strict_modes`. Microsoft lists `StrictModes` among the directives *not
available* in the in-box build — meaning you cannot turn this check off; it runs at its default
(`yes`). Plan for it rather than around it.
([OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration))

The Win32-OpenSSH security wiki summarises the same rules for host keys
(*"Host keys need to be owned by SY or AG. No other user should have access to host key files"*),
`authorized_keys` (*"This file should not be owned by, nor provide access to any other user"*), and
`administrators_authorized_keys` (*"This file should only be accessible by SYSTEM and Administrators
group"*).
([Security protection of various files in Win32-OpenSSH](https://github.com/PowerShell/Win32-OpenSSH/wiki/Security-protection-of-various-files-in-Win32-OpenSSH))

There is also a `check_secure_folder_permission()` that inspects `C:\ProgramData\ssh` and only
*logs* (does not refuse) if non-admin principals hold write access.

### Also worth knowing

`AuthorizedKeysCommand` / `AuthorizedKeysCommandUser` **are not supported** on Windows:

> "Windows OpenSSH doesn't support the `AuthorizedKeysCommand` and `AuthorizedKeysCommandUser`
> directives. Meaning you can't dynamically fetch SSH keys from Active Directory using these
> directives as you might on Linux system."
> — [Key-Based Authentication](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement)

So there is no "pull the key from AD" shortcut; vmlab must place the file.

`AuthenticationMethods` supports only `password` and `publickey` — plus `GSSAPIAuthentication` as a
separate switch on newer builds. `AllowTcpForwarding` is **not** in the unsupported list, so port
forwarding is available (relevant to the map's "dev-port forwarding" open question); `PermitTunnel`
and `AllowStreamLocalForwarding` **are** unsupported, so no `-w` tun devices and no Unix-socket
forwarding.
([OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration))

---

## 3. Does a key-authenticated domain session get a Kerberos TGT?

**No.** Not by any configuration. Here is the full chain, from statement to mechanism to spec.

### 3a. Microsoft says so directly

> "A remote session opened via key-based authentication doesn't have associated user credentials. As
> a result, the session isn't capable of outbound authentication as the user. This behavior is by
> design."
> — [Key-Based Authentication in OpenSSH for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement)

### 3b. The mechanism: S4U2self via `LsaLogonUser(..., Network, ...)`

`__posix_spawn_asuser()` in
[`spawn-ext.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/spawn-ext.c)
picks the token for the session, in this precedence:

```c
extern HANDLE password_auth_token;
extern HANDLE sspi_auth_user;
...
/* use token generated from password auth if already present */
if (password_auth_token)
        user_token = password_auth_token;
else if (sspi_auth_user)
        user_token = sspi_auth_user;

if (!user_token && (user_token = get_user_token(user, 1)) == NULL) {
        error("unable to get security token for user %s", user);
```

Public-key auth produces neither `password_auth_token` nor `sspi_auth_user`, so it falls to
`get_user_token()` → `generate_s4u_user_token()` in
[`win32_usertoken_utils.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/win32_usertoken_utils.c):

```c
BOOL domain_user = wcschr(user_cpn, L'\\') != NULL;
...
InitLsaString(&auth_package_name, (domain_user) ? MICROSOFT_KERBEROS_NAME_A : MSV1_0_PACKAGE_NAME);
...
        KERB_S4U_LOGON *s4u_logon;
        ...
        s4u_logon->MessageType = KerbS4ULogon;
...
if ((ret = LsaLogonUser(lsa_handle, &origin_name, Network, auth_package_id,
        logon_info, (ULONG)logon_info_size, NULL, &source_context,
        (PVOID*)&profile, &profile_size, &logon_id, &token, &quotas, &subStatus)) != STATUS_SUCCESS) {
```

So: **domain user + public key ⇒ `KerbS4ULogon` (S4U2self) with logon type `Network`.**
The logon type is a compile-time constant. There is no `sshd_config` directive and no registry value
that changes it.

### 3c. Why S4U2self cannot yield network credentials

Microsoft's own protocol specification is unambiguous:

> "S4U2self is described in the top half of the preceding figure. Using this extension, the service
> receives a service ticket to the service itself (**a ticket that cannot be used elsewhere**)."
>
> "Although S4U2self provides information about the user to Service 1, **this extension does not
> allow Service 1 to make requests of other services on the user's behalf.** That is the role of
> S4U2proxy."
> — [\[MS-SFU\] 1.3.3 Protocol Overview](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-sfu/1fb9caca-449f-4183-8f7a-1a5fc7e7290a)

The user never presented a Kerberos credential, so there is no TGT to forward. `sshd` asked the KDC
"tell me about `CONTOSO\dev`" and got back an authorization blob, not a credential. And `sshd` does
**not** subsequently call S4U2proxy, so even a correctly configured constrained-delegation setup on
the guest would not help: nothing in the Win32 OpenSSH source requests a proxy ticket.

The Win32 `LogonUser` documentation describes the same limitation for the equivalent network logon:

> "If you convert the token to a primary token and use it in `CreateProcessAsUser` to start a
> process, **the new process cannot access other network resources, such as remote servers or
> printers, through the redirector.** An exception is that if the network resource is not access
> controlled, then the new process will be able to access it."
> — [`LogonUserW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-logonuserw)

### 3d. What this looks like in practice

In a key-authenticated `CONTOSO\dev` session, expect:

- ✅ `whoami` → `contoso\dev`; `whoami /groups` → real domain groups; NTFS ACLs honour them.
- ✅ Anything purely local: compilers, MSBuild against local paths, local SQL Express with a local
  account, `dotnet build`, the VS Code / JetBrains remote backend itself.
- ❌ `klist` → no tickets.
- ❌ `dir \\fileserver\share` → access denied (or anonymous, if the share allows it).
- ❌ `Invoke-Sqlcmd -ServerInstance dc-sql -TrustedConnection` → login failed for
  `NT AUTHORITY\ANONYMOUS LOGON`.
- ❌ Domain-authenticated package feeds, IIS with Windows auth, `Get-ADUser`, Group Policy tools.
- ❌ Anything that reads Credential Manager (see "Unverified").

**This is exactly the "second hop problem"** Microsoft documents for PowerShell Remoting: *"Access to
the resource on ServerC is denied, because the credentials you used to create the … session aren't
passed from ServerB to ServerC."*
([Making the second hop in PowerShell Remoting](https://learn.microsoft.com/en-us/powershell/scripting/security/remoting/ps-remoting-second-hop))
Framing it that way is useful for the spec: it is a known, named Windows problem with a known,
named menu of answers.

### 3e. What closes the gap

#### Option 1 — password authentication (the surprising one)

Windows sshd's password path is *not* a plain network logon. From the same
`win32_usertoken_utils.c`:

```c
if (pLogonUserExExW(unam_utf16, udom_utf16, pwd_utf16, LOGON32_LOGON_NETWORK_CLEARTEXT,
    LOGON32_PROVIDER_DEFAULT, NULL, &token, NULL, NULL, NULL, NULL) == TRUE)
```

And Microsoft documents that flag as specifically solving the outbound-credential problem:

> "**LOGON32_LOGON_NETWORK_CLEARTEXT** — This logon type preserves the name and password in the
> authentication package, **which allows the server to make connections to other network servers
> while impersonating the client.** A server can accept plaintext credentials from a client, call
> LogonUser, verify that the user can access the system across the network, and still communicate
> with other servers."
> — [`LogonUserW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-logonuserw)

**So a password-authenticated domain SSH session should have a usable TGT and reach file shares,
while a key-authenticated one does not.** That is a genuinely counter-intuitive result and it is the
cheapest fix available. It is a documented-mechanism inference rather than a Microsoft statement
about OpenSSH specifically, so it is on the test list below — but the mechanism is not ambiguous.

Cost: vmlab would have to hold the developer's domain password (or provision a lab-only domain
account whose password vmlab owns). The latter is probably fine for a lab — vmlab already
provisions domains — and is worth putting in front of the human in the §19 design.

#### Option 2 — GSSAPI with credential delegation

Availability, per Microsoft:

> "The `GSSAPIAuthentication` configuration argument specifies whether GSSAPI (Kerberos) based user
> authentication is allowed. The default for `GSSAPIAuthentication` is no."
>
> "**Important:** GSSAPI is only available starting in Windows Server 2022, Windows 11, and Windows
> 10 ([May 2021 Update](https://support.microsoft.com/help/5003173))."
> — [OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration)

Server: `GSSAPIAuthentication yes`. Client:

```
Host SERVER01.contoso.com
    GSSAPIAuthentication yes
    GSSAPIDelegateCredentials yes
```

(or `ssh -K`). Same page.

The server side always *requests* delegation — from
[`gss-sspi.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/gss-sspi.c):

```c
ULONG sspi_req_flags = ASC_REQ_CONFIDENTIALITY | ASC_REQ_MUTUAL_AUTH | ASC_REQ_INTEGRITY |
        ASC_REQ_DELEGATE | ASC_REQ_SEQUENCE_DETECT | ASC_REQ_ALLOCATE_MEMORY;
...
/* report if delegation was requested by not fulfilled */
if ((sspi_req_flags & ASC_REQ_DELEGATE) != 0 && (sspi_ret_flags & ASC_RET_DELEGATE) == 0)
        debug("%s: delegation was requested but not fulfilled", __FUNCTION__);
...
/* get the user token for impersonation */
if (delegated_cred_handle != NULL) {
        ...
        if (SecFunctions->QuerySecurityContextToken(*context_handle, &sspi_auth_user) != SEC_E_OK)
```

and that `sspi_auth_user` is exactly the handle `spawn-ext.c` prefers over the S4U token. So when
delegation succeeds, the session runs on a token from a real Kerberos context with the client's
forwarded TGT in its LSA session — which is what a TGT-bearing session means.

Preconditions vmlab would have to satisfy, in increasing order of pain:

1. Guest must be Win11 / WS2022 / Win10 21H1+. **Windows Server 2019 and Windows 10 1809 are out.**
2. The **client** must hold a forwardable Kerberos TGT for the developer. vmlab's host is Linux, so
   that means MIT krb5 configured against the lab domain and a `kinit` — i.e. vmlab would be
   managing a Kerberos identity on the host, not just an SSH keypair. This directly contradicts the
   settled seam in #47 ("vmlab owns the keypair end to end"), so it needs a human decision.
3. The SPN must be `host/<fqdn>`. The server validates this in source:
   ```c
   const int valid_spn = _wcsnicmp(target.sServerName, L"host/", wcslen(L"host/")) == 0;
   ...
   debug("client passed an invalid principal name");
   ```
   which means connecting by IP or by a non-registered alias will fail GSSAPI.
4. Delegation must actually be *permitted* by the domain. A forwarded TGT requires unconstrained
   delegation on the guest's computer account. Microsoft: *"This method provides no control of where
   delegated credentials are used. It's less secure than CredSSP. **This method should only be used
   for testing scenarios.**"*
   ([Second hop](https://learn.microsoft.com/en-us/powershell/scripting/security/remoting/ps-remoting-second-hop))
   For a throwaway lab domain that vmlab itself stands up, "testing scenarios" is precisely what
   this is — so the warning is less damning here than it sounds. Worth saying so in the spec.
5. Constrained / resource-based constrained delegation **will not help**, because they work via
   S4U2proxy from the intermediate service, and Win32 sshd never calls S4U2proxy. RBCD's
   `PrincipalsAllowedToDelegateToAccount` machinery
   ([KCD overview](https://learn.microsoft.com/en-us/windows-server/security/kerberos/kerberos-constrained-delegation-overview))
   is aimed at a different shape of problem.

#### Option 3 — explicit credentials inside the session

- `runas /netonly /user:CONTOSO\dev "<command>"` — *"indicates that the user information specified
  is for remote access only"*; it authenticates for outbound network requests while leaving the
  local identity alone.
  ([Runas](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-server-2012-r2-and-2012/cc771525(v=ws.11)) —
  archived page; the command still ships.) Caveat from the same page: RunAs *"accepts passwords only
  from the keyboard"*, so it cannot be scripted with a piped password.
- `Invoke-Command -Credential` / `New-PSSession -Credential` — Microsoft's "simplest to use but you
  must provide credentials" option.
- `net use \\server\share /user:CONTOSO\dev *`.

All of these are per-invocation. None of them help an editor's language server that just opens a
UNC path. **They are a workaround for a script, not a fix for a dev machine.**

#### Option 4 — don't fight it

Have vmlab mount the domain resources the developer needs at provision time, under a credential
vmlab holds, so the session finds them already available (drive letters via a persistent mapping
made under a credential-bearing logon, or vmlab's existing SMB share plumbing). This keeps the
keypair seam intact and moves the credential problem to provisioning, where vmlab already has one.
Worth putting on the table for #47.

---

## 4. Default shell

Set the shell in the registry:

> "You can configure the default ssh shell in the Windows registry by adding the full path to the
> shell executable to `HKEY_LOCAL_MACHINE\SOFTWARE\OpenSSH` in the string value `DefaultShell`."
> — [OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration)

```powershell
$NewItemPropertyParams = @{
    Path         = "HKLM:\SOFTWARE\OpenSSH"
    Name         = "DefaultShell"
    Value        = "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
    PropertyType = "String"
    Force        = $true
}
New-ItemProperty @NewItemPropertyParams
```

Facts that matter:

- **The initial default is `cmd.exe`.** *"The initial default in Windows is the Windows command
  prompt (cmd.exe)."* If vmlab does nothing, developers get `cmd`.
- The setting is **server-side and machine-wide** — one shell for every user on the box. There is no
  per-user default shell. *"Setting this path doesn't apply to OpenSSH Client."*
- It is a `REG_SZ` full path, so PowerShell 7 (`C:\Program Files\PowerShell\7\pwsh.exe`) works the
  same way.
- `install-sshd.ps1` locks the `HKLM\SOFTWARE\OpenSSH` key down to
  `O:BAG:SYD:P(A;OICI;KR;;;AU)(A;OICI;KA;;;SY)(A;OICI;KA;;;BA)` — read for Authenticated Users, full
  for SYSTEM and Administrators
  ([`install-sshd.ps1`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/openssh/install-sshd.ps1)),
  so writing it needs elevation.

**What depends on it.** Everything that assumes a POSIX-ish shell. `scp`/`sftp` go through
`sftp-server.exe` and are unaffected, but remote *command execution* (`ssh host <command>`) is
parsed by the default shell, and editors' remote backends universally shell out. VS Code
Remote-SSH's bootstrap in particular is sensitive to the default shell being `cmd` vs `powershell`
vs `pwsh`. Since vmlab owns the endpoint end to end, **pinning `DefaultShell` in the template is the
right call**, and the value should probably be part of the dev-machine declaration so a lab can pick
`pwsh` when the toolchain wants it.

---

## 5. Host keys

### Where they live

```
#HostKey __PROGRAMDATA__/ssh/ssh_host_rsa_key
#HostKey __PROGRAMDATA__/ssh/ssh_host_dsa_key
#HostKey __PROGRAMDATA__/ssh/ssh_host_ecdsa_key
#HostKey __PROGRAMDATA__/ssh/ssh_host_ed25519_key
```

> "If the defaults aren't present, sshd automatically generates them on a service start."
> — [OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration)

> "Because there's no user associated with the `sshd` service, the host keys are stored under
> *C:\ProgramData\ssh*." … "The first time the `sshd` service is used, the key pair for the host is
> automatically generated."
> — [Key-Based Authentication](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement)

ACL requirement: *"Host keys need to be owned by SY or AG. No other user should have access to host
key files."*
([Security protection of various files](https://github.com/PowerShell/Win32-OpenSSH/wiki/Security-protection-of-various-files-in-Win32-OpenSSH))
Enforced by `sshkey_perm_ok()` → `check_secure_file_permission()`
([`authfile.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/authfile.c)); a
violation logs `WARNING: UNPROTECTED PRIVATE KEY FILE!` and the key is ignored.

### Across a template clone

Host keys are ordinary files on the guest disk. **A template captured after `sshd` has run carries
those keys into every clone, so every VM from that template presents an identical host-key
fingerprint.** I found no Microsoft statement to this effect — it follows from the keys being files,
and from the fact that nothing in the documented sysprep behaviour touches `C:\ProgramData\ssh`.
Treat "sysprep does not clear SSH host keys" as *strongly expected but not documented* (see
Unverified).

The fix is the standard one, and OpenSSH provides the primitive:

> "**-A** — Generate host keys of all default key types (rsa, ecdsa, mldsa44-ed25519 and ed25519) if
> they do not already exist. The host keys are generated with the default key file path, an empty
> passphrase, default bits for the key type, and default comment."
> — [`ssh-keygen(1)`](https://man.openbsd.org/ssh-keygen)

So the vmlab template build should, immediately before capture:

```powershell
Stop-Service sshd
Remove-Item C:\ProgramData\ssh\ssh_host_* -Force
```

and either let the first `sshd` start regenerate them (documented behaviour, above) or run
`ssh-keygen -A` from a first-boot action. **Then re-apply the ACL**, since regeneration under an
unexpected context is the usual way this goes wrong.

**Design consequence for #47.** If clones regenerate host keys, the host-side SSH alias that vmlab
publishes cannot pin a `known_hosts` entry at template-build time. vmlab has to learn the host key
at first boot and write it into the per-lab `known_hosts` it manages — or set
`StrictHostKeyChecking no` + `UserKnownHostsFile /dev/null` for the generated alias, which is
defensible given PRD §1.2 ("vmlab is not a security boundary") but should be a conscious choice
recorded in the spec. My preference: **generate the host key at provision time, read the public key
back out over the existing agent channel, and write a real `known_hosts` entry.** vmlab already has
an in-guest agent with file pull; this costs almost nothing and keeps editors from prompting.

### Across a snapshot restore

Host keys are files on the guest disk, so a snapshot restore restores the keys that existed at
snapshot time. That is the *desirable* behaviour: the guest's SSH identity is stable across revert,
and the developer's `known_hosts` does not churn. The only failure mode is reverting to a snapshot
taken **before** `sshd` first started, which restores a keyless state; the next service start
regenerates a *different* key and every prior `known_hosts` entry breaks.

**The much bigger snapshot problem for a domain dev machine is not SSH at all.** Domain members
rotate their computer-account password:

> "In Active Directory–based domains, each device has an account and password. **By default, the
> domain members submit a password change every 30 days.**"
> — [Domain member: Maximum machine account password age](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-10/security/threat-protection/security-policy-settings/domain-member-maximum-machine-account-password-age)

Reverting a domain-joined guest to a snapshot older than the rotation interval leaves the guest with
a stale machine password, the secure channel breaks, and *domain logons stop working entirely* —
including the SSH logon this whole effort is about. The map's "rebuild story" open question should
name this. Mitigations: keep dev-machine snapshots short-lived, or set
**Domain member: Disable machine account password changes** in the lab domain's policy (acceptable
for a throwaway lab; Microsoft warns against it for production, same page family).

---

## Unverified / needs testing

Everything above is sourced. These are not, and I am flagging them rather than guessing.

1. **Does a *password*-authenticated domain SSH session actually get a usable TGT?** The mechanism
   is documented (`LOGON32_LOGON_NETWORK_CLEARTEXT`, §3e) and the source confirms sshd uses it, but
   I found no Microsoft statement saying "password-authenticated OpenSSH sessions can reach domain
   resources". **This is the single highest-value test.** If it holds, it is the cheapest gap-closer
   vmlab has.
   *Test:* domain-joined guest, `PasswordAuthentication yes`, `ssh dev@contoso@guest`, then in the
   session run `klist` and `dir \\dc01\netlogon`. Compare against the same commands in a
   key-authenticated session.

2. **Does GSSAPI + delegation from an OpenSSH *Linux* client to Windows sshd actually land a TGT?**
   The source shows the server takes the SSPI token and uses it, and the delegation flag is
   requested. But every worked example I found was Windows→Windows, and there is a known failure
   mode where `AcceptSecurityContext` returns without `ASC_RET_DELEGATE` and the session silently
   has no tickets. vmlab's host is Linux, so this is exactly the untested direction.
   *Test:* MIT krb5 on the host, `kinit dev@CONTOSO.LOCAL`, `ssh -K -o GSSAPIDelegateCredentials=yes
   dev@guest.contoso.local`, then `klist` and `dir \\dc01\netlogon` in the session. Watch
   `sshd -d -d -d` for `delegation was requested but not fulfilled`.

3. **Does sysprep clear `C:\ProgramData\ssh`?** I could find no Microsoft statement either way. I
   expect not.
   *Test:* install OpenSSH Server, start sshd, record `ssh_host_ed25519_key.pub` fingerprint,
   `sysprep /generalize /oobe /shutdown`, boot a clone, compare fingerprints.

4. **What exactly happens on the very first key-auth attempt by a never-logged-on domain user?**
   I have proved from source that `pw_dir` becomes `C:\Windows`. I have *not* confirmed the
   resulting log line or whether any code path creates the profile earlier than I think.
   *Test:* fresh domain-joined guest, domain user who has never logged on, place the key at
   `C:\Users\dev\.ssh\authorized_keys` (creating the folder manually) and at
   `C:\Windows\.ssh\authorized_keys`, attempt login with `sshd -d -d -d` running, and see which one
   is read.

5. **Is Windows Server 2019's OpenSSH FoD payload obtainable offline?** The FoD-media table on
   Microsoft's Features-on-Demand page lists Windows 11, Server 2022, and several Windows 10
   versions — but **not Server 2019**. Historically Server 2019 had a separate FoD ISO on VLSC. I
   could not confirm from a primary Microsoft page.
   *Test:* on an offline Server 2019 guest, `Add-WindowsCapability -Online -Name
   OpenSSH.Server~~~~0.0.1.0 -Source <server2019 FoD ISO> -LimitAccess`. If this cannot be made to
   work, Server 2019 dev machines require internet at build time and vmlab should say so.

6. **Does Credential Manager (`cmdkey` / Windows Vault) work inside a key-authenticated session?**
   Microsoft documents that vault credentials are *"saved in special encrypted folders on the
   computer under the user's profile"*
   ([Credentials Processes](https://learn.microsoft.com/en-us/windows-server/security/windows-authentication/credentials-processes-in-windows-authentication)),
   which is DPAPI-protected and normally requires the user's credential to unseal. An S4U logon has
   no credential, so I expect stored credentials to be unavailable — but I found no Microsoft
   statement confirming it for this case, and it matters because "just `cmdkey` the share once" is
   an obvious-looking workaround that may not work.
   *Test:* in a key-auth session, `cmdkey /list` and `cmdkey /add:dc01 /user:... /pass:...` followed
   by `dir \\dc01\share`.

7. **Which OpenSSH version ships in-box per Windows release.** The Win32-OpenSSH wiki has no such
   table and I found no Microsoft page listing it. Matters only if the spec wants to depend on a
   feature added in a specific upstream version.
   *Test:* `ssh -V` on each target guest, or `(Get-Item C:\Windows\System32\OpenSSH\sshd.exe).VersionInfo`.

8. **Whether the `OpenSSH Users` group on Server 2025 is a hard gate.** Microsoft says to add users
   to it to "allow or restrict" access, but does not say whether membership is *required* for
   non-admin logon. If it is, a vmlab dev machine on Server 2025 must add the domain user to it.
   *Test:* Server 2025, domain user not in `OpenSSH Users`, attempt key auth.

---

## Sources

**Microsoft Learn**

- [Get started with OpenSSH Server for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse)
- [Key-Based Authentication in OpenSSH for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_keymanagement)
- [OpenSSH Server Configuration for Windows](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh-server-configuration)
- [What's new in Windows Server 2025](https://learn.microsoft.com/en-us/windows-server/get-started/whats-new-windows-server-2025)
- [Available features on demand](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/features-on-demand-non-language-fod)
- [Features On Demand](https://learn.microsoft.com/en-us/windows-hardware/manufacture/desktop/features-on-demand-v2--capabilities)
- [`Add-WindowsCapability`](https://learn.microsoft.com/en-us/powershell/module/dism/add-windowscapability)
- [`LogonUserW`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-logonuserw)
- [`LsaLogonUser`](https://learn.microsoft.com/en-us/windows/win32/api/ntsecapi/nf-ntsecapi-lsalogonuser)
- [\[MS-SFU\] 1.3.3 Protocol Overview](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-sfu/1fb9caca-449f-4183-8f7a-1a5fc7e7290a)
- [Kerberos Constrained Delegation Overview](https://learn.microsoft.com/en-us/windows-server/security/kerberos/kerberos-constrained-delegation-overview)
- [Making the second hop in PowerShell Remoting](https://learn.microsoft.com/en-us/powershell/scripting/security/remoting/ps-remoting-second-hop)
- [Credentials Processes in Windows Authentication](https://learn.microsoft.com/en-us/windows-server/security/windows-authentication/credentials-processes-in-windows-authentication)
- [Domain member: Maximum machine account password age](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-10/security/threat-protection/security-policy-settings/domain-member-maximum-machine-account-password-age)
- [Runas](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-server-2012-r2-and-2012/cc771525(v=ws.11)) (archived)

**OpenBSD manuals** (upstream, referenced by Microsoft's own docs)

- [`ssh-keygen(1)`](https://man.openbsd.org/ssh-keygen)
- [`sshd_config(5)`](https://man.openbsd.org/sshd_config)

**Win32-OpenSSH project**

- [Install Win32 OpenSSH](https://github.com/PowerShell/Win32-OpenSSH/wiki/Install-Win32-OpenSSH)
- [Install Win32 OpenSSH Using MSI](https://github.com/PowerShell/Win32-OpenSSH/wiki/Install-Win32-OpenSSH-Using-MSI)
- [Setup public key based authentication for windows](https://github.com/PowerShell/Win32-OpenSSH/wiki/Setup-public-key-based-authentication-for-windows)
- [Security protection of various files in Win32-OpenSSH](https://github.com/PowerShell/Win32-OpenSSH/wiki/Security-protection-of-various-files-in-Win32-OpenSSH)
- [About Win32 OpenSSH and Design Details](https://github.com/PowerShell/Win32-OpenSSH/wiki/About-Win32-OpenSSH-and-Design-Details)
- [Releases](https://github.com/PowerShell/Win32-OpenSSH/releases)

**Shipping source** — `PowerShell/openssh-portable`, branch `latestw_all`

- [`contrib/win32/win32compat/pwd.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/pwd.c) — home-directory resolution, name normalisation
- [`contrib/win32/win32compat/win32_usertoken_utils.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/win32_usertoken_utils.c) — `generate_s4u_user_token`, `get_user_token`, `load_user_profile`, password logon
- [`contrib/win32/win32compat/spawn-ext.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/spawn-ext.c) — token precedence
- [`contrib/win32/win32compat/gss-sspi.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/gss-sspi.c) — GSSAPI/SSPI, delegation, SPN check
- [`contrib/win32/win32compat/w32-sshfileperm.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/win32compat/w32-sshfileperm.c) — ACL enforcement
- [`auth2-pubkeyfile.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/auth2-pubkeyfile.c) — where the ACL check gates `authorized_keys`
- [`authfile.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/authfile.c) — host-key permission check
- [`auth.c`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/auth.c) — `expand_authorized_keys`, absolute-path handling
- [`contrib/win32/openssh/sshd_config`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/openssh/sshd_config) — shipped defaults incl. `Match Group administrators`
- [`contrib/win32/openssh/install-sshd.ps1`](https://github.com/PowerShell/openssh-portable/blob/latestw_all/contrib/win32/openssh/install-sshd.ps1) — registry ACLs
