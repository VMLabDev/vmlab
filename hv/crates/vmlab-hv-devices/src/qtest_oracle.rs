//! The QEMU qtest oracle for the pc floppy controller, ISA DMA and UHCI
//! (`docs/hwref/pc-fdc-uhci-oracle.json`, recorded by
//! `scripts/pc-fdc-uhci-oracle.py`). Device tests replay its steps and
//! compare every read, guest-memory read and IRQ level.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::IrqLine;

pub(crate) const ORACLE: &str = include_str!("../../../docs/hwref/pc-fdc-uhci-oracle.json");

#[derive(Debug, Clone)]
pub(crate) enum Op {
    Note(String),
    Out { port: u16, width: u8, value: u32 },
    In { port: u16, width: u8, value: u32 },
    Write { address: u64, data: Vec<u8> },
    Read { address: u64, data: Vec<u8> },
    Step { ns: u64 },
}

#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub op: Op,
    /// IRQ lines high after the step.
    pub irq: Vec<u32>,
}

pub(crate) struct Run {
    pub steps: Vec<Step>,
    pub value: serde_json::Value,
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

pub(crate) fn run(configuration: &str) -> Run {
    let oracle: serde_json::Value = serde_json::from_str(ORACLE).unwrap();
    assert!(
        oracle.get("error").is_none(),
        "the oracle recorded an error"
    );
    let value = oracle["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["configuration"] == configuration)
        .unwrap_or_else(|| panic!("no {configuration} run"))
        .clone();
    let steps = value["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            let number = |key: &str| step[key].as_u64().unwrap();
            let op = match step["op"].as_str().unwrap() {
                "note" => Op::Note(step["text"].as_str().unwrap().into()),
                "out" => Op::Out {
                    port: number("port") as u16,
                    width: number("width") as u8,
                    value: number("value") as u32,
                },
                "in" => Op::In {
                    port: number("port") as u16,
                    width: number("width") as u8,
                    value: number("value") as u32,
                },
                "write" => Op::Write {
                    address: number("address"),
                    data: hex(step["hex"].as_str().unwrap()),
                },
                "read" => Op::Read {
                    address: number("address"),
                    data: hex(step["hex"].as_str().unwrap()),
                },
                "step" => Op::Step { ns: number("ns") },
                other => panic!("unknown oracle op {other}"),
            };
            let irq = step
                .get("irq")
                .and_then(|irq| irq.as_array())
                .map(|lines| {
                    lines
                        .iter()
                        .map(|line| line.as_u64().unwrap() as u32)
                        .collect()
                })
                .unwrap_or_default();
            Step { op, irq }
        })
        .collect();
    Run { steps, value }
}

/// The oracle's floppy image: each sector starts with its LBA, then a
/// pattern (`floppy_byte` in the oracle script).
pub(crate) fn floppy_image() -> Vec<u8> {
    (0..2880 * 512)
        .map(|offset: usize| {
            let (lba, index) = (offset / 512, offset % 512);
            if index < 2 {
                (lba >> (8 * index)) as u8
            } else {
                ((index * 7 + lba * 13) & 0xff) as u8
            }
        })
        .collect()
}

#[derive(Default)]
pub(crate) struct Line(AtomicBool);

impl Line {
    pub fn high(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl IrqLine for Line {
    fn set_level(&self, high: bool) {
        self.0.store(high, Ordering::SeqCst);
    }
}

/// Collects mismatches with their position (oracle note and the index of
/// the compared value within it), so one replay reports all of them.
pub(crate) struct Comparison<'a> {
    configuration: &'a str,
    section: String,
    index: usize,
    compared: usize,
    mismatches: Vec<String>,
}

impl<'a> Comparison<'a> {
    pub fn new(configuration: &'a str) -> Self {
        Self {
            configuration,
            section: String::new(),
            index: 0,
            compared: 0,
            mismatches: Vec::new(),
        }
    }

    pub fn section(&mut self, text: &str) {
        self.section = text.into();
        self.index = 0;
    }

    pub fn check<T: PartialEq + std::fmt::Debug>(&mut self, what: &str, actual: T, expected: T) {
        let index = self.index;
        self.index += 1;
        self.compared += 1;
        if actual == expected {
            return;
        }
        self.mismatches.push(format!(
            "{} / {} #{index} {what}: actual {actual:x?}, QEMU {expected:x?}",
            self.configuration, self.section
        ));
    }

    /// Fail on any mismatch; return how many values were compared.
    pub fn finish(self) -> usize {
        assert!(
            self.mismatches.is_empty(),
            "{} mismatches:\n{}",
            self.mismatches.len(),
            self.mismatches
                .iter()
                .take(60)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
        self.compared
    }
}
