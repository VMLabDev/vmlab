//! Windows of a boot (card C13b, #1144): the form of the boot lockstep of
//! DESIGN.md O4 that ends in under two hours for a whole Debian boot.
//!
//! A boot of our machine to agent ready writes the state of its CPU and its
//! RAM at points of the boot (`VMLAB_HV_A64_WINDOWS`, in `vmlab-hv-vmm`).
//! Each point is a window: QEMU starts from that state, and our CPU follows
//! its stream before every instruction, as in O4, until the window ends. The
//! leader starts at EL2 (`virtualization=on`) and runs a prelude from its
//! flash that writes every register of EL1 and EL0 and leaves with `ERET`,
//! so the first instruction that the leader runs at EL1 or EL0 is the first
//! instruction of the window.
//!
//! A window ends at its budget, at the first access outside RAM (a device:
//! the leader has none of ours, and our follower has only RAM), at `WFI`, at
//! `HVC` or `SMC`, at an interrupt (the leader has a GIC that nobody turned
//! on, so none comes), and at the first CPU difference.
//!
//! What a window does not prove: the moment and the handling of an
//! interrupt, every device and its DMA, and the code that runs only around
//! a device access. Those stay with the O4 run from the reset and with S1.
//! It proves the CPU on the instructions of the kernel and of user space
//! where the boot spends its time, in every phase of it.
//!
//! QEMU is a black box here (S6). Nothing in this module is part of a
//! product path.

use super::{Compared, Core, Vectors, apply_system, asm, compared, system_of};
use crate::state::State;
use crate::sysreg;

/// The first line of a window file.
const MAGIC: &str = "vmlab-a64-window 1";

/// Where the data of the prelude starts in its flash image.
pub const DATA: u64 = 0x1_0000;
/// The first bytes of RAM, where QEMU puts the device tree of a machine that
/// starts from its flash. The leader gets the RAM above them as a file, and
/// the prelude copies these from its flash image, at `LOW_IN_FLASH`.
pub const LOW: u64 = 0x10_0000;
const LOW_IN_FLASH: u64 = 0x10_0000;
/// Where RAM starts.
const RAM_BASE: u64 = 0x4000_0000;
const AT_X: u32 = 0;
const AT_SP: u32 = 248;
const AT_PC: u32 = 256;
const AT_CPSR: u32 = 264;
const AT_COUNTER: u32 = 272;
const AT_FPCR: u32 = 280;
const AT_FPSR: u32 = 288;
const AT_V: u32 = 512;
const AT_SYSTEM: u32 = 1024;

/// The encodings of the registers of EL2 and the others that the prelude
/// writes and that no table of ours has: op0, op1, CRn, CRm and op2 as the
/// 16 bits of `MRS` and `MSR`.
mod reg {
    pub const HCR_EL2: u16 = 0xe088;
    pub const CPTR_EL2: u16 = 0xe08a;
    pub const CNTHCTL_EL2: u16 = 0xe708;
    pub const CNTVOFF_EL2: u16 = 0xe703;
    pub const VPIDR_EL2: u16 = 0xe000;
    pub const VMPIDR_EL2: u16 = 0xe005;
    pub const SPSR_EL2: u16 = 0xe200;
    pub const ELR_EL2: u16 = 0xe201;
    pub const SP_EL1: u16 = 0xe208;
    pub const SP_EL0: u16 = 0xc208;
    pub const MIDR_EL1: u16 = 0xc000;
    pub const MPIDR_EL1: u16 = 0xc005;
    pub const CNTPCT_EL0: u16 = 0xdf01;
    pub const FPCR: u16 = 0xda20;
    pub const FPSR: u16 = 0xda21;
    pub const OSLAR_EL1: u16 = 0x8084;
    pub const DBGCLAIMSET_EL1: u16 = 0x83c6;
}

/// The state of our CPU at the start of a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    /// The instructions that the CPU had run.
    pub instructions: u64,
    /// The virtual count of the generic timer.
    pub counter: u64,
    pub core: Core,
    pub vectors: Vectors,
    /// The registers of `compared()`, by the names of the plugin API.
    pub system: Vec<(String, u64)>,
}

impl Start {
    pub fn of(state: &State, instructions: u64, counter: u64) -> Self {
        let list = compared();
        let values = system_of(state, &list);
        Self {
            instructions,
            counter,
            core: Core::of(state),
            vectors: Vectors::of(state),
            system: list
                .iter()
                .zip(values)
                .map(|(register, value)| (register.qemu.to_string(), value))
                .collect(),
        }
    }

