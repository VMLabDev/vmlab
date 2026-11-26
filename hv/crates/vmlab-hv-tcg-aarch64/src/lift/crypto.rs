//! The crypto instructions of the 3d set (card C7): the seven format classes
//! of the `AES` and `SHA2` records that are not `PMULL` (14 records). Each
//! is one call of the pure helper `helpers::crypto::CRYPTO` on the registers
//! of the instruction (DESIGN.md G3d).
//!
//! Sources read (DESIGN.md "Sources"): S1, the generated tables of the LLVM
//! AArch64 records. S6, the lockstep against QEMU, which checks which
//! register holds what. No file of S2, S3 or S4 was opened for this file.
//! Not read: QEMU's source, Arm's ASL or pseudocode (X1, X2).

use super::{Builder, Lifted};
use crate::generated::decode::Record;
use crate::helpers::crypto::{CRYPTO, Crypto};

/// The operation of a record, and whether it reads `Rd` and `Rm`.
pub fn plan(record: Record) -> Option<(Crypto, bool, bool)> {
    use Record::*;
    Some(match record {
        AESErr => (Crypto::AesE, true, false),
        AESDrr => (Crypto::AesD, true, false),
        AESMCrr => (Crypto::AesMc, false, false),
        AESIMCrr => (Crypto::AesImc, false, false),
        SHA1Crrr => (Crypto::Sha1C, true, true),
        SHA1Prrr => (Crypto::Sha1P, true, true),
        SHA1Mrrr => (Crypto::Sha1M, true, true),
        SHA1Hrr => (Crypto::Sha1H, false, false),
        SHA1SU0rrr => (Crypto::Sha1Su0, true, true),
        SHA1SU1rr => (Crypto::Sha1Su1, true, false),
        SHA256Hrrr => (Crypto::Sha256H, true, true),
        SHA256H2rrr => (Crypto::Sha256H2, true, true),
        SHA256SU0rr => (Crypto::Sha256Su0, true, false),
        SHA256SU1rrr => (Crypto::Sha256Su1, true, true),
        _ => return None,
    })
}

/// Lift the crypto instruction `word` of `record`.
pub fn lift(b: &mut Builder, record: Record, word: u32) -> Lifted {
    let Some((operation, reads_d, reads_m)) = plan(record) else {
        return Lifted::Unsupported;
    };
    let (rd, rn, rm) = (word & 31, (word >> 5) & 31, (word >> 16) & 31);
    let code = b.imm(32, operation as u64);
    let d = if reads_d { b.v(rd, 128) } else { b.imm(128, 0) };
    let n = b.v(rn, 128);
    let m = if reads_m { b.v(rm, 128) } else { b.imm(128, 0) };
    let result = b.call(128, &CRYPTO, &[code, d, n, m]);
    b.set_v(rd, 128, result);
    Lifted::Next
}
