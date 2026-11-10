//! qcow2 images (version 2 and 3), backing chains and internal snapshots.
//!
//! Implemented from the qcow2 spec (`docs/interop/qcow2.txt` in QEMU's tree);
//! see `DESIGN.md` for the imago-vs-own decision and the black-box QEMU
//! observations that pin the vmstate layout.

mod recovery;
mod refcount;
mod snapshot;

use self::refcount::FreeClusters;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::{DiskImage, Error, Format, Result, SnapshotInfo};

pub const QCOW_MAGIC: u32 = 0x5146_49fb;

/// Bits 9-55 of an L1/L2 entry: the host cluster offset.
pub(crate) const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
pub(crate) const COMPRESSED_BIT: u64 = 1 << 62;
pub(crate) const COPIED_BIT: u64 = 1 << 63;
pub(crate) const ZERO_BIT: u64 = 1;

/// Backing-file header extension type.
const EXT_BACKING_FORMAT: u32 = 0xe279_2aca;
const EXT_END: u32 = 0;

const MAX_BACKING_DEPTH: usize = 8;
const INCOMPAT_DIRTY: u64 = 1;

#[derive(Clone, Copy, PartialEq)]
enum ReadSource {
    Backing,
    Zero,
    File(u64),
    Compressed(u64),
}

impl ReadSource {
    fn from_entry(entry: u64, in_cluster: u64) -> Self {
        if entry & COMPRESSED_BIT != 0 {
            Self::Compressed(entry)
        } else if entry & ZERO_BIT != 0 {
            Self::Zero
        } else if entry & OFFSET_MASK == 0 {
            Self::Backing
        } else {
            Self::File((entry & OFFSET_MASK) + in_cluster)
        }
    }
}

/// Cluster-keyed write-back cache (L2 tables, refcount blocks) with a
/// simple LRU eviction. `key` is the host offset for L2 tables and the
/// refcount-table index for refblocks.
#[derive(Default)]
struct TableCache {
    map: HashMap<u64, (Vec<u8>, bool)>,
    order: VecDeque<u64>,
    cap: usize,
}

