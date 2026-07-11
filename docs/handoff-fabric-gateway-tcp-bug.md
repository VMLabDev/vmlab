# Handoff: fabric gateway TCP stalls + silently loses bulk guest→host writes

Status: **FIXED.** Found 2026-07-12 while benchmarking the SMB share
path during the virtiofs transport work (commits 4be70a5 / 4204b5f / eb5d719).
virtiofs is now the default share/volume transport, so day-to-day impact is
limited to the SMB fallback tier — but SMB remains the only transport for
vintage guests (XP/2003/DOS via smb1) and for hosts without virtiofsd, and
**it can silently lose data**, so this should be fixed, not just documented.

## Resolution

The gateway handed each frame to NAT in a separately spawned Tokio task.
Consecutive TCP segments could therefore overtake one another before reaching
the deliberately in-order vTCP receiver; bounded `try_send` queues compounded
the problem by silently dropping frames under pressure. The fix awaits the NAT
uplink from the gateway's single ordered task and applies bounded backpressure
in both directions. Regression coverage now sends 10 MiB through passive vTCP
and verifies the exact bytes received by a loopback sink.

The side findings were addressed at the same time: smbd startup clears a
lab-local pidfile only when its PID is confirmed dead, and the Ubuntu 24.04
template's autoinstall netplan matches `en*` rather than a PCI-derived name.

## Symptom

Bulk data written by a guest through a lab SMB share:

- trickles at **~200 kB/s** (reads in the other direction do 204 MB/s), and
- **silently lands as a 0-byte file on the host** — `dd conv=fsync` inside the
  guest reports success, the file exists host-side with size 0.

Small writes (a few KB, e.g. `echo hello > /mnt/share/t.txt`) land correctly.
That asymmetry is why the share/volume machinery has always *appeared* to
work: provisioning drops small files; nobody had pushed bulk data up before.

## Repro (10 minutes)

```sh
mkdir -p /tmp/smb-bench/shared && cd /tmp/smb-bench
cat > vmlab.wcl <<'EOF'
import <vmlab.wcl>

lab "smb-bench" {
  vm "box" {
    template = "x86_64/ubuntu-24.04"
    cpus   = 2
    memory = 2GiB
    nic { nat = true }
    share { host = "./shared" guest = "/mnt/share" transport = "smb" }
  }
}
EOF
# isolated daemon namespace so the main supervisor is untouched:
export XDG_RUNTIME_DIR=/tmp/smb-bench/run XDG_STATE_HOME=/tmp/smb-bench/state
vmlab up   # wait for ready
```

Two template quirks get in the way of the repro (see "Side findings"):
the ubuntu-24.04 template's netplan pins `enp0s4` but the NIC enumerates as
`enp0s3` in this minimal lab, so fix DHCP first:

```sh
vmlab exec box -- sh -c 'printf "network:\n  version: 2\n  ethernets:\n    all:\n      match: {name: \"en*\"}\n      dhcp4: true\n" > /etc/netplan/99-fix.yaml && chmod 600 /etc/netplan/99-fix.yaml && netplan apply'
# if the ready-time auto-mount window already expired, mount manually:
P=$(cut -d: -f2 .vmlab/smb/creds)
vmlab exec box -- sh -c "mkdir -p /mnt/share && mount -t cifs //10.213.0.1/mnt_share /mnt/share -o username=wil,password=$P,vers=3.0"
```

Then:

```sh
vmlab exec box -- sh -c 'dd if=/dev/urandom of=/mnt/share/t.bin bs=1M count=10 conv=fsync 2>&1 | tail -1'
stat -c %s shared/t.bin   # ← 0 bytes, despite dd reporting ~200 kB/s "success"
vmlab exec box -- sh -c 'dmesg | grep -i cifs | tail'
```

Guest dmesg during the transfer:

```
CIFS: VFS: \\10.213.0.1 sends on sock ... stuck for 15 seconds
CIFS: VFS: \\10.213.0.1 Send error in SessSetup = -11
CIFS: VFS: \\10.213.0.1 disabling echoes and oplocks
CIFS: VFS: No writable handle in writepages rc=-9
```

Control (read direction is healthy):

