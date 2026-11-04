---
id: "14"
kind: research
---

# Windows file sharing options

## Question
What are the credible options for fast, reliable host-to-guest file sharing into
Windows guests now that we own the hypervisor and could write guest-side code?
Feeds Q25 (transport choice; does virtiofsd stay). Today: virtiofs via external
virtiofsd (Linux guests), SMB at the segment gateway as universal fallback, agent
fileops + workspace syncer (vmlab `docs/vmlab-prd.md:500-565, 1513-1700`).

## Findings

**The premise moved: the VioFS corruption bug is fixed upstream (facts).**
- PR #1633 **merged 2026-10-26**, shipped in virtio-win tag **mm325** —
  "viofs: Fix zero-copy read IOCTL probe-and-lock in wrong process context"
  (verified on the release page:
  https://github.com/virtio-win/kvm-guest-drivers-windows/releases/tag/mm325).
  Issue #1659 is still open (last activity 2026-10-12), awaiting a numbered
  stable build; competing fix PR #1669, same approach, also open.
- Root cause: the zero-copy read path added in mm318/build 0.1.298 (2026-03-16)
  probe-and-locked the caller's buffer at DISPATCH_LEVEL from the completion DPC
  in an arbitrary process → STATUS_IN_PAGE_ERROR. Fix: probe/lock in the
  requestor's context (`EvtIoInCallerContext`). Fix evidence: 0 failed reads vs
  33-in-8-min stock; 233,807 byte-verified reads under Driver Verifier
  (https://github.com/virtio-win/kvm-guest-drivers-windows/pull/1633).
- The PRD's lineage is wrong and needs correcting: repo issue #137 is a 2017
  SMBus null-driver PR; no "VioFS 0.1.285" exists in the tag↔build map
  (attested builds: 0.1.262/266/296–302). Older large-copy bugs were #527
  (closed 2022; `queue-size=1024` workaround) and #764 (closed 2023).
  https://github.com/virtio-win/kvm-guest-drivers-windows/wiki/Builds-and-tags-mapping
- mm323 (2026-09-10) added optional polling change notification (`-n`, 3 s
  default, open handles only, off by default) — matches the PRD's reading
  (`docs/vmlab-prd.md:1646-1648`). mm325 also disables viofs PersistentAcls
  (Recycle Bin corruption, PR #1661).
- VioFS is still "Tech Preview": case-insensitivity under development (bug
  #1620), no alternate data streams, partial security descriptors. No
  path-length cap or hard-link gap is documented in the official wiki — the
  PRD's "~1023-char cap" is unverified.
  https://github.com/virtio-win/kvm-guest-drivers-windows/wiki/Virtiofs:-Shared-file-system

**Why Windows is hard: guest-side shape + signing (facts).**
- Three shapes: kernel FSD (fastest, hardest); RDBSS kernel mini-redirector
  (network-provider shape, still kernel); user-mode FS on WinFsp/Dokany, where
  a pre-signed FSD exists and our part is an ordinary user process. WinFsp is
  what VioFS, sshfs-win, JuiceFS and rclone all use
  (https://github.com/winfsp/winfsp#readme).
- Kernel drivers must be Microsoft-signed: attestation signing needs an EV
  certificate (~US$350–700/yr) + Hardware Developer Program; cross-signing is
  dead; **Server 2016+ and Win7/8 require full HLK/WHQL**, attestation covers
  Win10/11 desktop only
  (https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings).
  Test-signing mode = desktop watermark, Secure Boot must be disabled, HVCI and
  anti-cheat breakage (BattlEye/Vanguard) — dev-only, never shippable
  (https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option).
- Old guests: XP/2003 accept any signing cert imported into Trusted Publishers
  (32-bit XP has no kernel enforcement); Win7 new drivers are WHCP-only and
  SHA-2 validation needs KB4474419.
- WinFsp: kernel FSD arrives Microsoft-attestation-signed in the official MSI;
  a third-party **user-mode client carries zero signing burden**. License:
  GPLv3 + FLOSS exception (MIT glue distributing the unmodified installer
  qualifies) or commercial US$6,000/3yr per team
  (https://winfsp.dev/com/, https://github.com/winfsp/winfsp/blob/master/License.txt).

**How others do it (facts; signing details "not found" where closed).**
- VMware HGFS: closed kernel driver in Tools; HGFS RPC over backdoor/vsock
  (`hgfsProto.h` in open-vm-tools). Reads 500–900+ MB/s (2014 iozone);
  user-mode FUSE client write cliff 24.7 MB/s @4k (open-vm-tools #246).
- VirtualBox VBoxSF: HGCM host service + Windows "pseudo-network redirector"
  (manual §4.3) + vboxsf kernel FS on Linux; reads <100 MB/s (2014).
- Hyper-V/WSL2: `\\wsl.localhost` = 9P server in guest init, Windows client
  p9rdr.sys/p9np.dll over hvsocket (microsoft/WSL#12934). /mnt/c default is
  still 9P at msize=64 KiB (~40 MB/s; 4× build tax vs native ext4,
  https://lorbic.com/wsl2-performance-tax-go-windows/); `virtiofs=true` remains
  opt-in experimental, default false (Learn wsl-config, 2026-10-16), non-DAX.
  Copy-VMFile: one-way push over the Guest Service Interface (off by default).
- Parallels: GPL prl_fs kernel module (Linux) + closed Windows driver; prl_fs
  was **removed** in Desktop 20.2.0 (Jan 2025), breaking Vagrant users — a
  cautionary tale for guest-driver dependence.
- Windows Sandbox: vSMB over VMBus — SMB2 served by a user-mode server in the
  host worker, wcifs.sys in guest, large transfers via RDMA-style VMBus file
  mapping; all in-box, nothing to sign (Check Point reverse-engineering,
  https://research.checkpoint.com/2021/playing-in-the-windows-sandbox/). The
  guest pieces are Microsoft-internal: not reproducible by us.
- Minor: spice-webdav (WebDAV redirector, 50 MB WebClient cap), spice-vdagent
  xfer (push-only, 2 KiB chunks), Mile.Cirno (9P over hvsocket on Dokany),
  jlagedo/hyperv-virtiofs (2026: OpenVMM virtio-fs device into HCS guests).
  Firecracker ships no virtio-fs at all (virtio-blk only).

**Numbers that matter (measured, with sources).**
- Windows guest, same host/NVMe, 2 GB robocopy (2023): VioFS+WinFsp 105 MB/s vs
  Samba over bridged virtio-net 955 MB/s (~9×); Linux guest virtiofs ~680 MB/s.
  The Windows **user-mode FS stack** is the bottleneck, not virtio-fs.
  https://www.heiko-sieger.info/sharing-files-between-the-linux-host-and-a-windows-vm-using-virtiofs/
- virtio-fs vs 9P on Linux (Goyal, fio, 2020): 6–9× on buffered/mmap with DAX
  or guest cache; parity-to-worse on O_DIRECT psync and randwrite.
  https://lists.gnu.org/archive/html/qemu-devel/2020-12/msg02704.html
- virtio-fs DAX is unreachable upstream today: mainline QEMU has no cache-size
  property; Rust virtiofsd 1.14 answers FUSE_SETUPMAPPING with ENOSYS; Cloud
  Hypervisor removed DAX and closed it "never going to move forward" (2026-10);
  the C virtiofsd left the QEMU tree in 8.0.
- SMB: ~110 MB/s per 1 Gbps; signing is required-by-default on Win11 24H2+ /
  Server 2025 with AES-128-GMAC acceleration since Win11/Server 2022;
  multichannel needs RSS/multi-NIC — **no gain on a single non-RSS virtio-net**;
  single-threaded small-file copies can fall under 1 MB/s on per-create
  latency (https://learn.microsoft.com/en-us/troubleshoot/windows-server/networking/slow-smb-file-transfer).
- virtiofsd migration: ≥1.11.0 + vhost-user device-state (QEMU ≥8.2);
  `--migration-mode find-paths|file-handles`, `--migration-verify-handles`;
  file data never rides the stream — exactly what vmlab already validates on
  (`docs/vmlab-prd.md:514`). https://gitlab.com/virtio-fs/virtiofsd/-/blob/main/doc/migration.md
- Device-side features upstream lacks, that our own device could ship first:
  synchronous queues (libkrun PR #933, Oct 2026: +32% 4K randrw, −36% unlink
  latency, −28% kernel-build time) and FUSE multi-queue (Linux 6.10, up to 5×;
  virtiofsd does not implement it). https://sinrega.org/2026-10-16-synchronous-queues-virtio-fs/

## Options

**(a) Track upstream VioFS (≥mm325), keep virtiofsd.**
- Perf: ~100 MB/s class into Windows (measured 105); ~9× behind SMB for large
  sequential; better than SMB on small files/metadata (estimate: no per-file
  network round-trip, no signing).
- Reliability: fix merged with strong test evidence; still Tech Preview; watch
  case-insensitivity #1620 and the numbered stable ≥mm325.
- Windows: Win10/11 (virtio-win driver, Red Hat-signed). Older Windows: not
  covered by VioFS (Win7/XP support not checked).
- Signing: none for us.
- Snapshots: unchanged — virtiofsd state rides the stream (PRD:514).
- Effort: small — byte-verified robocopy/verify loops under concurrency as a
  gate, driver pinning in templates. Basis: the fix is merged; work is
  validation + rollout policy.
- Linux: unchanged; stays the fast path.

**(b) Own guest-side client.**
- b1 kernel FSD / RDBSS mini-redirector: EV cert + attestation, **HLK for
  Server guests**, zero maintained samples (nulmrx/smbmrx dead ~20 yrs), C-only,
  no OSS ecosystem (OSR: "no active projects use rdbss"). Effort: person-years
  (basis: RDBSS doc scope + no third-party existence proof in two decades).
  Not credible now.
- b2 own user-mode FS on WinFsp over our own virtio device: zero signing;
  GPLv3-FLOSS exception covers MIT glue + unmodified installer (commercial
  $6k/3yr fallback). Perf ceiling stays VioFS-class — the WinFsp user-mode path
  is the measured bottleneck; what it buys is protocol control (real
  notifications, sync queues, our semantics), not throughput. Windows: Win7–11
  (WinFsp supports 7+). Snapshots: our device, our state — clean by design.
  Effort: months (basis: sshfs-win/JuiceFS scale, new protocol + FUSE glue).
  Linux: needs a second client or Linux stays on virtiofs — two protocols.
- b3 keep the **standard virtio-fs protocol** (VIRTIO 1.2 §5.11) but serve it
  from our own in-process device on rust-vmm crates (crosvm's BSD `fuse` crate
  is liftable under D7); guest side stays stock VioFS + Linux virtiofs. Zero
  new guest code; aligns with D5 (one process per machine); device state moves
  natively into our snapshot format (Q8); we can ship sync queues/multi-queue
  before upstream. Effort: moderate–large (FUSE-over-virtqueue server +
  migration-state port). Basis: virtiofsd is exactly this on the same crates.

**(c) Better SMB.**
- Perf (fact): 955 MB/s measured over bridged Samba; expect the same class from
  a competent server on the segment (estimate); embedded Rust server has no
  published numbers — not checked. Small-file trees stay per-create-latency
  bound. Multichannel: no gain on single non-RSS virtio-net (fact); multiple
  vNICs per VM would enable it (not designed).
- Reliability: highest — in-box client, mature protocol, stale-TCP self-heal
  after restore (PRD:522).
- Windows: everything, XP/2003 via NT1 to Win11 (PRD:521).
- Signing: none.
- Snapshots: zero device state — the best of all options.
- Effort: already scoped as "the largest single engineering component"
  (PRD:549); add AES-128-GMAC signing to the embedded server's scope (guests
  require signing by default now).
- Linux: kernel CIFS is ubiquitous; slightly worse than virtiofs for metadata.

**(d) Agent-based sync (workspace syncer, ADR-0014).** Complementary, not a
competitor: watched source trees need it under every passthrough transport
(VioFS polls 3 s/open-handles-only; Samba's watch is non-recursive; PRD
§19.6). Perf: guest-native disk after convergence, full transfer on first
sync. Snapshots: bracketing already designed. Effort: already in the PRD. No
new finding changes it.

**(e) virtio-fs DAX / shared-memory window.** Dead upstream everywhere that
matters (facts above); no Windows DAX client exists at all. Doing it ourselves
(crosvm Mapper + vhost-user SHMEM_MAP/UNMAP, Linux 5.10+ guests) yields 4–7.5×
on buffered/mmap Linux workloads but: window must fit the working set or it is
4× worse than off; ~16 MB/GB guest RAM overhead; DAX+migration untested
anywhere. Linux-only, high effort, snapshot risk — park unless Linux small-IO
becomes a stated target.

**(f) Others.** 9P on Windows: no in-box client; third-party is Dokany-based
(kernel driver, niche) — no. WebDAV: 50 MB WebClient cap, service glue — no.
NFS: Windows client only in Pro/Enterprise editions — not checked further.
vSMB-style paravirt SMB: needs Microsoft-internal guest pieces — the
reproducible version is our embedded SMB server on the segment (option c).

## Conclusion (ranked; 1–2 are estimates on fact, 3–5 are estimates)
1. **(a)+(c): re-validate VioFS on ≥mm325 and re-enable it for Windows
   profiles** (small-file/metadata workloads) while keeping SMB as the default
   big-file path and universal fallback — then invest in the SMB server. Zero
   signing, snapshots already work, both pieces are already built or scoped.
2. **(b3): own host-side virtio-fs device speaking the standard protocol**
   (medium-term): the real "we own the hypervisor" payoff without touching
   guest drivers — kills the external virtiofsd process and lets sync
   queues/multi-queue land first.
3. **(d) syncer**: unchanged, already decided; this research confirms rather
   than challenges it.
4. **(e) DAX**: park. 5. **(b1) own kernel driver: no; (b2) own WinFsp
   client: only if we need semantics VioFS won't take upstream** (true
   notifications), accepting no throughput gain.

## Consequences
- PRD §7.5's premise is stale: "#1633 unmerged" and the #137/0.1.285 lineage
  are wrong (fix merged 2026-10-26; real lineage mm318/0.1.298 → #1659/#1633;
  old bugs #527/#764). The Windows-profile virtiofs ban should be re-decided
  on mm325+ validation evidence.
- mm325 is a pre-release tag; no numbered stable ships the fix yet. Templates
  can't surface their driver version (PRD:516), so a driver-version gate needs
  a mechanism (mount-time check or template-build pinning).
- Q25 must choose external virtiofsd vs in-process device (b3); it interacts
  with Q8 (device state must serialize into our own snapshot format; port the
  find-paths/file-handles semantics).
- Embedded SMB server scope: add AES-128-GMAC signing (default-required on
  Win11 24H2+/Server 2025 guests); if bundling smbd, verify its NT1 support.
- Record the WinFsp license posture (FLOSS exception vs $6k/3yr) if (b2) is
  ever picked; VioFS users already depend on it implicitly.
- Not checked: VioFS on pre-Win10 guests; virtio-net RSS exposure; embedded
  Rust SMB server performance (no data exists anywhere); NFS client editions.
- Watch: #1659 close + numbered stable ≥mm325; PR #1669; VioFS
  case-insensitivity #1620; WSL2 flipping virtiofs to default (host-side
  precedent for Windows-virtiofs viability).

DONE /home/wil/dev/vmlab-hypervisor/.scratch/plans/vmlab-hypervisor/notes/14-windows-file-sharing.md
