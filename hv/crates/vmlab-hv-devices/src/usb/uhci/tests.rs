use std::sync::Arc;

use vm_memory::{Bytes, GuestAddress};

use super::*;
use crate::qtest_oracle::{self, Comparison, Line, Op};
use crate::usb::Tablet;


/// The PIIX3 USB function as the oracle's qtest machine addresses it:
/// configuration through 0xcf8/0xcfc at 00:01.2, BAR4 wherever the replay
/// places it, and IRQ 11 through PIRQD.
struct Machine {
    uhci: UhciController,
    memory: GuestMemory,
    irq: Arc<Line>,
    address: u32,
}

const BDF: u32 = 0x0a;

impl Machine {
    fn new() -> Self {
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x40_0000)]).unwrap();
        let irq = Arc::new(Line::default());
        let mut uhci = UhciController::new("uhci", memory.clone(), irq.clone());
        uhci.attach(1, Box::new(Tablet::full_speed("28754-0000:00:01.2-1")))
            .unwrap();
        Self {
            uhci,
            memory,
            irq,
            address: 0,
        }
    }

    fn bar(&mut self) -> Option<u16> {
        let command = self.uhci.config_read(1);
        let bar = self.uhci.config_read(8);
        (command & 1 != 0 && bar & 1 != 0).then_some((bar & 0xffe0) as u16)
    }

    fn out(&mut self, port: u16, width: u8, value: u32) {
        let bytes = &value.to_le_bytes()[..usize::from(width)];
        match port {
            0xcf8 => self.address = value,
            0xcfc..=0xcff => {
                if (self.address >> 8) & 0xffff == BDF {
                    self.uhci.config_write(
                        (self.address as usize & 0xfc) / 4,
                        u64::from(port - 0xcfc),
                        bytes,
                    );
                }
            }
            _ => {
                let base = self.bar().expect("UHCI BAR4 is not decoded");
                assert!((base..base + 32).contains(&port), "unrouted port {port:#x}");
                self.uhci.bar_write(4, u64::from(port - base), bytes);
            }
        }
    }

    fn inp(&mut self, port: u16, width: u8) -> u32 {
        let mut bytes = [0u8; 4];
        let data = &mut bytes[..usize::from(width)];
        match port {
            0xcfc..=0xcff if (self.address >> 8) & 0xffff == BDF => {
                let value = self
                    .uhci
                    .config_read((self.address as usize & 0xfc) / 4)
                    .to_le_bytes();
                let offset = usize::from(port - 0xcfc);
                data.copy_from_slice(&value[offset..offset + data.len()]);
            }
            0xcfc..=0xcff => panic!("the replay reads only the UHCI function"),
            _ => {
                let base = self.bar().expect("UHCI BAR4 is not decoded");
                assert!((base..base + 32).contains(&port), "unrouted port {port:#x}");
                self.uhci.bar_read(4, u64::from(port - base), data);
            }
        }
        u32::from_le_bytes(bytes)
    }

    fn replay(&mut self, until: Option<&str>) {
        let run = qtest_oracle::run("uhci");
        let mut comparison = Comparison::new("uhci");
        for step in &run.steps {
            match &step.op {
                Op::Note(text) => {
                    if until == Some(text.as_str()) {
                        break;
                    }
                    comparison.section(text);
                    continue;
                }
                Op::Out { port, width, value } => self.out(*port, *width, *value),
                Op::In { port, width, value } => {
                    let actual = self.inp(*port, *width);
                    comparison.check(&format!("in{width} {port:#x}"), actual, *value);
                }
                Op::Write { address, data } => {
                    self.memory.write_slice(data, GuestAddress(*address)).unwrap();
                }
                Op::Read { address, data } => {
                    let mut actual = vec![0; data.len()];
                    self.memory
                        .read_slice(&mut actual, GuestAddress(*address))
                        .unwrap();
                    comparison.check(&format!("memory {address:#x}"), actual, data.clone());
                }
                Op::Step { ns } => self.uhci.advance(*ns),
            }
            comparison.check("IRQ 11", self.irq.high(), step.irq.contains(&11));
        }
        let compared = comparison.finish();
        assert!(
            until.is_some() || compared > 500,
            "only {compared} oracle values compared"
        );
    }
}

