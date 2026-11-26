//! Streams with the MMU on (card C8, DESIGN.md O3): random translation
//! tables, and a run of loads, stores, branches, address translation
//! instructions and TLB and cache operations at addresses that those tables
//! map, do not map, or map without the permission.
//!
//! An image has four parts, all in RAM (`Layout`): the code (a vector table
//! and the stream), a page of `RET` that the stream calls through other
//! addresses, a window of data, and the translation tables. The code, the
//! `RET` page and the tables are mapped at their physical addresses, so the
//! stream runs the same with the MMU off and on. Everything else that the
//! tables map is a probe: a page or a block at another address, with any
//! descriptor.
//!
//! What a stream keeps to, because the two sides are free to differ there:
//!
//! - **A changed translation is invalidated.** After a write to a
//!   descriptor or to a control register the stream runs a TLBI that names
//!   every ASID. What a TLB holds beyond that is not a function of the
//!   architecture: QEMU's TLB and ours keep different entries.
//! - **The tables are written by one instruction of the stream only**, and
//!   no probe maps them or the code for writing.
//! - **Little-endian**, and SCTLR_EL1.SA and SA0 clear: the stack pointer
//!   alignment check is not built.
//!
//! `Profile` says which CPU leads. With `strict` the stream stays inside what
//! an Armv8.0 walk and the walk of a later CPU agree on, for the walk test
//! of the 16K and 64K granules against QEMU's `-cpu max`.

use super::asm::{self, op, reg};
use super::stream::{Generator, records_of, unpredictable};
use super::system::{EXIT_CALL, spsr};
use crate::generated::decode::{FormatClass, decode};
use crate::lift::{IMMEDIATE_FORMATS, LOAD_STORE_FORMATS, REGISTER_FORMATS};
use crate::mmu::geometry;
use std::collections::{BTreeMap, BTreeSet};

/// Where the parts of an image are, from its base, which is a multiple of
/// 32 MiB.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub base: u64,
}

/// The bytes of the code, of the `RET` page, of the window and of the area
/// of the tables.
pub const CODE_BYTES: u64 = 0x1_0000;
pub const STUB_BYTES: u64 = 0x1_0000;
pub const WINDOW_BYTES: u64 = 0x4_0000;
pub const TABLES_BYTES: u64 = 0x20_0000;
/// A table has a slot of this size, whatever its granule.
const SLOT_BYTES: u64 = 0x1_0000;
/// The vector table takes the first bytes of the code.
pub const VECTORS_BYTES: u64 = 0x800;

impl Layout {
    pub fn stub(self) -> u64 {
        self.base + CODE_BYTES
    }

    pub fn window(self) -> u64 {
        self.base + 0x4_0000
    }

    pub fn tables(self) -> u64 {
        self.base + 0x20_0000
    }
}

/// Which CPU leads, and how far the stream goes.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    /// The granules that the CPU has, by their shift.
    pub granules: &'static [u32],
    /// The width of a physical address of the CPU that both sides use.
    pub pa_bits: u32,
    /// Stay inside what Armv8.0 and a later CPU agree on: no reserved value
    /// of a TCR field, no bit that a later extension gives a meaning, and
    /// no block at a level that a later extension allows.
    pub strict: bool,
}

/// QEMU's `cortex-a53`: 4K and 64K, 40 bits.
pub const CORTEX_A53: Profile = Profile {
    granules: &[12, 16],
    pa_bits: 40,
    strict: false,
};

/// The walk test against QEMU's `-cpu max`: 16K and 64K, 48 bits.
pub const MAX_WALK: Profile = Profile {
    granules: &[14, 16],
    pa_bits: 48,
    strict: true,
};

/// The bits 47 to 12 of a descriptor.
const ADDRESS: u64 = 0x0000_ffff_ffff_f000;

/// SCTLR_EL1 of a stream without the bits that it varies: the value after
/// the reset without SA and SA0.
const SCTLR: u64 = 0x00c5_0820;
const SCTLR_M: u64 = 1;
const SCTLR_A: u64 = 1 << 1;
const SCTLR_DZE: u64 = 1 << 14;
const SCTLR_WXN: u64 = 1 << 19;
const SCTLR_UCI: u64 = 1 << 26;

/// The translation tables of an image.
struct Tables {
    next: u64,
    end: u64,
    /// Each table: its address and the shift of its granule.
    tables: Vec<(u64, u32)>,
    /// The descriptors that are not zero, by their address.
    words: BTreeMap<u64, u64>,
}

impl Tables {
    fn table(&mut self, granule: u32) -> Option<u64> {
        let at = self.next;
        if at + SLOT_BYTES > self.end {
            return None;
        }
        self.next += SLOT_BYTES;
        self.tables.push((at, granule));
        Some(at)
    }

    fn get(&self, entry: u64) -> u64 {
        self.words.get(&entry).copied().unwrap_or(0)
    }

