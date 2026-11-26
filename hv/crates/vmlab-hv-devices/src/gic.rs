//! The Arm GICv2 interrupt controller and its GICv2m MSI frame, as a device
//! model in the process (DESIGN.md of phase 3d, F3): the controller of an
//! emulated aarch64 machine, where no kernel holds one.
//!
//! Sources: the ARM Generic Interrupt Controller Architecture Specification,
//! architecture version 2.0 (Arm IHI 0048B), chapters 3 and 4, the Server
//! Base System Architecture, appendix E (the GICv2m frame: `MSI_TYPER`,
//! `MSI_SETSPI_NS`, `MSI_IIDR`), and what a running QEMU 11.1.3 answers
//! (`docs/hwref/virt-devices-oracle.json`, recorded by
//! `scripts/virt-devices-oracle.py`). Where the specification leaves a
//! choice, or QEMU differs from it, this model does what QEMU does, because
//! the guests ran there:
//!
//! - No security extensions. `IGROUPR` is read and write. Group 0 goes to
//!   the IRQ line, or to FIQ with `GICC_CTLR.FIQEn`. A group 1 interrupt is
//!   acknowledged through `GICC_IAR` with `AckCtl`: `GICC_AIAR` and
//!   `GICC_AHPPIR` read 0 and `GICC_AEOIR` ignores a write.
//! - One interrupt is the candidate of a CPU: the pending, enabled, not
//!   active one of the highest priority, whatever its group. It is signalled
//!   when its group is enabled in the distributor and in the CPU interface.
//!   An interrupt of a group that is not enabled so hides the ones behind it.
//! - `GICC_HPPIR` shows the candidate also when the running priority keeps
//!   it from being signalled. The priority mask hides it.
//! - The active priorities are the 128 bits of `GICC_APR0` to `GICC_APR3`,
//!   one per group priority. `GICC_RPR` reads the group priority, not the
//!   priority, of the running interrupt.
//! - A write of `GICC_EOIR` drops the running priority also when the
//!   interrupt it names is not active, and does nothing when no priority is
//!   running: the named interrupt then stays active.
//! - With one CPU `GICD_ITARGETSR` reads 0 and ignores a write.
//! - The configuration of a PPI can be written.
//!
//! Not as QEMU: a write of `GICD_SPENDSGIR` sets the SGI that it names, as
//! the specification says. QEMU 11.1.3 shows all four SGIs of the word
//! pending, and stops on an assertion at an acknowledge after a write that
//! names a CPU that the machine does not have.
//!
//! The registers are banked by CPU. A bus access has no CPU in it, so the
//! thread of a vCPU says who it is once with [`set_current_cpu`]. Any other
//! thread reads and writes as CPU 0.

use std::cell::Cell;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{BusDevice, DeviceError, IrqLine, MsiMessage, MsiSink, Resettable, Snapshot};

/// The size of the distributor block.
pub const DISTRIBUTOR_SIZE: u64 = 0x1000;
/// The size of the CPU interface block: `GICC_DIR` is at 0x1000.
pub const CPU_INTERFACE_SIZE: u64 = 0x2000;
/// The size of the GICv2m frame.
pub const V2M_SIZE: u64 = 0x1000;
/// The offset of `MSI_SETSPI_NS` in the frame: the address of a message.
pub const V2M_SETSPI: u64 = 0x40;

/// The most CPUs of a GICv2.
pub const MAX_CPUS: usize = 8;
/// The interrupt IDs below this are private to a CPU: 16 SGIs, 16 PPIs.
pub const PRIVATE: usize = 32;
const SGIS: usize = 16;
const SPURIOUS: u16 = 1023;
/// What `GICC_IAR` reads for a group 1 interrupt without `AckCtl`.
const GROUP1_PENDING: u16 = 1022;
/// No priority is running.
const IDLE: u16 = 0x100;

const SNAPSHOT_MAGIC: &[u8; 5] = b"GIC2\x01";

const CTLR_ACK: u32 = 1 << 2;
const CTLR_FIQ: u32 = 1 << 3;
const CTLR_CBPR: u32 = 1 << 4;
const CTLR_EOIMODE: u32 = 1 << 9;
const CTLR_MASK: u32 = 0x21f;

