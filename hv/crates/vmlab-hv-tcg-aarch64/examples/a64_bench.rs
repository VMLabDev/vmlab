//! Run a flat aarch64 image of `bench/a64` on the interpreter loop and time
//! it (card C5, DESIGN.md M6).
//!
//! The image is loaded at `LOAD` in 16 MiB of RAM at the base of the `virt`
//! machine, and runs from its first word. It ends with the semihosting call
//! `SYS_EXIT`: `hlt #0xf000` with W0 = 0x18 and X1 at a block of the reason
//! and the exit code. The loop has no lifter for `HLT` and stops in front of
//! it, which is the end of the measurement. The same image runs on QEMU,
//! which ends with that exit code.
//!
//! Usage: `a64_bench IMAGE MAX_INSTRUCTIONS`. It prints one line of JSON.
//! The time holds the translation of the blocks: nothing is warmed first.

use std::time::Instant;
use vmlab_hv_tcg::{FlatMemory, Memory};
use vmlab_hv_tcg_aarch64::{Cpu, Record, State, Stop};

/// The RAM of QEMU's `virt` machine starts here.
const RAM_BASE: u64 = 0x4000_0000;
const RAM_BYTES: usize = 16 * 1024 * 1024;
/// `bench/a64/link.ld`.
const LOAD: u64 = 0x4020_0000;
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
    let mut cpu = Cpu::new(memory, State::reset(LOAD));
    let start = Instant::now();
    let stop = cpu.run(limit);
    let seconds = start.elapsed().as_secs_f64();
    let pc = cpu.state.pc();
    match stop {
        Stop::Unimplemented {
            record: Record::HLT,
            ..
        } => {}
        Stop::Budget => return Err(format!("instruction limit at PC {pc:#x}").into()),
        other => return Err(format!("the image stopped wrong: {other:?}").into()),
    }
    if cpu.state.x(0) & 0xffff_ffff != SYS_EXIT {
        return Err(format!("HLT at {pc:#x} is not SYS_EXIT: {:?}", cpu.state).into());
    }
    let block = cpu.state.x(1);
    let reason = word(&mut cpu.memory, block)?;
    let code = word(&mut cpu.memory, block + 8)?;
    if reason != APPLICATION_EXIT {
        return Err(format!("the exit reason is {reason:#x}").into());
    }
    let mips = cpu.instructions as f64 / seconds / 1e6;
    println!(
        "{{\"instructions\": {}, \"seconds\": {seconds:.9}, \"mips\": {mips:.6}, \
         \"translations\": {}, \"exit_code\": {code}, \
         \"x2\": {}, \"x3\": {}, \"x4\": {}, \"x5\": {}}}",
        cpu.instructions,
        cpu.translations,
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
