/// A byte range into a single source file.
///
/// Compilation units are single files in v1 (PRD §3.9), so a span does not
/// carry a file id; the consumer (CLI, LSP) knows which source it compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub const DUMMY: Span = Span { lo: 0, hi: 0 };

    pub fn new(lo: u32, hi: u32) -> Span {
        Span { lo, hi }
    }

    /// Offset into a multi-file compilation's global address space.
    pub fn shifted(self, base: u32) -> Span {
        Span {
            lo: self.lo + base,
            hi: self.hi + base,
        }
    }

    /// Smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        Span {
            lo: self.lo.min(other.lo),
            hi: self.hi.max(other.hi),
        }
    }

    pub fn len(self) -> u32 {
        self.hi - self.lo
    }

    pub fn is_empty(self) -> bool {
        self.lo == self.hi
    }
}
