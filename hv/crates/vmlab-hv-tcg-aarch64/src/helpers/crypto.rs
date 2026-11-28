//! The crypto instructions of the 3d set (card C7): one round step of AES,
//! and the four-round and schedule steps of SHA-1 and SHA-256, each a pure
//! function of up to three 128-bit values. Each operation is a helper of
//! its own (3e DESIGN.md J13), which compiled code calls with the six words
//! of its three values in the argument registers.
//!
//! The byte `i` of a vector is the byte `i` of the AES state, which runs down
//! the columns. The lane `i` of 32 bits is the word `i` of a SHA value.
//!
//! Sources read (DESIGN.md "Sources"): the published algorithms, FIPS 197
//! and FIPS 180-4, and the published definitions of the instructions, from
//! the author's memory. The tests run the known answers of both standards
//! through these functions, as a guest computes them. S6, the lockstep
//! against QEMU. No file of S2, S3 or S4 was opened for this file. Not read:
//! QEMU's source, Arm's ASL or pseudocode (X1, X2).

use vmlab_hv_tcg_ir::native::{Effects, Helper, HelperFn};
use vmlab_hv_tcg_jit::{RegisterFn, Words};

/// One crypto instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Crypto {
    AesE,
    AesD,
    AesMc,
    AesImc,
    Sha1C,
    Sha1P,
    Sha1M,
    Sha1H,
    Sha1Su0,
    Sha1Su1,
    Sha256H,
    Sha256H2,
    Sha256Su0,
    Sha256Su1,
}

impl Crypto {
    pub const ALL: [Self; 14] = [
        Self::AesE,
        Self::AesD,
        Self::AesMc,
        Self::AesImc,
        Self::Sha1C,
        Self::Sha1P,
        Self::Sha1M,
        Self::Sha1H,
        Self::Sha1Su0,
        Self::Sha1Su1,
        Self::Sha256H,
        Self::Sha256H2,
        Self::Sha256Su0,
        Self::Sha256Su1,
    ];
}

/// Multiply by x in GF(2^8) modulo x^8 + x^4 + x^3 + x + 1.
const fn double(value: u8) -> u8 {
    (value << 1) ^ if value & 0x80 != 0 { 0x1b } else { 0 }
}

const fn multiply(a: u8, b: u8) -> u8 {
    let mut product = 0;
    let mut a = a;
    let mut bit = 0;
    while bit < 8 {
        if b >> bit & 1 != 0 {
            product ^= a;
        }
        a = double(a);
        bit += 1;
    }
    product
}

/// The S-box: the inverse in the field, then the affine map of FIPS 197.
const fn s_box() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut value = 0usize;
    while value < 256 {
        // The inverse is the 254th power. Zero stays zero.
        let mut inverse = 1u8;
        let mut count = 0;
        while count < 254 {
            inverse = multiply(inverse, value as u8);
            count += 1;
        }
        let x = inverse;
        table[value] =
            x ^ x.rotate_left(1) ^ x.rotate_left(2) ^ x.rotate_left(3) ^ x.rotate_left(4) ^ 0x63;
        value += 1;
    }
    table
}

const fn inverse_of(table: &[u8; 256]) -> [u8; 256] {
    let mut inverse = [0u8; 256];
    let mut value = 0usize;
    while value < 256 {
        inverse[table[value] as usize] = value as u8;
        value += 1;
    }
    inverse
}

static S_BOX: [u8; 256] = s_box();
static INVERSE_S_BOX: [u8; 256] = inverse_of(&s_box());

/// `AESE` and `AESD`: add the round key, move the rows, substitute.
fn aes_round(state: u128, key: u128, decrypt: bool) -> u128 {
    let input = (state ^ key).to_le_bytes();
    let mut output = [0u8; 16];
    for column in 0..4 {
        for row in 0..4 {
            // Row `row` moves left by `row` columns, or right to decrypt.
            let from = if decrypt {
                (column + 4 - row) % 4
            } else {
                (column + row) % 4
            };
            let byte = input[row + 4 * from];
            output[row + 4 * column] = if decrypt {
                INVERSE_S_BOX[usize::from(byte)]
            } else {
                S_BOX[usize::from(byte)]
            };
        }
    }
    u128::from_le_bytes(output)
}

/// `AESMC` and `AESIMC`: each column times the matrix of the standard.
fn aes_mix(state: u128, inverse: bool) -> u128 {
    let input = state.to_le_bytes();
    let factors: [u8; 4] = if inverse {
        [0x0e, 0x0b, 0x0d, 0x09]
    } else {
        [2, 3, 1, 1]
    };
    let mut output = [0u8; 16];
    for column in 0..4 {
        for row in 0..4 {
            let mut value = 0;
            for term in 0..4 {
                value ^= multiply(input[4 * column + (row + term) % 4], factors[term]);
            }
            output[4 * column + row] = value;
        }
    }
    u128::from_le_bytes(output)
}

