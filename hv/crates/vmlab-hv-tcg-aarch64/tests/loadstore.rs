//! Loads and stores on a flat memory (card C3): every addressing mode, the
//! pairs, the vector register forms, the structure loads and stores, the
//! exclusives on one CPU and on two, and what a failed access leaves.
//!
//! The instructions are built from the record tables, so a word here is
//! the record that its line names.

use vmlab_hv_tcg::{FlatMemory, Memory, SharedMemory};
use vmlab_hv_tcg_aarch64::state::slot;
use vmlab_hv_tcg_aarch64::{Cpu, Record, State, Stop, decode, with_field};

const BASE: u64 = 0x4000_0000;
/// The data of the tests, two pages above the code.
const DATA: u64 = BASE + 0x2000;
const SIZE: usize = 0x4000;

/// The word of `record` with these fields and every other free bit zero.
fn word(record: Record, fields: &[(&str, u32)]) -> u32 {
    let mut word = record.info().value;
    for (name, value) in fields {
        word = with_field(record, name, word, *value)
            .unwrap_or_else(|| panic!("{} has no field {name}", record.name()));
    }
    assert_eq!(
        decode(word).map(|decoded| decoded.record),
        Some(record),
        "{word:08x} is not {}",
        record.name()
    );
    word
}

fn image(words: &[u32]) -> Vec<u8> {
    let mut data = vec![0u8; SIZE];
    for (index, word) in words.iter().enumerate() {
        data[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    // A pattern in the data pages: the byte at DATA + n is n.
    for (index, byte) in data.iter_mut().enumerate().skip(0x2000) {
        *byte = index as u8;
    }
    data
}

fn cpu(words: &[u32]) -> Cpu<FlatMemory> {
    Cpu::new(FlatMemory::new(BASE, image(words)), State::reset(BASE))
}

fn read(cpu: &mut Cpu<impl Memory>, address: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    cpu.memory.read(address, &mut bytes).unwrap();
    bytes
}

fn v(cpu: &Cpu<impl Memory>, n: usize) -> u128 {
    cpu.state.slots[slot::V0 + n]
}

fn set_v(cpu: &mut Cpu<impl Memory>, n: usize, value: u128) {
    cpu.state.slots[slot::V0 + n] = value;
}

/// The eight bytes of the pattern from `DATA + offset`, little-endian.
fn pattern(offset: u64, bytes: u32) -> u64 {
    (0..u64::from(bytes)).fold(0, |value, index| {
        value | ((offset + index) & 0xff) << (8 * index)
    })
}

#[test]
fn every_addressing_mode_finds_its_address_and_writes_its_base_back() {
    let mut cpu = cpu(&[
        // ldr x1, [x0, #16]
        word(Record::LDRXui, &[("rt", 1), ("rn", 0), ("offset", 2)]),
        // ldur x2, [x0, #-3]
        word(Record::LDURXi, &[("rt", 2), ("rn", 0), ("offset", 0x1fd)]),
        // ldr x3, [x0, #8]!
        word(Record::LDRXpre, &[("rt", 3), ("rn", 0), ("offset", 8)]),
        // ldr x4, [x0], #-24
        word(Record::LDRXpost, &[("rt", 4), ("rn", 0), ("offset", 0x1e8)]),
        // ldr x5, [x0, x10, lsl #3]
        word(
            Record::LDRXroX,
            &[("rt", 5), ("rn", 0), ("rm", 10), ("extend", 1)],
        ),
        // ldr x6, [x0, w11, sxtw]
        word(
            Record::LDRXroW,
            &[("rt", 6), ("rn", 0), ("rm", 11), ("extend", 2)],
        ),
        // ldr w7, [x0, w12, uxtw #2]: the upper half of x12 is not part of the offset.
        word(
            Record::LDRWroW,
            &[("rt", 7), ("rn", 0), ("rm", 12), ("extend", 1)],
        ),
        // ldr x8, .+0x2000 - 28: the first data page.
        word(Record::LDRXl, &[("rt", 8), ("label", (0x2000 - 28) / 4)]),
        // ldr x9, [sp, #8]: register 31 is the stack pointer as a base.
        word(Record::LDRXui, &[("rt", 9), ("rn", 31), ("offset", 1)]),
    ]);
    cpu.state.set_x(0, DATA + 0x100);
    cpu.state.set_x(10, 5);
    cpu.state.set_x(11, 0xffff_fff0);
    cpu.state.set_x(12, 0xdead_beef_0000_0003);
    cpu.state.set_sp(DATA + 0x200);
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 9);
    assert_eq!(cpu.state.x(1), pattern(0x110, 8));
    assert_eq!(cpu.state.x(2), pattern(0xfd, 8));
    assert_eq!(cpu.state.x(3), pattern(0x108, 8));
    // The post-indexed load reads at the base that the pre-indexed one left.
    assert_eq!(cpu.state.x(4), pattern(0x108, 8));
    // 0x108 - 24.
    assert_eq!(cpu.state.x(0), DATA + 0xf0);
    assert_eq!(cpu.state.x(5), pattern(0xf0 + 40, 8));
    assert_eq!(cpu.state.x(6), pattern(0xf0 - 16, 8));
    assert_eq!(cpu.state.x(7), pattern(0xf0 + 12, 4));
    assert_eq!(cpu.state.x(8), pattern(0, 8));
    assert_eq!(cpu.state.x(9), pattern(0x208, 8));
}

#[test]
fn a_load_extends_with_zeros_or_with_the_sign_to_its_register() {
    let mut cpu = cpu(&[
        word(Record::LDRBBui, &[("rt", 1), ("rn", 0)]),
        word(Record::LDRSBWui, &[("rt", 2), ("rn", 0)]),
        word(Record::LDRSBXui, &[("rt", 3), ("rn", 0)]),
        word(Record::LDRHHui, &[("rt", 4), ("rn", 0)]),
        word(Record::LDRSHWui, &[("rt", 5), ("rn", 0)]),
        word(Record::LDRSHXui, &[("rt", 6), ("rn", 0)]),
        word(Record::LDRWui, &[("rt", 7), ("rn", 0)]),
        word(Record::LDRSWui, &[("rt", 8), ("rn", 0)]),
        // A load into register 31 reads memory and writes no register.
        word(Record::LDRXui, &[("rt", 31), ("rn", 0)]),
    ]);
    // The bytes at DATA + 0xf0 are f0 f1 f2 f3: negative at every size.
    cpu.state.set_x(0, DATA + 0xf0);
    for n in 1..=8 {
        cpu.state.set_x(n, u64::MAX);
    }
    let sp = cpu.state.sp();
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 9);
    assert_eq!(cpu.state.x(1), 0xf0);
    assert_eq!(cpu.state.x(2), 0xffff_fff0);
    assert_eq!(cpu.state.x(3), 0xffff_ffff_ffff_fff0);
    assert_eq!(cpu.state.x(4), 0xf1f0);
    assert_eq!(cpu.state.x(5), 0xffff_f1f0);
    assert_eq!(cpu.state.x(6), 0xffff_ffff_ffff_f1f0);
    assert_eq!(cpu.state.x(7), 0xf3f2_f1f0);
    assert_eq!(cpu.state.x(8), 0xffff_ffff_f3f2_f1f0);
    assert_eq!(cpu.state.sp(), sp);
}

