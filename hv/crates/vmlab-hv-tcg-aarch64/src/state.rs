//! The register file of an aarch64 CPU as numbered slots (R11), and the
//! state that is not in a slot.
//!
//! Sources read: none beyond the public register names. The layout is ours.
//! The system registers are in `sysreg.rs`.

use crate::sysreg::{CpuModel, System, V8_0, reg};
use vmlab_hv_tcg_ir::native::ScalarType;
use vmlab_hv_tcg_ir::native::regfile::{RegisterFile, Slot};

/// Slot numbers. A slot enters a block as the graph input of its number.
pub mod slot {
    /// X0 to X30 are the slots 0 to 30.
    pub const X0: usize = 0;
    pub const X30: usize = 30;
    /// The stack pointer of each exception level. EL2 has a slot so that a
    /// later machine with EL2 changes no numbering (DESIGN.md G4).
    pub const SP_EL0: usize = 31;
    pub const SP_EL1: usize = 32;
    pub const SP_EL2: usize = 33;
    pub const PC: usize = 34;
    /// The flags are four slots of one bit (G3c).
    pub const N: usize = 35;
    pub const Z: usize = 36;
    pub const C: usize = 37;
    pub const V: usize = 38;
    /// V0 to V31 are the slots 39 to 70.
    pub const V0: usize = 39;
    pub const FPCR: usize = 71;
    pub const FPSR: usize = 72;
    pub const COUNT: usize = 73;
}

const fn bits(name: &'static str, width: u16) -> Slot {
    Slot {
        name,
        ty: ScalarType::Bits(width),
    }
}

static SLOTS: [Slot; slot::COUNT] = [
    bits("x0", 64),
    bits("x1", 64),
    bits("x2", 64),
    bits("x3", 64),
    bits("x4", 64),
    bits("x5", 64),
    bits("x6", 64),
    bits("x7", 64),
    bits("x8", 64),
    bits("x9", 64),
    bits("x10", 64),
    bits("x11", 64),
    bits("x12", 64),
    bits("x13", 64),
    bits("x14", 64),
    bits("x15", 64),
    bits("x16", 64),
    bits("x17", 64),
    bits("x18", 64),
    bits("x19", 64),
    bits("x20", 64),
    bits("x21", 64),
    bits("x22", 64),
    bits("x23", 64),
    bits("x24", 64),
    bits("x25", 64),
    bits("x26", 64),
    bits("x27", 64),
    bits("x28", 64),
    bits("x29", 64),
    bits("x30", 64),
    bits("sp_el0", 64),
    bits("sp_el1", 64),
    bits("sp_el2", 64),
    bits("pc", 64),
    bits("n", 1),
    bits("z", 1),
    bits("c", 1),
    bits("v", 1),
    bits("v0", 128),
    bits("v1", 128),
    bits("v2", 128),
    bits("v3", 128),
    bits("v4", 128),
    bits("v5", 128),
    bits("v6", 128),
    bits("v7", 128),
    bits("v8", 128),
    bits("v9", 128),
    bits("v10", 128),
    bits("v11", 128),
    bits("v12", 128),
    bits("v13", 128),
    bits("v14", 128),
    bits("v15", 128),
    bits("v16", 128),
    bits("v17", 128),
    bits("v18", 128),
    bits("v19", 128),
    bits("v20", 128),
    bits("v21", 128),
    bits("v22", 128),
    bits("v23", 128),
    bits("v24", 128),
    bits("v25", 128),
    bits("v26", 128),
    bits("v27", 128),
    bits("v28", 128),
    bits("v29", 128),
    bits("v30", 128),
    bits("v31", 128),
    bits("fpcr", 32),
    bits("fpsr", 32),
];

/// The register file that every aarch64 block reads and writes.
pub static REGISTER_FILE: RegisterFile = RegisterFile { slots: &SLOTS };

/// What decides how a block is lifted and is not a slot: a block is cached
/// under its PC and this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Context {
    /// The exception level, 0 to 2. EL3 is never built (G4).
    pub el: u8,
    /// PSTATE.SP: the stack pointer of the current level, not SP_EL0.
    pub sp_sel: bool,
    /// FP and AdvSIMD instructions run at this level: CPACR_EL1.FPEN allows
    /// them. Otherwise each one traps.
    pub fp: bool,
    /// TCR.TBI0 and TBI1 of the regime as the bits 0 and 1: a branch to a
    /// register drops the top byte of its target in the half that has its
    /// bit set (card C8).
    pub tbi: u8,
}

impl Context {
    /// The slot that `SP` names in this context.
    pub fn sp_slot(self) -> usize {
        match (self.sp_sel, self.el) {
            (false, _) | (true, 0) => slot::SP_EL0,
            (true, 1) => slot::SP_EL1,
            (true, _) => slot::SP_EL2,
        }
    }
}

