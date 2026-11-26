//! The lifters of the system instructions (card C6): exception generation,
//! `ERET`, `MRS` and `MSR`, the PSTATE forms, the hint space and the
//! barriers.
//!
//! Most of them end their block with a `Trap`, which the loop runs in Rust
//! after the registers are written back (DESIGN.md G3d): they can take an
//! exception, change the level or write a system register. What stays in
//! the graph is what a guest runs often and what touches only register
//! slots: the hints, the barriers, and NZCV, FPCR and FPSR. An `MRS` of a
//! register that is a stored word, such as TPIDR_EL1, stays in the block
//! too, as an ordered helper call (R10).
//!
//! Sources read (DESIGN.md "Sources"):
//! - S1: the generated tables, for the records, their fields and the PSTATE
//!   field encodings.
//! - S6: the lockstep against QEMU and the probe of its system registers
//!   (`tests/qemu-cortex-a53-sysregs.txt`).
//! - No file of S2 to S5 was opened, and no text of the Arm manual. QEMU's
//!   source was not read (X1).

use super::Lifted;
use super::builder::{Builder, PstateField, Trap};
use crate::exception::Exception;
use crate::generated::decode::{
    CRmSystemI, ExceptionGeneration, HintI, MRSI, MSRI, MSRpstateImm0_15, Record, UDFType,
};
use crate::state::slot;
use crate::sysreg::{self, FPCR_MASK, FPSR_MASK};
use vmlab_hv_tcg_ir::native::Binary;

/// The encodings of the three registers that are register slots.
const NZCV: u32 = 0xda10;
const FPCR: u32 = 0xda20;
const FPSR: u32 = 0xda21;

fn trap(b: &mut Builder, trap: Trap) -> Lifted {
    b.trap(trap);
    Lifted::Branch
}

/// The instruction takes the Undefined Instruction exception on this CPU.
pub fn undefined(b: &mut Builder) -> Lifted {
    trap(b, Trap::Exception(Exception::Undefined))
}

/// `SVC`, `HVC`, `BRK`, `HLT`, `DCPS1` and `DCPS2`. `HLT` and the `DCPS`
/// forms belong to halting debug, which this CPU does not have.
pub fn exception(b: &mut Builder, record: Record, f: &ExceptionGeneration) -> Lifted {
    let immediate = f.imm as u16;
    match record {
        Record::SVC => trap(b, Trap::Exception(Exception::SupervisorCall(immediate))),
        Record::HVC => trap(b, Trap::Hvc(immediate)),
        Record::BRK => trap(b, Trap::Exception(Exception::Breakpoint(immediate))),
        Record::HLT | Record::DCPS1 | Record::DCPS2 => undefined(b),
        _ => Lifted::Unsupported,
    }
}

/// `UDF`: permanently undefined.
pub fn udf(b: &mut Builder, _record: Record, _f: &UDFType) -> Lifted {
    undefined(b)
}

/// `ERET`, and `DRPS`, which is undefined outside debug state.
pub fn special_return(b: &mut Builder, record: Record) -> Lifted {
    match record {
        Record::ERET => trap(b, Trap::Eret),
        Record::DRPS => undefined(b),
        _ => Lifted::Unsupported,
    }
}

/// The hint space. On this CPU every hint is a NOP but `WFI`, and `WFE` at
/// EL0, which SCTLR_EL1 can trap.
pub fn hint(b: &mut Builder, _record: Record, f: &HintI) -> Lifted {
    match f.imm & 0x7f {
        2 if b.context().el == 0 => trap(b, Trap::Wfe),
        3 => trap(b, Trap::Wfi),
        _ => Lifted::Next,
    }
}

/// The hints that have a record of their own: `NOP`, the pointer
/// authentication hints, `CHKFEAT`. All are NOPs on this CPU.
pub fn no_operands(_b: &mut Builder, _record: Record) -> Lifted {
    Lifted::Next
}