    fn set(&mut self, entry: u64, value: u64) {
        self.words.insert(entry, value);
    }

    /// The addresses of the descriptors that a walk from `root` for `va`
    /// reads, from the first level to `leaf`, with the tables between made
    /// where they are missing. `None`: no room for a table, a level that
    /// the walk does not have, or a descriptor on the way that is no table
    /// of ours.
    fn path(
        &mut self,
        root: u64,
        granule: u32,
        input: u32,
        va: u64,
        leaf: u32,
    ) -> Option<Vec<u64>> {
        let (start, first_bits, stride) = geometry(granule, input);
        if leaf < start {
            return None;
        }
        let va = va & ((1u64 << input) - 1);
        let mut table = root;
        let mut entries = Vec::new();
        for level in start..=leaf {
            let shift = granule + stride * (3 - level);
            let bits = if level == start { first_bits } else { stride };
            let entry = table + ((va >> shift) & ((1 << bits) - 1)) * 8;
            entries.push(entry);
            if level == leaf {
                break;
            }
            let descriptor = self.get(entry);
            if descriptor == 0 {
                table = self.table(granule)?;
                self.set(entry, table | 3);
            } else if descriptor & 3 == 3 {
                table = descriptor & ADDRESS & !((1u64 << granule) - 1);
                if !self.tables.iter().any(|(at, _)| *at == table) {
                    return None;
                }
            } else {
                return None;
            }
        }
        Some(entries)
    }

    /// The tables as memory: address and bytes.
    fn memory(&self) -> Vec<(u64, Vec<u8>)> {
        self.tables
            .iter()
            .map(|(at, granule)| {
                let mut bytes = vec![0u8; 1 << granule];
                for (entry, value) in self.words.range(*at..*at + (1u64 << granule)) {
                    let offset = (entry - at) as usize;
                    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
                }
                (*at, bytes)
            })
            .collect()
    }
}

/// An address that the tables of an image say something about.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    pub va: u64,
    /// The physical address of its page or block descriptor.
    pub entry: u64,
    pub level: u32,
    pub granule: u32,
    /// The bytes that the descriptor maps.
    pub span: u64,
    /// The mapping is of the `RET` page, for a call.
    pub stub: bool,
}

/// The control registers that the prelude of an image writes.
#[derive(Clone, Copy, Debug)]
pub struct Controls {
    pub tcr: u64,
    pub ttbr: [u64; 2],
    pub mair: u64,
    pub sctlr: u64,
}

/// An image with the MMU on.
#[derive(Clone, Debug)]
pub struct Image {
    /// What a round loads: address and bytes.
    pub memory: Vec<(u64, Vec<u8>)>,
    /// The first instruction.
    pub start: u64,
    pub controls: Controls,
    pub probes: Vec<Probe>,
    /// The granule of each half of the address space, by its shift.
    pub granules: [u32; 2],
}

/// The vector table of a stream with the MMU on. An instruction abort
/// returns to X30: the stream calls its probes, so that is the instruction
/// after the call. An exception from EL0 that is the exit call returns to
/// EL1 with `exit_spsr`. Every other exception skips its instruction.
pub fn vector_table(exit_spsr: u16) -> Vec<u32> {
    // All ones but bit 20: 63 ones, rotated right by 43.
    let and_not_il = |rd: u32| 0x9240_0000 | 43 << 16 | 62 << 10 | rd << 5 | rd;
    // All ones but the bits 1 and 0: 62 ones, rotated right by 62.
    let and_aligned = |rd: u32| 0x9240_0000 | 62 << 16 | 61 << 10 | rd << 5 | rd;
    let mut words = Vec::new();
    for entry in 0..16 {
        let mut handler = Vec::new();
        // The synchronous entry from a lower level in AArch64.
        if entry == 8 {
            handler.extend([
                asm::mrs(16, reg::ESR_EL1),
                asm::movz(17, EXIT_CALL, 0),
                asm::movk(17, 0x5600, 1),
                asm::eor(16, 16, 17),
                asm::cbnz(16, 4),
                asm::movz(16, exit_spsr, 0),
                asm::msr(reg::SPSR_EL1, 16),
                asm::ERET,
            ]);
        }
        handler.extend([
            // The exception class without its low bit: 0x10 for an
            // instruction abort from either level.
            asm::mrs(16, reg::ESR_EL1),
            asm::lsr(17, 16, 27),
            asm::movz(16, 0x10, 0),
            asm::eor(17, 17, 16),
            asm::cbnz(17, 6),
            asm::msr(reg::ELR_EL1, 30),
            asm::mrs(16, reg::SPSR_EL1),
            and_not_il(16),
            asm::msr(reg::SPSR_EL1, 16),
            asm::ERET,
            // Clear the saved IL, and return to the instruction after the
            // one that faulted, at a multiple of four.
            asm::mrs(16, reg::SPSR_EL1),
            and_not_il(16),
            asm::msr(reg::SPSR_EL1, 16),
            asm::mrs(17, reg::ELR_EL1),
            asm::add(17, 17, 4),
            and_aligned(17),
            asm::msr(reg::ELR_EL1, 17),
            asm::ERET,
        ]);
        words.extend(&handler);
        words.resize(words.len() + 32 - handler.len(), asm::NOP);
    }
    words
}

