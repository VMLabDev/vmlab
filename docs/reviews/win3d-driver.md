# Review: the win3d guest driver package and the Windows shared-memory display

Card #741, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Line numbers of files in this repository are those of commit `a89166b4`
(`next` at the start of the card).

The kernel driver is not in this repository. `guest/win3d/build/kmd/PINS`
pins `arehnman/yttrium-virtio-gpu` at `e9917ae25b2597306d483e1063dba08658139bc9`,
and `guest/win3d/patches/kmd/series` lists 15 patches on it. A path that
starts with `viogpu/` is a file of that source, and its line number is the
line after all 15 patches are applied in series order. To get the same tree:
unpack the pinned commit and run `git apply` for each patch of the series.

Most kernel findings are in code that upstream wrote and no patch here
touches. They are in this list because the package builds, signs and ships
that code, and the patches add new callers of it.

## How the review was done

The review is by reading. No test was written or run, no driver was built,
and no guest was started. Each finding is a claim from the code. The proposed
test is what proves or refutes it.

Three statements about Windows are from the reviewer's knowledge and were
not checked on a guest:

- `dxgkrnl` copies the private data of `D3DKMTEscape` into kernel memory and
  calls `DxgkDdiEscape` in the context of the calling process. A pointer
  inside that data is not probed by anybody.
- A directory made under `C:\ProgramData` inherits an access entry that lets
  the group Users create files and directories in it, and one that makes the
  creator of a file its owner with full control.
- A shift of a 64-bit value by a count of 64 or more uses the count modulo 64
  in the x64 code that clang and MSVC emit.

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic. "User" means an unprivileged
program in the guest. "Host" means the virtio-gpu device: vmlab-hv, or QEMU
for a guest that is not x86.

### What was read

- All 15 patches of `guest/win3d/patches/kmd/`, and in the patched source:
  `viogpu/viogpu3d/viogpu_adapter.cpp` lines 1395-1930 (the escape handler),
  2503-2540 and 4120-4200, all of `viogpu_allocation.cpp`,
  `viogpu_blob_user_view.cpp` lines 60-258, `viogpu_command.cpp` lines
  279-1215, 1235-1292 and 1393-1445, `viogpu_device.cpp` lines 57-300 and
  400-735, `viogpu_native_clock.h`, `viogpu/shared/viogpum.h`,
  `viogpu/shared/viogpu_clock.h`, and in `viogpu/common/viogpu_queue.cpp`
  the capset, display-info and EDID requests and the synchronous wait.
- The two helpers the INF installs, where they meet a user:
  `viogpu/shared/pipe.cpp`, `viogpu/shared/pipe.h` and
  `viogpu/viogpusc/Session.cpp`.
- `guest/win3d/installer/vmlab-win3d.ps1` (the state directory, the bundle
  copy, the file-security helpers, the startup tasks, the trust step, the
  bridge install, the remove step and the main block), `Install.cmd`,
  `README.txt`, `guest/win3d/build/signing/make-test-cert.sh`,
  `guest/win3d/build/kmd/README.md` and
  `guest/win3d/docs/decisions/0001-signing.md`.
- `guest/vdisplay-windows/`: `protocol.h`, `driver.cpp`, `install.ps1`,
  `sign.ps1`, the service and process-launch parts of `install.cpp`, and
  `README.md`.

### What was not read

- The rest of the kernel driver: `viogpu_vidpn.cpp` except the EDID and
  display-info loops, `viogpu_adapter.cpp` outside the ranges above (start,
  stop, the interrupt routine, the recovery of patch 0004 as a whole),
  `viogpu_shmem_allocator.cpp`, `viogpu_pci.cpp`, and the VirtIO library
  under `VirtIO/`. So this review does not say whether a used-ring entry
  with a wrong descriptor id or length is safe, nor whether the shared-memory
  capability of the device is bounded against its BAR.
- The recovery patch (`kmd-recover-from-timeouts.patch`) was read for its
  queue half only (the first 520 lines). Its locking was not checked.
