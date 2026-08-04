//! One WCL block → typed values (ADR-0006).
//!
//! Every place vmlab reads WCL — lab files, host configuration, guest OS
//! profiles, template store metadata — reads it through [`Reader`]. This
//! module owns typed field access, coercion, source spans and the issue
//! wording; the call sites keep only their field mappings. It knows how to
//! read a typed field from a block; it does not know what a lab, a profile,
//! a host config or a metadata file contains.
//!
//! Issues accumulate rather than abort, so one pass reports everything
//! wrong with a file. A getter returns `None` when the field is absent,
//! explicitly `none`, or malformed — a malformed one having pushed a
//! positioned issue on its way out. Nothing here takes a per-call-site
//! flag: if it did, this would be four extractors again with extra steps.
//!
//! Coercion is hand-written for now. ADR-0005 (the schema is reflected,
//! not restated) covers the projecting direction and will let the rules the
//! schema already states — `std.ByteSize` is a non-negative byte count,
//! `std.Duration` a nanosecond count — be read off the schema instead.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use miette::NamedSource;
use thiserror::Error;
use wcl_lang::{Block, Value};

use super::model::Span;
use super::{ConfigErrors, Issue, IssueList};

/// A value together with the span of the source text it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spanned<T> {
    pub value: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(value: T, span: Span) -> Self {
        Self { value, span }
    }

    /// Convert the value, keeping the span it came from.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Spanned<U> {
        Spanned {
            value: f(self.value),
            span: self.span,
        }
    }
}

/// Drop the span from an optional extraction:
/// `r.bool("nat").unspan().unwrap_or(false)`.
pub trait Unspan<T> {
    fn unspan(self) -> Option<T>;
}

impl<T> Unspan<T> for Option<Spanned<T>> {
    fn unspan(self) -> Option<T> {
        self.map(|s| s.value)
    }
}

// ---- the issue vocabulary --------------------------------------------------
//
// Every message the extractor can produce is built here, so the same mistake
// reads the same way whichever file it was made in.

/// Name a value's type for a diagnostic — "got a string", not "got Utf8(..)".
fn describe(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "a bool",
        Value::I8(_)
        | Value::I16(_)
        | Value::I32(_)
        | Value::I64(_)
        | Value::I128(_)
        | Value::Isize(_)
        | Value::U8(_)
        | Value::U16(_)
        | Value::U32(_)
        | Value::U64(_)
        | Value::U128(_)
        | Value::Usize(_) => "an integer",
        Value::F32(_) | Value::F64(_) => "a decimal",
        Value::Utf8(_) | Value::Ascii(_) | Value::Utf16(_) | Value::Utf32(_) => "a string",
        Value::Identifier(_) => "an identifier",
        Value::Symbol(_) => "a symbol",
        Value::None => "none",
        Value::Function(_) => "a function",
        Value::List(_) => "a list",
        Value::Tensor { .. } => "a tensor",
        Value::Variant { .. } => "a union variant",
        Value::Record { .. } => "a record",
        Value::DataPath { .. } => "a reference",
        Value::PendingUnit { .. } => "a unit literal without a type",
    }
}

fn wrong_type(name: &str, expected: &str, got: &Value) -> String {
    format!("`{name}` must be {expected}, got {}", describe(got))
}

fn out_of_range(name: &str, lo: i64, hi: i64, got: i64) -> String {
    format!("`{name}` must be between {lo} and {hi}, got {got}")
}

fn below_floor(name: &str, min: i64, got: i64) -> String {
    format!("`{name}` must be at least {min}, got {got}")
}

fn too_large(name: &str, got: i64) -> String {
    format!("`{name}` is too large: {got}")
}

fn must_be_one_of(name: &str, allowed: &str, got: &str) -> String {
    format!("`{name}` must be one of {allowed}, got `{got}`")
}

// ---- the extractor ---------------------------------------------------------

/// Span of a block, for a caller holding the block rather than a reader.
pub fn span_of(b: &Block) -> Span {
    let s = b.span();
    (s.start, s.end)
}

/// Typed, span-carrying, issue-accumulating access to one WCL block.
///
/// Build one per block; nested blocks get their own reader over the same
/// issue list (see [`Reader::issues`]).
pub struct Reader<'b, 'i> {
    block: &'b Block<'b>,
    issues: &'i mut IssueList,
}

impl<'b, 'i> Reader<'b, 'i> {
    pub fn new(block: &'b Block<'b>, issues: &'i mut IssueList) -> Self {
        Self { block, issues }
    }

    // ---- the block itself --------------------------------------------

