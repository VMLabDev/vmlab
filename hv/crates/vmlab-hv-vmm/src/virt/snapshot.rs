//! The machine state of the `virt` machine in a snapshot (phase 3d, F6 and
//! D13): each emulated CPU with its system registers and its generic
//! timers, which CPUs are on, the system counter, and the devices that are
//! no PCI function: the GICv2, the PL011, the PL031, the GED, fw_cfg, the
//! flash bank of the variables and the host bridge. The virtio functions,
//! the BARs of the PCI root and RAM go through the common path of
//! `snapshot.rs`.
//!
//! A restore puts the counter back first, so that the timers and the PL031
//! compare against the time of the save, then the devices, then the CPUs.
//! The GIC drives the interrupt inputs of the CPUs as it restores. A CPU
//! that takes its state builds its caches again (`Cpu::replace_state`).
//!
//! Sources read (phase 3d DESIGN.md, "Sources"): none for the CPU: the
//! state is the one of `vmlab-hv-tcg-aarch64`, written out field by field.

use serde::{Deserialize, Serialize};
use vmlab_hv_devices::Snapshot;
use vmlab_hv_tcg_aarch64::State;
use vmlab_hv_tcg_aarch64::state::{Context, slot};
use vmlab_hv_tcg_aarch64::sysreg::{self, CpuModel};
use vmlab_hv_tcg_aarch64::timer::Timer;

use super::Virt;
use crate::snapshot::SavedSections;
use crate::vcpu::{VcpuBackend, VcpuState};

/// The layout of [`SavedCpu`]. A change of the layout is a new version.
const CPU_VERSION: u32 = 1;

const POWERED: &str = "virt/powered";
const CLOCK: &str = "virt/clock";
const BRIDGE: &str = "virt/host-bridge";

fn cpu_key(id: usize) -> String {
    format!("virt/cpu{id}")
}

/// The slots of a saved CPU: the register file of version 1, with the
/// flags as four slots of one bit, N to V, at `FLAG_SLOT`. The CPU keeps
/// its flags lazy (card #1142), and a save computes them.
const SAVED_SLOTS: usize = slot::COUNT + 1;
const FLAG_SLOT: usize = slot::FLAGS_KIND;
/// The slots of the CPU that follow the flags, as saved.
const AFTER_FLAGS: usize = slot::FLAGS_B + 1;

/// One CPU as a snapshot keeps it.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SavedCpu {
    version: u32,
    /// The name of the CPU model: a restore into another one refuses.
    model: String,
    /// Each slot as its low and its high 64 bits, in the layout of
    /// `SAVED_SLOTS`.
    slots: Vec<[u64; 2]>,
    el: u8,
    sp_sel: bool,
    daif: u8,
    il: bool,
    pstate13: bool,
    /// The system registers as (row name, value), in the order of the table.
    regs: Vec<(String, u64)>,
    /// The physical and the virtual timer: control and compare value.
    timers: [(u8, u64); 2],
}

impl SavedCpu {
    pub(crate) fn of(state: &State) -> Self {
        let nzcv = state.nzcv();
        let flags = (0..4).rev().map(|bit| u128::from(nzcv >> bit & 1));
        let slots = state.slots[..FLAG_SLOT]
            .iter()
            .copied()
            .chain(flags)
            .chain(state.slots[AFTER_FLAGS..].iter().copied());
        Self {
            version: CPU_VERSION,
            model: state.system.model.name.into(),
            slots: slots
                .map(|value| [value as u64, (value >> 64) as u64])
                .collect(),
            el: state.context.el,
            sp_sel: state.context.sp_sel,
            daif: state.daif,
            il: state.il,
            pstate13: state.pstate13,
            regs: sysreg::ROWS
                .iter()
                .zip(state.system.regs)
                .map(|(row, value)| (row.name.into(), value))
                .collect(),
            timers: state
                .system
                .timers
                .map(|timer| (timer.control, timer.compare)),
        }
    }