impl TableCache {
    fn new(cap: usize) -> Self {
        TableCache {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    fn touch(&mut self, key: u64) {
        self.order.retain(|k| *k != key);
        self.order.push_back(key);
    }

    fn get(&mut self, key: u64) -> Option<&(Vec<u8>, bool)> {
        if self.map.contains_key(&key) {
            self.touch(key);
        }
        self.map.get(&key)
    }

    /// Insert an entry, returning any dirty entries evicted to make room.
    /// The caller must write those to disk before their clusters can be
    /// handed out again — dropping a dirty table would silently lose
    /// refcount or L2 updates.
    fn put(&mut self, key: u64, data: Vec<u8>, dirty: bool) -> Vec<(u64, Vec<u8>)> {
        let mut evicted = Vec::new();
        if !self.map.contains_key(&key) {
            while self.map.len() >= self.cap {
                let Some(k) = self.order.pop_front() else {
                    break;
                };
                if let Some((buf, dirty)) = self.map.remove(&k)
                    && dirty
                {
                    evicted.push((k, buf));
                }
            }
        }
        self.touch(key);
        self.map.insert(key, (data, dirty));
        evicted
    }

    fn mark_dirty(&mut self, key: u64) {
        if let Some(entry) = self.map.get_mut(&key) {
            entry.1 = true;
        }
    }

    fn remove(&mut self, key: u64) {
        self.map.remove(&key);
        self.order.retain(|k| *k != key);
    }

    /// Copies of the dirty entries. They stay dirty: the caller marks each
    /// one clean with [`TableCache::mark_clean`] only after its write
    /// succeeded, so a failed write leaves it for the next flush (VML-108).
    fn dirty(&self) -> Vec<(u64, Vec<u8>)> {
        self.map
            .iter()
            .filter(|(_, (_, dirty))| *dirty)
            .map(|(k, (buf, _))| (*k, buf.clone()))
            .collect()
    }

    fn mark_clean(&mut self, key: u64) {
        if let Some(entry) = self.map.get_mut(&key) {
            entry.1 = false;
        }
    }

    /// Put back evicted dirty entries whose write failed. This can take
    /// the cache over its capacity until the next `put` evicts again.
    fn restore_dirty(&mut self, entries: impl IntoIterator<Item = (u64, Vec<u8>)>) {
        for (key, data) in entries {
            self.touch(key);
            self.map.insert(key, (data, true));
        }
    }
}

pub(crate) struct BackingRef {
    /// As recorded in the header (may be relative).
    pub name: String,
    /// Backing format from the header extension, if recorded.
    pub format: Option<String>,
}

pub struct Qcow2Image {
    file: File,
    path: PathBuf,
    read_only: bool,
    poisoned: bool,
    snapshot_mutating: bool,

    version: u32,
    cluster_bits: u32,
    cluster_size: u64,
    /// Active virtual disk size in bytes.
    size: u64,
    /// Entries in the active L1 table (as recorded in the header).
    l1_size: u32,
    l1_table_offset: u64,
    refcount_table_offset: u64,
    refcount_table_clusters: u32,
    /// Refcount width in bits (1 << refcount_order; 16 for v2).
    refcount_bits: u32,
    nb_snapshots: u32,
    snapshots_offset: u64,
    /// The whole first cluster: header, extensions, backing file name.
    /// Rewritten from this buffer so unknown fields survive.
    header_cluster: Vec<u8>,
    backing: Option<BackingRef>,
    backing_image: Option<Box<dyn DiskImage>>,

    /// Active L1 entries (raw values, copied bit included).
    l1: Vec<u64>,
    /// Refcount table entries (raw values).
    refcount_table: Vec<u64>,

    l2_cache: TableCache,
    refblock_cache: TableCache,
    free: FreeClusters,
    header_dirty: bool,
    l1_dirty: bool,
    refcount_table_dirty: bool,
    // A zero refcount is not reusable until the old mapping is durable.
    retired: std::collections::HashSet<u64>,
    write_started: bool,
}

impl Qcow2Image {
    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        Self::open_depth(path, read_only, 0)
    }

    fn open_depth(path: &Path, read_only: bool, depth: usize) -> Result<Self> {
        if depth > MAX_BACKING_DEPTH {
            return Err(Error::Unsupported(format!(
                "backing chain deeper than {MAX_BACKING_DEPTH} at {}",
                path.display()
            )));
        }
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let mut img = Self::parse(path, file, read_only)?;
        // v2 has no dirty marker, so never trust its refcounts on writable open.
        if !read_only
            && (img.is_dirty() || img.version == 2)
            && let Err(error) = img.rebuild_refcounts()
        {
            // Never flush a partially recovered image from Drop.
            img.read_only = true;
            return Err(error);
        }
        img.open_backing(depth)?;
        Ok(img)
    }

    fn parse(path: &Path, mut file: File, read_only: bool) -> Result<Self> {
        let mut first = vec![0u8; 512];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut first)?;
        let be32 = |o: usize| u32::from_be_bytes(first[o..o + 4].try_into().unwrap());
        let be64 = |o: usize| u64::from_be_bytes(first[o..o + 8].try_into().unwrap());

        if be32(0) != QCOW_MAGIC {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!("bad magic {:#010x} in {}", be32(0), path.display()),
            });
        }
        let version = be32(4);
        if version != 2 && version != 3 {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!("unsupported version {version}"),
            });
        }
        let backing_file_offset = be64(8);
        let backing_file_size = be32(16) as u64;
        let cluster_bits = be32(20);
        if !(9..=21).contains(&cluster_bits) {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!("cluster_bits {cluster_bits} out of range"),
            });
        }
        let cluster_size = 1u64 << cluster_bits;
        // Keep the whole header cluster so extensions and the backing name
        // survive rewrites.
        let mut header_cluster = vec![0u8; cluster_size as usize];
        header_cluster[..512].copy_from_slice(&first);
        if cluster_size as usize > 512 {
            file.seek(SeekFrom::Start(512))?;
            file.read_exact(&mut header_cluster[512..])?;
        }
        let be64h = |o: usize| u64::from_be_bytes(header_cluster[o..o + 8].try_into().unwrap());

        let size = be64h(24);
        let crypt = be32(32);
        if crypt != 0 {
            return Err(Error::Unsupported(format!(
                "encrypted qcow2 images (crypt_method {crypt}) are not supported"
            )));
        }
        let l1_size = be32(36);
        let l1_table_offset = be64h(40);
        let refcount_table_offset = be64h(48);
        let refcount_table_clusters = be32(56);
        let nb_snapshots = be32(60);
        let snapshots_offset = be64h(64);

        let (incompatible_features, autoclear_features, refcount_order, header_length) =
            if version >= 3 {
                (be64h(72), be64h(88), be32(96), be32(100))
            } else {
                (0, 0, 4, 72)
            };
        if version >= 3 {
            if header_length < 104 || header_length % 8 != 0 {
                return Err(Error::Format {
                    format: "qcow2",
                    reason: format!("bad header length {header_length}"),
                });
            }
            if refcount_order > 6 {
                return Err(Error::Format {
                    format: "qcow2",
                    reason: format!("bad refcount_order {refcount_order}"),
                });
            }
        }
        const INCOMPAT_CORRUPT: u64 = 1 << 1;
        const INCOMPAT_EXTERNAL_DATA: u64 = 1 << 2;
        const INCOMPAT_COMPRESSION_TYPE: u64 = 1 << 3;
        const INCOMPAT_EXTENDED_L2: u64 = 1 << 4;
        let known = INCOMPAT_DIRTY
            | INCOMPAT_CORRUPT
            | INCOMPAT_EXTERNAL_DATA
            | INCOMPAT_COMPRESSION_TYPE
            | INCOMPAT_EXTENDED_L2;
        if incompatible_features & !known != 0 {
            return Err(Error::Unsupported(format!(
                "unknown incompatible features {incompatible_features:#x}"
            )));
        }
        if incompatible_features & INCOMPAT_CORRUPT != 0 {
            return Err(Error::Unsupported("image is marked corrupt".into()));
        }
        if incompatible_features & INCOMPAT_EXTERNAL_DATA != 0 {
            return Err(Error::Unsupported(
                "external data files are not supported".into(),
            ));
        }
        if incompatible_features & INCOMPAT_EXTENDED_L2 != 0 {
            return Err(Error::Unsupported(
                "extended L2 entries are not supported".into(),
            ));
        }
        if incompatible_features & INCOMPAT_COMPRESSION_TYPE != 0 {
            let compression_type = header_cluster.get(104).copied().unwrap_or(0);
            if compression_type != 0 {
                return Err(Error::Unsupported(format!(
                    "compression type {compression_type} (only deflate/zlib is supported)"
                )));
            }
        }
        if !read_only && autoclear_features != 0 {
            return Err(Error::Unsupported(format!(
                "autoclear features {autoclear_features:#x} are set; opening for \
                 write would silently drop them"
            )));
        }
        let refcount_bits = 1u32 << refcount_order;

        if l1_size > 0 && l1_table_offset % cluster_size != 0 {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!("L1 table offset {l1_table_offset:#x} is not cluster-aligned"),
            });
        }
        if refcount_table_clusters > 0 && refcount_table_offset % cluster_size != 0 {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!(
                    "refcount table offset {refcount_table_offset:#x} is not cluster-aligned"
                ),
            });
        }
        if nb_snapshots > 0 && snapshots_offset % cluster_size != 0 {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!(
                    "snapshot table offset {snapshots_offset:#x} is not cluster-aligned"
                ),
            });
        }

        // Validate hostile allocation lengths against the opened file first.
        let file_len = file.metadata()?.len();
        checked_file_range(file_len, l1_table_offset, u64::from(l1_size) * 8)?;
        checked_file_range(
            file_len,
            refcount_table_offset,
            u64::from(refcount_table_clusters) * cluster_size,
        )?;
        // Read the active L1.
        let mut l1_bytes = vec![0u8; l1_size as usize * 8];
        if !l1_bytes.is_empty() {
            file.seek(SeekFrom::Start(l1_table_offset))?;
            file.read_exact(&mut l1_bytes)?;
        }
        let l1 = l1_bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();

        // Read the refcount table.
        let rt_len = refcount_table_clusters as usize * cluster_size as usize;
        let mut rt_bytes = vec![0u8; rt_len];
        if !rt_bytes.is_empty() {
            file.seek(SeekFrom::Start(refcount_table_offset))?;
            file.read_exact(&mut rt_bytes)?;
        }
        let refcount_table = rt_bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();

        // Backing file reference.
        let backing = if backing_file_offset != 0 && backing_file_size > 0 {
            if backing_file_size > 1023 {
                return Err(Error::Format {
                    format: "qcow2",
                    reason: format!("backing file name too long ({backing_file_size})"),
                });
            }
            let start = backing_file_offset as usize;
            let end = start + backing_file_size as usize;
            if end > header_cluster.len() {
                return Err(Error::Format {
                    format: "qcow2",
                    reason: format!(
                        "backing file name at {backing_file_offset:#x} is outside the header cluster"
                    ),
                });
            }
            let name = String::from_utf8_lossy(&header_cluster[start..end]).into_owned();
            Some(BackingRef {
                name,
                format: parse_backing_format(&header_cluster, header_length as usize),
            })
        } else {
            None
        };

        let img = Qcow2Image {
            file,
            path: path.to_path_buf(),
            read_only,
            poisoned: false,
            snapshot_mutating: false,
            version,
            cluster_bits,
            cluster_size,
            size,
            l1_size,
            l1_table_offset,
            refcount_table_offset,
            refcount_table_clusters,
            refcount_bits,
            nb_snapshots,
            snapshots_offset,
            header_cluster,
            backing,
            backing_image: None,
            l1,
            refcount_table,
            l2_cache: TableCache::new(64),
            refblock_cache: TableCache::new(64),
            header_dirty: false,
            l1_dirty: false,
            refcount_table_dirty: false,
            free: FreeClusters::default(),
            retired: Default::default(),
            write_started: false,
        };
        Ok(img)
    }

    fn open_backing(&mut self, depth: usize) -> Result<()> {
        let Some(backing) = &self.backing else {
            return Ok(());
        };
        let backing_path = resolve_backing(&self.path, &backing.name);
        let format = match backing.format.as_deref() {
            Some("qcow2") => Format::Qcow2,
            Some("raw") => Format::Raw,
            Some(other) => {
                return Err(Error::Unsupported(format!(
                    "backing format {other} is not supported"
                )));
            }
            None => probe_format(&backing_path)?,
        };
        let image: Box<dyn DiskImage> = match format {
            Format::Raw => Box::new(crate::raw::RawImage::open(&backing_path, true)?),
            Format::Qcow2 => Box::new(Qcow2Image::open_depth(&backing_path, true, depth + 1)?),
        };
        self.backing_image = Some(image);
        Ok(())
    }

    // ----- geometry -----

    /// Cluster size in bytes (debugging and ops helpers).
    pub fn cluster_size_bytes(&self) -> u64 {
        self.cluster_size
    }

    pub(crate) fn l2_entries(&self) -> u64 {
        self.cluster_size / 8
    }

    /// Guest clusters covered by one L1 entry.
    pub(crate) fn l2_span(&self) -> u64 {
        self.l2_entries() * self.cluster_size
    }

    /// The offset at which the vmstate area starts: the first L1 entry that
    /// describes no regular guest cluster (observed from QEMU's savevm).
    pub(crate) fn vmstate_base(&self) -> u64 {
        let span = self.l2_span();
        span * self.size.div_ceil(span)
    }

    fn l2_index_bits(&self) -> u32 {
        self.cluster_bits + (self.cluster_bits - 3)
    }

    fn l1_index_of(&self, guest_offset: u64) -> u64 {
        guest_offset >> self.l2_index_bits()
    }

    fn l2_index_of(&self, guest_offset: u64) -> u64 {
        (guest_offset >> self.cluster_bits) & (self.l2_entries() - 1)
    }

    // ----- low-level file IO -----

    fn read_file(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }

    fn write_file(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        self.begin_write()?;
        self.file.write_all_at(buf, offset)?;
        Ok(())
    }

    // ----- L2 table access -----

    fn load_l2(&mut self, host_offset: u64) -> Result<()> {
        if self.l2_cache.get(host_offset).is_some() {
            return Ok(());
        }
        let mut buf = vec![0u8; self.cluster_size as usize];
        self.read_file(host_offset, &mut buf)?;
        let mut evicted = self.l2_cache.put(host_offset, buf, false).into_iter();
        while let Some((host, table)) = evicted.next() {
            if let Err(e) = self.write_file(host, &table) {
                // Keep the failed table and the unwritten rest dirty.
                self.l2_cache
                    .restore_dirty(std::iter::once((host, table)).chain(evicted));
                return Err(e);
            }
        }
        Ok(())
    }

    fn l2_entry(&mut self, l2_host: u64, index: u64) -> Result<u64> {
        self.load_l2(l2_host)?;
        let (buf, _) = self.l2_cache.get(l2_host).expect("just cached");
        let off = index as usize * 8;
        Ok(u64::from_be_bytes(buf[off..off + 8].try_into().unwrap()))
    }

    fn set_l2_entry(&mut self, l2_host: u64, index: u64, value: u64) -> Result<()> {
        self.load_l2(l2_host)?;
        self.l2_cache.mark_dirty(l2_host);
        let (buf, _) = self.l2_cache.map.get_mut(&l2_host).expect("just cached");
        let off = index as usize * 8;
        buf[off..off + 8].copy_from_slice(&value.to_be_bytes());
        Ok(())
    }

    /// Read a full cluster's current content: through the cache, which
    /// holds changes not yet flushed to disk.
    fn read_cluster_via_cache(&mut self, host: u64) -> Result<Vec<u8>> {
        self.load_l2(host)?;
        let (buf, _) = self.l2_cache.get(host).expect("just cached");
        Ok(buf.clone())
    }

    /// Copy-on-write an L2 table that is shared with a snapshot before the
    /// active layer modifies any of its entries. Returns the L2 host offset
    /// the active layer should use (unchanged when exclusively owned).
    fn cow_l2_if_shared(&mut self, l1_index: usize) -> Result<u64> {
        let l2_host = self.l1[l1_index] & OFFSET_MASK;
        if l2_host == 0 {
            return Ok(0);
        }
        if self.get_refcount(l2_host / self.cluster_size)? <= 1 {
            return Ok(l2_host);
        }
        let content = self.read_cluster_via_cache(l2_host)?;
        let new_l2 = self.alloc_cluster()?;
        self.write_file(new_l2, &content)?;
        self.set_cluster_refcount(new_l2 / self.cluster_size, 1)?;
        self.dec_refcount(l2_host / self.cluster_size)?;
        self.l1[l1_index] = new_l2 | COPIED_BIT;
        self.l1_dirty = true;
        Ok(new_l2)
    }

    fn l2_allocated_indexes(&mut self, l2_host: u64) -> Result<Vec<u64>> {
        self.load_l2(l2_host)?;
        let (buf, _) = self.l2_cache.get(l2_host).expect("just cached");
        Ok(buf
            .as_chunks::<8>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, c)| **c != [0u8; 8])
            .map(|(i, _)| i as u64)
            .collect())
    }

    // ----- guest byte IO -----

    fn read_unallocated(&mut self, guest_offset: u64, out: &mut [u8]) -> Result<()> {
        match self.backing_image.as_mut() {
            Some(backing) => {
                if guest_offset >= backing.size() {
                    out.fill(0);
                } else {
                    let take = out.len().min((backing.size() - guest_offset) as usize);
                    backing.read_at(guest_offset, &mut out[..take])?;
                    out[take..].fill(0);
                }
                Ok(())
            }
            None => {
                out.fill(0);
                Ok(())
            }
        }
    }

    fn read_compressed_cluster(&mut self, entry: u64, out: &mut [u8]) -> Result<()> {
        let x = 62 - (self.cluster_bits - 8);
        let host_offset = entry & ((1u64 << x) - 1);
        let sector_bits = 62 - x;
        let additional_sectors = (entry >> x) & ((1u64 << sector_bits) - 1);
        // The stream runs from the sector containing `host_offset` through
        // `additional_sectors` more; it may end mid-sector.
        let first_sector = host_offset / 512;
        let end = (first_sector + additional_sectors + 1) * 512;
        let mut compressed = vec![0u8; (end - host_offset) as usize];
        self.read_file(host_offset, &mut compressed)?;
        let mut decompressor = flate2::Decompress::new(false);
        // One spare byte lets the decoder consume the end marker even when
        // the valid output fills a whole cluster, and detects overlong data.
        let mut decoded = vec![0; out.len() + 1];
        let status = decompressor
            .decompress(&compressed, &mut decoded, flate2::FlushDecompress::Finish)
            .map_err(|e| Error::Format {
                format: "qcow2",
                reason: format!("deflate error in compressed cluster: {e}"),
            })?;
        if status != flate2::Status::StreamEnd || decompressor.total_out() != out.len() as u64 {
            return Err(Error::Format {
                format: "qcow2",
                reason: "compressed cluster must contain one complete cluster".into(),
            });
        }
        out.copy_from_slice(&decoded[..out.len()]);
        // Sector padding after the complete stream is allowed.
        Ok(())
    }

    /// Coalesce mappings within one L2 table without crossing a source change.
    fn read_extent(&mut self, offset: u64, max_len: usize) -> Result<(ReadSource, usize)> {
        let l1 = self
            .l1
            .get(self.l1_index_of(offset) as usize)
            .copied()
            .unwrap_or(0);
        let l2_host = l1 & OFFSET_MASK;
        if l2_host == 0 {
            let len = max_len.min((self.l2_span() - offset % self.l2_span()) as usize);
            return Ok((ReadSource::Backing, len));
        }
        let cluster_size = self.cluster_size;
        let in_cluster = offset % cluster_size;
        let index = self.l2_index_of(offset) as usize;
        self.load_l2(l2_host)?;
        let (table, _) = self.l2_cache.get(l2_host).expect("just cached");
        let entries = table.as_chunks::<8>().0;
        let source = ReadSource::from_entry(u64::from_be_bytes(entries[index]), in_cluster);
        let mut len = max_len.min((cluster_size - in_cluster) as usize);
        if !matches!(source, ReadSource::Compressed(_)) {
            for entry in &entries[index + 1..] {
                if len == max_len {
                    break;
                }
                let next = ReadSource::from_entry(u64::from_be_bytes(*entry), 0);
                let adjacent = match (source, next) {
                    (ReadSource::File(start), ReadSource::File(next)) => next == start + len as u64,
                    (ReadSource::Backing, ReadSource::Backing)
                    | (ReadSource::Zero, ReadSource::Zero) => true,
                    _ => false,
                };
                if !adjacent {
                    break;
                }
                len += (max_len - len).min(cluster_size as usize);
            }
        }
        Ok((source, len))
    }

    fn read_bytes(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let abs = offset + done as u64;
            let (source, len) = self.read_extent(abs, buf.len() - done)?;
            let out = &mut buf[done..done + len];
            match source {
                ReadSource::Backing => self.read_unallocated(abs, out)?,
                ReadSource::Zero => out.fill(0),
                ReadSource::File(host) => self.read_file(host, out)?,
                ReadSource::Compressed(entry) => {
                    let mut cluster = vec![0; self.cluster_size as usize];
                    self.read_compressed_cluster(entry, &mut cluster)?;
                    let start = (abs % self.cluster_size) as usize;
                    out.copy_from_slice(&cluster[start..start + len]);
                }
            }
            done += len;
        }
        Ok(())
    }

    fn read_target(
        &mut self,
        offset: u64,
        len: usize,
        target: &mut dyn crate::ReadTarget,
    ) -> Result<()> {
        let mut done = 0;
        while done < len {
            let abs = offset + done as u64;
            let (source, count) = self.read_extent(abs, len - done)?;
            match source {
                ReadSource::File(host) => target.read_file(&self.file, host, count)?,
                ReadSource::Zero => target.zero(count)?,
                ReadSource::Backing => {
                    let backed = self.backing_image.as_ref().map_or(0, |image| {
                        image.size().saturating_sub(abs).min(count as u64) as usize
                    });
                    if backed != 0 {
                        self.backing_image
                            .as_mut()
                            .unwrap()
                            .read_into(abs, backed, target)?;
                    }
                    target.zero(count - backed)?;
                }
                ReadSource::Compressed(entry) => {
                    let mut cluster = vec![0; self.cluster_size as usize];
                    self.read_compressed_cluster(entry, &mut cluster)?;
                    let start = (abs % self.cluster_size) as usize;
                    target.write_all(&cluster[start..start + count])?;
                }
            }
            done += count;
        }
        Ok(())
    }

    fn write_bytes(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let abs = offset + done as u64;
            let cluster_off = abs & !(self.cluster_size - 1);
            let in_off = abs % self.cluster_size;
            let take = ((self.cluster_size - in_off) as usize).min(buf.len() - done);
            let l1_index = self.l1_index_of(cluster_off) as usize;
            let l2_index = self.l2_index_of(cluster_off);
            let l1e = *self
                .l1
                .get(l1_index)
                .ok_or_else(|| Error::Unsupported("write past L1 coverage".into()))?;
            let mut l2_host = l1e & OFFSET_MASK;
            if l2_host == 0 {
                let l2 = self.alloc_cluster()?;
                let zeros = vec![0u8; self.cluster_size as usize];
                self.write_file(l2, &zeros)?;
                self.set_cluster_refcount(l2 / self.cluster_size, 1)?;
                self.l1[l1_index] = l2 | COPIED_BIT;
                self.l1_dirty = true;
                self.header_dirty = true;
                l2_host = l2;
            } else {
                l2_host = self.cow_l2_if_shared(l1_index)?;
            }
            let entry = self.l2_entry(l2_host, l2_index)?;
            let host = entry & OFFSET_MASK;
            let compressed = entry & COMPRESSED_BIT != 0;
            let zero_flag = entry & ZERO_BIT != 0;

            if !compressed && host != 0 {
                let owned = if entry & COPIED_BIT != 0 {
                    true
                } else {
                    // The copied bit is a hint; the refcount is the truth.
                    self.get_refcount(host / self.cluster_size)? == 1
                };
                if owned {
                    if entry & COPIED_BIT == 0 {
                        self.set_l2_entry(l2_host, l2_index, entry | COPIED_BIT)?;
                    }
                    self.write_file(host + in_off, &buf[done..done + take])?;
                    done += take;
                    continue;
                }
            }

            // Need a fresh cluster: build the full cluster content first.
            let mut cluster = vec![0u8; self.cluster_size as usize];
            let full = in_off == 0 && take as u64 == self.cluster_size;
            if !full {
                if compressed {
                    self.read_compressed_cluster(entry, &mut cluster)?;
                } else if zero_flag {
                    // base is zeros
                } else if host != 0 {
                    self.read_file(host, &mut cluster)?;
                } else {
                    self.read_unallocated(cluster_off, &mut cluster)?;
                }
            }
            cluster[in_off as usize..in_off as usize + take]
                .copy_from_slice(&buf[done..done + take]);
            let new_host = self.alloc_cluster()?;
            self.write_file(new_host, &cluster)?;
            self.set_cluster_refcount(new_host / self.cluster_size, 1)?;
            if compressed {
                self.adjust_compressed_refcounts(entry, -1)?;
            } else if host != 0 {
                self.dec_refcount(host / self.cluster_size)?;
            }
            self.set_l2_entry(l2_host, l2_index, new_host | COPIED_BIT)?;
            done += take;
        }
        Ok(())
    }

    /// Adjust refcounts for the host clusters a compressed descriptor covers.
    pub(crate) fn adjust_compressed_refcounts(&mut self, entry: u64, delta: i64) -> Result<()> {
        let x = 62u32 - (self.cluster_bits - 8);
        let host_offset = entry & ((1u64 << x) - 1);
        let sector_bits = 62 - x;
        let additional_sectors = (entry >> x) & ((1u64 << sector_bits) - 1);
        let end = host_offset + (additional_sectors + 1) * 512;
        let first = host_offset / self.cluster_size;
        let last = end.div_ceil(self.cluster_size) - 1;
        for cluster_idx in first..=last {
            if delta > 0 {
                self.inc_refcount(cluster_idx)?;
            } else {
                self.dec_refcount(cluster_idx)?;
            }
        }
        Ok(())
    }

    // ----- metadata flush -----

    fn flush_metadata(&mut self) -> Result<()> {
        self.check_writable()?;
        self.begin_write()?;
        // Refblocks first (their location comes from the refcount table),
        // then the refcount table, the L1, the L2s and the header.
        // Each table is marked clean only after its write succeeded, so a
        // failed flush can be retried.
        for (index, block) in self.refblock_cache.dirty() {
            let host = self.refblock_host(index)?;
            self.write_file(host, &block)?;
            self.refblock_cache.mark_clean(index);
        }
        if self.refcount_table_dirty {
            let bytes: Vec<u8> = self
                .refcount_table
                .iter()
                .flat_map(|e| e.to_be_bytes())
                .collect();
            self.write_file(self.refcount_table_offset, &bytes)?;
            self.refcount_table_dirty = false;
        }
        self.file.sync_all()?;
        #[cfg(test)]
        self.crash_checkpoint("refcounts");
        if self.l1_dirty {
            let bytes: Vec<u8> = self.l1.iter().flat_map(|e| e.to_be_bytes()).collect();
            self.write_file(self.l1_table_offset, &bytes)?;
            self.l1_dirty = false;
        }
        for (host, table) in self.l2_cache.dirty() {
            self.write_file(host, &table)?;
            self.l2_cache.mark_clean(host);
        }
        if self.header_dirty {
            self.patch_header_fields();
            let cluster = self.header_cluster.clone();
            self.write_file(0, &cluster)?;
            self.header_dirty = false;
        }
        self.file.sync_all()?;
        #[cfg(test)]
        self.crash_checkpoint("mappings");
        self.retired.clear();
        self.free = FreeClusters::default();
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        self.version >= 3
            && u64::from_be_bytes(self.header_cluster[72..80].try_into().unwrap()) & INCOMPAT_DIRTY
                != 0
    }

    /// Persist the dirty marker before the first data or metadata write.
    /// Version 2 has no incompatible-feature word.
    fn begin_write(&mut self) -> Result<()> {
        if self.write_started {
            return Ok(());
        }
        if self.version >= 3 {
            let features = u64::from_be_bytes(self.header_cluster[72..80].try_into().unwrap())
                | INCOMPAT_DIRTY;
            self.file.write_all_at(&features.to_be_bytes(), 72)?;
            self.file.sync_all()?;
            self.header_cluster[72..80].copy_from_slice(&features.to_be_bytes());
        }
        self.write_started = true;
        Ok(())
    }

    fn close_clean(&mut self) -> Result<()> {
        self.flush()?;
        if self.is_dirty() {
            let features = u64::from_be_bytes(self.header_cluster[72..80].try_into().unwrap())
                & !INCOMPAT_DIRTY;
            self.file.write_all_at(&features.to_be_bytes(), 72)?;
            self.file.sync_all()?;
            self.header_cluster[72..80].copy_from_slice(&features.to_be_bytes());
        }
        Ok(())
    }

    #[cfg(test)]
    fn crash_checkpoint(&self, phase: &str) {
        if std::env::var_os("VMLAB_QCOW_CRASH_IMAGE").as_deref() == Some(self.path.as_os_str())
            && std::env::var("VMLAB_QCOW_CRASH_PHASE").as_deref() == Ok(phase)
        {
            use std::io::Write;
            println!("QCOW_CHECKPOINT");
            std::io::stdout().flush().unwrap();
            // The parent kills this child by PID. Do not run Drop.
            let mut byte = [0];
            std::io::stdin().read_exact(&mut byte).unwrap();
            panic!("crash checkpoint unexpectedly resumed");
        }
    }

    fn refblock_host(&self, table_index: u64) -> Result<u64> {
        let entry = *self
            .refcount_table
            .get(table_index as usize)
            .ok_or_else(|| Error::Format {
                format: "qcow2",
                reason: format!("refcount table index {table_index} out of range"),
            })?;
        let host = entry & OFFSET_MASK;
        if host == 0 {
            return Err(Error::Format {
                format: "qcow2",
                reason: format!("refblock {table_index} is not allocated"),
            });
        }
        Ok(host)
    }

    fn patch_header_fields(&mut self) {
        let put32 = |hc: &mut [u8], off: usize, v: u32| {
            hc[off..off + 4].copy_from_slice(&v.to_be_bytes());
        };
        let put64 = |hc: &mut [u8], off: usize, v: u64| {
            hc[off..off + 8].copy_from_slice(&v.to_be_bytes());
        };
        let hc = &mut self.header_cluster;
        put64(hc, 24, self.size);
        put32(hc, 36, self.l1_size);
        put64(hc, 40, self.l1_table_offset);
        put64(hc, 48, self.refcount_table_offset);
        put32(hc, 56, self.refcount_table_clusters);
        put32(hc, 60, self.nb_snapshots);
        put64(hc, 64, self.snapshots_offset);
    }

    fn check_writable(&self) -> Result<()> {
        if self.poisoned {
            return Err(Error::Unsupported("snapshot change failed; reopen the image before writing".into()));
        }
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        Ok(())
    }

    // ----- construction (used by ops::create) -----

    /// Create a fresh version-3 image at `path` (which must not exist).
    /// `cluster_bits` 16 (64 KiB) matches qemu-img's default.
    pub fn create(
        path: &Path,
        size: u64,
        cluster_bits: u32,
        backing: Option<(&Path, Option<&str>)>,
    ) -> Result<Qcow2Image> {
        let cluster_size = 1u64 << cluster_bits;
        let span = (cluster_size / 8) * cluster_size;
        let l1_size = size.div_ceil(span).max(1) as u32;
        let l1_clusters = l1_size_as_clusters(l1_size, cluster_size);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        // Layout: cluster 0 header, cluster 1 refcount table, cluster 2
        // refblock 0, then the L1 table.
        let rt_clusters: u64 = 1;
        let refblock0 = 2 * cluster_size;
        let l1_offset = 3 * cluster_size;
        file.set_len(l1_offset + l1_clusters * cluster_size)?;

        let header_len: usize = 112;
        let mut hc = vec![0u8; cluster_size as usize];
        hc[0..4].copy_from_slice(&QCOW_MAGIC.to_be_bytes());
        hc[4..8].copy_from_slice(&3u32.to_be_bytes());
        hc[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        hc[24..32].copy_from_slice(&size.to_be_bytes());
        hc[32..36].copy_from_slice(&0u32.to_be_bytes()); // crypt: none
        hc[36..40].copy_from_slice(&l1_size.to_be_bytes());
        hc[40..48].copy_from_slice(&l1_offset.to_be_bytes());
        hc[48..56].copy_from_slice(&cluster_size.to_be_bytes());
        hc[56..60].copy_from_slice(&(rt_clusters as u32).to_be_bytes());
        hc[72..80].copy_from_slice(&0u64.to_be_bytes()); // incompatible
        hc[80..88].copy_from_slice(&0u64.to_be_bytes()); // compatible
        hc[88..96].copy_from_slice(&0u64.to_be_bytes()); // autoclear
        hc[96..100].copy_from_slice(&4u32.to_be_bytes()); // 16-bit refcounts
        hc[100..104].copy_from_slice(&(header_len as u32).to_be_bytes());
        hc[104] = 0; // compression type: deflate

        let mut ext_off = header_len;
        let add_ext = |hc: &mut [u8], off: &mut usize, et: u32, data: &[u8]| {
            hc[*off..*off + 4].copy_from_slice(&et.to_be_bytes());
            hc[*off + 4..*off + 8].copy_from_slice(&(data.len() as u32).to_be_bytes());
            hc[*off + 8..*off + 8 + data.len()].copy_from_slice(data);
            *off += 8 + data.len().div_ceil(8) * 8;
        };
        if let Some((_, backing_format)) = backing
            && let Some(fmt) = backing_format
        {
            add_ext(&mut hc, &mut ext_off, EXT_BACKING_FORMAT, fmt.as_bytes());
        }
        add_ext(&mut hc, &mut ext_off, EXT_END, &[]);
        if let Some((backing_path, _)) = backing {
            let name = backing_path.to_string_lossy().into_owned();
            let name = name.as_bytes();
            if ext_off + name.len() > hc.len() {
                return Err(Error::Unsupported(
                    "backing file name does not fit in the header cluster".into(),
                ));
            }
            hc[8..16].copy_from_slice(&(ext_off as u64).to_be_bytes());
            hc[16..20].copy_from_slice(&(name.len() as u32).to_be_bytes());
            hc[ext_off..ext_off + name.len()].copy_from_slice(name);
        }

        // Refcount table: entry 0 -> refblock 0.
        let mut rt = vec![0u8; rt_clusters as usize * cluster_size as usize];
        rt[0..8].copy_from_slice(&refblock0.to_be_bytes());
        let refcount_table: Vec<u64> = rt
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_be_bytes(*c))
            .collect();

        let mut img = Qcow2Image {
            file,
            path: path.to_path_buf(),
            read_only: false,
            version: 3,
            cluster_bits,
            cluster_size,
            size,
            l1_size,
            l1_table_offset: l1_offset,
            refcount_table_offset: cluster_size,
            refcount_table_clusters: rt_clusters as u32,
            refcount_bits: 16,
            nb_snapshots: 0,
            snapshots_offset: 0,
            header_cluster: hc.clone(),
            backing: backing.map(|(p, f)| BackingRef {
                name: p.to_string_lossy().into_owned(),
                format: f.map(|s| s.to_string()),
            }),
            backing_image: None,
            l1: vec![0u64; l1_size as usize],
            refcount_table,
            l2_cache: TableCache::new(64),
            refblock_cache: TableCache::new(64),
            header_dirty: false,
            l1_dirty: false,
            refcount_table_dirty: false,
            free: FreeClusters::default(),
            retired: Default::default(),
            write_started: false,
        };
        // Install the initial header before begin_write updates its marker.
        img.file.write_all_at(&hc, 0)?;
        // Refcounts for the metadata clusters themselves.
        for idx in 0u64..(3 + l1_clusters) {
            img.set_cluster_refcount(idx, 1)?;
        }
        img.write_file(cluster_size, &rt)?;
        img.open_backing(0)?;
        Ok(img)
    }

    // ----- introspection used by ops -----

    /// The backing file name exactly as recorded in the header.
    pub fn backing_file_name(&self) -> Option<&str> {
        self.backing.as_ref().map(|b| b.name.as_str())
    }

    /// The backing file path resolved against this image's directory.
    pub fn backing_file_path(&self) -> Option<PathBuf> {
        self.backing
            .as_ref()
            .map(|b| resolve_backing(&self.path, &b.name))
    }

    /// The recorded backing format, if any.
    pub fn backing_format(&self) -> Option<&str> {
        self.backing.as_ref().and_then(|b| b.format.as_deref())
    }

    /// Replace the backing reference, rewriting the header cluster. `format`
    /// is recorded in the backing-format extension.
    pub fn set_backing(&mut self, name: &str, format: Option<&str>) -> Result<()> {
        self.check_writable()?;
        let header_len = if self.version >= 3 {
            u32::from_be_bytes(self.header_cluster[100..104].try_into().unwrap()) as usize
        } else {
            72
        };
        let mut hc = vec![0u8; self.cluster_size as usize];
        hc[..header_len].copy_from_slice(&self.header_cluster[..header_len]);
        // Preserve any extensions we know nothing about? A full rewrite drops
        // unknown extensions; QEMU's rebase does the same for formats it
        // rewrites. Keep it simple: rebuild extensions from scratch.
        let mut ext_off = header_len;
        let add_ext = |hc: &mut [u8], off: &mut usize, et: u32, data: &[u8]| {
            hc[*off..*off + 4].copy_from_slice(&et.to_be_bytes());
            hc[*off + 4..*off + 8].copy_from_slice(&(data.len() as u32).to_be_bytes());
            hc[*off + 8..*off + 8 + data.len()].copy_from_slice(data);
            *off += 8 + data.len().div_ceil(8) * 8;
        };
        if let Some(fmt) = format {
            add_ext(&mut hc, &mut ext_off, EXT_BACKING_FORMAT, fmt.as_bytes());
        }
        add_ext(&mut hc, &mut ext_off, EXT_END, &[]);
        let name_bytes = name.as_bytes();
        if name_bytes.len() > 1023 || ext_off + name_bytes.len() > hc.len() {
            return Err(Error::Unsupported(
                "backing file name does not fit in the header cluster".into(),
            ));
        }
        hc[ext_off..ext_off + name_bytes.len()].copy_from_slice(name_bytes);
        hc[8..16].copy_from_slice(&(ext_off as u64).to_be_bytes());
        hc[16..20].copy_from_slice(&(name_bytes.len() as u32).to_be_bytes());

        self.header_cluster = hc;
        self.backing = Some(BackingRef {
            name: name.to_string(),
            format: format.map(|s| s.to_string()),
        });
        self.header_dirty = true;
        // Drop the open backing image; callers reopen as needed.
        self.backing_image = None;
        Ok(())
    }

    /// Open the backing chain of this image for reading and hand it over
    /// (used by rebase, which needs the old chain while comparing).
    pub fn take_backing_image(&mut self) -> Option<Box<dyn DiskImage>> {
        self.backing_image.take()
    }

    /// Guest clusters allocated in this image's own layer (any non-zero L2
    /// entry: data, zero-flag or compressed).
    pub fn for_each_own_cluster(
        &mut self,
        f: &mut dyn FnMut(&mut Qcow2Image, u64) -> Result<()>,
    ) -> Result<()> {
        for l1_index in 0..self.l1.len() as u64 {
            let l2_host = self.l1[l1_index as usize] & OFFSET_MASK;
            if l2_host == 0 {
                continue;
            }
            for l2_index in self.l2_allocated_indexes(l2_host)? {
                let guest_cluster = l1_index * self.l2_entries() + l2_index;
                f(self, guest_cluster)?;
            }
        }
        Ok(())
    }

    /// Install a snapshot's L1 as the active view, in memory only. Used for
    /// reading a snapshot without modifying the image (convert -l).
    pub fn install_snapshot_view(&mut self, name: &str) -> Result<SnapshotInfo> {
        let (info, l1, disk_size) = self.load_snapshot_l1(name)?;
        self.l1 = l1;
        self.l1_size = self.l1.len() as u32;
        if let Some(disk_size) = disk_size {
            self.size = disk_size;
        }
        Ok(info)
    }

    /// Grow the image to `new_size`, extending the L1 table as needed.
    /// Shrinking is refused (vmlab never shrinks).
    pub fn resize_grow(&mut self, new_size: u64) -> Result<()> {
        self.check_writable()?;
        if new_size < self.size {
            return Err(Error::Unsupported(format!(
                "shrinking a qcow2 image from {} to {new_size} bytes is not supported",
                self.size
            )));
        }
        if new_size == self.size {
            return Ok(());
        }
        self.ensure_l1_covers(new_size)?;
        self.size = new_size;
        self.header_dirty = true;
        Ok(())
    }

    /// Ensure the active L1 (memory and on-disk region) has at least
    /// `needed` entries.
    pub(crate) fn ensure_l1_covers(&mut self, end_offset: u64) -> Result<()> {
        let needed = (self.l1_index_of(end_offset - 1) + 1).max(1) as u32;
        if needed <= self.l1_size {
            return Ok(());
        }
        self.ensure_l1_size(needed)
    }
}

