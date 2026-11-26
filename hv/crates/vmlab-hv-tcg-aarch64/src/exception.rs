//! Exceptions and exception levels (DESIGN.md G4): what is taken to EL1, the
//! syndrome that it leaves, the entry, the return, and the interrupt masks.
//! EL0 and EL1 exist. The level is data: every entry asks `target_el`, the
//! banks have room for EL2, and nothing here assumes that EL1 is the highest
//! level. EL3 is never built.
//!
//! Sources read (DESIGN.md "Sources"):
//! - S6: the registers of a running QEMU after each exception and each
//!   return, through the lockstep of this card (`VERIFICATION.md`, card C6):
//!   ESR_EL1, FAR_EL1, ELR_EL1, SPSR_EL1, the PC and CPSR are compared after
//!   every instruction.
//! - No file of S2 to S5 was opened, and no text of the Arm manual. The
//!   layout of a syndrome and of the vector table is general knowledge of
//!   the architecture. QEMU's source was not read (X1).

use crate::state::State;
use crate::sysreg::reg;

/// A synchronous exception.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exception {
    /// The Undefined Instruction exception: an encoding or a register that
    /// this CPU does not have, or an access that this level may not make.
    Undefined,
    /// `WFI` or `WFE` at EL0 while SCTLR_EL1 traps it. `true`: `WFE`.
    WaitTrap(bool),
    /// An FP or AdvSIMD instruction while CPACR_EL1.FPEN disables them.
    FpAccess,
    /// An instruction while PSTATE.IL is set.
    IllegalState,
    /// `SVC` with its immediate.
    SupervisorCall(u16),
    /// A system instruction that a control bit traps to EL1, with its ISS.
    SystemTrap(u32),
    /// The memory holds nothing at the PC. The fault status code is the one
    /// of a synchronous external abort.
    InstructionAbort,
    /// The PC is not a multiple of four.
    PcAlignment,
    /// `BRK` with its immediate.
    Breakpoint(u16),
}

/// The instruction length bit of a syndrome: every A64 instruction is 32
/// bits long.
const IL: u32 = 1 << 25;

impl Exception {
    /// ESR_EL1 for the exception, taken from the level `from`.
    pub fn syndrome(self, from: u8) -> u32 {
        let lower = u32::from(from == 0);
        let (class, iss) = match self {
            Self::Undefined => (0x00, 0),
            // CV and COND say "always", and bit 0 says which instruction.
            Self::WaitTrap(wfe) => (0x01, 0x01e0_0000 | u32::from(wfe)),
            Self::FpAccess => (0x07, 0x01e0_0000),
            Self::IllegalState => (0x0e, 0),
            Self::SupervisorCall(immediate) => (0x15, u32::from(immediate)),
            Self::SystemTrap(iss) => (0x18, iss),
            // 0x20 from a lower level, 0x21 from the same one.
            Self::InstructionAbort => (0x21 - lower, 0x10),
            Self::PcAlignment => (0x22, 0),
            Self::Breakpoint(immediate) => (0x3c, u32::from(immediate)),
        };
        class << 26 | IL | iss
    }

    /// Whether the exception writes FAR_EL1 with the PC.
    fn sets_far(self) -> bool {
        matches!(self, Self::InstructionAbort | Self::PcAlignment)
    }

    /// The address that ELR_EL1 gets for an exception at `pc`: the next
    /// instruction after a call, the instruction itself after a fault.
    pub fn return_address(self, pc: u64) -> u64 {
        match self {
            Self::SupervisorCall(_) => pc.wrapping_add(4),
            _ => pc,
        }
    }
}

/// An asynchronous exception: one of the two interrupt lines of the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interrupt {
    Irq,
    Fiq,
}

/// What an entry is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Exception(Exception),
    Interrupt(Interrupt),
}

/// The level that takes an exception or an interrupt from this state. With
/// EL0 and EL1 that is EL1 always. A CPU with EL2 routes here.
pub fn target_el(_cause: Cause, _state: &State) -> u8 {
    1
}

/// The offset of a vector in the table: the kind of entry in the bits 8 and
/// 7, and where it comes from in the bits 10 and 9.
pub fn vector_offset(cause: Cause, from: u8, target: u8, sp_sel: bool) -> u64 {
    let kind = match cause {
        Cause::Exception(_) => 0x000,
        Cause::Interrupt(Interrupt::Irq) => 0x080,
        Cause::Interrupt(Interrupt::Fiq) => 0x100,
    };
    let source = if from < target {
        0x400
    } else if sp_sel {
        0x200
    } else {
        0x000
    };
    source + kind
}

