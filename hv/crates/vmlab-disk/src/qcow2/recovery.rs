//! Reconstruct dirty-image refcounts from reachable metadata, never from
//! the old refcounts. Build replacement tables at EOF so recovery itself is
//! restartable: only publish their header pointer after syncing the tables.

use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;

use super::{COMPRESSED_BIT, OFFSET_MASK, Qcow2Image, TableCache};
use crate::{Error, Result};

impl Qcow2Image {
    pub(super) fn rebuild_refcounts(&mut self) -> Result<()> {
        let mut counts = BTreeMap::<u64, u64>::new();
        self.count_range(&mut counts, 0, self.cluster_size)?;
        self.count_range(&mut counts, self.l1_table_offset, self.l1_size as u64 * 8)?;
        self.count_view(&mut counts, &self.l1)?;

        let mut offset = self.snapshots_offset;
        for _ in 0..self.nb_snapshots {
            let mut fixed = [0u8; 40];
            self.file.read_exact_at(&mut fixed, offset)?;
            let l1_offset = u64::from_be_bytes(fixed[..8].try_into().unwrap());
            let entries = u32::from_be_bytes(fixed[8..12].try_into().unwrap()) as u64;
            self.count_range(&mut counts, l1_offset, entries * 8)?;
            let mut bytes = vec![0; (entries * 8) as usize];
            self.file.read_exact_at(&mut bytes, l1_offset)?;
            let l1: Vec<_> = bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| u64::from_be_bytes(*b))
                .collect();
            self.count_view(&mut counts, &l1)?;
            let id = u16::from_be_bytes(fixed[12..14].try_into().unwrap()) as u64;
            let name = u16::from_be_bytes(fixed[14..16].try_into().unwrap()) as u64;
            let extra = u32::from_be_bytes(fixed[36..40].try_into().unwrap()) as u64;
            offset = offset
                .checked_add((40 + extra + id + name).div_ceil(8) * 8)
                .ok_or_else(|| Error::Unsupported("snapshot table overflows u64".into()))?;
        }
        if self.nb_snapshots != 0 {
            self.count_range(
                &mut counts,
                self.snapshots_offset,
                offset - self.snapshots_offset,
            )?;
        }

        let first = self.file.metadata()?.len().div_ceil(self.cluster_size);
        let rbe = self.refblock_entries();
        // Include the replacement table and blocks in their own coverage.
        let mut blocks = first.div_ceil(rbe);
        let (blocks, table_clusters) = loop {
            let table_clusters = (blocks * 8).div_ceil(self.cluster_size).max(1);
            let needed = (first + table_clusters + blocks).div_ceil(rbe);
            if needed == blocks {
                break (blocks, table_clusters);
            }
            blocks = needed;
        };
        let table_offset = first * self.cluster_size;
        let block_start = first + table_clusters;
        self.file
            .set_len((block_start + blocks) * self.cluster_size)?;
        self.count_range(
            &mut counts,
            table_offset,
            (table_clusters + blocks) * self.cluster_size,
        )?;
        let mut table = vec![0u8; (table_clusters * self.cluster_size) as usize];
        for index in 0..blocks {
            let host = (block_start + index) * self.cluster_size;
            table[index as usize * 8..index as usize * 8 + 8].copy_from_slice(&host.to_be_bytes());
            let mut block = vec![0u8; self.cluster_size as usize];
            for (&cluster, &count) in counts.range(index * rbe..(index + 1) * rbe) {
                super::refcount::bits_set(&mut block, cluster % rbe, self.refcount_bits, count);
            }
            self.file.write_all_at(&block, host)?;
        }
        self.file.write_all_at(&table, table_offset)?;
        self.file.sync_all()?;
        // Offset and size are adjacent in the header. Leave DIRTY set until
        // clean close, including if recovery is interrupted after this write.
        let mut pointer = [0u8; 12];
        pointer[..8].copy_from_slice(&table_offset.to_be_bytes());
        let table_clusters = u32::try_from(table_clusters)
            .map_err(|_| Error::Unsupported("refcount table too large".into()))?;
        pointer[8..].copy_from_slice(&table_clusters.to_be_bytes());
        self.file.write_all_at(&pointer, 48)?;
        self.file.sync_all()?;
        self.refcount_table_offset = table_offset;
        self.refcount_table_clusters = table_clusters;
        self.refcount_table = table
            .as_chunks::<8>()
            .0
            .iter()
            .map(|b| u64::from_be_bytes(*b))
            .collect();
        self.refblock_cache = TableCache::new(64);
        self.patch_header_fields();
        self.write_started = true;
        self.rewrite_active_copied_bits()?;
        self.flush_metadata()
    }

    fn count_range(&self, counts: &mut BTreeMap<u64, u64>, offset: u64, len: u64) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::Unsupported("qcow2 reference overflows u64".into()))?;
        if end > self.file.metadata()?.len() {
            return Err(Error::Unsupported(
                "qcow2 reference beyond end of file".into(),
            ));
        }
        for cluster in offset / self.cluster_size..end.div_ceil(self.cluster_size) {
            let count = counts.entry(cluster).or_default();
            *count = count
                .checked_add(1)
                .filter(|n| *n <= (u64::MAX >> (64 - self.refcount_bits)))
                .ok_or_else(|| Error::Unsupported("recovered refcount overflow".into()))?;
        }
        Ok(())
    }

    fn count_view(&self, counts: &mut BTreeMap<u64, u64>, l1: &[u64]) -> Result<()> {
        for entry in l1 {
            let host = entry & OFFSET_MASK;
            if host == 0 {
                continue;
            }
            self.count_range(counts, host, self.cluster_size)?;
            let mut table = vec![0u8; self.cluster_size as usize];
            self.file.read_exact_at(&mut table, host)?;
            for bytes in table.as_chunks::<8>().0 {
                let entry = u64::from_be_bytes(*bytes);
                if entry & COMPRESSED_BIT != 0 {
                    let shift = 62 - (self.cluster_bits - 8);
                    let offset = entry & ((1u64 << shift) - 1);
                    let sectors = ((entry >> shift) & ((1u64 << (62 - shift)) - 1)) + 1;
                    self.count_range(counts, offset, sectors * 512)?;
                } else if entry & OFFSET_MASK != 0 {
                    self.count_range(counts, entry & OFFSET_MASK, self.cluster_size)?;
                }
            }
        }
        Ok(())
    }
}
