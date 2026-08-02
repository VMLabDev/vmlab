# Workspace transport reality: virtiofs and SMB inside a Windows guest

Research for [#51](https://github.com/VMLabDev/vmlab/issues/51), a child of the
dev-machines map [#47](https://github.com/VMLabDev/vmlab/issues/47).
Investigated 2026-08-02.

**The question.** vmlab has two share transports behind one `share {}` surface
(PRD §7.5): virtiofs (one `virtiofsd` per share, vhost-user-fs, mounted in a
Windows guest by virtio-win's `virtiofs.exe` on top of WinFsp) and SMB served by
the bundled `smbd` at the segment gateway. Is a host-side workspace shared into a
Windows guest over either transport good enough to **develop** on — build, debug,
run a language server — or does the source have to live on the guest's own disk?

**Method.** Every claim below is cited to a primary source: project source at a
pinned revision, official specification text, Microsoft Learn, or the owning
project's issue tracker. Issue-tracker evidence is labelled as such. Where a
conclusion is derived from reading source rather than quoted from documentation,
that is stated inline. Nothing here was measured on a running system; §7 lists
what still needs to be.

---

## Bottom line

**Source must live on the guest's own disk. Neither transport can carry a
workspace a Windows toolchain watches.**

1. **Host→guest change notification does not work on either transport, and fails
   silently.** This is the decisive finding, and it is not a tuning problem — it
   is structural on both paths.
   - **virtiofs.** WinFsp raises `ReadDirectoryChangesW` events automatically
     only for operations that pass through the guest kernel. For changes made on
     the host, the user-mode file system must *push* them via
     `FspFileSystemNotify`, and virtio-win's `viofs` did not call that API at all
     until a polling notifier merged **2026-07-28** — after the latest build tag,
     so **no released virtio-win contains it**. When it does ship it is off by
     default, polls every 3 s, watches **only files the guest already has open**,
     and emits a single `FILE_ACTION_MODIFIED` per watched path with no
     added/removed/renamed detail.
   - **SMB.** The protocol supports it and Samba implements it, but Samba's
     inotify backend registers **one non-recursive watch on the watched directory
     only**. A `SMB2_WATCH_TREE` subscription — what every language server,
     `dotnet watch` and bundler uses — therefore sees host-side edits in
     subdirectories not at all; below the first level, only changes made *through
     smbd* are reported.
   - Both failures are silent. `FileSystemWatcher` stays armed and quiet. Per
     issue #51's own framing, that is worse than an error.

2. **This closes a question the map left open, in the direction the map already
   leaned.** #47 has already settled that the editor attaches *into* the guest and
   that the language server, build, debugger and terminal run guest-side. The
   finding here extends that one step: the *workspace itself* must be guest-side
   too. A dev machine whose `dev {}` workspace is a `share {}` is a dev machine
   whose watchers do not fire.

3. **Once everything is guest-side, the problem disappears.** Guest-local NTFS
   gives `ReadDirectoryChangesW` its native behaviour, gives git `core.fsmonitor`
   (Windows is one of only two supported platforms), gives real NTFS semantics,
   and removes every round trip. There is nothing left to work around.

4. **This is exactly what the devcontainer ecosystem concluded.** "Clone
   Repository in Container Volume" exists because bind mounts are slow *and*
   because Docker documents flatly that "Linux containers only receive file change
   events, 'inotify events', if the original files are stored in the Linux
   filesystem". The cost is real and vmlab will inherit it: the workspace is no
   longer reachable by host tools, and nothing but `git push` gets work out.
   VS Code does not document that cost — vmlab should.

5. **Shares remain the right mechanism for what they already do** — datasets,
   installers, artefact drops, read-mostly inputs, getting a build output back to
   the host. Nothing here argues against `share {}`. It argues against putting a
   *watched, built-against source tree* on one.

6. **Two PRD §7.5 items resolve as a by-product** (details in §6): the flagged
   folder-path mechanism is *correct as implemented* (`mklink /D` to a UNC target
   is documented as supported; `mklink /J` would not have been), and the stated
   virtiofsd floor of ≥1.10 is **wrong — it is ≥1.11.0**.

7. **No authoritative performance numbers exist.** Neither the virtio-fs project,
   QEMU, Microsoft, nor Samba publishes a benchmark for build-shaped workloads.
   §7 states precisely what to measure on a real lab instead of inventing figures.

---

## 1. File change notification

### 1.1 What Windows itself guarantees

`ReadDirectoryChangesW`
([Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw))
is the substrate under .NET's `FileSystemWatcher`, VS Code's Windows watcher, and
every MSBuild/LSP file watcher. Its documented contract already admits loss:

> "If the network redirector or the target file system does not support this
> operation, the function fails with **ERROR_INVALID_FUNCTION**."

> "If the buffer overflows, **ReadDirectoryChangesW** will still return true, but
> the entire contents of the buffer are discarded and the *lpBytesReturned*
> parameter will be zero"

> "**ReadDirectoryChangesW** fails with **ERROR_NOTIFY_ENUM_DIR** when the system
> was unable to record all the changes to the directory. In this case, you should
> compute the changes by enumerating the directory or subtree."

> "**ReadDirectoryChangesW** fails with **ERROR_INVALID_PARAMETER** when the
> buffer length is greater than 64 KB and the application is monitoring a
> directory over the network. This is due to a packet size limitation with the
> underlying file sharing protocols."

The same page carries a support matrix listing "Server Message Block (SMB) 3.0
protocol | Yes".

.NET's
[`FileSystemWatcher`](https://learn.microsoft.com/en-us/dotnet/api/system.io.filesystemwatcher)
inherits all of it: "You can create a component to watch files on a local
computer, a network drive, or a remote computer", but "The maximum size you can
set for the `InternalBufferSize` property for monitoring a directory over the
network is 64 KB", and on overflow "This causes the component to lose track of
changes in the directory, and it will only provide blanket notification."

So even in the best case, a networked watcher is a best-effort feed with a
documented re-enumerate-on-overflow recovery path. The question is whether events
arrive **at all** for host-originated changes.

### 1.2 virtiofs + WinFsp: two mechanisms, and only one of them is automatic

WinFsp has two distinct notification paths, and the split is the whole story.

**Mechanism A — automatic, for changes routed through the guest kernel.** The
WinFsp FSD calls `FsRtlNotifyFullReportChange` itself on create, overwrite,
delete-on-cleanup, rename, size change, EA and security changes
([`src/sys/file.c#L2312-L2372`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/src/sys/file.c#L2312-L2372),
[`cleanup.c#L173-L206`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/src/sys/cleanup.c#L173-L206),
[`fileinfo.c#L1955-L1971`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/src/sys/fileinfo.c#L1955-L1971)).
"Directory change notifications" is listed under Supported features in
[`doc/NTFS-Compatibility.asciidoc`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/doc/NTFS-Compatibility.asciidoc).
The user-mode file system does nothing.

**Mechanism B — an explicit push API, required for everything else.**
`FspFileSystemNotifyBegin` / `FspFileSystemNotify` / `FspFileSystemNotifyEnd`
([`inc/winfsp/winfsp.h#L1242-L1307`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/inc/winfsp/winfsp.h#L1242-L1307)),
carrying `FSP_FSCTL_NOTIFY_INFO { Size, Filter, Action, FileNameBuf }`
([`inc/winfsp/fsctl.h#L324-L332`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/inc/winfsp/fsctl.h#L324-L332)).

WinFsp's author states the division exactly. *(Project issue tracker.)*
[winfsp#571](https://github.com/winfsp/winfsp/issues/571) (closed 2024-08-20):

> "By default WinFsp reports notifications for file system operations that happen
> via the local kernel, **because it cannot know about files that are added
> remotely.** WinFsp does have an API that allows a file system such as rclone to
> report notifications for remote operations, but I do not believe that rclone
> currently uses it."

And the earlier design thread
[winfsp#68](https://github.com/winfsp/winfsp/issues/68), before the API existed:

> "…you may be looking for a way to inform the OS (and apps running on it) of
> changes that happen outside that OS (e.g. by a different computer on a network).
> **There is no way to do this currently in WinFsp.**"

Worth recording for risk: the author's own assessment of the resulting API, in
[winfsp#682](https://github.com/winfsp/winfsp/issues/682) (2026-07-27) —
"the notification API which may be the weakest part in WinFsp. (Adding the
notification API may have been a mistake. It was a highly requested feature, but
it has caused problems over the years, because it inverts the usual way that
things work.)" [winfsp#678](https://github.com/winfsp/winfsp/issues/678) (open) is
a live report that watch registrations do not survive a volume
disconnect/reconnect.

**Does virtio-win's `viofs` call Mechanism B? Only since 2026-07-28, and not in
any release.**

[virtio-win/kvm-guest-drivers-windows#1616](https://github.com/virtio-win/kvm-guest-drivers-windows/pull/1616)
— "RHEL-216649: viofs: add optional polling-based file change notifications" —
merged 2026-07-28. The implementation is `VIRTFS::RefreshWatchedHandles`
([`viofs/svc/virtiofs.cpp#L593-L655`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L593-L655)),
and reading it gives the shape of the feature:

| Property | Value (from source) |
| --- | --- |
| Detection | one `FUSE_GETATTR` per watched handle, on a background thread ([`#L467-L490`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L467)) |
| Change token | `{atime, mtime, ctime, nsecs, nlink}`; files compare **mtime only** ([`#L462`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L462)) |
| Scope | **only paths the guest currently has open** (`RegisterRefresh` at [`#L1771`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L1771)) |
| Interval | default 3 s, max 60 s, `0` coerced to 3 ([`#L64-L65`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L64), [`#L3351-L3357`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L3351)) |
| Default | **off** — `bool RefreshEnabled{false}` ([`#L3281`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L3281)); enabled by `-n Seconds` or `HKLM\Software\VirtIO-FS` `NotifierEnabled` |
| Event emitted | one `FILE_ACTION_MODIFIED` naming the watched path — **no ADDED / REMOVED / RENAMED, no per-entry names** |
| Cost when on | one synchronous host round trip per open handle per interval, whole sweep under one exclusive `SRWLOCK` |
| Shipped? | **No.** Latest build tag in `status.txt` is `mm322` (2026-07-22); the notifier merged 2026-07-28. Latest release is [virtio-win-0.1.285-1](https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/latest-virtio/) |

The FUSE reverse-notification opcodes (`FUSE_NOTIFY_INVAL_ENTRY` and friends)
exist in
[`viofs/shared/fuse.h`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/shared/fuse.h#L480-L487)
but are **referenced nowhere** in `virtiofs.cpp` — the host daemon's push channel
is unused, and guest-side polling is the only path. That matches the host side:
virtiofsd does not advertise `VIRTIO_FS_F_NOTIFICATION` at all (§4.1).

The project's own wiki still describes virtiofs on Windows as
["a Tech Preview feature"](https://github.com/virtio-win/kvm-guest-drivers-windows/wiki/Virtiofs:-Shared-file-system).

### 1.3 SMB + Samba: the protocol is fine, the server's recursive path is not

**The wire protocol supports it.** MS-SMB2 §2.2.35 defines `SMB2 CHANGE_NOTIFY`
with `SMB2_WATCH_TREE 0x0001` — "monitor changes on any file or directory
contained beneath the directory" —
([spec](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/598f395a-e7a2-4cc8-afb3-ccb30dd2df7c)),
and §3.3.5.19 defines the loss path
([spec](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/05869c32-39f0-4726-afc9-671b76ae5ca7)):

> "The server MUST send an SMB2 CHANGE_NOTIFY Response **only if a change
> occurs**. … will result in, **at most, one response** from the server."

> "**If the server is unable to copy the results into the buffer** … the server
> MUST construct the response … with an **OutputBufferLength of zero**, and set
> the Status in the SMB2 header to **STATUS_NOTIFY_ENUM_DIR**."

There is no protocol-level replay; the client re-enumerates. Also
([§3.3.1.3](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/1e03994f-ccce-4fc8-a370-4efa610b3e05)),
the filter and watch-tree flag are taken from the **first** request on an open and
ignored thereafter.

**Samba documents the intent.** `kernel change notify` (global, default **yes**),
[Samba's own doc source](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/misc/kernelchangenotify.xml):

> "This parameter specifies whether Samba should **ask the kernel for change
> notifications in directories so that SMB clients can refresh whenever the data
> on the server changes.** This parameter is only used when your kernel supports
> change notification to user programs using the **inotify** interface."

**But the implementation is one level deep.** `notifyd` has two paths: an internal
`MSG_SMB_NOTIFY_TRIGGER` path that fires when *another smbd* changes something
([`source3/smbd/notifyd/notifyd.c`](https://github.com/samba-team/samba/blob/master/source3/smbd/notifyd/notifyd.c)),
and an inotify path
([`source3/smbd/notify_inotify.c`](https://github.com/samba-team/samba/blob/master/source3/smbd/notify_inotify.c))
which sees everything including host-side writes. `notifyd` hands the backend both
a `sys_filter` and a `sys_subdir_filter`, and the backend is contracted to strip
the bits it can serve — but **`inotify_watch()` never touches `*subdir_filter`**.
It issues a single `inotify_add_watch(..., mask | IN_MASK_ADD | IN_ONLYDIR)` on
the one directory. inotify has no recursive mode and Samba does not walk the
subtree.

Consequences, **derived from that source and not documented by Samba**:

- Non-recursive watch on directory *D*: host-side changes to *D*'s direct
  children **are** reported.
- `SMB2_WATCH_TREE` on *D* (i.e. `IncludeSubdirectories = true`, which is what
  every real watcher uses): inotify contributes **nothing** below the first level.
  A host-side editor saving `D/src/main.cs`, a host-side `git checkout`, a
  host-side `cargo build` — all invisible to the Windows watcher.
- Even at the top level the mapping table
  ([`notify_inotify.c` L68-L79](https://github.com/samba-team/samba/blob/master/source3/smbd/notify_inotify.c))
  covers only `FILE_NAME`, `DIR_NAME`, `ATTRIBUTES`, `LAST_WRITE`, `LAST_ACCESS`,
  `EA`, `SECURITY`. `FILE_NOTIFY_CHANGE_SIZE` and `FILE_NOTIFY_CHANGE_CREATION`
  have **no inotify mapping** and fall back to the smbd-only path.

This is the single most important claim in this document that rests on source
reading rather than a documentation sentence. §7 gives the test that settles it.

**Two further coherency ceilings apply even where notification works.**

*Leases vs. out-of-band writes — a hard either/or on Samba.* SMB2/3 caching is
revocation-driven; nothing in MS-SMB2 addresses a process modifying the backing
store behind the server. Samba's answer is `kernel oplocks`, and its own docs
state the price:

| | `kernel oplocks = no` (**default**) | `kernel oplocks = yes` |
| --- | --- | --- |
| SMB2 leases | available | **disabled** |
| SMB3 directory leases | available | **disabled** |
| Level II oplocks | available | **disabled** |
| Break on host-side local write | **no** | yes |

Sources: [`oplocks`](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/locking/oplocks.xml)
("allows the clients to aggressively cache files locally"),
[`kernel oplocks`](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/locking/kerneloplocks.xml)
("**However, this disables Level II oplocks** … Kernel oplocks support allows
Samba oplocks to be broken whenever a **local UNIX process or NFS operation
accesses a file that smbd has oplocked** … This parameter defaults to no"),
[`smb2 leases`](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/locking/smb2leases.xml)
and
[`smb3 directory leases`](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/locking/smb3directoryleases.xml)
(both "only available with … **`kernel oplocks = no`**").

*The Windows redirector caches metadata on a timer regardless.* Microsoft's
["SMB2 Client Redirector Caches Explained"](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-7/ff686200(v=ws.10)):
`FileInfoCacheLifetime` **10 s**, `DirectoryCacheLifetime` **10 s**,
`FileNotFoundCacheLifetime` **5 s**. And, precisely on point:

> "Applications which require a high level of file information consistency across
> clients which may utilize **creation or changing of a file as a notification
> mechanism** to other nodes may encounter **delays or consistency issues** with
> these default values."

So even with everything else working: up to ~10 s of stale `stat`/enumeration and
~5 s of stale negative lookups.

### 1.4 What the toolchain does when watching is unreliable — it polls

This is not speculation about how bad it would be; the ecosystem already shipped
the workaround, and the workaround's existence is itself first-party evidence that
the problem is real.

- **.NET.** `dotnet watch` reads `DOTNET_USE_POLLING_FILE_WATCHER`:
  "When set to `1` or `true`, `dotnet watch` uses a polling file watcher instead
  of `System.IO.FileSystemWatcher`. **Polling is required for some file systems,
  such as network shares, Docker mounted volumes, and other virtual file
  systems.**"
  ([Microsoft Learn](https://learn.microsoft.com/en-us/dotnet/core/tools/dotnet-watch)).
  That sentence names both of vmlab's transports in one breath.
- **MSBuild does not watch at all** — it compares timestamps: "MSBuild compares
  the time stamp of every input item to the time stamp of its corresponding output
  item… An item is considered up-to-date if its output file is the same age or
  newer than its input file"
  ([Microsoft Learn](https://learn.microsoft.com/en-us/visualstudio/msbuild/incremental-builds)).
  Incremental MSBuild therefore does not depend on notification — but it *does*
  depend on timestamp fidelity and on `stat` not being 10 s stale, which is a
  different exposure on the same transports.
- **VS Code** publishes `files.watcherExclude` guidance and, for the WSL 1 case,
  an explicit polling switch `remote.WSL.fileWatcher.polling` with the note that
  "polling based file watching has a performance impact for large workspaces"
  ([VS Code docs](https://code.visualstudio.com/docs/remote/troubleshooting)).
  Note the scope: that setting is a WSL 1 remedy and is documented as *not*
  applying to WSL 2, and **no polling fallback exists or is documented for Dev
  Containers**.
- **Kata Containers** — the largest production micro-VM deployment of virtiofs —
  solved the same problem by polling and copying rather than by fixing the
  transport. See §4.2.

---

## 2. Path and filesystem semantics

### 2.1 virtiofs / WinFsp in a Windows guest

All viofs values below are read from its `FSP_FSCTL_VOLUME_PARAMS` block
([`virtiofs.cpp#L3056-L3077`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L3056-L3077))
and its operation table
([`#L2966-L2989`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L2966));
WinFsp capabilities from
[`inc/winfsp/fsctl.h`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/inc/winfsp/fsctl.h)
and
[`doc/NTFS-Compatibility.asciidoc`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/doc/NTFS-Compatibility.asciidoc).

| Semantic | WinFsp | viofs as shipped |
| --- | --- | --- |
| **Case sensitivity** | supports either | **case-SENSITIVE by default** (`CaseInsensitive` defaults false). `-i` opts in, and the wiki's only listed known limitation is "Case insensitivity may not work properly." |
| Case preserving | yes | yes |
| **Alternate data streams** | `NamedStreams` supported | **not enabled.** Wiki: "NTFS-like file streams are not supported." No Mark-of-the-Web, no `Zone.Identifier`. |
| **Hard links** | **not supported by WinFsp** (`HardLinks` is "unimplemented; set to 0") | n/a |
| **Reparse points / symlinks** | full support incl. symlinks | `ReparsePoints = 1` but only `ResolveReparsePoints` + `SetReparsePoint` are implemented — **no `GetReparsePoint`, no `DeleteReparsePoint`**, so `FSCTL_GET_REPARSE_POINT` returns `STATUS_INVALID_DEVICE_REQUEST`. Symlink creation rejects **absolute** targets ([`#L2877`](https://github.com/virtio-win/kvm-guest-drivers-windows/blob/a66c7af4aef5ded62b2b19048df7e9c396b927ef/viofs/svc/virtiofs.cpp#L2877)). |
| Extended attributes | `ExtendedAttributes` supported | not enabled → no WSL metadata |
| Short (8.3) names | "WinFsp made a conscious decision not to support them" | n/a |
| **Max path** | **hard cap `FSP_FSCTL_TRANSACT_PATH_SIZEMAX = 1024 * sizeof(WCHAR)`** ([`fsctl.h#L136`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/inc/winfsp/fsctl.h#L136)) → ~1023 WCHARs, vs NTFS's 32 767 | inherits it; `MaxComponentLength` defaults to 255. Several hot paths still use fixed `MAX_PATH` buffers — cf. fix `0b1a0ab4` (2026-03-10) "viofs: fix stack override by long guest file names" |
| **Share modes** (`FILE_SHARE_*`) | enforced in kernel by the FSD (`IoCheckShareAccess`, [`file.c#L730-L742`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/src/sys/file.c#L730)) | inherited — but **guest-local only.** The host sees no Windows share modes. |
| **Byte-range locks** | handled entirely in the FSD (`FsRtlProcessFileLock`); there is **no lock callback in `FSP_FILE_SYSTEM_INTERFACE`** | **not propagated to the host** — no `FUSE_SETLK`/`GETLK` anywhere in viofs. Two guest processes interlock; guest↔host does not. |
| Sparse / compressed / quotas / change journal / object IDs | listed Unsupported by WinFsp | n/a |

The case-sensitivity default is the sharpest trap. Microsoft's own warning about
case-sensitive directories on Windows
([Learn](https://learn.microsoft.com/en-us/windows/wsl/case-sensitivity)):

> "Some Windows applications, using the assumption that the file system is case
> insensitive, don't use the correct case to refer to files. For example, it's not
> uncommon for applications to transform filenames to use all upper or lower case.
> **In directories marked as case sensitive, this means that these applications
> can no longer access the files.**"

A Windows toolchain on a case-sensitive virtiofs volume is in exactly that
position, by default, with no NTFS involved.

Real breakage this produces, *(project issue tracker)*:

| Issue | State | Substance |
| --- | --- | --- |
| [#517](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/517) | closed | `cargo build` fails: "could not canonicalize path" for every ancestor, then `os error 1005` (`ERROR_UNRECOGNIZED_VOLUME`). Maintainer workaround: mount via the Mount Manager, `MountPoint = \\.\Z:`. |
| [#777](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/777) | closed | `git clone` of a large bundle **crashes the virtiofs service**. Resolved by a *host-side* libvirt change (`queue="1024"`), not a code fix. Secondary failure needed `git config core.protectNTFS false`. |
| [#985](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/985) | **open** | `rm -rf` of a directory **silently fails** — i.e. clean/rebuild steps. |
| [#1149](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1149) | **open** | self-symlink crashes the virtiofs service |
| [#1376](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1376) | **open** | corrupted recycle bin on Windows 11 |
| [#1039](https://github.com/virtio-win/kvm-guest-drivers-windows/issues/1039) | **open** | throughput 45–60 MB/s vs ~275 MB/s for Samba on the same host; reporter notes directory listings are conversely *faster* than Samba |

Those three named tools — `cargo`, `git clone`, `rm -rf` — are the workload in
question.

### 2.2 SMB from a Windows guest

- **Case sensitivity is a protocol property, not a Samba knob.** MS-SMB2 §2.2:
  "**Unless otherwise specified, the SMB 2 Protocol does not support
  case-sensitivity in handling strings** (file names, directory names, path names,
  share names, stream names, etc.)"
  ([spec](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-smb2/6eaf6e75-9c23-4eda-be99-c9223c60b181)).
  Samba's man page concurs: "**No Windows or DOS system supports case-sensitive
  filename** so setting this option to auto is the same as setting it to no for
  them" and "Samba … is case insensitive but case preserving"
  ([smb.conf.5](https://github.com/samba-team/samba/blob/master/docs-xml/manpages/smb.conf.5.xml)).
  A Linux-side tree containing both `Makefile` and `makefile` is not safely
  addressable from the guest.
- **Linux symlinks are invisible.** Samba resolves them server-side and confines
  them to the share; the in-source comment is explicit — "Right now, SMB2 and SMB1
  always traverse symlinks within the share"
  ([`source3/smbd/filename.c`](https://github.com/samba-team/samba/blob/master/source3/smbd/filename.c)).
  `wide links` defaults **no**, and "will be automatically disabled … if the
  `unix extensions` option is on"
  ([doc](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/misc/widelinks.xml)) —
  and `unix extensions` defaults **yes**, so out of the box a symlink escaping the
  share root fails. Reparse points created *by* the Windows client do round-trip,
  stored as an xattr (`SAMBA_XATTR_REPARSE_ATTRIB`,
  [`util_reparse.c`](https://github.com/samba-team/samba/blob/master/source3/modules/util_reparse.c)),
  but as an opaque blob no Linux-side tool interprets.
- **Alternate data streams** require `vfs_streams_xattr`, whose own man page says
  it "will fail for applications that store serious amounts of data in ADSs"
  ([Samba](https://www.samba.org/samba/docs/current/man-html/vfs_streams_xattr.8.html)).
- **Long paths.** MAX_PATH is 260, and Microsoft's own illustration is on the
  nose: "you may hit this limitation if you are cloning a git repo that has long
  file names into a folder that itself has a long name"
  ([Learn](https://learn.microsoft.com/en-us/windows/win32/fileio/maximum-file-path-limitation)).
  `\\?\UNC\server\share` raises it, but opting out needs **both**
  `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled = 1` **and** a
  `longPathAware` manifest — a per-application property vmlab cannot set for the
  user's toolchain. Note also that a UNC prefix consumes more of the 260 than
  `Z:\` does, so **vmlab's drive-letter share form buys real headroom** over the
  folder-path form.
- **Byte-range locks do cross the boundary**, unlike caching: `posix locking`
  (default yes) means "file locks obtained by SMB clients are consistent with
  those seen by POSIX compliant applications accessing the files via a non-SMB
  method"
  ([doc](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/locking/posixlocking.xml)).
  But `strict locking` defaults to `Auto`, which "performs file lock checks only
  on non-oplocked files"
  ([doc](https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/locking/strictlocking.xml)) —
  so for a leased file, enforcement is delegated to the client and a host-side
  process is unconstrained.

### 2.3 Path form: drive letter vs junction — and what vmlab does

PRD §7.5 carries a ⚠ implementation note asking someone to verify the folder-path
mechanism against current Windows. Answered, for the SMB transport:

| Form | First-party status |
| --- | --- |
| Mapped drive letter `Z:\proj` | Supported. Shortest prefix → most MAX_PATH headroom. |
| UNC `\\gw\share\proj` | Supported. `FileSystemWatcher.Path` "supports Universal Naming Convention (UNC) paths". |
| **Junction** (`mklink /J`) → UNC or mapped drive | **Not permitted.** ["Hard links and junctions"](https://learn.microsoft.com/en-us/windows/win32/fileio/hard-links-and-junctions): "The following references **aren't permitted because they reference mapped network volumes** … `C:\dir1` linked to `Z:\dir2`". |
| **Directory symbolic link** (`mklink /D`) → UNC | **Supported.** ["Creating symbolic links"](https://learn.microsoft.com/en-us/windows/win32/fileio/creating-symbolic-links): "**Symbolic links can point directly to a remote file or directory using the UNC path.**" |

`mklink` reference:
[`/d` "Creates a directory symbolic link", `/j` "Creates a Directory Junction"](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/mklink).

**vmlab already does the correct one.** `src/smb/mount.rs::windows_mount_cmds`
emits `cmd /c mklink /D <folder> \\<gw>\<share>` for a folder-path target. Had it
used `/J`, Microsoft documents that it would not work.

The remaining gate is symlink evaluation: a local path pointing at a UNC target is
the **L2R** case of
[`fsutil behavior set symlinkevaluation`](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/fsutil-behavior),
which is Group-Policy-overridable. **The defaults for L2L/L2R/R2L/R2R are not
stated by any first-party page I could find** — the widely-repeated "L2L and L2R
on, R2L and R2R off" is attested only by an archived MSDN blog and secondary
sources. §7 lists this as needing a one-line check in the guest.

**For the virtiofs transport the picture is different and worse for folder
paths.** vmlab passes the declared `guest` path straight through to
`launchctl start virtiofs viofs-<tag> <tag> <guest>` (`src/smb/steps.rs`), and
viofs forwards it verbatim to `FspFileSystemSetMountPoint`. WinFsp's documentation
([`WinFsp-API-winfsp.h.md`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/doc/WinFsp-API-winfsp.h.md)):

> "Directories: They can be used as mount points for disk based file systems. They
> cannot be used for network file systems. This is a limitation that Windows
> imposes on junctions."

A directory mount point is a **real NTFS junction** created with
`FSCTL_SET_REPARSE_POINT`, and the directory is opened
`FILE_FLAG_DELETE_ON_CLOSE`
([`src/dll/mount.c#L474-L583`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/src/dll/mount.c#L474)).
WinFsp's FAQ explains why: "inability to mount over a non-empty directory on
Windows … NTFS disallows the creation of (mountpoint) reparse points on non-empty
directories", and "There is no way to guarantee the removal of a reparse point in
the same way." So for virtiofs a folder-path `guest` target:

- requires the directory to be absent or empty,
- vanishes on service stop / crash / reboot,
- and per issue #517 the Mount-Manager form (`\\.\Z:`) was the maintainer's fix
  for a toolchain (`cargo`) that could not cope with the plain form.

Drive-letter targets avoid all of it.

### 2.4 git

git's own documentation is unusually explicit about network filesystems.

- **`git fsmonitor--daemon`** (verbatim from
  [`Documentation/git-fsmonitor--daemon.adoc`](https://github.com/git/git/blob/master/Documentation/git-fsmonitor--daemon.adoc)):
  "By default, the fsmonitor daemon **refuses to work with network-mounted
  repositories**; this may be overridden by setting `fsmonitor.allowRemote` to
  `true`. Note, however, that the fsmonitor daemon is **not guaranteed to work
  correctly** with all network-mounted repositories, so such use is considered
  **experimental**." Its IPC socket is likewise "not [supported] on
  network-mounted filesystems, NTFS, or FAT32."
- **`core.fsmonitor`**: "The built-in file system monitor is currently available
  only on a limited set of supported platforms. Currently, this includes
  **Windows and MacOS**"
  ([`config/core.adoc`](https://github.com/git/git/blob/master/Documentation/config/core.adoc)).
  So a *guest-local* NTFS workspace gets git's fastest `git status` path; a shared
  one does not.
- **`core.ignoreStat`**: "useful on systems where lstat() calls are very slow,
  such as **CIFS/Microsoft Windows**" — git ships a knob whose documented
  justification is that SMB `lstat` is slow. The cost is that "the user will need
  to stage the modified files explicitly."
- **`core.fscache`** (Git for Windows only): "Git for Windows uses this to
  **bulk-read and cache lstat data of entire directories** (instead of doing lstat
  file by file)"
  ([Git for Windows](https://github.com/git-for-windows/git/blob/main/Documentation/config/core.adoc)) —
  the single most useful knob for `git status` latency over either transport.
- **`core.ignoreCase`** is probed and recorded at clone/init time, and "Git relies
  on the proper configuration of this variable for your operating and file system.
  Modifying this value may result in unexpected behavior." A repo cloned on the
  Linux host records `core.ignoreCase = false`, then gets used from a Windows
  guest through a case-**insensitive** SMB view — a mismatch git warns about
  directly.
- **`safe.directory`**: "By default, Git will refuse to even parse a Git config of
  a repository owned by someone else"
  ([doc](https://github.com/git/git/blob/master/Documentation/config/safe.adoc)).
  Ownership as reported over SMB will not match the guest user's SID; expect the
  "dubious ownership" refusal.
- **`core.protectNTFS`** defaults true on Windows and, per virtio-win #777, had to
  be turned off to check out a real repository on a virtiofs share.

I found **no** git documentation saying git as a whole is unsupported on a UNC or
mapped path. The exclusions git documents are specific: fsmonitor, and the
performance knobs that exist because network stat is slow.

---

## 3. Performance

**No first-party benchmark for build-shaped workloads exists on any of the four
sources.** Stating that plainly is more useful than a fabricated number.

- The **virtio-fs project publishes nothing quantitative.** Its site is six pages
  (index, design, howto-boot/kata/qemu/windows) with no performance page. Its
  claims are qualitative: virtio-9p and other network protocols "are not optimized
  for virtualization use cases. As a result they do not perform as well as local
  file systems" ([virtio-fs.gitlab.io](https://virtio-fs.gitlab.io/)). The QEMU
  blog post on virtio-fs is likewise qualitative and defers to a third-party
  thesis for figures.
- **Microsoft publishes no SMB round-trip cost model.** The closest first-party
  quantitative statement is on the redirector caches: turning off the file
  information cache "could **nearly double the number of network transactions**
  required for executing a given scenario"
  ([Learn](https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-7/ff686200(v=ws.10))).
  The SMB tuning page's only relevant advice for many-small-files is parallelism
  (`robocopy /mt`), plus `AsynchronousCredits` (default 512), which it notes
  matters "**for file change notification requests, in particular**"
  ([Learn](https://learn.microsoft.com/en-us/windows-server/administration/performance-tuning/role/file-server/smb-file-server)).
- **Samba's only figure is for oplocks**: "The oplock code can dramatically
  (approx. 30% or more) improve the speed of access to files on Samba servers."

What *is* documented is the **structure** of the cost, which is enough to reason
about shape:

**virtiofs on Windows pays three layers per uncached operation.** WinFsp's own
performance doc
([`WinFsp-Performance-Testing.asciidoc`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/doc/WinFsp-Performance-Testing.asciidoc))
says "a single file system operation may require a roundtrip to the user mode file
system and **such a roundtrip requires two process context switches**", and — on
exactly the two operations a build is made of:

> `iter.file_open_test` (repeatedly opening the same files): "NTFS has the best
> performance … with **NTPTFS a distant third**. … **The problem is that in most
> cases the WinFsp API design requires a round-trip to the user mode file system
> when opening a file. Improving WinFsp performance here would likely require
> substantial changes to the WinFsp API.**"

> `file_list_single_test`: "NTFS has again best performance … **NTFS does a better
> job than WinFsp at caching directory data.**"

NTPTFS is the pass-through file system, the closest analogue to viofs — and for
viofs every one of those round trips is *additionally* a FUSE virtqueue round trip
to `virtiofsd` on the host.

viofs's caching choices sharpen it. It sets **`FileInfoTimeout = 1000` and nothing
else**, so file-info, dir-info, security and volume-info caches all expire at 1 s:
repeated `stat`/`FindFirstFile` inside a second are free, everything beyond is a
fresh host round trip. It enables `PostCleanupWhenModifiedOnly` and
`PassQueryDirectoryFileName` (good), leaves `PassQueryDirectoryPattern` commented
out (so `*.obj`-style scans enumerate the whole directory in the FSD), and sets
`FlushAndPurgeOnCleanup = 1` — whose in-source consequence WinFsp spells out:
"the FSD simply purges the cache section (if any), which means that **CACHED DATA
WILL BE LOST**"
([`file.c#L1009-L1035`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/src/sys/file.c#L1009)).
A build that writes a file and immediately re-reads it goes back to the host.

On the plus side viofs negotiates `FUSE_DO_READDIRPLUS` (attributes arrive with
directory entries), which is a genuine win for tree walks — consistent with the
#1039 reporter's observation that virtiofs directory listings beat Samba even
though throughput does not.

**If the notifier is ever enabled**, add one synchronous `FUSE_GETATTR` per open
handle per interval, the whole sweep under a single exclusive `SRWLOCK` that
`Create`/`Open`/`Close` also contend on. A build holding thousands of handles
turns into thousands of host round trips every 3 s.

**SMB pays a network round trip plus a Samba `stat`.** The mitigations that exist
are the redirector caches (10 s / 10 s / 5 s), leases and directory leases — and
per §1.3 you can only keep those by giving up out-of-band coherency.

**Guest-local disk pays none of it.** NTFS on a qcow2 linked clone is a local
filesystem to every layer above it.

---

## 4. Linux guests and container micro-VMs, briefly

### 4.1 inotify over virtiofs does not fire for host-side changes either

The Linux case is not better; it fails the same way, for reasons three layers
deep. All three are source findings — no project doc states the conclusion
outright.

1. **FUSE reverse notifications exist but never reach fsnotify.**
   `enum fuse_notify_code { FUSE_NOTIFY_POLL, INVAL_INODE, INVAL_ENTRY, STORE,
   RETRIEVE, DELETE, RESEND, INC_EPOCH, PRUNE }`
   ([`include/uapi/linux/fuse.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/linux/fuse.h)),
   but
   [`fs/fuse/notify.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/fuse/notify.c)
   contains **zero** `fsnotify_*` calls. `fuse_reverse_inval_entry()` only
   invalidates dentries. A daemon that sent `INVAL_ENTRY` would refresh the guest's
   cache and deliver no event.
2. **virtiofsd never sends one.** Verbatim comment in
   [`src/vhost_user.rs`](https://gitlab.com/virtio-fs/virtiofsd/-/blob/main/src/vhost_user.rs):
   "Since `VIRTIO_FS_F_NOTIFICATION` is not advertised we do not have a
   notification queue."
3. **The guest driver has no notification queue.**
   [`fs/fuse/virtio_fs.c`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/fuse/virtio_fs.c)
   declares only `enum { VQ_HIPRIO, VQ_REQUEST }`.

And the spec's notification queue is not for this anyway: the
[virtio spec](https://github.com/oasis-tcs/virtio-spec/blob/master/device-types/fs/description.tex)
enumerates exactly one supported message type, `FUSE_NOTIFY_LOCK` (blocking POSIX
locks).

Corroborated by Kata *(project issue tracker)*:
[kata-containers#1879](https://github.com/kata-containers/kata-containers/issues/1879),
"virtio-fs: inotify does not work" — "While there is discussion on enabling in
FUSE itself and then in virtio-fs, **this will be months of work without any
active developer focusing on this.**"

**Cache coherency for the ordinary case is, however, fine.** vmlab runs
`virtiofsd --cache auto` (`src/qemu/virtiofsd.rs`). Read from
[virtiofsd source](https://gitlab.com/virtio-fs/virtiofsd/-/blob/main/src/main.rs),
`auto` = 1 s entry/attr TTL and no `FOPEN_KEEP_CACHE`, i.e. close-to-open
consistency — a guest that re-opens a file sees the host's version. virtiofsd's
own doc comments reserve `always`/`metadata` for "when the file system has
exclusive access to the directory", and `never` for "when file contents may change
without the knowledge of the FUSE client"
([`src/passthrough/mod.rs`](https://gitlab.com/virtio-fs/virtiofsd/-/blob/main/src/passthrough/mod.rs)).
`auto` is the right choice for vmlab's shape; it is unsafe only for files held
open across a host write, and for mmap. **Data staleness is not the problem —
event delivery is.**

**DAX would not rescue the read path.** Guest kernel support is merged
(`CONFIG_FUSE_DAX`), but QEMU master's `hw/virtio/vhost-user-fs.c` exposes only
`chardev`, `tag`, `num-request-queues`, `queue-size` — **no `cache-size`**, so no
DAX window can be created; virtiofsd implements no `SETUPMAPPING`; the project
says "This feature is not complete and **only available when building from
source**" ([howto-qemu](https://virtio-fs.gitlab.io/howto-qemu.html)) and marks it
`[experimental]` on the front page. Windows viofs has no DAX at all (zero
occurrences of `dax` in the source tree).

### 4.2 Container micro-VMs change nothing at the protocol level

Kata is the reference deployment and it ships `cache=auto`, DAX off
(`DEFVIRTIOFSCACHESIZE ?= 0`), `--thread-pool-size=1`
([`src/runtime/Makefile`](https://github.com/kata-containers/kata-containers/blob/main/src/runtime/Makefile)) —
the same posture vmlab has.

Kata's answer to the inotify problem is the interesting part, because it is the
only working one anybody has shipped: **poll on the host side and copy into a
guest-local filesystem where inotify is real.**
[PR #1964](https://github.com/kata-containers/kata-containers/pull/1964) added
"watchable mounts"; the agent "creates a watchable path in tmpfs and polls the
watchable-bind source to keep the mount-point synchronized". The limits in
[`src/agent/src/watcher.rs`](https://github.com/kata-containers/kata-containers/blob/main/src/agent/src/watcher.rs)
say everything about what it costs:

```
MAX_ENTRIES_PER_STORAGE:      16 files
MAX_SIZE_PER_WATCHABLE_MOUNT: 1 MiB
WATCH_INTERVAL_SECS:          2
```

16 files. 1 MiB. That is what "make host edits visible to a guest watcher"
actually costs when you build it — and it is three orders of magnitude too small
for a source tree.

---

## 5. What the devcontainer world does about the same problem

The ecosystem hit this exact wall and its answer is the one this document reaches.

**The documented problem.** VS Code, "Improve disk performance"
([docs](https://code.visualstudio.com/remote/advancedcontainers/improve-performance)):

> "The Dev Containers extension uses 'bind mounts' to source code in your local
> filesystem by default. While this is the simplest option, **on macOS and
> Windows, you may encounter slower disk performance** … Since macOS and Windows
> run containers in a VM, 'bind' mounts are not as fast as using the container's
> filesystem directly."

**And Docker states the watching half flatly** — this is the strongest primary
source on the whole topic
([Docker docs, WSL best practices](https://docs.docker.com/desktop/features/wsl/best-practices/)):

> "**Linux containers only receive file change events, 'inotify events', if the
> original files are stored in the Linux filesystem.** For example, some web
> development workflows rely on inotify events for automatic reloading when files
> have changed."

Microsoft says the same for WSL generally
([Learn](https://learn.microsoft.com/en-us/windows/wsl/filesystems)): "We recommend
against working across operating systems with your files… For the fastest
performance speed, store your files in the WSL file system if you are working in a
Linux command line… If you're working in a Windows command line, store your files
in the Windows file system." The corresponding notification gap is tracked as
[microsoft/WSL#4739](https://github.com/microsoft/WSL/issues/4739), **still open
since 2019** *(project issue tracker)*, with a Microsoft maintainer confirming in
2024 that the workaround is still "using `\\wsl.localhost\` to access the files
from the Linux file system".

**The pattern: clone into a volume.**
[Docs](https://code.visualstudio.com/docs/devcontainers/containers#_quick-start-open-a-git-repository-or-github-pr-in-an-isolated-container-volume):
"Repository Containers use isolated, local Docker volumes instead of binding to
the local filesystem. In addition to not polluting your file tree, local volumes
have the added benefit of improved performance on Windows and macOS." Mechanically
it is a named volume as `workspaceMount` with `type=volume`, cloned into from
inside a bootstrap container. The Dev Containers release notes give the reason:
"Using a Docker volume has better disk performance because it uses a Linux
filesystem **without any extra layer** between the Linux Kernel and the
filesystem."

**What it buys:** native filesystem semantics, native event delivery, no
cross-OS layer, and survival across `Rebuild Container` — "named volume that can
act like the container's filesystem but **survives container rebuilds**".

**What it costs — and note that VS Code documents almost none of this:**

| Cost | Evidence |
| --- | --- |
| Host tools cannot see the workspace | Docker, not VS Code: "**Volumes are not a good choice if you need to access the files from the host**, as the volume is completely managed by Docker" ([docs](https://docs.docker.com/engine/storage/volumes/)) |
| Getting files out needs a container | VS Code's answer is `Dev Containers: Explore a Volume in a Dev Container...`, or Docker's `--volumes-from` + `tar` dance |
| A failed rebuild has no local fallback | With a volume you get **Open in Recovery Container**; with a bind mount you get **Open Folder Locally** |
| `git push` is effectively the only durable exit | Implied by the credential-sharing guidance; **never stated** |
| Volume deletion destroys uncommitted work | VS Code ships `Clean Up Dev Volumes...` and `docker volume prune` exists; **no warning is documented anywhere** |

The last two are a genuine documentation gap, confirmed by searching
`microsoft/vscode-docs` for any warning about uncommitted work in a dev volume and
finding none. **vmlab should not repeat that gap** — if a dev machine's workspace
lives on a disposable linked clone (PRD §7.1: "Clones live in `.vmlab/` and are
disposable: `destroy` deletes them"), the "what happens to my uncommitted work"
question is *sharper* for vmlab than for Docker, because `vmlab destroy` is a
first-class verb.

One more asymmetry worth carrying into the §19 spec: `Rebuild Container` "will
'reset' the container to its starting contents (**with the exception of your local
source code**)"
([docs](https://code.visualstudio.com/docs/devcontainers/create-dev-container)) —
that exception is exactly what a guest-local workspace gives up, and it is the
substance of #47's open "rebuild story" question. Snapshot revert (§7.3) rolls the
workspace back with the machine; a share would not (§7.5: "Share contents are
outside snapshot scope"). Which behaviour a dev machine wants is now a real
decision rather than a side effect.

---

## 6. Corrections and confirmations for PRD §7.5

| §7.5 claim | Verdict |
| --- | --- |
| Folder-path target "realised as a directory symlink/junction to the UNC path" with a ⚠ to verify `mklink`-to-UNC | **Confirmed correct as implemented.** `mklink /D` to a UNC target is documented supported; `mklink /J` is documented **not** permitted for network volumes. `src/smb/mount.rs` uses `/D`. Residual gate: `fsutil behavior` L2R symlink evaluation (defaults not first-party documented — see §7). |
| "snapshot-safe since QEMU ≥8.2 / **virtiofsd ≥1.10**" | **QEMU floor confirmed; virtiofsd floor is wrong.** QEMU `v8.1.0`'s `vuf_vmstate` is `.unmigratable = 1`; `v8.2.0` has a real VMState with a back-end subsection (commit [`bca3e2a13814`](https://github.com/qemu/qemu/commit/bca3e2a13814)). virtiofsd's README states "QEMU version 8.2 or newer is required". But `--migration-mode` is **absent in virtiofsd v1.10.0 and present in v1.11.0** (2024-06-06). **The PRD should say ≥1.11.0.** |
| virtiofs shares "transfer the virtiofsd session state through the snapshot's migration stream" (§7.3) | Confirmed — and virtiofsd documents the limit vmlab already states: "virtiofsd's state includes absolutely **no data**; therefore, some mechanism outside of virtio-fs/virtiofsd must be used to ensure that when restoring such a snapshot, the shared directory is in exactly the same state" ([`doc/migration.md`](https://gitlab.com/virtio-fs/virtiofsd/-/blob/main/doc/migration.md)). vmlab's `--migration-mode=find-paths` is documented as "vulnerable to third parties changing metadata in the shared directory", which `--migration-verify-handles` (which vmlab passes) mitigates. |
| "Windows needs the virtio-win driver + WinFsp in the template" | Correct, and worth adding: virtio-win's own wiki calls Windows virtiofs "a Tech Preview feature", and `cargo build`, `git clone` and `rm -rf` all have tracker issues against it (§2.1). |
| Windows virtiofs `readonly` | **Observation, out of this ticket's scope:** `src/smb/steps.rs` honours `VirtiofsMount::readonly` on Linux (`-o ro`) but not on the Windows `launchctl` path. `virtiofsd --readonly` still applies host-side, so the share is read-only either way — but the guest-side flag is silently dropped. Worth a separate issue if it matters. |

---

## 7. Unverified / needs testing

Honest accounting of what this document does **not** establish. Nothing below was
run; everything below is settleable on one real vmlab lab.

### 7.1 Claims resting on source reading, not documentation

| # | Claim | Why it matters | Test |
| --- | --- | --- | --- |
| U1 | **Samba's `SMB2_WATCH_TREE` does not deliver host-side changes below the first level.** Derived from `notify_inotify.c` (`inotify_watch()` never consumes `*subdir_filter`) + `notifyd.c`. No Samba doc says this. | It is the whole SMB verdict. | Lab with an SMB share. In-guest: `FileSystemWatcher` with `IncludeSubdirectories = true` on the share root, logging every event. On the host: `touch <share>/a.txt`, then `touch <share>/sub/b.txt`, then `git checkout` a branch in `<share>`. Record which fire. Repeat with `IncludeSubdirectories = false` on `<share>/sub`. |
| U2 | **viofs's `FILE_ACTION_MODIFIED`-on-the-directory notification may not reach a watcher registered *on* that directory.** WinFsp's `FspNotifyReportChange` passes the last-component offset, which routes to watchers of the **parent** ([`file.c#L2366-L2371`](https://github.com/winfsp/winfsp/blob/43b9a6e2228d2095c8f46dd49d652b2f7d8fc871/src/sys/file.c#L2366)). Not traced through `FsRtl` internals. | Determines whether the unreleased notifier is worth anything even once shipped. | Blocked until virtio-win ships `-n`. Then: enable `-n 1`, watch `X:\` and `X:\sub` separately, edit on host, record which watcher fires. |
| U3 | **inotify does not fire in a Linux guest for host-side virtiofs changes.** Airtight from three independent source layers, but no doc sentence. | Decides whether the Linux dev machine is any different from Windows. | Linux guest, virtiofs share, `inotifywait -m -r /mnt/src` while the host writes `/mnt/src/a` and `/mnt/src/sub/b`. |
| U4 | `fsutil behavior` **L2R symlink-evaluation defaults**. Not documented first-party. | vmlab's folder-path SMB shares depend on it. | `fsutil behavior query symlinkevaluation` in each guest profile (Server 2019/2022/2025, Win 10/11), fresh template. One line; settle it and record it. |

### 7.2 Performance — what to measure, since nobody publishes it

No first-party numbers exist (§3). Rather than invent an order of magnitude, run
**one** benchmark across **three** placements of the same tree, on one lab, one
Windows guest, warm and cold:

- **Placements:** (a) guest-local disk (qcow2 linked clone, NTFS); (b) virtiofs
  share, `--cache auto` (vmlab's current setting); (c) SMB share via the segment
  gateway's `smbd`.
- **Workloads**, chosen because each isolates a different documented cost:
  1. `git clone` a mid-size repo, then `git status` cold and `git status` warm —
     isolates `lstat` storms; also exercises `core.fscache` / `core.protectNTFS` /
     `safe.directory` behaviour (§2.4).
  2. `dotnet build` (or `msbuild`) of a multi-project solution, clean then
     incremental — isolates many-small-reads plus writing an `obj/`/`bin/` tree,
     and tests MSBuild's timestamp comparison against the transport's metadata
     staleness.
  3. A bare directory walk (`Get-ChildItem -Recurse | Measure-Object`) — isolates
     `NtQueryDirectory`, where §3 predicts virtiofs may beat SMB even while losing
     on throughput.
  4. Language-server cold start (open the solution in an attached editor, time to
     first completion) — the workload #47 actually cares about.
- **Also record**, because they are the real decision inputs and cost nothing
  extra: whether a `FileSystemWatcher` on each placement sees a host-side edit at
  all (U1), and how long a host-side change takes to become visible to a plain
  `stat` (predicted ~1 s on virtiofs, up to ~10 s on SMB per the redirector
  caches).
- **Resolution needed:** order of magnitude. A single run of each is enough to
  decide; this is a decision input, not a benchmark suite.

**Prediction to falsify** (so the measurement is not an open-ended fishing trip):
guest-local wins every workload; SMB beats virtiofs on bulk throughput and loses
on directory enumeration; both shares lose to guest-local by enough that no tuning
closes the gap. If that prediction is wrong, the conclusion in §"Bottom line" is
unaffected — **the change-notification finding is decisive on its own, and
performance only changes how much worse the alternative is.**

### 7.3 Other things I could not establish

- **Which virtio-win release will first contain the notifier**, or whether the
  installer will enable it. It merged after build tag `mm322`; nothing in the repo
  indicates a target release.
- **Whether Windows viofs supports DAX** — concluded from absence of any
  implementation and absence from the project's Windows how-to, not from an
  explicit denial.
- **What `FileSystemWatcher`'s "Remote computers must have one of the required
  platforms installed" means for a Samba server.** The .NET docs never enumerate
  the platforms.
- **How many round trips one `SMB2 CREATE` + query + `CLOSE` actually costs**, and
  how much compounding recovers. MS-SMB2 permits compounding and states no cost
  model.
- **Whether `docker`'s `consistency=cached`/`delegated` are now no-ops.** Widely
  repeated; no primary source asserts it. They are absent from the current Engine
  bind-mount options table, "platform specific" per Compose, still fully
  documented for Swarm services, and the devcontainers CLI still emits
  `consistency=cached` by default on non-Linux hosts.
- **Nothing here was measured.** Every performance statement in §3 is structural
  (round-trip counts, cache TTLs, documented API costs), not empirical.
