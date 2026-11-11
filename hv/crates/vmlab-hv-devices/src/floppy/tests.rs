use std::sync::Arc;

use parking_lot::Mutex;
use vm_memory::{Bytes, GuestAddress};

use super::*;
use crate::GuestMemory;
use crate::chipset::rtc::Rtc;
use crate::qtest_oracle::{self, Comparison, Line, Op};

#[test]
fn sector_255_steps_to_next_track_without_wrapping_to_zero() {
    let mut m = Machine::new("floppy");
    m.floppy.drives[0].at.sect = 255;
    m.floppy.execution = Some(Execution {
        operation: Operation::Read,
        command: vec![0x66, 0, 0, 0, 255, 2, 255, 0x1b, 0xff],
        seeked: false,
        pio: Vec::new(),
        pio_position: 0,
        remaining_ns: 0,
    });
    assert!(m.floppy.step(0, 255, false));
    assert_eq!(m.floppy.drives[0].at.sect, 1);
}

#[test]
fn restore_rejects_pio_position_at_end_of_buffer() {
    let mut m = Machine::new("floppy");
    let mut state: serde_json::Value =
        serde_json::from_slice(&m.floppy.save_state().unwrap()).unwrap();
    state["phase"] = "Execution".into();
    state["execution"] = serde_json::json!({
        "operation":"Read", "command":[0x66,0,0,0,1,2,18,0x1b,0xff],
        "seeked":false, "pio":vec![0; SECTOR], "pio_position":SECTOR, "remaining_ns":0
    });
    assert!(
        m.floppy
            .restore_state(&serde_json::to_vec(&state).unwrap())
            .is_err()
    );
}

struct Machine {
    isa: Arc<Mutex<IsaDevices>>,
    floppy: Floppy,
    rtc: Rtc,
    memory: GuestMemory,
    irq: Arc<Line>,
    _directory: tempfile::TempDir,
    image: std::path::PathBuf,
}

impl Machine {
    fn new(configuration: &str) -> Self {
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x40_0000)]).unwrap();
        let isa = Arc::new(Mutex::new(IsaDevices::new()));
        isa.lock().set_memory(memory.clone());
        let irq = Arc::new(Line::default());
        let mut floppy = Floppy::new("floppy", isa.clone(), irq.clone());
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("floppy.img");
        if configuration != "empty" {
            std::fs::write(&image, qtest_oracle::floppy_image()).unwrap();
            floppy
                .insert(
                    0,
                    vmlab_disk::open(&image, vmlab_disk::Format::Raw, configuration == "readonly")
                        .unwrap(),
                )
                .unwrap();
        }
        let mut rtc = Rtc::new(128 << 20, 0, Arc::new(Line::default()));
        rtc.configure_pc([None; 4], false, 1, floppy.cmos_drive_types());
        Self {
            isa,
            floppy,
            rtc,
            memory,
            irq,
            _directory: directory,
            image,
        }
    }

    fn out(&mut self, port: u16, value: u8) {
        match port {
            0x3f0..=0x3f5 | 0x3f7 => self.floppy.io_write(port - 0x3f0, value),
            0x00..=0x0f | 0x80..=0x8f | 0xc0..=0xdf => {
                let pending = {
                    let mut isa = self.isa.lock();
                    isa.io_write(port, value);
                    isa.pending_requests()
                };
                if pending & (1 << DMA_CHANNEL) != 0 {
                    self.floppy.dma_ready();
                }
            }
            0x70 | 0x71 => crate::BusDevice::write(&mut self.rtc, u64::from(port - 0x70), &[value]),
            _ => panic!("unrouted oracle port {port:#x}"),
        }
    }

    fn inp(&mut self, port: u16) -> u8 {
        match port {
            0x3f0..=0x3f5 | 0x3f7 => self.floppy.io_read(port - 0x3f0),
            0x00..=0x0f | 0x80..=0x8f | 0xc0..=0xdf => self.isa.lock().io_read(port),
            0x70 | 0x71 => {
                let mut value = [0];
                crate::BusDevice::read(&mut self.rtc, u64::from(port - 0x70), &mut value);
                value[0]
            }
            _ => panic!("unrouted oracle port {port:#x}"),
        }
    }

    /// Replay the oracle run; returns the comparison for reporting.
    fn replay(&mut self, configuration: &str, until: Option<&str>) {
        let run = qtest_oracle::run(configuration);
        let mut comparison = Comparison::new(configuration);
        for step in &run.steps {
            match &step.op {
                Op::Note(text) => {
                    if until == Some(text.as_str()) {
                        break;
                    }
                    comparison.section(text);
                    continue;
                }
                Op::Out { port, width, value } => {
                    assert_eq!(*width, 1);
                    self.out(*port, *value as u8);
                }
                Op::In { port, width, value } => {
                    assert_eq!(*width, 1);
                    let actual = self.inp(*port);
                    comparison.check(&format!("in {port:#x}"), u32::from(actual), *value);
                }
                Op::Write { address, data } => {
                    self.memory
                        .write_slice(data, GuestAddress(*address))
                        .unwrap();
                }
                Op::Read { address, data } => {
                    let mut actual = vec![0; data.len()];
                    self.memory
                        .read_slice(&mut actual, GuestAddress(*address))
                        .unwrap();
                    comparison.check(&format!("memory {address:#x}"), actual, data.clone());
                }
                Op::Step { ns } => self.floppy.advance(*ns),
            }
            comparison.check("IRQ 6", self.irq.high(), step.irq.contains(&6));
        }
        if until.is_none() {
            let original = qtest_oracle::floppy_image();
            let image = std::fs::read(&self.image).unwrap_or_default();
            let changes: Vec<serde_json::Value> = (0..image.len())
                .step_by(512)
                .filter(|offset| image[*offset..offset + 512] != original[*offset..offset + 512])
                .map(|offset| {
                    serde_json::json!({
                        "offset": offset,
                        "hex": image[offset..offset + 512]
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>(),
                    })
                })
                .collect();
            let expected = run.value["image_changes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            comparison.check("image changes", changes, expected);
        }
        let compared = comparison.finish();
        assert!(
            until.is_some() || compared > 500,
            "only {compared} oracle values compared"
        );
    }
}

