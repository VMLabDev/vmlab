# Review: the TCG interpreter and its emitters

Card #742, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

All paths are relative to `hv/crates/`. Line numbers are those of commit
`a89166b4` (`next` at the start of the card). `tcg/` stands for
`vmlab-hv-tcg/src/`, `ir/` for `vmlab-hv-tcg-ir/src/`, `sail/` for
`vmlab-hv-sail-ir/src/` and `emit/` for `vmlab-hv-tcg-emit/src/`.

The five crates are the software CPU: a RISC-V hart run from the Sail model.
`vmlab-hv-sail-ir` parses the model's textual Jib, `vmlab-hv-tcg-ir` lowers it
to the IR, `vmlab-hv-tcg-emit` writes the IR into `vmlab-hv-tcg-riscv64`, and
`vmlab-hv-tcg` interprets it, with an optional trace specialiser
(`Riscv64::specialize`). No crate of the VMM links them yet, so no finding
here reaches a running vmlab machine today.

## How the review was done

By reading, and by scratch tests. Each finding below was reproduced on the
build host with a test that is not committed. The tests ran in the checkout of
the card as untracked files under `vmlab-hv-tcg/tests/`, with
`cargo test --release -p vmlab-hv-tcg --test <name> -- --nocapture`, and were
removed before the merge bar ran. The "Proof" entry of a finding describes a
test that a fix card can add. It states what the scratch test saw.

Differential runs compared the plain interpreter with the trace
specialiser on the same guest state. After every run they compared every Sail
global and all of guest RAM. They are the evidence for the section "What was
checked and found sound".

The earlier findings F68 to F73 of card #9 (operation budget, terminal cap,
reservation granules, SoftFloat state, the scratch lease, the removed
builders) were read first and are not repeated. Card #11 has no finding in
these crates.

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic. The guest is RISC-V machine
code and the data it writes. The Sail model and the Jib text are build inputs
that the project pins, so a fault that needs a changed model is ranked below
one a guest reaches. No finding lets a guest read or write host memory, and
none panics the host process from guest code. One finding (F1) lets a
guest make the host process hold memory.

## What was read and what was not

Read in full, without the test modules:

- `tcg/`: `lib.rs`, `cpu.rs`, `memory.rs`, `intrinsics.rs`, `value.rs`,
  `decode.rs`, `memo.rs`, `globals.rs`, `indexed.rs`, `continuation.rs`,
  `publication.rs`, `softfloat.rs`, `lockstep.rs`, and `../build.rs`
- `sail/lib.rs`
- `emit/main.rs`
- `ir/`: `lib.rs`, `address.rs`, `effects.rs`, `indexed.rs`,
  `native/packed.rs`, `native/assume.rs`

Read in part:

- `tcg/trace.rs` (1-1770, which is all of it but the test module)
- `tcg/record.rs` (1-140, 380-470, 930-1293) and `tcg/record/lower.rs`
  (1-330)
- `ir/native.rs` (323-420, 754-800) and `ir/native/linear.rs` (1-130)
- `vmlab-hv-tcg-riscv64/src/lib.rs`: the generated code after the licence
  text. The model payload `model.json.gz` was not reviewed as a model. It was
  searched for the intrinsics, the memory targets and the functions without a
  body that the interpreter must provide.

Not read: `tcg/record.rs` 140-380 and 470-930 (the recorder's symbolic
values and its handling of each IR operation), `tcg/record/lower.rs` 330-519,
`ir/native/` `modular.rs`, `schedule.rs`, `block.rs`,
`liveness.rs`, `bitfield.rs`, `dce.rs`, the rest of `native.rs` and
`linear.rs`, `sail/format.rs`, the vendored SoftFloat C code, the examples,
the committed tests beyond their harness, and `vmlab-hv-lockstep` (outside
the scope). The unread recorder and graph passes are covered only by the
differential runs, which is weaker than a reading.

Not tested: more than one hart on a `SharedMemory`, the `lockstep` feature,
user mode, the hypervisor extension, vector and floating-point instructions
beyond the single random words of the first differential run, and
`run_until_store`.

## Findings, worst first

### F1. The trace cache of one hart can hold more than 2 GiB of host memory