/// The simple loads and stores of a stream, with X0 as the base: the word,
/// and the bytes of the access.
const ACCESSES: [(u32, u64); 16] = [
    (0xf940_0001, 8),  // ldr x1, [x0]
    (0xf900_0001, 8),  // str x1, [x0]
    (0x3940_0001, 1),  // ldrb w1, [x0]
    (0x3900_0001, 1),  // strb w1, [x0]
    (0x7940_0001, 2),  // ldrh w1, [x0]
    (0xb940_0001, 4),  // ldr w1, [x0]
    (0xb900_0001, 4),  // str w1, [x0]
    (0xa940_0801, 16), // ldp x1, x2, [x0]
    (0xa900_0801, 16), // stp x1, x2, [x0]
    (0xf840_0801, 8),  // ldtr x1, [x0]
    (0xf800_0801, 8),  // sttr x1, [x0]
    (0x3840_0801, 1),  // ldtrb w1, [x0]
    (0x3dc0_0001, 16), // ldr q1, [x0]
    (0x3d80_0001, 16), // str q1, [x0]
    (0xc85f_7c01, 8),  // ldxr x1, [x0]
    (0xc802_7c01, 8),  // stxr w2, x1, [x0]
];

/// The cache operations of a stream that take an address.
const CACHE_BY_ADDRESS: [u16; 6] = [
    op::DC_ZVA,
    0x1bd1, // dc cvac
    0x1bd9, // dc cvau
    0x1bf1, // dc civac
    0x1ba9, // ic ivau
    0x03b1, // dc ivac
];

/// The cache operations of a stream that take no address.
const CACHE_OTHER: [u16; 5] = [
    0x03b2, // dc isw
    0x03d2, // dc csw
    0x03f2, // dc cisw
    0x0388, // ic ialluis
    0x03a8, // ic iallu
];

/// The TLBI operations of the EL1&0 regime in Armv8.0, by op2. Each exists
/// for this CPU (CRm 7) and for all of them (CRm 3).
const TLBI_OP2: [u16; 6] = [0, 1, 2, 3, 5, 7];

struct Builder<'a> {
    generator: &'a mut Generator,
    layout: Layout,
    profile: Profile,
    tables: Tables,
    /// The descriptors that the code and the tables are reached through.
    protected: BTreeSet<u64>,
    output_bits: u32,
    probes: Vec<Probe>,
}