    /// The state of a CPU of `model`.
    pub(crate) fn state(&self, model: &'static CpuModel) -> Result<State, String> {
        if self.version != CPU_VERSION {
            return Err(format!(
                "the CPU state has version {}, and this vmlab-hv reads version {CPU_VERSION}",
                self.version
            ));
        }
        if self.model != model.name {
            return Err(format!(
                "the snapshot is of the CPU {}, and this machine has the CPU {}",
                self.model, model.name
            ));
        }
        if self.slots.len() != SAVED_SLOTS
            || self.regs.len() != sysreg::COUNT
            || self
                .regs
                .iter()
                .zip(sysreg::ROWS)
                .any(|((name, _), row)| name != row.name)
        {
            return Err("the snapshot has another register file than this CPU".into());
        }
        if self.el > 1 || self.daif > 0xf || self.timers.iter().any(|(control, _)| *control > 3) {
            return Err("the CPU state is outside what this CPU can be in".into());
        }
        let value = |&[low, high]: &[u64; 2]| u128::from(low) | u128::from(high) << 64;
        let flags = &self.slots[FLAG_SLOT..FLAG_SLOT + 4];
        if flags.iter().any(|saved| value(saved) > 1) {
            return Err("the CPU state is outside what this CPU can be in".into());
        }
        let mut state = State::reset_as(0, model);
        let others = self.slots[..FLAG_SLOT]
            .iter()
            .chain(&self.slots[FLAG_SLOT + 4..]);
        let places = (0..FLAG_SLOT).chain(AFTER_FLAGS..slot::COUNT);
        for (index, saved) in places.zip(others) {
            state.slots[index] = value(saved);
        }
        state.set_nzcv(
            flags
                .iter()
                .fold(0, |nzcv, saved| nzcv << 1 | value(saved) as u8),
        );
        state.context = Context {
            el: self.el,
            sp_sel: self.sp_sel,
            ..state.context
        };
        state.daif = self.daif;
        state.il = self.il;
        state.pstate13 = self.pstate13;
        for (word, (_, value)) in state.system.regs.iter_mut().zip(&self.regs) {
            *word = *value;
        }
        for (timer, (control, compare)) in state.system.timers.iter_mut().zip(self.timers) {
            *timer = Timer { control, compare };
        }
        // FP access and the tag bits of the context follow from the
        // registers.
        state.refresh();
        Ok(state)
    }
}

fn json<T: Serialize + ?Sized>(value: &T, what: &str) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|e| format!("{what}: {e}"))
}

fn section<'a>(sections: &'a SavedSections, key: &str) -> Result<&'a [u8], String> {
    sections
        .get(key)
        .ok_or_else(|| format!("snapshot lacks section {key:?}"))
}

fn parse<T: for<'de> Deserialize<'de>>(sections: &SavedSections, key: &str) -> Result<T, String> {
    serde_json::from_slice(section(sections, key)?).map_err(|e| format!("{key}: {e}"))
}

impl Virt {
    /// The devices of the machine that are no PCI function, in the order of
    /// a restore.
    fn devices(&self) -> Vec<&parking_lot::Mutex<dyn Snapshot>> {
        let mut devices: Vec<&parking_lot::Mutex<dyn Snapshot>> = vec![
            &*self.gic,
            &*self.uart,
            &*self.rtc,
            &*self.ged,
            &*self.fw_cfg,
        ];
        if let Some(flash) = &self.flash {
            devices.push(&**flash);
        }
        devices
    }

    /// The sections of the machine. Every vCPU thread is parked.
    pub(crate) fn save_sections(
        &self,
        sections: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), String> {
        for (id, vcpu) in self.vcpus.iter().enumerate() {
            let VcpuState::Aarch64(state) = vcpu.lock().state()? else {
                return Err(format!("vcpu {id} is no aarch64 CPU"));
            };
            sections.push((cpu_key(id), json(&SavedCpu::of(&state), "CPU state")?));
        }
        sections.push((POWERED.into(), json(&*self.powered.lock(), POWERED)?));
        sections.push((CLOCK.into(), json(&self.clock.now_ns(), CLOCK)?));
        sections.push((BRIDGE.into(), json(&self.bridge.lock().registers, BRIDGE)?));
        for device in self.devices() {
            let device = device.lock();
            let id = device.state_id();
            let state = device.save_state().map_err(|e| format!("{id}: {e}"))?;
            sections.push((id, state));
        }
        Ok(())
    }