#[test]
fn qemu_oracle_floppy_with_a_writable_medium() {
    Machine::new("floppy").replay("floppy", None);
}

#[test]
fn qemu_oracle_floppy_with_a_write_protected_medium() {
    Machine::new("readonly").replay("readonly", None);
}

#[test]
fn qemu_oracle_floppy_with_an_empty_drive() {
    Machine::new("empty").replay("empty", None);
}

#[test]
fn cmos_drive_types_follow_the_medium() {
    let isa = Arc::new(Mutex::new(IsaDevices::new()));
    let mut floppy = Floppy::new("floppy", isa, Arc::new(Line::default()));
    assert_eq!(
        floppy.cmos_drive_types(),
        0x50,
        "empty drive: 2.88 MB fallback"
    );
    let directory = tempfile::tempdir().unwrap();
    for (size, cmos) in [(1_474_560usize, 0x40), (2_949_120, 0x50)] {
        let path = directory.path().join(format!("{size}.img"));
        std::fs::write(&path, vec![0; size]).unwrap();
        floppy
            .insert(
                0,
                vmlab_disk::open(&path, vmlab_disk::Format::Raw, false).unwrap(),
            )
            .unwrap();
        assert_eq!(floppy.cmos_drive_types(), cmos);
    }
    let path = directory.path().join("odd.img");
    std::fs::write(&path, vec![0; 737_280]).unwrap();
    assert!(
        floppy
            .insert(
                0,
                vmlab_disk::open(&path, vmlab_disk::Format::Raw, false).unwrap()
            )
            .is_err()
    );
}