- Where: `tcg/cpu.rs:5` (`TRACE_CACHE_LIMIT = 8_000_000`), `tcg/cpu.rs:388-432`
  (`cache_variants`) and `tcg/trace.rs:1725-1768` (`calculate_cache_units`).
- Fault: the limit on the trace cache counts units, and the host pays bytes.
  A unit is one scalar, one event, one binding or one constant. A binding
  owns the name of its global as a `String`, a constant counts as one unit
  plus a small term whatever its heap size, and each exit keeps frames of
  the interpreter. The scratch test measured about 270 bytes for each unit:
  a trace of 32 `addi` instructions costs about 0.7 MiB. The cache may hold
  8,000,000 units and 4096 entry PCs, and both limits are for each hart.
- Reach: any guest code, with `specialize` on. The guest needs only many
  distinct entry PCs. The scratch test ran one straight run of
  `addi x10, x10, 1` over 512 KiB of RAM once: after 143,360 guest steps
  the resident size of the process had grown by 2180 MiB, with 2972 traces in
  the cache, the largest of 2691 units, 4481 recordings and 1509 evictions.
  The growth stopped there, so the bound holds, but at that size. The same
  run was also slower than plain interpretation (about 200 guest steps per
  second on the build host).
- Severity: security against a hostile guest. Each hart of a guest can take
  about 2.2 GiB of host memory that no machine setting accounts for. It is a
  bounded amount and it is not a crash.
- Proof: test `the_trace_cache_stays_within_a_byte_budget` in
  `vmlab-hv-tcg/tests/hardening.rs`: a `FlatMemory` holding 6400
  `addi x10, x10, 1` and a jump back, `specialize` on, `run` until 6400
  steps are done. Read the resident pages from `/proc/self/statm` before and
  after and assert the growth is under a stated budget, for example 32 MiB.
  By the measured rate, the 200 traces of this run take about 146 MiB. A better form of the
  test asserts on a byte count that the cache keeps itself, once it has one.
  No time is measured.
- Confidence: high for the measurement. The split of the bytes between
  bindings, frames and constants was read from the code and not measured.
- Fixed by card #769. The cache keeps a count of bytes beside the count of
  units (`Trace::cache_bytes`, from the capacity of every table a trace
  owns), and a hart keeps at most `TRACE_CACHE_BYTES` (32 MiB) of it. One
  trace takes no further fragment past a quarter of that. The proof test is
  in its own file, `vmlab-hv-tcg/tests/trace_cache_bytes.rs`, because it
  counts live heap bytes through a global allocator and not resident pages.
  Over 3200 steps of the straight run the hart held 72.6 MB before the fix
  and holds under 48 MiB after it, and the cache's own count is within a
  factor of two of the heap the run holds.

### F2. A guest access outside the host's RAM is a host error and the hart cannot pass it

- Where: `tcg/lib.rs:647-696` (`memory_operation`: the `?` at `668`, `685`
  and `692`), `tcg/memory.rs:86-96` (`FlatMemory::range` returns
  `Error::Memory`), `tcg/trace.rs:1431-1438` and `1454-1458` (the same in a
  trace), `tcg/cpu.rs:184-206` (`step`).
- Fault: the Sail model decides which physical addresses are memory from its
  own region table (`zpma_regions`), which `Riscv64::new` leaves at the
  model's default. The `Memory` the host passes in is not consulted. When
  the model accepts an address that the `Memory` does not hold, the read or
  write returns `Error::Memory`, and that leaves `step` as `Err`. It is not
  turned into the load, store or instruction access fault that the guest's
  trap handler expects. The PC and `steps` do not move, so every later
  `step` returns the same error: the guest has stopped its own hart for good.
- Reach: any guest code, in any privilege mode that passes PMP. With a
  4 KiB `FlatMemory` at `0x8000_0000` the scratch test saw the error for:
  `ld` and `sd` at `0x8000_1000` and `0x8010_0000` (past the end), `ld` and
  `sd` at `0x8000_0ffc` (the last four bytes, so the access crosses the
  end), `ld` at `0x1000`, `ld` and `sd` at `0x1000_0000`, and the fetch
  after `jalr` to `0x8000_1000`. Addresses the model does not map (`0`,
  `0x4000_0000`, `0x1_0000_0000`) raise the guest fault as they should, and
  the CLINT at `0x200_0000` works.
