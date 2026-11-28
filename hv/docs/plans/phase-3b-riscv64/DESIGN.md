---
slug: phase-3b-riscv64
kind: design
created: 2026-11-28
card: 1242
---

# Phase 3b of vmlab-hv: the riscv64 emulator and the `virt` machine

This document is the design of sub-phase 3b (GitHub #230). It replaces the
Sail-interpreting route of 3a as the product path for riscv64. `Dn` without a
prefix is a row of `../phase-3-tcg/MAP.md`. `HV-Dn` is a row of
`../vmlab-hypervisor/MAP.md`. `3d:` and `3e:` point into
`../phase-3d-aarch64/DESIGN.md` and `../phase-3e-aarch64-jit/DESIGN.md`. The
codes of this document (`P`, `L`, `X`, `E`, `M`, `G`, `V`, `F`, `O`, `U`, `H`,
`R`, `C`, `Q`) are stable.

Every number marked *measured* comes from helios (Ryzen 9 9950X, QEMU 11.1.3)
on 2026-11-28, with a host load of 5 to 15 from other cards. The tools are
in `measure/`. "How to measure again" is at the end. A number marked *est.*
is an estimate, with its basis.

riscv64 is the last product path that starts QEMU. Card C16 of 3d (#1051,
remove the QEMU hypervisor code) follows this phase.

## The decisions of the operator (2026-11-28)

- **P1.** The semantics are our own code in the typed IR of
  `vmlab-hv-tcg-ir`, as in 3d. The generated Sail decoder and the in-process
  Sail C model stay, as the oracle of every instruction. The condition: no
  source and no binary is relied on before its licence is checked and
  recorded (the table L below). QEMU stays a black box (X1).
- **P2.** The instruction set is RV64GC with Zba, Zbb and Zbs. Translation is
  Sv39 and Sv48. There is no V and no H.
- **P3.** The templates are accepted with the JIT on. The riscv64 forms of
  the JIT are part of 3b.
- **P4.** vmlab bundles a pinned EDK2 `RISCV_VIRT` and a pinned OpenSBI in
  its guest assets, as it bundles AAVMF, and the boot logo is the vmlab logo.
- **P5.** The refusal list is the one of aarch64 as 3d shipped it, and there
  is no QEMU fallback: a riscv64 VM that vmlab-hv refuses fails at `up` and
  names each device.

P1 replaces D5's "specialised Sail IR drives the interpreter" for the product
path of riscv64, and keeps D6 (the Sail C model per instruction, QEMU per
boot). P2 fixes the list that D7 left open. P3 moves the riscv64 JIT forms
from 3c into 3b. Phase 3c keeps only speed: the rows against QEMU, without a
gate (3e Q4). It is named here and not designed.

## Sources and licences

P1 asks for this table before any card relies on a source. Each licence was
read in the file named, on 2026-11-28.

| Code | Source | Licence, and where it was read | Use |
|---|---|---|---|
| L1 | sail-riscv at `fc074f99` (`toolchain/sail/pins.env`) and Sail 0.20.3 | BSD-2-Clause. `crates/vmlab-hv-tcg-riscv64/LICENSE.sail-riscv` and `LICENSE.sail`, and the entry in `THIRD_PARTY_NOTICES`. | Read as the specification. Its compiled C model is the oracle (O1). A semantics file that follows a model function step by step is a derivative: the new crate gets an entry in `THIRD_PARTY_NOTICES` with the same notice, once, in card C1. |
| L2 | The RISC-V Instruction Set Manual, volumes I and II (`riscv/riscv-isa-manual`) | CC-BY-4.0 (`LICENSE` of the repository) | Read. Unlike the Arm manual (3d:X2) it may be read whole, with its tables. A table that we copy into code (the opcode map) names the manual in the file header, which is the attribution that the licence asks for. |
| L3 | The SBI specification (`riscv-non-isa/riscv-sbi-doc`) and the PLIC specification (`riscv/riscv-plic-spec`) | CC-BY-4.0 (`LICENSE` of each repository) | Read, for the PLIC model and to know what OpenSBI serves. |
| L4 | OpenSBI (`riscv-software-src/opensbi`), with the libfdt inside it | BSD-2-Clause (`COPYING.BSD`). libfdt is offered under GPL-2.0-or-later or BSD-2-Clause, and OpenSBI takes it under BSD-2-Clause (`ThirdPartyNotices.md`). | Shipped as a binary (F2), with its notice beside it. Its documentation of `fw_dynamic` is read for the reset stub (V3). |
| L5 | EDK2 (`tianocore/edk2`), the `RiscVVirt` platform | BSD-2-Clause-Patent (`License.txt`). The binary also holds the submodules that Debian's `copyright` file lists (OpenSSL under Apache-2.0 among them). | Shipped as a binary (F1). The `copyright` file of the Debian source package is shipped beside it as `COPYRIGHT`, as for OVMF and AAVMF. |
| L6 | riscv-tests at `3df703cd` | BSD-3-Clause (`LICENSE`) | Built at test time, never committed. 3a runs it already. |
| L7 | riscv-arch-test at `eb66181d` | BSD-3-Clause or Apache-2.0 per file, documents CC-BY-4.0 (its README, `COPYING.BSD`, `COPYING.APACHE`) | Built at test time, never committed. 3a runs it already. |
| L8 | Spike (`riscv-software-src/riscv-isa-sim`) | BSD-3-Clause (`LICENSE`) | A second readable text for a behaviour that L1 and L2 leave open. Not an oracle. The card that reads a file names it. |
| L9 | Berkeley SoftFloat 3e | BSD-3-Clause, vendored, in `THIRD_PARTY_NOTICES` | The `RISCV` specialisation that `vmlab-hv-tcg` builds today. |
| L10 | The behaviour of a running QEMU 11.1.3: its device tree (`dumpdtb`), its register answers over qtest and the gdbstub, its plugin stream, its logs | not a source text | The reference for the machine (V) and the leader of the boot lockstep (O3). |
| L11 | Documents of devices: the TL16C550C data sheet, the Devicetree Specification, the binding documents of the Linux tree, the CFI flash documents | documents | Device models and the device tree, as `hv/AGENTS.md` says. |
| L12 | The documentation of the goldfish RTC and of the SiFive test device | not checked | The card that models them (C6) checks the licence of the text that it reads. Without a text, the registers are pinned by probes on QEMU (L10), which is how 3d pinned the PL031. |

Not allowed for the CPU core:

- **X1.** QEMU source under `target/riscv`, `fpu`, `tcg` and `accel/tcg`.
  Not read, not copied, not translated. The device rule of `hv/AGENTS.md`
  stays as it is for device models.
- **X2.** Unicorn and any other emulator under the GPL (3d:X3).

There is no row like 3d:X2: every specification of this architecture is
under a licence that allows reading and attribution. Each semantics file
names in its header the sources that its author read. Each card report lists
them. Card C1 adds X1 and X2 for riscv64 to `hv/AGENTS.md`.

**What the licence check found.** Nothing blocks the design. Two points need
care. The patent grant of EDK2 covers EDK2 only, and OpenSBI and sail-riscv
have none, which is the same position as SoftFloat and gem5 in 3d. And the
firmware binaries carry more than one licence, so the bundle ships the
notices as files (F3) and does not fold them into vmlab's MIT text.

## 1. The state today

### E1. How vmlab runs a riscv64 VM

- `src/qemu/resolve.rs` gives every riscv64 VM the machine `virt,acpi=off`.
  `src/qemu/cmdline.rs` composes the argv: TCG, `-cpu max`, the two flash
  banks, `virtio-blk-pci` disks and CD-ROMs, `virtio-net-pci`, the agent on
  `virtio-serial-pci`, `virtio-gpu-pci`, `qemu-xhci` with `usb-tablet`, and
  the serial log on the 16550.
- `src/config/hypervisor.rs` falls back to QEMU for riscv64 automatically,
  with a reason in `status`. `emulated_guest` is true for aarch64 only.
- The firmware is not bundled. `src/qemu/firmware.rs` finds
  `RISCV_VIRT_CODE.fd` and `RISCV_VIRT_VARS.fd` under the host's
  `/usr/share/qemu-efi-riscv64`, `/usr/share/edk2/riscv64` or
  `/usr/share/edk2/riscv`. `guest/fetch-firmware.sh` knows x86_64 and
  aarch64 only. OpenSBI is not vmlab's at all: QEMU loads its own
  `opensbi-riscv64-generic-fw_dynamic.bin` (measured: OpenSBI v1.8.1, 275,928
  bytes, from the package `qemu-system-riscv-firmware`).
- Templates in the local store: `debian-13` (13.20261001.0), `fedora-42`
  (42.20250911.3) and `ubuntu-24.04` (24.04.20260520). The agent is the musl
  build `linux-riscv64` of `guest/build-agent.sh`.
- There is no riscv64 container micro-VM: `guest/build-asset.sh` builds the
  kernel and the initramfs for x86_64 and aarch64 only. Containers are out of
  3b.
- e2e `arch.emulated` boots an aarch64 guest only.

### E2. What 3a built

- The pinned Sail toolchain in a container (`toolchain/sail`): the Jib IR of
  sail-riscv, and the same model compiled to C++ behind a C interface
  (`oracle.h`: step, get and set of the PC, the registers and the CSRs, memory
  through callbacks of the host, a clock that only the harness advances).
- `vmlab-hv-tcg-riscv64`: the generated decoder accessors and the compressed
  IR (361 instruction tags, 1,986 model functions, 214 registers).
- `vmlab-hv-tcg`: the interpreter of that IR, with `Memory`, `FlatMemory`,
  `SharedMemory`, the reservation pair and the code stamps. 134 of 134
  riscv-tests pass on flat images. It runs CoreMark at 51 MIPS and a
  three-instruction loop at 1,594 MIPS (`crates/vmlab-hv-tcg/PERF.md`). QEMU
  runs the same images at 4,082 and 6,346 MIPS
  (`bench/history/riscv64.jsonl`).
- `vmlab-hv-lockstep`: the step-and-compare runner against the Sail C model,
  133 programs and 450,726 instructions without a difference
  (`crates/vmlab-hv-tcg-riscv64/VERIFICATION.md`). Its random driver makes
  RV64I only.
- `bench/`: the flat CoreMark and the loop for RV64GC (`bench/rv64`), and the
  QEMU baseline of Debian 13.

The 51 MIPS are the reason for P1. A boot is 24 to 41 billion instructions
(M2), so the interpreter of the model needs 8 to 13 minutes of pure
execution for one boot, and the model stays inside every block that a JIT
would compile (3e:Y4).

### E3. What 3d and 3e built that 3b uses

| Code | Part | Where | For riscv64 |
|---|---|---|---|
| E3a | The typed IR: the graph, `Select`, the helper call with declared effects, the register file as slots, `Block<X>` with the exit as a parameter (3d:R8 to R11) | `vmlab-hv-tcg-ir::native` | Used as it is. The riscv64 lifters write this graph. |
| E3b | The pre-decoded block form and its interpreter loop (3d:M6, 3e:H15) | `vmlab-hv-tcg-ir::block` | Used as it is. It knows no guest. |
| E3c | The softmmu TLB: 4 KiB pages, an address space identifier per entry, the span of the mapping, the host page (3d:R12) | `vmlab-hv-tcg::tlb` | Used as it is. The ASID of `satp` is the identifier. The fill and the flush are ours (G6). |
| E3d | The JIT: assembler, code region in a double mapping, emitter, chaining, jump cache, fast TLB, perf map, the kick through the budget (3e:H1 to H15) | `vmlab-hv-tcg-jit` | Used as it is. What riscv64 adds is in G8. |
| E3e | The vCPU backend seam: one loop over `VcpuBackend`, with KVM and the emulator behind it (3d:F1) | `vmlab-hv-vmm/src/vcpu.rs` | Used as it is. `vcpu/emulated.rs` holds the aarch64 core by name: it gets a second core (C6). |
| E3f | Guest memory for an emulated CPU, with MMIO on the bus and stamped device writes (3d:F2) | `vcpu/guest_bus.rs` | Used as it is. |
| E3g | pflash, fw_cfg over MMIO, the FDT writer, the PCI host with its ECAM, the virtio functions, the 2D virtio-gpu and the xHCI of #1225 | `vmlab-hv-devices`, `vmlab-hv-vmm/src/virt/fdt.rs`, `pci.rs` | Used as they are. The 16550 exists for x86 and gets an MMIO mapping. |
| E3h | CPU and machine state of an emulated machine in a snapshot, reset in place, the caches rebuilt after a restore (3d:F6, D13) | `virt/snapshot.rs`, `snapshot.rs` | The path is used. The state of the riscv64 CPU and of its devices is new (C9). |
| E3i | The proof tools: the block differential (3e:K2), the replay check on a boot (3e:K3), the window boot lockstep (3d:O4w), the qtest oracle of device registers | `vmlab-hv-tcg-aarch64/src/oracle`, `vmlab-hv-devices/src/virt_oracle.rs` | The methods are used. The code is aarch64 by name and is written again for riscv64, smaller (O). |
| E3j | The bench: history, `report.py`, `check.py`, the flat backends, the boot MIPS row | `bench/` | Used. It gets a riscv64 flat backend for the new core (C5). |

Three things are not shared today and matter for the sizes:

- **E3k.** The run loop of aarch64 is `vmlab-hv-tcg-aarch64/src/exec.rs`,
  8,147 lines: the block cache, the translator, the loads and stores over
  the TLB, the entry into compiled code, chaining, the replay check. Most of
  it knows `A64Exit`, the flag maps and the exception model. riscv64 cannot
  call it (R1).
- **E3l.** The lockstep state of `vmlab-hv-lockstep` is riscv-shaped and
  fits. Its random driver is not enough.
- **E3m.** `VcpuBackend`'s `Hypercall` exit carries a PSCI conduit. riscv64
  has no hypercall to the machine: SBI is served by OpenSBI in the guest.

## 2. Measurements

### M1. The machine that QEMU presents (measured)

`-machine virt,acpi=off -machine dumpdtb=` with 2 vCPUs and 2 GiB, read with
`dtc`:

| Device | Address | Size | Interrupt | Compatible |
|---|---|---|---|---|
| Test device (power-off, reboot) | `0x100000` | `0x1000` | none | `sifive,test1`, `sifive,test0`, `syscon` |
| RTC | `0x101000` | `0x1000` | PLIC 11 | `google,goldfish-rtc` |
| CLINT | `0x2000000` | `0x10000` | to each hart: 3 (software) and 7 (timer), machine level | `sifive,clint0`, `riscv,clint0` |
| PCI I/O window | `0x3000000` | `0x10000` | | |
| Platform bus (empty) | `0x4000000` | `0x2000000` | | `qemu,platform`, `simple-bus` |
| PLIC | `0xc000000` | `0x600000` | to each hart: 11 (machine external) and 9 (supervisor external). 95 sources (`riscv,ndev`) | `sifive,plic-1.0.0`, `riscv,plic0` |
| UART | `0x10000000` | `0x100` | PLIC 10 | `ns16550a` |
| Eight virtio-mmio slots (no device behind them) | `0x10001000` to `0x10008000` | `0x1000` each | PLIC 1 to 8 | `virtio,mmio` |
| fw_cfg | `0x10100000` | `0x18` | none | `qemu,fw-cfg-mmio` |
| Flash, two banks | `0x20000000` and `0x22000000` | 32 MiB each | none | `cfi-flash` |
| PCI ECAM | `0x30000000` | 256 MiB | INTx on PLIC 32 to 35 | `pci-host-ecam-generic` |
| PCI memory, 32 bits | `0x40000000` | 1 GiB | | |
| RAM | `0x80000000` | as configured | | |
| PCI memory, 64 bits | `0x400000000` | 16 GiB | | |

- The time base is 10 MHz (`timebase-frequency`).
- There is no MSI controller. Every PCI function interrupts through one of
  four INTx lines, shared.
- With `-cpu max` the ISA string names 129 extensions, with V, H and the
  vector crypto sets, and the MMU type is `riscv,sv57`. With `-cpu rv64` it
  is shorter, still with H and Sv57.
- The tree has a `riscv,pmu` node and the nodes `syscon-poweroff` and
  `syscon-reboot`.

The firmware path, from the banner that OpenSBI prints on the serial line of
a Debian boot (measured):

- OpenSBI v1.8.1 runs at `0x80000000` in M-mode: 321 KiB of firmware, SBI
  3.0, the extensions `time, rfnc, ipi, base, hsm, srst, pmu, dbcn, fwft,
  legacy, sse`.
- Its devices: `aclint-mswi` for IPIs, `aclint-mtimer @ 10000000Hz` for the
  timer, `uart8250` for the console, `syscon-reboot` and `syscon-poweroff`.
- It protects itself with PMP: 16 entries, a granularity of 2 bits, 54
  address bits. `MIDELEG` is `0x222` and `MEDELEG` is `0x4b109`.
- It leaves to `0x20000000` in S-mode: EDK2 runs from the first flash bank.
  The device tree is at `0xffe00000` (2 GiB of RAM), passed in `a1`.

### M2. The templates boot on the instruction set of P2 (measured)

QEMU cannot be asked for "RV64GC with Zba, Zbb and Zbs" by name. The CPU of
the measurement is `rv64` with everything else turned off:

```
-cpu rv64,h=off,zbc=off,zfa=off,zawrs=off,zicbom=off,zicbop=off,zicboz=off,sstc=off,svadu=off,svvptc=off,zihintntl=off,sv57=off,sdtrig=off
```

Its device tree says `rv64imafdc_zic64b_ziccamoa_ziccif_zicclsm_ziccrse_zicntr_zicsr_zifencei_zihintpause_zihpm_zmmul_za64rs_zaamo_zalrsc_zca_zcd_zba_zbb_zbs`
with twelve `sh*` and `ss*` names that describe the privileged architecture
and add no instruction, and `mmu-type = "riscv,sv48"`.

All three templates reach agent ready on it, at one vCPU, with vmlab's argv
otherwise (`bench/backends.py`) and the counting plugin:

| Template | Kernel | Instructions to agent ready | 16-bit instructions | Distinct encodings | Interrupts | Exceptions |
|---|---|---|---|---|---|---|
| debian-13 | 6.12.111 | 23.85e9 | 43.5 % | 258,385 | 17,678 | 432,126 |
| fedora-42 | 6.16.4 | 35.40e9 | 45.6 % | 292,890 | 117,670 | 869,330 |
| ubuntu-24.04 | not read | 40.51e9 | 46.7 % | 317,069 | 31,261 | 869,714 |

OpenSBI and EDK2 are inside each count.

| Group | debian-13 | fedora-42 | ubuntu-24.04 |
|---|---|---|---|
| M (multiply, divide) | 0.835 % | 0.538 % | 0.296 % |
| Zba, Zbb, Zbs | 0.289 % | 0.504 % | 0.363 % |
| SYSTEM (CSR accesses, `ecall`, `sret`, `mret`, `wfi`, `sfence.vma`) | 0.143 % | 0.141 % | 0.148 % |
| A (`lr`, `sc`, the AMOs) | 0.050 % | 0.073 % | 0.050 % |
| F and D | 0.002 % | 0.005 % | 0.017 % |
| V | 0 | 0 | 0 |

What follows from it:

- **M2a. P2 is enough.** No template needs V, H, Sstc, Zicboz or Sv57 to
  reach agent ready.
- **M2b. The mix is integer code.** More than 98 % of the executed
  instructions are integer operations, branches, loads and stores, and
  nearly half are 16 bits long. The union of the three boots is 173
  mnemonics of QEMU's disassembly. FP is under 0.02 %, so SoftFloat helper
  calls cost nothing that a boot can show.
- **M2c. The bit-manipulation sets are used**, by the kernels (`add.uw`,
  `clz`, `ctz`, `cpop`, `orc.b`, `rev8`, `rori`, `roriw`), where the device
  tree offers them.
- **M2d. M-mode is entered rarely.** `mret` runs 237,847 times in the three
  boots together, against 2,099,931 `sret` and 1,817,296 `ecall`. One trap
  return in ten is OpenSBI's.
- **M2e. The `time` CSR is read 39.2 million times** in the three boots. It
  must be served inside the core, and inline in the JIT (G7c).
- **M2f. `sfence.vma` runs 1.98 million times**, 600,000 per boot and more.
  The flush of the TLB must be cheap and by address where the instruction
  names one.
- **M2g. 104 CSR numbers are touched.** The list is in "CSRs the templates
  touch" below. 16 PMP address registers, 16 hardware counter events and 29
  hardware counters are among them: OpenSBI probes each one.

**Sstc.** The same Debian boot with `sstc=on`: 23.49e9 instructions (1.5 %
fewer), 9,560 interrupts in place of 17,678, 419,608 exceptions in place of
432,126, and agent ready after 34.4 s against 36.9 s, both with the plugin,
at a load of 9.5 against 14.9. The supervisor timer is not worth a line of P2 for a
boot. It stays out of 3b and is the first candidate of 3c.

### M3. The QEMU baseline (measured)

Boot to agent ready, QEMU 11.1.3 TCG, vmlab's argv, EDK2 202608 of the host
and QEMU's OpenSBI v1.8.1, a fresh overlay per run, pinned to cores 24 to 27,
one run each, no plugin:

| Template | CPU of M2, 1 vCPU (s) | CPU of M2, 2 vCPUs (s) | `-cpu max`, 1 vCPU (s) | `-cpu max`, 2 vCPUs (s) | QEMU average MIPS, CPU of M2, 1 vCPU |
|---|---|---|---|---|---|
| debian-13 | 24.4 | 18.2 | 49.2 | 33.3 | 978 |
| fedora-42 | 46.0 | 36.3 | 86.5 | 65.0 | 769 |
| ubuntu-24.04 | 44.4 | 33.8 | 68.7 | 49.4 | 913 |

The last column divides the instruction count of M2 by the boot time. Idle
time is inside it. The host load fell while these boots ran: the `-cpu max`
boots ran first, at a load of 6.4 to 9.7, and the boots on the CPU of M2
last, at 4.8 to 6.1. With the plugin, the boots of M2 took 36.9, 76.4 and
61.7 s.

- **M3a.** `-cpu max`, which vmlab passes today, took 1.5 to 2.0 times as
  long as the CPU of P2 on every template. The difference in load does not
  explain a factor of that size, but the two were not measured apart. If
  the factor holds, a guest on the CPU of P2 does less work per boot than
  today's guest, and the yardstick for vmlab-hv is the faster column.
- **M3b.** These are single runs under the load of other cards. Card C5
  records the baseline with three repeats in `bench/history/riscv64.jsonl`,
  where the row of Debian from 2026-10-31 is 52.36 s at one vCPU and 29.85 s
  at four.

### M4. What the core must reach

- vmlab waits 600 s for a VM to become ready (`VM_READY_TIMEOUT`,
  `src/labd/vm.rs`). The largest boot is 40.5 billion instructions (Ubuntu).
  The floor is 68 MIPS on the boot mix, at one vCPU and without idle time.
  With a margin of two it is 135 MIPS.
- The aarch64 interpreter of 3d ran its boots at 54 MIPS, and the JIT of 3e
  at 279 MIPS after its first eight cards (3e, Summary). So the interpreter
  alone does not hold the floor for Ubuntu and Fedora, and P3 follows from
  the numbers: the templates are accepted with the JIT on.
- *Est.*: 250 to 450 MIPS on a boot with the JIT, the range that 3e
  measured for aarch64 with the same emitter. The basis for "not lower":
  riscv64 has no flags, its instructions are 2 to 6 graph operations, and
  FP is absent from a boot. The basis for "not higher": the costs that 3e
  found outside compiled code (round trips, the softmmu, translation) are
  the same code here. At 250 MIPS the three boots are 95 to 162 s of
  execution.

## 3. The CPU core

### G1. The crate and its parts

A new crate `vmlab-hv-tcg-rv64` holds the core. `vmlab-hv-tcg-riscv64` keeps
its name and its content, the generated Sail artefacts, and becomes a
dependency of tests only.

```
decode.rs        the opcode map of RV64GC, Zba, Zbb, Zbs: one variant per format
compressed.rs    a 16-bit instruction to its 32-bit form, or Illegal
state.rs         x1 to x31, f0 to f31, pc, the privilege mode, the CSR store
lift/builder.rs  the builder over native::Graph
lift/integer.rs  lift/branch.rs  lift/loadstore.rs  lift/atomic.rs
lift/fp.rs       lift/system.rs
helpers/         FP through SoftFloat, the AMOs, the CSR accesses
csr.rs           one table: storage, reset value, access per mode, write mask, hook
trap.rs          exceptions and interrupts: delegation, entry, mret, sret, wfi
pmp.rs           the 16 entries as a check at the fill of the TLB
mmu.rs           the Sv39 and Sv48 walk, the fill and the flush of the TLB
exec.rs          the run loop: translate, run, write back, take traps
oracle/          the adapters of O1 to O3, the stream generator, the block differential
```

### G2. The instruction set, exactly

The ISA string of the device tree, and what `misa` says:

```
rv64imafdc_zicntr_zicsr_zifencei_zihintpause_zihpm_zmmul_zaamo_zalrsc_zca_zcd_zba_zbb_zbs
```

- `I`, `M`, `A`, `F`, `D`, `C`, with Zicsr and Zifencei: RV64GC.
- Zba, Zbb, Zbs: P2.
- Zicntr: `cycle`, `time` and `instret`. The kernels read `time` (M2e).
- Zihpm: the 29 hardware counters and their events exist, hold what is
  written to them, and do not count. OpenSBI finds 16 of them on QEMU and
  will find 29 here or 16, which C4 decides by probe.
- Zihintpause: `pause` is a NOP of the `fence` space. It is listed because
  the kernel asks for the name.
- Zmmul, Zaamo, Zalrsc, Zca, Zcd: other names for parts of M, A and C.
- The MMU type is `riscv,sv48`. Sv39 is implemented too, because a kernel
  may choose it. Sv57 is refused as the specification says: a write of that
  mode to `satp` leaves `satp` unchanged.
- Privilege: M, S and U, version 1.12. No H. PMP with 16 entries and a
  granularity of 4 bytes (M1).
- `mvendorid`, `marchid` and `mimpid` read zero. QEMU's `marchid` is `0x2a`.
- Not present, and each is an Illegal Instruction or an absent CSR: V, H,
  Zbc, Zfa, Zawrs, Zicbom, Zicbop, Zicboz, Zicond, Sstc, Svadu, Svinval,
  Svnapot, Svpbmt, Sscofpmf, AIA, the debug triggers.

This is a guest-visible difference to today under HV-D30, already approved
as D7. The list above is the allow-list entry that D7 asks for.

The decoder:

- **G2a.** `decode.rs` is written by hand from the opcode tables of L2:
  about 190 instructions in about 40 formats. There is no generator. The
  LLVM route of 3d (2,332 records) answers a problem that riscv64 does not
  have.
- **G2b.** The oracle of the decoder is the generated Sail decoder of 3a
  (P1): a test of `just ci::check` asks the 3a interpreter for the tag of an
  encoding and compares it with ours, for every 16-bit word and for a fixed
  set of 32-bit words (every instruction with boundary fields, and random
  words). The exhaustive run over 32-bit words of the opcodes that we decode
  is a recipe.
- **G2c.** An instruction of 16 bits is expanded to its 32-bit form before
  the lifter (`compressed.rs`), so there is one lifter per operation. The
  length stays with the instruction for the PC and for `jal` and `jalr`.

### G3. How the semantics are written

As 3d:G3, with these differences:

- **G3a. One lifter per format, pure IR for all of the integer set.** There
  are no flags, so nothing like 3d:G3c exists. A conditional branch compares
  two registers and is one guard.
- **G3b. `x0` is a constant** in the builder, never a slot.
- **G3c. FP is helper calls** with declared effects (`fcsr`, and the FS
  field of `mstatus`). The values are NaN-boxed in 64-bit slots. The rule
  for when FS becomes Dirty follows the Sail model exactly, because the
  oracle compares `mstatus` after every instruction (R7).
- **G3d. CSR accesses, `ecall`, `ebreak`, `mret`, `sret`, `wfi` and
  `sfence.vma` end a block** and run in Rust, as the `Trap` of the aarch64
  loop does. The exceptions are the hot reads of G7c.
- **G3e. Unknown is loud** (3d:G3f): an encoding outside G2 and a CSR
  without a row take the Illegal Instruction exception and are logged once
  with the name.

### G4. Privilege, traps and interrupts

Unlike 3d:G4, the highest level is real: OpenSBI runs in M-mode from the
reset, so M, S and U are all built in card C4.

- `trap.rs` holds one function for the target mode of a trap (from `medeleg`
  and `mideleg`) and one for the entry. `mstatus` is one stored value, and
  `sstatus`, `sie` and `sip` are views of the machine registers.
- The core has four interrupt inputs as levels: machine software, machine
  timer, machine external and supervisor external. The CLINT drives the
  first two and the PLIC the last two (V2). The supervisor software and
  timer bits of `mip` are written by OpenSBI. The core looks at the inputs
  between two slices, as the aarch64 core does.
- `wfi` with no enabled interrupt pending is a halt exit of the vCPU loop.
- There is no hypercall exit (E3m). SBI calls are `ecall` instructions that
  trap to M-mode inside the guest.
- **Why not serve SBI in the VMM** and skip M-mode: OpenSBI is what QEMU
  runs, so its banner, its SBI version and its behaviour are what the guest
  sees today (HV-D30), and M2d shows that it is cheap. A machine that needs
  no firmware (a later container micro-VM) could get an SBI in the VMM. That
  is not part of 3b.

### G5. PMP

OpenSBI programs the entries from the eight regions of its Domain0 (M1).

- The check runs at the fill of a TLB entry, not at an access. An entry of
  the TLB holds the rights of its page for the current mode.
- A 4 KiB page that PMP does not cover with one answer for all of its bytes
  is not cached: its accesses take the slow path. OpenSBI's regions are
  aligned to 4 KiB (M1), so a boot has no such page.
- A write of a PMP register flushes the TLB of the hart. It happens at the
  start of OpenSBI only.

### G6. Translation on the shared TLB

- `mmu.rs` walks Sv39 and Sv48 with the level count as data. The accessed
  and dirty bits are handled as the Sail model does for a hart without
  Svadu: a page fault when the bit is clear. C4 confirms against the model
  what QEMU does on this CPU, because the two could differ, and the boot
  lockstep follows QEMU (R3).
- One `Tlb` of E3c per translation regime: one for M-mode (no translation,
  PMP only) and one for S and U together, with `SUM`, `MXR` and the mode in
  the context key of a block, as 3d does for its levels.
- The ASID of `satp` is the identifier of an entry. A write of `satp` that
  changes only the ASID flushes nothing.
- `sfence.vma` with an address invalidates that mapping, by its span. With
  an ASID it invalidates that address space. With neither it flushes (M2f).
- `MPRV` in `mstatus` makes M-mode loads and stores use the translation of
  `MPP`. OpenSBI uses it to read the instruction of a trap. Such an access
  takes the slow path.
- A fetch that crosses a page boundary (a 32-bit instruction at offset
  `0xffe`) is translated for both pages, and the block of that instruction
  holds no other instruction (R6).
- Self-modifying code goes through the code stamps of E3c, as in 3d.
- There is no broadcast invalidation in the core. A remote `sfence.vma`
  is an IPI that OpenSBI sends, and each hart runs its own.

### G7. Time and counters

- **G7a.** `mtime` of the CLINT is the guest's clock: 10 MHz, derived from
  host time with an offset that a pause and a snapshot keep (3d:V4). Under
  the lockstep it comes from the leader.
- **G7b.** `instret` and `cycle` count retired instructions. The budget of a
  block already holds that count. Under the lockstep the Sail model ticks
  them once per step, and ours must match it exactly.
- **G7c.** `time`, when `mcounteren` and `scounteren` allow it, does not end
  a block: it is a helper call that reads the clock of the CLINT. 39 million
  reads in three boots (M2e) must not be 39 million block ends.

### G8. The JIT forms (P3)

The emitter of `vmlab-hv-tcg-jit` compiles the block form of E3b, which
knows no guest. What riscv64 adds:

- **G8a.** A lowering of its blocks into the block form, the exits with
  their write-back, and the context key (the mode, `satp`, `SUM`, `MXR`,
  `MPRV`, FS).
- **G8b.** The fill and the flush of the fast TLB (3e:H7) from G5 and G6.
- **G8c.** Chaining inside a page and the jump cache with the ASID in its
  key (3e:H5, H6). A block may start at any even address.
- **G8d.** The helpers as direct calls: the FP operations, the AMOs, `time`.
- **G8e.** What it does not need: the flag maps of 3e:H8 and the host-flag
  fusion. A compare-and-branch of riscv64 is the form that 3e:H8 builds for
  aarch64 from two instructions.
- **G8f.** Where the run loop lives. Card C1 writes a small loop for the
  interpreter only. Card C8 then moves the parts of the aarch64 loop that
  know no guest (the block store, the entry into compiled code, chaining,
  the jump cache, the replay check) behind a trait in `vmlab-hv-tcg`, with
  aarch64 as the first user and riscv64 as the second. The proof that
  aarch64 did not change is its own K1 to K3 and its bench rows (R1).

### G9. More than one vCPU

One host thread per hart, with its own block store, code cache and TLB
(D10, 3e:H14).

- `lr` and `sc` are `load_reserved` and `store_conditional` of `Memory`
  (3d:R3).
- An AMO is one step: a host atomic after a TLB hit, or the lock of
  `SharedMemory`. 3a found lost updates here before it added the lock.
- Plain loads and stores are plain host accesses. The host's ordering is
  stronger than RVWMO.
- Every hart starts at the reset vector and runs OpenSBI, which parks the
  ones that are not the boot hart in `wfi` until an HSM call starts them.
  The machine starts nothing itself, and has no PSCI.

## 4. The `virt` machine

### V1. The map and the device tree

The machine presents M1, address for address. `vmlab-hv-vmm` gets a module
`riscv_virt` beside `virt`, which stays the aarch64 machine. The device tree
is written with the FDT writer of E3g, node for node from M1, and placed
where QEMU places it (`0xffe00000` for 2 GiB; C7 measures the rule for other
sizes). `/chosen` carries the `rng-seed` and the `stdout-path`.

Differences to QEMU's tree, each in the approved-differences file of the
hwref diff (O6): the ISA strings and the MMU type (G2), and no `riscv,pmu`
node, because Sscofpmf is absent.

There is no ACPI (`acpi=off`), so nothing of 3d's ACPI work applies. SMBIOS
and the other files that QEMU offers through fw_cfg are listed by a qtest
probe in C7 and matched.

### V2. New devices

| Code | Device | What it is | Est. lines |
|---|---|---|---|
| V2a | PLIC | 95 sources, a priority per source, pending bits, and per context (M and S of each hart) the enables, the threshold, and claim and complete. Level inputs through `IrqLine`. Two output lines per hart into the core. | 500 |
| V2b | CLINT | Per hart `msip` and `mtimecmp`, and one `mtime`. It drives the machine software and timer lines. It owns the clock of G7a. | 250 |
| V2c | goldfish RTC | The time in nanoseconds in two registers, an alarm, an interrupt. The guest's `rtc0`. | 200 |
| V2d | Test device | A write of `0x5555` is a power-off, `0x7777` a reset, `0x3333` with a code a failure. They become the shutdown and the reset of the vCPU loop. | 80 |
| V2e | 16550 over MMIO | The existing `Serial`, mapped at `0x10000000` with byte registers, on PLIC 10. | 100 |
| V2f | Empty virtio-mmio slots | Eight pages that answer as QEMU's empty slots do (a probe decides the values), so that the kernel's probe ends as it does today. | 60 |

Existing and reused: the two flash banks (32 MiB each), fw_cfg over MMIO,
the PCI host with its ECAM and windows, virtio-blk, virtio-net,
virtio-serial.

**INTx.** The PCI functions interrupt through the four INTx lines on PLIC 32
to 35, with the swizzle that the `interrupt-map` of M1 names. The virtio
transport and the xHCI already take an INTx line (`intx: Option<Arc<dyn
IrqLine>>`). On aarch64 and x86 the guests use MSI-X, so the INTx path with
several functions on one level line is used for the first time on a real
boot here (R4).

### V3. Reset and the firmware path

- The reset vector is a small ROM of our own at `0x1000`, as QEMU has one
  there. It sets `a0` to the hart id, `a1` to the address of the device
  tree, `a2` to the address of a `fw_dynamic_info` record (magic, version,
  the next address `0x20000000`, the next mode S), and jumps to
  `0x80000000`. The record is documented by OpenSBI (L4). C7 reads the
  registers at the first instruction of OpenSBI on QEMU over the gdbstub and
  matches them.
- OpenSBI is loaded at `0x80000000`. EDK2 is the first flash bank and the
  variables are the second.
- A reset from the test device puts every hart back to the reset vector, in
  place, as the aarch64 machine resets (3d:C14).

### V4. What the machine does not present in 3b (P5)

The refusal list of aarch64 as 3d shipped it, for riscv64:

- the machine is `virt`, the firmware is UEFI (EDK2), 1 to 8 vCPUs, at least
  64 MiB of memory,
- virtio-blk disks and CD-ROMs, virtio-net NICs, the agent on virtio-serial,
- refused, each by name: a `gpu {}` block, `usb = true`, a TPM, `audio`, a
  `cpu_model`, a NIC that is not virtio-net, `agent_transport =
  "isa-serial"`, `qemu_args`, `accel = "kvm"`, a floppy, an Apple SMC,
- a display device that the profile names is left out and the VM runs
  headless on its serial console, with the line that 3d printed for it.

The limit of 8 vCPUs is 3d's number and not a limit of the PLIC. It stays so
that the two lists are one list.

What riscv64 can take later from the devices that aarch64 gains now:

| Card | Devices | For riscv64 |
|---|---|---|
| R1 (#1224) | No QEMU fallback, refusal at `up` by name | In 3b from the start (P5). |
| R2 (#1225) | virtio-gpu with a 2D scanout, the xHCI with keyboard and tablet, the vmlab display path | Reusable without a new device: all are PCI functions, and EDK2 `RISCV_VIRT` has the drivers. The work is the INTx wiring, the display path of `describe.rs` for riscv64 and the e2e screenshot. It is the first follow-up, because the logo of P4 is seen only on a display. |
| R3 (#1226) | xHCI for passthrough, virtio-sound, a CD-ROM with eject | Reusable: PCI functions that know no architecture. |
| R4 (#1227) | virtio-fs with write tracking, TAP NICs | Reusable: the write tracking is the stamped write of `GuestBus` (E3f), which riscv64 uses from C6. The vhost rule of `guest_bus.rs` applies as on aarch64. |

## 5. The firmware (P4)

- **F1. EDK2.** `RISCV_VIRT_CODE.fd` is built from the source that
  `guest/build-ovmf.sh` already pins (Debian's `edk2` 2026.08+ds-2, the
  `orig` and `debian` tarballs by sha256), with `guest/logo/Logo.bmp` in
  place of the EDK2 logo, by a cross compiler in the `vmlab-build` image.
  `RISCV_VIRT_VARS.fd` comes from the same build. Both are 32 MiB.
- **F2. OpenSBI.** `fw_dynamic.bin` of the generic platform, from Debian's
  `opensbi` package, pinned by version and sha256 in
  `guest/fetch-firmware.sh`, as AAVMF is. The version is the one nearest to
  QEMU 11.1.3's v1.8.1 that Debian has. Its banner is guest-visible, so the
  version is in the approved differences when it is not 1.8.1.
- **F3. The bundle.** `guest/dist/firmware/riscv64/` holds the three files,
  `VERSION`, and `COPYRIGHT` (Debian's `copyright` files of both packages,
  which carry L4 and L5). `src/qemu/firmware.rs` gets the bundled names
  for riscv64 before the host paths. `guest/package.sh` and the release
  assets include the directory.
- **F4. In the protocol.** `FirmwareKind` of `vmlab-hv-proto` gets a kind
  for this machine with three paths: the code, the variables and the SBI
  firmware.
- **F5. For the hv tests.** Until F1 is built, cards C7 to C10 use the pair
  of the host (`/usr/share/edk2/riscv64`, EDK2 202608) and QEMU's
  `opensbi-riscv64-generic-fw_dynamic.bin`, as `bench/lib.py` does today.
- **F6. A fact about "as for the other arches".** Only the x86_64 firmware
  carries the vmlab logo today. AAVMF is Debian's binary, unchanged. The
  script of F1 can build AAVMF with the logo too. That is not part of 3b.

## 6. The proof

### O1. Instruction lockstep against the Sail C model

The method of 3a (D6), on the new core. No QEMU is in it.

1. The oracle is built from the pinned model with a configuration of its
   own, `rv64-3b.json`: the extensions of G2 on and every other one off, 16
   PMP entries, Sv48. The configuration is data that the C model reads when
   it starts (`oracle_create`). So "Illegal on one side only" is a
   difference and never noise.
2. The corpus: riscv-tests (134 flat images today), riscv-arch-test for I,
   M, A, F, D, C, Zba, Zbb, Zbs and the privileged tests that fit G2, and
   random streams.
3. The stream generator takes formats from `decode.rs` and makes random
   instructions with boundary values, 16 and 32 bits mixed, in a flat image
   with a trap vector that records and continues, as 3d:O3 does. A second
   source is the list of distinct encodings that the three boots executed
   (M2: 258,385 to 317,069 per boot).
4. Both sides run from the same state. The harness sets the state of the
   oracle through its setters, which 3d could not do with QEMU. It compares
   after each instruction: the PC, `x1` to `x31`, `f0` to `f31`, the
   privilege mode, every CSR that the oracle lists, and the ordered
   physical writes.
5. For the privileged group the streams run with random page tables of Sv39
   and Sv48, random PMP entries, traps into M and S with random delegation,
   and interrupts raised by the harness on both sides at the same step.

A card proves a group with: every instruction of the group in the stream, at
least 100,000 instructions per format without a difference, the executed
encodings of the three boots for the group, the tests of L6 and L7 for the
group, and the counts and the seed in its report. The speed of the Sail C
model is not measured yet. Card C1 measures it and sets the size of the run
that fits into `just ci::check`. The rest is the recipe `just
tcg-rv64-lockstep group=...`. The oracle library is built by `just
tcg-oracle-build` in the container and is not built inside `ci::check`, so
the merge bar replays a committed regression corpus against stored states,
as 3d does.

### O2. The JIT against the interpreter

3e:K1 to K3, for riscv64: O1 with the JIT on, the block differential on two
copies of a `FlatMemory` as a test of `just ci::check`, and the replay check
of every block on a real boot behind a switch.

### O3. The window boot lockstep of Debian against QEMU

The form of 3d:O4w. Our machine boots Debian to agent ready at one vCPU and
writes the state of its CPU and its RAM at points of the boot. For each
point a QEMU starts from that state with the plugin, and our CPU follows its
stream before every instruction.

What changes for riscv64:

- The leader starts with `-bios none` from a flash image that our side
  writes. Its prelude runs in M-mode: it writes every CSR of M and S, the
  PMP entries, the FP registers and `fcsr`, the integer registers last, and
  leaves with `mret` into the mode of the window. A window may start in M, S
  or U.
- The leader's CPU is the one of M2. Its RAM is loaded as a file. What QEMU
  writes into RAM itself (the device tree) is put back by the prelude, as
  3d does for its first MiB.
- Adopted from the leader: the value of a `time`, `cycle` or `instret` read,
  and `mip` bits that only the leader's devices drive.
- A window ends at its budget, at the first access outside RAM, at `wfi`, at
  an interrupt of the leader, or at a CPU difference. An `ecall` does not
  end it: OpenSBI is in the RAM image, so the window runs through M-mode
  until OpenSBI touches a device.

What it proves and what not is 3d:W1 to W5, with one gain: QEMU and the Sail
model are two independent references, so a behaviour where our core agrees
with one and not the other is a finding by itself (R3).

### O4. Devices and the machine

- A qtest recording of QEMU's PLIC, CLINT, goldfish RTC, test device and
  16550 over MMIO (`docs/hwref/`), replayed on our models in `ci::check`:
  every read, every interrupt level, every byte of serial output, as
  `virt_oracle.rs` does for the GIC and the PL011.
- The device tree: ours against `dumpdtb` of QEMU, normalised, with the
  approved differences of V1.
- The hwref diff of a booted Debian (D9, `scripts/hwref`): `lspci`, the
  interrupt list, `/proc/cpuinfo`, the SBI lines of `dmesg`, with the
  approved differences of G2 and F2.

### O5. S1: the templates

Each of Debian 13, Fedora 42 and Ubuntu 24.04 reaches agent ready on the
control socket of `vmlab-hv`, at 1 and at 2 vCPUs, three runs each, with the
JIT on, inside 600 s and under the memory cap, with no emergency mode in the
serial log. 18 boots.

### O6. Snapshots

The S1 snapshot tests of 3d:C14 on a riscv64 machine, with the JIT on: an
online and an offline snapshot, a restore, a guest reset, and the agent
answers after each. The code cache, the TLBs and the jump cache are built
again after a restore (D13).

### O7. e2e and the merge bar

- e2e `arch.emulated` gets a riscv64 guest on `--hypervisor vmlab`: the
  bundled EDK2 and OpenSBI reach the UEFI shell on the serial log, and the
  selected hypervisor is vmlab-hv. The screenshot of the aarch64 leg waits
  for R2.
- `just ci::check` at exit 0 on every card.

### What this method cannot prove

- **U1.** Where the Sail model and QEMU agree and are both wrong, or where
  the specification allows a choice and we take the model's.
- **U2.** The moment of an interrupt. O1 raises interrupts at chosen steps
  and O3 has none inside a window. Masking, delegation and priority are
  tested by O1, and the rest by the boots.
- **U3.** More than one hart: interleavings, `lr` and `sc` under
  contention, IPIs. O1 and O3 run one hart. The tests are a stress test of
  `lr`, `sc` and the AMOs on two threads, and the templates at 2 vCPUs.
- **U4.** Devices and DMA beyond the recorded accesses of O4, and the INTx
  path under load (R4).
- **U5.** The instructions between two windows of O3, and that our boot
  reached the state of a window as QEMU would have (3d:W3, W4).
- **U6.** A behaviour that no test reaches.

## 7. The vmlab side

- **H1.** `emulated_guest` is true for riscv64, and the automatic fallback
  to QEMU goes. `hypervisor = "qemu"` on a riscv64 VM stays possible as a
  troubleshooting setting until C16 of 3d removes the code, as for aarch64.
- **H2.** The refusals of V4 from the one list in `src/config/hypervisor.rs`,
  read by validation and by the lab daemon, with texts that name riscv64.
- **H3.** `describe.rs` composes the machine description of a riscv64
  launch: the machine, `accel: tcg`, the firmware of F4, the disks, the
  NICs and the agent port.
- **H4.** An existing QEMU identity of a riscv64 VM moves to vmlab-hv at the
  next start, and `snapshot list` names an online QEMU snapshot as not
  restorable, as §22.3 does for the other guests.
- **H5.** The firmware lookup of F3, `vmlab doctor` and `install.sh` where
  they name QEMU for riscv64.
- **H6.** The PRD (§22), the manual, `CLAUDE.md`,
  `docs/agents/qemu-removal-remaining.md`, and `examples/riscv64-ubuntu`.
- **H7.** `../phase-3-tcg/MAP.md` has the rows of this design (D20 to D23,
  Q22 to Q25), and `SPEC.md` names it for 3b.

## 8. Risks

- **R1. The run loop is not shared.** 8,147 lines of `exec.rs` are aarch64
  by name (E3k). Card C8 either moves the guest-free parts behind a trait
  (G8f) or writes them a second time. The first costs a change in the hot
  path of aarch64, proven by its own tests and rows. The second costs
  about 3,000 lines that then drift apart. The design takes the first and
  gives C8 the time for it.
- **R2. M-mode is real.** 3d built two levels and no highest one. 3b builds
  three modes, PMP, delegation and `MPRV` before the first firmware line
  prints. The Sail model covers all of it per instruction, which 3d did not
  have.
- **R3. Two references can disagree.** QEMU's CPU and the Sail model may
  differ where the specification allows a choice: the accessed and dirty
  bits, the value of `mtval` on some traps, WARL fields, when FS becomes
  Dirty. The rule: O1 (the model) decides the core, and a place where the
  boot lockstep then differs from QEMU is recorded in an allow-list of the
  follower with the paragraph of the specification, and not "fixed" towards
  QEMU.
- **R4. INTx only.** No guest of vmlab-hv has run its virtio devices on
  shared level lines for a whole boot. C6 tests two functions on one line
  through the PLIC, and C7 boots with disk, NIC and agent on them.
- **R5. Fedora at one vCPU.** The aarch64 Fedora missed S1 at one vCPU
  under load until the JIT was faster (3e:C13b-S1). Fedora 42 is 35.4
  billion instructions here, and Ubuntu 40.5 billion. Both need the JIT
  (M4). C10 measures the margin before C11 selects vmlab-hv.
- **R6. Instructions of two lengths.** A block can start at any even
  address and an instruction can cross a page. The invalidation of
  translated code (the code stamps) and chaining inside a page must both
  count the second page of such an instruction. Tests in C1 and C8.
- **R7. FS and the oracle.** The Sail model sets FS to Dirty by its own
  rule, and a kernel context switch reads FS. A lazy FS in the JIT would be
  faster and would differ from the oracle at every FP instruction. 3b keeps
  FS exact. FP is 0.002 to 0.017 % of a boot.
- **R8. The cross build of EDK2.** The `vmlab-build` image has no riscv64 C
  compiler today. F1 adds one, and the build must be reproducible as
  `build-ovmf.sh` is. F5 keeps the other cards from waiting on it.
- **R9. The speed of the Sail C model is not measured.** If it is far below
  1 MIPS, the size of the O1 runs per card is smaller than 3d's, and the
  executed encodings of the boots (up to 317,069) carry more of the proof
  than long random streams.

## 9. Sizes (est.)

Hand-written lines of Rust without tests. The basis is the count of formats
and CSRs above and the sizes of the same parts of aarch64 in the tree
(`vmlab-hv-tcg-aarch64`: 13,021 lines of core with a loop of 8,147, 6,813 of
lifters, 2,822 of helpers, 9,157 of oracle tools; the `virt` machine 3,216;
its devices 1,449).

| Part | Lines |
|---|---|
| Decoder and the compressed expansion | 1,300 |
| State, builder, field helpers | 800 |
| Integer, M, Zba, Zbb, Zbs, branches | 900 |
| Loads, stores, `lr`, `sc`, the AMOs | 700 |
| F and D: lifters, helpers, the SoftFloat glue | 1,500 |
| CSR table (about 110 rows), traps, interrupts, `wfi` | 1,800 |
| PMP, the Sv39 and Sv48 walk, the TLB fill and flush | 1,400 |
| The interpreter loop and the block cache (C1, C4) | 1,500 |
| The JIT forms, and the shared loop moved out of aarch64 (G8) | 3,500 |
| Oracle tools: the Sail adapter, the stream generator, the block differential, the window follower and its prelude | 4,000 |
| Devices of V2 with their recordings | 1,800 |
| The machine: map, device tree, reset ROM, builder, snapshot state | 2,500 |
| The emulated backend for a second core | 600 |
| vmlab side: describe, refusals, selection, firmware lookup | 1,500 |
| Firmware scripts, bench, e2e | 1,100 |
| Total hand-written | about 24,900 |

3d estimated 34,600 lines for aarch64. The difference is the instruction
set: about 190 instructions against 2,332 records, and no AdvSIMD.

## CSRs the templates touch (measured)

The union over the three boots of M2, 104 numbers, OpenSBI and EDK2
included:

- Floating point: `fflags`, `frm`, `fcsr`.
- Supervisor: `sstatus`, `sie`, `stvec`, `scounteren`, `sscratch`, `sepc`,
  `scause`, `stval`, `sip`, `satp`, and as probes that trap on this CPU
  `sstateen0` (`0x10c`), `stimecmp` (`0x14d`) and `scountovf` (`0xda0`).
- Machine: `mstatus`, `misa`, `medeleg`, `mideleg`, `mie`, `mtvec`,
  `mcounteren`, `menvcfg`, `mcountinhibit`, `mscratch`, `mepc`, `mcause`,
  `mtval`, `mip`, `mvendorid`, `marchid`, `mimpid`, `mhartid`, and as probes
  `mstateen0` (`0x30c`), `mcyclecfg` (`0x321`), `tselect` (`0x7a0`) and
  `mtopi` (`0xfb0`).
- PMP: `pmpcfg0`, `pmpaddr0` to `pmpaddr15`, and `pmpaddr16` as a probe.
- Counters: `cycle`, `time`, `instret`, `mhpmevent3` to `mhpmevent18`,
  `mhpmcounter3` to `mhpmcounter31`.

The busiest: `sstatus` 77.3 million accesses, `time` 39.2 million,
`sscratch` 8.4 million, `sepc`, `scause` and `stval` about 2.1 to 4.2
million each. Other SYSTEM instructions: `sret` 2,099,931, `sfence.vma`
1,980,823, `ecall` 1,817,296, `mret` 237,847, `wfi` 1,640, `ebreak` 3.

Whether a probed CSR traps or reads zero decides what OpenSBI and the
kernel believe about the hart. Card C4 has a row and a test for each of the
104, with the answer that the Sail model gives under `rv64-3b.json`.

## 10. The cards

The order is the number. "With" names the cards that can run at the same
time. Every card ends with `just ci::check` at exit 0, lists the sources
that it read (L1 to L12), keeps X1 and X2, and from C5 on adds rows to
`hv/bench/history/riscv64.jsonl`.

| Card | Title | What to do | Proof | After | With |
|---|---|---|---|---|---|
| C1 | riscv64 core path end to end: decoder, integer set and branches, in lockstep against the Sail model | The crate `vmlab-hv-tcg-rv64` (G1). `decode.rs` and `compressed.rs` for all of G2, with the decoder test of G2b. The state and the builder. Lifters for I, M, Zba, Zbb, Zbs, the branches and the jumps. A small interpreter loop on `FlatMemory`, M-mode only, with `ecall` and Illegal Instruction as stops. The oracle configuration `rv64-3b.json` and the adapter of O1, with the stream generator. The notices (L1, L2), and X1 and X2 in `hv/AGENTS.md`. | The decoder agrees with the Sail decoder on every 16-bit word and on the fixed 32-bit set. O1 for the group: 100,000 instructions per format and the executed encodings of the three boots, no difference, counts and seed in the report. `rv64ui`, `rv64um` and `rv64uc` of riscv-tests, and riscv-arch-test for I, M, C and the three Zb sets. The measured speed of the Sail C model and of our loop. | none | none |
| C2 | Loads, stores and the A extension | The loads and stores of every width on a flat address space, `lr`, `sc` on the reservation pair, the AMOs as one step (G9). `fence` and `fence.i`. Misaligned accesses as the model does them. | O1 for the group with the memory writes compared. `rv64ua`. A stress test of `lr`, `sc` and the AMOs on two threads over `SharedMemory`. | C1 | C3 |
| C3 | F and D | The FP register file, `fcsr`, FS (G3c, R7), the lifters and the SoftFloat helpers for F and D, with the compressed FP loads and stores. | O1 for the group with `fcsr` and `mstatus` compared, in all five rounding modes. `rv64uf` and `rv64ud`. riscv-arch-test for F and D. | C1 | C2, C4 |
| C4 | Privilege, the CSRs, PMP and translation on the shared TLB | `csr.rs`, `trap.rs`, `pmp.rs`, `mmu.rs` (G4 to G7): M, S and U, delegation, `mret`, `sret`, `wfi`, the four interrupt inputs, the 104 CSRs of the measured list, PMP at the fill of the TLB, Sv39 and Sv48, `sfence.vma` by address and by ASID, `MPRV`, `SUM`, `MXR`, the counters, a fetch across a page. | O1 for the privileged group (O1, step 5). `rv64mi` and `rv64si`. A row and a test for each of the 104 CSRs. The unit tests of the TLB rules. | C2 | C3, C5 |
| C5 | Flat CoreMark and the loop on the new core, and the baseline | A flat backend of `bench/` for the new core beside the two of 3a. The QEMU baseline of the three templates at 1 and 2 vCPUs with three repeats, on the CPU of M2 and on `-cpu max`. | CoreMark's own checksums. The rows in the history, three repeats, pinned: the interpreter of the block form against QEMU's 4,082 and 6,346 MIPS. | C2 | C3, C4, C6 |
| C6 | VMM: the riscv64 core behind the vCPU seam, and the devices of `virt` | `vcpu/emulated.rs` for a second core (E3e, E3m). The PLIC, the CLINT with the clock, the goldfish RTC, the test device, the 16550 over MMIO, the empty virtio-mmio slots (V2). No machine yet. | The KVM and aarch64 tests of the hv workspace pass unchanged. The qtest recordings of O4 replayed on each model. Two PCI functions on one INTx line through the PLIC (R4). A test VM of one emulated hart that writes to the UART. | C1 (the seam), C4 (the test VM) | C5 |
| C7 | The `virt` machine: OpenSBI and EDK2 to the UEFI shell, and the window boot lockstep | The module `riscv_virt` (V1, V3): the map, the device tree, the reset ROM, the flash banks, fw_cfg, the ECAM with virtio-blk, virtio-net and virtio-serial on INTx, reset and power-off. The firmware of F5 and the proto kind of F4. The follower and the prelude of O3. | S1: the OpenSBI banner and the UEFI shell on the serial log. The device tree diff of O4. A Debian kernel reaches its first user process. O3 runs windows of that boot without a CPU difference, or each difference is in the allow-list of R3. | C4, C6 | C8 |
| C8 | The JIT forms for riscv64 | G8: the guest-free parts of the aarch64 loop behind a trait, the lowering, the exits, the context key, the fast TLB fill and flush, chaining, the jump cache, the direct helper calls, `time` without a block end. The block differential and the replay check of O2. The JIT on by default for riscv64 when O2 passes. | aarch64: its K1 to K3 and its bench rows before and after, no difference and no regression. riscv64: O1 with the JIT on for every group, one million random blocks in the differential, the replay check on the boot of C7, the stress test of C2 with the JIT on. The rows. | C4, C5 | C7 |
| C9 | Snapshots, reset and the debug port | The state of the riscv64 CPU, the PLIC, the CLINT with its clock, the RTC, the UART, fw_cfg and the variables bank in the snapshot format (E3h). A guest reset in place. The debug port for riscv64 registers. | O6. | C7 | C8, C10 |
| C10 | The three templates to agent ready | What the boots still miss, found by O3 and by the unknown-is-loud log (G3e). Two vCPUs. The bench suite for riscv64 on vmlab-hv. | O5: 18 boots. O3 for Debian over the whole boot, with the count of compared instructions. The hwref diff of O4. The rows of the boot, `sha256` and `xz` against QEMU, at 1 and 4 vCPUs, recorded without a gate. | C7, C8, C3 | C9 |
| C11 | vmlab side: bundle the firmware, describe and select vmlab-hv for riscv64 | F1 to F3: the EDK2 build with the vmlab logo, the pinned OpenSBI, the bundle and its notices. H1 to H6. | `guest/fetch-firmware.sh riscv64` and the EDK2 build give the same bytes twice. The unit tests of the selection, the refusals and the description. A riscv64 lab comes up with `vmlab up` on vmlab-hv and a refused one fails at `up` naming each device. | C9, C10 | none |
| C12 | e2e `arch.emulated` for riscv64 | The riscv64 leg of the scenario (O7), and the scope tables of `e2e/features.py` without a QEMU run for riscv64. | `just e2e::run` for `arch.emulated` on `--hypervisor vmlab`. No `qemu-system-riscv64` is started by a product path. Then C16 of 3d (#1051) is free. | C11 | none |

C1 is small enough to prove the path: one crate, the decoder for the whole
set, the lifters of the group that is 98 % of a boot, and the harness that
every later card uses.

Side by side: after C1, two agents (C2, C3). After C2, three (C3, C4, C5),
and C6 as soon as C4 gives it a test VM. C7 and C8 run together. The longest
chain is C1, C2, C4, C7, C10, C11, C12.

Phase 3c is what comes after: Sstc, an inline `time`, the costs that the
profiles of C10 name, and the rows against QEMU. It has no gate (3e Q4) and
no design yet.

## Questions for the operator

P1 to P5 are answered (thread 3, post 137). These are new.

- **Q1. What "the generated Sail decoder stays" means.** P1 can be read in
  two ways. *O1 (recommended, and what G2 designs):* our own decoder table
  in the new crate, with the generated Sail decoder as its oracle in a test.
  The product binary then holds nothing of the Sail interpreter. *O2:* the
  generated Sail decoder is the product decoder. It needs the 3a interpreter
  and its 2 MB of model IR inside `vmlab-hv`, and a cache in front of it,
  because it decodes far slower than a table.
- **Q2. The guest-visible differences of G2, V1 and F2**, for approval under
  HV-D30 beyond what D7 already allows: the MMU type `sv48` in place of
  `sv57`, no `riscv,pmu` node and no Sscofpmf, `marchid` zero in place of
  `0x2a`, and an OpenSBI version that may not be 1.8.1. *Recommended: yes.*
  The three templates boot with the first two (M2). The last two are
  printed in `dmesg` and used by nothing.
- **Q3. The display.** P5 gives a riscv64 VM no display in 3b, so it loses
  the screen that it has on QEMU today, and the logo of P4 is not seen
  until R2 is taken over (V4). *O1 (recommended):* as P5 says, and one
  follow-up card after C12 for the virtio-gpu, the xHCI input and the
  screenshot of e2e. *O2:* the display inside C11, which makes C11 wait for
  the INTx proof of two more functions and moves the end of 3b.
- **Q4. Where the Sail model and QEMU disagree** on a choice that the
  specification leaves open (R3). *Recommended:* the Sail model decides,
  and the boot lockstep keeps an allow-list. The other answer, QEMU
  decides, would make the per-instruction oracle wrong at those places.

## How to measure again

On a host with QEMU 11, `dtc`, gcc and glib, from `hv/`:

```sh
m=docs/plans/phase-3b-riscv64/measure
gcc -O2 -shared -fPIC $(pkg-config --cflags glib-2.0) -o rv64trace.so $m/rv64trace.c

# M1: the device tree
qemu-system-riscv64 -machine virt,acpi=off -machine dumpdtb=virt.dtb -cpu rv64 -smp 2 -m 2048 -display none
dtc -I dtb -O dts virt.dtb

# M2: one boot to agent ready (bench dir, template, -cpu, vCPUs, output dir, plugin)
ISA=rv64,h=off,zbc=off,zfa=off,zawrs=off,zicbom=off,zicbop=off,zicboz=off,sstc=off,svadu=off,svvptc=off,zihintntl=off,sv57=off,sdtrig=off
TAG=isa python3 $m/probe.py bench debian-13 "$ISA" 1 runs rv64trace.so
python3 $m/summary.py runs/*-isa-smp1-plug/trace.txt
grep -a -E '^(OpenSBI|Platform|Firmware|Boot HART|Domain0)' runs/debian-13-isa-smp1-plug/serial.log

# M3: the same without the plugin, and with -cpu max
TAG=isanp python3 $m/probe.py bench debian-13 "$ISA" 2 runs
TAG=max python3 $m/probe.py bench debian-13 max 2 runs
```

`probe.py` needs the templates under `~/.local/share/vmlab/templates/riscv64`
and the firmware of `bench/lib.py` (`/usr/share/edk2/riscv64`). Every QEMU ran
under the memory cap of the bench, one at a time, pinned.