impl DiskImage for Qcow2Image {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| Error::Unsupported("read overflows u64".into()))?;
        if end > self.size {
            return Err(Error::Unsupported(format!(
                "read beyond end of image {} (offset {offset}, len {})",
                self.path.display(),
                buf.len()
            )));
        }
        self.read_bytes(offset, buf)
    }

    fn read_into(
        &mut self,
        offset: u64,
        len: usize,
        target: &mut dyn crate::ReadTarget,
    ) -> Result<()> {
        crate::check_read(offset, len, self.size)?;
        self.read_target(offset, len, target)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.check_writable()?;
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| Error::Unsupported("write overflows u64".into()))?;
        if end > self.size {
            return Err(Error::Unsupported(format!(
                "write beyond end of image {} (offset {offset}, len {})",
                self.path.display(),
                buf.len()
            )));
        }
        self.write_bytes(offset, buf)
    }

    fn flush(&mut self) -> Result<()> {
        if !self.read_only {
            self.check_writable()?;
            self.flush_metadata()?;
            self.file.sync_all()?;
        }
        Ok(())
    }

    fn discard(&mut self, offset: u64, len: u64) -> Result<()> {
        self.check_writable()?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::Unsupported("discard overflows u64".into()))?;
        if end > self.size {
            return Err(Error::Unsupported("discard beyond end of image".into()));
        }
        // Discard is a hint. Keep partially covered clusters: freeing one
        // would destroy live filesystem blocks outside the requested range.
        let first = offset.div_ceil(self.cluster_size);
        let last = end / self.cluster_size;
        for cluster in first..last {
            let cluster_off = cluster * self.cluster_size;
            let l1_index = self.l1_index_of(cluster_off) as usize;
            let l2_index = self.l2_index_of(cluster_off);
            if l1_index < self.l1.len() {
                let l2_host = self.cow_l2_if_shared(l1_index)?;
                if l2_host != 0 {
                    let entry = self.l2_entry(l2_host, l2_index)?;
                    if entry & COMPRESSED_BIT != 0 {
                        self.adjust_compressed_refcounts(entry, -1)?;
                        self.set_l2_entry(l2_host, l2_index, 0)?;
                    } else {
                        let host = entry & OFFSET_MASK;
                        if host != 0 {
                            self.dec_refcount(host / self.cluster_size)?;
                        }
                        self.set_l2_entry(l2_host, l2_index, 0)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn snapshots(&self) -> Result<Vec<SnapshotInfo>> {
        self.list_snapshots()
    }

    fn snapshot_create(
        &mut self,
        name: &str,
        vm_state_size: u64,
        vm_clock_nsec: u64,
    ) -> Result<()> {
        let result = self.snapshot_create_impl(name, vm_state_size, vm_clock_nsec);
        self.finish_snapshot_change(result)
    }

    fn snapshot_apply(&mut self, name: &str) -> Result<SnapshotInfo> {
        let result = self.snapshot_apply_impl(name);
        self.finish_snapshot_change(result)
    }

    fn snapshot_delete(&mut self, name: &str) -> Result<()> {
        let result = self.snapshot_delete_impl(name);
        self.finish_snapshot_change(result)
    }

    fn write_vmstate(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.check_writable()?;
        let base = self.vmstate_base();
        let offset = base
            .checked_add(offset)
            .ok_or_else(|| Error::Unsupported("vmstate offset overflows u64".into()))?;
        self.ensure_l1_covers(offset + buf.len() as u64)?;
        self.write_bytes(offset, buf)
    }

    fn read_vmstate(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let base = self.vmstate_base();
        let offset = base
            .checked_add(offset)
            .ok_or_else(|| Error::Unsupported("vmstate offset overflows u64".into()))?;
        self.read_bytes(offset, buf)
    }
}

/// Closing an image writes its cached metadata. Data clusters are written at
/// once, but the L2 and refcount updates that map them wait in the caches
/// until a flush. Without this, an owner that drops the image without
/// `flush` leaves the data in the file but unmapped (VML-107).
///
/// This is best effort, like `BufWriter`: errors leave the dirty marker set.
/// Call [`DiskImage::flush`] to see errors and for durability.
impl Drop for Qcow2Image {
    fn drop(&mut self) {
        if !self.read_only && !self.poisoned {
            let _ = self.close_clean();
        }
    }
}

fn checked_file_range(file_len: u64, offset: u64, len: u64) -> Result<()> {
    if len != 0 && offset.checked_add(len).is_none_or(|end| end > file_len) {
        return Err(Error::Format {
            format: "qcow2",
            reason: "metadata range exceeds file length".into(),
        });
    }
    Ok(())
}

fn l1_size_as_clusters(l1_size: u32, cluster_size: u64) -> u64 {
    ((l1_size as u64) * 8).div_ceil(cluster_size).max(1)
}

fn parse_backing_format(header_cluster: &[u8], header_length: usize) -> Option<String> {
    let mut off = header_length;
    let limit = header_cluster.len();
    while off + 8 <= limit {
        let et = u32::from_be_bytes(header_cluster[off..off + 4].try_into().unwrap());
        let el = u32::from_be_bytes(header_cluster[off + 4..off + 8].try_into().unwrap()) as usize;
        if et == EXT_END {
            return None;
        }
        if et == EXT_BACKING_FORMAT {
            let data = &header_cluster[off + 8..(off + 8 + el).min(limit)];
            return Some(String::from_utf8_lossy(data).into_owned());
        }
        off += 8 + el.div_ceil(8) * 8;
    }
    None
}

/// Relative backing paths resolve against the image's directory; absolute
/// paths are taken as-is.
pub(crate) fn resolve_backing(image_path: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        image_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(p)
    }
}

pub(crate) fn probe_format(path: &Path) -> Result<Format> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    match f.read_exact(&mut magic) {
        Ok(()) if u32::from_be_bytes(magic) == QCOW_MAGIC => Ok(Format::Qcow2),
        Ok(()) => Ok(Format::Raw),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(Format::Raw),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_snapshot_does_not_walk_unrelated_active_l2() {
        let (_dir, mut img) = testsupport::blank(1 << 30);
        img.write_at(0, &[0x11; 512]).unwrap();
        img.snapshot_create("early", 0, 0).unwrap();
        let far = img.l2_span();
        img.write_at(far, &[0x22; 512]).unwrap();
        img.flush().unwrap();
        let unrelated = img.l1[1] & OFFSET_MASK;
        img.l2_cache = TableCache::new(64);
        img.snapshot_delete("early").unwrap();
        assert!(!img.l2_cache.map.contains_key(&unrelated));
        let mut byte = [0];
        img.read_at(far, &mut byte).unwrap();
        assert_eq!(byte, [0x22]);
    }

    #[test]
    fn snapshot_namespace_rejects_ids_and_skips_numeric_names() {
        let (_dir, mut img) = testsupport::blank(8 << 20);
        img.snapshot_create("2", 0, 0).unwrap();
        img.snapshot_create("second", 0, 0).unwrap();
        let entries = img.snapshots().unwrap();
        assert_eq!(entries[0].id, "1");
        assert_eq!(entries[1].id, "3");
        assert!(img.snapshot_create("1", 0, 0).is_err());
        assert!(!img.poisoned, "validation must not poison the image");
        assert_eq!(img.snapshot_apply("2").unwrap().name, "2");
        img.snapshot_delete("3").unwrap();
        assert_eq!(img.snapshots().unwrap().len(), 1);
    }

    #[test]
    fn failed_snapshot_changes_poison_all_write_entry_points() {
        for operation in ["create", "apply", "delete"] {
            let (_dir, mut img) = testsupport::blank(8 << 20);
            img.write_at(0, &[0x35; 512]).unwrap();
            img.snapshot_create("saved", 0, 0).unwrap();
            fail_writes(&mut img);
            let result = match operation {
                "create" => img.snapshot_create("new", 0, 0),
                "apply" => img.snapshot_apply("saved").map(|_| ()),
                _ => img.snapshot_delete("saved"),
            };
            assert!(result.is_err(), "{operation}");
            assert!(img.poisoned, "{operation}");
            allow_writes(&mut img);
            assert!(img.write_at(0, &[1]).is_err());
            assert!(img.discard(0, img.cluster_size).is_err());
            assert!(img.write_vmstate(0, &[1]).is_err());
            assert!(img.snapshot_create("retry", 0, 0).is_err());
            assert!(img.flush().is_err());
        }
    }

    #[test]
    fn snapshot_reads_keep_inode_and_seek_position() {
        let (dir, mut img) = testsupport::blank(8 << 20);
        img.snapshot_create("original", 0, 0).unwrap();
        img.file.seek(SeekFrom::Start(123)).unwrap();
        std::fs::rename(&img.path, dir.path().join("moved.qcow2")).unwrap();
        std::fs::write(&img.path, b"replacement").unwrap();
        assert_eq!(img.snapshots().unwrap()[0].name, "original");
        assert_eq!(img.file.stream_position().unwrap(), 123);
    }

    #[test]
    fn hostile_metadata_lengths_fail_before_allocation() {
        let (_dir, mut img) = testsupport::blank(8 << 20);
        img.file.write_all_at(&u32::MAX.to_be_bytes(), 36).unwrap();
        assert!(Qcow2Image::open(&img.path, true).is_err());
        img.file.write_all_at(&1u32.to_be_bytes(), 36).unwrap();
        img.file.write_all_at(&u32::MAX.to_be_bytes(), 56).unwrap();
        assert!(Qcow2Image::open(&img.path, true).is_err());
    }

    #[test]
    fn compressed_cluster_requires_complete_exact_length_stream() {
        use std::io::Write;
        let (_dir, mut img) = testsupport::blank(8 << 20);
        let size = img.cluster_size as usize;
        for len in [size - 1, size, size + 1] {
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&vec![0x5a; len]).unwrap();
            let mut bytes = encoder.finish().unwrap();
            bytes.resize(512, 0);
            let offset = img.file.metadata().unwrap().len().div_ceil(512) * 512;
            img.file.write_all_at(&bytes, offset).unwrap();
            let mut out = vec![0; size];
            assert_eq!(img.read_compressed_cluster(COMPRESSED_BIT | offset, &mut out).is_ok(), len == size);
        }
    }

    #[test]
    fn crash_writer_child() {
        let Some(path) = std::env::var_os("VMLAB_QCOW_CRASH_IMAGE") else {
            return;
        };
        let mut img = Qcow2Image::open(Path::new(&path), false).unwrap();
        img.write_at(img.cluster_size, &vec![0x72; img.cluster_size as usize])
            .unwrap();
        img.flush().unwrap();
        panic!("child did not stop at the crash checkpoint");
    }

    #[test]
    fn killed_metadata_writer_recovers_before_allocating() {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        for phase in ["refcounts", "mappings"] {
            let (_dir, mut img) = testsupport::blank(8 << 20);
            let path = img.path.clone();
            let cluster = img.cluster_size;
            img.write_at(0, &vec![0x31; cluster as usize]).unwrap();
            img.snapshot_create("before", 0, 0).unwrap();
            drop(img);
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "qcow2::tests::crash_writer_child", "--nocapture"])
                .env("VMLAB_QCOW_CRASH_IMAGE", &path)
                .env("VMLAB_QCOW_CRASH_PHASE", phase)
                .stdout(Stdio::piped())
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout).lines() {
                    if line.unwrap().contains("QCOW_CHECKPOINT") {
                        let _ = tx.send(());
                        break;
                    }
                }
            });
            let checkpoint = rx.recv_timeout(Duration::from_secs(30));
            child.kill().unwrap();
            let status = child.wait().unwrap();
            reader.join().unwrap();
            checkpoint.expect("child reached metadata boundary");
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(9));

            let read_only = Qcow2Image::open(&path, true).unwrap();
            assert!(read_only.is_dirty());
            drop(read_only);
            let mut recovered = Qcow2Image::open(&path, false).unwrap();
            let mut bytes = vec![0; cluster as usize];
            recovered.read_at(0, &mut bytes).unwrap();
            assert_eq!(bytes, vec![0x31; cluster as usize]);
            recovered.read_at(cluster, &mut bytes).unwrap();
            assert_eq!(
                bytes,
                vec![if phase == "mappings" { 0x72 } else { 0 }; cluster as usize]
            );
            recovered
                .write_at(2 * cluster, &vec![0x93; cluster as usize])
                .unwrap();
            recovered.flush().unwrap();
            recovered.read_at(0, &mut bytes).unwrap();
            assert_eq!(bytes, vec![0x31; cluster as usize]);
            recovered.snapshot_apply("before").unwrap();
            recovered.read_at(0, &mut bytes).unwrap();
            assert_eq!(bytes, vec![0x31; cluster as usize]);
            drop(recovered);
            assert!(!Qcow2Image::open(&path, true).unwrap().is_dirty());
            let check = Command::new("qemu-img")
                .args(["check", "-f", "qcow2"])
                .arg(&path)
                .output()
                .expect("qemu-img is required for crash recovery test");
            assert!(
                check.status.success(),
                "{}{}",
                String::from_utf8_lossy(&check.stdout),
                String::from_utf8_lossy(&check.stderr)
            );
        }
    }

    #[test]
    fn dirty_marker_survives_flush_and_failed_close() {
        let (_dir, mut img) = testsupport::blank(8 << 20);
        let path = img.path.clone();
        img.write_at(0, &[0x51; 512]).unwrap();
        img.flush().unwrap();
        assert!(Qcow2Image::open(&path, true).unwrap().is_dirty());
        img.write_at(img.cluster_size, &[0x52; 512]).unwrap();
        fail_writes(&mut img);
        drop(img);
        assert!(Qcow2Image::open(&path, true).unwrap().is_dirty());
        drop(Qcow2Image::open(&path, false).unwrap());
        assert!(!Qcow2Image::open(&path, true).unwrap().is_dirty());
    }

    #[test]
    fn read_extents_coalesce_adjacent_clusters_and_stop_at_holes() {
        let (_dir, mut img) = testsupport::blank(4 << 20);
        let (source, len) = img.read_extent(512, 1 << 20).unwrap();
        assert!(matches!(source, ReadSource::Backing));
        assert_eq!(len, 1 << 20);
        let cluster = img.cluster_size as usize;
        img.write_at(0, &vec![0x37; cluster * 2]).unwrap();
        let (source, len) = img.read_extent(512, cluster * 3).unwrap();
        assert!(matches!(source, ReadSource::File(_)));
        assert_eq!(len, cluster * 2 - 512);
        let (source, len) = img.read_extent((cluster * 2) as u64, cluster * 2).unwrap();
        assert!(matches!(source, ReadSource::Backing));
        assert_eq!(len, cluster * 2);
    }

    #[test]
    fn resolve_backing_relative_and_absolute() {
        let img = Path::new("/store/machines/m1/disk.qcow2");
        assert_eq!(
            resolve_backing(img, "../../disk.qcow2"),
            PathBuf::from("/store/machines/m1/../../disk.qcow2")
        );
        assert_eq!(
            resolve_backing(img, "/store/base.qcow2"),
            PathBuf::from("/store/base.qcow2")
        );
    }

    #[test]
    fn backing_format_extension_parsing() {
        let mut hc = vec![0u8; 512];
        let mut off = 112;
        hc[off..off + 4].copy_from_slice(&EXT_BACKING_FORMAT.to_be_bytes());
        hc[off + 4..off + 8].copy_from_slice(&5u32.to_be_bytes());
        hc[off + 8..off + 13].copy_from_slice(b"qcow2");
        off += 8 + 8;
        hc[off..off + 4].copy_from_slice(&EXT_END.to_be_bytes());
        assert_eq!(parse_backing_format(&hc, 112).as_deref(), Some("qcow2"));
        assert_eq!(parse_backing_format(&vec![0u8; 512], 112), None);
    }

    #[test]
    fn vmstate_base_is_first_non_guest_l1_entry() {
        // 10 MiB disk, 64 KiB clusters: L1 span is 512 MiB, so the vmstate
        // starts at 512 MiB (observed from QEMU's savevm).
        let (_dir, mut img) = testsupport::blank(10 * 1024 * 1024);
        assert_eq!(img.vmstate_base(), 512 * 1024 * 1024);
        img.size = 512 * 1024 * 1024;
        assert_eq!(img.vmstate_base(), 512 * 1024 * 1024);
        img.size = 600 * 1024 * 1024;
        assert_eq!(img.vmstate_base(), 1024 * 1024 * 1024);
    }

    #[test]
    fn freed_clusters_are_reused_lowest_first() {
        let (_dir, mut img) = testsupport::blank(8 * 1024 * 1024);
        let a = img.alloc_cluster().unwrap();
        let b = img.alloc_cluster().unwrap();
        let c = img.alloc_cluster().unwrap();
        assert_eq!(b, a + img.cluster_size);
        img.dec_refcount(a / img.cluster_size).unwrap();
        img.dec_refcount(c / img.cluster_size).unwrap();
        img.flush().unwrap();
        // The lowest hole goes first, then the next.
        assert_eq!(img.alloc_cluster().unwrap(), a);
        assert_eq!(img.alloc_cluster().unwrap(), c);
    }

    #[test]
    fn blank_images_with_same_size_have_distinct_paths() {
        let (_first_dir, mut first) = testsupport::blank(1024 * 1024);
        let (_second_dir, mut second) = testsupport::blank(1024 * 1024);
        assert_ne!(first.path, second.path);
        first.write_at(0, &[0x51; 512]).unwrap();
        second.write_at(0, &[0xa2; 512]).unwrap();
        first.flush().unwrap();
        second.flush().unwrap();
        let mut reopened = Qcow2Image::open(&first.path, true).unwrap();
        let mut bytes = [0; 512];
        reopened.read_at(0, &mut bytes).unwrap();
        assert_eq!(bytes, [0x51; 512]);
    }

    #[test]
    fn allocation_rescans_new_refcount_coverage() {
        let (_dir, mut img) = testsupport::blank(8 * 1024 * 1024);
        let covered = img.refblock_entries();
        let first = img.alloc_clusters_contiguous(covered).unwrap();
        assert!(first > 0);
        img.flush().unwrap();
        assert_eq!(img.get_refcount(first / img.cluster_size).unwrap(), 1);
        assert_eq!(
            img.get_refcount(first / img.cluster_size + covered - 1)
                .unwrap(),
            1
        );
    }

    #[test]
    fn holes_from_a_previous_session_are_reused_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reuse.qcow2");
        Qcow2Image::create(&path, 8 * 1024 * 1024, 16, None)
            .expect("create")
            .flush()
            .expect("flush create");

        // Fill a region, then free it: snapshot, overwrite (the snapshot
        // pins the old clusters), delete the snapshot.
        {
            let mut img = Qcow2Image::open(&path, false).unwrap();
            img.write_at(0, &vec![0xa5; 256 * 1024]).unwrap();
            img.flush().unwrap();
            img.snapshot_create("s", 0, 0).unwrap();
            img.write_at(0, &vec![0x5a; 256 * 1024]).unwrap();
            img.snapshot_delete("s").unwrap();
            img.flush().unwrap();
        }
        let len_freed = std::fs::metadata(&path).unwrap().len();

        // A fresh open must see the holes in the refblocks, not a hint
        // past the highest refcounted cluster.
        {
            let mut img = Qcow2Image::open(&path, false).unwrap();
            img.write_at(2 * 1024 * 1024, &vec![0x3c; 256 * 1024])
                .unwrap();
            img.flush().unwrap();
        }
        let len_after = std::fs::metadata(&path).unwrap().len();
        assert!(
            len_after <= len_freed + img_cluster_size(&path),
            "reopen did not reuse freed holes: {len_freed} -> {len_after}"
        );
    }

    /// Make every later write of `img` fail (EBADF) by swapping in a
    /// read-only handle, as a full or failing host disk would.
    fn fail_writes(img: &mut Qcow2Image) {
        img.file = File::open(&img.path).expect("open read-only handle");
    }

    fn allow_writes(img: &mut Qcow2Image) {
        img.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&img.path)
            .expect("open read-write handle");
    }

    fn read_back(path: &Path, offset: u64, len: usize) -> Vec<u8> {
        let mut reopened = Qcow2Image::open(path, true).expect("reopen read-only");
        let mut buf = vec![0u8; len];
        reopened.read_at(offset, &mut buf).expect("read back");
        buf
    }

    #[test]
    fn failed_metadata_flush_keeps_tables_dirty_for_a_retry() {
        let (_dir, mut img) = testsupport::blank(8 * 1024 * 1024);
        let cluster = img.cluster_size as usize;
        // New clusters: the data goes to the file now, the L2 entry and the
        // refcounts wait in the caches.
        img.write_at(0, &vec![0x6b; cluster * 2]).unwrap();
        assert!(!img.l2_cache.dirty().is_empty());
        assert!(!img.refblock_cache.dirty().is_empty());

        fail_writes(&mut img);
        assert!(img.flush().is_err(), "flush must fail without writes");
        assert!(
            !img.l2_cache.dirty().is_empty() && !img.refblock_cache.dirty().is_empty(),
            "a failed flush marked the unwritten tables clean"
        );

        allow_writes(&mut img);
        img.flush().expect("retried flush");
        assert!(img.l2_cache.dirty().is_empty());
        assert!(img.refblock_cache.dirty().is_empty());
        // Read through a second handle while `img` is alive, so its Drop
        // cannot write the tables for the retry.
        assert_eq!(
            read_back(&img.path, 0, cluster * 2),
            vec![0x6b; cluster * 2]
        );
        let mut check = Qcow2Image::open(&img.path, true).unwrap();
        assert_eq!(check.get_refcount(0).unwrap(), 1, "header cluster");
        let (source, _) = check.read_extent(0, cluster).unwrap();
        let ReadSource::File(host) = source else {
            panic!("guest cluster 0 is not mapped after the retry");
        };
        assert_eq!(check.get_refcount(host / img.cluster_size).unwrap(), 1);
    }

    #[test]
    fn failed_eviction_write_keeps_the_evicted_table_dirty() {
        // Two L2 tables: with 64 KiB clusters one table maps 512 MiB.
        let (_dir, mut img) = testsupport::blank(1024 * 1024 * 1024);
        let cluster = img.cluster_size as usize;
        let far = 600 * 1024 * 1024;
        img.write_at(0, &vec![0x11; cluster]).unwrap();
        img.write_at(far, &vec![0x22; cluster]).unwrap();
        img.flush().unwrap();

        // One L2 table fits. Map a new cluster in the first table, so it is
        // dirty, then load the second one with writes failing: the dirty
        // table is evicted and its write fails.
        // Every table is clean after the flush, so a new cache loses nothing.
        img.l2_cache = TableCache::new(1);
        let second = 4 * cluster as u64;
        img.write_at(second, &vec![0x33; cluster]).unwrap();
        fail_writes(&mut img);
        let mut buf = vec![0u8; cluster];
        assert!(
            img.read_at(far, &mut buf).is_err(),
            "the evicted table's write must fail"
        );

        allow_writes(&mut img);
        img.flush().expect("retried flush");
        assert!(img.l2_cache.dirty().is_empty());
        assert_eq!(read_back(&img.path, second, cluster), vec![0x33; cluster]);
        assert_eq!(read_back(&img.path, far, cluster), vec![0x22; cluster]);
    }

    fn img_cluster_size(path: &Path) -> u64 {
        Qcow2Image::open(path, true).unwrap().cluster_size_bytes()
    }
}

#[cfg(test)]
mod testsupport {
    use super::*;

    /// Keep the directory alive for the lifetime of the image.
    pub(super) fn blank(size: u64) -> (tempfile::TempDir, Qcow2Image) {
        let dir = tempfile::tempdir().expect("create image directory");
        let image =
            Qcow2Image::create(&dir.path().join("blank.qcow2"), size, 16, None).expect("create");
        (dir, image)
    }
}
