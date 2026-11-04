---
id: "03"
kind: research
---
# Emulator architecture and the QEMU TCG baseline

Facts measured 2026-10-31 on helios (Ryzen 9 9950X, 32 threads, 186 GiB RAM, QEMU
11.1.2 from Arch). Scratch data: `target/p3-research/g3/` (driver `bench.py`, per-run
`run/<arch>-smp<N>-r2/result.json`, native baselines in `native/`). Reproduce a run:
`python3 target/p3-research/g3/bench.py <arch> <smp> <id>` with arch ∈ aarch64|riscv64.
Estimates are marked est.; anything skipped says not checked.

## 1. The QEMU TCG baseline (HV-D28's parity bar)

Method: `bench.py` launches QEMU with vmlab's own argv shape for a `linux-modern`
VM on the `virt` machine — built from `/home/wil/dev/vmlab/src/qemu/cmdline.rs:427`
(`build_args`): `-machine virt|virt,acpi=off -accel tcg -cpu max -smp N -m 2048M
-display none -vnc unix:…,share=ignore -qmp unix:… -monitor none -serial file:…
-drive if=pflash ×2 (AAVMF / RISCV_VIRT edk2, exactly what vmlab's
`firmware.rs` picks on this host) -chardev socket + virtio-serial-pci +
virtserialport name=vmlab.agent.0 -blockdev qcow2 overlay (copy-on-write over the
debian-13 13.20261001.0 templates — templates untouched) -device virtio-blk-pci
-device qemu-xhci -device usb-tablet -S`. Deviations from vmlab: `-nic none` (no
segment switch in the harness) and no TPM/GPU. CPUs released via QMP `cont` like
`vm.rs:1109`; readiness = the guest agent answering its Hello on `vmlab.agent.0`,
dialed fresh every 1 s exactly like vmlab's readiness poller (`vm.rs:1182`,
`agent_probe`). Benchmarks run in-guest through the agent's `open_exec`
(`guest/agent-proto/src/lib.rs:426`), timed host-side: `dd` 256 MiB into /tmp,
`sha256sum` of it, `xz -9 -T1` over 32 MiB of repeated text; `nproc` verified the
guest CPU count (1 or 4). Every QEMU ran inside
`systemd-run --user --scope -q -p MemoryMax=32G -p MemorySwapMax=0`, one at a time;
nothing left running (verified after each run).

| guest | -smp | boot→agent-ready | sha256 256 MiB | xz -9 32 MiB |
|---|---|---|---|---|
| aarch64 | 1 | 28.2 s | 1.96 s | 2.6 s |
| aarch64 | 4 | 17.9 s | 1.96 s | 2.7 s |
| riscv64 | 1 | 52.3 s | 1.9 s | 34.3 s |
| riscv64 | 4 | 28.3 s | 1.8 s | 33.4 s |
| native x86_64 | — | — | 0.111 s | 0.249 s |

- sha256 runs at 5.7–6.1 % of native on both guest arches, nearly identical across
  arches and smp — suggesting QEMU lowers the guest crypto instructions to
  efficient host code (mechanism not checked). xz (scalar integer code) is
  9.2–9.6 % of native on aarch64 but 0.73–0.75 % on riscv64: a 13× arch gap from
  TCG codegen plus the guest binaries' generic riscv64 tuning (not disentangled).
- MTTCG is the default here: `-smp 4` aarch64 shows host threads literally named
  `CPU 0/TCG`…`CPU 3/TCG` (measured, `run/` scratch). Boot halves going 1→4 vCPU;
  single-threaded benchmarks do not speed up, as expected.
- Cross-check: vmlab's own event log for the aarch64 debian-13 build lab
  (`~/.local/state/vmlab/labs/bt-deb-aarch64/events.jsonl`) records
  `vm.starting`→`vm.ready` = 19.4 s (template cpus=2), consistent with the
  28.2 s (smp1) / 17.9 s (smp4) bracket.