impl Builder<'_> {
    fn below(&mut self, bound: u64) -> u64 {
        self.generator.rng.below(bound)
    }

    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.generator.rng.chance(numerator, denominator)
    }

    /// Map `bytes` at `pa` to the same virtual address in the lower half,
    /// with the descriptor bits `attributes`.
    fn identity(&mut self, root: u64, granule: u32, input: u32, pa: u64, bytes: u64, attributes: u64) {
        let page = 1u64 << granule;
        let mut at = pa & !(page - 1);
        while at < pa + bytes {
            let entries = self
                .tables
                .path(root, granule, input, at, 3)
                .expect("the tables of an image have room for its own mapping");
            let leaf = entries[entries.len() - 1];
            self.tables.set(leaf, at | attributes | 3);
            self.protected.extend(entries);
            at += page;
        }
    }

    /// A physical address for a probe that maps `span` bytes, and whether
    /// it is the `RET` page.
    fn output(&mut self, span: u64, writable_block: bool) -> (u64, bool) {
        let layout = self.layout;
        match self.below(20) {
            // Beyond the RAM of the machine: an external abort.
            0 | 1 => (layout.base + 0x0800_0000 + self.below(16) * span, false),
            // Wider than the output size: an address size fault.
            2 | 3 if self.output_bits < 48 => {
                let bit = u64::from(self.output_bits) + self.below(u64::from(48 - self.output_bits));
                ((layout.window() & !(span - 1)) | 1 << bit, false)
            }
            4 | 5 if span <= STUB_BYTES => (layout.stub(), true),
            _ if span <= WINDOW_BYTES => {
                let pages = WINDOW_BYTES / span;
                (layout.window() + self.below(pages) * span, false)
            }
            // A block is larger than the window: it maps the RAM around the
            // image, and nothing writes through it.
            _ => {
                debug_assert!(!writable_block);
                (layout.base & !(span - 1), false)
            }
        }
    }

    /// A page or block descriptor for a probe at `level`.
    fn leaf(&mut self, level: u32, granule: u32) -> (u64, bool) {
        let stride = granule - 3;
        let span = 1u64 << (granule + stride * (3 - level));
        let (pa, stub) = self.output(span, false);
        let mut descriptor = pa & ADDRESS & !(span - 1);
        if !self.chance(1, 12) {
            descriptor |= 1;
        }
        if level == 3 {
            descriptor |= 2;
        }
        // The access flag.
        if !self.chance(1, 8) {
            descriptor |= 1 << 10;
        }
        // AP[2:1]: EL0 access and read-only. A block maps more than the
        // window, so it is read-only. The `RET` page is never written.
        let mut permissions = self.below(4);
        if span > WINDOW_BYTES || stub {
            permissions |= 2;
        }
        descriptor |= permissions << 6;
        // UXN and PXN.
        descriptor |= self.below(4) << 53;
        // nG, the attribute index and the shareability.
        descriptor |= self.below(2) << 11 | self.below(8) << 2 | self.below(4) << 8;
        // The bits that the walk ignores: the contiguous bit, and what
        // software may use.
        if self.chance(1, 4) {
            descriptor |= self.below(2) << 52 | self.below(16) << 55;
        }
        if !self.profile.strict && self.chance(1, 8) {
            // Bits that Armv8.0 reserves or ignores, and later extensions
            // give a meaning.
            descriptor |= self.below(4) << 50 | self.below(32) << 59;
        }
        (descriptor, stub)
    }

    /// Put probes into one half: pages next to each other, and blocks.
    fn probes(&mut self, root: u64, granule: u32, input: u32, region: u64, count: u32) {
        let page = 1u64 << granule;
        let stride = granule - 3;
        let half = 1u64 << input;
        for index in 0..count {
            // Three in four are pages, next to each other. The others are
            // blocks, further up.
            let (level, va) = if index % 4 != 3 {
                (3, region + u64::from(index) * page)
            } else {
                let level = if granule == 12 && self.chance(1, 4) { 1 } else { 2 };
                let span = 1u64 << (granule + stride * (3 - level));
                let slot = 1 + self.below(4);
                (level, (region & !(span - 1)) + slot * span)
            };
            // The address stays in its half.
            let within = va & (half - 1);
            if (va ^ region) & !(half - 1) != 0 || within < (region & (half - 1)) {
                continue;
            }
            let Some(entries) = self.tables.path(root, granule, input, va, level) else {
                continue;
            };
            let entry = entries[entries.len() - 1];
            if self.protected.contains(&entry) || self.tables.get(entry) != 0 {
                continue;
            }
            let (descriptor, stub) = self.leaf(level, granule);
            self.tables.set(entry, descriptor);
            self.probes.push(Probe {
                va,
                entry,
                level,
                granule,
                span: 1u64 << (granule + stride * (3 - level)),
                stub,
            });
            // Something on the way to the probe, one time in five.
            if self.chance(4, 5) {
                continue;
            }
            let on_the_way: Vec<u64> = entries[..entries.len() - 1]
                .iter()
                .copied()
                .filter(|entry| !self.protected.contains(entry))
                .collect();
            if on_the_way.is_empty() {
                continue;
            }
            let entry = on_the_way[self.below(on_the_way.len() as u64) as usize];
            let old = self.tables.get(entry);
            let new = match self.below(8) {
                // No table.
                0 => 0,
                // A table beyond the output size, and one beyond the RAM.
                1 if self.output_bits < 48 => old | 1 << self.output_bits,
                2 => (self.layout.base + 0x0800_0000) | 3,
                // A block where no block exists.
                3 if !self.profile.strict => old & !2,
                // What a table descriptor takes away: APTable, UXNTable,
                // PXNTable, and NSTable, which says nothing here.
                _ => old | self.below(32) << 59,
            };
            self.tables.set(entry, new);
        }
    }

    /// The value of a register write of the stream.
    fn sctlr(&mut self, enabled: bool) -> u64 {
        let mut value = SCTLR;
        if enabled {
            value |= SCTLR_M;
        }
        for (bit, numerator, denominator) in [
            (SCTLR_WXN, 1, 4),
            (SCTLR_A, 1, 4),
            (SCTLR_UCI, 1, 2),
            (SCTLR_DZE, 1, 2),
        ] {
            if self.chance(numerator, denominator) {
                value |= bit;
            }
        }
        value
    }

    fn mair(&mut self) -> u64 {
        // The code uses the index 0, which is Normal memory.
        let mut value = 0xff;
        for index in 1..8 {
            let byte = match self.below(8) {
                0 => 0x00,
                1 => 0x04,
                2 => 0x0c,
                3 => 0x44,
                4 | 5 => 0xff,
                6 => 0xbb,
                _ => self.below(256),
            };
            value |= byte << (8 * index);
        }
        value
    }
}