#[test]
fn a_store_writes_the_low_bytes_of_its_register_and_nothing_beside_them() {
    let mut cpu = cpu(&[
        word(Record::STRBBui, &[("rt", 1), ("rn", 0), ("offset", 1)]),
        word(Record::STRHHui, &[("rt", 1), ("rn", 0), ("offset", 4)]),
        word(Record::STRWui, &[("rt", 1), ("rn", 0), ("offset", 4)]),
        word(Record::STRXui, &[("rt", 1), ("rn", 0), ("offset", 4)]),
        // str xzr, [x0, #48]
        word(Record::STRXui, &[("rt", 31), ("rn", 0), ("offset", 6)]),
        // str x0, [x0, #-8]!: the value is the base before the write back.
        word(Record::STRXpre, &[("rt", 0), ("rn", 0), ("offset", 0x1f8)]),
    ]);
    cpu.state.set_x(0, DATA + 0x400);
    cpu.state.set_x(1, 0x1122_3344_5566_7788);
    cpu.memory.write(DATA + 0x400, &[0xaa; 64]).unwrap();
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    let bytes = read(&mut cpu, DATA + 0x400, 64);
    assert_eq!(bytes[..4], [0xaa, 0x88, 0xaa, 0xaa]);
    assert_eq!(bytes[8..12], [0x88, 0x77, 0xaa, 0xaa]);
    assert_eq!(bytes[16..24], [0x88, 0x77, 0x66, 0x55, 0xaa, 0xaa, 0xaa, 0xaa]);
    assert_eq!(bytes[32..40], 0x1122_3344_5566_7788u64.to_le_bytes());
    assert_eq!(bytes[48..56], [0; 8]);
    assert_eq!(cpu.state.x(0), DATA + 0x3f8);
    assert_eq!(
        read(&mut cpu, DATA + 0x3f8, 8),
        (DATA + 0x400).to_le_bytes()
    );
}

#[test]
fn a_pair_is_two_registers_side_by_side_with_the_first_at_the_lower_address() {
    let mut cpu = cpu(&[
        // stp x1, x2, [sp, #-16]!
        word(
            Record::STPXpre,
            &[("rt", 1), ("rt2", 2), ("rn", 31), ("offset", 0x7e)],
        ),
        // ldp x3, x4, [sp], #16
        word(
            Record::LDPXpost,
            &[("rt", 3), ("rt2", 4), ("rn", 31), ("offset", 2)],
        ),
        // ldpsw x5, x6, [x0, #8]
        word(
            Record::LDPSWi,
            &[("rt", 5), ("rt2", 6), ("rn", 0), ("offset", 2)],
        ),
        // ldp w7, w8, [x0, #-4]
        word(
            Record::LDPWi,
            &[("rt", 7), ("rt2", 8), ("rn", 0), ("offset", 0x7f)],
        ),
        // stp q1, q2, [x9, #32]
        word(
            Record::STPQi,
            &[("rt", 1), ("rt2", 2), ("rn", 9), ("offset", 2)],
        ),
        // ldnp d3, d4, [x9, #40]
        word(
            Record::LDNPDi,
            &[("rt", 3), ("rt2", 4), ("rn", 9), ("offset", 5)],
        ),
    ]);
    cpu.state.set_sp(DATA + 0x800);
    cpu.state.set_x(0, DATA + 0xf0);
    cpu.state.set_x(1, 0x1111);
    cpu.state.set_x(2, 0x2222);
    cpu.state.set_x(9, DATA + 0x1000);
    set_v(&mut cpu, 1, 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100);
    set_v(&mut cpu, 2, 0x1f1e_1d1c_1b1a_1918_1716_1514_1312_1110);
    set_v(&mut cpu, 3, u128::MAX);
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 6);
    assert_eq!(cpu.state.sp(), DATA + 0x800);
    assert_eq!(read(&mut cpu, DATA + 0x7f0, 8), 0x1111u64.to_le_bytes());
    assert_eq!(read(&mut cpu, DATA + 0x7f8, 8), 0x2222u64.to_le_bytes());
    assert_eq!((cpu.state.x(3), cpu.state.x(4)), (0x1111, 0x2222));
    assert_eq!(cpu.state.x(5), 0xffff_ffff_fbfa_f9f8);
    assert_eq!(cpu.state.x(6), 0xffff_ffff_fffe_fdfc);
    assert_eq!(cpu.state.x(7), 0xefee_edec);
    assert_eq!(cpu.state.x(8), 0xf3f2_f1f0);
    let stored: Vec<u8> = (0..32).collect();
    assert_eq!(read(&mut cpu, DATA + 0x1020, 32), stored);
    // A load into the low part of a vector register clears the rest.
    assert_eq!(v(&cpu, 3), 0x0f0e_0d0c_0b0a_0908);
    assert_eq!(v(&cpu, 4), 0x1716_1514_1312_1110);
}