- Derived D28 bar ("TCG at least 50 % of QEMU's TCG") on this host: boot-to-ready
  no worse than ≈44 s aarch64 / ≈105 s riscv64 at the vmlab-default smp2, and
  ≥50 % of QEMU's in-guest throughput (sha256 ≥ ~65 MiB/s class, xz ≥ ~0.4 MiB/s
  riscv64 / ~6 MiB/s aarch64 class). Pydrofoil's published numbers show how far a
  Sail-derived design can land from that bar: its full AOT+tracing-JIT system is
  still 26.7× slower than QEMU (geomean, SPECint on Ryzen 9), and its
  interpreter-only mode is another 8.6–12.3× below that (arXiv:2503.04389 §4.2–4.3).
  A plain generated interpreter (~1.8 MIPS there) is nowhere near the bar; the
  interpreter-first plan (D22) is a correctness milestone, not a performance one.
- Operational fact for our virtio-serial: a client that connects to the port
  before the guest boots and sends one Hello never gets an answer (observed on
  both arches); vmlab only works because its poller redials every second. Our
  device must buffer pre-open host→guest bytes or document the same redial
  contract.
- Not checked: coremark (guest images carry no toolchain and the harness has no
  network to install one), `-accel tcg,thread=single` comparison, guest
  instruction counts per boot, FP/SIMD benchmarks, xz on identical statically
  built binaries across arches.

## 2. Architecture options, with evidence