/// Take `cause` at the instruction `pc`: the registers of the target level
/// get the state, and the CPU goes on at the vector. For an interrupt `pc`
/// is the instruction that has not run.
pub fn enter(state: &mut State, cause: Cause, pc: u64) {
    let target = target_el(cause, state);
    debug_assert_eq!(target, 1, "only EL1 takes exceptions in phase 3d");
    let from = state.context.el;
    let offset = vector_offset(cause, from, target, state.context.sp_sel);
    let saved = u64::from(state.cpsr());
    let system = &mut state.system;
    system.regs[reg::SPSR_EL1] = saved;
    match cause {
        Cause::Exception(exception) => {
            system.regs[reg::ELR_EL1] = exception.return_address(pc);
            system.regs[reg::ESR_EL1] = u64::from(exception.syndrome(from));
            if exception.sets_far() {
                system.regs[reg::FAR_EL1] = pc;
            }
        }
        Cause::Interrupt(_) => system.regs[reg::ELR_EL1] = pc,
    }
    let vector = system.regs[reg::VBAR_EL1].wrapping_add(offset);
    state.daif = 0xf;
    state.il = false;
    state.context.el = target;
    state.context.sp_sel = true;
    state.refresh();
    state.set_pc(vector);
}

/// Whether a saved PSTATE names a state that EL1 can return to: EL0 with
/// SP_EL0, or EL1 with either stack pointer, in AArch64.
pub fn return_is_legal(spsr: u64, from: u8) -> bool {
    let aarch32 = spsr & 0x10 != 0;
    let level = (spsr >> 2) as u8 & 3;
    let reserved = spsr & 2 != 0;
    let sp_sel = spsr & 1 != 0;
    !aarch32 && !reserved && level <= from && !(level == 0 && sp_sel)
}

/// `ERET` at EL1: PSTATE comes from SPSR_EL1 and the PC from ELR_EL1. A
/// saved state that EL1 cannot return to sets PSTATE.IL and leaves the level
/// and the stack pointer as they are.
pub fn exception_return(state: &mut State) {
    let from = state.context.el;
    let spsr = state.system.regs[reg::SPSR_EL1];
    let target = state.system.regs[reg::ELR_EL1];
    state.set_nzcv((spsr >> 28) as u8 & 15);
    state.daif = (spsr >> 6) as u8 & 15;
    if return_is_legal(spsr, from) {
        state.context.el = (spsr >> 2) as u8 & 3;
        state.context.sp_sel = spsr & 1 != 0;
        state.il = spsr & (1 << 20) != 0;
    } else {
        state.il = true;
    }
    state.refresh();
    state.set_pc(target);
}