    /// Put the machine into the state of `sections`. Every vCPU thread is
    /// parked. Nothing is changed when a section of a CPU cannot be read.
    pub(crate) fn restore_sections(&self, sections: &SavedSections) -> Result<(), String> {
        let mut states = Vec::with_capacity(self.vcpus.len());
        for id in 0..self.vcpus.len() {
            let key = cpu_key(id);
            let saved: SavedCpu = parse(sections, &key)?;
            states.push(saved.state(self.model).map_err(|e| format!("{key}: {e}"))?);
        }
        if sections.get(&cpu_key(self.vcpus.len())).is_some() {
            return Err("the snapshot has more CPUs than this machine".into());
        }
        let powered: Vec<bool> = parse(sections, POWERED)?;
        if powered.len() != self.vcpus.len() {
            return Err(format!("{POWERED}: the snapshot has another count of CPUs"));
        }
        let clock: u64 = parse(sections, CLOCK)?;
        let bridge: [u32; 16] = parse(sections, BRIDGE)?;

        self.clock.set_now_ns(clock);
        self.bridge.lock().registers = bridge;
        for device in self.devices() {
            let mut device = device.lock();
            let id = device.state_id();
            device
                .restore_state(section(sections, &id)?)
                .map_err(|e| format!("{id}: {e}"))?;
        }
        *self.powered.lock() = powered;
        for (vcpu, state) in self.vcpus.iter().zip(states) {
            vcpu.lock()
                .set_state(&VcpuState::Aarch64(Box::new(state)))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_state_comes_back_as_it_was_saved() {
        let mut state = State::reset(0x4000_0000);
        state.set_x(3, 0x1234);
        state.slots[slot::V0 + 5] = u128::MAX - 7;
        state.context.el = 0;
        state.context.sp_sel = false;
        state.daif = 0x5;
        // Lazy flags: the compare of 1 with 2 (N set, C clear), saved as
        // the four bits of version 1.
        state.slots[slot::FLAGS_KIND] = 0b1110;
        state.slots[slot::FLAGS_A] = 1;
        state.slots[slot::FLAGS_B] = 2;
        assert_eq!(state.nzcv(), 0b1000);
        state.system.regs[sysreg::reg::TCR_EL1] = 0x35_5551_0010;
        state.system.regs[sysreg::reg::CPACR_EL1] = 3 << 20;
        state.system.timers[1] = Timer {
            control: Timer::ENABLE,
            compare: 99,
        };
        state.refresh();
        let saved = SavedCpu::of(&state);
        let bytes = serde_json::to_vec(&saved).unwrap();
        let back: SavedCpu = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, saved);
        assert_eq!(saved.slots.len(), 73);
        assert_eq!(saved.slots[35..39], [[1, 0], [0, 0], [0, 0], [0, 0]]);
        assert_eq!(saved.slots[39 + 5], [u64::MAX - 7, u64::MAX]);
        assert_eq!(back.state(&sysreg::V8_0).unwrap(), state);
    }

    #[test]
    fn a_cpu_state_of_another_cpu_or_layout_is_refused() {
        let state = State::reset(0);
        let saved = SavedCpu::of(&state);
        assert!(
            saved
                .state(&sysreg::QEMU_CORTEX_A53)
                .unwrap_err()
                .contains("this machine has the CPU")
        );
        let mut flags = SavedCpu::of(&state);
        flags.slots[FLAG_SLOT] = [2, 0];
        assert!(flags.state(&sysreg::V8_0).is_err());
        let mut short = SavedCpu::of(&state);
        short.regs.pop();
        assert!(short.state(&sysreg::V8_0).is_err());
        let mut level = SavedCpu::of(&state);
        level.el = 3;
        assert!(level.state(&sysreg::V8_0).is_err());
        let mut version = SavedCpu::of(&state);
        version.version = 2;
        assert!(version.state(&sysreg::V8_0).is_err());
    }
}