- Severity: wrong result. A guest gets a dead hart where the architecture
  gives it a trap. It is security against a hostile guest only in that a
  guest can end its own emulation with one instruction. The host is not
  affected.
- Proof: test `an_access_past_host_ram_raises_a_guest_fault` in
  `vmlab-hv-tcg/tests/hardening.rs`: a 4 KiB `FlatMemory` at `0x8000_0000`
  holding `ld x10, 0(x8)`, `mtvec` set to `0x8000_0800`, `x8 = 0x8000_1000`.
  `step()` must return `Ok`, with the PC at `mtvec` and `mcause` 5. Today it
  returns `Err(physical access fault at 0x80001000)` and the PC stays at
  `0x8000_0000`. The same for `sd` (`mcause` 7), for `x8 = 0x8000_0ffc`, and
  for a `jalr` to `0x8000_1000` followed by a second `step` (`mcause` 1).
  Run each with `specialize` off and on.
- Confidence: high. The fix has two possible places, and the card should
  choose: build the model's region table from `Memory::ram_bounds`, or map
  `Error::Memory` to the model's access fault at the memory call.

### F3. Writing the `seed` CSR is a host error

- Where: `tcg/lib.rs:346-363` (`call`). The model's `zread_seed_csr` calls
  `zget_16_random_bits`, which the model declares and does not define.
- Fault: `call` provides two kinds of platform function, `zplat_term_write`
  and the `zriscv_` SoftFloat names. Every other name must be a function of
  the model, or the call fails with `Error::Undefined`. The model has one more
  function without a body, the entropy source of the Zkr extension. A guest
  that reads entropy the documented way gets a host error from `step`, and
  the hart cannot pass that instruction (as in F2).
- Reach: machine-mode guest code, one instruction: `csrrw x10, seed, x0`
  (`0x01501573`). A read with `csrrs` raises the illegal-instruction trap as
  the specification says, so only the legal access fails.
- Severity: wrong result, with the same dead hart as F2.
- Proof: test `the_seed_csr_returns_entropy` in
  `vmlab-hv-tcg/tests/hardening.rs`: a hart at `0x8000_0000` holding
  `0x01501573`. `step()` must return `Ok` and the PC must be `0x8000_0004`.
  Today it returns `Err(... zread_seed_csr IR instruction 5: undefined
  value: function zget_16_random_bits)`. A second test should list every
  function the model calls and does not define, and assert that `call`
  serves each one, so a model upgrade cannot add another silently. The list
  today is the SoftFloat names, `zplat_term_write` and `zget_16_random_bits`.
- Confidence: high.

### F4. An ordinary store by the same hart cancels its LR reservation, against the model's setting

- Where: `tcg/lib.rs:671` (`self.reservation = None` after every write) and
  `tcg/trace.rs:1460` (the same in a trace).
- Fault: the model carries the platform setting
  `zplat_reservation_invalidate_on_same_hart_store`, and its value in the
  committed model is `false`: a store by the hart that holds the reservation
  does not cancel it. The interpreter clears the reservation after every
  store anyway, to any address. So `lr.d`, then a store to another page, then
  `sc.d` fails here and succeeds on the Sail reference with the same
  configuration. The RISC-V specification allows an SC to fail, so a correct
  guest still works, but the hart does not behave as the model it is built
  from, and a lockstep comparison against the Sail emulator will report it.
- Reach: any guest code with a store between LR and SC.
- Severity: wrong result against the reference model. Low.
- Proof: test `a_same_hart_store_keeps_the_reservation` in
  `vmlab-hv-tcg/tests/hardening.rs`: `lr.d x10, (x8)`, `sd x12, 0(x13)`,
  `sc.d x11, x12, (x8)` with `x8 = 0x8000_1000` and `x13 = 0x8000_3000`.
  After three steps `x11` must be 0 and the doubleword at `0x8000_1000` must
  hold `x12`. Today `x11` is 1 and the memory is unchanged, with `specialize`
  off and on. If the project wants the stricter behaviour, the fix is to set
  the model's flag to `true` and let the model cancel, so the two agree.
