//! PSCI 1.1 of the aarch64 `virt` machine (phase 3d, G4 and V1): what the
//! machine answers to an `HVC` or an `SMC` of the guest. The decision is a
//! value: the machine carries it out.
//!
//! Sources read (phase 3d DESIGN.md, "Sources"): S7, the Arm Power State
//! Coordination Interface specification (DEN 0022, the function identifiers,
//! the arguments and the return codes) and the SMC Calling Convention
//! (DEN 0028). No QEMU source was read.

pub const VERSION: u32 = 0x8400_0000;
pub const CPU_SUSPEND: u32 = 0x8400_0001;
pub const CPU_OFF: u32 = 0x8400_0002;
pub const CPU_ON: u32 = 0x8400_0003;
pub const AFFINITY_INFO: u32 = 0x8400_0004;
pub const MIGRATE: u32 = 0x8400_0005;
pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
pub const SYSTEM_OFF: u32 = 0x8400_0008;
pub const SYSTEM_RESET: u32 = 0x8400_0009;
pub const FEATURES: u32 = 0x8400_000a;
/// The bit of a function identifier that says SMC64.
pub const SMC64: u32 = 0x4000_0000;
pub const CPU_SUSPEND64: u32 = CPU_SUSPEND | SMC64;
pub const CPU_ON64: u32 = CPU_ON | SMC64;
pub const AFFINITY_INFO64: u32 = AFFINITY_INFO | SMC64;
pub const MIGRATE64: u32 = MIGRATE | SMC64;
pub const SMCCC_VERSION: u32 = 0x8000_0000;

/// PSCI 1.1.
const PSCI_1_1: u64 = 0x0001_0001;
/// SMCCC 1.1.
const SMCCC_1_1: u64 = 0x0001_0001;

pub const SUCCESS: u64 = 0;
pub const NOT_SUPPORTED: u64 = -1i64 as u64;
pub const INVALID_PARAMETERS: u64 = -2i64 as u64;
pub const ALREADY_ON: u64 = -4i64 as u64;

/// `MIGRATE_INFO_TYPE`: no trusted OS, so no migration.
const NO_MIGRATION: u64 = 2;
/// `AFFINITY_INFO`.
const ON: u64 = 0;
const OFF: u64 = 1;

/// What the machine does for a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The call returns this in X0.
    Return(u64),
    /// The calling CPU goes off. The call does not return.
    Off,
    /// The CPU `cpu` starts at `entry` with `context` in X0, and the call
    /// returns success.
    On {
        cpu: u8,
        entry: u64,
        context: u64,
    },
    SystemOff,
    SystemReset,
}

/// The CPU that the MPIDR `affinity` names, among `count`: affinity level 0
/// is the number of the CPU, and every higher level is 0.
fn cpu_of(affinity: u64, count: usize) -> Option<u8> {
    let cpu = affinity & 0xff;
    (affinity & 0xff_00ff_ff00 == 0 && (cpu as usize) < count).then_some(cpu as u8)
}

fn known(function: u32) -> bool {
    matches!(
        function,
        VERSION
            | CPU_SUSPEND
            | CPU_SUSPEND64
            | CPU_OFF
            | CPU_ON
            | CPU_ON64
            | AFFINITY_INFO
            | AFFINITY_INFO64
            | MIGRATE_INFO_TYPE
            | SYSTEM_OFF
            | SYSTEM_RESET
            | FEATURES
            | SMCCC_VERSION
    )
}

