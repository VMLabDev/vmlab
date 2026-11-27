//! The debug port of an emulated aarch64 machine (phase 3d, F6): the CPUs
//! of `virt` run in the process, so their state is read and written
//! straight, a virtual address is translated by the CPU's own walk, and a
//! step runs one instruction on the thread of the request while the
//! machine stays paused. Breakpoints are not built for an emulated vCPU.
//!
//! Sources read: none beyond the state of `vmlab-hv-tcg-aarch64`.

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::Mutex;
use vmlab_hv_proto::control::ErrorCode;
use vmlab_hv_proto::debug::Aarch64Registers;
use vmlab_hv_tcg_aarch64::State;
use vmlab_hv_tcg_aarch64::state::slot;
use vmlab_hv_tcg_aarch64::sysreg;

use super::{Result, internal, invalid};
use crate::vcpu::emulated::EmulatedVcpu;
use crate::vcpu::{VcpuBackend, VcpuState};
use crate::vm::VmShared;

pub(super) fn vcpu(shared: &VmShared, id: u8) -> Result<Arc<Mutex<EmulatedVcpu>>> {
    shared
        .virt
        .get()
        .and_then(|virt| virt.vcpu(id))
        .ok_or_else(|| invalid(format!("no vCPU {id}")))
}

pub(super) fn ids(shared: &VmShared) -> Vec<u8> {
    shared
        .virt
        .get()
        .map(|virt| (0..virt.vcpus() as u8).collect())
        .unwrap_or_default()
}

fn state(vcpu: &Mutex<EmulatedVcpu>) -> Result<State> {
    match vcpu.lock().state().map_err(internal)? {
        VcpuState::Aarch64(state) => Ok(*state),
        VcpuState::X86 { .. } => Err(internal("an emulated vCPU gave an x86 state")),
    }
}

pub(super) fn pc(shared: &VmShared, id: u8) -> Result<u64> {
    let vcpu = vcpu(shared, id)?;
    Ok(state(&vcpu)?.pc())
}

/// The registers of `state` as the debug port shows them.
pub(super) fn registers(state: &State) -> Aarch64Registers {
    let mut system = BTreeMap::new();
    system.insert("sp_el0".to_string(), state.slots[slot::SP_EL0] as u64);
    system.insert("sp_el1".to_string(), state.slots[slot::SP_EL1] as u64);
    for (row, value) in sysreg::ROWS.iter().zip(state.system.regs) {
        system.insert(row.name.to_ascii_lowercase(), value);
    }
    Aarch64Registers {
        x: std::array::from_fn(|n| state.x(n)),
        sp: state.sp(),
        pc: state.pc(),
        cpsr: state.cpsr(),
        v: std::array::from_fn(|n| state.v(n).to_le_bytes()),
        fpcr: state.fpcr(),
        fpsr: state.fpsr(),
        system,
    }
}

/// Write the named registers into `state`. Nothing is written when one
/// name or value is refused.
pub(super) fn set(state: &mut State, values: &BTreeMap<String, u64>) -> Result<()> {
    let mut next = state.clone();
    for (name, value) in values {
        let value = *value;
        let lower = name.to_ascii_lowercase();
        let narrow =
            |value: u64| u32::try_from(value).map_err(|_| invalid(format!("{name} takes 32 bits")));
        if let Some(n) = lower
            .strip_prefix('x')
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n <= 30)
        {
            next.set_x(n, value);
        } else {
            match lower.as_str() {
                "sp" => next.set_sp(value),
                "pc" => next.set_pc(value),
                "sp_el0" => next.slots[slot::SP_EL0] = value.into(),
                "sp_el1" => next.slots[slot::SP_EL1] = value.into(),
                "fpcr" => next.set_fpcr(narrow(value)?),
                "fpsr" => next.set_fpsr(narrow(value)?),
                "cpsr" => {
                    if !next.set_cpsr(narrow(value)?) {
                        return Err(invalid(format!(
                            "cpsr {value:#x} names no state of this CPU"
                        )));
                    }
                }
                _ => {
                    let at = sysreg::ROWS
                        .iter()
                        .position(|row| row.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| invalid(format!("unknown register {name:?}")))?;
                    next.system.regs[at] = value;
                }
            }
        }
    }
    next.refresh();
    *state = next;
    Ok(())
}

pub(super) fn read_registers(shared: &VmShared, id: u8) -> Result<Aarch64Registers> {
    let vcpu = vcpu(shared, id)?;
    Ok(registers(&state(&vcpu)?))
}

pub(super) fn set_registers(
    shared: &VmShared,
    id: u8,
    values: &BTreeMap<String, u64>,
) -> Result<()> {
    let vcpu = vcpu(shared, id)?;
    let mut state = state(&vcpu)?;
    set(&mut state, values)?;
    vcpu.lock()
        .set_state(&VcpuState::Aarch64(Box::new(state)))
        .map_err(internal)
}

/// One instruction of a CPU that is on. Its PC after the step.
pub(super) fn step(shared: &VmShared, id: u8) -> Result<u64> {
    let vcpu = vcpu(shared, id)?;
    if !shared.virt.get().is_some_and(|virt| virt.powered(id)) {
        return Err((
            ErrorCode::InvalidState,
            format!("vCPU {id} is off: PSCI has not turned it on"),
        ));
    }
    let mut vcpu = vcpu.lock();
    vcpu.debug_step(shared).map_err(internal)?;
    Ok(vcpu.cpu().state.pc())
}

/// The physical address of the virtual address `va` of a CPU, in the
/// translation regime that it runs in now.
pub(super) fn translate(vcpu: &Mutex<EmulatedVcpu>, va: u64) -> Result<u64> {
    vcpu.lock()
        .cpu()
        .physical_address(va)
        .ok_or_else(|| invalid(format!("unmapped virtual address {va:#x}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_registers_are_written_and_a_bad_name_writes_nothing() {
        let mut state = State::reset(0x4000_0000);
        let values = BTreeMap::from([
            ("x5".to_string(), 7),
            ("pc".to_string(), 0x4000_1000),
            ("VBAR_EL1".to_string(), 0x4000_8000),
            ("cpsr".to_string(), 0x6000_03c5),
        ]);
        set(&mut state, &values).unwrap();
        let shown = registers(&state);
        assert_eq!(shown.x[5], 7);
        assert_eq!(shown.pc, 0x4000_1000);
        assert_eq!(shown.cpsr, 0x6000_03c5);
        assert_eq!(shown.system["vbar_el1"], 0x4000_8000);
        let before = state.clone();
        let bad = BTreeMap::from([("x1".to_string(), 1), ("nope".to_string(), 0)]);
        assert!(set(&mut state, &bad).is_err());
        // EL3 does not exist on this CPU.
        let el3 = BTreeMap::from([("cpsr".to_string(), 0xd)]);
        assert!(set(&mut state, &el3).is_err());
        assert_eq!(state, before);
    }
}
