//! The lifters of data processing with registers against a model written a
//! second time in plain Rust: wide integers for the carries, the overflow and
//! the products, and a loop of single bits for the CRC. Every record of the
//! nineteen format classes runs on random and boundary registers. The
//! lockstep against QEMU is the proof (VERIFICATION.md). This test keeps the
//! lifters where the lockstep left them, without QEMU.

use vmlab_hv_tcg::FlatMemory;
use vmlab_hv_tcg_aarch64::fields::{Condition, ones, sign_extend};
use vmlab_hv_tcg_aarch64::lift::REGISTER_FORMATS;
use vmlab_hv_tcg_aarch64::oracle::stream::{Generator, Kind, records_of};
use vmlab_hv_tcg_aarch64::{Cpu, FormatClass, Record, State, field};

const BASE: u64 = 0x4200_0000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    x: [u64; 31],
    sp: u64,
    nzcv: u8,
}

impl Model {
    /// Register `n`, where 31 is zero.
    fn zr(&self, n: u32) -> u64 {
        if n == 31 { 0 } else { self.x[n as usize] }
    }

    /// Register `n`, where 31 is the stack pointer.
    fn sp(&self, n: u32) -> u64 {
        if n == 31 { self.sp } else { self.x[n as usize] }
    }

    fn set_zr(&mut self, n: u32, value: u64) {
        if n != 31 {
            self.x[n as usize] = value;
        }
    }

    fn set_sp(&mut self, n: u32, value: u64) {
        if n == 31 {
            self.sp = value;
        } else {
            self.x[n as usize] = value;
        }
    }

    fn carry(&self) -> u64 {
        u64::from((self.nzcv >> 1) & 1)
    }

    fn holds(&self, condition: u32) -> bool {
        Condition::from_bits(condition).holds(
            self.nzcv & 8 != 0,
            self.nzcv & 4 != 0,
            self.nzcv & 2 != 0,
            self.nzcv & 1 != 0,
        )
    }
}

/// `a + b + carry` in `width` bits and its flags, by doing the sum twice in
/// wider integers: once unsigned for the carry, once signed for the overflow.
fn add_with_carry(width: u32, a: u64, b: u64, carry: u64) -> (u64, u8) {
    let mask = ones(width);
    let (a, b) = (a & mask, b & mask);
    let unsigned = u128::from(a) + u128::from(b) + u128::from(carry);
    let result = unsigned as u64 & mask;
    let signed =
        i128::from(sign_extend(a, width)) + i128::from(sign_extend(b, width)) + i128::from(carry);
    let n = (result >> (width - 1)) & 1 != 0;
    let z = result == 0;
    let c = unsigned >> width != 0;
    let v = i128::from(sign_extend(result, width)) != signed;
    (
        result,
        (u8::from(n) << 3) | (u8::from(z) << 2) | (u8::from(c) << 1) | u8::from(v),
    )
}

fn add_or_subtract(width: u32, subtract: bool, a: u64, b: u64) -> (u64, u8) {
    if subtract {
        add_with_carry(width, a, !b, 1)
    } else {
        add_with_carry(width, a, b, 0)
    }
}

fn logical_flags(width: u32, result: u64) -> u8 {
    (u8::from((result >> (width - 1)) & 1 != 0) << 3) | (u8::from(result == 0) << 2)
}

fn rotate_right(width: u32, value: u64, amount: u32) -> u64 {
    if width == 32 {
        (value as u32).rotate_right(amount).into()
    } else {
        value.rotate_right(amount)
    }
}

fn shift(width: u32, kind: u32, value: u64, amount: u32) -> u64 {
    let mask = ones(width);
    let value = value & mask;
    (match kind {
        0 => value << amount,
        1 => value >> amount,
        2 => (sign_extend(value, width) >> amount) as u64,
        _ => rotate_right(width, value, amount),
    }) & mask
}

fn extend(width: u32, option: u32, value: u64, left: u32) -> u64 {
    let bits = (8 << (option & 3)).min(width);
    let low = value & ones(bits);
    let extended = if option & 4 != 0 {
        sign_extend(low, bits) as u64
    } else {
        low
    };
    (extended << left) & ones(width)
}