#[test]
fn a_vector_register_moves_its_low_bytes_and_a_load_clears_the_rest() {
    let mut cpu = cpu(&[
        word(Record::STRBui, &[("rt", 1), ("rn", 0)]),
        word(Record::STRHui, &[("rt", 1), ("rn", 0), ("offset", 1)]),
        word(Record::STRSui, &[("rt", 1), ("rn", 0), ("offset", 1)]),
        word(Record::STRDui, &[("rt", 1), ("rn", 0), ("offset", 1)]),
        word(Record::STRQui, &[("rt", 1), ("rn", 0), ("offset", 1)]),
        word(Record::LDRBui, &[("rt", 2), ("rn", 0), ("offset", 16)]),
        word(Record::LDRHui, &[("rt", 3), ("rn", 0), ("offset", 8)]),
        word(Record::LDRSui, &[("rt", 4), ("rn", 0), ("offset", 4)]),
        word(Record::LDRDui, &[("rt", 5), ("rn", 0), ("offset", 2)]),
        word(Record::LDRQui, &[("rt", 6), ("rn", 0), ("offset", 1)]),
        // ldr q7, .+0x2000 - 40
        word(Record::LDRQl, &[("rt", 7), ("label", (0x2000 - 40) / 4)]),
        // ldr q8, [x0, x9, lsl #4]
        word(
            Record::LDRQroX,
            &[("rt", 8), ("rn", 0), ("rm", 9), ("extend", 1)],
        ),
    ]);
    let value = 0xffee_ddcc_bbaa_9988_7766_5544_3322_1100u128;
    cpu.state.set_x(0, DATA + 0x400);
    cpu.state.set_x(9, 1);
    cpu.memory.write(DATA + 0x400, &[0xaa; 32]).unwrap();
    set_v(&mut cpu, 1, value);
    for n in 2..=8 {
        set_v(&mut cpu, n, u128::MAX);
    }
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 12);
    let bytes = read(&mut cpu, DATA + 0x400, 32);
    assert_eq!(bytes[..4], [0x00, 0xaa, 0x00, 0x11]);
    assert_eq!(bytes[4..8], [0x00, 0x11, 0x22, 0x33]);
    assert_eq!(bytes[8..16], (value as u64).to_le_bytes());
    assert_eq!(bytes[16..32], value.to_le_bytes());
    assert_eq!(v(&cpu, 2), 0x00);
    assert_eq!(v(&cpu, 3), 0x1100);
    assert_eq!(v(&cpu, 4), 0x3322_1100);
    assert_eq!(v(&cpu, 5), 0x7766_5544_3322_1100);
    assert_eq!(v(&cpu, 6), value);
    assert_eq!(v(&cpu, 7), u128::from_le_bytes(core::array::from_fn(|n| n as u8)));
    assert_eq!(v(&cpu, 8), value);
}

