//! The FILE_NOTIFY_INFORMATION chain `ReadDirectoryChangesW` writes, parsed
//! from its bytes without trusting any offset in it (code review F60).
//!
//! The layout is fixed: `NextEntryOffset`, `Action` and `FileNameLength` as
//! native `u32`s, then `FileNameLength` bytes of UTF-16, unterminated. Each
//! record is bounded by the bytes the call reported (and by the buffer), the
//! last one included: its header is the 12 bytes before the name, not the
//! padded struct size, so a short final name is not dropped, and its name
//! must end inside the reported length. A next offset that is not
//! DWORD-aligned, or that leaves the reported region, ends the walk.

/// Bytes before `FileName` in a record.
const HEADER: usize = 12;

/// `(action, root-relative path)` for each record in the first `len` bytes
/// of `bytes`, `\` turned into `/`; records with an empty name are skipped.
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn parse(bytes: &[u8], len: usize) -> Vec<(u32, String)> {
    let len = len.min(bytes.len());
    let bytes = &bytes[..len];
    let field = |at: usize| -> u32 {
        u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let mut out = Vec::new();
    let mut offset = 0usize;
    loop {
        let Some(name_at) = offset.checked_add(HEADER) else {
            return out;
        };
        if name_at > len {
            return out;
        }
        let next = field(offset) as usize;
        let action = field(offset + 4);
        let name_len = field(offset + 8) as usize;
        let Some(name_end) = name_at.checked_add(name_len) else {
            return out;
        };
        if name_end > len || !name_len.is_multiple_of(2) {
            return out;
        }
        let (pairs, _) = bytes[name_at..name_end].as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|&c| u16::from_ne_bytes(c)).collect();
        let rel = String::from_utf16_lossy(&units).replace('\\', "/");
        if !rel.is_empty() {
            out.push((action, rel));
        }
        if next == 0 || !next.is_multiple_of(4) {
            return out;
        }
        match offset.checked_add(next) {
            Some(o) if o < len => offset = o,
            _ => return out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn record(next: u32, action: u32, name: &str, pad_to: usize) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut b = Vec::new();
        b.extend(next.to_ne_bytes());
        b.extend(action.to_ne_bytes());
        b.extend(((units.len() * 2) as u32).to_ne_bytes());
        for u in units {
            b.extend(u.to_ne_bytes());
        }
        b.resize(b.len().max(pad_to), 0);
        b
    }

    #[test]
    fn a_chain_is_read_to_its_last_record() {
        let mut bytes = record(16, 1, "a\\b", 16);
        // Last record: a one-character name, 14 bytes reported, no padding.
        bytes.extend(record(0, 2, "c", 0));
        let len = bytes.len();
        assert_eq!(len, 30);
        bytes.resize(64, 0xff);
        assert_eq!(
            parse(&bytes, len),
            vec![(1, "a/b".to_string()), (2, "c".to_string())]
        );
    }

    #[test]
    fn a_name_past_the_reported_length_ends_the_walk() {
        let mut bytes = record(16, 1, "ok", 16);
        bytes.extend(record(0, 2, "longer-name", 0));
        let len = bytes.len() - 2;
        assert_eq!(parse(&bytes, len), vec![(1, "ok".to_string())]);
    }

    #[test]
    fn hostile_offsets_end_the_walk() {
        // Misaligned next.
        let mut bytes = record(18, 1, "x", 18);
        bytes.extend(record(0, 2, "y", 0));
        assert_eq!(parse(&bytes, bytes.len()), vec![(1, "x".to_string())]);
        // Next past the end, and a length claimed past the buffer.
        let bytes = record(4096, 1, "x", 16);
        assert_eq!(parse(&bytes, 1 << 20), vec![(1, "x".to_string())]);
        // A name length near u32::MAX.
        let mut bytes = record(0, 1, "", 16);
        bytes[8..12].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(parse(&bytes, bytes.len()).is_empty());
        // Shorter than a header.
        assert!(parse(&[0; 11], 11).is_empty());
    }
}