/// The state of one CPU. Every slot is a `u128`, so that the slot executor
/// copies values in and out without a case per type. A value is always
/// inside the width of its slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub slots: [u128; slot::COUNT],
    pub context: Context,
    /// PSTATE.D, A, I and F as the bits 3 to 0.
    pub daif: u8,
    /// PSTATE.IL: the next instruction takes the Illegal Execution State
    /// exception.
    pub il: bool,
    /// Bit 13 of PSTATE in the layout of an SPSR. It does nothing on this
    /// CPU. QEMU's `cortex-a53` takes it from SPSR_EL1 at an exception return
    /// to a state that does not exist, shows it in CPSR and saves it at the
    /// next exception (the probe, `tests/qemu-cortex-a53-sysregs.txt`). We
    /// keep it so that our PSTATE is QEMU's (DESIGN.md P2, U1).
    pub pstate13: bool,
    /// The system registers.
    pub system: System,
}

impl Default for State {
    fn default() -> Self {
        Self::reset(0)
    }
}

impl State {
    /// The state after a reset into EL1 with SP_EL1 and every interrupt
    /// masked, as the `virt` machine starts its firmware (DESIGN.md G4, V2).
    pub fn reset(pc: u64) -> Self {
        Self::reset_as(pc, &V8_0)
    }

    /// As `reset`, for the CPU that `model` describes.
    pub fn reset_as(pc: u64, model: &'static CpuModel) -> Self {
        let mut state = Self {
            slots: [0; slot::COUNT],
            context: Context {
                el: 1,
                sp_sel: true,
                fp: false,
                tbi: 0,
            },
            daif: 0xf,
            il: false,
            pstate13: false,
            system: System::reset(model),
        };
        state.set_pc(pc);
        state.refresh();
        state
    }

    /// Bring the part of the context that follows from the system registers
    /// up to date. Whoever changes the level, CPACR_EL1 or TCR_EL1 calls
    /// this.
    pub fn refresh(&mut self) {
        let fpen = (self.system.regs[reg::CPACR_EL1] >> 20) & 3;
        self.context.fp = fpen == 3 || (fpen == 1 && self.context.el != 0);
        self.context.tbi = (self.system.regs[reg::TCR_EL1] >> 37) as u8 & 3;
    }

    /// `address` as a PC: with TCR.TBI of its half set, the top byte is the
    /// bit 55 repeated.
    pub fn untagged(&self, address: u64) -> u64 {
        let upper = address & (1 << 55) != 0;
        if self.context.tbi & (1 << u8::from(upper)) == 0 {
            address
        } else if upper {
            address | 0xff << 56
        } else {
            address & !(0xff << 56)
        }
    }

    /// The width of a physical address, from ID_AA64MMFR0_EL1.PARange.
    pub fn physical_address_bits(&self) -> u32 {
        let range = self.system.regs[crate::sysreg::index("ID_AA64MMFR0_EL1")] & 15;
        [32, 36, 40, 42, 44, 48, 52]
            .get(range as usize)
            .copied()
            .unwrap_or(48)
    }

    /// Set CPACR_EL1.FPEN so that FP and AdvSIMD run at EL0 and EL1.
    pub fn enable_fp(&mut self) {
        self.system.regs[reg::CPACR_EL1] |= 3 << 20;
        self.refresh();
    }

    /// X0 to X30. Register 31 is not an X register: use `sp` or zero.
    pub fn x(&self, n: usize) -> u64 {
        assert!(n <= slot::X30, "X{n} does not exist");
        self.slots[slot::X0 + n] as u64
    }

    pub fn set_x(&mut self, n: usize, value: u64) {
        assert!(n <= slot::X30, "X{n} does not exist");
        self.slots[slot::X0 + n] = value.into();
    }

    /// The stack pointer that the current context selects.
    pub fn sp(&self) -> u64 {
        self.slots[self.context.sp_slot()] as u64
    }

    pub fn set_sp(&mut self, value: u64) {
        self.slots[self.context.sp_slot()] = value.into();
    }

    pub fn pc(&self) -> u64 {
        self.slots[slot::PC] as u64
    }

    pub fn set_pc(&mut self, value: u64) {
        self.slots[slot::PC] = value.into();
    }

    /// The whole of V0 to V31.
    pub fn v(&self, n: usize) -> u128 {
        assert!(n < 32, "V{n} does not exist");
        self.slots[slot::V0 + n]
    }

    pub fn set_v(&mut self, n: usize, value: u128) {
        assert!(n < 32, "V{n} does not exist");
        self.slots[slot::V0 + n] = value;
    }

    pub fn fpcr(&self) -> u32 {
        self.slots[slot::FPCR] as u32
    }

    pub fn set_fpcr(&mut self, value: u32) {
        self.slots[slot::FPCR] = value.into();
    }

    pub fn fpsr(&self) -> u32 {
        self.slots[slot::FPSR] as u32
    }

    pub fn set_fpsr(&mut self, value: u32) {
        self.slots[slot::FPSR] = value.into();
    }

    /// N, Z, C and V as the bits 3 to 0.
    pub fn nzcv(&self) -> u8 {
        ((self.slots[slot::N] as u8 & 1) << 3)
            | ((self.slots[slot::Z] as u8 & 1) << 2)
            | ((self.slots[slot::C] as u8 & 1) << 1)
            | (self.slots[slot::V] as u8 & 1)
    }