fn crc(polynomial: u32, mut state: u32, data: u64, bytes: u32) -> u32 {
    for index in 0..bytes {
        state ^= (data >> (8 * index)) as u32 & 0xff;
        for _ in 0..8 {
            state = (state >> 1) ^ if state & 1 != 0 { polynomial } else { 0 };
        }
    }
    state
}

/// What `word` of `record` does to the registers, or `None` for a record
/// that this model does not know.
fn model(record: Record, word: u32, before: &Model) -> Option<Model> {
    let name = record.name();
    let get = |field_name: &str| field(record, field_name, word).expect("a field of the record");
    // No 64-bit record of the group but the CRC ones has a `W` in its name.
    let width = if name.contains('W') { 32 } else { 64 };
    let mask = ones(width);
    let mut after = before.clone();
    match record.info().format {
        FormatClass::BaseAddSubSReg => {
            let (rd, rn, rm, shift_field) = (get("rd"), get("rn"), get("rm"), get("shift"));
            let right = shift(width, shift_field >> 6, before.zr(rm), shift_field & 0x3f);
            let (result, flags) =
                add_or_subtract(width, name.starts_with("SUB"), before.zr(rn), right);
            if name.as_bytes()[3] == b'S' {
                after.nzcv = flags;
            }
            after.set_zr(rd, result);
        }
        FormatClass::BaseAddSubEReg | FormatClass::BaseAddSubEReg64 => {
            let (rd, rn, rm) = (get("rd"), get("rn"), get("rm"));
            // Bits 15 to 13 of the word are the option and 12 to 10 the shift.
            let (option, left) = ((word >> 13) & 7, (word >> 10) & 7);
            let right = extend(width, option, before.zr(rm), left);
            let (result, flags) =
                add_or_subtract(width, name.starts_with("SUB"), before.sp(rn), right);
            if name.as_bytes()[3] == b'S' {
                after.nzcv = flags;
                after.set_zr(rd, result);
            } else {
                after.set_sp(rd, result);
            }
        }
        FormatClass::BaseAddSubCarry | FormatClass::BaseAddSubCarrySetFlags => {
            let (rd, rn, rm) = (get("rd"), get("rn"), get("rm"));
            let right = if name.starts_with("SBC") {
                !before.zr(rm)
            } else {
                before.zr(rm)
            };
            let (result, flags) = add_with_carry(width, before.zr(rn), right, before.carry());
            if name.as_bytes()[3] == b'S' {
                after.nzcv = flags;
            }
            after.set_zr(rd, result);
        }
        FormatClass::BaseLogicalSReg => {
            let (rd, rn, rm, shift_field) = (get("rd"), get("rn"), get("rm"), get("shift"));
            let right = shift(width, shift_field >> 6, before.zr(rm), shift_field & 0x3f);
            let left = before.zr(rn) & mask;
            let result = match &name[..3] {
                "AND" => left & right,
                "BIC" => left & !right,
                "ORR" => left | right,
                "ORN" => left | !right,
                "EOR" => left ^ right,
                "EON" => left ^ !right,
                _ => return None,
            } & mask;
            if name.as_bytes()[3] == b'S' {
                after.nzcv = logical_flags(width, result);
            }
            after.set_zr(rd, result);
        }
        FormatClass::BaseCondComparisonImm | FormatClass::BaseCondComparisonReg => {
            let right = if name.ends_with('i') {
                get("imm").into()
            } else {
                before.zr(get("rm"))
            };
            after.nzcv = if before.holds(get("cond")) {
                add_or_subtract(width, name.starts_with("CCMP"), before.zr(get("rn")), right).1
            } else {
                get("nzcv") as u8
            };
        }
        FormatClass::BaseCondSelect | FormatClass::BaseCondSelectOp => {
            let source = before.zr(get("rm"));
            let result = if before.holds(get("cond")) {
                before.zr(get("rn"))
            } else {
                match &name[..5] {
                    "CSELW" | "CSELX" => source,
                    "CSINC" => source.wrapping_add(1),
                    "CSINV" => !source,
                    "CSNEG" => source.wrapping_neg(),
                    _ => return None,
                }
            };
            after.set_zr(get("rd"), result & mask);
        }
        FormatClass::BaseShift => {
            let amount = (before.zr(get("rm")) % u64::from(width)) as u32;
            let kind = match &name[..3] {
                "LSL" => 0,
                "LSR" => 1,
                "ASR" => 2,
                "ROR" => 3,
                _ => return None,
            };
            after.set_zr(get("rd"), shift(width, kind, before.zr(get("rn")), amount));
        }
        FormatClass::BaseDiv => {
            let (left, right) = (before.zr(get("rn")) & mask, before.zr(get("rm")) & mask);
            let result = if right == 0 {
                0
            } else if name.starts_with('U') {
                left / right
            } else {
                let quotient =
                    i128::from(sign_extend(left, width)) / i128::from(sign_extend(right, width));
                quotient as u64
            };
            after.set_zr(get("rd"), result & mask);
        }
        FormatClass::BaseMulAccum => {
            let product = before.zr(get("rn")).wrapping_mul(before.zr(get("rm")));
            let addend = before.zr(get("ra"));
            let result = if name.starts_with("MSUB") {
                addend.wrapping_sub(product)
            } else {
                addend.wrapping_add(product)
            };
            after.set_zr(get("rd"), result & mask);
        }
        FormatClass::WideMulAccum => {
            let (left, right) = (before.zr(get("rn")) as u32, before.zr(get("rm")) as u32);
            let product = if name.starts_with('S') {
                (i64::from(left as i32) * i64::from(right as i32)) as u64
            } else {
                u64::from(left) * u64::from(right)
            };
            let addend = before.zr(get("ra"));
            let result = if name.contains("SUB") {
                addend.wrapping_sub(product)
            } else {
                addend.wrapping_add(product)
            };
            after.set_zr(get("rd"), result);
        }
        FormatClass::MulHi => {
            let (left, right) = (before.zr(get("rn")), before.zr(get("rm")));
            let result = if name.starts_with('S') {
                ((i128::from(left as i64) * i128::from(right as i64)) >> 64) as u64
            } else {
                ((u128::from(left) * u128::from(right)) >> 64) as u64
            };
            after.set_zr(get("rd"), result);
        }
        FormatClass::BaseOneOperandData => {
            let value = before.zr(get("rn")) & mask;
            let result = match &name[..3] {
                "RBI" => value.reverse_bits() >> (64 - width),
                "CLZ" => u64::from(value.leading_zeros()) - u64::from(64 - width),
                "CLS" => {
                    // Count the bits below the sign that repeat it.
                    let sign = (value >> (width - 1)) & 1;
                    let mut count = 0;
                    for index in (0..width - 1).rev() {
                        if (value >> index) & 1 != sign {
                            break;
                        }
                        count += 1;
                    }
                    count
                }
                _ => return None,
            };
            after.set_zr(get("rd"), result);
        }
        FormatClass::OneWRegData | FormatClass::OneXRegData => {
            let value = before.zr(get("rn")) & mask;
            // The size of the parts whose bytes are reversed.
            let part = match name {
                "REV16Wr" | "REV16Xr" => 2,
                "REVWr" | "REV32Xr" => 4,
                "REVXr" => 8,
                _ => return None,
            };
            let mut bytes = value.to_le_bytes();
            for chunk in bytes[..(width / 8) as usize].chunks_mut(part) {
                chunk.reverse();
            }
            after.set_zr(get("rd"), u64::from_le_bytes(bytes));
        }
        FormatClass::BaseCRC32 => {
            let polynomial = if name.starts_with("CRC32C") {
                0x82f6_3b78
            } else {
                0xedb8_8320
            };
            let bytes = match name.as_bytes()[name.len() - 3] {
                b'B' => 1,
                b'H' => 2,
                b'W' => 4,
                b'X' => 8,
                _ => return None,
            };
            let result = crc(
                polynomial,
                before.zr(get("rn")) as u32,
                before.zr(get("rm")),
                bytes,
            );
            after.set_zr(get("rd"), result.into());
        }
        _ => return None,
    }
    Some(after)
}