fn words(value: u128) -> [u32; 4] {
    [
        value as u32,
        (value >> 32) as u32,
        (value >> 64) as u32,
        (value >> 96) as u32,
    ]
}

fn join(words: [u32; 4]) -> u128 {
    u128::from(words[0])
        | u128::from(words[1]) << 32
        | u128::from(words[2]) << 64
        | u128::from(words[3]) << 96
}

/// `SHA1C`, `SHA1P` and `SHA1M`: four rounds on `a`, `b`, `c` and `d` in
/// `state` with `e`, where the words of `schedule` have the round constant
/// added. Gives the new `a` to `d`.
fn sha1_rounds(state: u128, e: u32, schedule: u128, function: Crypto) -> u128 {
    let mut x = words(state);
    let mut y = e;
    for word in words(schedule) {
        let t = match function {
            Crypto::Sha1C => (x[1] & x[2]) | (!x[1] & x[3]),
            Crypto::Sha1M => (x[1] & x[2]) | (x[1] & x[3]) | (x[2] & x[3]),
            _ => x[1] ^ x[2] ^ x[3],
        };
        let a = y
            .wrapping_add(x[0].rotate_left(5))
            .wrapping_add(t)
            .wrapping_add(word);
        y = x[3];
        x = [a, x[0], x[1].rotate_left(30), x[2]];
    }
    join(x)
}

/// `SHA256H` and `SHA256H2`: four rounds on `a` to `d` in `x` and `e` to `h`
/// in `y`. Gives both halves after them.
fn sha256_rounds(x: u128, y: u128, schedule: u128) -> (u128, u128) {
    let mut x = words(x);
    let mut y = words(y);
    for word in words(schedule) {
        let choose = (y[0] & y[1]) | (!y[0] & y[2]);
        let majority = (x[0] & x[1]) | (x[0] & x[2]) | (x[1] & x[2]);
        let sigma1 = y[0].rotate_right(6) ^ y[0].rotate_right(11) ^ y[0].rotate_right(25);
        let sigma0 = x[0].rotate_right(2) ^ x[0].rotate_right(13) ^ x[0].rotate_right(22);
        let t = y[3]
            .wrapping_add(sigma1)
            .wrapping_add(choose)
            .wrapping_add(word);
        let e = x[3].wrapping_add(t);
        let a = t.wrapping_add(sigma0).wrapping_add(majority);
        x = [a, x[0], x[1], x[2]];
        y = [e, y[0], y[1], y[2]];
    }
    (join(x), join(y))
}

/// Run `operation` on the registers `d`, `n` and `m` of its instruction.
pub fn crypto(operation: Crypto, d: u128, n: u128, m: u128) -> u128 {
    match operation {
        Crypto::AesE => aes_round(d, n, false),
        Crypto::AesD => aes_round(d, n, true),
        Crypto::AesMc => aes_mix(n, false),
        Crypto::AesImc => aes_mix(n, true),
        Crypto::Sha1C | Crypto::Sha1P | Crypto::Sha1M => sha1_rounds(d, n as u32, m, operation),
        Crypto::Sha1H => u128::from((n as u32).rotate_left(30)),
        Crypto::Sha1Su0 => {
            // The words 2 and 3 of `d` and 0 and 1 of `n`.
            let shifted = (d >> 64) | (n << 64);
            shifted ^ d ^ m
        }
        Crypto::Sha1Su1 => {
            let t = words(d ^ (n >> 32));
            join([
                t[0].rotate_left(1),
                t[1].rotate_left(1),
                t[2].rotate_left(1),
                t[3].rotate_left(1) ^ t[0].rotate_left(2),
            ])
        }
        Crypto::Sha256H => sha256_rounds(d, n, m).0,
        Crypto::Sha256H2 => sha256_rounds(n, d, m).1,
        Crypto::Sha256Su0 => {
            let (d, n) = (words(d), words(n));
            let next = [d[1], d[2], d[3], n[0]];
            let mut result = [0; 4];
            for index in 0..4 {
                let w = next[index];
                let sigma = w.rotate_right(7) ^ w.rotate_right(18) ^ (w >> 3);
                result[index] = d[index].wrapping_add(sigma);
            }
            join(result)
        }
        Crypto::Sha256Su1 => {
            let (d, n, m) = (words(d), words(n), words(m));
            let sigma = |w: u32| w.rotate_right(17) ^ w.rotate_right(19) ^ (w >> 10);
            let seventh = [n[1], n[2], n[3], m[0]];
            let mut result = [0u32; 4];
            for index in 0..4 {
                // The word two places back: of `m`, then of this result.
                let second = if index < 2 {
                    m[index + 2]
                } else {
                    result[index - 2]
                };
                result[index] = d[index]
                    .wrapping_add(seventh[index])
                    .wrapping_add(sigma(second));
            }
            join(result)
        }
    }
}