- Confidence: medium. The value of the flag was read from the running model.
  That the Sail emulator succeeds in this case follows from the flag's
  meaning and was not run.

### F5. Jib text within the parser's nesting limits overflows the stack in `lower`

- Where: `sail/lib.rs:289-298` (`nested`, limit 128), `sail/lib.rs:507-521`
  and `529-540` (the suffix loops, limit 128 each), `ir/lib.rs:129`
  (`lower`) and `ir/lib.rs:435` (`fold`).
- Fault: the parser bounds two things separately: the nesting of
  expressions, and the number of `.field`, `is` and `as` suffixes on one
  operand. Each suffix adds one level to the expression tree without adding
  to the nesting count. A text with 120 nested struct literals whose operands
  each carry 120 suffixes is accepted and has a tree about 14,000 levels
  deep. `lower` clones and folds expressions by recursion, and the process
  dies with a stack overflow, which Rust turns into an abort.
- Reach: the Jib text given to `vmlab-hv-tcg-emit`. It comes from the pinned
  Sail toolchain, so this needs a changed build input. The cargo-fuzz target
  `vmlab-hv-sail-ir/fuzz/fuzz_targets/parse.rs` calls `format` on the parsed
  tree, which is also recursive, so the fuzzer can die on the same input.
- Severity: crash, of a build tool and of the fuzz target.
- Proof: test `deep_suffix_chains_are_refused` in `sail/lib.rs`: build
  `expr = "x"`, then 120 times `expr = "struct s { a = " + expr + ".a" * 120
  + " }"`, and parse `val f : (%unit) -> %unit` with
  `fn f(x) { y : %unit; y = <expr>; return = (); end; }`. `parse` must
  return an error. Today it returns `Ok` for this 31 KB text, and
  `vmlab_hv_tcg_ir::lower` on the result aborts the test process (seen:
  `thread ... has overflowed its stack`, SIGABRT).
- Confidence: high.

### F6. The emitter writes a model that the generated crate cannot load

- Where: `emit/main.rs:10` (`serde_json::to_vec`), `emit/main.rs:32` and
  `vmlab-hv-tcg-riscv64/src/lib.rs:176`
  (`serde_json::from_slice(&json).expect("generated model IR")`).
- Fault: writing JSON has no depth limit and reading it has one, 128 levels
  in `serde_json`. One field access is two JSON levels. An expression with 62
  chained field accesses, or 40 nested struct literals, is parsed, lowered
  and written, and `--check` also passes, because it compares bytes. The
  first call of `model()` then panics. The emitter's own test reads the
  payload back, but only for a shallow program.
- Reach: a model whose Jib has such an expression. The committed model has
  none. It needs a new model or toolchain version, and then every user of
  the crate panics at start.
- Severity: crash, found at the first test run after a model upgrade.
- Proof: test `the_payload_reads_back_for_a_deep_expression` in
  `emit/main.rs`: `generate` on `val f : (%unit) -> %unit` with
  `fn f(x) { y : %unit; y = x.a.a...; return = (); end; }` holding 64 `.a`,
  then unpack the payload and call `serde_json::from_slice::<Program>`. It
  must succeed, or `generate` must return an error. Today `generate`
  succeeds and the read fails with `recursion limit exceeded`. The scratch
  test saw the limit between 60 and 62 field accesses, and between 30 and 40
  struct literals.
- Confidence: high.

### F7. A loop that stores into its own code page never reuses a trace

- Where: `tcg/memory.rs:110-138` (`write` stamps a 4 KiB page,
  `code_stamp` reads the stamp), `tcg/record.rs:1074-1082` (a trace keeps
  the stamp of each fetch), `tcg/trace.rs:996-1008` (`code_valid`) and
  `1469-1476` (the check after each store), `tcg/cpu.rs:376-382` (a new
  recording replaces the oldest of four).
- Fault: a trace is valid while no byte of the pages it was fetched from has
  been written. That is correct, and it is why self-modifying code works
  (see below). The cost is that the test cannot tell a store to code from a
  store to data in the same page. A block that writes a variable in its own
  page makes its own trace invalid while it is recorded. The next entry
  misses, records again, and evicts. The specialiser then does more work
  than the plain interpreter for every instruction.