    /// The block's kind (`vm`, `segment`, `profile`, …).
    pub fn kind(&self) -> &'b str {
        self.block.kind()
    }

    /// Span of the whole block — where an issue about the block as a unit
    /// (a missing required field, a contradiction between two fields) goes.
    pub fn span(&self) -> Span {
        span_of(self.block)
    }

    /// Whether the field is written at all. Distinguishes "unset, take the
    /// default" from "set to the value that happens to be the default".
    pub fn has(&self, name: &str) -> bool {
        self.block.field(name).is_some()
    }

    /// The block's first label, as a name. A block that needs a label and
    /// has none is an issue.
    pub fn label(&mut self) -> Option<String> {
        let kind = self.kind();
        match self.block.labels() {
            Ok(labels) => match labels.first() {
                Some(Value::Utf8(s)) | Some(Value::Ascii(s)) | Some(Value::Identifier(s)) => {
                    Some(s.clone())
                }
                _ => {
                    self.issue(format!("`{kind}` requires a name label"));
                    None
                }
            },
            Err(e) => {
                self.issue(format!("cannot evaluate `{kind}` label: {e}"));
                None
            }
        }
    }

    /// The child blocks, for the caller's own field mapping. Precise
    /// capturing keeps the iterator off this reader, so a caller can walk
    /// the children and report issues about them at the same time.
    pub fn children(&self) -> impl Iterator<Item = Block<'b>> + use<'b> {
        self.block.blocks()
    }

    // ---- issues -------------------------------------------------------

    /// Borrow the issue list — for a reader over a child block, or for a
    /// caller-specific check the extractor cannot express.
    pub fn issues(&mut self) -> &mut IssueList {
        self.issues
    }

    /// Report a problem with the block as a whole.
    pub fn issue(&mut self, message: impl Into<String>) {
        let span = self.span();
        self.issues.push(Issue::at(span, message));
    }

    /// Report a problem at a specific span — usually one a getter handed back.
    pub fn issue_at(&mut self, span: Span, message: impl Into<String>) {
        self.issues.push(Issue::at(span, message));
    }

    /// Report a required field that is not there.
    pub fn missing(&mut self, name: &str) {
        self.issue(format!("missing required field `{name}`"));
    }

    // ---- scalars ------------------------------------------------------

    /// The field's evaluated value. `None` for an absent field, an
    /// explicit `none`, or one whose expression fails to evaluate (which
    /// reports an issue).
    pub fn value(&mut self, name: &str) -> Option<Spanned<Value>> {
        let field = self.block.field(name)?;
        let span = (field.span().start, field.span().end);
        match field.value() {
            Ok(Value::None) => None,
            Ok(v) => Some(Spanned::new(v.clone(), span)),
            Err(e) => {
                self.issue_at(span, format!("cannot evaluate `{name}`: {e}"));
                None
            }
        }
    }

    pub fn string(&mut self, name: &str) -> Option<Spanned<String>> {
        let v = self.value(name)?;
        match v.value {
            Value::Utf8(s) | Value::Ascii(s) | Value::Identifier(s) => {
                Some(Spanned::new(s, v.span))
            }
            other => {
                self.issue_at(v.span, wrong_type(name, "a string", &other));
                None
            }
        }
    }

    /// A string field that must be there.
    pub fn required_string(&mut self, name: &str) -> Option<Spanned<String>> {
        self.required(name, Self::string)
    }

    pub fn bool(&mut self, name: &str) -> Option<Spanned<bool>> {
        let v = self.value(name)?;
        match v.value {
            Value::Bool(b) => Some(Spanned::new(b, v.span)),
            other => {
                self.issue_at(v.span, wrong_type(name, "a bool", &other));
                None
            }
        }
    }

    /// An integer of any width, widened to `i64`.
    pub fn int(&mut self, name: &str) -> Option<Spanned<i64>> {
        let v = self.value(name)?;
        let n = match &v.value {
            Value::I8(n) => Some(i64::from(*n)),
            Value::I16(n) => Some(i64::from(*n)),
            Value::I32(n) => Some(i64::from(*n)),
            Value::I64(n) => Some(*n),
            Value::Isize(n) => i64::try_from(*n).ok(),
            Value::I128(n) => i64::try_from(*n).ok(),
            Value::U8(n) => Some(i64::from(*n)),
            Value::U16(n) => Some(i64::from(*n)),
            Value::U32(n) => Some(i64::from(*n)),
            Value::U64(n) => i64::try_from(*n).ok(),
            Value::Usize(n) => i64::try_from(*n).ok(),
            Value::U128(n) => i64::try_from(*n).ok(),
            _ => None,
        };
        match n {
            Some(n) => Some(Spanned::new(n, v.span)),
            None => {
                self.issue_at(v.span, wrong_type(name, "an integer", &v.value));
                None
            }
        }
    }

    /// An integer inside an inclusive range, narrowed to `T`. The range is
    /// the whole rule: `lo` and `hi` must themselves fit `T`, so a value
    /// that passes the range always narrows.
    pub fn int_in<T: TryFrom<i64>>(&mut self, name: &str, lo: i64, hi: i64) -> Option<Spanned<T>> {
        debug_assert!(
            T::try_from(lo).is_ok() && T::try_from(hi).is_ok(),
            "int_in({name}, {lo}, {hi}): the range must fit the target type, \
             or a rejected value would be reported against a range it met"
        );
        let n = self.int(name)?;
        if !(lo..=hi).contains(&n.value) {
            self.issue_at(n.span, out_of_range(name, lo, hi, n.value));
            return None;
        }
        T::try_from(n.value).ok().map(|v| Spanned::new(v, n.span))
    }

    /// An integer no smaller than `min`, narrowed to `T`. `T`'s width is
    /// the implicit ceiling: a value above `min` that will not fit is too
    /// large, which is a different mistake from being below the floor.
    pub fn int_at_least<T: TryFrom<i64>>(&mut self, name: &str, min: i64) -> Option<Spanned<T>> {
        let n = self.int(name)?;
        if n.value < min {
            self.issue_at(n.span, below_floor(name, min, n.value));
            return None;
        }
        match T::try_from(n.value) {
            Ok(v) => Some(Spanned::new(v, n.span)),
            Err(_) => {
                self.issue_at(n.span, too_large(name, n.value));
                None
            }
        }
    }

    /// A TCP/UDP port: 1–65535.
    pub fn port(&mut self, name: &str) -> Option<Spanned<u16>> {
        self.int_in(name, 1, u16::MAX as i64)
    }

    /// A `std.ByteSize` — a non-negative count of bytes. Unit suffixes
    /// (`8GiB`) are resolved to bytes by wcl before we see them.
    pub fn size(&mut self, name: &str) -> Option<Spanned<u64>> {
        self.int_at_least(name, 0)
    }

    /// A `std.Duration` — a non-negative nanosecond count. Unit suffixes
    /// (`10s`) are resolved to nanoseconds by wcl before we see them.
    pub fn duration(&mut self, name: &str) -> Option<Spanned<Duration>> {
        self.int_at_least::<u64>(name, 0)
            .map(|n| n.map(Duration::from_nanos))
    }

    pub fn path(&mut self, name: &str) -> Option<Spanned<PathBuf>> {
        self.string(name).map(|s| s.map(PathBuf::from))
    }

    /// A path field that must be there.
    pub fn required_path(&mut self, name: &str) -> Option<Spanned<PathBuf>> {
        self.required(name, Self::path)
    }

    /// A string parsed through `parse`, which words its own failure.
    pub fn parsed<T>(
        &mut self,
        name: &str,
        parse: impl FnOnce(&str) -> Result<T, String>,
    ) -> Option<Spanned<T>> {
        let s = self.string(name)?;
        match parse(&s.value) {
            Ok(v) => Some(Spanned::new(v, s.span)),
            Err(e) => {
                self.issue_at(s.span, e);
                None
            }
        }
    }

    /// A string parsed through `FromStr`. `what` names the thing for the
    /// diagnostic: `parse_as::<Ipv4Addr>("ip", "IP address")`.
    pub fn parse_as<T: FromStr>(&mut self, name: &str, what: &str) -> Option<Spanned<T>> {
        self.parsed(name, |s| {
            s.parse::<T>()
                .map_err(|_| format!("malformed {what} `{s}`"))
        })
    }

    /// A list of strings. An absent field and an empty list both read as
    /// empty — use [`Reader::opt_string_list`] where the difference matters.
    pub fn string_list(&mut self, name: &str) -> Vec<String> {
        let Some(v) = self.value(name) else {
            return Vec::new();
        };
        match &v.value {
            Value::List(items) => {
                let mut out = Vec::new();
                let mut bad = Vec::new();
                for item in items.iter() {
                    match item {
                        Value::Utf8(s) | Value::Ascii(s) | Value::Identifier(s) => {
                            out.push(s.clone())
                        }
                        other => bad.push(describe(other)),
                    }
                }
                for got in bad {
                    self.issue_at(
                        v.span,
                        format!("`{name}` must be a list of strings, found {got}"),
                    );
                }
                out
            }
            other => {
                self.issue_at(v.span, wrong_type(name, "a list", other));
                Vec::new()
            }
        }
    }

    /// Like [`Reader::string_list`], but tells an absent field from an
    /// empty list — needed where `[]` is meaningful (a container `command`
    /// override, say).
    pub fn opt_string_list(&mut self, name: &str) -> Option<Vec<String>> {
        self.has(name).then(|| self.string_list(name))
    }

    /// A string that must be drawn from a fixed set, kept as a string —
    /// the untyped sibling of [`Reader::keyword`], for sets the model
    /// carries verbatim (an architecture name, say).
    pub fn one_of(&mut self, name: &str, allowed: &[&str]) -> Option<Spanned<String>> {
        let s = self.string(name)?;
        if allowed.contains(&s.value.as_str()) {
            return Some(s);
        }
        let msg = must_be_one_of(name, &allowed.join(", "), &s.value);
        self.issue_at(s.span, msg);
        None
    }

    /// A string drawn from a fixed set: `firmware = "ovmf"`.
    pub fn keyword<T: Copy>(&mut self, name: &str, table: &[(&str, T)]) -> Option<Spanned<T>> {
        let s = self.string(name)?;
        match table.iter().find(|(k, _)| *k == s.value) {
            Some((_, v)) => Some(Spanned::new(*v, s.span)),
            None => {
                let allowed = table.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ");
                self.issue_at(s.span, must_be_one_of(name, &allowed, &s.value));
                None
            }
        }
    }

    /// A symbol drawn from a fixed set: `mode = :workload`.
    pub fn symbol<T: Copy>(&mut self, name: &str, table: &[(&str, T)]) -> Option<Spanned<T>> {
        let v = self.value(name)?;
        let Value::Symbol(symbol) = &v.value else {
            self.issue_at(v.span, wrong_type(name, "a symbol", &v.value));
            return None;
        };
        match table.iter().find(|(k, _)| k == symbol) {
            Some((_, value)) => Some(Spanned::new(*value, v.span)),
            None => {
                let allowed = table
                    .iter()
                    .map(|(k, _)| format!(":{k}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let got = format!(":{symbol}");
                self.issue_at(v.span, must_be_one_of(name, &allowed, &got));
                None
            }
        }
    }

    /// Run a getter and, if it came back empty without saying why, report
    /// the field as missing. Keeps "absent" and "malformed" from both
    /// landing on the same field, and makes any getter here a required
    /// one: `r.required("kind", |r, n| r.keyword(n, KINDS))`.
    pub fn required<T>(
        &mut self,
        name: &str,
        get: impl FnOnce(&mut Reader<'b, 'i>, &str) -> Option<Spanned<T>>,
    ) -> Option<Spanned<T>> {
        let before = self.issues.len();
        match get(self, name) {
            Some(v) => Some(v),
            None => {
                if self.issues.len() == before {
                    self.missing(name);
                }
                None
            }
        }
    }
}

/// Everything wrong with one file, as an error.
///
/// The message is the rendered list, so a caller that only prints it reads
/// the same `file:line:col` lines a lab file gives. The positioned issues
/// stay reachable through [`IssueError::diagnostic`], so a surface that
/// wants to highlight the offending text can, whichever of the four files
/// it came from — the CLI renders exactly that.
#[derive(Debug, Error)]
#[error("{rendered}")]
pub struct IssueError {
    rendered: String,
    name: String,
    text: String,
    issues: IssueList,
}

impl IssueError {
    /// The same issues as a miette diagnostic, renderable against the file
    /// they came from — the positioned form, for any surface that wants to
    /// point at the offending text rather than print a line number.
    pub fn diagnostic(&self) -> ConfigErrors {
        ConfigErrors {
            name: self.name.clone(),
            src: NamedSource::new(&self.name, self.text.clone()),
            issues: self.issues.clone(),
        }
    }
}

/// Turn one pass's accumulated issues into a `Result`: the extracted value
/// when nothing went wrong, else one error carrying every issue.
///
/// This is the shape all four call sites share once their field mapping is
/// done, so it lives here rather than being written out four times.
pub fn finish<T>(
    name: &str,
    source: &str,
    issues: IssueList,
    value: Option<T>,
) -> Result<T, IssueError> {
    if let Some(v) = value
        && issues.is_empty()
    {
        return Ok(v);
    }
    let mut issues = issues;
    if issues.is_empty() {
        // Nothing extracted and nothing to say: a field mapping gave up
        // without reporting why. Say something rather than "0 error(s)".
        issues.push(Issue::new(format!("cannot read {name}")));
    }
    Err(IssueError {
        rendered: render_issues(name, source, &issues),
        name: name.to_string(),
        text: source.to_string(),
        issues,
    })
}

/// Render an issue list as one error message, resolving spans to
/// `name:line:col`. The call sites that report through `anyhow` rather
/// than miette use this so a profile or host-config mistake still points
/// at the line it was made on.
pub fn render_issues(name: &str, source: &str, issues: &[Issue]) -> String {
    let mut out = format!("{} error(s) in {name}", issues.len());
    for issue in issues {
        let where_ = match issue.span {
            Some(span) => {
                let (line, col) = line_col(source, span.offset());
                format!("{name}:{line}:{col}")
            }
            None => name.to_string(),
        };
        out.push_str(&format!("\n  {where_}: {}", issue.message));
    }
    out
}

/// 1-based line and column of a byte offset.
fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let upto = &source[..offset.min(source.len())];
    let line = upto.matches('\n').count() + 1;
    let col = upto
        .rsplit_once('\n')
        .map_or(upto, |(_, rest)| rest)
        .chars()
        .count()
        + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wcl_lang::Document;

    /// Read `body` as the fields of one block and hand the reader to
    /// `f`, returning what it extracted alongside the issues raised.
    /// Schema-free: these tests are about coercion and wording, not about
    /// what any particular file is allowed to contain.
    fn read<T>(body: &str, f: impl FnOnce(&mut Reader) -> T) -> (T, Vec<String>) {
        let src = format!("thing \"n\" {{ {body} }}");
        let doc = Document::open(&src, "<test>").expect("test block parses");
        let block = doc.blocks().next().expect("one block");
        let mut issues = IssueList::new();
        let out = {
            let mut r = Reader::new(&block, &mut issues);
            f(&mut r)
        };
        (out, issues.iter().map(|i| i.message.clone()).collect())
    }

    /// The one issue `f` raised. Panics if it raised none, or several.
    fn issue<T>(body: &str, f: impl FnOnce(&mut Reader) -> T) -> String {
        let (_, issues) = read(body, f);
        assert_eq!(issues.len(), 1, "expected exactly one issue: {issues:?}");
        issues.into_iter().next().unwrap()
    }

    /// Assert `f` extracted nothing and said nothing — the shape of an
    /// absent optional field.
    fn silent_none<T: std::fmt::Debug>(body: &str, f: impl FnOnce(&mut Reader) -> Option<T>) {
        let (out, issues) = read(body, f);
        assert!(out.is_none(), "expected no value, got {out:?}");
        assert!(issues.is_empty(), "expected no issues, got {issues:?}");
    }

    // ---- the block itself --------------------------------------------

    #[test]
    fn reads_kind_label_and_span() {
        let ((kind, label, span), issues) =
            read("", |r| (r.kind().to_string(), r.label(), r.span()));
        assert_eq!(kind, "thing");
        assert_eq!(label.as_deref(), Some("n"));
        assert!(span.1 > span.0);
        assert!(issues.is_empty());
    }

    #[test]
    fn unlabelled_block_reports() {
        let doc = Document::open("thing { }", "<test>").unwrap();
        let block = doc.blocks().next().unwrap();
        let mut issues = IssueList::new();
        assert!(Reader::new(&block, &mut issues).label().is_none());
        assert_eq!(issues[0].message, "`thing` requires a name label");
        assert!(issues[0].span.is_some(), "label issues carry a span");
    }

    #[test]
    fn has_distinguishes_written_from_absent() {
        let ((written, absent), _) = read("a = false", |r| (r.has("a"), r.has("b")));
        assert!(written);
        assert!(!absent);
    }

    /// A malformed nested block: the child's reader shares the parent's
    /// issue list, so one pass reports the parent's mistakes and the
    /// child's, each against its own span.
    #[test]
    fn a_malformed_nested_block_reports_against_its_own_span() {
        let src = "thing \"n\" { a = 1 inner { b = \"x\" } }";
        let doc = Document::open(src, "<test>").unwrap();
        let block = doc.blocks().next().unwrap();
        let mut issues = IssueList::new();
        let mut r = Reader::new(&block, &mut issues);
        let parent_span = r.span();
        assert!(r.string("a").is_none());
        for child in r.children() {
            let mut c = Reader::new(&child, r.issues());
            assert!(c.int("b").is_none());
        }
        let msgs: Vec<&str> = issues.iter().map(|i| i.message.as_str()).collect();
        assert_eq!(
            msgs,
            [
                "`a` must be a string, got an integer",
                "`b` must be an integer, got a string",
            ]
        );
        // The child's issue points inside the child, not at the parent.
        let child_at = issues[1].span.unwrap().offset();
        assert!(
            child_at > issues[0].span.unwrap().offset() && child_at < parent_span.1,
            "child issue should sit after the parent's field and inside the parent"
        );
    }

    /// The extractor has no unknown-field branch on purpose: a field it is
    /// never asked for is simply not read, and naming what a block may
    /// contain is the schema's job (the four call sites all schema-check
    /// first). Reading a name that isn't there is silent, not an error.
    #[test]
    fn a_field_never_asked_for_is_not_the_extractors_business() {
        let (v, issues) = read("known = \"x\" surprise = 1", |r| r.string("known"));
        assert_eq!(v.unwrap().value, "x");
        assert!(issues.is_empty(), "{issues:?}");
        silent_none("surprise = 1", |r| r.string("absent"));
    }

    #[test]
    fn children_are_handed_back_unread() {
        let src = "thing \"n\" { inner { } inner { } other { } }";
        let doc = Document::open(src, "<test>").unwrap();
        let block = doc.blocks().next().unwrap();
        let mut issues = IssueList::new();
        let r = Reader::new(&block, &mut issues);
        let kinds: Vec<&str> = r.children().map(|c| c.kind()).collect();
        assert_eq!(kinds, ["inner", "inner", "other"]);
    }

    // ---- strings ------------------------------------------------------

    #[test]
    fn string_reads_utf8_and_identifiers() {
        let (v, issues) = read("a = \"hi\"", |r| r.string("a"));
        assert_eq!(v.unwrap().value, "hi");
        assert!(issues.is_empty());
    }

    #[test]
    fn string_carries_the_span_of_its_field() {
        let ((v, span), _) = read("a = \"hi\"", |r| (r.string("a"), r.span()));
        let v = v.unwrap();
        assert!(v.span.0 >= span.0 && v.span.1 <= span.1);
        assert!(v.span.1 > v.span.0);
    }

    #[test]
    fn string_rejects_other_types() {
        assert_eq!(
            issue("a = 1", |r| r.string("a")),
            "`a` must be a string, got an integer"
        );
        assert_eq!(
            issue("a = true", |r| r.string("a")),
            "`a` must be a string, got a bool"
        );
        assert_eq!(
            issue("a = [\"x\"]", |r| r.string("a")),
            "`a` must be a string, got a list"
        );
    }

    #[test]
    fn absent_and_explicit_none_read_alike() {
        silent_none("", |r| r.string("a"));
        silent_none("a = none", |r| r.string("a"));
    }

    #[test]
    fn required_string_reports_absence_once() {
        assert_eq!(
            issue("", |r| r.required_string("a")),
            "missing required field `a`"
        );
        assert_eq!(
            issue("a = none", |r| r.required_string("a")),
            "missing required field `a`"
        );
        // A malformed field says what is wrong with it, and does not also
        // claim to be missing.
        assert_eq!(
            issue("a = 1", |r| r.required_string("a")),
            "`a` must be a string, got an integer"
        );
    }

    #[test]
    fn unevaluatable_field_reports() {
        let msg = issue("a = nope.missing", |r| r.string("a"));
        assert!(msg.starts_with("cannot evaluate `a`: "), "{msg}");
    }

    // ---- bools and integers -------------------------------------------

    #[test]
    fn bool_reads_and_rejects() {
        let (v, _) = read("a = true", |r| r.bool("a"));
        assert!(v.unwrap().value);
        let (v, _) = read("a = false", |r| r.bool("a"));
        assert!(!v.unwrap().value);
        assert_eq!(
            issue("a = \"yes\"", |r| r.bool("a")),
            "`a` must be a bool, got a string"
        );
        silent_none("", |r| r.bool("a"));
    }

    #[test]
    fn int_widens_every_width() {
        let (v, _) = read("a = 7", |r| r.int("a"));
        assert_eq!(v.unwrap().value, 7);
        let (v, _) = read("a = -7", |r| r.int("a"));
        assert_eq!(v.unwrap().value, -7);
        assert_eq!(
            issue("a = \"7\"", |r| r.int("a")),
            "`a` must be an integer, got a string"
        );
    }

    #[test]
    fn int_in_bounds_both_ends() {
        let (v, _) = read("a = 576", |r| r.int_in::<u16>("a", 576, 65535));
        assert_eq!(v.unwrap().value, 576);
        let (v, _) = read("a = 65535", |r| r.int_in::<u16>("a", 576, 65535));
        assert_eq!(v.unwrap().value, 65535);
        assert_eq!(
            issue("a = 100", |r| r.int_in::<u16>("a", 576, 65535)),
            "`a` must be between 576 and 65535, got 100"
        );
        assert_eq!(
            issue("a = 70000", |r| r.int_in::<u16>("a", 576, 65535)),
            "`a` must be between 576 and 65535, got 70000"
        );
    }

    /// The range is the whole rule: a rejected value is reported against
    /// the range it actually missed, never against one it met.
    #[test]
    fn int_in_reports_the_range_it_was_given() {
        assert_eq!(
            issue("a = 300", |r| r.int_in::<u8>("a", 0, 200)),
            "`a` must be between 0 and 200, got 300"
        );
    }

    #[test]
    fn int_at_least_reports_the_floor() {
        let (v, _) = read("a = 1", |r| r.int_at_least::<u32>("a", 1));
        assert_eq!(v.unwrap().value, 1);
        assert_eq!(
            issue("a = 0", |r| r.int_at_least::<u32>("a", 1)),
            "`a` must be at least 1, got 0"
        );
        assert_eq!(
            issue("a = -1", |r| r.int_at_least::<u32>("a", 0)),
            "`a` must be at least 0, got -1"
        );
    }

    /// Above the floor but too wide for the target type is a different
    /// mistake from being below it, and says so.
    #[test]
    fn int_at_least_reports_a_value_too_wide_for_its_type() {
        assert_eq!(
            issue("a = 5000000000", |r| r.int_at_least::<u32>("a", 1)),
            "`a` is too large: 5000000000"
        );
    }

    #[test]
    fn port_takes_1_to_65535() {
        let (v, _) = read("a = 1", |r| r.port("a"));
        assert_eq!(v.unwrap().value, 1);
        assert_eq!(
            issue("a = 0", |r| r.port("a")),
            "`a` must be between 1 and 65535, got 0"
        );
        assert_eq!(
            issue("a = 99999", |r| r.port("a")),
            "`a` must be between 1 and 65535, got 99999"
        );
    }

    #[test]
    fn size_is_a_non_negative_byte_count() {
        let (v, _) = read("a = 1073741824", |r| r.size("a"));
        assert_eq!(v.unwrap().value, 1 << 30);
        assert_eq!(
            issue("a = -1", |r| r.size("a")),
            "`a` must be at least 0, got -1"
        );
        assert_eq!(
            issue("a = \"8GiB\"", |r| r.size("a")),
            "`a` must be an integer, got a string"
        );
    }

    #[test]
    fn duration_is_nanoseconds() {
        let (v, _) = read("a = 5000000000", |r| r.duration("a"));
        assert_eq!(v.unwrap().value, Duration::from_secs(5));
        assert_eq!(
            issue("a = -1", |r| r.duration("a")),
            "`a` must be at least 0, got -1"
        );
    }

    // ---- paths, parsing, lists ----------------------------------------

    #[test]
    fn path_comes_from_a_string() {
        let (v, _) = read("a = \"./isos/x.iso\"", |r| r.path("a"));
        assert_eq!(v.unwrap().value, PathBuf::from("./isos/x.iso"));
        assert_eq!(
            issue("", |r| r.required_path("a")),
            "missing required field `a`"
        );
    }

    #[test]
    fn parsed_reports_the_parser_wording() {
        let (v, _) = read("a = \"7\"", |r| {
            r.parsed("a", |s| s.parse::<u8>().map_err(|e| e.to_string()))
        });
        assert_eq!(v.unwrap().value, 7);
        assert_eq!(
            issue("a = \"x\"", |r| r.parsed("a", |_| Err::<u8, _>(
                "not a number at all".to_string()
            ))),
            "not a number at all"
        );
    }

    #[test]
    fn parse_as_names_the_thing() {
        let (v, _) = read("a = \"10.50.0.10\"", |r| {
            r.parse_as::<std::net::Ipv4Addr>("a", "IP address")
        });
        assert_eq!(v.unwrap().value.to_string(), "10.50.0.10");
        assert_eq!(
            issue("a = \"10.50\"", |r| r
                .parse_as::<std::net::Ipv4Addr>("a", "IP address")),
            "malformed IP address `10.50`"
        );
    }

    #[test]
    fn string_list_reads_and_rejects() {
        let (v, issues) = read("a = [\"x\", \"y\"]", |r| r.string_list("a"));
        assert_eq!(v, ["x", "y"]);
        assert!(issues.is_empty());
        // Absent reads as empty, silently.
        let (v, issues) = read("", |r| r.string_list("a"));
        assert!(v.is_empty() && issues.is_empty());
        assert_eq!(
            issue("a = \"x\"", |r| r.string_list("a")),
            "`a` must be a list, got a string"
        );
        // A bad element is reported; the good ones still come through.
        let (v, issues) = read("a = [\"x\", 1]", |r| r.string_list("a"));
        assert_eq!(v, ["x"]);
        assert_eq!(issues, ["`a` must be a list of strings, found an integer"]);
    }

    #[test]
    fn opt_string_list_tells_absent_from_empty() {
        let (v, _) = read("", |r| r.opt_string_list("a"));
        assert!(v.is_none());
        let (v, _) = read("a = []", |r| r.opt_string_list("a"));
        assert_eq!(v, Some(Vec::new()));
    }

    // ---- enumerations --------------------------------------------------

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Mode {
        One,
        Two,
    }

    const TABLE: &[(&str, Mode)] = &[("one", Mode::One), ("two", Mode::Two)];

    #[test]
    fn keyword_reads_and_lists_what_was_allowed() {
        let (v, _) = read("a = \"two\"", |r| r.keyword("a", TABLE));
        assert_eq!(v.unwrap().value, Mode::Two);
        assert_eq!(
            issue("a = \"three\"", |r| r.keyword("a", TABLE)),
            "`a` must be one of one, two, got `three`"
        );
        assert_eq!(
            issue("a = 1", |r| r.keyword("a", TABLE)),
            "`a` must be a string, got an integer"
        );
    }

    #[test]
    fn symbol_reads_and_lists_what_was_allowed() {
        let (v, _) = read("a = :two", |r| r.symbol("a", TABLE));
        assert_eq!(v.unwrap().value, Mode::Two);
        assert_eq!(
            issue("a = :three", |r| r.symbol("a", TABLE)),
            "`a` must be one of :one, :two, got `:three`"
        );
        assert_eq!(
            issue("a = \"two\"", |r| r.symbol("a", TABLE)),
            "`a` must be a symbol, got a string"
        );
    }

    // ---- accumulation ---------------------------------------------------

    #[test]
    fn every_bad_field_is_reported_in_one_pass() {
        let (_, issues) = read("a = 1 b = \"x\" c = 0", |r| {
            (r.string("a"), r.bool("b"), r.port("c"))
        });
        assert_eq!(
            issues,
            [
                "`a` must be a string, got an integer",
                "`b` must be a bool, got a string",
                "`c` must be between 1 and 65535, got 0",
            ]
        );
    }

    // ---- rendering -------------------------------------------------------

    #[test]
    fn finish_hands_back_the_value_when_nothing_went_wrong() {
        let v = finish("f.wcl", "one\n", IssueList::new(), Some(7)).unwrap();
        assert_eq!(v, 7);
    }

    #[test]
    fn finish_keeps_the_spans_reachable_not_just_rendered() {
        let issues = vec![Issue::at((4, 7), "second line")];
        let err = finish("f.wcl", "one\ntwo\nthree", issues, Some(7)).unwrap_err();
        // Printed, it reads like a lab file's diagnostic …
        assert_eq!(
            err.to_string(),
            "1 error(s) in f.wcl\n  f.wcl:2:1: second line"
        );
        // … and the positions survive for a surface that wants to
        // highlight the text rather than print a line number.
        let diag = err.diagnostic();
        assert_eq!(diag.name, "f.wcl");
        assert_eq!(diag.issues[0].span.unwrap().offset(), 4);
    }

    /// A field mapping that gives up without saying why would otherwise
    /// report "0 error(s)".
    #[test]
    fn finish_never_reports_an_empty_failure() {
        let err = finish("f.wcl", "", IssueList::new(), None::<u8>).unwrap_err();
        assert_eq!(
            err.to_string(),
            "1 error(s) in f.wcl\n  f.wcl: cannot read f.wcl"
        );
    }

    #[test]
    fn render_resolves_spans_to_line_and_column() {
        let source = "one\ntwo\nthree";
        let issues = vec![Issue::at((4, 7), "second line"), Issue::new("no position")];
        assert_eq!(
            render_issues("f.wcl", source, &issues),
            "2 error(s) in f.wcl\n  f.wcl:2:1: second line\n  f.wcl: no position"
        );
    }

    #[test]
    fn line_col_counts_from_one() {
        assert_eq!(line_col("abc", 0), (1, 1));
        assert_eq!(line_col("abc", 2), (1, 3));
        assert_eq!(line_col("a\nbc", 3), (2, 2));
        // Past the end clamps rather than panicking.
        assert_eq!(line_col("a\n", 99), (2, 1));
    }
}