fn run(word: u32, before: &Model) -> Result<Model, String> {
    let mut data = vec![0u8; 0x1000];
    data[..4].copy_from_slice(&word.to_le_bytes());
    let mut state = State::reset(BASE);
    for (n, value) in before.x.iter().enumerate() {
        state.set_x(n, *value);
    }
    state.set_sp(before.sp);
    state.set_nzcv(before.nzcv);
    let mut cpu = Cpu::new(FlatMemory::new(BASE, data), state);
    cpu.step().map_err(|stop| format!("{stop:?}"))?;
    if cpu.state.pc() != BASE + 4 {
        return Err(format!("the PC is {:#x}", cpu.state.pc()));
    }
    let mut x = [0; 31];
    for (n, value) in x.iter_mut().enumerate() {
        *value = cpu.state.x(n);
    }
    Ok(Model {
        x,
        sp: cpu.state.sp(),
        nzcv: cpu.state.nzcv(),
    })
}

#[test]
fn every_record_agrees_with_the_model_on_random_and_boundary_registers() {
    let mut generator = Generator::new(0xc2);
    generator.unconstrained_one_in = 0;
    let mut checked = 0u64;
    let mut records = 0;
    for format in REGISTER_FORMATS {
        for record in records_of(format) {
            records += 1;
            for _ in 0..400 {
                let (word, kind) = generator.instruction(record, BASE, (BASE, 0x1000));
                assert_eq!(kind, Kind::Valid, "{}", record.name());
                let (x, sp, nzcv) = generator.registers((BASE, 0x1000));
                let before = Model { x, sp, nzcv };
                let expected = model(record, word, &before)
                    .unwrap_or_else(|| panic!("no model for {}", record.name()));
                let ours = run(word, &before)
                    .unwrap_or_else(|error| panic!("{} {word:08x}: {error}", record.name()));
                assert_eq!(
                    ours,
                    expected,
                    "{} {word:08x} from {before:x?}",
                    record.name()
                );
                checked += 1;
            }
        }
    }
    assert_eq!(records, 101);
    println!("checked {checked} instructions of {records} records");
}

