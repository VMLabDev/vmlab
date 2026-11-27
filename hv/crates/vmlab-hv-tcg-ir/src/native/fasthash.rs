//! A hasher for the tables of a translation: the interned scalars of a
//! graph and the pools of its passes. Their keys are small integers and
//! operations made of them, which nobody outside the emulator chooses, so
//! the hash needs no protection against chosen collisions. SipHash, the
//! default of the standard library, was a sixth of the time of a
//! translation on a guest boot (card C13).
//!
//! The mixing is the multiply-and-rotate of the Firefox hash, written here
//! from its description: each word is added in, rotated, and multiplied by
//! an odd constant.

use std::hash::{BuildHasherDefault, Hasher};

/// A `HashMap` with [`FastHasher`].
pub type FastMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// A `HashSet` with [`FastHasher`].
pub type FastSet<K> = std::collections::HashSet<K, BuildHasherDefault<FastHasher>>;

/// The multiplier: an odd constant with its bits spread, the fractional part
/// of the golden ratio.
const SEED: u64 = 0x9e37_79b9_7f4a_7c15;

#[derive(Clone, Copy, Debug, Default)]
pub struct FastHasher {
    hash: u64,
}

impl FastHasher {
    #[inline(always)]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let mut word = [0u8; 8];
            word.copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word) ^ (rest.len() as u64) << 59);
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u16(&mut self, value: u16) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_u128(&mut self, value: u128) {
        self.add(value as u64);
        self.add((value >> 64) as u64);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // The low bits pick the bucket: fold the high bits, which the
        // multiplication fills best, into them.
        self.hash ^ (self.hash >> 32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_keys_hash_equal_and_near_keys_spread() {
        let mut map: FastMap<(u16, u128), usize> = FastMap::default();
        for i in 0..10_000u16 {
            map.insert((i, u128::from(i) << 64), usize::from(i));
        }
        for i in 0..10_000u16 {
            assert_eq!(map.get(&(i, u128::from(i) << 64)), Some(&usize::from(i)));
        }
        let mut buckets = [0usize; 64];
        for i in 0..64_000u64 {
            let mut hasher = FastHasher::default();
            hasher.write_u64(i);
            buckets[(hasher.finish() & 63) as usize] += 1;
        }
        assert!(buckets.iter().all(|&count| (500..1500).contains(&count)), "{buckets:?}");
    }
}
