//! Multi-file source mapping. Spans stay plain `{lo, hi}` byte offsets:
//! each file of a multi-file compilation occupies a disjoint range of a
//! virtual address space (`[base, base + len]`), and consumers map a
//! global offset back to (file, local offset) here.

/// One file's slot in the global span address space.
#[derive(Debug, Clone)]
pub struct SourceFileInfo {
    /// Display path (as written/resolved — used in diagnostics and
    /// stack traces).
    pub path: String,
    /// First global offset of this file's spans.
    pub base: u32,
    /// Source length in bytes.
    pub len: u32,
}

/// Map from global span offsets to files. Files are stored in ascending
/// `base` order; a single-file compilation has one entry at base 0.
#[derive(Debug, Clone, Default)]
pub struct SourceMap {
    pub files: Vec<SourceFileInfo>,
}

impl SourceMap {
    /// A single anonymous file covering `[0, len]`.
    pub fn single(path: impl Into<String>, len: u32) -> SourceMap {
        SourceMap {
            files: vec![SourceFileInfo {
                path: path.into(),
                base: 0,
                len,
            }],
        }
    }

    /// The file containing `offset`, with the offset made file-local.
    pub fn local(&self, offset: u32) -> Option<(&SourceFileInfo, u32)> {
        let idx = self
            .files
            .partition_point(|f| f.base <= offset)
            .checked_sub(1)?;
        let f = &self.files[idx];
        (offset <= f.base + f.len).then(|| (f, offset - f.base))
    }
}