#[test]
fn qemu_oracle_uhci_with_the_full_speed_tablet() {
    Machine::new().replay(None);
}

/// Snapshot an addressed, configured tablet with a NAKing interrupt TD,
/// restore into a fresh controller, inject input and see the report land.
#[test]
fn snapshot_round_trip_keeps_the_schedule_and_the_device_address() {
    let mut machine = Machine::new();
    machine.replay(Some("request to an absent address times out"));
    let td = 0x101100u64;
    let mut control = [0; 4];
    machine.memory.read_slice(&mut control, GuestAddress(td + 4)).unwrap();
    assert_eq!(u32::from_le_bytes(control), 0x1988_0000, "NAKing interrupt TD");
    let state = machine.uhci.save_state().unwrap();

    let mut restored = Machine::new();
    restored.memory = machine.memory.clone();
    restored.uhci = UhciController::new("uhci", machine.memory.clone(), restored.irq.clone());
    restored
        .uhci
        .attach(1, Box::new(Tablet::full_speed("28754-0000:00:01.2-1")))
        .unwrap();
    restored.uhci.restore_state(&state).unwrap();
    restored
        .uhci
        .inject_pointer(PointerEvent {
            x: 0x1234,
            y: 0x2345,
            buttons: 1,
            wheel: 0,
        })
        .unwrap();
    restored.uhci.advance(2_000_000);
    restored.memory.read_slice(&mut control, GuestAddress(td + 4)).unwrap();
    assert_eq!(
        u32::from_le_bytes(control) & (TD_ACTIVE | 0x7ff),
        5,
        "six-byte report"
    );
    let mut report = [0; 6];
    restored
        .memory
        .read_slice(&mut report, GuestAddress(0x102100))
        .unwrap();
    assert_eq!(report, [1, 0x34, 0x12, 0x45, 0x23, 0]);
    assert!(restored.irq.high(), "IOC raises INTx");

    // A controller without the tablet refuses the state.
    let mut bare = UhciController::new("uhci", machine.memory.clone(), Arc::new(Line::default()));
    assert!(bare.restore_state(&state).is_err());
    assert!(bare.restore_state(&state[..state.len() - 1]).is_err());
}

#[test]
fn reset_returns_to_power_on_configuration_and_registers() {
    let mut machine = Machine::new();
    machine.replay(Some("GET_DESCRIPTOR(device) at address 0"));
    machine.uhci.reset().unwrap();
    let fresh = UhciController::new("uhci", machine.memory.clone(), Arc::new(Line::default()));
    assert_eq!(machine.uhci.config, fresh.config);
    assert!(!machine.irq.high());
    let mut data = [0; 2];
    machine.uhci.read(0x10, &mut data);
    assert_eq!(u16::from_le_bytes(data), 0x83, "the tablet reconnects");
    machine.uhci.read(2, &mut data);
    assert_eq!(u16::from_le_bytes(data), 0x20);
}

#[test]
fn cyclic_schedules_and_bad_pointers_stay_bounded() {
    let mut machine = Machine::new();
    machine.replay(Some("GET_DESCRIPTOR(device) at address 0"));
    // A queue head whose horizontal link points at itself, and a TD that
    // links back to itself depth-first.
    let qh = 0x101000u32;
    let td = 0x101100u32;
    machine
        .memory
        .write_slice(&(qh | 2).to_le_bytes(), GuestAddress(u64::from(qh)))
        .unwrap();
    machine
        .memory
        .write_slice(&td.to_le_bytes(), GuestAddress(u64::from(qh) + 4))
        .unwrap();
    for (offset, value) in [(0, td | 4), (4, TD_ACTIVE), (8, 0x69 | 9 << 8), (12, 0)] {
        machine
            .memory
            .write_slice(
                &u32::to_le_bytes(value),
                GuestAddress(u64::from(td) + offset),
            )
            .unwrap();
    }
    machine.uhci.advance(5_000_000);
    // Frame-list base beyond RAM.
    machine.uhci.write(8, &0xfff0_0000u32.to_le_bytes());
    machine.uhci.advance(5_000_000);
    let mut frame = [0; 2];
    machine.uhci.read(6, &mut frame);
    assert!(u16::from_le_bytes(frame) > 0x10);
}