/// GICD_PIDR4 to 7, PIDR0 to 3 and CIDR0 to 3, from 0xfd0.
const DISTRIBUTOR_IDS: [u8; 12] = [4, 0, 0, 0, 0x90, 0xb4, 0x2b, 0, 0x0d, 0xf0, 0x05, 0xb1];

thread_local! {
    static CURRENT_CPU: Cell<usize> = const { Cell::new(0) };
}

/// Say that this thread runs the vCPU `cpu`: its accesses to the
/// distributor and to the CPU interface see the registers of that CPU.
pub fn set_current_cpu(cpu: usize) {
    CURRENT_CPU.with(|current| current.set(cpu));
}

fn current_cpu() -> usize {
    CURRENT_CPU.with(Cell::get)
}

/// The two interrupt lines into one CPU.
#[derive(Clone)]
pub struct CpuLines {
    pub irq: Arc<dyn IrqLine>,
    pub fiq: Arc<dyn IrqLine>,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Irq {
    enabled: bool,
    /// Pending by an edge or by a write of `GICD_ISPENDR`.
    latch: bool,
    /// The level of the input.
    line: bool,
    active: bool,
    group1: bool,
    edge: bool,
    priority: u8,
    /// The CPUs of an SPI, as `GICD_ITARGETSR` holds them.
    targets: u8,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cpu {
    private: [Irq; PRIVATE],
    /// For each SGI, the CPUs that sent it and wait for its acknowledge.
    sgi_sources: [u8; SGIS],
    ctlr: u32,
    pmr: u8,
    bpr: u8,
    abpr: u8,
    apr: [u32; 4],
    nsapr: [u32; 4],
    /// The candidate interrupt, kept by `update`.
    candidate: u16,
}

impl Cpu {
    fn reset() -> Self {
        let mut private = [Irq::default(); PRIVATE];
        for sgi in &mut private[..SGIS] {
            sgi.enabled = true;
            sgi.edge = true;
        }
        Self {
            private,
            sgi_sources: [0; SGIS],
            ctlr: 0,
            pmr: 0,
            bpr: 0,
            abpr: 1,
            apr: [0; 4],
            nsapr: [0; 4],
            candidate: SPURIOUS,
        }
    }

    /// The group priority that runs, or `IDLE`.
    fn running(&self) -> u16 {
        self.apr
            .iter()
            .enumerate()
            .find(|(_, word)| **word != 0)
            .map_or(IDLE, |(index, word)| {
                ((index as u16 * 32) + word.trailing_zeros() as u16) << 1
            })
    }

    fn drop_running(&mut self) {
        if let Some(word) = self.apr.iter_mut().find(|word| **word != 0) {
            *word &= *word - 1;
        }
    }

    /// The binary point that splits the priority of an interrupt of `group1`.
    fn binary_point(&self, group1: bool) -> u8 {
        if group1 && self.ctlr & CTLR_CBPR == 0 {
            self.abpr
        } else {
            self.bpr
        }
    }

    fn group_priority(&self, irq: &Irq) -> u16 {
        let low = (1u16 << (self.binary_point(irq.group1) + 1)) - 1;
        u16::from(irq.priority) & !low
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    ctlr: u32,
    cpus: Vec<Cpu>,
    spis: Vec<Irq>,
}

/// The distributor and the CPU interfaces of one GICv2.
pub struct Gic {
    id: String,
    state: State,
    lines: Vec<CpuLines>,
    levels: Vec<[Option<bool>; 2]>,
}

impl Gic {
    /// A controller for the CPUs of `lines` with `spis` shared interrupts.
    /// `spis` is a multiple of 32, and no more than 988.
    pub fn new(id: impl Into<String>, lines: Vec<CpuLines>, spis: usize) -> Arc<Mutex<Self>> {
        assert!(
            (1..=MAX_CPUS).contains(&lines.len()),
            "a GICv2 has 1 to 8 CPUs"
        );
        assert!(
            spis % 32 == 0 && spis <= 988,
            "the SPIs of a GICv2 come in words of 32, up to INTID 1019"
        );
        let cpus = lines.len();
        let mut gic = Self {
            id: id.into(),
            state: State {
                ctlr: 0,
                cpus: vec![Cpu::reset(); cpus],
                spis: vec![Irq::default(); spis],
            },
            levels: vec![[None; 2]; cpus],
            lines,
        };
        gic.update();
        Arc::new(Mutex::new(gic))
    }

    fn interrupts(&self) -> usize {
        PRIVATE + self.state.spis.len()
    }

    fn cpu_mask(&self) -> u8 {
        ((1u16 << self.state.cpus.len()) - 1) as u8
    }

    fn irq(&self, cpu: usize, n: usize) -> &Irq {
        if n < PRIVATE {
            &self.state.cpus[cpu].private[n]
        } else {
            &self.state.spis[n - PRIVATE]
        }
    }

    fn irq_mut(&mut self, cpu: usize, n: usize) -> &mut Irq {
        if n < PRIVATE {
            &mut self.state.cpus[cpu].private[n]
        } else {
            &mut self.state.spis[n - PRIVATE]
        }
    }

    fn pending(&self, cpu: usize, n: usize) -> bool {
        if n < SGIS {
            return self.state.cpus[cpu].sgi_sources[n] != 0;
        }
        let irq = self.irq(cpu, n);
        irq.latch || (!irq.edge && irq.line)
    }

    /// Whether the SPI or private interrupt `n` can go to `cpu`.
    fn targets(&self, cpu: usize, n: usize) -> bool {
        n < PRIVATE || self.state.cpus.len() == 1 || self.irq(cpu, n).targets & (1 << cpu) != 0
    }

    /// Whether the candidate `irq` of `cpu` is signalled: its group is
    /// enabled twice and its group priority is above the running one.
    fn signalled(&self, cpu: usize, irq: &Irq) -> bool {
        let state = &self.state.cpus[cpu];
        let group = 1u32 << u32::from(irq.group1);
        self.state.ctlr & group != 0
            && state.ctlr & group != 0
            && state.group_priority(irq) < state.running()
    }

    /// Find the candidate of each CPU again and drive its lines.
    fn update(&mut self) {
        for cpu in 0..self.state.cpus.len() {
            let state = &self.state.cpus[cpu];
            let mut candidate = SPURIOUS;
            let (mut irq_level, mut fiq_level) = (false, false);
            if self.state.ctlr & 3 != 0 && state.ctlr & 3 != 0 {
                let mut best = (0x100u16, SPURIOUS);
                for n in 0..self.interrupts() {
                    let irq = self.irq(cpu, n);
                    if irq.enabled
                        && !irq.active
                        && u16::from(irq.priority) < best.0
                        && self.pending(cpu, n)
                        && self.targets(cpu, n)
                    {
                        best = (u16::from(irq.priority), n as u16);
                    }
                }
                if best.0 < u16::from(state.pmr) {
                    candidate = best.1;
                    let irq = self.irq(cpu, usize::from(candidate));
                    if self.signalled(cpu, irq) {
                        if !irq.group1 && state.ctlr & CTLR_FIQ != 0 {
                            fiq_level = true;
                        } else {
                            irq_level = true;
                        }
                    }
                }
            }
            self.state.cpus[cpu].candidate = candidate;
            let lines = &self.lines[cpu];
            for (slot, (line, level)) in [(&lines.irq, irq_level), (&lines.fiq, fiq_level)]
                .into_iter()
                .enumerate()
            {
                if self.levels[cpu][slot] != Some(level) {
                    self.levels[cpu][slot] = Some(level);
                    line.set_level(level);
                }
            }
        }
    }

    fn set_line(&mut self, cpu: usize, n: usize, high: bool) {
        let irq = self.irq_mut(cpu, n);
        if high && !irq.line && irq.edge {
            irq.latch = true;
        }
        irq.line = high;
        self.update();
    }

    /// Drive the input of the shared interrupt `spi` (INTID `spi` + 32).
    pub fn set_spi(&mut self, spi: usize, high: bool) {
        if spi < self.state.spis.len() {
            self.set_line(0, PRIVATE + spi, high);
        }
    }

    /// Drive the input of the private interrupt `ppi` (INTID 16 to 31) of
    /// `cpu`.
    pub fn set_ppi(&mut self, cpu: usize, ppi: usize, high: bool) {
        if cpu < self.state.cpus.len() && (SGIS..PRIVATE).contains(&ppi) {
            self.set_line(cpu, ppi, high);
        }
    }

    /// The input of the shared interrupt `spi` as a line for a device.
    pub fn spi_line(gic: &Arc<Mutex<Self>>, spi: usize) -> Arc<dyn IrqLine> {
        Arc::new(Input {
            gic: gic.clone(),
            cpu: 0,
            interrupt: PRIVATE + spi,
        })
    }

    /// The input of the private interrupt `ppi` of `cpu` as a line.
    pub fn ppi_line(gic: &Arc<Mutex<Self>>, cpu: usize, ppi: usize) -> Arc<dyn IrqLine> {
        Arc::new(Input {
            gic: gic.clone(),
            cpu,
            interrupt: ppi,
        })
    }

    // ---- the CPU interface ---------------------------------------------

    fn acknowledge(&mut self, cpu: usize) -> u32 {
        let candidate = self.state.cpus[cpu].candidate;
        if candidate == SPURIOUS {
            return u32::from(SPURIOUS);
        }
        let n = usize::from(candidate);
        let irq = *self.irq(cpu, n);
        if irq.group1 && self.state.cpus[cpu].ctlr & CTLR_ACK == 0 {
            return u32::from(GROUP1_PENDING);
        }
        if !self.signalled(cpu, &irq) {
            return u32::from(SPURIOUS);
        }
        let mut value = u32::from(candidate);
        let state = &mut self.state.cpus[cpu];
        let level = usize::from(state.group_priority(&irq) >> 1);
        state.apr[level / 32] |= 1 << (level % 32);
        if n < SGIS {
            let source = state.sgi_sources[n].trailing_zeros();
            state.sgi_sources[n] &= !(1 << source);
            value |= source << 10;
        }
        let irq = self.irq_mut(cpu, n);
        irq.latch = false;
        irq.active = true;
        self.update();
        value
    }

    fn highest_pending(&self, cpu: usize) -> u32 {
        let state = &self.state.cpus[cpu];
        let n = usize::from(state.candidate);
        if state.candidate == SPURIOUS {
            return u32::from(SPURIOUS);
        }
        if self.irq(cpu, n).group1 && state.ctlr & CTLR_ACK == 0 {
            return u32::from(GROUP1_PENDING);
        }
        let source = if n < SGIS {
            state.sgi_sources[n].trailing_zeros()
        } else {
            0
        };
        u32::from(state.candidate) | source << 10
    }

    fn end_of_interrupt(&mut self, cpu: usize, value: u32) {
        let n = (value & 0x3ff) as usize;
        let state = &mut self.state.cpus[cpu];
        if n >= PRIVATE + self.state.spis.len() || state.running() == IDLE {
            return;
        }
        state.drop_running();
        if state.ctlr & CTLR_EOIMODE == 0 {
            self.irq_mut(cpu, n).active = false;
        }
        self.update();
    }

    fn deactivate(&mut self, cpu: usize, value: u32) {
        let n = (value & 0x3ff) as usize;
        if n < self.interrupts() && self.state.cpus[cpu].ctlr & CTLR_EOIMODE != 0 {
            self.irq_mut(cpu, n).active = false;
            self.update();
        }
    }

    fn cpu_read(&mut self, cpu: usize, offset: u64) -> u32 {
        let state = &self.state.cpus[cpu];
        match offset {
            0x00 => state.ctlr,
            0x04 => u32::from(state.pmr),
            0x08 => u32::from(state.bpr),
            0x0c => self.acknowledge(cpu),
            0x14 => u32::from(state.running().min(0xff)),
            0x18 => self.highest_pending(cpu),
            0x1c => u32::from(state.abpr),
            0xd0..=0xdc => state.apr[((offset - 0xd0) / 4) as usize],
            0xe0..=0xec => state.nsapr[((offset - 0xe0) / 4) as usize],
            0xfc => 0x0002_043b,
            _ => 0,
        }
    }

    fn cpu_write(&mut self, cpu: usize, offset: u64, value: u32) {
        let state = &mut self.state.cpus[cpu];
        match offset {
            0x00 => state.ctlr = value & CTLR_MASK,
            0x04 => state.pmr = value as u8,
            0x08 => state.bpr = value as u8 & 7,
            0x10 => return self.end_of_interrupt(cpu, value),
            0x1c => state.abpr = (value as u8 & 7).max(1),
            0xd0..=0xdc => state.apr[((offset - 0xd0) / 4) as usize] = value,
            0xe0..=0xec => state.nsapr[((offset - 0xe0) / 4) as usize] = value,
            0x1000 => return self.deactivate(cpu, value),
            _ => return,
        }
        self.update();
    }

    // ---- the distributor -----------------------------------------------

    /// One word of a register that has a bit per interrupt.
    fn bits(&self, word: usize, bit: impl Fn(&Self, usize) -> bool) -> u32 {
        (0..32)
            .filter(|index| {
                let n = word * 32 + index;
                n < self.interrupts() && bit(self, n)
            })
            .fold(0, |value, index| value | 1 << index)
    }

    /// Run `change` for each interrupt that `value` names in the word.
    fn each(&mut self, cpu: usize, word: usize, value: u32, change: impl Fn(&mut Irq, usize)) {
        for index in 0..32 {
            let n = word * 32 + index;
            if value & (1 << index) != 0 && n < self.interrupts() {
                change(self.irq_mut(cpu, n), n);
            }
        }
    }

    fn configuration(&self, cpu: usize, word: usize) -> u32 {
        (0..16)
            .filter(|index| {
                let n = word * 16 + index;
                n < self.interrupts() && self.irq(cpu, n).edge
            })
            .fold(0, |value, index| value | 2 << (index * 2))
    }

    fn distributor_read(&self, cpu: usize, offset: u64) -> u32 {
        let word = ((offset & 0x7f) / 4) as usize;
        match offset {
            0x000 => self.state.ctlr,
            0x004 => (self.interrupts() / 32 - 1) as u32 | ((self.state.cpus.len() - 1) as u32) << 5,
            0x008 => 0x43b,
            0x080..=0x0fc => self.bits(word, |gic, n| gic.irq(cpu, n).group1),
            0x100..=0x1fc => self.bits(word, |gic, n| gic.irq(cpu, n).enabled),
            0x200..=0x2fc => self.bits(word, |gic, n| gic.pending(cpu, n)),
            0x300..=0x3fc => self.bits(word, |gic, n| gic.irq(cpu, n).active),
            0x400..=0x7fc | 0x800..=0xbfc | 0xf10..=0xf2c => {
                let bytes: [u8; 4] =
                    std::array::from_fn(|index| self.distributor_byte(cpu, offset + index as u64));
                u32::from_le_bytes(bytes)
            }
            0xc00..=0xcfc => self.configuration(cpu, ((offset - 0xc00) / 4) as usize),
            0xfd0..=0xffc => u32::from(DISTRIBUTOR_IDS[((offset - 0xfd0) / 4) as usize]),
            _ => 0,
        }
    }

    /// One byte of the registers that have a byte per interrupt.
    fn distributor_byte(&self, cpu: usize, offset: u64) -> u8 {
        match offset {
            0x400..=0x7ff => {
                let n = (offset - 0x400) as usize;
                if n < self.interrupts() {
                    self.irq(cpu, n).priority
                } else {
                    0
                }
            }
            0x800..=0xbff => {
                let n = (offset - 0x800) as usize;
                if self.state.cpus.len() == 1 || n >= self.interrupts() {
                    0
                } else if n < PRIVATE {
                    1 << cpu
                } else {
                    self.irq(cpu, n).targets
                }
            }
            0xf10..=0xf1f | 0xf20..=0xf2f => self.state.cpus[cpu].sgi_sources[(offset & 0xf) as usize],
            _ => 0,
        }
    }

    fn distributor_write_byte(&mut self, cpu: usize, offset: u64, value: u8) {
        let mask = self.cpu_mask();
        match offset {
            0x400..=0x7ff => {
                let n = (offset - 0x400) as usize;
                if n < self.interrupts() {
                    self.irq_mut(cpu, n).priority = value;
                }
            }
            0x800..=0xbff => {
                let n = (offset - 0x800) as usize;
                if self.state.cpus.len() > 1 && (PRIVATE..self.interrupts()).contains(&n) {
                    self.irq_mut(cpu, n).targets = value & mask;
                }
            }
            0xf10..=0xf1f => self.state.cpus[cpu].sgi_sources[(offset & 0xf) as usize] &= !value,
            0xf20..=0xf2f => {
                self.state.cpus[cpu].sgi_sources[(offset & 0xf) as usize] |= value & mask;
            }
            _ => {}
        }
    }

    fn distributor_write(&mut self, cpu: usize, offset: u64, value: u32) {
        let word = ((offset & 0x7f) / 4) as usize;
        match offset {
            0x000 => self.state.ctlr = value & 3,
            0x080..=0x0fc => {
                for index in 0..32 {
                    let n = word * 32 + index;
                    if n < self.interrupts() {
                        self.irq_mut(cpu, n).group1 = value & (1 << index) != 0;
                    }
                }
            }
            0x100..=0x17c => self.each(cpu, word, value, |irq, _| irq.enabled = true),
            0x180..=0x1fc => self.each(cpu, word, value, |irq, n| irq.enabled = n < SGIS),
            0x200..=0x27c => self.each(cpu, word, value, |irq, n| irq.latch |= n >= SGIS),
            0x280..=0x2fc => self.each(cpu, word, value, |irq, _| irq.latch = false),
            0x300..=0x37c => self.each(cpu, word, value, |irq, _| irq.active = true),
            0x380..=0x3fc => self.each(cpu, word, value, |irq, _| irq.active = false),
            0xc00..=0xcfc => {
                let word = ((offset - 0xc00) / 4) as usize;
                for index in 0..16 {
                    let n = word * 16 + index;
                    if (SGIS..self.interrupts()).contains(&n) {
                        self.irq_mut(cpu, n).edge = value & (2 << (index * 2)) != 0;
                    }
                }
            }
            0xf00 => self.software_interrupt(cpu, value),
            _ => {}
        }
    }

    fn software_interrupt(&mut self, cpu: usize, value: u32) {
        let id = (value & 0xf) as usize;
        let targets = match (value >> 24) & 3 {
            0 => (value >> 16) as u8,
            1 => !(1 << cpu),
            2 => 1 << cpu,
            _ => 0,
        } & self.cpu_mask();
        for target in 0..self.state.cpus.len() {
            if targets & (1 << target) != 0 {
                self.state.cpus[target].sgi_sources[id] |= 1 << cpu;
            }
        }
    }
}

/// The bytes of `data` as a register value, zero-extended.
fn value_of(data: &[u8]) -> u32 {
    let mut value = [0; 4];
    for (value, byte) in value.iter_mut().zip(data) {
        *value = *byte;
    }
    u32::from_le_bytes(value)
}

/// Give `data` the bytes of the register `value` from the byte `offset & 3`.
fn fill(data: &mut [u8], offset: u64, value: u32) {
    let value = (u64::from(value) >> ((offset & 3) * 8)).to_le_bytes();
    for (byte, value) in data.iter_mut().zip(value) {
        *byte = value;
    }
}

struct Input {
    gic: Arc<Mutex<Gic>>,
    cpu: usize,
    interrupt: usize,
}

impl IrqLine for Input {
    fn set_level(&self, high: bool) {
        let mut gic = self.gic.lock();
        if self.interrupt < PRIVATE {
            gic.set_ppi(self.cpu, self.interrupt, high);
        } else {
            gic.set_spi(self.interrupt - PRIVATE, high);
        }
    }
}

/// The distributor block of a [`Gic`] on the bus.
pub struct Distributor(pub Arc<Mutex<Gic>>);

impl BusDevice for Distributor {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let gic = self.0.lock();
        let cpu = current_cpu().min(gic.state.cpus.len() - 1);
        fill(data, offset, gic.distributor_read(cpu, offset & !3));
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut gic = self.0.lock();
        let cpu = current_cpu().min(gic.state.cpus.len() - 1);
        match offset {
            // The registers that the architecture lets a byte access reach.
            0x400..=0xbff | 0xf10..=0xf2f => {
                for (index, byte) in data.iter().enumerate() {
                    gic.distributor_write_byte(cpu, offset + index as u64, *byte);
                }
            }
            _ if offset & 3 == 0 => gic.distributor_write(cpu, offset, value_of(data)),
            _ => {}
        }
        gic.update();
    }
}

/// The CPU interface block of a [`Gic`] on the bus.
pub struct CpuInterface(pub Arc<Mutex<Gic>>);

impl BusDevice for CpuInterface {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let mut gic = self.0.lock();
        let cpu = current_cpu().min(gic.state.cpus.len() - 1);
        let value = gic.cpu_read(cpu, offset & !3);
        fill(data, offset, value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset & 3 != 0 {
            return;
        }
        let mut gic = self.0.lock();
        let cpu = current_cpu().min(gic.state.cpus.len() - 1);
        gic.cpu_write(cpu, offset, value_of(data));
    }
}

impl Resettable for Gic {
    /// The inputs go low with the rest: each device drives its line again
    /// when it is reset.
    fn reset(&mut self) -> Result<(), DeviceError> {
        for cpu in &mut self.state.cpus {
            *cpu = Cpu::reset();
        }
        self.state.ctlr = 0;
        self.state.spis.fill(Irq::default());
        self.update();
        Ok(())
    }
}

impl Snapshot for Gic {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.extend(
            serde_json::to_vec(&self.state)
                .map_err(|error| DeviceError::State(error.to_string()))?,
        );
        Ok(bytes)
    }

    fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
        let invalid = |what: &str| DeviceError::State(format!("{}: {what}", self.id));
        let Some(body) = bytes.strip_prefix(SNAPSHOT_MAGIC) else {
            return Err(invalid("invalid GIC state or version"));
        };
        let state: State =
            serde_json::from_slice(body).map_err(|error| invalid(&error.to_string()))?;
        if state.cpus.len() != self.state.cpus.len() || state.spis.len() != self.state.spis.len() {
            return Err(invalid("the state is of a GIC of another size"));
        }
        if state
            .cpus
            .iter()
            .any(|cpu| cpu.bpr > 7 || !(1..=7).contains(&cpu.abpr))
        {
            return Err(invalid("a binary point is out of range"));
        }
        self.state = state;
        self.levels.fill([None; 2]);
        self.update();
        Ok(())
    }
}

