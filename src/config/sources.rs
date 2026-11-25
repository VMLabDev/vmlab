//! Which file a span is in.
//!
//! A document is its top file plus every file it imports, and WCL gives a
//! span as byte offsets into the file that declares the item. The model keeps
//! a span as two offsets and nothing else, and it outlives the document, so
//! an offset has to say which file it counts in by itself. Here every file of
//! a document gets a range of one address space: the top file starts at 0,
//! and each imported file starts one byte past the end of the file before it.
//! [`block::Reader`](super::block::Reader) hands out spans in that space
//! ([`base`]), and a loader turns each issue back into a position in its own
//! file before anything renders it ([`Sources::place`]).
//!
//! The order of the files is that of a walk over the document's blocks, so
//! two opens of the same files agree on every base. The validation pass
//! relies on that: it reports against a second open of the lab file.

use std::sync::Arc;

use miette::{NamedSource, SourceSpan};
use wcl_lang::{Block, Document};

use super::Issue;

/// One file of a document.
struct File {
    name: String,
    text: Arc<str>,
    /// Where the file starts in the document's address space.
    base: usize,
}

/// The files of one document, the top file first.
pub(crate) struct Sources {
    files: Vec<File>,
}

impl Sources {
    pub(crate) fn of(doc: &Document) -> Self {
        let mut sources = Self { files: Vec::new() };
        // The top file is first whether or not it declares a block.
        sources.add(doc.source());
        for block in doc.blocks() {
            sources.walk(&block);
        }
        sources
    }

    fn walk(&mut self, block: &Block) {
        self.add(&block.named_source());
        for child in block.blocks() {
            self.walk(&child);
        }
    }

    fn add(&mut self, source: &NamedSource<Arc<str>>) {
        if self.index_of(source).is_none() {
            self.files.push(File {
                name: source.name().to_string(),
                text: source.inner().clone(),
                base: self.end(),
            });
        }
    }

    /// One past the last address of the last file, with the gap that keeps a
    /// position at the end of one file apart from the start of the next.
    fn end(&self) -> usize {
        self.files.last().map_or(0, |f| f.base + f.text.len() + 1)
    }

    fn index_of(&self, source: &NamedSource<Arc<str>>) -> Option<usize> {
        self.files
            .iter()
            .position(|f| f.name == source.name() && same_text(&f.text, source.inner()))
    }

    /// The base of the file `source` is, or an address in no file for a
    /// source this document does not hold.
    pub(crate) fn base_of(&self, source: &NamedSource<Arc<str>>) -> usize {
        match self.index_of(source) {
            Some(i) => self.files[i].base,
            None => self.end(),
        }
    }

    /// The file an address is in, and the offset in that file.
    pub(crate) fn locate(&self, address: usize) -> Option<(usize, usize)> {
        self.files
            .iter()
            .enumerate()
            .find(|(_, f)| address >= f.base && address <= f.base + f.text.len())
            .map(|(i, f)| (i, address - f.base))
    }

    /// Every file's text as it is, the top file first.
    pub(crate) fn texts(&self) -> Vec<String> {
        self.files.iter().map(|f| f.text.to_string()).collect()
    }

    /// Every file's text with nothing left but its line breaks, for a file
    /// family whose text may not be shown.
    pub(crate) fn blank_texts(&self) -> Vec<String> {
        self.files.iter().map(|f| blank(&f.text)).collect()
    }

    /// Give each issue its position in its own file.
    ///
    /// An issue in the top file keeps its span, which the caller renders
    /// against the top file. An issue in an imported file gets the span in
    /// that file and the file itself, as `shown` has its text (one entry per
    /// file, as [`Sources::texts`] orders them). An address in no file is
    /// not a position, and the issue is left without one.
    pub(crate) fn place(&self, issues: &mut [Issue], shown: &[String]) {
        for issue in issues {
            let Some(span) = issue.span else { continue };
            match self.locate(span.offset()) {
                Some((0, _)) => {}
                Some((i, offset)) => {
                    let file = &self.files[i];
                    let len = span.len().min(file.text.len() - offset);
                    issue.span = Some(SourceSpan::new(offset.into(), len));
                    let text = shown.get(i).cloned().unwrap_or_else(|| blank(&file.text));
                    issue.origin = Some(NamedSource::new(&file.name, text));
                }
                None => issue.span = None,
            }
        }
    }
}

/// What to add to a span WCL gives for something `block` declares, to get
/// its place in the document's address space: 0 for the top file.
pub(crate) fn base(block: &Block) -> usize {
    let doc = block.doc();
    let source = block.named_source();
    let top = doc.source();
    if source.name() == top.name() && same_text(source.inner(), top.inner()) {
        return 0;
    }
    Sources::of(doc).base_of(&source)
}

fn same_text(a: &Arc<str>, b: &Arc<str>) -> bool {
    Arc::ptr_eq(a, b) || a == b
}

/// `text` with nothing left but its line breaks. Byte offsets are kept.
pub(crate) fn blank(text: &str) -> String {
    text.bytes()
        .map(|b| if b == b'\n' { '\n' } else { ' ' })
        .collect()
}