/// The answer to the call `function` with the arguments of X1 to X3.
/// `powered` says for each CPU of the machine whether it is on.
pub fn decide(function: u32, arguments: [u64; 3], powered: &[bool]) -> Action {
    // An SMC32 call takes the low halves of its arguments.
    let wide = function & SMC64 != 0;
    let argument = |n: usize| {
        if wide {
            arguments[n]
        } else {
            arguments[n] & 0xffff_ffff
        }
    };
    match function {
        VERSION => Action::Return(PSCI_1_1),
        SMCCC_VERSION => Action::Return(SMCCC_1_1),
        CPU_SUSPEND | CPU_SUSPEND64 => {
            // A standby state may return at once. A power-down state is one
            // that this machine does not have.
            if argument(0) & 1 << 16 != 0 {
                Action::Return(INVALID_PARAMETERS)
            } else {
                Action::Return(SUCCESS)
            }
        }
        CPU_OFF => Action::Off,
        CPU_ON | CPU_ON64 => match cpu_of(argument(0), powered.len()) {
            None => Action::Return(INVALID_PARAMETERS),
            Some(cpu) if powered[usize::from(cpu)] => Action::Return(ALREADY_ON),
            Some(cpu) => Action::On {
                cpu,
                entry: argument(1),
                context: argument(2),
            },
        },
        AFFINITY_INFO | AFFINITY_INFO64 => {
            // Only the lowest affinity level has a state of its own.
            match cpu_of(argument(0), powered.len()) {
                Some(cpu) if argument(1) == 0 => {
                    Action::Return(if powered[usize::from(cpu)] { ON } else { OFF })
                }
                _ => Action::Return(INVALID_PARAMETERS),
            }
        }
        MIGRATE_INFO_TYPE => Action::Return(NO_MIGRATION),
        SYSTEM_OFF => Action::SystemOff,
        SYSTEM_RESET => Action::SystemReset,
        FEATURES => {
            let asked = arguments[0] as u32;
            Action::Return(if known(asked) { SUCCESS } else { NOT_SUPPORTED })
        }
        _ => Action::Return(NOT_SUPPORTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_features() {
        let on = [true, false];
        assert_eq!(decide(VERSION, [0; 3], &on), Action::Return(0x1_0001));
        assert_eq!(decide(SMCCC_VERSION, [0; 3], &on), Action::Return(0x1_0001));
        for function in [CPU_ON64, CPU_OFF, SYSTEM_RESET, SMCCC_VERSION, FEATURES] {
            assert_eq!(
                decide(FEATURES, [u64::from(function), 0, 0], &on),
                Action::Return(SUCCESS)
            );
        }
        // MIGRATE is in the device tree and is not served.
        for function in [MIGRATE, MIGRATE64, 0x8400_000c, 0x8000_0001] {
            assert_eq!(
                decide(FEATURES, [u64::from(function), 0, 0], &on),
                Action::Return(NOT_SUPPORTED)
            );
            assert_eq!(decide(function, [0; 3], &on), Action::Return(NOT_SUPPORTED));
        }
        assert_eq!(decide(MIGRATE_INFO_TYPE, [0; 3], &on), Action::Return(2));
    }

    #[test]
    fn a_cpu_is_turned_on_once_and_by_its_affinity() {
        let powered = [true, false, false];
        assert_eq!(
            decide(CPU_ON64, [1, 0x4008_0000, 7], &powered),
            Action::On {
                cpu: 1,
                entry: 0x4008_0000,
                context: 7
            }
        );
        assert_eq!(
            decide(CPU_ON64, [0, 0x4008_0000, 7], &powered),
            Action::Return(ALREADY_ON)
        );
        for affinity in [3, 0x100, 0x1_0000, 1 << 32] {
            assert_eq!(
                decide(CPU_ON64, [affinity, 0, 0], &powered),
                Action::Return(INVALID_PARAMETERS)
            );
        }
        // SMC32 takes the low halves.
        assert_eq!(
            decide(CPU_ON, [0xffff_ffff_0000_0002, 0x1_0000_1000, 9], &powered),
            Action::On {
                cpu: 2,
                entry: 0x1000,
                context: 9
            }
        );
    }

    #[test]
    fn affinity_info_says_on_or_off() {
        let powered = [true, false];
        assert_eq!(
            decide(AFFINITY_INFO64, [0, 0, 0], &powered),
            Action::Return(0)
        );
        assert_eq!(
            decide(AFFINITY_INFO, [1, 0, 0], &powered),
            Action::Return(1)
        );
        assert_eq!(
            decide(AFFINITY_INFO, [1, 1, 0], &powered),
            Action::Return(INVALID_PARAMETERS)
        );
        assert_eq!(
            decide(AFFINITY_INFO, [2, 0, 0], &powered),
            Action::Return(INVALID_PARAMETERS)
        );
    }

    #[test]
    fn power_calls() {
        let powered = [true];
        assert_eq!(decide(CPU_OFF, [0; 3], &powered), Action::Off);
        assert_eq!(decide(SYSTEM_OFF, [0; 3], &powered), Action::SystemOff);
        assert_eq!(decide(SYSTEM_RESET, [0; 3], &powered), Action::SystemReset);
        assert_eq!(
            decide(CPU_SUSPEND64, [0, 0, 0], &powered),
            Action::Return(SUCCESS)
        );
        assert_eq!(
            decide(CPU_SUSPEND, [1 << 16, 0, 0], &powered),
            Action::Return(INVALID_PARAMETERS)
        );
    }
}