/// The pure function of the helper of `crypto::<OP>`: `(d, n, m)`.
fn pure<const OP: u8>(args: &[u128]) -> u128 {
    crypto(Crypto::ALL[usize::from(OP)], args[0], args[1], args[2])
}

/// The same with the words of `d`, `n` and `m` in the argument registers,
/// low word first, and the result in RAX and RDX: what compiled code calls
/// (3e DESIGN.md J13).
extern "sysv64" fn in_registers<const OP: u8>(
    d_low: u64,
    d_high: u64,
    n_low: u64,
    n_high: u64,
    m_low: u64,
    m_high: u64,
) -> Words {
    let value = |low: u64, high: u64| u128::from(low) | u128::from(high) << 64;
    let result = crypto(
        Crypto::ALL[usize::from(OP)],
        value(d_low, d_high),
        value(n_low, n_high),
        value(m_low, m_high),
    );
    Words {
        low: result as u64,
        high: (result >> 64) as u64,
    }
}

const fn helper(name: &'static str, function: fn(&[u128]) -> u128) -> Helper {
    Helper {
        name,
        args: 3,
        results: 1,
        effects: Effects::NONE,
        function: HelperFn::Pure(function),
    }
}

/// One helper for each operation, in the order of `Crypto::ALL`: `(d, n,
/// m)`, the registers of the instruction, with no operation to dispatch on
/// (3e DESIGN.md J13).
pub static HELPERS: [Helper; 14] = [
    helper("aese", pure::<0>),
    helper("aesd", pure::<1>),
    helper("aesmc", pure::<2>),
    helper("aesimc", pure::<3>),
    helper("sha1c", pure::<4>),
    helper("sha1p", pure::<5>),
    helper("sha1m", pure::<6>),
    helper("sha1h", pure::<7>),
    helper("sha1su0", pure::<8>),
    helper("sha1su1", pure::<9>),
    helper("sha256h", pure::<10>),
    helper("sha256h2", pure::<11>),
    helper("sha256su0", pure::<12>),
    helper("sha256su1", pure::<13>),
];

/// The functions with their words in registers, as `HELPERS`.
static IN_REGISTERS: [RegisterFn; 14] = [
    in_registers::<0>,
    in_registers::<1>,
    in_registers::<2>,
    in_registers::<3>,
    in_registers::<4>,
    in_registers::<5>,
    in_registers::<6>,
    in_registers::<7>,
    in_registers::<8>,
    in_registers::<9>,
    in_registers::<10>,
    in_registers::<11>,
    in_registers::<12>,
    in_registers::<13>,
];

/// The helper of `operation`.
pub fn helper_of(operation: Crypto) -> &'static Helper {
    &HELPERS[operation as usize]
}