impl Generator {
    /// An image with the MMU on for the CPU of `profile`, with about `count`
    /// items in its stream.
    pub fn translated_image(&mut self, layout: Layout, profile: Profile, count: usize) -> Image {
        let granule_of = |generator: &mut Generator| {
            profile.granules[generator.rng.below(profile.granules.len() as u64) as usize]
        };
        let granules = [granule_of(self), granule_of(self)];
        // The sizes of the two halves. The code is below 2 GiB, so the lower
        // half has at least 31 bits. Outside 16 to 39 the size is taken as
        // the nearest of the two.
        let mut tsz = [16 + self.rng.below(18) as u32, 16 + self.rng.below(18) as u32];
        let mut written = tsz;
        if !profile.strict && self.rng.chance(1, 12) {
            written[0] = self.rng.below(16) as u32;
            tsz[0] = 16;
        }
        if !profile.strict && self.rng.chance(1, 12) {
            written[1] = if self.rng.chance(1, 2) {
                tsz[1] = 16;
                self.rng.below(16) as u32
            } else {
                tsz[1] = 39;
                40 + self.rng.below(24) as u32
            };
        }
        let input = [64 - tsz[0], 64 - tsz[1]];
        // The output size: IPS, and at most what the CPU has.
        let ips = if profile.strict {
            self.rng.below(6)
        } else {
            self.rng.below(8)
        };
        let ips_bits = [32, 36, 40, 42, 44, 48, 52, 52][ips as usize];
        let output_bits = ips_bits.min(profile.pa_bits);
        let mut builder = Builder {
            generator: self,
            layout,
            profile,
            tables: Tables {
                next: layout.tables(),
                end: layout.tables() + TABLES_BYTES,
                tables: Vec::new(),
                words: BTreeMap::new(),
            },
            protected: BTreeSet::new(),
            output_bits,
            probes: Vec::new(),
        };
        let roots = [
            builder.tables.table(granules[0]).expect("an empty area has room"),
            builder.tables.table(granules[1]).expect("an empty area has room"),
        ];
        // The code and the `RET` page: read-only at both levels, executable
        // at both, global, Normal memory, with the access flag.
        builder.identity(
            roots[0],
            granules[0],
            input[0],
            layout.base,
            CODE_BYTES + STUB_BYTES,
            1 << 10 | 3 << 6,
        );
        // The tables: for EL1 only, and never executable.
        builder.identity(
            roots[0],
            granules[0],
            input[0],
            layout.tables(),
            TABLES_BYTES,
            1 << 10 | 3 << 53,
        );
        // The probes of the lower half are above the RAM and below 2 GiB.
        // Those of the upper half are in its last GiB.
        builder.probes(roots[0], granules[0], input[0], 0x6000_0000, 12);
        builder.probes(roots[1], granules[1], input[1], u64::MAX << input[1].min(30), 12);

        // --- the control registers --------------------------------------
        let tg0 = match granules[0] {
            12 => 0,
            14 => 2,
            _ => 1,
        };
        let tg1 = match granules[1] {
            12 => 2,
            14 => 1,
            _ => 3,
        };
        let mut tcr = u64::from(written[0])
            | tg0 << 14
            | u64::from(written[1]) << 16
            | tg1 << 30
            | ips << 32
            // The cacheability and shareability of the walks say nothing here.
            | builder.below(64) << 8
            | builder.below(64) << 24
            // A1, AS, TBI0 and TBI1.
            | builder.below(2) << 22
            | builder.below(2) << 36
            | builder.below(4) << 37;
        if builder.chance(1, 16) {
            // EPD1: no walk through the upper half.
            tcr |= 1 << 23;
        }
        if !profile.strict && builder.chance(1, 8) {
            // Bits that Armv8.0 reserves.
            tcr |= builder.generator.rng.next_u64() & 0xffff_ff80_0000_0000 & !(3 << 37);
        }
        let mut ttbr = [0u64; 2];
        for (half, register) in ttbr.iter_mut().enumerate() {
            *register = roots[half] | builder.generator.rng.next_u64() << 48;
        }
        let controls = Controls {
            tcr,
            ttbr,
            mair: builder.mair(),
            sctlr: builder.sctlr(true),
        };

        // --- the code -----------------------------------------------------
        let exit_spsr = [0x3c5, 0x3c4, 0x005, 0x145][builder.below(4) as usize];
        let mut words = vector_table(exit_spsr);
        let write = |words: &mut Vec<u32>, register: u16, value: u64| {
            words.extend(asm::load(16, value));
            words.push(asm::msr(register, 16));
        };
        write(&mut words, reg::VBAR_EL1, layout.base);
        // FP and AdvSIMD at both levels, for the vector loads and stores.
        write(&mut words, reg::CPACR_EL1, 3 << 20);
        write(&mut words, reg::MAIR_EL1, controls.mair);
        write(&mut words, reg::TCR_EL1, controls.tcr);
        write(&mut words, reg::TTBR0_EL1, controls.ttbr[0]);
        write(&mut words, reg::TTBR1_EL1, controls.ttbr[1]);
        write(&mut words, reg::SCTLR_EL1, controls.sctlr);
        words.push(asm::ISB);
        let limit = (CODE_BYTES / 4) as usize - 64;
        let mut items = 0;
        let mut enabled = true;
        while items < count && words.len() < limit {
            let at = layout.base + 4 * words.len() as u64;
            builder.item(&mut words, at, roots, &mut enabled);
            items += 1;
        }
        // The stream ends in a branch to itself: a round that gets here ends
        // as a loop.
        words.push(asm::NOP);
        words.push(asm::b(0));

        let mut memory = vec![(layout.base, asm::bytes(&words))];
        // The rest of the code area is zero: nothing of an older round.
        memory[0].1.resize(CODE_BYTES as usize, 0);
        memory.push((
            layout.stub(),
            asm::bytes(&vec![asm::RET; (STUB_BYTES / 4) as usize]),
        ));
        let window = (layout.window(), WINDOW_BYTES);
        memory.push((
            layout.window(),
            builder.generator.memory_bytes(window, WINDOW_BYTES as usize),
        ));
        memory.extend(builder.tables.memory());
        Image {
            memory,
            start: layout.base + VECTORS_BYTES,
            controls,
            probes: builder.probes,
            granules,
        }
    }
}