#[test]
fn whole_register_structures_lie_in_memory_in_turn_or_one_register_after_the_other() {
    let mut cpu = cpu(&[
        // ld2 {v30.4s, v31.4s}, [x0], #32
        word(
            Record::LD2Twov4s_POST,
            &[("vt", 30), ("rn", 0), ("xm", 31)],
        ),
        // ld4 {v31.8b, v0.8b, v1.8b, v2.8b}, [x1]: the list wraps to V0.
        word(Record::LD4Fourv8b, &[("vt", 31), ("rn", 1)]),
        // ld1 {v3.2d, v4.2d, v5.2d}, [x1], x9
        word(
            Record::LD1Threev2d_POST,
            &[("vt", 3), ("rn", 1), ("xm", 9)],
        ),
        // ld3 {v6.8h, v7.8h, v8.8h}, [x2]
        word(Record::LD3Threev8h, &[("vt", 6), ("rn", 2)]),
        // st2 {v6.8h, v7.8h}, [x3]
        word(Record::ST2Twov8h, &[("vt", 6), ("rn", 3)]),
        // st1 {v8.1d}, [x4], #8
        word(
            Record::ST1Onev1d_POST,
            &[("vt", 8), ("rn", 4), ("xm", 31)],
        ),
        // st4 {v3.2s, v4.2s, v5.2s, v6.2s}, [x5]
        word(Record::ST4Fourv2s, &[("vt", 3), ("rn", 5)]),
    ]);
    cpu.state.set_x(0, DATA);
    cpu.state.set_x(1, DATA + 0x40);
    cpu.state.set_x(2, DATA + 0x80);
    cpu.state.set_x(3, DATA + 0x1000);
    cpu.state.set_x(4, DATA + 0x1040);
    cpu.state.set_x(5, DATA + 0x1080);
    cpu.state.set_x(9, 0x1000);
    for n in 0..32 {
        set_v(&mut cpu, n, u128::MAX);
    }
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 7);
    // LD2 of 32-bit elements: the even words to the first register.
    let words = |offsets: [u64; 4]| {
        offsets.iter().enumerate().fold(0u128, |value, (n, offset)| {
            value | u128::from(pattern(*offset, 4)) << (32 * n)
        })
    };
    assert_eq!(v(&cpu, 30), words([0, 8, 16, 24]));
    assert_eq!(cpu.state.x(0), DATA + 32);
    // LD4 of bytes into 64-bit registers: every fourth byte, the rest zero.
    let every_fourth = |first: u64| {
        (0..8u64).fold(0u128, |value, n| {
            value | u128::from(0x40 + first + 4 * n) << (8 * n)
        })
    };
    assert_eq!(v(&cpu, 31), every_fourth(0));
    assert_eq!(v(&cpu, 0), every_fourth(1));
    assert_eq!(v(&cpu, 1), every_fourth(2));
    assert_eq!(v(&cpu, 2), every_fourth(3));
    // LD1 of three registers: 48 bytes in their order. The base moves by X9.
    let run = |first: u64| {
        (0..16u64).fold(0u128, |value, n| {
            value | u128::from((first + n) & 0xff) << (8 * n)
        })
    };
    assert_eq!(v(&cpu, 3), run(0x40));
    assert_eq!(v(&cpu, 4), run(0x50));
    assert_eq!(v(&cpu, 5), run(0x60));
    assert_eq!(cpu.state.x(1), DATA + 0x1040);
    // LD3 of 16-bit elements.
    let thirds = |first: u64| {
        (0..8u64).fold(0u128, |value, n| {
            value | u128::from(pattern(0x80 + first + 6 * n, 2)) << (16 * n)
        })
    };
    assert_eq!(v(&cpu, 6), thirds(0));
    assert_eq!(v(&cpu, 7), thirds(2));
    assert_eq!(v(&cpu, 8), thirds(4));
    // ST2 of V6 and V7 puts the first two thirds back in turn.
    let stored = read(&mut cpu, DATA + 0x1000, 32);
    for element in 0..8usize {
        let at = 0x80 + 6 * element as u8;
        assert_eq!(
            stored[element * 4..element * 4 + 4],
            [at, at + 1, at + 2, at + 3],
            "element {element}"
        );
    }
    // ST1 of one 64-bit register writes 8 bytes and moves the base by 8.
    assert_eq!(
        read(&mut cpu, DATA + 0x1040, 16)[..8],
        (thirds(4) as u64).to_le_bytes()
    );
    assert_eq!(read(&mut cpu, DATA + 0x1048, 8), pattern(0x1048, 8).to_le_bytes());
    assert_eq!(cpu.state.x(4), DATA + 0x1048);
    // ST4 of two 32-bit elements each: e0 of V3, V4, V5, V6, then e1.
    let stored = read(&mut cpu, DATA + 0x1080, 32);
    for (index, register) in [3usize, 4, 5, 6, 3, 4, 5, 6].into_iter().enumerate() {
        let element = (v(&cpu, register) >> (32 * (index / 4))) as u32;
        assert_eq!(
            stored[index * 4..index * 4 + 4],
            element.to_le_bytes(),
            "part {index}"
        );
    }
}