- Reach: any guest whose code and written data share a 4 KiB page: a small
  bare-metal image, a boot stub with its stack beside it, or a guest that
  does it on purpose.
- Severity: cosmetic, as the results are right. It is a large slowdown that
  the guest chooses. The scratch test ran the loop `sd x3, 0(x1)`,
  `addi x3, x3, 1`, `addi x16, x16, -1`, `bne x16, x0, -12` for 6000 steps:
  with `x1` in another page, 5656 trace hits and 6 recordings in 0.79 s.
  With `x1` in the code page, 0 hits, 1501 recordings, 1496 evictions and
  14.8 s.
- Proof: test `a_store_beside_the_code_keeps_the_trace` in
  `vmlab-hv-tcg/tests/specialized.rs`: that loop at `0x8000_0000` with
  `x1 = 0x8000_0800` and `x16 = 2000`, `run(6000)` with `specialize` on.
  Assert `specialization_stats.hits > 0` and
  `specialization_stats.recordings < 100`. Today hits is 0 and recordings is
  1501. No time is measured.
- Confidence: high. A fix needs a finer stamp (the bytes a trace fetched)
  or a fallback that stops recording a PC that keeps invalidating itself.

### F8. `read_csr` and `write_csr` take an index past 12 bits and use another CSR

- Where: `tcg/cpu.rs:170-181`.
- Fault: both build the CSR number with `Value::bits(12, csr)`, which keeps
  the low 12 bits of the `u16`. `read_csr(0x1300)` reads `mstatus` (`0x300`)
  and `write_csr(0x1340, 7)` writes `mscratch`. `set_gpr` refuses an index
  out of range, and these two do not.
- Reach: the host code that embeds the hart. Not the guest.
- Severity: cosmetic.
- Proof: test `a_csr_index_past_twelve_bits_is_refused` in
  `vmlab-hv-tcg/tests/hardening.rs`: `read_csr(0x1300)` and
  `write_csr(0x1340, 7)` must return `Err`, and `read_csr(0x340)` must still
  be 0. Today the first returns the value of `mstatus`, the second returns
  `Ok`, and `mscratch` reads 7.
- Confidence: high.

## What was checked and found sound

These are the places the card asked about first. No fault was found in them.

- Out-of-range guest addresses and host memory. `FlatMemory::range`
  (`tcg/memory.rs:86-96`) checks every access with checked arithmetic, and a
  trace reads and writes through the same call. The only unchecked indexing
  is `Packed::execute_unchecked` (`ir/native/packed.rs:312-376`), which
  indexes a trace's scratch array by slot numbers. `Trace::from_record`
  asserts `valid_slots` for every instruction against the array lengths
  (`tcg/trace.rs:973-982`), immediates are folded before that assertion, and
  the lease of card #9 restores the arrays on every exit. A guest value is
  never a slot number.
- Self-modifying code. A store inside a trace is followed by `code_valid`
  and leaves the trace when a fetched page changed (`tcg/trace.rs:1469`).
  Entry checks the stamps again. The scratch test rewrote an instruction
  later in the same straight-line block, in the same page and in the next
  page, for 14 rounds with different new instructions. The results matched
  the interpreter in every round.
- Decoding. 4000 random instruction words (random 32-bit words, random words
  under each major opcode, and random 16-bit compressed words), each run
  with up to three sets of register values that favour edge values and
  addresses at the borders of RAM. The interpreter and the specialiser
  agreed on every global and every byte of RAM. The only errors were those of
  F2. There was no panic.
- Wrong results in traces. 150 random straight-line programs of RV64IMA
  instructions (14 runs each with new register values and three step
  limits), 40 random loops (12 runs each, so that traces are promoted to
  loop traces), and 60 pairs of programs in supervisor mode under Sv39, where
  the host swapped the leaf PTE of the code page between runs with and
  without a write of `satp`. No difference from the interpreter in any run.