/// The interrupt that the CPU takes now, when a line is high and PSTATE
/// does not mask it. FIQ goes first.
pub fn pending(state: &State, irq: bool, fiq: bool) -> Option<Interrupt> {
    const I: u8 = 2;
    const F: u8 = 1;
    if fiq && state.daif & F == 0 {
        Some(Interrupt::Fiq)
    } else if irq && state.daif & I == 0 {
        Some(Interrupt::Irq)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syndromes_are_class_length_and_iss() {
        assert_eq!(Exception::Undefined.syndrome(1), 0x0200_0000);
        assert_eq!(Exception::WaitTrap(false).syndrome(0), 0x07e0_0000);
        assert_eq!(Exception::WaitTrap(true).syndrome(0), 0x07e0_0001);
        assert_eq!(Exception::FpAccess.syndrome(0), 0x1fe0_0000);
        assert_eq!(Exception::IllegalState.syndrome(1), 0x3a00_0000);
        assert_eq!(Exception::SupervisorCall(0xe1).syndrome(0), 0x5600_00e1);
        // MRS X0, CTR_EL0 at EL0, as QEMU reports it.
        assert_eq!(Exception::SystemTrap(0x0032_c001).syndrome(0), 0x6232_c001);
        assert_eq!(Exception::InstructionAbort.syndrome(0), 0x8200_0010);
        assert_eq!(Exception::InstructionAbort.syndrome(1), 0x8600_0010);
        assert_eq!(Exception::PcAlignment.syndrome(1), 0x8a00_0000);
        assert_eq!(Exception::Breakpoint(0x3e8).syndrome(1), 0xf200_03e8);
    }

    #[test]
    fn the_vector_depends_on_the_source_and_on_the_kind() {
        let sync = Cause::Exception(Exception::Undefined);
        let irq = Cause::Interrupt(Interrupt::Irq);
        let fiq = Cause::Interrupt(Interrupt::Fiq);
        assert_eq!(vector_offset(sync, 1, 1, false), 0x000);
        assert_eq!(vector_offset(sync, 1, 1, true), 0x200);
        assert_eq!(vector_offset(sync, 0, 1, false), 0x400);
        assert_eq!(vector_offset(irq, 1, 1, true), 0x280);
        assert_eq!(vector_offset(fiq, 1, 1, false), 0x100);
        assert_eq!(vector_offset(irq, 0, 1, false), 0x480);
        assert_eq!(vector_offset(fiq, 0, 1, false), 0x500);
    }

    #[test]
    fn an_entry_saves_the_state_and_masks_everything() {
        let mut state = State::reset(0x4000_1000);
        state.system.regs[reg::VBAR_EL1] = 0x4008_0000;
        state.set_nzcv(0b1010);
        state.daif = 0b0100;
        state.context.el = 0;
        state.context.sp_sel = false;
        state.system.regs[reg::FAR_EL1] = 0x1234;
        enter(
            &mut state,
            Cause::Exception(Exception::SupervisorCall(7)),
            0x4000_1000,
        );
        assert_eq!(state.pc(), 0x4008_0400);
        assert_eq!(state.cpsr(), 0xa000_03c5);
        assert_eq!(state.system.regs[reg::SPSR_EL1], 0xa000_0100);
        assert_eq!(state.system.regs[reg::ELR_EL1], 0x4000_1004);
        assert_eq!(state.system.regs[reg::ESR_EL1], 0x5600_0007);
        // A call leaves FAR_EL1 alone.
        assert_eq!(state.system.regs[reg::FAR_EL1], 0x1234);
        // A fault at EL1 with SP_EL1 returns to the instruction itself.
        enter(&mut state, Cause::Exception(Exception::PcAlignment), 0x4008_0402);
        assert_eq!(state.pc(), 0x4008_0200);
        assert_eq!(state.system.regs[reg::ELR_EL1], 0x4008_0402);
        assert_eq!(state.system.regs[reg::FAR_EL1], 0x4008_0402);
        assert_eq!(state.system.regs[reg::ESR_EL1], 0x8a00_0000);
        assert_eq!(state.system.regs[reg::SPSR_EL1], 0xa000_03c5);
        // An interrupt leaves ESR_EL1 alone and returns to what did not run.
        state.context.sp_sel = false;
        enter(&mut state, Cause::Interrupt(Interrupt::Irq), 0x4000_2000);
        assert_eq!(state.pc(), 0x4008_0080);
        assert_eq!(state.system.regs[reg::ELR_EL1], 0x4000_2000);
        assert_eq!(state.system.regs[reg::ESR_EL1], 0x8a00_0000);
        assert_eq!(state.system.regs[reg::SPSR_EL1], 0xa000_03c4);
        assert!(state.context.sp_sel);
    }

    #[test]
    fn a_return_restores_the_state_or_marks_it_illegal() {
        let mut state = State::reset(0);
        state.system.regs[reg::SPSR_EL1] = 0x6000_0140;
        state.system.regs[reg::ELR_EL1] = 0x4000_0040;
        exception_return(&mut state);
        assert_eq!(state.pc(), 0x4000_0040);
        assert_eq!(state.cpsr(), 0x6000_0140);
        assert_eq!((state.context.el, state.context.sp_sel), (0, false));
        // EL1 with SP_EL0, and the bits that PSTATE does not have are dropped.
        let mut state = State::reset(0);
        state.system.regs[reg::SPSR_EL1] = 0x0fc0_fc04;
        exception_return(&mut state);
        assert_eq!(state.cpsr(), 0x0000_0004);
        // A saved IL comes back.
        let mut state = State::reset(0);
        state.system.regs[reg::SPSR_EL1] = 0x0010_03c5;
        exception_return(&mut state);
        assert!(state.il);
        assert_eq!(state.cpsr(), 0x0010_03c5);
        // EL2, EL3, a reserved mode, EL0 with its own stack pointer selected,
        // and AArch32: the level and the stack pointer stay, IL is set, and
        // the flags and the masks still come from the saved state.
        for spsr in [0x3c9u64, 0x3cd, 0x3c2, 0x3c6, 0x3c1, 0x3d0, 0x3d3] {
            let mut state = State::reset(0);
            state.system.regs[reg::SPSR_EL1] = spsr | 0x9000_0000;
            state.system.regs[reg::ELR_EL1] = 0x40;
            exception_return(&mut state);
            assert!(state.il, "{spsr:#x}");
            assert_eq!(state.cpsr(), 0x9010_03c5, "{spsr:#x}");
            assert_eq!(state.pc(), 0x40);
        }
        assert!(return_is_legal(0x3c5, 1));
        assert!(return_is_legal(0x3c4, 1));
        assert!(return_is_legal(0x3c0, 1));
    }

    #[test]
    fn a_line_is_taken_when_its_mask_is_clear_and_fiq_goes_first() {
        let mut state = State::reset(0);
        assert_eq!(pending(&state, true, true), None);
        state.daif = 0b1101;
        assert_eq!(pending(&state, true, false), Some(Interrupt::Irq));
        assert_eq!(pending(&state, false, true), None);
        state.daif = 0b1100;
        assert_eq!(pending(&state, true, true), Some(Interrupt::Fiq));
        assert_eq!(pending(&state, false, false), None);
        assert_eq!(target_el(Cause::Interrupt(Interrupt::Irq), &state), 1);
    }
}
