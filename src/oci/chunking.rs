//! Chunking and zstd compression of qcow2 disk images for OCI transport
//! (PRD §6.4, §16 decision #8).
//!
//! The qcow2 is split into fixed-size chunks (default **512 MiB**,
//! configurable) — each compressed with zstd and pushed as one ordered
//! layer blob. The manifest annotations record chunk count, chunk size,
//! total size and the digest of the *assembled* (uncompressed) image; pull
//! reassembles in order and verifies that whole-image digest before
//! installing.
//!
//! Everything here streams: a chunk's bytes pass disk → hasher → zstd →
//! disk in bounded buffers, so a 64 GiB image never lands in memory.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

/// Default chunk size: 512 MiB (PRD §6.4 — clears GHCR's 10-minute
/// per-upload timeout with wide margin while keeping retry/resume cheap).
pub const DEFAULT_CHUNK_SIZE: u64 = 512 * 1024 * 1024;

/// zstd compression level for chunk blobs. Level 3 is zstd's default —
/// a good speed/ratio balance for large, already-sparse qcow2 data.
pub const ZSTD_LEVEL: i32 = 3;

/// I/O copy buffer.
const COPY_BUF: usize = 1 << 20; // 1 MiB

/// One compressed chunk and the digests/sizes needed to push it as a layer
/// and to reassemble the image on pull.
#[derive(Debug, Clone)]
pub struct ChunkInfo {
    /// Zero-based ordinal — layers are pushed/pulled in this order.
    pub index: u32,
    /// On-disk path of the compressed `chunk-NNNN.zst` blob.
    pub compressed_path: PathBuf,
    /// `sha256:hex` digest of the COMPRESSED blob (the layer digest).
    pub compressed_digest: String,
    /// Size of the compressed blob in bytes.
    pub compressed_size: u64,
    /// Size of the uncompressed slice of the image this chunk covers.
    pub uncompressed_size: u64,
}

/// The full result of [`chunk_and_compress`].
#[derive(Debug, Clone)]
pub struct ChunkSet {
    pub chunks: Vec<ChunkInfo>,
    /// `sha256:hex` of the ASSEMBLED uncompressed image — what pull
    /// verifies after reassembly, recorded in a manifest annotation.
    pub whole_digest: String,
    /// Total uncompressed image size in bytes.
    pub total_size: u64,
    /// Chunk size used (the boundary granularity, not the last chunk's size).
    pub chunk_size: u64,
    /// Number of chunks produced.
    pub chunk_count: u32,
}

/// Format a hex sha256 as the OCI `sha256:<hex>` digest string.
fn digest_str(hex_digest: &str) -> String {
    format!("sha256:{hex_digest}")
}