#[test]
fn a_single_lane_keeps_the_other_lanes_and_a_replicated_one_fills_them() {
    let mut cpu = cpu(&[
        // ld1 {v1.s}[2], [x0]
        word(Record::LD1i32, &[("vt", 1), ("rn", 0), ("idx", 2)]),
        // ld2 {v2.b, v3.b}[15], [x0], #2
        word(
            Record::LD2i8_POST,
            &[("vt", 2), ("rn", 0), ("idx", 15), ("xm", 31)],
        ),
        // ld3 {v4.d, v5.d, v6.d}[0], [x1], x9
        word(
            Record::LD3i64_POST,
            &[("vt", 4), ("rn", 1), ("idx", 0), ("xm", 9)],
        ),
        // ld4 {v31.h, v0.h, v1.h, v2.h}[7], [x2]
        word(Record::LD4i16, &[("vt", 31), ("rn", 2), ("idx", 7)]),
        // st1 {v10.b}[3], [x3]
        word(Record::ST1i8, &[("vt", 10), ("rn", 3), ("idx", 3)]),
        // st2 {v10.s, v11.s}[1], [x3, #8]: through X4.
        word(Record::ST2i32, &[("vt", 10), ("rn", 4), ("idx", 1)]),
        // st3 {v10.d, v11.d, v12.d}[1], [x5], #24
        word(
            Record::ST3i64_POST,
            &[("vt", 10), ("rn", 5), ("idx", 1), ("xm", 31)],
        ),
        // ld1r {v20.8b}, [x6]
        word(Record::LD1Rv8b, &[("vt", 20), ("rn", 6)]),
        // ld2r {v21.4s, v22.4s}, [x6], #8
        word(
            Record::LD2Rv4s_POST,
            &[("vt", 21), ("rn", 6), ("xm", 31)],
        ),
        // ld3r {v23.2d, v24.2d, v25.2d}, [x7]
        word(Record::LD3Rv2d, &[("vt", 23), ("rn", 7)]),
        // ld1r {v26.1d}, [x7]
        word(Record::LD1Rv1d, &[("vt", 26), ("rn", 7)]),
    ]);
    cpu.state.set_x(0, DATA + 0x10);
    cpu.state.set_x(1, DATA + 0x20);
    cpu.state.set_x(2, DATA + 0x40);
    cpu.state.set_x(3, DATA + 0x1000);
    cpu.state.set_x(4, DATA + 0x1008);
    cpu.state.set_x(5, DATA + 0x1010);
    cpu.state.set_x(6, DATA + 0x60);
    cpu.state.set_x(7, DATA + 0x80);
    cpu.state.set_x(9, 0x100);
    for n in 0..32 {
        set_v(&mut cpu, n, 0xeeee_eeee_eeee_eeee_eeee_eeee_eeee_eeee);
    }
    set_v(&mut cpu, 10, 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100);
    set_v(&mut cpu, 11, 0x1f1e_1d1c_1b1a_1918_1716_1514_1312_1110);
    set_v(&mut cpu, 12, 0x2f2e_2d2c_2b2a_2928_2726_2524_2322_2120);
    cpu.memory.write(DATA + 0x1000, &[0xaa; 64]).unwrap();
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 11);
    let e = 0xeeee_eeee_eeee_eeee_eeee_eeee_eeee_eeeeu128;
    // LD4 of 16-bit lanes ran later and changed the lane 7 of V1 and V2.
    let lane = |old: u128, bits: u32, index: u32, value: u64| {
        let mask = ((1u128 << bits) - 1) << (bits * index);
        (old & !mask) | u128::from(value) << (bits * index)
    };
    let v1 = lane(e, 32, 2, pattern(0x10, 4));
    assert_eq!(v(&cpu, 1), lane(v1, 16, 7, pattern(0x44, 2)));
    let v2 = lane(e, 8, 15, 0x10);
    assert_eq!(v(&cpu, 2), lane(v2, 16, 7, pattern(0x46, 2)));
    assert_eq!(v(&cpu, 3), lane(e, 8, 15, 0x11));
    assert_eq!(cpu.state.x(0), DATA + 0x12);
    assert_eq!(v(&cpu, 4), lane(e, 64, 0, pattern(0x20, 8)));
    assert_eq!(v(&cpu, 5), lane(e, 64, 0, pattern(0x28, 8)));
    assert_eq!(v(&cpu, 6), lane(e, 64, 0, pattern(0x30, 8)));
    assert_eq!(cpu.state.x(1), DATA + 0x120);
    assert_eq!(v(&cpu, 31), lane(e, 16, 7, pattern(0x40, 2)));
    assert_eq!(v(&cpu, 0), lane(e, 16, 7, pattern(0x42, 2)));
    let stored = read(&mut cpu, DATA + 0x1000, 0x30);
    assert_eq!(stored[..2], [0x03, 0xaa]);
    assert_eq!(stored[8..16], [0x04, 0x05, 0x06, 0x07, 0x14, 0x15, 0x16, 0x17]);
    assert_eq!(stored[0x10..0x18], [0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]);
    assert_eq!(stored[0x18..0x20], [0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f]);
    assert_eq!(stored[0x20..0x28], [0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f]);
    assert_eq!(stored[0x28..0x30], [0xaa; 8]);
    assert_eq!(cpu.state.x(5), DATA + 0x1028);
    // A replicated element fills 64 or 128 bits. 64 bits leave the rest zero.
    assert_eq!(v(&cpu, 20), 0x6060_6060_6060_6060);
    assert_eq!(v(&cpu, 21), u128::from(pattern(0x60, 4)) * 0x1_0000_0001_0000_0001_0000_0001);
    assert_eq!(v(&cpu, 22), u128::from(pattern(0x64, 4)) * 0x1_0000_0001_0000_0001_0000_0001);
    assert_eq!(cpu.state.x(6), DATA + 0x68);
    assert_eq!(v(&cpu, 23), u128::from(pattern(0x80, 8)) * 0x1_0000_0000_0000_0001);
    assert_eq!(v(&cpu, 25), u128::from(pattern(0x90, 8)) * 0x1_0000_0000_0000_0001);
    assert_eq!(v(&cpu, 26), u128::from(pattern(0x80, 8)));
}

#[test]
fn a_prefetch_touches_nothing_also_at_an_address_that_holds_no_memory() {
    let mut cpu = cpu(&[
        word(Record::PRFMui, &[("rt", 0), ("rn", 0)]),
        word(Record::PRFUMi, &[("rt", 1), ("rn", 0), ("offset", 0x1ff)]),
        word(Record::PRFMroX, &[("rt", 2), ("rn", 0), ("rm", 1)]),
        word(Record::PRFMl, &[("rt", 3), ("label", 0x40000)]),
        word(Record::RPRFM, &[("rt", 0x18), ("rn", 0), ("rm", 1)]),
    ]);
    cpu.state.set_x(0, 0x1234);
    cpu.state.set_x(1, u64::MAX);
    let mut before = cpu.state.clone();
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 5);
    before.set_pc(BASE + 20);
    assert_eq!(cpu.state, before);
}