/// The GICv2m frame: a write of an interrupt ID to `MSI_SETSPI_NS` pulses
/// that SPI. It is also the [`MsiSink`] of the machine, for a device model
/// that sends its message without a bus write.
#[derive(Clone)]
pub struct V2m {
    gic: Arc<Mutex<Gic>>,
    /// The guest physical address of the frame.
    base: u64,
    /// The first interrupt ID of the frame, and how many it has.
    first: u32,
    count: u32,
}

impl V2m {
    /// The frame at the guest address `base`, for the interrupt IDs `first`
    /// to `first + count - 1`. The guest must make them edge-triggered, as
    /// its v2m driver does: a pulse leaves nothing on a level input.
    pub fn new(gic: Arc<Mutex<Gic>>, base: u64, first: u32, count: u32) -> Self {
        Self {
            gic,
            base,
            first,
            count,
        }
    }

    fn message(&self, value: u32) {
        let id = value & 0x3ff;
        if (self.first..self.first + self.count).contains(&id) {
            let spi = id as usize - PRIVATE;
            let mut gic = self.gic.lock();
            gic.set_spi(spi, true);
            gic.set_spi(spi, false);
        }
    }
}

impl BusDevice for V2m {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = match offset & !3 {
            // MSI_TYPER
            0x008 => self.first << 16 | self.count,
            // MSI_IIDR
            0xfcc => 0x0510_0000,
            _ => 0,
        };
        fill(data, offset, value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset == V2M_SETSPI && data.len() >= 2 {
            self.message(value_of(data));
        }
    }
}

impl MsiSink for V2m {
    fn signal(&self, msg: MsiMessage) {
        if msg.address == self.base + V2M_SETSPI {
            self.message(msg.data);
        }
    }
}
