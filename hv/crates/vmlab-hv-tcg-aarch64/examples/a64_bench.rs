//! Run a flat aarch64 image of `bench/a64` on the interpreter loop and time
//! it (card C5, DESIGN.md M6).
//!
//! The image is loaded at `LOAD` in 16 MiB of RAM at the base of the `virt`
//! machine, and runs from its first word. It ends with the semihosting call
//! `SYS_EXIT`: `hlt #0xf000` with W0 = 0x18 and X1 at a block of the reason
//! and the exit code. This CPU has no halting debug, so `HLT` takes the
//! Undefined Instruction exception. The vector holds an `hvc`, which stops
//! the loop, and that is the end of the measurement. The count leaves out
//! the `hlt` and the `hvc`, so it is the count of the instructions in front
//! of the `hlt`. The same image runs on QEMU, which ends with that exit
//! code.
//!
//! Usage: `a64_bench IMAGE MAX_INSTRUCTIONS`. It prints one line of JSON.
//! The time holds the translation of the blocks: nothing is warmed first.

use std::time::Instant;
use vmlab_hv_tcg::{FlatMemory, Memory};
use vmlab_hv_tcg_aarch64::exception::{Cause, Exception};
use vmlab_hv_tcg_aarch64::sysreg::reg;
use vmlab_hv_tcg_aarch64::{Cpu, State, Stop};

/// The RAM of QEMU's `virt` machine starts here.
const RAM_BASE: u64 = 0x4000_0000;
const RAM_BYTES: usize = 16 * 1024 * 1024;
/// `bench/a64/link.ld`.
const LOAD: u64 = 0x4020_0000;
/// The vector base of the run, in RAM below the image, and the vector of a
/// synchronous exception from the current level with its own stack pointer.
const VECTORS: u64 = 0x4010_0000;
const SYNCHRONOUS: u64 = VECTORS + 0x200;
/// `hvc #0` and `hlt #0xf000`.
const HVC: u32 = 0xd400_0002;
const HLT: u32 = 0xd45e_0000;
const SYS_EXIT: u64 = 0x18;
const APPLICATION_EXIT: u64 = 0x2_0026;

fn word(memory: &mut FlatMemory, address: u64) -> Result<u64, Box<dyn std::error::Error>> {
    let mut bytes = [0u8; 8];
    memory.read(address, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [image, limit] = arguments.as_slice() else {
        return Err("usage: a64_bench IMAGE MAX_INSTRUCTIONS".into());
    };
    let limit: u64 = limit.parse()?;
    let image = std::fs::read(image)?;
    let mut memory = FlatMemory::new(RAM_BASE, vec![0; RAM_BYTES]);
    memory.write(LOAD, &image)?;
    memory.write(SYNCHRONOUS, &HVC.to_le_bytes())?;
    let mut cpu = Cpu::new(memory, State::reset(LOAD));
    if let Some(error) = &cpu.jit_error {
        return Err(format!("VMLAB_HV_JIT: {error}").into());
    }
    cpu.state.system.regs[reg::VBAR_EL1] = VECTORS;
    let start = Instant::now();
    let stop = cpu.run(limit);
    let seconds = start.elapsed().as_secs_f64();
    let pc = cpu.state.pc();
    match stop {
        Stop::Hypercall { pc: at, .. } if at == SYNCHRONOUS => {}
        Stop::Budget => return Err(format!("instruction limit at PC {pc:#x}").into()),
        other => return Err(format!("the image stopped wrong: {other:?}").into()),
    }
    // The one exception of the run is the Undefined Instruction exception
    // of the `hlt`.
    let Some(taken) = cpu.taken else {
        return Err("the vector ran without an exception".into());
    };
    let mut at = [0u8; 4];
    cpu.memory.read(taken.from, &mut at)?;
    if cpu.exceptions != 1
        || taken.cause != Cause::Exception(Exception::Undefined)
        || u32::from_le_bytes(at) != HLT
    {
        return Err(
            format!("the image stopped at an exception that is not its HLT: {taken:?}").into(),
        );
    }
    let pc = taken.from;
    // Without the `hlt` and the `hvc` of the vector.
    let instructions = cpu.instructions - 2;
    if cpu.state.x(0) & 0xffff_ffff != SYS_EXIT {
        return Err(format!("HLT at {pc:#x} is not SYS_EXIT: {:?}", cpu.state).into());
    }
    let block = cpu.state.x(1);
    let reason = word(&mut cpu.memory, block)?;
    let code = word(&mut cpu.memory, block + 8)?;
    if reason != APPLICATION_EXIT {
        return Err(format!("the exit reason is {reason:#x}").into());
    }
    let mips = instructions as f64 / seconds / 1e6;
    // With `VMLAB_HV_JIT=1` (3e DESIGN.md J1): the blocks and instructions
    // that ran in the interpreter (H2b), and what the emitter made. J4: the
    // exits of compiled blocks to the next block: through a linked jump, a
    // hit of the jump cache, or to the loop (`unlinked`, `lookups`), and the
    // runs that the loop entered.
    let jit = cpu.jit_counts();
    println!(
        "{{\"instructions\": {}, \"seconds\": {seconds:.9}, \"mips\": {mips:.6}, \
         \"translations\": {}, \"exit_code\": {code}, \
         \"jit\": {}, \"interpreted_blocks\": {}, \"interpreted_instructions\": {}, \
         \"compiled_blocks\": {}, \"refused_blocks\": {}, \"code_bytes\": {}, \
         \"compile_ns\": {}, \"chaining\": {}, \"chained\": {}, \"jump_hits\": {}, \
         \"unlinked\": {}, \"lookups\": {}, \"entered\": {}, \"links\": {}, \
         \"unlinks\": {}, \"fills\": {}, \"clears\": {}, \"tlb_accesses\": {}, \"tlb_misses\": {}, \
         \"fused\": {}, \"lazy\": {}, \
         \"x2\": {}, \"x3\": {}, \"x4\": {}, \"x5\": {}}}",
        instructions,
        cpu.translations,
        cpu.jit_enabled(),
        jit.interpreted_runs,
        jit.interpreted_instructions,
        jit.compiled,
        jit.refused,
        jit.bytes,
        jit.compile_ns,
        cpu.chaining_enabled(),
        jit.chained.saturating_sub(jit.unlinked),
        jit.jump_hits,
        jit.unlinked,
        jit.lookups,
        jit.compiled_runs,
        jit.links,
        jit.unlinks,
        jit.fills,
        jit.clears,
        jit.tlb_accesses,
        jit.tlb_misses,
        jit.fused,
        jit.lazy,
        cpu.state.x(2),
        cpu.state.x(3),
        cpu.state.x(4),
        cpu.state.x(5),
    );
    if code != 0 {
        return Err(format!("the guest failed with exit code {code}").into());
    }
    Ok(())
}