/// Split `qcow2` into fixed-size chunks, zstd-compress each, and write
/// `chunk-0000.zst`, `chunk-0001.zst`, … into `out_dir`. Returns a
/// [`ChunkSet`] describing every chunk plus the whole-image digest.
///
/// Streams the source exactly once: bytes feed the whole-image hasher and
/// a per-chunk zstd encoder simultaneously, so the image is never fully
/// resident. A zero-length image still yields a single empty chunk so the
/// layer/round-trip invariants hold.
pub fn chunk_and_compress(qcow2: &Path, chunk_size: u64, out_dir: &Path) -> Result<ChunkSet> {
    ensure!(chunk_size > 0, "chunk_size must be non-zero");
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("cannot create chunk dir {}", out_dir.display()))?;

    let file =
        File::open(qcow2).with_context(|| format!("cannot open image {}", qcow2.display()))?;
    let mut reader = BufReader::with_capacity(COPY_BUF, file);

    let mut whole = Sha256::new();
    let mut chunks: Vec<ChunkInfo> = Vec::new();
    let mut total_size: u64 = 0;
    let mut index: u32 = 0;

    let mut buf = vec![0u8; COPY_BUF];
    let mut eof = false;

    while !eof {
        let compressed_path = out_dir.join(format!("chunk-{index:04}.zst"));
        let out = File::create(&compressed_path)
            .with_context(|| format!("cannot create {}", compressed_path.display()))?;
        // Hash the compressed bytes on the way out so we never re-read.
        let mut sink = HashingWriter::new(BufWriter::with_capacity(COPY_BUF, out));
        let mut encoder =
            zstd::Encoder::new(&mut sink, ZSTD_LEVEL).context("cannot start zstd encoder")?;

        let mut chunk_uncompressed: u64 = 0;
        while chunk_uncompressed < chunk_size {
            let want = std::cmp::min(buf.len() as u64, chunk_size - chunk_uncompressed) as usize;
            let n = reader
                .read(&mut buf[..want])
                .with_context(|| format!("cannot read {}", qcow2.display()))?;
            if n == 0 {
                eof = true;
                break;
            }
            let slice = &buf[..n];
            whole.update(slice);
            encoder
                .write_all(slice)
                .with_context(|| format!("cannot compress into {}", compressed_path.display()))?;
            chunk_uncompressed += n as u64;
            total_size += n as u64;
        }

        encoder.finish().context("cannot finish zstd chunk")?;
        let (inner, compressed_digest_hex) = sink.finish();
        inner
            .into_inner()
            .map_err(|e| anyhow::anyhow!("cannot flush {}: {e}", compressed_path.display()))?;

        let compressed_size = std::fs::metadata(&compressed_path)
            .with_context(|| format!("cannot stat {}", compressed_path.display()))?
            .len();

        // Stop only after emitting at least one chunk; an empty trailing
        // chunk (when the image size is an exact multiple of chunk_size)
        // would be useless, so drop it unless it is the only one.
        if chunk_uncompressed == 0 && index != 0 {
            let _ = std::fs::remove_file(&compressed_path);
            break;
        }

        chunks.push(ChunkInfo {
            index,
            compressed_path,
            compressed_digest: digest_str(&compressed_digest_hex),
            compressed_size,
            uncompressed_size: chunk_uncompressed,
        });
        index += 1;
    }

    let whole_digest = digest_str(&hex::encode(whole.finalize()));
    let chunk_count = chunks.len() as u32;
    Ok(ChunkSet {
        chunks,
        whole_digest,
        total_size,
        chunk_size,
        chunk_count,
    })
}

/// Reassemble an image from compressed chunk blobs given in order: each is
/// zstd-decompressed and concatenated into `out`. The caller MUST then call
/// [`verify_whole`] against the manifest's whole-image digest before
/// installing — assembly itself does not validate content.
pub fn assemble(chunk_paths: &[PathBuf], out: &Path) -> Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let out_file = File::create(out).with_context(|| format!("cannot create {}", out.display()))?;
    let mut writer = BufWriter::with_capacity(COPY_BUF, out_file);

    for path in chunk_paths {
        let compressed =
            File::open(path).with_context(|| format!("cannot open chunk {}", path.display()))?;
        let mut decoder = zstd::Decoder::new(BufReader::with_capacity(COPY_BUF, compressed))
            .with_context(|| format!("cannot read zstd chunk {}", path.display()))?;
        std::io::copy(&mut decoder, &mut writer)
            .with_context(|| format!("cannot decompress chunk {}", path.display()))?;
    }
    writer
        .flush()
        .with_context(|| format!("cannot flush {}", out.display()))?;
    Ok(())
}

/// Verify that `path`'s streaming sha256 equals `expected` (an OCI
/// `sha256:<hex>` digest string, case-insensitive on the hex). Errors with
/// a clear mismatch message otherwise.
pub fn verify_whole(path: &Path, expected: &str) -> Result<()> {
    let want = expected
        .strip_prefix("sha256:")
        .unwrap_or(expected)
        .to_ascii_lowercase();
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(COPY_BUF, file);
    let mut hasher = Sha256::new();
    crate::hashing::feed(&mut reader, &mut hasher)
        .with_context(|| format!("cannot hash {}", path.display()))?;
    let got = hex::encode(hasher.finalize());
    if got != want {
        bail!(
            "whole-image digest mismatch: expected sha256:{want}, got sha256:{got} \
             — the reassembled image is corrupt"
        );
    }
    Ok(())
}