/// `CLREX`, `DSB`, `DMB` and `ISB`. The interpreter runs one instruction
/// after the other and has no cache, so the barriers do nothing. `CLREX`
/// clears the reservation of the exclusives, which card C3 brings.
pub fn barrier(_b: &mut Builder, _record: Record, _f: &CRmSystemI) -> Lifted {
    Lifted::Next
}

/// `MRS`. NZCV, FPCR and FPSR are register slots and stay in the graph.
pub fn mrs(b: &mut Builder, _record: Record, f: &MRSI) -> Lifted {
    let fp = b.context().fp;
    match f.systemreg {
        NZCV => {
            let mut value = b.imm(64, 0);
            for (flag, bit) in [(slot::N, 31), (slot::Z, 30), (slot::C, 29), (slot::V, 28)] {
                let flag = b.read(flag);
                let flag = b.zero_extend(flag, 64);
                let flag = b.shl(64, flag, bit);
                value = b.binary(64, Binary::Or, value, flag);
            }
            b.set_x(f.rt, 64, value);
            Lifted::Next
        }
        FPCR | FPSR if !fp => trap(b, Trap::Exception(Exception::FpAccess)),
        FPCR | FPSR => {
            let value = b.read(if f.systemreg == FPCR {
                slot::FPCR
            } else {
                slot::FPSR
            });
            let value = b.zero_extend(value, 64);
            b.set_x(f.rt, 64, value);
            Lifted::Next
        }
        encoding => {
            // A stored word that this level may read without a condition is
            // read inside the block. Every write of a system register ends
            // its block, so the word is current.
            if let Some(row) = sysreg::find(encoding as u16)
                && sysreg::reads_stored(row, b.context().el)
            {
                let row = b.imm(64, row as u64);
                let value = b.call_ordered(64, &sysreg::READ_STORED, &[row]);
                b.set_x(f.rt, 64, value);
                return Lifted::Next;
            }
            trap(
                b,
                Trap::Mrs {
                    encoding: encoding as u16,
                    rt: f.rt as u8,
                },
            )
        }
    }
}

/// `MSR` of a register.
pub fn msr(b: &mut Builder, _record: Record, f: &MSRI) -> Lifted {
    let fp = b.context().fp;
    match f.systemreg {
        NZCV => {
            let value = b.x(f.rt, 64);
            for (flag, bit) in [(slot::N, 31), (slot::Z, 30), (slot::C, 29), (slot::V, 28)] {
                let bit = b.bit(value, bit);
                b.write(flag, bit);
            }
            Lifted::Next
        }
        FPCR | FPSR if !fp => trap(b, Trap::Exception(Exception::FpAccess)),
        FPCR | FPSR => {
            let (slot, mask) = if f.systemreg == FPCR {
                (slot::FPCR, FPCR_MASK)
            } else {
                (slot::FPSR, FPSR_MASK)
            };
            let value = b.x(f.rt, 32);
            let mask = b.imm(32, mask);
            let value = b.binary(32, Binary::And, value, mask);
            b.write(slot, value);
            // The helpers of later instructions take FPCR from the slot, so
            // the block can go on.
            Lifted::Next
        }
        _ => trap(
            b,
            Trap::Msr {
                encoding: f.systemreg as u16,
                rt: f.rt as u8,
            },
        ),
    }
}

/// `MSR` of a PSTATE field with an immediate: `SPSel`, `DAIFSet` and
/// `DAIFClr` in the 3d set. The field is op1:op2.
pub fn msr_pstate(b: &mut Builder, _record: Record, f: &MSRpstateImm0_15) -> Lifted {
    let field = match f.pstatefield {
        0b000_101 => PstateField::SpSel,
        0b011_110 => PstateField::DaifSet,
        0b011_111 => PstateField::DaifClr,
        _ => return undefined(b),
    };
    trap(
        b,
        Trap::MsrPstate {
            field,
            immediate: f.imm as u8 & 15,
        },
    )
}