impl Builder<'_> {
    /// An address of a probe, or beside one: mostly at its start or near
    /// its end, where an access crosses into the next page.
    fn address(&mut self) -> u64 {
        if self.probes.is_empty() || self.chance(1, 24) {
            return self.generator.rng.next_u64();
        }
        let probe = self.probes[self.below(self.probes.len() as u64) as usize];
        let offset = match self.below(8) {
            0..=2 => 0,
            3 => self.below(probe.span.min(1 << 16)) & !7,
            4 | 5 => probe.span - 1 - self.below(16),
            6 => probe.span - 8 * (1 + self.below(4)),
            _ => self.below(probe.span),
        };
        let mut va = probe.va.wrapping_add(offset);
        // A tag in the top byte, which the regime ignores or does not.
        if self.chance(1, 10) {
            va ^= self.below(256) << 56;
        }
        va
    }

    /// Append one item of the stream at the address `at`.
    fn item(&mut self, words: &mut Vec<u32>, at: u64, roots: [u64; 2], enabled: &mut bool) {
        let image = (self.layout.base + VECTORS_BYTES, CODE_BYTES - VECTORS_BYTES);
        match self.below(100) {
            // A load or a store at a probe.
            0..=33 => {
                let va = self.address();
                words.extend(asm::load(0, va));
                if self.chance(2, 3) {
                    let (mut word, _) = ACCESSES[self.below(ACCESSES.len() as u64) as usize];
                    // With `strict` there is no exclusive access: a later
                    // CPU does not fault one that is not aligned.
                    if self.profile.strict && word >> 24 == 0xc8 {
                        word = ACCESSES[0].0;
                    }
                    words.push(word);
                    // An exclusive load has its store behind it.
                    if word == 0xc85f_7c01 && self.chance(3, 4) {
                        words.push(0xc802_7c01);
                    }
                    return;
                }
                // Any record of the group, with X0 as its base.
                let format =
                    LOAD_STORE_FORMATS[self.below(LOAD_STORE_FORMATS.len() as u64) as usize];
                let records = records_of(format);
                let record = records[self.below(records.len() as u64) as usize];
                let (word, _) = self.generator.instruction(record, at, image);
                let word = crate::with_field(record, "rn", word, 0).unwrap_or(word);
                let exclusive = matches!(
                    format,
                    FormatClass::LoadExclusive
                        | FormatClass::StoreExclusive
                        | FormatClass::LoadExclusivePair
                        | FormatClass::StoreExclusivePair
                        | FormatClass::LoadAcquire
                        | FormatClass::StoreRelease
                );
                let usable = decode(word)
                    .is_some_and(|decoded| decoded.record == record && !unpredictable(&decoded));
                if usable && !(self.profile.strict && exclusive) {
                    words.push(word);
                }
            }
            // A call through a probe: of the `RET` page most of the time.
            34..=41 => {
                let stubs: Vec<Probe> = self.probes.iter().copied().filter(|p| p.stub).collect();
                let va = if stubs.is_empty() || self.chance(1, 4) {
                    self.address() & !3
                } else {
                    let probe = stubs[self.below(stubs.len() as u64) as usize];
                    probe.va + 4 * self.below(probe.span.min(STUB_BYTES) / 4)
                };
                words.extend(asm::load(16, va));
                words.push(asm::blr(16));
            }
            // An address translation instruction.
            42..=49 => {
                let va = self.address();
                words.extend(asm::load(1, va));
                words.push(asm::sys(op::AT_S1E1R + self.below(4) as u16, 1));
                words.push(asm::mrs(2, reg::PAR_EL1));
            }
            // A cache operation, and DC ZVA most of all.
            50..=55 => {
                let va = self.address();
                words.extend(asm::load(0, va));
                let operation = if self.chance(1, 2) {
                    op::DC_ZVA
                } else {
                    CACHE_BY_ADDRESS[self.below(CACHE_BY_ADDRESS.len() as u64) as usize]
                };
                words.push(asm::sys(operation, 0));
            }
            56 | 57 => {
                let operation = CACHE_OTHER[self.below(CACHE_OTHER.len() as u64) as usize];
                let rt = if operation & 7 == 0 { 31 } else { self.below(31) as u32 };
                words.push(asm::sys(operation, rt));
            }
            // A TLBI of the regime. It only drops translations.
            58..=62 => {
                let va = self.address();
                let argument = (va >> 12) & ((1 << 44) - 1) | self.generator.rng.next_u64() << 48;
                words.extend(asm::load(1, argument));
                let op2 = TLBI_OP2[self.below(TLBI_OP2.len() as u64) as usize];
                let crm = if self.chance(1, 2) { 7 } else { 3 };
                let rt = if op2 == 0 { 31 } else { 1 };
                words.push(asm::sys(8 << 7 | crm << 3 | op2, rt));
            }
            // Any `SYS` or `SYSL`: both sides have the operation, or neither.
            63..=65 if !self.profile.strict => {
                let encoding = self.below(1 << 14) as u16;
                // A TLBI of another kind may exist on one side only in a
                // way that shows later. It is left to the directed tests.
                let word = asm::sys(encoding, self.below(32) as u32) | (self.below(2) as u32) << 21;
                words.push(word);
            }
            // A new descriptor for a probe, and the TLBI that names every
            // ASID.
            66..=73 if !self.probes.is_empty() => {
                let probe = self.probes[self.below(self.probes.len() as u64) as usize];
                let (descriptor, stub) = self.leaf(probe.level, probe.granule);
                // The probe of the `RET` page keeps what its calls rely on.
                if stub != probe.stub {
                    return;
                }
                words.extend(asm::load(0, probe.entry));
                words.extend(asm::load(1, descriptor));
                words.push(0xf900_0001); // str x1, [x0]
                if self.chance(1, 2) {
                    words.push(asm::sys(op::TLBI_VMALLE1, 31));
                } else {
                    words.extend(asm::load(1, (probe.va >> 12) & ((1 << 44) - 1)));
                    let operation = if self.chance(1, 2) {
                        op::TLBI_VAAE1
                    } else {
                        op::TLBI_VAALE1
                    };
                    words.push(asm::sys(operation, 1));
                }
                words.push(asm::ISB);
            }
            // To EL0 or to EL1, with a saved state that the stream makes.
            74..=78 => {
                let saved = spsr(&mut self.generator.rng);
                words.extend(asm::load(16, saved));
                words.push(asm::msr(reg::SPSR_EL1, 16));
                let here = at + 4 * 5;
                let adr = self.generator.adr(16, here, here + 12);
                words.push(adr.unwrap_or(asm::NOP));
                words.push(asm::msr(reg::ELR_EL1, 16));
                words.push(asm::ERET);
            }
            // Back to EL1. From EL1 the call skips one instruction.
            79..=83 => {
                words.push(asm::svc(EXIT_CALL));
                words.push(asm::NOP);
            }
            // A control register, and the TLBI that drops everything.
            84..=89 => {
                let (register, value) = match self.below(8) {
                    0 | 1 => {
                        let half = self.below(2) as usize;
                        let asid = self.generator.rng.next_u64() << 48;
                        ([reg::TTBR0_EL1, reg::TTBR1_EL1][half], roots[half] | asid)
                    }
                    2 => (reg::MAIR_EL1, self.mair()),
                    // The MMU off, one time in three, and on again the next
                    // time.
                    _ => {
                        *enabled = !*enabled || !self.chance(1, 3);
                        (reg::SCTLR_EL1, self.sctlr(*enabled))
                    }
                };
                words.extend(asm::load(16, value));
                words.push(asm::msr(register, 16));
                words.push(asm::sys(op::TLBI_VMALLE1, 31));
                words.push(asm::ISB);
            }
            // An instruction of the integer groups, between the others.
            _ => {
                let formats = [IMMEDIATE_FORMATS.as_slice(), REGISTER_FORMATS.as_slice()].concat();
                let format = formats[self.below(formats.len() as u64) as usize];
                let records = records_of(format);
                let record = records[self.below(records.len() as u64) as usize];
                let (word, _) = self.generator.instruction(record, at, image);
                words.push(word);
            }
        }
    }
}