#[test]
fn a_failed_access_stops_before_its_instruction_with_every_register_as_it_was() {
    let program = [
        // add x1, x1, #1
        0x9100_0421,
        // ldr x2, [x0, #8]!: outside the memory.
        word(Record::LDRXpre, &[("rt", 2), ("rn", 0), ("offset", 8)]),
        0x9100_0421,
    ];
    for single in [false, true] {
        let mut cpu = cpu(&program);
        cpu.state.set_x(0, 0x1000);
        cpu.state.set_x(2, 77);
        let stop = if single {
            cpu.step().unwrap();
            cpu.step().unwrap_err()
        } else {
            cpu.run(100)
        };
        assert_eq!(
            stop,
            Stop::DataAbort {
                pc: BASE + 4,
                address: 0x1008,
                write: false
            }
        );
        assert_eq!(cpu.state.pc(), BASE + 4);
        assert_eq!(cpu.instructions, 1);
        assert_eq!(cpu.state.x(1), 1);
        // No write back, and the register of the load keeps its value.
        assert_eq!(cpu.state.x(0), 0x1000);
        assert_eq!(cpu.state.x(2), 77);
    }
    // A pair whose second half is outside the memory: the first half is
    // written, and no register has moved.
    let mut cpu = cpu(&[
        word(
            Record::STPXpost,
            &[("rt", 1), ("rt2", 2), ("rn", 0), ("offset", 2)],
        ),
    ]);
    let end = BASE + SIZE as u64;
    cpu.state.set_x(0, end - 8);
    cpu.state.set_x(1, 0x0102_0304_0506_0708);
    assert_eq!(
        cpu.run(100),
        Stop::DataAbort {
            pc: BASE,
            address: end,
            write: true
        }
    );
    assert_eq!(cpu.state.x(0), end - 8);
    assert_eq!(
        read(&mut cpu, end - 8, 8),
        0x0102_0304_0506_0708u64.to_le_bytes()
    );
    // A load of two registers that fails at the second leaves the first.
    let mut cpu = self::cpu(&[
        word(
            Record::LDPXi,
            &[("rt", 1), ("rt2", 2), ("rn", 0), ("offset", 0)],
        ),
    ]);
    cpu.state.set_x(0, end - 8);
    cpu.state.set_x(1, 5);
    assert!(matches!(cpu.run(100), Stop::DataAbort { write: false, .. }));
    assert_eq!(cpu.state.x(1), 5);
    assert_eq!(cpu.instructions, 0);
}

#[test]
fn a_store_into_translated_code_runs_the_new_code() {
    // The store replaces the third instruction of its own block.
    let mut cpu = cpu(&[
        // str w1, [x0]
        word(Record::STRWui, &[("rt", 1), ("rn", 0)]),
        // add x2, x2, #1
        0x9100_0442,
        // add x3, x3, #1, to be replaced by add x3, x3, #2.
        0x9100_0463,
    ]);
    cpu.state.set_x(0, BASE + 8);
    cpu.state.set_x(1, 0x9100_0863);
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 3);
    assert_eq!((cpu.state.x(2), cpu.state.x(3)), (1, 2));
    // The same through single steps.
    let mut cpu = self::cpu(&[
        word(Record::STRWui, &[("rt", 1), ("rn", 0)]),
        0x9100_0442,
        0x9100_0463,
    ]);
    cpu.state.set_x(0, BASE + 8);
    cpu.state.set_x(1, 0x9100_0863);
    for _ in 0..3 {
        cpu.step().unwrap();
    }
    assert_eq!((cpu.state.x(2), cpu.state.x(3)), (1, 2));
}

#[test]
fn a_store_into_data_keeps_the_translated_blocks() {
    // str x1, [x0], #8 ; subs x1, x1, #1 ; b.ne -8
    let mut cpu = cpu(&[
        word(Record::STRXpost, &[("rt", 1), ("rn", 0), ("offset", 8)]),
        0xf100_0421,
        0x54ff_ffc1,
    ]);
    cpu.state.set_x(0, DATA);
    cpu.state.set_x(1, 100);
    assert!(matches!(cpu.run(u64::MAX), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 300);
    assert_eq!(cpu.translations, 1);
    assert_eq!(read(&mut cpu, DATA + 99 * 8, 8), 1u64.to_le_bytes());
    // A second run finds the block again: our own stores are accounted for.
    cpu.state.set_pc(BASE);
    cpu.state.set_x(0, DATA);
    cpu.state.set_x(1, 10);
    assert!(matches!(cpu.run(u64::MAX), Stop::Unimplemented { .. }));
    assert_eq!(cpu.translations, 1);
    // A write from outside drops it.
    cpu.memory.write(DATA, &[1]).unwrap();
    cpu.state.set_pc(BASE);
    cpu.state.set_x(1, 1);
    assert!(matches!(cpu.run(u64::MAX), Stop::Unimplemented { .. }));
    assert_eq!(cpu.translations, 2);
}

fn ldxr(rt: u32, rn: u32) -> u32 {
    word(Record::LDXRX, &[("rt", rt), ("rn", rn)])
}

fn stxr(ws: u32, rt: u32, rn: u32) -> u32 {
    word(Record::STXRX, &[("ws", ws), ("rt", rt), ("rn", rn)])
}

#[test]
fn an_exclusive_store_writes_once_after_an_exclusive_load_of_the_same_place() {
    let mut cpu = cpu(&[
        // No reservation yet: the store fails and writes nothing.
        stxr(9, 1, 0),
        ldxr(2, 0),
        stxr(10, 1, 0),
        // The reservation is used up.
        stxr(11, 1, 0),
        // Another address than the reserved one.
        ldxr(2, 0),
        stxr(12, 1, 3),
        // That failed store ended the reservation too.
        stxr(13, 1, 0),
        // Another size at the same address.
        ldxr(2, 0),
        word(Record::STXRW, &[("ws", 14), ("rt", 1), ("rn", 0)]),
    ]);
    cpu.state.set_x(0, DATA + 0x100);
    cpu.state.set_x(1, 0x1234_5678_9abc_def0);
    cpu.state.set_x(3, DATA + 0x108);
    for n in 9..=14 {
        cpu.state.set_x(n, u64::MAX);
    }
    cpu.step().unwrap();
    assert_eq!(cpu.state.x(9), 1);
    assert_eq!(read(&mut cpu, DATA + 0x100, 8), pattern(0x100, 8).to_le_bytes());
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 9);
    assert_eq!(cpu.state.x(2), 0x1234_5678_9abc_def0);
    assert_eq!(
        [10, 11, 12, 13, 14].map(|n| cpu.state.x(n)),
        [0, 1, 1, 1, 1]
    );
    assert_eq!(
        read(&mut cpu, DATA + 0x100, 16),
        [
            0x1234_5678_9abc_def0u64.to_le_bytes(),
            pattern(0x108, 8).to_le_bytes()
        ]
        .concat()
    );
}