    /// The text of a window file.
    pub fn to_text(&self) -> String {
        let mut text = format!(
            "{MAGIC}\ninstructions {}\ncounter {:x}\ncore",
            self.instructions, self.counter
        );
        for value in self.core.x {
            text.push_str(&format!(" {value:x}"));
        }
        text.push_str(&format!(
            " {:x} {:x} {:x}\nfp {:x} {:x}\nv",
            self.core.sp, self.core.pc, self.core.cpsr, self.vectors.fpcr, self.vectors.fpsr
        ));
        for value in self.vectors.v {
            text.push_str(&format!(" {value:x}"));
        }
        text.push('\n');
        for (name, value) in &self.system {
            text.push_str(&format!("sys {name} {value:x}\n"));
        }
        text
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(MAGIC) {
            return Err("not a window file".into());
        }
        let hex = |word: &str| u128::from_str_radix(word, 16).map_err(|e| format!("{word}: {e}"));
        let mut start = Self {
            instructions: 0,
            counter: 0,
            core: Core {
                x: [0; 31],
                sp: 0,
                pc: 0,
                cpsr: 0,
            },
            vectors: Vectors::default(),
            system: Vec::new(),
        };
        for line in lines {
            let mut words = line.split(' ');
            match words.next() {
                Some("instructions") => {
                    start.instructions = words
                        .next()
                        .ok_or("no count")?
                        .parse()
                        .map_err(|e| format!("{e}"))?;
                }
                Some("counter") => start.counter = hex(words.next().ok_or("no counter")?)? as u64,
                Some("core") => {
                    let values = words.map(hex).collect::<Result<Vec<_>, _>>()?;
                    if values.len() != 34 {
                        return Err("the core line has not 34 values".into());
                    }
                    for (n, value) in values[..31].iter().enumerate() {
                        start.core.x[n] = *value as u64;
                    }
                    start.core.sp = values[31] as u64;
                    start.core.pc = values[32] as u64;
                    start.core.cpsr = values[33] as u32;
                }
                Some("fp") => {
                    let values = words.map(hex).collect::<Result<Vec<_>, _>>()?;
                    if values.len() != 2 {
                        return Err("the fp line has not 2 values".into());
                    }
                    start.vectors.fpcr = values[0] as u32;
                    start.vectors.fpsr = values[1] as u32;
                }
                Some("v") => {
                    let values = words.map(hex).collect::<Result<Vec<_>, _>>()?;
                    if values.len() != 32 {
                        return Err("the v line has not 32 values".into());
                    }
                    start.vectors.v.copy_from_slice(&values);
                }
                Some("sys") => {
                    let name = words.next().ok_or("no name")?;
                    let value = hex(words.next().ok_or("no value")?)? as u64;
                    start.system.push((name.to_string(), value));
                }
                Some("") | None => {}
                Some(other) => return Err(format!("unknown line {other}")),
            }
        }
        Ok(start)
    }

    /// The values of `list`, from the registers of the file. `None`: the
    /// file has no value for one of them.
    pub fn values(&self, list: &[Compared]) -> Option<Vec<u64>> {
        list.iter()
            .map(|register| {
                self.system
                    .iter()
                    .find(|(name, _)| name == register.qemu)
                    .map(|(_, value)| *value)
            })
            .collect()
    }

    /// Our CPU at the start of the window, as the CPU of the lockstep
    /// (`QEMU_CORTEX_A53`, whose ID registers are those of the leader).
    /// `None`: the file misses a register, or its CPSR names no state.
    pub fn state(&self) -> Option<State> {
        let mut state = State::reset_as(self.core.pc, &sysreg::QEMU_CORTEX_A53);
        let list = compared();
        let values = self.values(&list)?;
        apply_system(&mut state, &list, &values);
        if !self.core.apply(&mut state) {
            return None;
        }
        self.vectors.apply(&mut state);
        Some(state)
    }

    /// The exception level of the window.
    pub fn el(&self) -> u32 {
        self.core.cpsr >> 2 & 3
    }
}

/// `LDR Xt, [Xn, #offset]`, the offset a multiple of 8 below 32768.
fn ldr(rt: u32, rn: u32, offset: u32) -> u32 {
    debug_assert!(offset.is_multiple_of(8) && offset < 32768);
    0xf940_0000 | (offset / 8) << 10 | (rn & 31) << 5 | (rt & 31)
}