/// The format classes that a stream with the MMU on is counted by.
pub const STREAM_FORMATS: [FormatClass; 2] = [FormatClass::SystemXtI, FormatClass::SystemLXtI];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exception::{Cause, Exception};
    use crate::exec::{Cpu, Stop};
    use crate::state::State;
    use crate::sysreg::{self, QEMU_CORTEX_A53};
    use vmlab_hv_tcg::{FlatMemory, Memory};

    const RAM: u64 = 0x4000_0000;
    const LAYOUT: Layout = Layout { base: RAM + 0x0200_0000 };

    #[test]
    fn the_vector_table_is_sixteen_entries_of_code() {
        let table = vector_table(0x3c5);
        assert_eq!(table.len() as u64 * 4, VECTORS_BYTES);
        for word in &table {
            assert!(decode(*word).is_some(), "{word:08x}");
        }
        // The branch over the return to X30 lands on the skip.
        assert_eq!(table[4], asm::cbnz(17, 6));
        assert_eq!(table[10], asm::mrs(16, reg::SPSR_EL1));
        assert_eq!(table[8 * 32 + 8 + 4], asm::cbnz(17, 6));
    }

    /// Run streams on our interpreter alone: the MMU goes on, the stream
    /// reaches its probes at both levels, and every kind of fault is taken.
    #[test]
    fn a_stream_runs_on_our_interpreter_with_the_mmu_on() {
        let mut kinds = std::collections::BTreeMap::new();
        let (mut on, mut user, mut stores, mut entries) = (0u64, 0u64, 0u64, 0usize);
        for seed in 0..24 {
            let mut generator = Generator::new(seed);
            let image = generator.translated_image(LAYOUT, CORTEX_A53, 300);
            assert!(!image.probes.is_empty());
            let mut memory = FlatMemory::new(RAM, vec![0u8; 64 << 20]);
            for (address, bytes) in &image.memory {
                memory.write(*address, bytes).unwrap();
            }
            let mut cpu = Cpu::new(memory, State::reset_as(image.start, &QEMU_CORTEX_A53));
            cpu.log_accesses(true);
            for _ in 0..4000 {
                match cpu.step() {
                    Ok(()) => {}
                    Err(Stop::Hypercall { pc, .. }) => cpu.refuse_hypercall(pc),
                    Err(stop) => panic!("seed {seed}: {stop:?} at {:#x}", cpu.state.pc()),
                }
                let sctlr = cpu.state.system.regs[sysreg::reg::SCTLR_EL1];
                on += sctlr & 1;
                user += u64::from(cpu.state.context.el == 0);
                stores += cpu.take_accesses().iter().filter(|access| access.write).count() as u64;
                entries = entries.max(cpu.tlb_entries());
                if let Some(taken) = cpu.taken
                    && let Cause::Exception(exception) = taken.cause
                {
                    let kind = match exception {
                        Exception::DataAbort { status, .. } => 0x100 | u32::from(status),
                        Exception::InstructionAbort(status) => 0x200 | u32::from(status),
                        _ => 0,
                    };
                    *kinds.entry(kind).or_insert(0u64) += 1;
                }
            }
        }
        assert!(on > 40_000, "{on}");
        assert!(user > 2_000, "{user}");
        assert!(stores > 500, "{stores}");
        assert!(entries > 4, "{entries}");
        // Data aborts: translation, access flag and permission faults of
        // level 3, an alignment fault and an external abort. Instruction
        // aborts: a translation and a permission fault.
        for kind in [0x107, 0x10b, 0x10f, 0x121, 0x110, 0x207, 0x20f] {
            assert!(kinds.contains_key(&kind), "{kind:#x}: {kinds:x?}");
        }
    }

    #[test]
    fn the_walk_test_keeps_to_its_granules_and_to_armv8_0() {
        for seed in 0..40 {
            let mut generator = Generator::new(seed);
            let image = generator.translated_image(LAYOUT, MAX_WALK, 200);
            assert!(image.granules.iter().all(|granule| [14, 16].contains(granule)));
            let tcr = image.controls.tcr;
            // No bit above TBI1, sizes inside 16 to 39, and an output size
            // of at most 48 bits.
            assert_eq!(tcr >> 39, 0);
            assert!((16..=39).contains(&(tcr & 0x3f)) && (16..=39).contains(&(tcr >> 16 & 0x3f)));
            assert!(tcr >> 32 & 7 <= 5);
            // No word of the stream is an exclusive access.
            let code = &image.memory[0].1;
            for word in code.as_chunks::<4>().0 {
                let word = u32::from_le_bytes(*word);
                assert_ne!(word & 0x3f00_0000, 0x0800_0000, "{word:08x}");
            }
        }
    }
}