/// The cases that random registers reach seldom.
#[test]
fn the_corners_of_divide_shift_and_carry() {
    let zero = Model {
        x: [0; 31],
        sp: 0,
        nzcv: 0,
    };
    let with = |values: &[(usize, u64)], nzcv: u8| {
        let mut model = zero.clone();
        for (n, value) in values {
            model.x[*n] = *value;
        }
        model.nzcv = nzcv;
        model
    };
    // sdiv x0, x1, x2: the most negative number by minus one is itself.
    let after = run(0x9ac2_0c20, &with(&[(1, 1 << 63), (2, u64::MAX)], 0)).unwrap();
    assert_eq!(after.x[0], 1 << 63);
    // sdiv w0, w1, w2: the same in 32 bits, and the upper half is clear.
    let after = run(0x1ac2_0c20, &with(&[(1, 0x8000_0000), (2, 0xffff_ffff)], 0)).unwrap();
    assert_eq!(after.x[0], 0x8000_0000);
    // sdiv and udiv by zero give zero.
    let after = run(0x9ac2_0c20, &with(&[(0, 7), (1, 5)], 0)).unwrap();
    assert_eq!(after.x[0], 0);
    let after = run(0x9ac2_0820, &with(&[(0, 7), (1, 5)], 0)).unwrap();
    assert_eq!(after.x[0], 0);
    // sdiv x0, x1, x2: -7 / 2 rounds toward zero.
    let after = run(0x9ac2_0c20, &with(&[(1, -7i64 as u64), (2, 2)], 0)).unwrap();
    assert_eq!(after.x[0], -3i64 as u64);
    // ror x0, x1, x2 with an amount of 64 is a rotation by 0.
    let after = run(0x9ac2_2c20, &with(&[(1, 0x1234), (2, 64)], 0)).unwrap();
    assert_eq!(after.x[0], 0x1234);
    // lsl w0, w1, w2 with an amount of 33 shifts by 1.
    let after = run(0x1ac2_2020, &with(&[(1, 0xc000_0001), (2, 33)], 0)).unwrap();
    assert_eq!(after.x[0], 0x8000_0002);
    // adcs x0, x1, x2: all ones plus zero plus the carry wraps to zero with
    // a carry out and no overflow.
    let after = run(0xba02_0020, &with(&[(1, u64::MAX)], 0b0010)).unwrap();
    assert_eq!((after.x[0], after.nzcv), (0, 0b0110));
    // sbcs x0, x1, x2 without the carry: 0 - 0 - 1 is minus one, a borrow.
    let after = run(0xfa02_0020, &zero).unwrap();
    assert_eq!((after.x[0], after.nzcv), (u64::MAX, 0b1000));
    // ccmp x1, x2, #0b1010, eq with Z clear: the flags are the immediate.
    let after = run(0xfa42_002a, &with(&[(1, 1), (2, 1)], 0)).unwrap();
    assert_eq!(after.nzcv, 0b1010);
    // The same with Z set: the flags of 1 - 1.
    let after = run(0xfa42_002a, &with(&[(1, 1), (2, 1)], 0b0100)).unwrap();
    assert_eq!(after.nzcv, 0b0110);
    // umulh x0, x1, x2 and smulh x0, x1, x2 of all ones by all ones.
    let after = run(0x9bc2_7c20, &with(&[(1, u64::MAX), (2, u64::MAX)], 0)).unwrap();
    assert_eq!(after.x[0], u64::MAX - 1);
    let after = run(0x9b42_7c20, &with(&[(1, u64::MAX), (2, u64::MAX)], 0)).unwrap();
    assert_eq!(after.x[0], 0);
    // cls x0, x1 of zero and of minus one is 63. clz x0, x1 of zero is 64.
    assert_eq!(run(0xdac0_1420, &zero).unwrap().x[0], 63);
    let after = run(0xdac0_1420, &with(&[(1, u64::MAX)], 0)).unwrap();
    assert_eq!(after.x[0], 63);
    assert_eq!(run(0xdac0_1020, &zero).unwrap().x[0], 64);
    // crc32b w0, w1, w2: the CRC-32 of one byte 0x61 from a state of all
    // ones is the inverse of the checksum of "a", 0xe8b7be43.
    let after = run(0x1ac2_4020, &with(&[(1, 0xffff_ffff), (2, 0x61)], 0)).unwrap();
    assert_eq!(after.x[0], u64::from(!0xe8b7_be43u32));
    // add sp, sp, x1, uxtx #3 writes the stack pointer, and
    // adds xzr, sp, x1 (cmn sp, x1) reads it and writes no register.
    let mut stack = with(&[(1, 2)], 0);
    stack.sp = 0x1000;
    let after = run(0x8b21_6fff, &stack).unwrap();
    assert_eq!(after.sp, 0x1010);
    let after = run(0xab21_63ff, &stack).unwrap();
    assert_eq!((after.sp, after.nzcv), (0x1000, 0));
    assert_eq!(after.x, stack.x);
}

#[test]
fn a_block_of_sixty_four_wide_crcs_translates_and_runs() {
    // crc32cx w0, w0, x1, sixty-four times: the largest graph of the group.
    let word = 0x9ac1_5c00u32;
    let mut data = vec![0u8; 0x1000];
    for index in 0..64 {
        data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    let mut state = State::reset(BASE);
    state.set_x(0, 0xffff_ffff);
    state.set_x(1, 0x0123_4567_89ab_cdef);
    let mut cpu = Cpu::new(FlatMemory::new(BASE, data), state);
    assert_eq!(cpu.run(64), vmlab_hv_tcg_aarch64::Stop::Budget);
    assert_eq!(cpu.translations, 1);
    let mut expected = 0xffff_ffffu32;
    for _ in 0..64 {
        expected = crc(0x82f6_3b78, expected, 0x0123_4567_89ab_cdef, 8);
    }
    assert_eq!(cpu.state.x(0), u64::from(expected));
}