- Interrupts. 24 loops with the machine timer interrupt enabled, where the
  guest sets `mtimecmp` through the CLINT and the handler sets it again, run
  in pieces of 300, 17 and 5 steps. The interrupt arrived at the same
  instruction in both modes in every run (8 to 66 interrupts for each loop).
- Translation caches. The trace cache holds at most 4096 entry PCs, four
  variants each, and `TRACE_CACHE_LIMIT` units (`tcg/cpu.rs:388-432`). The
  decode cache is cleared at 4096 entries (`tcg/lib.rs:453`). A memo keeps
  128 results per function (`tcg/memo.rs:94`). The per-CSR and
  per-instruction function caches are bounded by the CSR space and the
  instruction list. Fragments are bounded per exit, in size and in depth
  (`tcg/trace.rs:1251`, `1276`, `1332-1336`). The bounds hold. F1 is about
  their size.
- LR/SC. `lr` and `sc` of both widths, an SC without an LR, an SC at another
  address, two SCs after one LR and a misaligned LR gave the same results in
  both modes, and each is a legal outcome. F4 is the one deviation.
- Cost of one instruction. The most expensive instruction seen took about
  63,000 IR operations, against a budget of 100,000,000.
- Platform functions. Every intrinsic and every expression operator that the
  committed model uses has an arm in `tcg/intrinsics.rs`. Every SoftFloat
  name the model calls is in the dispatch table of `build.rs`.

## Notes

These have no proving test, or are not faults.

- N1. A guest that makes the specialiser record at every step is slower
  than the plain interpreter. F1 and F7 are two ways to do it, and the timer
  interrupt runs showed a third, smaller one: 40 to 360 recordings for about
  2500 steps, about five for each interrupt taken. Nothing stops recording
  for an entry PC that keeps missing.
- N2. After `step` returns `Err`, the hart's state is whatever the model had
  written up to the failing operation, and `steps` and the clock are not
  advanced. Nothing documents whether the caller may continue. F2 and F3 are
  the two ways a guest gets there.
- N3. A trace run, a recording and a resumed continuation hold the
  `SharedMemory` transaction lock for their whole length
  (`tcg/trace.rs:672-673`, `1036-1037`, `tcg/continuation.rs:33-34`), up to
  256 guest instructions. Other harts wait for that time. It is correct. It
  was not measured with more than one hart.
- N4. `specialized_run` reads the environment variable
  `VMLAB_TCG_TRACE_PC` at every recording (`tcg/cpu.rs:356`), and
  `entry_valid` at every context mismatch (`tcg/trace.rs:1109`). With F7
  that is once per guest instruction.
- N5. The decode cache key holds the fetch address, the memory generation,
  the instruction bits and a copy of every global the decoder depends on
  (`tcg/lib.rs:442-448`, `tcg/decode.rs:91-96`). The instruction bits and
  the context already decide the result. The generation changes on every
  store anywhere in RAM, so each store makes all entries unreachable until
  the cache fills and is cleared, and the copy is taken twice for each
  miss. This costs time and no correctness.
- N6. `SpecializationStats::stop_reasons` (`tcg/cpu.rs:368-374`) keeps one
  string for each distinct IR operation that ended a recording. The model
  bounds the count (about 338,000 operations), and no guest was found that
  reaches many of them.
- N7. `execute_unchecked` is sound only while a trace is never entered again
  during its own run, because a second entry would find the scratch arrays
  taken and empty. No path does that today: a guard exit runs another
  `Trace` value or the interpreter. Nothing in the types prevents it.
- N8. `sail/lib.rs:269-281`: a decimal string escape of 128 or more becomes
  the Unicode character of that number, not the byte. `sail/lib.rs:354-370`:
  a list accepts a trailing comma. A type such as `%bv4294967295` is
  accepted, and `Interpreter::default_value` (`tcg/lib.rs:176`) would then
  allocate a 512 MiB mask. All three need a changed Jib text.
- N9. `LockstepMemory` (`tcg/lockstep.rs`) keeps the whole-memory
  reservation generation that card #9 replaced in `FlatMemory`. It is used
  only by the comparison tool, with one hart.
- N10. With the committed model, HTIF is off (`zhtif_tohost_base` is
  `None`), so the terminal buffer of card #9 has no guest path to it unless
  the host sets that global.
