//! The QEMU qtest oracle for the devices of the aarch64 `virt` machine
//! (`docs/hwref/virt-devices-oracle.json`, recorded by
//! `scripts/virt-devices-oracle.py`): the GICv2, the GICv2m frame, the
//! PL011, the PL031 and the GED. The test replays each recorded access on
//! our models and compares every read, every interrupt level and edge, and
//! every byte of serial output.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::ged::Ged;
use crate::gic::{self, CpuInterface, CpuLines, Distributor, Gic, V2m};
use crate::pl011::Pl011;
use crate::pl031::{self, Pl031};
use crate::qtest_oracle::Comparison;
use crate::serial::SerialOut;
use crate::{BusDevice, IrqLine, MsiMessage, MsiSink, Resettable, Snapshot};

const ORACLE: &str = include_str!("../../../docs/hwref/virt-devices-oracle.json");

const GICD: u64 = 0x0800_0000;
const GICC: u64 = 0x0801_0000;
const V2M: u64 = 0x0802_0000;
const PL011: u64 = 0x0900_0000;
const PL031: u64 = 0x0901_0000;
const GED: u64 = 0x0908_0000;

/// The clock of the oracle: `-rtc base=2020-01-01T00:00:00,clock=vm`.
const EPOCH: u32 = 1_577_836_800;
/// The inputs of the GIC that the machine gives the three devices.
const SPI_PL011: usize = 1;
const SPI_PL031: usize = 2;
const SPI_GED: usize = 9;

/// The levels and the edges of the lines that the oracle intercepted.
#[derive(Default)]
struct Lines {
    high: Mutex<Vec<u32>>,
    edges: Mutex<Vec<(u32, bool)>>,
}

struct Line(Arc<Lines>, u32);

impl IrqLine for Line {
    fn set_level(&self, high: bool) {
        let mut levels = self.0.high.lock();
        let was = levels.contains(&self.1);
        if was == high {
            return;
        }
        if high {
            levels.push(self.1);
            levels.sort_unstable();
        } else {
            levels.retain(|line| *line != self.1);
        }
        self.0.edges.lock().push((self.1, high));
    }
}

#[derive(Default)]
struct TestClock(AtomicU64);