/// The function that compiled code calls with the words of `helper` in
/// registers, when it is one of `HELPERS`.
pub fn in_registers_of(helper: &Helper) -> Option<RegisterFn> {
    HELPERS
        .iter()
        .position(|own| std::ptr::eq(own, helper))
        .map(|index| IN_REGISTERS[index])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(hex: &str) -> u128 {
        let mut bytes = [0u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * index..2 * index + 2], 16).unwrap();
        }
        u128::from_le_bytes(bytes)
    }

    #[test]
    fn the_first_round_of_the_example_of_fips_197() {
        // Appendix B: the cipher key and the input, and the state after
        // SubBytes and ShiftRows, after MixColumns, and at the start of the
        // second round.
        let key = block("2b7e151628aed2a6abf7158809cf4f3c");
        let input = block("3243f6a8885a308d313198a2e0370734");
        let moved = block("d4bf5d30e0b452aeb84111f11e2798e5");
        let mixed = block("046681e5e0cb199a48f8d37a2806264c");
        let round_key = block("a0fafe1788542cb123a339392a6c7605");
        assert_eq!(S_BOX[0], 0x63);
        assert_eq!(S_BOX[0x53], 0xed);
        assert_eq!(crypto(Crypto::AesE, input, key, 0), moved);
        assert_eq!(crypto(Crypto::AesMc, 0, moved, 0), mixed);
        assert_eq!(mixed ^ round_key, block("a49c7ff2689f352b6b5bea43026a5049"));
        // The inverses undo both steps.
        assert_eq!(crypto(Crypto::AesD, moved, 0, 0), input ^ key);
        assert_eq!(crypto(Crypto::AesImc, 0, mixed, 0), moved);
    }

    /// The largest `r` with `r^power <= value`.
    fn root(value: u128, power: u32) -> u128 {
        let (mut low, mut high) = (0u128, 1u128 << 40);
        while low + 1 < high {
            let middle = (low + high) / 2;
            if middle.pow(power) <= value {
                low = middle;
            } else {
                high = middle;
            }
        }
        low
    }

    fn primes(count: usize) -> Vec<u128> {
        (2u128..)
            .filter(|n| (2..*n).all(|d| n % d != 0))
            .take(count)
            .collect()
    }

    /// One block: the message, the bit one, zeros and the length in bits,
    /// as 16 big-endian words in four vectors.
    fn padded(message: &[u8]) -> [u128; 4] {
        let mut bytes = [0u8; 64];
        bytes[..message.len()].copy_from_slice(message);
        bytes[message.len()] = 0x80;
        bytes[56..].copy_from_slice(&(message.len() as u64 * 8).to_be_bytes());
        let mut vectors = [0u128; 4];
        for (index, chunk) in bytes.chunks(4).enumerate() {
            let word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            vectors[index / 4] |= u128::from(word) << (32 * (index % 4));
        }
        vectors
    }

    fn add(a: u128, b: u128) -> u128 {
        let (a, b) = (words(a), words(b));
        join([
            a[0].wrapping_add(b[0]),
            a[1].wrapping_add(b[1]),
            a[2].wrapping_add(b[2]),
            a[3].wrapping_add(b[3]),
        ])
    }

    #[test]
    fn sha256_of_abc_as_a_guest_computes_it() {
        // The constants are the fractions of the cube roots and of the
        // square roots of the first primes.
        let primes = primes(64);
        let k: Vec<u32> = primes.iter().map(|p| root(p << 96, 3) as u32).collect();
        let h: Vec<u32> = primes[..8]
            .iter()
            .map(|p| root(p << 64, 2) as u32)
            .collect();
        assert_eq!(k[0], 0x428a_2f98);
        assert_eq!(h[0], 0x6a09_e667);
        let mut w = padded(b"abc");
        let start = (
            join([h[0], h[1], h[2], h[3]]),
            join([h[4], h[5], h[6], h[7]]),
        );
        let (mut abcd, mut efgh) = start;
        for group in 0..16 {
            let constants = join([
                k[4 * group],
                k[4 * group + 1],
                k[4 * group + 2],
                k[4 * group + 3],
            ]);
            let schedule = add(w[0], constants);
            let before = abcd;
            abcd = crypto(Crypto::Sha256H, abcd, efgh, schedule);
            efgh = crypto(Crypto::Sha256H2, efgh, before, schedule);
            let partial = crypto(Crypto::Sha256Su0, w[0], w[1], 0);
            let next = crypto(Crypto::Sha256Su1, partial, w[2], w[3]);
            w = [w[1], w[2], w[3], next];
        }
        let digest = [words(add(abcd, start.0)), words(add(efgh, start.1))].concat();
        assert_eq!(
            digest,
            [
                0xba78_16bf,
                0x8f01_cfea,
                0x4141_40de,
                0x5dae_2223,
                0xb003_61a3,
                0x9617_7a9c,
                0xb410_ff61,
                0xf200_15ad
            ]
        );
    }

    #[test]
    fn sha1_of_abc_as_a_guest_computes_it() {
        let h = [
            0x6745_2301u32,
            0xefcd_ab89,
            0x98ba_dcfe,
            0x1032_5476,
            0xc3d2_e1f0,
        ];
        let k = [0x5a82_7999u32, 0x6ed9_eba1, 0x8f1b_bcdc, 0xca62_c1d6];
        let mut w = padded(b"abc");
        let mut abcd = join([h[0], h[1], h[2], h[3]]);
        let mut e = h[4];
        for group in 0..20 {
            let constant = k[group / 5];
            let schedule = add(w[0], join([constant; 4]));
            let function = match group / 5 {
                0 => Crypto::Sha1C,
                2 => Crypto::Sha1M,
                _ => Crypto::Sha1P,
            };
            let next_e = crypto(Crypto::Sha1H, 0, abcd, 0) as u32;
            abcd = crypto(function, abcd, u128::from(e), schedule);
            e = next_e;
            let partial = crypto(Crypto::Sha1Su0, w[0], w[1], w[2]);
            let next = crypto(Crypto::Sha1Su1, partial, w[3], 0);
            w = [w[1], w[2], w[3], next];
        }
        let state = words(abcd);
        let digest = [
            state[0].wrapping_add(h[0]),
            state[1].wrapping_add(h[1]),
            state[2].wrapping_add(h[2]),
            state[3].wrapping_add(h[3]),
            e.wrapping_add(h[4]),
        ];
        assert_eq!(
            digest,
            [
                0xa999_3e36,
                0x4706_816a,
                0xba3e_2571,
                0x7850_c26c,
                0x9cd0_d89d
            ]
        );
    }
}