```sh
head -c 10485760 /dev/urandom > shared/r.bin
vmlab exec box -- sh -c 'dd if=/mnt/share/r.bin of=/dev/null bs=1M; md5sum /mnt/share/r.bin'
# 204 MB/s, md5 matches the host file
```

## What was ruled out

Each of these was tested explicitly on 2026-07-12:

- **Debug build slowness** — reproduced identically with a `--release` daemon.
- **MTU / jumbo frames** — reproduced at segment MTU 9000 (default on nat)
  AND with an explicit `segment { mtu = 1500 }`.
- **The eBPF fastpath tiers** — reproduced on the userspace stream-socket
  attachment and on the afxdp tap attachment. Expected: gateway-addressed
  traffic never rides the fastpath — §9.1.1 punts it to the daemon on every
  tier, so the failing code is tier-independent.
- **smbd itself** — smbd is a stock Samba on loopback; the loopback leg is
  ordinary kernel TCP. (Not 100% exonerated, but the "sends on sock stuck"
  error is the guest failing to push bytes toward the *gateway*, i.e. before
  smbd ever sees them.)

## Reading of the evidence

The failing direction is guest→daemon bulk TCP. "Sends on sock stuck" means
the guest kernel's socket send buffer is full and nothing is draining it —
the gateway side has stopped ACKing / reading. Under that stall the CIFS
session poisons (`SessSetup = -11`), the client drops the writable handle,
and page-cache writeback is discarded (`No writable handle in writepages`)
— hence 0-byte files with a "successful" dd (soft mount semantics eat the
error; the fsync had already been acknowledged against the poisoned session).

The ~200 kB/s that does trickle through suggests retransmissions of small
segments succeed while large in-flight windows starve, i.e. a receive-window
/ backpressure / reassembly problem in the userspace TCP the gateway
services speak, or in the redirect splice between that TCP endpoint and the
loopback connection to smbd.

## Where to look

- `src/labd/lab.rs` `ensure_smb()` — installs the `RedirectRule` DNAT:
  segment `gateway:445` → `127.0.0.1:<smbd port>` on the segment's NAT
  services.
- `src/net/` — the userspace fabric: the gateway/service TCP implementation
  and the NAT/redirect engine that proxies the guest connection to loopback.
  The bug is almost certainly in the receive/drain half of that proxy path
  (guest→gateway), since gateway→guest is fine at 200+ MB/s.
- PRD §9.1.1 for the punt-path design (gateway MAC never enters kernel
  forwarding tables; all this traffic traverses the userspace switch).

A focused unit/integration test to write first: a guest-side TCP client
pushing ≥10 MiB through a RedirectRule to a local sink, asserting byte count
and stall-free progress — that should reproduce without QEMU in the loop.

## Impact

- VM shares on the SMB transport: bulk uploads corrupt (silently).
- Container volumes on the CIFS fallback (hosts without virtiofsd): same.
- Anything else that pushes bulk TCP *to a gateway service* through the NAT
  redirect machinery (port-forwards to loopback targets, if used that way).
- NOT affected: virtiofs shares/volumes (default now), host→guest transfers,
  guest↔guest traffic (that path is switch/fastpath, not gateway services).

## Side findings from the same session (small, separate fixes)

1. **Orphan smbd on destroy/force-down:** `vmlab destroy` / `down --force`
   can leave the lab's smbd running; its pidfile then blocks the next lab
   start ("smbd is already running", surfaced as `smb.failed` + "shares will
   not mount"). The stop path should reap smbd and/or the start path should
   clear a stale pidfile whose process is dead.
2. **ubuntu-24.04 template netplan pins `enp0s4`:** in a minimal lab (fewer
   PCI devices than the build-time VM) the NIC enumerates as `enp0s3`, so the
   clone never gets a DHCP lease and share mounts silently fail after the
   retry window. The template should match `en*` instead of a fixed name.
   (Template-side fix in vmlab-templates; check the other Linux templates.)

## Benchmarks for context (2026-07-12, same host)

| path                          | write        | read     |
| ----------------------------- | ------------ | -------- |
| SMB share (userspace fabric)  | ~0.2 MB/s + data loss | 204 MB/s |
| virtiofs share, Linux VM      | 950 MB/s     | 3.2 GB/s |
| virtiofs volume, container    | 1500 MB/s    | —        |
| virtiofs share, WS2025        | 522 MB/s     | 630 MB/s |