**Interpreter designs.** The generator (D34) emits Rust from Sail's IR. Dispatch
options: (a) `match` switch; (b) tail-call threaded code — Wasm3's design; in C,
tail-call dispatch measures ~10–12 % faster than switch (CPython 3.14 work,
discuss.python.org/t/interpreter-dispatch-and-performance-on-webassembly/27246);
Rust has no stable explicit tail calls (RFC 3407, `become`, is nightly — tracking
issue rust-lang/rust#112788), so the practical stable-Rust forms are switch or
callback tables — a real cost to budget for. (c) decode-cache: decode once per
guest PC into a struct with pre-extracted fields (Titzer 2022, "A fast in-place
interpreter for WebAssembly"), which suits a block cache keyed by PC. Pydrofoil's
ablation is the cautionary data: interpreter-only is 8.6–12.3× slower than its JIT
despite all its static optimisations (§4.2) — but its AOT static optimisations
(bitvector→u64 lowering, specialisation) bought 63×/42× *before* the JIT
mattered, so emitter-side specialisation is the highest-leverage interpreter work.

**Block-based JIT with Cranelift.** Cranelift: ~200 k LoC, codegen ~an order of
magnitude faster than an LLVM-based system, generated code ~2 % slower than V8
TurboFan and ~14 % slower than LLVM (cranelift.dev, citing arXiv:2011.13127);
rustc's Cranelift backend cuts rustc CPU time ~40 % vs LLVM
(blog.rust-lang.org 2024-03 "Cranelift code generation comes to Rust", 125 vs
211 CPU-s on rustc) — codegen ~an order of magnitude faster than LLVM-class
backends at ~15 % quality cost. That profile fits per-block guest translation
(compile once, execute many times) far better than it fits rustc-sized units.
QEMU's own design confirms the shape:
translate to TBs, chain blocks directly, look up next TB only on exit
(docs/devel/tcg.rst, "Direct block chaining"). Pydrofoil's lesson is that naive
IR-lifting of Sail semantics will not reach QEMU class (26.7× gap): reaching
50 % of QEMU needs block-level translation with host register allocation for
guest registers — which Cranelift gives if we lower to CLIF with guest GPRs as
CLIF values, not a memory-resident state struct per instruction.

**Dynasm-style template JITs.** Copy-and-patch: pre-compile stencils at build
time with the LLVM toolchain, patch holes at runtime — compilation two orders of
magnitude faster than LLVM -O0 (three vs -O2), generated code 14 % faster than
LLVM -O0 and 39–63 % faster than Chrome's Liftoff baseline JIT, while compiling
4.9–6.5× faster than Liftoff (arXiv:2011.13127). In Rust, `dynasmrt` exists but
is low-level (maintenance status not checked); copy-and-patch stencil generation
needs clang at build time (a build dependency the workspace can carry; clean-room
policy unaffected). Template JITs are
the fallback if Cranelift compile latency ever shows up in boot (Pydrofoil notes
Linux boot is exactly the JIT-latency benchmark, §4).

**Softmmu TLB and MMIO dispatch.** QEMU's shape (docs/devel/tcg.rst, "MMU
emulation"): software TLB per vCPU, physically indexed so MMU changes don't
flush code caches, fast entry stores the guest→host offset so RAM hits stay in
translated code, TLB flag bits force the slow path for MMIO/dirty tracking, MMIO
calls out to device emulation under the global lock. vmlab-hv already owns the
device-model side (BusDevice in vmlab-hv-devices), so the seam is: TLB miss →
guest page-table walk (from the Sail model) → fill; MMIO fault → bus dispatch.

**Multi-vCPU and memory ordering.** QEMU enables MTTCG when the host memory
model is at least as strong as the guest's
(docs/devel/multi-thread-tcg.rst: `guest_default_memory_order &
~TCG_TARGET_DEFAULT_MO is zero`). aarch64/riscv64 guests on an x86_64 TSO host
satisfy it — hence the measured default. Plain guest loads/stores on the TSO
host are safe without extra fences; what needs care is atomics and exclusives:
LR/SC and aarch64 LDXR/STXR must not be split across vCPU thread switches, and
cross-vCPU TLB flushes need quiescent points (QEMU forces "safe work" exits;
same doc). Not checked: exact QEMU exclusive implementation (C source — out of
bounds for clean room); design from the Arm/RISC-V architecture manuals instead.

**Precise exceptions and interrupt checks.** QEMU: exit to the main loop at TB
boundaries re-evaluates pending interrupts; deferred CPU state (flags, etc.) is
stored per guest instruction for precise restoration; host faults (SIGSEGV on
softmmu misses) map host PC→guest PC to recover (docs/devel/tcg.rst,
"Exception support"). For an interpreter the same contract is easier: check
interrupts every N instructions / at branch targets, and keep per-instruction
unwinding trivial since state lives in the CPU struct.

**Self-modifying code and icache invalidation.** x86 needs page-write-trap SMC
detection; our guests are RISCs: aarch64 requires IC IVAU to make stores to
instruction memory visible, riscv64 uses FENCE.I (both architecturally mandated
cache-maintenance/serialization instructions — Arm ARM DDI 0487; RISC-V
unprivileged ISA). So a JIT can trap those two instructions and invalidate
translated blocks in the target page, instead of write-protecting all code
pages. Pydrofoil's analogue — 8-byte-granularity immutable-code tracking with
write invalidation — was worth 2.85–3.23× on its own (§3.5, §4.2), the single
biggest runtime win after the JIT.

**FP/SIMD semantics.** Soft-float (bit-exact, matches the Sail models, which
themselves use Berkeley SoftFloat for RISC-V FP — Pydrofoil §4) vs host FPU
(fast, but NaN payloads/FPCR rounding modes/x87-style pitfalls differ).
Deterministic lockstep (below) argues for soft-float first, host-FPU fast paths
behind a bit-exactness gate later. riscv64 adds NaN-boxing (narrow FP values in
FLEN registers must read as NaN-boxed when re-read wide — RISC-V unprivileged
ISA, "NaN Boxing"); aarch64 carries FPCR rounding/mode bits that must be
modelled and affect block codegen assumptions.

**Determinism and the lockstep harness.** QEMU's answer is icount + record/
replay: count guest instructions, log every non-deterministic event with the
instruction count at which it occurred, replay timers/interrupts/input from the
log under a replay mutex (docs/devel/tcg-icount.rst, docs/devel/replay.rst);
icount is incompatible with MTTCG, forcing single-thread round-robin — a
deterministic mode and a fast mode that cannot share a scheduler. For vmlab-hv,
D22's lockstep harness is per-instruction against the Sail model (TestRIG
pattern: RVFI-DII socket protocol, direct instruction injection bypassing
fetch, per-instruction register/writeback traces, automatic counterexample
reduction — github.com/CTSRD-CHERI/TestRIG). That tier needs our emulator to
expose the same "inject instruction + emit trace" port, and needs timer/
interrupt determinism from instruction counts, not wall-clock. Full record/
replay of whole boots is not required by D26; not checked: whether snapshot
restore (D14) needs the same event log.

## 3. Size estimates (est., basis stated)

| Component | riscv64 | aarch64 | Basis |
|---|---|---|---|
| Emitter (Sail IR→Rust) | 8–15 k LoC | same tool | Pydrofoil's multi-ISA RPython compiler+runtime is 35.8 k LoC (`pydrofoil/pydrofoil`, measured); ours is single-ISA, Rust, no tracing runtime |
| Generated interpreter | 150–300 k LoC generated (read-only artifact) | 2–4× riscv | sail-riscv model is ~30 k lines of .sail (measured: core 6.3 k + extensions 23.4 k); Pydrofoil's RV64 IR text is 7.5 MB (measured); generated code inflates 5–10× over source |
| Runtime (softmmu, TLB, exceptions, IRQs) | 5–10 k LoC | +2–3 k | QEMU's shape per tcg.rst; our device bus already exists |
| Virt machine + devices | 3–6 k LoC | 8–15 k | riscv virt: ACLINT/PLIC, uart, virtio wiring; aarch64 virt: GICv3, timers, SBSA-adjacent — more architecturally visible state |
| Cranelift JIT | 10–20 k LoC ours + ~200 k dep | mostly shared | CLIF lowering of blocks; cranelift.dev's ~200 k LoC for the backend itself |
| Lockstep harness | 3–6 k LoC | shared | RVFI-DII-shaped channel, trace compare, reducer; TestRIG's vengines are small scripts over the socket protocol |

aarch64 is bigger everywhere but the emitter: the sail-arm models are much larger
than sail-riscv (v8.5/9.3/9.4 model trees), and GICv3 is the single largest
device. No Arm estimate is given for generated code beyond "2–4× riscv" — the
sail-arm tree was not cloned (licence hygiene: fine to measure, not done; not
checked).

## Open questions this raises

- O1: Which Sail IR does the emitter consume (the `.ir` text Pydrofoil parses, or
  the JSON AST), and how stable is it across Sail releases?
- O2: Can the interpreter hit 50 % of QEMU TCG at all, or is Cranelift required
  for every performance-gated profile — Pydrofoil's 26.7× gap suggests the
  interpreter lands 20–40× below QEMU, i.e. boots of ~10–20 min.
- O3: Does the lockstep port (instruction injection + trace) double as the
  record/replay substrate for snapshot determinism, or are those separate?
- O4: For atomics/exclusives under MTTCG: exclusive monitors per-vCPU with TB
  exit on contention, or a global monitor? Needs a design note against the Arm
  and RISC-V memory-model appendices.
- O5: Is bit-exact soft-float acceptable for riscv64 FP at ship, given the Sail
  models themselves lean on Berkeley SoftFloat (a BSD-3 dependency to vendor)?
- O6: Our virtio-serial must decide the pre-open buffering contract (§1 finding)
  before phase 1 reuses vmlab's agent channel unchanged.

DONE /home/wil/dev/vmlab-hypervisor/.scratch/plans/phase-3-tcg/notes/03-architecture-baseline.md