/// `LDR Qt, [Xn, #offset]`, the offset a multiple of 16.
fn ldr_q(rt: u32, rn: u32, offset: u32) -> u32 {
    debug_assert!(offset.is_multiple_of(16) && offset < 65536);
    0x3dc0_0000 | (offset / 16) << 10 | (rn & 31) << 5 | (rt & 31)
}

/// `LDR Xt, [Xn], #8`.
fn ldr_next(rt: u32, rn: u32) -> u32 {
    0xf840_0400 | 8 << 12 | (rn & 31) << 5 | (rt & 31)
}

/// `STR Xt, [Xn], #8`.
fn str_next(rt: u32, rn: u32) -> u32 {
    0xf800_0400 | 8 << 12 | (rn & 31) << 5 | (rt & 31)
}

/// `SUBS Xd, Xn, #1`.
fn subs_one(rd: u32, rn: u32) -> u32 {
    0xf100_0400 | (rn & 31) << 5 | (rd & 31)
}

/// `B.NE` to an offset in instructions.
fn b_ne(offset: i32) -> u32 {
    0x5400_0001 | (offset as u32 & 0x7_ffff) << 5
}

/// `SUB Xd, Xn, Xm`.
fn sub(rd: u32, rn: u32, rm: u32) -> u32 {
    0xcb00_0000 | (rm & 31) << 16 | (rn & 31) << 5 | (rd & 31)
}

/// Where the prelude writes one register of `compared()`, and what it
/// writes there: the register itself, or the one that sets it.
fn target(register: &Compared, value: u64) -> Option<(u16, u64)> {
    match (register.qemu, register.row) {
        ("SP_EL1", None) => Some((reg::SP_EL1, value)),
        // OSLSR_EL1 has the lock in bit 1, and OSLAR_EL1 sets it from bit 0.
        ("OSLSR_EL1", _) => Some((reg::OSLAR_EL1, value >> 1 & 1)),
        // A read of DBGCLAIMCLR_EL1 gives the claim tags that are set.
        ("DBGCLAIMCLR_EL1", _) => Some((reg::DBGCLAIMSET_EL1, value)),
        // The status bit of a timer control is a function of the time.
        ("CNTV_CTL_EL0" | "CNTP_CTL_EL0", Some(row)) => {
            Some((sysreg::ROWS[row].encoding, value & 3))
        }
        (_, Some(row)) => Some((sysreg::ROWS[row].encoding, value)),
        (_, None) => None,
    }
}