/// Snapshot in the middle of a DMA-waiting read, restore into a fresh
/// controller with the same medium, unmask, and get QEMU's result.
#[test]
fn snapshot_round_trip_resumes_a_command_waiting_on_dma() {
    let mut machine = Machine::new("floppy");
    machine.replay(
        "floppy",
        Some("a masked channel holds the transfer until unmasked"),
    );
    // Program a masked channel and issue the read, as the oracle does.
    for (port, value) in [
        (0x0a, 0x06),
        (0x0c, 0),
        (0x0b, 0x46),
        (0x04, 0x00),
        (0x04, 0x90),
        (0x81, 0),
        (0x0c, 0),
        (0x05, 0xff),
        (0x05, 0x01),
    ] {
        machine.out(port, value);
    }
    for byte in [0x66, 0x00, 0, 0, 6, 2, 18, 0x1b, 0xff] {
        machine.out(0x3f5, byte);
    }
    assert_eq!(machine.inp(0x3f4), 0x10);
    let floppy_state = machine.floppy.save_state().unwrap();
    let isa_state = machine.isa.lock().save_state().unwrap();

    let mut restored = Machine::new("floppy");
    restored.isa.lock().restore_state(&isa_state).unwrap();
    restored.floppy.restore_state(&floppy_state).unwrap();
    assert_eq!(restored.inp(0x3f4), 0x10);
    assert_eq!(restored.inp(0x08), 0x40, "DREQ survives the snapshot");
    restored.out(0x0a, 0x02);
    assert!(restored.irq.high());
    let result: Vec<u8> = (0..7).map(|_| restored.inp(0x3f5)).collect();
    assert_eq!(result, [0, 0, 0, 0, 0, 7, 2]);
    assert!(!restored.irq.high());
    let mut sector = [0; 512];
    restored
        .memory
        .read_slice(&mut sector, GuestAddress(0x9000))
        .unwrap();
    assert_eq!(sector[..], qtest_oracle::floppy_image()[5 * 512..6 * 512]);

    // A different medium refuses the state.
    let empty = Machine::new("empty");
    let mut empty_floppy = empty.floppy;
    assert!(empty_floppy.restore_state(&floppy_state).is_err());
}

#[test]
fn reset_returns_to_power_on_and_keeps_the_medium() {
    let mut machine = Machine::new("floppy");
    machine.replay("floppy", Some("DMA read of two sectors"));
    machine.floppy.reset().unwrap();
    let mut fresh = Machine::new("floppy");
    for port in [0x3f0, 0x3f1, 0x3f2, 0x3f3, 0x3f4, 0x3f7] {
        assert_eq!(machine.inp(port), fresh.inp(port), "port {port:#x}");
    }
    assert!(!machine.irq.high());
    // The medium is still there: recalibrate and read sector 1 by PIO-free DMA.
    for (port, value) in [
        (0x0a, 0x06),
        (0x0c, 0),
        (0x0b, 0x46),
        (0x04, 0),
        (0x04, 0x20),
    ] {
        machine.out(port, value);
    }
    for (port, value) in [
        (0x81, 0),
        (0x0c, 0),
        (0x05, 0xff),
        (0x05, 0x01),
        (0x0a, 0x02),
    ] {
        machine.out(port, value);
    }
    for byte in [0xe6, 0x00, 0, 0, 1, 2, 18, 0x1b, 0xff] {
        machine.out(0x3f5, byte);
    }
    let result: Vec<u8> = (0..7).map(|_| machine.inp(0x3f5)).collect();
    assert_eq!(result, [0, 0, 0, 0, 0, 2, 2]);
    let _ = &mut fresh;
}

#[test]
fn malformed_snapshots_are_refused() {
    let mut machine = Machine::new("floppy");
    let state = machine.floppy.save_state().unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&state).unwrap();
    value["phase"] = "Execution".into();
    assert!(
        machine
            .floppy
            .restore_state(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
    let mut value: serde_json::Value = serde_json::from_slice(&state).unwrap();
    value["drives"][0]["at"]["track"] = 200.into();
    assert!(
        machine
            .floppy
            .restore_state(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
    let mut value: serde_json::Value = serde_json::from_slice(&state).unwrap();
    value["phase"] = "Execution".into();
    value["execution"] = serde_json::json!({
        "operation":"Read","command":[],"seeked":false,"pio":[],"pio_position":0,"remaining_ns":0
    });
    assert!(
        machine
            .floppy
            .restore_state(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
    machine.floppy.restore_state(&state).unwrap();
}