#[test]
fn a_store_of_the_same_cpu_keeps_the_reservation_unless_it_changes_the_value() {
    let mut cpu = cpu(&[
        ldxr(2, 0),
        // str x1, [x0, #8]: the same page, another place.
        word(Record::STRXui, &[("rt", 1), ("rn", 0), ("offset", 1)]),
        stxr(10, 1, 0),
        ldxr(2, 0),
        // str x4, [x0]: the reserved place takes another value.
        word(Record::STRXui, &[("rt", 4), ("rn", 0)]),
        stxr(11, 1, 0),
    ]);
    cpu.state.set_x(0, DATA + 0x100);
    cpu.state.set_x(1, 7);
    cpu.state.set_x(4, 9);
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!((cpu.state.x(10), cpu.state.x(11)), (0, 1));
    assert_eq!(read(&mut cpu, DATA + 0x100, 8), 9u64.to_le_bytes());
    // `clear_exclusive` ends a reservation, as CLREX will.
    let mut cpu = self::cpu(&[ldxr(2, 0), stxr(10, 1, 0)]);
    cpu.state.set_x(0, DATA + 0x100);
    cpu.step().unwrap();
    cpu.clear_exclusive();
    cpu.step().unwrap();
    assert_eq!(cpu.state.x(10), 1);
}

#[test]
fn exclusive_pairs_and_the_smaller_sizes_move_what_their_names_say() {
    let mut cpu = cpu(&[
        // ldxp x1, x2, [x0]
        word(Record::LDXPX, &[("rt", 1), ("rt2", 2), ("rn", 0)]),
        // stxp w9, x3, x4, [x0]
        word(
            Record::STXPX,
            &[("ws", 9), ("rt", 3), ("rt2", 4), ("rn", 0)],
        ),
        // ldaxp w5, w6, [x0]
        word(Record::LDAXPW, &[("rt", 5), ("rt2", 6), ("rn", 0)]),
        // stlxp w10, w6, w5, [x0]
        word(
            Record::STLXPW,
            &[("ws", 10), ("rt", 6), ("rt2", 5), ("rn", 0)],
        ),
        // ldxrb w7, [x0] ; stlxrh w11, w7, [x0]: another size, so it fails.
        word(Record::LDXRB, &[("rt", 7), ("rn", 0)]),
        word(Record::STLXRH, &[("ws", 11), ("rt", 7), ("rn", 0)]),
        // ldaxrh w8, [x0] ; stxrh w12, w3, [x0]
        word(Record::LDAXRH, &[("rt", 8), ("rn", 0)]),
        word(Record::STXRH, &[("ws", 12), ("rt", 3), ("rn", 0)]),
        // ldarb w13, [x0, #1 through x14] ; stlr x3, [x0]
        word(Record::LDARB, &[("rt", 13), ("rn", 14)]),
        word(Record::STLRX, &[("rt", 3), ("rn", 0)]),
    ]);
    cpu.state.set_x(0, DATA + 0x100);
    cpu.state.set_x(3, 0x3333_3333_3333_3333);
    cpu.state.set_x(4, 0x4444_4444_4444_4444);
    cpu.state.set_x(14, DATA + 0x101);
    assert!(matches!(cpu.run(100), Stop::Unimplemented { .. }));
    assert_eq!(cpu.instructions, 10);
    assert_eq!(cpu.state.x(1), pattern(0x100, 8));
    assert_eq!(cpu.state.x(2), pattern(0x108, 8));
    assert_eq!(cpu.state.x(9), 0);
    assert_eq!((cpu.state.x(5), cpu.state.x(6)), (0x3333_3333, 0x3333_3333));
    assert_eq!(cpu.state.x(10), 0);
    assert_eq!(cpu.state.x(7), 0x33);
    assert_eq!(cpu.state.x(11), 1);
    assert_eq!(cpu.state.x(8), 0x3333);
    assert_eq!(cpu.state.x(12), 0);
    assert_eq!(cpu.state.x(13), 0x33);
    assert_eq!(
        read(&mut cpu, DATA + 0x100, 16),
        [0x3333_3333_3333_3333u64, 0x4444_4444_4444_4444]
            .map(u64::to_le_bytes)
            .concat()
    );
}