/// The flash image of the leader: the prelude at 0, its data at `DATA`,
/// and `low`, the first `LOW` bytes of our RAM, at `LOW_IN_FLASH`. `None`:
/// the file misses a register of `compared()`.
pub fn prelude(start: &Start, low: &[u8]) -> Option<Vec<u8>> {
    let list = compared();
    let values = start.values(&list)?;
    let mut words = Vec::new();
    // The first bytes of RAM, eight at a time.
    words.extend(asm::load(2, LOW_IN_FLASH));
    words.extend(asm::load(3, RAM_BASE));
    words.extend(asm::load(4, LOW / 8));
    words.extend([ldr_next(0, 2), str_next(0, 3), subs_one(4, 4), b_ne(-3)]);
    // X1 points at the data.
    words.extend(asm::load(1, DATA));
    // EL1 in AArch64 with no trap to EL2, the FP registers open to EL2,
    // the physical counter and timer open to EL1, and the IDs of the CPU.
    for (register, value) in [
        (reg::HCR_EL2, 1u64 << 31),
        (reg::CPTR_EL2, 0x33ff),
        (reg::CNTHCTL_EL2, 3),
    ] {
        words.extend(asm::load(0, value));
        words.push(asm::msr(register, 0));
    }
    words.push(asm::mrs(0, reg::MIDR_EL1));
    words.push(asm::msr(reg::VPIDR_EL2, 0));
    words.push(asm::mrs(0, reg::MPIDR_EL1));
    words.push(asm::msr(reg::VMPIDR_EL2, 0));
    words.push(asm::ISB);
    // The virtual count starts at ours: CNTVOFF_EL2 is the physical count
    // now less it.
    words.push(ldr(2, 1, AT_COUNTER));
    words.push(asm::mrs(0, reg::CNTPCT_EL0));
    words.push(sub(0, 0, 2));
    words.push(asm::msr(reg::CNTVOFF_EL2, 0));
    let mut data = vec![0u8; AT_SYSTEM as usize + 8 * list.len()];
    let mut put = |at: u32, value: u64| {
        data[at as usize..at as usize + 8].copy_from_slice(&value.to_le_bytes());
    };
    for (n, value) in start.core.x.iter().enumerate() {
        put(AT_X + 8 * n as u32, *value);
    }
    put(AT_SP, start.core.sp);
    put(AT_PC, start.core.pc);
    put(AT_CPSR, u64::from(start.core.cpsr));
    put(AT_COUNTER, start.counter);
    put(AT_FPCR, u64::from(start.vectors.fpcr));
    put(AT_FPSR, u64::from(start.vectors.fpsr));
    for (n, register) in list.iter().enumerate() {
        let Some((encoding, value)) = target(register, values[n]) else {
            continue;
        };
        let at = AT_SYSTEM + 8 * n as u32;
        put(at, value);
        words.push(ldr(0, 1, at));
        words.push(asm::msr(encoding, 0));
    }
    // The stack pointer in use is the one of the core registers.
    let banked = if start.el() == 1 && start.core.cpsr & 1 != 0 {
        reg::SP_EL1
    } else {
        reg::SP_EL0
    };
    words.push(ldr(0, 1, AT_SP));
    words.push(asm::msr(banked, 0));
    words.push(ldr(0, 1, AT_CPSR));
    words.push(asm::msr(reg::SPSR_EL2, 0));
    words.push(ldr(0, 1, AT_PC));
    words.push(asm::msr(reg::ELR_EL2, 0));
    words.push(ldr(0, 1, AT_FPCR));
    words.push(asm::msr(reg::FPCR, 0));
    words.push(ldr(0, 1, AT_FPSR));
    words.push(asm::msr(reg::FPSR, 0));
    for n in 0..32 {
        words.push(ldr_q(n, 1, AT_V + 16 * n));
    }
    // SP_EL2 takes the data, and X0 to X30 come from it.
    words.push(asm::add(31, 1, 0));
    for n in (0..31).rev() {
        words.push(ldr(n, 31, AT_X + 8 * n));
    }
    words.push(asm::ISB);
    words.push(asm::ERET);
    let mut image = asm::bytes(&words);
    assert!(image.len() as u64 <= DATA, "the prelude outgrew its space");
    image.resize(DATA as usize, 0);
    for (n, value) in start.vectors.v.iter().enumerate() {
        let at = AT_V as usize + 16 * n;
        data[at..at + 16].copy_from_slice(&value.to_le_bytes());
    }
    image.extend(data);
    image.resize(LOW_IN_FLASH as usize, 0);
    let mut first = low.to_vec();
    first.resize(LOW as usize, 0);
    image.extend(first);
    Some(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::decode::{Record, decode};

    fn sample() -> State {
        let mut state = State::reset_as(0xffff_8000_1234_5678, &sysreg::QEMU_CORTEX_A53);
        for n in 0..31 {
            state.set_x(n, 0x1111_0000 + n as u64);
        }
        // EL1 with SP_EL1, IRQs open.
        assert!(state.set_cpsr(0x0000_0345 | 0x6000_0000));
        state.set_sp(0xffff_8000_0abc_d000);
        for n in 0..32 {
            state.set_v(n, (n as u128) << 64 | 0x77);
        }
        state.set_fpcr(0x0300_0000);
        state.set_fpsr(0x10);
        let list = compared();
        let mut values = system_of(&state, &list);
        for (register, value) in list.iter().zip(values.iter_mut()) {
            match register.qemu {
                "TTBR1_EL1" => *value = 0x4123_4000,
                "VBAR_EL1" => *value = 0xffff_8000_1000_0800,
                "SP_EL0" => *value = 0x0000_ffff_f000_0000,
                _ => {}
            }
        }
        apply_system(&mut state, &list, &values);
        state.set_sp(0xffff_8000_0abc_d000);
        state
    }

    #[test]
    fn a_window_file_gives_back_the_state_that_wrote_it() {
        let state = sample();
        let start = Start::of(&state, 12_345_678_901, 0xabcdef);
        let text = start.to_text();
        let parsed = Start::parse(&text).expect("the file parses");
        assert_eq!(parsed, start);
        let back = parsed.state().expect("the state builds");
        assert_eq!(Core::of(&back), Core::of(&state));
        assert_eq!(Vectors::of(&back), Vectors::of(&state));
        let list = compared();
        assert_eq!(system_of(&back, &list), system_of(&state, &list));
        assert_eq!(parsed.el(), 1);
        assert!(Start::parse("something else").is_err());
    }

    #[test]
    fn the_prelude_is_instructions_that_end_in_eret_and_data_that_holds_the_state() {
        let start = Start::of(&sample(), 1, 0x99);
        let image = prelude(&start, &[0x5a; 16]).expect("the prelude builds");
        let word = |at: usize| u32::from_le_bytes(image[at..at + 4].try_into().unwrap());
        let mut code = Vec::new();
        let mut at = 0;
        while word(at) != asm::ERET {
            code.push(word(at));
            at += 4;
        }
        assert!(at < DATA as usize);
        for word in &code {
            assert!(decode(*word).is_some(), "{word:08x} decodes");
        }
        let records: Vec<Record> = code
            .iter()
            .map(|word| decode(*word).unwrap().record)
            .collect();
        assert!(records.contains(&Record::MSR));
        assert!(records.contains(&Record::LDRQui));
        assert!(records.contains(&Record::LDRXui));
        assert_eq!(code.last(), Some(&asm::ISB));
        // The data: the core registers and the vectors at their places.
        let data = |offset: u32| {
            let at = DATA as usize + offset as usize;
            u64::from_le_bytes(image[at..at + 8].try_into().unwrap())
        };
        assert_eq!(data(AT_X + 8 * 7), 0x1111_0007);
        assert_eq!(data(AT_PC), 0xffff_8000_1234_5678);
        assert_eq!(data(AT_SP), 0xffff_8000_0abc_d000);
        assert_eq!(data(AT_CPSR), 0x6000_0345);
        assert_eq!(data(AT_COUNTER), 0x99);
        assert_eq!(data(AT_V + 16 * 3 + 8), 3);
        // The last loads fill X0 to X30 from the stack pointer of EL2, X0
        // last, and SP_EL1 takes the stack pointer of the core.
        assert_eq!(code[code.len() - 2], ldr(0, 31, AT_X));
        assert!(
            code.windows(2)
                .any(|pair| pair == [ldr(0, 1, AT_SP), asm::msr(reg::SP_EL1, 0)])
        );
        assert_eq!(sub(0, 0, 2), 0xcb02_0000);
        assert_eq!(ldr(2, 1, 272), 0xf940_8822);
        assert_eq!(ldr_q(5, 1, 512), 0x3dc0_8025);
        // The copy of the first bytes of RAM, and those bytes in the image.
        assert!(
            code.windows(4)
                .any(|four| four == [ldr_next(0, 2), str_next(0, 3), subs_one(4, 4), b_ne(-3)])
        );
        assert_eq!(ldr_next(0, 2), 0xf840_8440);
        assert_eq!(str_next(0, 3), 0xf800_8460);
        assert_eq!(subs_one(4, 4), 0xf100_0484);
        assert_eq!(b_ne(-3), 0x54ff_ffa1);
        assert_eq!(image.len() as u64, LOW_IN_FLASH + LOW);
        assert_eq!(image[LOW_IN_FLASH as usize + 15], 0x5a);
        assert_eq!(image[LOW_IN_FLASH as usize + 16], 0);
    }

    #[test]
    fn the_registers_without_an_msr_of_their_own_are_written_through_the_ones_that_set_them() {
        let list = compared();
        let find = |name: &str| *list.iter().find(|r| r.qemu == name).unwrap();
        assert_eq!(target(&find("OSLSR_EL1"), 0xa), Some((reg::OSLAR_EL1, 1)));
        assert_eq!(target(&find("OSLSR_EL1"), 0x8), Some((reg::OSLAR_EL1, 0)));
        assert_eq!(
            target(&find("DBGCLAIMCLR_EL1"), 3),
            Some((reg::DBGCLAIMSET_EL1, 3))
        );
        assert_eq!(target(&find("SP_EL1"), 5), Some((reg::SP_EL1, 5)));
        assert_eq!(
            target(&find("CNTV_CTL_EL0"), 7).map(|(_, value)| value),
            Some(3)
        );
        assert_eq!(
            target(&find("TTBR0_EL1"), 9),
            Some((asm::reg::TTBR0_EL1, 9))
        );
    }
}