/// A `Write` adapter that feeds every byte through a SHA-256 hasher on its
/// way to an inner writer, so compressed-blob bytes are hashed exactly once
/// as they are produced.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    /// Returns the inner writer and the hex digest of everything written.
    fn finish(self) -> (W, String) {
        (self.inner, hex::encode(self.hasher.finalize()))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }

    #[test]
    fn round_trip_multi_chunk() {
        let dir = tempfile::tempdir().unwrap();
        // 5 MiB image, 1 MiB chunks → exactly 5 chunks.
        let mut data = vec![0u8; 5 * 1024 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let img = dir.path().join("disk.qcow2");
        std::fs::write(&img, &data).unwrap();

        let out = dir.path().join("chunks");
        let set = chunk_and_compress(&img, 1024 * 1024, &out).unwrap();

        assert_eq!(set.chunk_count, 5);
        assert_eq!(set.chunks.len(), 5);
        assert_eq!(set.total_size, data.len() as u64);
        assert_eq!(set.chunk_size, 1024 * 1024);
        assert_eq!(set.whole_digest, sha256_hex(&data));

        for (i, c) in set.chunks.iter().enumerate() {
            assert_eq!(c.index as usize, i);
            assert_eq!(c.uncompressed_size, 1024 * 1024);
            assert!(c.compressed_path.exists());
            assert!(c.compressed_digest.starts_with("sha256:"));
            // recorded compressed digest matches the file on disk
            let bytes = std::fs::read(&c.compressed_path).unwrap();
            assert_eq!(c.compressed_digest, sha256_hex(&bytes));
            assert_eq!(c.compressed_size, bytes.len() as u64);
        }

        // assemble back and verify the bytes are identical
        let paths: Vec<_> = set
            .chunks
            .iter()
            .map(|c| c.compressed_path.clone())
            .collect();
        let reassembled = dir.path().join("out.qcow2");
        assemble(&paths, &reassembled).unwrap();
        assert_eq!(std::fs::read(&reassembled).unwrap(), data);

        verify_whole(&reassembled, &set.whole_digest).unwrap();
    }

    #[test]
    fn ragged_final_chunk() {
        let dir = tempfile::tempdir().unwrap();
        // 2.5 MiB with 1 MiB chunks → 3 chunks, last is 0.5 MiB.
        let data: Vec<u8> = (0..2_621_440u32).map(|i| (i % 255) as u8).collect();
        let img = dir.path().join("disk.qcow2");
        std::fs::write(&img, &data).unwrap();
        let out = dir.path().join("chunks");
        let set = chunk_and_compress(&img, 1024 * 1024, &out).unwrap();
        assert_eq!(set.chunk_count, 3);
        assert_eq!(set.chunks[0].uncompressed_size, 1024 * 1024);
        assert_eq!(set.chunks[2].uncompressed_size, 2_621_440 - 2 * 1024 * 1024);

        let paths: Vec<_> = set
            .chunks
            .iter()
            .map(|c| c.compressed_path.clone())
            .collect();
        let reassembled = dir.path().join("out.qcow2");
        assemble(&paths, &reassembled).unwrap();
        assert_eq!(std::fs::read(&reassembled).unwrap(), data);
        verify_whole(&reassembled, &set.whole_digest).unwrap();
    }

    #[test]
    fn empty_image_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("disk.qcow2");
        std::fs::write(&img, b"").unwrap();
        let out = dir.path().join("chunks");
        let set = chunk_and_compress(&img, 1024 * 1024, &out).unwrap();
        assert_eq!(set.chunk_count, 1);
        assert_eq!(set.total_size, 0);
        assert_eq!(set.whole_digest, sha256_hex(b""));
        let paths: Vec<_> = set
            .chunks
            .iter()
            .map(|c| c.compressed_path.clone())
            .collect();
        let reassembled = dir.path().join("out.qcow2");
        assemble(&paths, &reassembled).unwrap();
        assert_eq!(std::fs::read(&reassembled).unwrap().len(), 0);
    }

    #[test]
    fn exact_multiple_no_trailing_empty_chunk() {
        let dir = tempfile::tempdir().unwrap();
        // exactly 2 MiB with 1 MiB chunks → 2 chunks, no empty 3rd.
        let data = vec![7u8; 2 * 1024 * 1024];
        let img = dir.path().join("disk.qcow2");
        std::fs::write(&img, &data).unwrap();
        let out = dir.path().join("chunks");
        let set = chunk_and_compress(&img, 1024 * 1024, &out).unwrap();
        assert_eq!(set.chunk_count, 2);
    }

    #[test]
    fn verify_whole_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("a");
        std::fs::write(&good, b"hello world").unwrap();
        let digest = sha256_hex(b"hello world");
        verify_whole(&good, &digest).unwrap();

        let bad = dir.path().join("b");
        std::fs::write(&bad, b"hello wXrld").unwrap();
        let err = verify_whole(&bad, &digest).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
    }
}
