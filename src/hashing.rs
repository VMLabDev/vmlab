//! Streaming digest helper shared by the template store, the media cache and
//! the OCI paths.
//!
//! RustCrypto 0.11 dropped the `std::io::Write` impl on digests, so the
//! `std::io::copy(&mut reader, &mut hasher)` idiom no longer compiles. This is
//! the replacement: same streaming behaviour, no whole-file buffering.

use std::io::{self, Read};

use sha2::Digest;

/// Read buffer size. Matches what `std::io::copy` used to pick for us.
const BUF: usize = 64 * 1024;

/// Feeds every byte of `reader` into `hasher`, stopping at EOF.
pub fn feed<R: Read, D: Digest>(mut reader: R, hasher: &mut D) -> io::Result<()> {
    let mut buf = vec![0u8; BUF];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Sha256;

    /// The content-addressed caches (media, artefacts, OCI blobs) key off
    /// these digests, so pin one against an independently computed value —
    /// and make it larger than `BUF` so the multi-read path is covered.
    #[test]
    fn multi_chunk_digest_matches_a_known_value() {
        let data: Vec<u8> = (0..200_000u32).map(|i| ((i * 7 + 3) % 256) as u8).collect();
        let mut hasher = Sha256::new();
        feed(&data[..], &mut hasher).expect("hashing a slice cannot fail");
        assert_eq!(
            hex::encode(hasher.finalize()),
            "ec0ebf98b6f2954bf0f7b839402b1ba245996c39d18e155414e91a2b4353c157"
        );
    }

    /// A reader that hands back one byte at a time — `Read` is free to return
    /// short reads, and the loop must not stop on the first one.
    #[test]
    fn short_reads_hash_the_same_as_one_shot() {
        struct OneByteAtATime<'a>(&'a [u8]);
        impl Read for OneByteAtATime<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                match self.0.split_first() {
                    Some((b, rest)) if !out.is_empty() => {
                        out[0] = *b;
                        self.0 = rest;
                        Ok(1)
                    }
                    _ => Ok(0),
                }
            }
        }

        let data = b"the quick brown fox";
        let mut dribbled = Sha256::new();
        feed(OneByteAtATime(data), &mut dribbled).expect("hashing cannot fail");
        let mut whole = Sha256::new();
        feed(&data[..], &mut whole).expect("hashing cannot fail");
        assert_eq!(dribbled.finalize(), whole.finalize());
    }
}