- The user-mode drivers (the Mesa, vkd3d-proton, DXVK and dxil-spirv patches).
  Only the escape and allocation structures they share with the kernel
  driver were read.
- `guest/win3d/build/` except the files named above: the compile scripts,
  `sign-driver.sh`, `sign-inner.sh`, `load-test.sh`, and
  `build/package/make-inf.py`. The INF was read only for its two services.
- The installer's device pin, baseline capture, Vulkan loader steps and
  verification (`vmlab-win3d.ps1` lines 200-435, 480-590 and 760-910).
- `guest/vdisplay-windows/`: `build.py`, `package.py`, `rebuild.py`,
  `shm.py`, `check.cpp`, the tests, `build-vm/`, and the upstream IVSHMEM
  driver that the package carries as a signed binary.
- What vmlab-hv puts in `num_scanouts` and in the capset sizes. F5 and F6 are
  about a host that sends other values than a correct one.

## Findings

### F1. The capset escape writes to an address the user gives, without a probe

- Where: `viogpu/viogpu3d/viogpu_adapter.cpp:1597-1600` and the helper at
  `1403-1417`.
- Fault: `VIOGPU_GET_CAPS` copies the capset the host returned to
  `Capset.Capset`, a 64-bit value taken from the escape data, with a plain
  `memcpy` inside `__try`. No `ProbeForWrite` runs, and the source tree has
  no `ProbeFor` call at all. The `__try` catches a fault on a bad user
  address. It does nothing for an address in kernel space that is mapped and
  writable: the copy succeeds there. The user chooses the address and the
  length (`Capset.Size`, up to the capset's size). The bytes are the host's
  capset, which the user can read first with a normal call. The escape needs
  no device handle, only an adapter handle, which any process can open.
- Proof: a probe program run as a standard user. It reads the capset into
  its own buffer, then repeats the escape with `Capset.Capset` set to a
  kernel address. A safe target for the test is the address of a pool
  allocation that a test driver made and reports, filled with a known
  pattern. The test passes when the escape fails and the pattern is whole.
  Today the pattern is replaced by capset bytes. Without a test driver: pass
  `0xFFFF800000000000`, which is not mapped. The fault there is a bugcheck
  0x50, because `__try` does not catch an invalid kernel address.
- Severity: security against a hostile guest program (a write into kernel
  memory from an unprivileged process, which is a path to SYSTEM), and crash.

### F2. The installer's state directory can be owned or written by a standard user, and a SYSTEM task runs a script from it

- Where: `guest/win3d/installer/vmlab-win3d.ps1:1108` (the directory is made
  with `New-Item` and no access list), `:472` (the script is copied into it),
  `:604-609` (two startup tasks run that copy as SYSTEM at every boot),
  `:83-87` (`state.json` is written through a `.tmp` file in the same
  directory), `:745` and `:926-932` (a security descriptor, and in one case a
  backup path, are read from `state.json` and applied to
  `System32\d3d12.dll`).
- Fault: `C:\ProgramData\vmlab-win3d` gets the access list it inherits from
  `C:\ProgramData`. There are three routes.
  1. A standard user makes the directory before the first install. The user
     is then its owner. `New-Item -Force` accepts the directory as it is. The
     user can replace `vmlab-win3d.ps1` after the install, and the repair
     task runs it as SYSTEM at the next boot.
  2. With a directory the installer made, a user can still create new files
     in it. A user who creates `state.json.tmp` first owns that file.
     `Write-Json` writes into it and renames it to `state.json`, so the user
     keeps write access to the state. The remove step sets the owner and
     access list of `System32\d3d12.dll` from `native_d3d12_<arch>.sddl`,
     and when the private native copy is missing it copies the file named
     by `native_d3d12_<arch>.backup` there.
  3. The bundle is checked against `manifest.json`, which is in the same
     directory. On route 1 the user can change both.
- Proof: on a Windows 11 guest with a standard user `u` and an
  administrator. As `u`: `mkdir C:\ProgramData\vmlab-win3d`. As the
  administrator: run `Install.cmd` to the state `Installed`. As `u`: replace
  the content of `C:\ProgramData\vmlab-win3d\vmlab-win3d.ps1` with a script
  that writes the output of `whoami` to `C:\proof.txt`. Restart. The test
  passes when the replacement is refused or `C:\proof.txt` does not appear.
  Today the claim is that it holds `nt authority\system`. For route 2,
  without the pre-made directory: as `u`, create `state.json.tmp` before the
  install, and after it check with `icacls` who owns `state.json`.
- Severity: security against a hostile guest user (standard user to SYSTEM).

### F3. The capset id of the escape is not bounded before it indexes an array

- Where: `viogpu/viogpu3d/viogpu_adapter.cpp:1572-1578`.
- Fault: the check is `m_supportedCapsetIDs & (1ull << CapsetId)`. A shift by
  64 or more is undefined, and the x64 instruction uses the count modulo 64.
  A user value of 64 + k passes when capset k is supported. The value then
  indexes `m_capsetInfos`, which has 64 entries of 12 bytes. An index near
  2^32 reads about 48 GiB past the adapter object, which is not mapped. A
  small index reads other fields of the adapter as `max_size` and
  `max_version`, and `max_size` becomes an allocation size and a copy length.
- Proof: a probe as a standard user sends `VIOGPU_GET_CAPS` with
  `CapsetId = 0xFFFFFFC0 + k`, where k is a capset id that
  `VIOGPU_ADAPTERINFO.SupportedCapsetIDs` names. The test passes when the
  escape returns `STATUS_INVALID_PARAMETER_1`. Today the claim is a bugcheck
  0x50 in `viogpu3d.sys`.
- Severity: crash (the whole guest, from an unprivileged process).

### F4. The submit escape adds a user length in 32 bits and then copies that length

- Where: `viogpu/viogpu3d/viogpu_adapter.cpp:1864-1895`.
- Fault: `needed = sizeof(*req) + req->CmdSize` is a `UINT`. With `CmdSize`
  of `0xFFFFFFF4` or more the sum wraps to less than 12 and the size check
  passes. The driver then allocates `CmdSize` bytes of nonpaged pool, about
  4 GiB, and copies `CmdSize` bytes from an escape buffer that holds at most
  64 KiB. When the allocation fails the escape returns an error. When it
  succeeds, the copy reads about 4 GiB past the buffer.
- Proof: a probe as a standard user, on a guest with 16 GiB of memory so
  that the allocation can succeed, with a context made by `VIOGPU_CTX_INIT`.
  It sends `VIOGPU_SUBMIT_CMD` with `DataLength = 12`, `CmdType = 1` and
  `CmdSize = 0xFFFFFFF8`. The test passes when the escape returns
  `STATUS_INVALID_BUFFER_SIZE`. Today the claim is a bugcheck 0x50, or
  `STATUS_INSUFFICIENT_RESOURCES` on a guest too small for the allocation.
  A unit test of the size check alone (the three lines, compiled on the
  host) shows the wrap without a guest.
- Severity: crash. On a small guest: a 4 GiB nonpaged allocation attempt per
  call.
- Status: fixed by card #752.
  `guest/win3d/patches/kmd/kmd-check-submit-escape-size.patch` compares the
  size with a subtraction and returns `STATUS_INVALID_BUFFER_SIZE`.
  `guest/win3d/docs/acceptance/C752.md` has the proof, before and after.

### F5. The scanout count of the host is used as a loop bound over arrays of one and sixteen

- Where: `viogpu/viogpu3d/viogpu_adapter.cpp:3237-3240` reads `num_scanouts`
  from the device configuration with no check.
  `viogpu/viogpu3d/viogpu_vidpn.cpp:1791-1794` loops to it and writes 256
  bytes to `m_EDIDs[i]`, an array with one entry (`MAX_CHILDREN` is 1).
  `viogpu/common/viogpu_queue.cpp:679-691` reads 256 bytes at
  `edid + id * 128` from a 1024-byte field. `viogpu_vidpn.cpp:1692-1696`
  with `viogpu_queue.cpp:562` reads `pmodes[id]`, an array of 16.
- Fault: a host that reports two scanouts and offers `VIRTIO_GPU_F_EDID`
  makes the driver write 256 bytes past `m_EDIDs` into the fields that
  follow it in the VidPN object. A count above 7 reads past the EDID
  response, and a count above 16 reads past the display-info response. Two
  scanouts is a normal setting of QEMU (`max_outputs=2`), not only a hostile
  one.
- Proof: a host-side test. A virtio-gpu device model (vmlab-hv's test
  device, or QEMU `virtio-vga-gl,max_outputs=2` for an aarch64 guest) that
  reports `num_scanouts = 2` and answers `GET_EDID` for scanout 1. Start a
  Windows guest with the driver under Driver Verifier with special pool. The
  test passes when the driver starts and uses one scanout. Today the claim is
  a special-pool bugcheck or silent corruption of the VidPN object.
- Severity: security against a hostile host (a write past a kernel object),
  and crash on a host with a legal configuration.

### F6. The capset size of the host wraps the response size, and the escape then copies the unwrapped size

- Where: `viogpu/viogpu3d/viogpu_adapter.cpp:4181` stores
  `capset_max_size` from the host unchecked.
  `viogpu/common/viogpu_queue.cpp:777` computes
  `int resp_size = sizeof(GPU_RESP_CAPSET) + capset_size`.
  `viogpu_adapter.cpp:1597-1600` copies `min(Capset.Size, max_size)` bytes
  from `capset_data`.
- Fault: with a `capset_max_size` near 2^32 the sum wraps to a few bytes. The
  response buffer is then smaller than its own header, and the escape copies
  up to `Capset.Size` bytes from behind it to the user: kernel pool content,
  or a fault when the read leaves mapped memory.
- Proof: a host-side test with a device model that answers
  `GET_CAPSET_INFO` with `capset_max_size = 0xFFFFFFF0`. A probe in the guest
  asks for the capset with `Capset.Size = 65536`. The test passes when the
  driver refuses the capset at start or the escape fails. Today the claim is
  that the probe's buffer holds pool memory, or the guest bugchecks.
- Severity: security against a hostile host together with a guest user
  (kernel memory to user mode), and crash.

### F7. Any process can point the scanout at any resource

- Where: `viogpu/viogpu3d/viogpu_device.cpp:595-609` accepts
  `VIOGPU_CMD_PRESENT_FLIP` in the command stream of `DxgkDdiRender` and
  checks only its size and format. `viogpu/viogpu3d/viogpu_command.cpp:1144`
  and the lines after it send `SET_SCANOUT` or `SET_SCANOUT_BLOB` and a
  flush with the resource id, scanout id, size and offset from that stream.
  The escape `VIOGPU_RES_SET_SCANOUT_BLOB`
  (`viogpu/viogpu3d/viogpu_allocation.cpp:1093-1140`) does the same for an
  allocation of the caller.
- Fault: the flip of Windows comes through `DxgkDdiPresent`, which the
  kernel builds. The same command in a user stream is a second way to the
  scanout that Windows does not know about. A program without privilege can
  put an image of its own on the screen, over the desktop of another session
  or over the secure desktop, and the resource id in the stream is not
  checked against the resources of the caller's context.
- Proof: a probe as a standard user makes a blob allocation, fills it with
  one colour, and sends either the escape or a render with
  `VIOGPU_CMD_PRESENT_FLIP` for scanout 0. While a UAC prompt is open, a
  screenshot from the host (`vmlab screenshot`) is taken. The test passes
  when the screenshot shows the prompt. Today the claim is that it shows the
  probe's colour.
- Severity: security against a hostile guest program (it replaces what the
  screen shows), and wrong result.
- Status: fixed by card #755.
  `guest/win3d/patches/kmd/kmd-scanout-only-for-its-owner.patch` refuses the
  flip command in a render stream with `STATUS_PRIVILEGED_INSTRUCTION`, and
  serves the escape only for the process that Windows gave the scanout
  (`STATUS_ACCESS_DENIED` for every other). The escape was not removed: the
  user-mode driver shows the frames of a fullscreen application with it.
  `guest/win3d/docs/acceptance/C755.md` has the proof, before and after.

### F8. A present that a user can make fail is mapped in Patch, where a failure is a bugcheck

- Where: `viogpu/viogpu3d/viogpu_command.cpp:320-335`
  (`VioGpuCpuCopyBltSupported` accepts any blob source when the destination
  is not a blob), `viogpu/viogpu3d/viogpu_device.cpp:429-440` (the mapping
  is tried in `Present` only when both allocations are resident or already
  mapped), `viogpu_command.cpp:1420-1433` (the mapping in `Patch`, with the
  comment that Windows treats a failure there as bugcheck 0x119).
  `kmd-present-blt-into-mappable-blob.patch` moved the blob-destination case
  into `Present` for this reason. The blob-source case stayed.
- Fault: `MapForCpuCopy` fails for a blob source without the mappable flag
  (`viogpu_allocation.cpp:521-524`) and for a blob the host refuses to
  create (`:513-519`). Both properties come from the private data of
  `D3DKMTCreateAllocation`, which the user writes. When the source is not
  resident at `Present`, the failure comes in `Patch`.
- Proof: a probe as a standard user in a Venus context makes a blob
  allocation with `BlobFlags = 0` (or with a `BlobId` the host does not
  know) and presents it with a Blt to its window, in a loop while a second
  thread fills the segment so that allocations are evicted. The test passes
  when every present returns an error. Today the claim is a bugcheck 0x119.
  The reviewer did not work out how often `Patch` is reached with the source
  not resident: the test decides whether this is real.
- Severity: crash.

### F9. The pipe of the two helpers reads more bytes than its buffer has

- Where: `viogpu/shared/pipe.h:14` defines `DATA_BUFFER_SIZE` as
  `DATA_BUFFER_LENGTH * sizeof(wchar_t)` without parentheses.
  `viogpu/shared/pipe.cpp:133-137` sets
  `left = DATA_BUFFER_SIZE * sizeof(wchar_t)`, which is 4096, and reads that
  many bytes into a buffer of 1024 `wchar_t` (2048 bytes). Both read loops
  (`:137`, `:301`) add the byte count to an index of `wchar_t`, so a second
  piece of a message lands at twice the offset.
- Fault: one message of more than 2048 bytes overflows the heap buffer of
  the service `vgpusrv.exe`, which runs as SYSTEM. A message read in two
  pieces overflows the buffer of `viogpuap.exe`, which the service starts in
  the console session with the user's linked (elevated) token
  (`viogpu/viogpusc/Session.cpp:134-165`). The pipe is made with a null
  security descriptor and without `FILE_FLAG_FIRST_PIPE_INSTANCE`
  (`pipe.cpp:42-49`). The reviewer did not check who may write to it: with
  the default descriptor of a SYSTEM service it is administrators only, and
  then the overflow needs a pipe that another process made first under the
  same name.
- Proof: a host-buildable unit test: compile `pipe.cpp` with AddressSanitizer
  (mingw or MSVC), connect a client to a `PipeServer`, and write one message
  of 3000 bytes. The test passes when the server rejects the message. Today
  the claim is a heap-buffer-overflow report in `PipeServer::Read`.
- Severity: crash of the helper. Security against a hostile guest user only
  if the test of the pipe's access list shows that a standard user can reach
  either end.

### F10. A failed allocation in CreateAllocation is dereferenced

- Where: `viogpu/viogpu3d/viogpu_allocation.cpp:887-888`.
- Fault: `new (NonPagedPoolNx) VioGpuAllocation(...)` returns null when the
  pool is exhausted, and the next line calls `allocation->IsValid()`. The
  same function handles only `pAllocationInfo[0]` and ignores
  `NumAllocations`.
- Proof: Driver Verifier with low-resources simulation on `viogpu3d.sys`,
  and a probe that creates allocations in a loop. The test passes when every
  failure is `STATUS_INSUFFICIENT_RESOURCES`. Today the claim is a bugcheck
  with a null read in `DxgkCreateAllocation`. For the second half: a probe
  calls `D3DKMTCreateAllocation` with `NumAllocations = 2`, and the test
  passes when the call fails or both allocations work.
- Severity: crash.

## Notes

These have no proving test, or are decisions that are written down already.

- N1. Test signing. The installer turns on `testsigning` and puts the test
  certificate in `LocalMachine\Root` and `TrustedPublisher`
  (`vmlab-win3d.ps1:638-647`). `docs/decisions/0001-signing.md` decides
  this. What follows from it: the private key is on the build host without a
  passphrase (`make-test-cert.sh:49-51`, `-nodes`), it is valid ten years,
  and every guest that installed the package trusts code signed with it
  until the package is removed. The decision record says the key is in
  `.signing/` at the repository root. The script writes
  `~/.config/vmlab/signing/win3d/`. `guest/vdisplay-windows/sign.ps1` makes
  a new certificate per build and deletes its key, which has no such tail.
- N2. `viogpuap.exe` runs with the linked token. For an administrator with
  UAC that is the elevated token, on the user's desktop, with the user's
  environment block (`Session.cpp:134-165`). The reviewer found no way to
  use it, but a helper that only changes the resolution does not need it.
- N3. `VMLabDisplayConsole` starts a child with a copy of its own SYSTEM
  token in the console session, on `winsta0\default`
  (`guest/vdisplay-windows/install.cpp:108-123`). The child was not read for
  windows or message loops.
- N4. Waits that a user cannot leave. `VIOGPU_RES_BUSY` with `Wait` set
  waits in kernel mode, not alertable, while the resource is busy
  (`viogpu_allocation.cpp:1079-1086`). Every synchronous control request
  waits without limit for the host (`viogpu_queue.cpp:167-222`). A host that
  never answers leaves the calling thread, and its process, unkillable.
- N5. The driver bugchecks on purpose (0xE2) when a pool allocation for a
  submit fails (`viogpu_command.cpp:1085-1097`) and when a control request
  cannot be queued. Under memory pressure a user's render is then a crash
  where an error would do.
- N6. The two diagnostic escapes of patch 0004 (magic `0x56475458` and
  `0x56475444`, `viogpu_adapter.cpp:1425-1484`) answer any process. They
  return fence numbers, context ids and reset counts, and no pointer.
- N7. The trace build accepts a knob that stalls every present copy at
  DISPATCH_LEVEL for up to 20 ms, from any process
  (`viogpu_adapter.cpp:1447-1464`). `build/kmd/README.md` says a trace build
  is not for release. Nothing in the package step was read to see whether a
  trace build is refused there.
- N8. `UploadCursor` (`kmd-hardware-pointer.patch`) waits on one event for
  the no-op that follows the pointer upload, with a limit of 250 ms. The
  completion of an earlier no-op that passed its limit sets the same event,
  so a later upload can stop waiting too early and the host can show the
  old pointer image for one update. Cosmetic.
- N9. `guest/vdisplay-windows/driver.cpp:366` writes the pointer image and
  position from the timer without `publish_lock`, while the frame thread
  writes the frame header under it. The two areas do not overlap, and the
  host reads both through sequence counters, so this is a note.
- N10. The vdisplay driver validates the shared header once, keeps its own
  copy of the layout, and checks the one index it reads again later
  (`protocol.h:87-108` and `:143-147`). The reviewer found no host value that it trusts
  after validation.
- N11. `build/kmd/README.md` says the build runs no static analysis and
  turns several clang warnings off. F1, F3 and F4 are of the kind that
  analysis finds.