impl pl031::Clock for TestClock {
    fn now_ns(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The devices at their addresses on `virt`.
struct Machine {
    lines: Arc<Lines>,
    clock: Arc<TestClock>,
    output: Output,
    gic: Arc<Mutex<Gic>>,
    distributor: Distributor,
    interface: CpuInterface,
    v2m: V2m,
    uart: Pl011,
    rtc: Pl031,
    ged: Ged,
    /// Bytes that the host sent and the UART has no room for yet.
    backlog: VecDeque<u8>,
}

impl Machine {
    /// `inputs`: the oracle intercepted the inputs of the GIC, so the lines
    /// of the devices are compared. Else its outputs, IRQ and FIQ.
    fn new(inputs: bool) -> Self {
        let lines = Arc::new(Lines::default());
        let line = |number| -> Arc<dyn IrqLine> { Arc::new(Line(lines.clone(), number)) };
        let ignored = || -> Arc<dyn IrqLine> { Arc::new(Line(Arc::default(), 0)) };
        let outputs = if inputs {
            CpuLines {
                irq: ignored(),
                fiq: ignored(),
            }
        } else {
            CpuLines {
                irq: line(0),
                fiq: line(1),
            }
        };
        let gic = Gic::new("gic", vec![outputs], 256);
        let device = |spi: usize| {
            if inputs {
                line(spi as u32)
            } else {
                Gic::spi_line(&gic, spi)
            }
        };
        let clock = Arc::new(TestClock::default());
        let output = Output::default();
        Self {
            distributor: Distributor(gic.clone()),
            interface: CpuInterface(gic.clone()),
            v2m: V2m::new(gic.clone(), V2M, 80, 64),
            uart: Pl011::new(
                "pl011",
                SerialOut::new(Box::new(output.clone())),
                device(SPI_PL011),
            ),
            rtc: Pl031::new("pl031", EPOCH, clock.clone(), device(SPI_PL031)),
            ged: Ged::new("ged", device(SPI_GED)),
            gic,
            lines,
            clock,
            output,
            backlog: VecDeque::new(),
        }
    }

    fn device(&mut self, address: u64) -> Option<(&mut dyn BusDevice, u64)> {
        let at = |base: u64, size: u64| (base..base + size).contains(&address);
        Some(if at(GICD, gic::DISTRIBUTOR_SIZE) {
            (&mut self.distributor, address - GICD)
        } else if at(GICC, gic::CPU_INTERFACE_SIZE) {
            (&mut self.interface, address - GICC)
        } else if at(V2M, gic::V2M_SIZE) {
            (&mut self.v2m, address - V2M)
        } else if at(PL011, crate::pl011::SIZE) {
            (&mut self.uart, address - PL011)
        } else if at(PL031, pl031::SIZE) {
            (&mut self.rtc, address - PL031)
        } else if at(GED, crate::ged::SIZE) {
            (&mut self.ged, address - GED)
        } else {
            return None;
        })
    }

    fn read(&mut self, address: u64, width: usize) -> u64 {
        let mut data = [0; 8];
        if let Some((device, offset)) = self.device(address) {
            device.read(offset, &mut data[..width]);
        }
        u64::from_le_bytes(data)
    }

    fn write(&mut self, address: u64, width: usize, value: u64) {
        if let Some((device, offset)) = self.device(address) {
            device.write(offset, &value.to_le_bytes()[..width]);
        }
    }

    /// Give the UART what it has room for. QEMU does this from its main
    /// loop, so a byte that waited is in the FIFO at the next access.
    fn deliver(&mut self) {
        while let Some(&byte) = self.backlog.front() {
            if self.uart.receive(&[byte]) == 0 {
                break;
            }
            self.backlog.pop_front();
        }
    }
}

fn replay(configuration: &str) -> usize {
    let oracle: serde_json::Value = serde_json::from_str(ORACLE).unwrap();
    assert!(
        oracle.get("error").is_none(),
        "the oracle recorded an error"
    );
    let run = oracle["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["configuration"] == configuration)
        .unwrap_or_else(|| panic!("no {configuration} run"));
    let mut machine = Machine::new(run["intercept"] == "in");
    let mut compare = Comparison::new(configuration);
    for step in run["steps"].as_array().unwrap() {
        let number = |key: &str| step[key].as_u64().unwrap();
        let op = step["op"].as_str().unwrap();
        if op == "note" {
            compare.section(step["text"].as_str().unwrap());
            continue;
        }
        machine.deliver();
        match op {
            "read" => {
                let value = machine.read(number("address"), number("width") as usize);
                compare.check(
                    &format!("read {:#x}", number("address")),
                    value,
                    number("value"),
                );
            }
            "write" => {
                machine.write(number("address"), number("width") as usize, number("value"));
                let sent: String = std::mem::take(&mut *machine.output.0.lock())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                compare.check(
                    &format!("output of the write {:#x}", number("address")),
                    sent.as_str(),
                    step["tx"].as_str().unwrap_or(""),
                );
            }
            "line" => {
                let (input, high) = (number("input") as usize, step["high"].as_bool().unwrap());
                let mut gic = machine.gic.lock();
                match input.checked_sub(256) {
                    Some(private) => gic.set_ppi(0, private, high),
                    None => gic.set_spi(input, high),
                }
            }
            "step" => {
                machine.clock.0.fetch_add(number("ns"), Ordering::SeqCst);
                machine.rtc.tick();
            }
            "rx" => {
                let text = step["hex"].as_str().unwrap();
                machine.backlog.extend(
                    (0..text.len())
                        .step_by(2)
                        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap()),
                );
                machine.deliver();
            }
            "powerdown" => machine.ged.power_down(),
            // What QMP said: not a state of a device.
            "events" => continue,
            other => panic!("unknown oracle op {other}"),
        }
        let numbers = |key: &str| -> Vec<serde_json::Value> {
            step.get(key)
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default()
        };
        let high: Vec<u32> = numbers("irq")
            .iter()
            .map(|line| line.as_u64().unwrap() as u32)
            .collect();
        compare.check(
            &format!("lines high after {op}"),
            machine.lines.high.lock().clone(),
            high,
        );
        let edges: Vec<(u32, bool)> = numbers("edges")
            .iter()
            .map(|edge| (edge[0].as_u64().unwrap() as u32, edge[1].as_bool().unwrap()))
            .collect();
        compare.check(
            &format!("edges of {op}"),
            std::mem::take(&mut *machine.lines.edges.lock()),
            edges,
        );
    }
    compare.finish()
}

#[test]
fn pl011_answers_as_qemu() {
    assert!(replay("pl011") > 1000);
}

#[test]
fn pl031_answers_as_qemu() {
    assert!(replay("pl031") > 200);
}

#[test]
fn ged_answers_as_qemu() {
    assert!(replay("ged") > 50);
}

#[test]
fn gic_answers_as_qemu() {
    assert!(replay("gic") > 2000);
}

#[test]
fn gicv2m_answers_as_qemu() {
    assert!(replay("gicv2m") > 200);
}

#[test]
fn a_message_through_the_sink_is_a_write_of_the_frame() {
    let mut machine = Machine::new(false);
    machine.write(GICD, 4, 1);
    machine.write(GICC, 4, 1);
    machine.write(GICC + 4, 4, 0xff);
    // INTID 115: edge, enabled.
    machine.write(GICD + 0xc1c, 4, 0xaaaa_aaaa);
    machine.write(GICD + 0x10c, 4, 1 << (115 - 96));
    let message = |address, data| machine.v2m.signal(MsiMessage { address, data });
    message(V2M + 0x44, 115);
    message(V2M + gic::V2M_SETSPI, 144);
    assert!(machine.lines.high.lock().is_empty());
    message(V2M + gic::V2M_SETSPI, 115);
    assert_eq!(*machine.lines.high.lock(), [0]);
    assert_eq!(machine.read(GICC + 0xc, 4), 115);
}

#[test]
fn each_cpu_has_its_private_interrupts_and_its_interface() {
    let lines = Arc::new(Lines::default());
    let cpu = |number: u32| CpuLines {
        irq: Arc::new(Line(lines.clone(), number)),
        fiq: Arc::new(Line(Arc::default(), 0)),
    };
    let gic = Gic::new("gic", vec![cpu(0), cpu(1)], 32);
    let (mut distributor, mut interface) = (Distributor(gic.clone()), CpuInterface(gic.clone()));
    let read = |device: &mut dyn BusDevice, offset| {
        let mut data = [0; 4];
        device.read(offset, &mut data);
        u32::from_le_bytes(data)
    };
    let write =
        |device: &mut dyn BusDevice, offset, value: u32| device.write(offset, &value.to_le_bytes());
    // Two CPUs and 64 interrupt IDs.
    assert_eq!(read(&mut distributor, 0x004), 1 << 5 | 1);
    write(&mut distributor, 0x000, 1);
    for number in [0, 1] {
        gic::set_current_cpu(number);
        write(&mut interface, 0x00, 1);
        write(&mut interface, 0x04, 0xff);
        // The targets of a private interrupt are the CPU that reads them.
        assert_eq!(read(&mut distributor, 0x800), 0x0101_0101 << number);
    }
    // PPI 27 of CPU 1 is enabled on CPU 1 alone and reaches CPU 1 alone.
    write(&mut distributor, 0x100, 1 << 27);
    assert_eq!(read(&mut distributor, 0x100), 1 << 27 | 0xffff);
    gic::set_current_cpu(0);
    assert_eq!(read(&mut distributor, 0x100), 0xffff);
    Gic::ppi_line(&gic, 1, 27).set_level(true);
    assert_eq!(*lines.high.lock(), [1]);
    assert_eq!(read(&mut interface, 0x0c), 1023);
    gic::set_current_cpu(1);
    assert_eq!(read(&mut interface, 0x0c), 27);
    Gic::ppi_line(&gic, 1, 27).set_level(false);
    write(&mut interface, 0x10, 27);
    // An SPI goes to the CPUs of its target byte.
    write(&mut distributor, 0x104, 1);
    Gic::spi_line(&gic, 0).set_level(true);
    assert!(lines.high.lock().is_empty());
    write(&mut distributor, 0x820, 0xff);
    assert_eq!(read(&mut distributor, 0x820), 3);
    assert_eq!(*lines.high.lock(), [0, 1]);
    write(&mut distributor, 0x820, 2);
    assert_eq!(*lines.high.lock(), [1]);
    Gic::spi_line(&gic, 0).set_level(false);
    // CPU 1 sends SGI 5 to CPU 0, which reads the sender in the acknowledge.
    write(&mut distributor, 0xf00, 1 << 16 | 5);
    assert_eq!(*lines.high.lock(), [0]);
    gic::set_current_cpu(0);
    assert_eq!(read(&mut distributor, 0xf24), 2 << 8);
    assert_eq!(read(&mut interface, 0x0c), 1 << 10 | 5);
    write(&mut interface, 0x10, 1 << 10 | 5);
    assert!(lines.high.lock().is_empty());
    // To all but the sender, and the set-pending register by the book.
    write(&mut distributor, 0xf00, 1 << 24 | 6);
    assert_eq!(*lines.high.lock(), [1]);
    write(&mut distributor, 0xf20, 1 << 8);
    assert_eq!(read(&mut distributor, 0xf20), 1 << 8);
    assert_eq!(read(&mut distributor, 0x200), 1 << 1);
    write(&mut distributor, 0xf10, 0xff << 8);
    assert_eq!(read(&mut distributor, 0x200), 0);
}

#[test]
fn state_survives_a_snapshot_and_a_reset_clears_it() {
    let mut machine = Machine::new(false);
    machine.write(GICD, 4, 1);
    machine.write(GICC, 4, 1);
    machine.write(GICC + 4, 4, 0xff);
    machine.write(GICD + 0x100, 4, 0xffff_ffff);
    machine.write(GICD + 0x104, 4, 0xffff_ffff);
    machine.write(PL011 + 0x2c, 4, 0x70);
    machine.write(PL011 + 0x38, 4, 0x10);
    assert_eq!(machine.uart.receive(b"hello"), 5);
    machine.write(PL031 + 0x08, 4, 77);
    machine.write(PL031 + 0x04, 4, 77);
    machine.write(PL031 + 0x10, 4, 1);
    machine.ged.power_down();
    assert_eq!(*machine.lines.high.lock(), [0]);
    assert_eq!(machine.read(GICC + 0x0c, 4), 33);

    let saved = (
        machine.gic.lock().save_state().unwrap(),
        machine.uart.save_state().unwrap(),
        machine.rtc.save_state().unwrap(),
        machine.ged.save_state().unwrap(),
    );
    machine.gic.lock().reset().unwrap();
    machine.uart.reset().unwrap();
    machine.rtc.reset().unwrap();
    machine.ged.reset().unwrap();
    assert_eq!(machine.read(GICD, 4), 0);
    assert_eq!(machine.read(GICD + 0x304, 4), 0);
    assert_eq!(machine.read(PL011 + 0x18, 4), 0x90);
    assert_eq!(machine.read(PL031, 4), u64::from(EPOCH));
    assert_eq!(machine.read(GED, 4), 0);

    machine.uart.restore_state(&saved.1).unwrap();
    machine.rtc.restore_state(&saved.2).unwrap();
    machine.ged.restore_state(&saved.3).unwrap();
    machine.gic.lock().restore_state(&saved.0).unwrap();
    // The UART is acknowledged and active, the clock waits behind it.
    assert_eq!(machine.read(GICD + 0x304, 4), 1 << SPI_PL011);
    assert_eq!(
        machine.read(GICD + 0x204, 4),
        1 << SPI_PL011 | 1 << SPI_PL031
    );
    assert_eq!(machine.read(GICC + 0x14, 4), 0);
    assert_eq!(machine.read(PL011, 4), u64::from(b'h'));
    assert_eq!(machine.read(PL031, 4), 77);
    assert_eq!(machine.read(PL031 + 0x14, 4), 1);
    assert_eq!(machine.read(GED, 4), 2);

    for bad in [&b""[..], b"GIC2\x02{}", b"GIC2\x01{}"] {
        assert!(machine.gic.lock().restore_state(bad).is_err());
        assert!(machine.uart.restore_state(bad).is_err());
        assert!(machine.rtc.restore_state(bad).is_err());
        assert!(machine.ged.restore_state(bad).is_err());
    }
}