    pub fn set_nzcv(&mut self, nzcv: u8) {
        self.slots[slot::N] = u128::from((nzcv >> 3) & 1);
        self.slots[slot::Z] = u128::from((nzcv >> 2) & 1);
        self.slots[slot::C] = u128::from((nzcv >> 1) & 1);
        self.slots[slot::V] = u128::from(nzcv & 1);
    }

    /// PSTATE in the layout of an SPSR, which is also what a debugger calls
    /// CPSR: NZCV in the bits 31 to 28, DAIF in 9 to 6, the level in 3 to 2
    /// and PSTATE.SP in bit 0. PSTATE.IL is bit 20.
    pub fn cpsr(&self) -> u32 {
        (u32::from(self.nzcv()) << 28)
            | (u32::from(self.il) << 20)
            | (u32::from(self.pstate13) << 13)
            | (u32::from(self.daif & 0xf) << 6)
            | (u32::from(self.context.el & 3) << 2)
            | u32::from(self.context.sp_sel)
    }
}

impl State {
    /// Put a PSTATE in the layout of `cpsr` into the state. The stack
    /// pointers stay in their slots. `false`: the value names no state of
    /// this CPU (AArch32, EL2 or EL3, a reserved mode), and nothing changes.
    pub fn set_cpsr(&mut self, cpsr: u32) -> bool {
        let level = (cpsr >> 2) as u8 & 3;
        if cpsr & 0x12 != 0 || level > 1 || (level == 0 && cpsr & 1 != 0) {
            return false;
        }
        self.set_nzcv((cpsr >> 28) as u8);
        self.daif = (cpsr >> 6) as u8 & 0xf;
        self.il = cpsr & (1 << 20) != 0;
        self.pstate13 = cpsr & (1 << 13) != 0;
        self.context.el = level;
        self.context.sp_sel = cpsr & 1 != 0;
        self.refresh();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_matches_the_slot_numbers() {
        assert_eq!(REGISTER_FILE.len(), slot::COUNT);
        assert_eq!(SLOTS[slot::X30].name, "x30");
        assert_eq!(SLOTS[slot::SP_EL1].name, "sp_el1");
        assert_eq!(SLOTS[slot::PC].name, "pc");
        assert_eq!(SLOTS[slot::V].name, "v");
        assert_eq!(SLOTS[slot::V0].name, "v0");
        assert_eq!(SLOTS[slot::V0 + 31].name, "v31");
        assert_eq!(SLOTS[slot::FPSR].name, "fpsr");
        assert_eq!(SLOTS[slot::N].ty, ScalarType::Bits(1));
        assert_eq!(SLOTS[slot::V0].ty, ScalarType::Bits(128));
    }

    #[test]
    fn reset_is_el1h_with_interrupts_masked() {
        let mut state = State::reset(0x4000_0000);
        assert_eq!(state.cpsr(), 0x3c5);
        assert_eq!(state.pc(), 0x4000_0000);
        state.set_nzcv(0b0110);
        assert_eq!(state.cpsr(), 0x6000_03c5);
        assert_eq!(state.nzcv(), 0b0110);
        state.set_sp(0x1000);
        assert_eq!(state.slots[slot::SP_EL1], 0x1000);
        state.context.sp_sel = false;
        assert_eq!(state.sp(), 0);
        let context = |el| Context {
            el,
            sp_sel: true,
            fp: false,
            tbi: 0,
        };
        assert_eq!(context(0).sp_slot(), slot::SP_EL0);
        assert_eq!(context(2).sp_slot(), slot::SP_EL2);
    }

    #[test]
    fn fp_follows_cpacr_and_the_level() {
        let mut state = State::reset(0);
        assert!(!state.context.fp);
        // FPEN 1: EL1 only.
        state.system.regs[reg::CPACR_EL1] = 1 << 20;
        state.refresh();
        assert!(state.context.fp);
        state.context.el = 0;
        state.refresh();
        assert!(!state.context.fp);
        // FPEN 2 traps both levels, FPEN 3 none.
        state.system.regs[reg::CPACR_EL1] = 2 << 20;
        state.context.el = 1;
        state.refresh();
        assert!(!state.context.fp);
        state.enable_fp();
        state.context.el = 0;
        state.refresh();
        assert!(state.context.fp);
    }

    #[test]
    fn a_cpsr_goes_into_the_state_and_back() {
        let mut state = State::reset(0);
        for cpsr in [0x3c5u32, 0x3c4, 0x6000_0000, 0x9010_2345, 0x0000_0080] {
            assert!(state.set_cpsr(cpsr), "{cpsr:#x}");
            assert_eq!(state.cpsr(), cpsr);
        }
        // EL2, AArch32, a reserved mode and EL0 with PSTATE.SP set.
        for cpsr in [0x3c9u32, 0x3d0, 0x3c2, 0x3c1] {
            assert!(!state.set_cpsr(cpsr), "{cpsr:#x}");
            assert_eq!(state.cpsr(), 0x80);
        }
    }
}