#[test]
fn an_exclusive_access_asks_for_an_address_that_is_a_multiple_of_its_size() {
    let mut cpu = cpu(&[0x9100_0421, ldxr(2, 0)]);
    cpu.state.set_x(0, DATA + 0x101);
    cpu.state.set_x(2, 5);
    assert_eq!(
        cpu.run(100),
        Stop::Alignment {
            pc: BASE + 4,
            address: DATA + 0x101
        }
    );
    assert_eq!(cpu.instructions, 1);
    assert_eq!((cpu.state.x(1), cpu.state.x(2)), (1, 5));
    // A pair of two 8-byte registers is one access of 16 bytes.
    let mut cpu = self::cpu(&[word(Record::LDXPX, &[("rt", 1), ("rt2", 2), ("rn", 0)])]);
    cpu.state.set_x(0, DATA + 0x108);
    assert!(matches!(cpu.step(), Err(Stop::Alignment { .. })));
    // One byte has no alignment.
    let mut cpu = self::cpu(&[word(Record::LDXRB, &[("rt", 1), ("rn", 0)])]);
    cpu.state.set_x(0, DATA + 0x101);
    cpu.step().unwrap();
    assert_eq!(cpu.state.x(1), 0x01);
}

#[test]
fn the_accesses_of_an_instruction_are_kept_for_a_harness_that_asks() {
    let mut cpu = cpu(&[
        word(
            Record::STPXi,
            &[("rt", 1), ("rt2", 2), ("rn", 0), ("offset", 0)],
        ),
        word(Record::LDRBBui, &[("rt", 3), ("rn", 0)]),
    ]);
    cpu.state.set_x(0, DATA);
    cpu.state.set_x(1, 0x11);
    cpu.state.set_x(2, 0x22);
    assert!(cpu.take_accesses().is_empty());
    cpu.log_accesses(true);
    cpu.step().unwrap();
    let accesses = cpu.take_accesses();
    assert_eq!(accesses.len(), 2);
    assert_eq!(
        (accesses[0].address, accesses[0].bytes, accesses[0].value, accesses[0].write),
        (DATA, 8, 0x11, true)
    );
    assert_eq!((accesses[1].address, accesses[1].value), (DATA + 8, 0x22));
    cpu.step().unwrap();
    let accesses = cpu.take_accesses();
    assert_eq!(accesses.len(), 1);
    assert_eq!(
        (accesses[0].address, accesses[0].bytes, accesses[0].value, accesses[0].write),
        (DATA, 1, 0x11, false)
    );
}

/// The loop of the two-CPU tests: add one to the counter at X0, X3 times.
///
/// ```text
/// 0:  ldxr x1, [x0]
/// 4:  add x1, x1, #1
/// 8:  stxr w2, x1, [x0]
/// c:  cbnz w2, 0
/// 10: subs x3, x3, #1
/// 14: b.ne 0
/// ```
fn counter_loop() -> Vec<u32> {
    vec![
        ldxr(1, 0),
        0x9100_0421,
        stxr(2, 1, 0),
        word(Record::CBNZW, &[("rt", 2), ("target", 0x7fffd)]),
        0xf100_0463,
        word(Record::Bcc, &[("cond", 1), ("target", 0x7fffb)]),
    ]
}

fn shared(words: &[u32]) -> (Cpu<SharedMemory>, Cpu<SharedMemory>) {
    let memory = SharedMemory::new(FlatMemory::new(BASE, image(words)));
    let make = |memory: SharedMemory| {
        let mut cpu = Cpu::new(memory, State::reset(BASE));
        cpu.state.set_x(0, DATA + 0x1000);
        cpu
    };
    (make(memory.clone()), make(memory))
}

#[test]
fn a_write_of_another_cpu_between_the_load_and_the_store_fails_the_store() {
    let (mut first, mut second) = shared(&counter_loop());
    second.memory.write(DATA + 0x1000, &0u64.to_le_bytes()).unwrap();
    first.state.set_x(3, 1);
    second.state.set_x(3, 1);
    // Both load 0 and reserve.
    first.step().unwrap();
    second.step().unwrap();
    first.step().unwrap();
    second.step().unwrap();
    // The first store wins, and the second fails.
    first.step().unwrap();
    second.step().unwrap();
    assert_eq!((first.state.x(2), second.state.x(2)), (0, 1));
    assert_eq!(read(&mut first, DATA + 0x1000, 8), 1u64.to_le_bytes());
    // The second goes around its loop and counts on top of the first.
    assert!(matches!(second.run(100), Stop::Unimplemented { .. }));
    assert_eq!(read(&mut first, DATA + 0x1000, 8), 2u64.to_le_bytes());
}

#[test]
fn two_threads_count_with_exclusives_and_lose_no_increment() {
    const TURNS: u64 = 20_000;
    let (mut first, mut second) = shared(&counter_loop());
    first.memory.write(DATA + 0x1000, &0u64.to_le_bytes()).unwrap();
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = [&mut first, &mut second]
        .into_iter()
        .map(|cpu| {
            cpu.state.set_x(3, TURNS);
            let mut cpu = Cpu::new(cpu.memory.clone(), cpu.state.clone());
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                let stop = cpu.run(u64::MAX);
                (stop, cpu.state.x(3), cpu.instructions)
            })
        })
        .collect();
    let mut instructions = 0;
    for thread in threads {
        let (stop, left, count) = thread.join().unwrap();
        assert!(matches!(stop, Stop::Unimplemented { pc, .. } if pc == BASE + 24), "{stop:?}");
        assert_eq!(left, 0);
        instructions += count;
    }
    assert_eq!(
        read(&mut first, DATA + 0x1000, 8),
        (2 * TURNS).to_le_bytes()
    );
    // Six instructions for a turn that stored, four more for each retry.
    assert!(instructions >= 2 * TURNS * 6);
    assert_eq!((instructions - 2 * TURNS * 6) % 4, 0);
}
