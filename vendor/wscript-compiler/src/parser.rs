//! Hand-written recursive-descent parser with error recovery (PRD §5.1).
//!
//! On broken input the parser reports diagnostics and produces a partial
//! AST (with `Error` nodes) rather than bailing — the LSP depends on this.
//!
//! Statement termination: a `Newline` token ends a statement. Continuation
//! happens when the construct cannot end yet — inside `(`/`[` the lexer
//! suppresses newlines entirely; after a binary operator or `,` the parser
//! simply skips newlines before the operand; and two lookahead cases let a
//! *following* line continue the previous one: a line starting with `.`
//! (method chains) and an `else` after `}`.

use wscript_core::diag::Diagnostic;
use wscript_core::span::Span;

use crate::ast::*;
use crate::lexer;
use crate::token::{Token, TokenKind};

pub struct ParseOutput {
    pub file: SourceFile,
    pub diags: Vec<Diagnostic>,
}

/// Nesting budget for a single expression/pattern/type. Pathological
/// nesting must produce a diagnostic, not overflow the stack — the parser
/// recurses per level, and the checker and emitter recurse over the AST
/// it builds (the LSP runs them on smaller tokio stacks). Costs are
/// weighted by what they burn: a recursion level crosses the whole
/// precedence chain (~14 debug frames, upwards of 10 KiB of a 2 MiB
/// thread stack) and costs `RECURSION_COST`; an operator/postfix chain
/// link adds no parser recursion, only AST depth, and costs 1. Net
/// effect: ~100 nested levels or ~500 chained operations, far beyond
/// real code; see `nesting_too_deep`.
const MAX_NESTING_BUDGET: u32 = 500;
const RECURSION_COST: u32 = 5;

pub fn parse(src: &str) -> ParseOutput {
    let mut next_id = 0;
    parse_file(src, 0, &mut next_id)
}

/// Parse one file of a (possibly multi-file) compilation: spans are
/// offset by `base` into the global address space, and `next_id` is the
/// program-wide NodeId counter (checker side tables are keyed by NodeId,
/// so ids must be unique across files).
pub fn parse_file(src: &str, base: u32, next_id: &mut NodeId) -> ParseOutput {
    let lexed = lexer::lex_at(src, base);
    let mut parser = Parser {
        tokens: lexed.tokens,
        pos: 0,
        diags: lexed.diags,
        next_id: *next_id,
        no_struct_lit: false,
        depth: 0,
        depth_exceeded: false,
    };
    let file = parser.source_file();
    *next_id = parser.next_id;
    ParseOutput {
        file,
        diags: parser.diags,
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    diags: Vec<Diagnostic>,
    next_id: NodeId,
    /// Set while parsing `if`/`while`/`for`/`match` headers, where `{`
    /// starts the body rather than a struct literal.
    no_struct_lit: bool,
    /// Current nesting level — recursive entry points and the operator/
    /// postfix chain loops both count toward it (chains deepen the AST
    /// without deepening the parse stack).
    depth: u32,
    /// Tripped `MAX_NESTING_BUDGET`: the rest of the file was skipped and
    /// further diagnostics are suppressed.
    depth_exceeded: bool,
}

/// The bracket pair around a delimited list.
///
/// Everything [`Parser::list`] needs beyond the element parser is derived
/// from this: the closing token, whether newlines inside are noise, and
/// where a malformed element resyncs to. A call site names the brackets it
/// wrote and gets the rest, so the lexer's delimiter table stops being
/// tribal knowledge — it used to surface as a `skip_newlines` present at
/// some list sites and absent at others, with nothing at either saying why.
#[derive(Clone, Copy)]
enum Brackets {
    /// `(` … `)`.
    Paren,
    /// `[` … `]`.
    Bracket,
    /// `{` … `}`, including the `#{` of a map literal.
    Brace,
    /// `|` … `|` — closure parameters. Not a lexer delimiter, so newlines
    /// stay significant and end the list rather than being skipped.
    Pipe,
}

impl Brackets {
    fn close(self) -> TokenKind {
        match self {
            Brackets::Paren => TokenKind::RParen,
            Brackets::Bracket => TokenKind::RBracket,
            Brackets::Brace => TokenKind::RBrace,
            Brackets::Pipe => TokenKind::Pipe,
        }
    }

    /// Are newlines inside this list noise the list has to skip itself?
    ///
    /// Only in braces. The lexer suppresses newlines outright inside `(`
    /// and `[` (`Delim::Paren`/`Delim::Bracket`), so there is nothing left
    /// to skip; `{` and `#{` both push `Delim::Brace`, which does not. `|`
    /// is not a delimiter at all, so a newline there ends the construct.
    fn skips_newlines(self) -> bool {
        matches!(self, Brackets::Brace)
    }

    /// Where a malformed element resyncs to: the list's own punctuation,
    /// plus the boundaries it must not recover past.
    ///
    /// Four sets, one per bracket shape, replacing six literals that
    /// expressed about four concepts — two of them the same three tokens
    /// written in a different order.
    ///
    /// Outside braces the set also carries `{` and `}`, which are the
    /// block *around* the list. They matter because the lexer suppresses
    /// newlines inside `(` and `[`: without them the only stop reachable
    /// in `fn main() { let x = [1 2\n}` is the `{` of the *next*
    /// declaration, so recovery eats the block's `}` and the whole
    /// declaration after it — the silent-declaration-loss that `starts_item`
    /// was introduced to end. Inside braces `}` is already the close.
    fn follow(self) -> &'static [TokenKind] {
        match self {
            Brackets::Paren => &[
                TokenKind::Comma,
                TokenKind::RParen,
                TokenKind::Newline,
                TokenKind::LBrace,
                TokenKind::RBrace,
            ],
            Brackets::Bracket => &[
                TokenKind::Comma,
                TokenKind::RBracket,
                TokenKind::Newline,
                TokenKind::LBrace,
                TokenKind::RBrace,
            ],
            Brackets::Pipe => &[
                TokenKind::Comma,
                TokenKind::Pipe,
                TokenKind::Newline,
                TokenKind::LBrace,
                TokenKind::RBrace,
            ],
            Brackets::Brace => &[TokenKind::Comma, TokenKind::RBrace, TokenKind::Newline],
        }
    }

    /// Can the list pick up again from the token recovery landed on?
    ///
    /// Its own punctuation, yes. A newline only inside braces, where it is
    /// layout between entries; everywhere else a newline is the end of the
    /// construct, and so is anything else in the follow set.
    fn resumes_at(self, kind: &TokenKind) -> bool {
        *kind == TokenKind::Comma
            || *kind == self.close()
            || (self.skips_newlines() && *kind == TokenKind::Newline)
    }
}

/// What ends one element of a list and starts the next.
#[derive(Clone, Copy)]
enum Sep {
    /// A `,`.
    Comma,
    /// A `,` or a newline — `units` bodies and `match` arms read better as
    /// a table, one entry per line.
    CommaOrNewline,
}

impl Sep {
    /// The separator as it reads in `expected …, found …`.
    fn describe(self) -> &'static str {
        match self {
            Sep::Comma => "`,`",
            Sep::CommaOrNewline => "`,`, a newline",
        }
    }
}

impl Parser {
    // ------------------------------------------------------------ cursor

    fn id(&mut self) -> NodeId {
        self.next_id += 1;
        self.next_id
    }

    fn tok(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn kind(&self) -> &TokenKind {
        &self.tok().kind
    }

    fn span(&self) -> Span {
        self.tok().span
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1).min(self.tokens.len() - 1)].span
    }

    fn nth_kind(&self, n: usize) -> &TokenKind {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)].kind
    }

    fn bump(&mut self) -> Token {
        let t = self.tok().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, kind: &TokenKind) -> bool {
        self.kind() == kind
    }

    fn at_eof(&self) -> bool {
        matches!(self.kind(), TokenKind::Eof)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.kind(), TokenKind::Newline | TokenKind::DocComment(_)) {
            self.bump();
        }
    }

    /// Skip newlines/semicolons, collecting `///` doc comments for the
    /// next declaration.
    fn collect_docs(&mut self) -> Option<String> {
        let mut docs: Vec<String> = Vec::new();
        loop {
            match self.kind() {
                TokenKind::Newline | TokenKind::Semi => {
                    self.bump();
                }
                TokenKind::DocComment(text) => {
                    docs.push(text.clone());
                    self.bump();
                }
                _ => break,
            }
        }
        if docs.is_empty() {
            None
        } else {
            Some(docs.join("\n"))
        }
    }

    /// Peek at the next token kind, looking through newlines.
    fn peek_through_newlines(&self) -> &TokenKind {
        let mut n = 0;
        while matches!(
            self.nth_kind(n),
            TokenKind::Newline | TokenKind::DocComment(_)
        ) {
            n += 1;
        }
        self.nth_kind(n)
    }

    fn error(&mut self, code: &'static str, span: Span, msg: impl Into<String>) {
        if self.depth_exceeded {
            return;
        }
        self.diags.push(Diagnostic::error(code, span, msg));
    }

    fn error_help(
        &mut self,
        code: &'static str,
        span: Span,
        msg: impl Into<String>,
        help: impl Into<String>,
    ) {
        if self.depth_exceeded {
            return;
        }
        self.diags
            .push(Diagnostic::error(code, span, msg).with_help(help));
    }

    /// Check the nesting budget before going one level deeper. On the
    /// first trip this reports E0114 and jumps to EOF — the unwinding
    /// frames would otherwise flood the diagnostics with follow-on
    /// "expected ..." errors, so `error` goes quiet from here on.
    fn nesting_too_deep(&mut self) -> bool {
        if self.depth < MAX_NESTING_BUDGET {
            return false;
        }
        let span = self.span();
        self.error_help(
            "E0114",
            span,
            "code is nested too deeply",
            "the compiler allows about 100 nested levels (500 chained operations); \
             split this into smaller statements",
        );
        self.depth_exceeded = true;
        self.pos = self.tokens.len() - 1;
        true
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Option<Span> {
        if self.at(kind) {
            Some(self.bump().span)
        } else {
            let found = self.kind().describe();
            let span = self.span();
            self.error("E0100", span, format!("expected {what}, found {found}"));
            None
        }
    }

    fn expect_ident(&mut self, what: &str) -> Option<Ident> {
        match self.kind() {
            TokenKind::Ident(name) => {
                let name = name.clone();
                let span = self.bump().span;
                Some(Ident { name, span })
            }
            _ => {
                let found = self.kind().describe();
                let span = self.span();
                self.error("E0100", span, format!("expected {what}, found {found}"));
                None
            }
        }
    }

    // --------------------------------------------------- delimited lists

    /// Skip tokens until one of `stops` (or EOF). Does not consume the stop.
    fn recover_to(&mut self, stops: &[TokenKind]) {
        while !self.at_eof() && !stops.iter().any(|s| self.at(s)) {
            self.bump();
        }
    }

    /// Resync to the next method signature in a `trait` or `impl` body.
    fn sync_to_method(&mut self) {
        self.recover_to(&[TokenKind::KwFn, TokenKind::RBrace]);
    }

    /// Resync to the end of the current statement.
    fn sync_to_stmt_end(&mut self) {
        self.recover_to(&[TokenKind::Newline, TokenKind::Semi, TokenKind::RBrace]);
    }

    /// Consume the separator between two elements. Reports nothing: a
    /// missing separator is [`Self::list`]'s to describe, because only it
    /// knows what would have closed the list instead.
    fn eat_sep(&mut self, brackets: Brackets, sep: Sep) -> bool {
        if self.eat(&TokenKind::Comma) {
            return true;
        }
        match sep {
            // A newline ends an entry as well as a `,` does.
            Sep::CommaOrNewline if self.at(&TokenKind::Newline) => {
                self.skip_newlines();
                true
            }
            // Inside braces a newline *before* the `,` is only layout.
            Sep::Comma if brackets.skips_newlines() => {
                self.skip_newlines();
                self.eat(&TokenKind::Comma)
            }
            _ => false,
        }
    }

    /// Parse a `close`-terminated, `sep`-separated list of elements. The
    /// opening bracket is already consumed — call sites reach it too many
    /// ways (`expect` with a bespoke message, a bare `eat`, a `?`) for one
    /// signature to cover — so `open` is its span, which is where an
    /// unclosed list is reported: the reader needs the bracket that was
    /// never closed, not the end of the file where that became apparent.
    /// `f` parses one element and returns `None` when it could not, which
    /// resyncs to [`Brackets::follow`] and carries on.
    ///
    /// `what` names the list in diagnostics ("argument list", "map
    /// literal"); the punctuation around it is derived, so no call site
    /// spells out `,` or the closing bracket.
    ///
    /// Owning all of a list's punctuation is what makes this one
    /// convention rather than sixteen: every `expect` at a separator or a
    /// closing bracket happens here, so no call site has to choose between
    /// propagating the `Option` it returns, ignoring it, and expecting then
    /// recovering then eating — the three that were in use.
    ///
    /// Trailing separators are accepted everywhere, and the loop cannot
    /// spin: an iteration that consumed nothing ends the list. Not every
    /// hand-rolled loop had that property. `struct S { : bool, }` parked
    /// the old struct-field loop on the `,` — `recover_to` stops without
    /// consuming, and the `continue` re-entered on the same token — and it
    /// pushed a fresh diagnostic per turn until the process was OOM-killed.
    fn list<T>(
        &mut self,
        open: Span,
        brackets: Brackets,
        sep: Sep,
        what: &str,
        mut f: impl FnMut(&mut Self) -> Option<T>,
    ) -> Vec<T> {
        let close = brackets.close();
        let mut out = Vec::new();
        loop {
            if brackets.skips_newlines() {
                self.skip_newlines();
            }
            if self.eat(&close) {
                return out;
            }
            if self.at_eof() {
                let msg = format!("unclosed {what}: missing {}", close.describe());
                self.error("E0100", open, msg);
                return out;
            }
            let start = self.pos;
            match f(self) {
                Some(item) => out.push(item),
                None => self.recover_to(brackets.follow()),
            }
            if self.eat_sep(brackets, sep) {
                continue;
            }
            // The close is left for the top of the loop, which is what
            // makes a trailing separator legal without a second check.
            if self.at(&close) {
                continue;
            }
            let msg = format!("{} or {} in {what}", sep.describe(), close.describe());
            self.expect(&close, &msg);
            self.recover_to(brackets.follow());
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            // Nothing to resume from — the construct ended, or recovery
            // could not move at all. Either way the list is over; carrying
            // on would report the same token forever.
            if self.pos == start || !brackets.resumes_at(self.kind()) {
                return out;
            }
        }
    }

    // ------------------------------------------------------------- items

    fn source_file(&mut self) -> SourceFile {
        let mut items = Vec::new();
        loop {
            let doc = self.collect_docs();
            if self.at_eof() {
                break;
            }
            match self.item(doc, true) {
                Some(item) => items.push(item),
                None => self.sync_to_item(),
            }
        }
        SourceFile { items }
    }

    /// Skip tokens until something that can plausibly start an item.
    /// Does the current token begin an item?
    ///
    /// One definition, shared by [`Self::item`]'s dispatch and
    /// [`Self::sync_to_item`]'s recovery. They used to carry separate
    /// lists and had already drifted: recovery omitted `mod`, `const` and
    /// contextual `units`, so a malformed item swallowed the declaration
    /// that followed instead of resyncing on it.
    fn starts_item(&self) -> bool {
        match self.kind() {
            TokenKind::KwFn
            | TokenKind::KwStruct
            | TokenKind::KwEnum
            | TokenKind::KwTrait
            | TokenKind::KwImpl
            | TokenKind::KwUse
            | TokenKind::KwMod
            | TokenKind::KwConst
            | TokenKind::Hash => true,
            // `units` is contextual — an item only when a name follows.
            TokenKind::Ident(name) => {
                name == "units" && matches!(self.nth_kind(1), TokenKind::Ident(_))
            }
            _ => false,
        }
    }

    fn sync_to_item(&mut self) {
        let mut depth = 0usize;
        loop {
            match self.kind() {
                TokenKind::Eof => break,
                TokenKind::LBrace | TokenKind::HashBrace => {
                    depth += 1;
                    self.bump();
                }
                TokenKind::RBrace => {
                    depth = depth.saturating_sub(1);
                    self.bump();
                    if depth == 0 {
                        break;
                    }
                }
                _ if depth == 0 && self.starts_item() => break,
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn item(&mut self, doc: Option<String>, allow_mod: bool) -> Option<Item> {
        let (derives, opaque) = self.attributes();
        match self.kind() {
            TokenKind::KwMod if allow_mod => self.mod_decl(doc).map(Item::Mod),
            TokenKind::KwConst => self.const_decl(doc).map(Item::Const),
            TokenKind::KwUse => {
                if !derives.is_empty() {
                    let span = self.span();
                    self.error("E0101", span, "attributes are not allowed on `use`");
                }
                self.use_decl().map(Item::Use)
            }
            TokenKind::KwFn => {
                if !derives.is_empty() {
                    let span = self.span();
                    self.error_help(
                        "E0101",
                        span,
                        "`#[derive(...)]` is not allowed on functions",
                        "derives apply to `struct` and `enum` declarations",
                    );
                }
                self.fn_decl(false, doc).map(Item::Fn)
            }
            // `units` is contextual: only a keyword when it heads an item
            // and is followed by a type name, so scripts may still use it
            // as an ordinary identifier elsewhere.
            TokenKind::Ident(name)
                if name == "units" && matches!(self.nth_kind(1), TokenKind::Ident(_)) =>
            {
                self.units_decl(derives, doc).map(Item::Units)
            }
            TokenKind::KwStruct => self.struct_decl(derives, opaque, doc).map(Item::Struct),
            TokenKind::KwEnum => self.enum_decl(derives, doc).map(Item::Enum),
            TokenKind::KwTrait => self.trait_decl().map(Item::Trait),
            TokenKind::KwImpl => self.impl_decl().map(Item::Impl),
            _ => {
                let found = self.kind().describe();
                let span = self.span();
                self.error_help(
                    "E0102",
                    span,
                    format!("expected an item, found {found}"),
                    "top-level code lives in functions; script execution starts at `fn main()`",
                );
                None
            }
        }
    }

    /// `#[derive(A, B)]` (scripts) and `#[opaque]` (interface files).
    fn attributes(&mut self) -> (Vec<Ident>, bool) {
        let mut derives = Vec::new();
        let mut opaque = false;
        while self.at(&TokenKind::Hash) {
            let hash_span = self.bump().span;
            if self.expect(&TokenKind::LBracket, "`[` after `#`").is_none() {
                break;
            }
            let name = match self.expect_ident("attribute name") {
                Some(n) => n,
                None => break,
            };
            if name.name == "opaque" {
                opaque = true;
                self.expect(&TokenKind::RBracket, "`]` to close the attribute");
                self.skip_newlines();
                continue;
            }
            if name.name != "derive" {
                self.error_help(
                    "E0103",
                    hash_span.to(name.span),
                    format!("unknown attribute `{}`", name.name),
                    "the only attributes supported are `#[derive(...)]` and `#[opaque]` \
                     (interface files)",
                );
            }
            if self.eat(&TokenKind::LParen) {
                let open = self.prev_span();
                derives.extend(
                    self.list(open, Brackets::Paren, Sep::Comma, "derive list", |p| {
                        p.expect_ident("trait name in derive list")
                    }),
                );
            }
            self.expect(&TokenKind::RBracket, "`]` to close the attribute");
            self.skip_newlines();
        }
        (derives, opaque)
    }

    fn use_decl(&mut self) -> Option<UseDecl> {
        let kw = self.bump().span;
        // Path form: `use "./helpers.wscript" [as name]`.
        if let TokenKind::Str(p) = self.kind() {
            let p = p.clone();
            let path_span = self.bump().span;
            let module = if self.eat_kw_as() {
                self.expect_ident("module name after `as`")?
            } else {
                // Default module name: the file stem.
                let stem = std::path::Path::new(&p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let ok = !stem.is_empty()
                    && stem.chars().enumerate().all(|(i, c)| {
                        c == '_' || c.is_ascii_alphanumeric() && (i > 0 || !c.is_ascii_digit())
                    });
                if !ok {
                    self.error(
                        "E0200",
                        path_span,
                        format!("cannot derive a module name from `{p}`; add `as name`"),
                    );
                    self.terminate_stmt();
                    return None;
                }
                Ident {
                    name: stem,
                    span: path_span,
                }
            };
            let span = kw.to(self.prev_span());
            self.terminate_stmt();
            return Some(UseDecl {
                module,
                item: None,
                path_lit: Some(p),
                span,
            });
        }
        let module = self.expect_ident("module name after `use`")?;
        let mut item = None;
        if self.eat(&TokenKind::ColonColon) {
            // A missing item name keeps the declaration as a bare `use
            // module`: the error is already reported, and an editor
            // completing `use math::` needs the module left in the tree to
            // answer from (see `check::Index`).
            item = self.expect_ident("item name after `::`");
        }
        let span = kw.to(self.prev_span());
        self.terminate_stmt();
        Some(UseDecl {
            module,
            item,
            path_lit: None,
            span,
        })
    }

    /// Contextual keyword `as` (an ordinary identifier elsewhere).
    fn eat_kw_as(&mut self) -> bool {
        if let TokenKind::Ident(name) = self.kind()
            && name == "as"
        {
            self.bump();
            return true;
        }
        false
    }

    /// `allow_self`: parsing inside an impl/trait block.
    fn fn_decl(&mut self, allow_self: bool, doc: Option<String>) -> Option<FnDecl> {
        let kw = self.bump().span; // `fn`
        let name = self.expect_ident("function name after `fn`")?;
        let type_params = if self.at(&TokenKind::LBracket) {
            self.type_param_list()
        } else {
            Vec::new()
        };
        let open = self.expect(&TokenKind::LParen, "`(` to start the parameter list")?;
        let params = self.params(open, allow_self);
        let mut ret = None;
        if self.eat(&TokenKind::Arrow) {
            ret = Some(self.type_expr());
        }
        let sig_span = kw.to(self.prev_span());
        // Bodyless declarations are the `.wscripti` interface form (PRD §9.1);
        // the checker rejects them in scripts.
        if self.peek_through_newlines() != &TokenKind::LBrace {
            let id = self.id();
            return Some(FnDecl {
                name,
                type_params,
                params,
                ret,
                body: Block {
                    stmts: vec![],
                    span: sig_span,
                    id,
                },
                has_body: false,
                doc,
                span: sig_span,
                sig_span,
            });
        }
        self.skip_newlines();
        let body = self.block()?;
        let span = kw.to(body.span);
        Some(FnDecl {
            name,
            type_params,
            params,
            ret,
            body,
            has_body: true,
            doc,
            span,
            sig_span,
        })
    }

    fn mod_decl(&mut self, doc: Option<String>) -> Option<ModDecl> {
        let kw = self.bump().span; // `mod`
        let name = self.expect_ident("module name after `mod`")?;
        self.skip_newlines();
        self.expect(&TokenKind::LBrace, "`{` to start the module block")?;
        let mut items = Vec::new();
        loop {
            let doc = self.collect_docs();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error("E0100", kw, "unclosed module block: missing `}`");
                break;
            }
            match self.item(doc, false) {
                Some(item) => items.push(item),
                None => self.sync_to_item(),
            }
        }
        let span = kw.to(self.prev_span());
        Some(ModDecl {
            name,
            items,
            doc,
            span,
        })
    }

    fn const_decl(&mut self, doc: Option<String>) -> Option<ConstDecl> {
        let kw = self.bump().span; // `const`
        let name = self.expect_ident("constant name after `const`")?;
        self.expect(&TokenKind::Colon, "`:` after the constant name")?;
        let ty = self.type_expr();
        let value = self.eat(&TokenKind::Eq).then(|| self.expr());
        let span = kw.to(value.as_ref().map_or(ty.span, |v| v.span));
        self.terminate_stmt();
        Some(ConstDecl {
            name,
            ty,
            value,
            doc,
            span,
        })
    }

    /// `open`: the span of the `(`, for an unclosed parameter list.
    fn params(&mut self, open: Span, allow_self: bool) -> Vec<Param> {
        let mut first = true;
        self.list(open, Brackets::Paren, Sep::Comma, "parameter list", |p| {
            let is_first = std::mem::replace(&mut first, false);
            p.param(is_first, allow_self)
        })
    }

    /// One parameter: `name: type`, or a bare `self` heading a method.
    /// `first`: this is the parameter list's first element, the only
    /// position `self` may occupy.
    fn param(&mut self, first: bool, allow_self: bool) -> Option<Param> {
        if self.at(&TokenKind::KwSelf) {
            let span = self.bump().span;
            if !allow_self {
                self.error_help(
                    "E0104",
                    span,
                    "`self` parameter outside an `impl` or `trait` block",
                    "`self` is only valid as the first parameter of a method",
                );
            } else if !first {
                self.error("E0104", span, "`self` must be the first parameter");
            }
            return Some(Param {
                name: Ident {
                    name: "self".into(),
                    span,
                },
                ty: None,
                is_self: true,
                span,
            });
        }
        let name = self.expect_ident("parameter name")?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.type_expr())
        } else {
            let span = name.span;
            self.error_help(
                "E0105",
                span,
                format!("parameter `{}` is missing a type annotation", name.name),
                "annotations are required on function parameters: `name: type` (PRD §3.3)",
            );
            None
        };
        let span = name.span.to(self.prev_span());
        Some(Param {
            name,
            ty,
            is_self: false,
            span,
        })
    }

    fn struct_decl(
        &mut self,
        derives: Vec<Ident>,
        opaque: bool,
        doc: Option<String>,
    ) -> Option<StructDecl> {
        let kw = self.bump().span;
        let name = self.expect_ident("struct name")?;
        let open = self.expect(&TokenKind::LBrace, "`{` to start the field list")?;
        let fields = self.list(
            open,
            Brackets::Brace,
            Sep::Comma,
            "struct declaration",
            Self::field_decl,
        );
        let span = kw.to(self.prev_span());
        Some(StructDecl {
            name,
            fields,
            derives,
            opaque,
            doc,
            span,
        })
    }

    /// One `name: type` field. Shared by `struct` declarations and enum
    /// struct-variants, which is the point: they carried separate loops
    /// for the same grammar, and the copies had drifted — the struct's
    /// resynced on a malformed field, the variant's abandoned the list.
    fn field_decl(&mut self) -> Option<FieldDecl> {
        let name = self.expect_ident("field name")?;
        self.expect(&TokenKind::Colon, "`:` after field name");
        let ty = self.type_expr();
        let span = name.span.to(ty.span);
        Some(FieldDecl { name, ty, span })
    }

    /// `units Duration: int { ns = 1, ms = 1_000 * us }` — entries may be
    /// separated by commas or newlines, since the table form reads better.
    fn units_decl(&mut self, derives: Vec<Ident>, doc: Option<String>) -> Option<UnitsDecl> {
        let kw = self.bump().span;
        let name = self.expect_ident("unit family name")?;
        self.expect(
            &TokenKind::Colon,
            "`:` and a backing type (`int` or `float`)",
        );
        let base = self.type_expr();
        let open = self.expect(&TokenKind::LBrace, "`{` to start the unit list")?;
        let units = self.list(
            open,
            Brackets::Brace,
            Sep::CommaOrNewline,
            "`units` declaration",
            |p| {
                let name = p.expect_ident("unit name")?;
                p.expect(&TokenKind::Eq, "`=` and a conversion factor");
                let factor = p.expr();
                let span = name.span.to(factor.span);
                Some(UnitEntry { name, factor, span })
            },
        );
        let span = kw.to(self.prev_span());
        Some(UnitsDecl {
            name,
            base,
            units,
            derives,
            doc,
            span,
        })
    }

    fn enum_decl(&mut self, derives: Vec<Ident>, doc: Option<String>) -> Option<EnumDecl> {
        let kw = self.bump().span;
        let name = self.expect_ident("enum name")?;
        let open = self.expect(&TokenKind::LBrace, "`{` to start the variant list")?;
        let variants = self.list(open, Brackets::Brace, Sep::Comma, "enum declaration", |p| {
            let name = p.expect_ident("variant name")?;
            let body = if p.eat(&TokenKind::LParen) {
                let open = p.prev_span();
                VariantBody::Tuple(p.list(
                    open,
                    Brackets::Paren,
                    Sep::Comma,
                    "variant payload",
                    |p| Some(p.type_expr()),
                ))
            } else if p.eat(&TokenKind::LBrace) {
                let open = p.prev_span();
                VariantBody::Struct(p.list(
                    open,
                    Brackets::Brace,
                    Sep::Comma,
                    "variant field list",
                    Self::field_decl,
                ))
            } else {
                VariantBody::Unit
            };
            let span = name.span.to(p.prev_span());
            Some(VariantDecl { name, body, span })
        });
        let span = kw.to(self.prev_span());
        Some(EnumDecl {
            name,
            variants,
            derives,
            doc,
            span,
        })
    }

    fn trait_decl(&mut self) -> Option<TraitDecl> {
        let kw = self.bump().span;
        let name = self.expect_ident("trait name")?;
        self.expect(&TokenKind::LBrace, "`{` to start the trait body")?;
        let mut methods = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                let span = self.span();
                self.error("E0100", span, "unclosed trait declaration");
                break;
            }
            if !self.at(&TokenKind::KwFn) {
                let found = self.kind().describe();
                let span = self.span();
                self.error(
                    "E0100",
                    span,
                    format!("expected `fn` method signature in trait body, found {found}"),
                );
                self.sync_to_method();
                continue;
            }
            let kw_fn = self.bump().span;
            let mname = match self.expect_ident("method name") {
                Some(n) => n,
                None => continue,
            };
            let open = self
                .expect(&TokenKind::LParen, "`(` to start the parameter list")
                .unwrap_or(mname.span);
            let mut params = self.params(open, true);
            if params.first().is_none_or(|p| !p.is_self) {
                self.error_help(
                    "E0106",
                    mname.span,
                    "trait methods must take `self` as their first parameter",
                    "write `fn name(self, ...)`",
                );
            } else {
                params.remove(0);
            }
            let ret = if self.eat(&TokenKind::Arrow) {
                Some(self.type_expr())
            } else {
                None
            };
            let span = kw_fn.to(self.prev_span());
            if self.peek_through_newlines() == &TokenKind::LBrace {
                self.skip_newlines();
                let body_span = self.span();
                self.error_help(
                    "E0107",
                    body_span,
                    "default method bodies are not supported in v1",
                    "declare the signature only; implement it in `impl Trait for Type` blocks",
                );
                // Skip the body for recovery.
                let _ = self.block();
            }
            methods.push(TraitMethodDecl {
                name: mname,
                params,
                ret,
                span,
            });
        }
        let span = kw.to(self.prev_span());
        Some(TraitDecl {
            name,
            methods,
            span,
        })
    }

    fn impl_decl(&mut self) -> Option<ImplDecl> {
        let kw = self.bump().span;
        let first = self.expect_ident("type or trait name after `impl`")?;
        let (trait_name, ty_name) = if self.eat(&TokenKind::KwFor) {
            let ty = self.expect_ident("type name after `for`")?;
            (Some(first), ty)
        } else {
            (None, first)
        };
        self.skip_newlines();
        self.expect(&TokenKind::LBrace, "`{` to start the impl body")?;
        let mut fns = Vec::new();
        loop {
            let doc = self.collect_docs();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                let span = self.span();
                self.error("E0100", span, "unclosed impl block");
                break;
            }
            if !self.at(&TokenKind::KwFn) {
                let found = self.kind().describe();
                let span = self.span();
                self.error(
                    "E0100",
                    span,
                    format!("expected `fn` in impl body, found {found}"),
                );
                self.sync_to_method();
                continue;
            }
            if let Some(f) = self.fn_decl(true, doc) {
                fns.push(f);
            } else {
                self.sync_to_method();
            }
        }
        let span = kw.to(self.prev_span());
        Some(ImplDecl {
            trait_name,
            ty_name,
            fns,
            span,
        })
    }

    // ------------------------------------------------------------- types

    fn type_expr(&mut self) -> TypeExpr {
        if self.nesting_too_deep() {
            return TypeExpr {
                kind: TypeExprKind::Error,
                span: self.span(),
            };
        }
        self.depth += RECURSION_COST;
        let t = self.type_expr_inner();
        self.depth -= RECURSION_COST;
        t
    }

    fn type_expr_inner(&mut self) -> TypeExpr {
        let start = self.span();
        match self.kind().clone() {
            TokenKind::LParen => {
                self.bump();
                if self.eat(&TokenKind::RParen) {
                    return TypeExpr {
                        kind: TypeExprKind::Unit,
                        span: start.to(self.prev_span()),
                    };
                }
                let inner = self.type_expr();
                self.expect(&TokenKind::RParen, "`)` to close the type");
                inner
            }
            TokenKind::KwFn => {
                self.bump();
                let open = self
                    .expect(&TokenKind::LParen, "`(` in function type")
                    .unwrap_or(start);
                let params = self.list(open, Brackets::Paren, Sep::Comma, "function type", |p| {
                    Some(p.type_expr())
                });
                let ret = if self.eat(&TokenKind::Arrow) {
                    Some(Box::new(self.type_expr()))
                } else {
                    None
                };
                TypeExpr {
                    kind: TypeExprKind::Fn(params, ret),
                    span: start.to(self.prev_span()),
                }
            }
            TokenKind::KwDyn => {
                self.bump();
                match self.expect_ident("trait name after `dyn`") {
                    Some(name) => TypeExpr {
                        kind: TypeExprKind::Dyn(name),
                        span: start.to(self.prev_span()),
                    },
                    None => TypeExpr {
                        kind: TypeExprKind::Error,
                        span: start,
                    },
                }
            }
            TokenKind::Ident(name) => {
                let ident = Ident {
                    name,
                    span: self.bump().span,
                };
                if self.eat(&TokenKind::LBracket) {
                    let open = self.prev_span();
                    let args =
                        self.list(open, Brackets::Bracket, Sep::Comma, "type arguments", |p| {
                            Some(p.type_expr())
                        });
                    TypeExpr {
                        kind: TypeExprKind::App(ident, args),
                        span: start.to(self.prev_span()),
                    }
                } else {
                    TypeExpr {
                        span: ident.span,
                        kind: TypeExprKind::Name(ident),
                    }
                }
            }
            other => {
                let span = self.span();
                self.error(
                    "E0108",
                    span,
                    format!("expected a type, found {}", other.describe()),
                );
                TypeExpr {
                    kind: TypeExprKind::Error,
                    span,
                }
            }
        }
    }

    // -------------------------------------------------------- statements

    fn block(&mut self) -> Option<Block> {
        let open = self.expect(&TokenKind::LBrace, "`{` to start a block")?;
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            while self.eat(&TokenKind::Semi) {
                self.skip_newlines();
            }
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_eof() {
                self.error("E0100", open, "unclosed block: missing `}`");
                break;
            }
            let stmt = self.stmt();
            stmts.push(stmt);
        }
        Some(Block {
            stmts,
            span: open.to(self.prev_span()),
            id: self.id(),
        })
    }

    fn stmt(&mut self) -> Stmt {
        if self.at(&TokenKind::KwLet) {
            return self.let_stmt();
        }
        let expr = self.expr();
        if matches!(expr.kind, ExprKind::Error) {
            // Recovery: resync to a statement boundary.
            self.sync_to_stmt_end();
        }
        let terminated = self.terminate_stmt();
        Stmt::Expr { expr, terminated }
    }

    /// Consume a statement terminator; report if the statement is followed
    /// by something else on the same line. Returns true when an explicit
    /// `;` was used (which discards the value even in tail position).
    fn terminate_stmt(&mut self) -> bool {
        match self.kind() {
            TokenKind::Semi => {
                self.bump();
                true
            }
            TokenKind::Newline => {
                self.bump();
                false
            }
            TokenKind::RBrace | TokenKind::Eof => false,
            other => {
                let found = other.describe();
                let span = self.span();
                self.error_help(
                    "E0109",
                    span,
                    format!("expected end of statement, found {found}"),
                    "statements end at a newline; use `;` to put several on one line",
                );
                self.sync_to_stmt_end();
                self.eat(&TokenKind::Newline);
                false
            }
        }
    }

    fn let_stmt(&mut self) -> Stmt {
        let kw = self.bump().span; // `let`
        // Simple binding: `let name [: ty] = init`. Anything else is a
        // pattern and requires `else` (let-else, PRD §3.4).
        let simple = matches!(self.kind(), TokenKind::Ident(_))
            && matches!(self.nth_kind(1), TokenKind::Colon | TokenKind::Eq);
        if simple {
            let name = self.expect_ident("binding name").unwrap();
            let ty = if self.eat(&TokenKind::Colon) {
                Some(self.type_expr())
            } else {
                None
            };
            self.expect(&TokenKind::Eq, "`=` in `let`");
            let init = self.expr();
            // `let x = e else { ... }` with a plain binding is suspicious
            // but grammatical — the checker rejects irrefutable let-else.
            let span = kw.to(init.span);
            let id = self.id();
            self.terminate_stmt();
            return Stmt::Let {
                name,
                ty,
                init,
                span,
                id,
            };
        }
        let pat = self.pattern();
        self.expect(&TokenKind::Eq, "`=` in `let`");
        let init = self.expr_no_struct_lit();
        self.skip_newlines();
        if !self.at(&TokenKind::KwElse) {
            let span = kw.to(init.span);
            self.error_help(
                "E0110",
                span,
                "destructuring `let` requires an `else` block in v1",
                "write `let pat = expr else { ... }`; the else block must diverge \
                 (return, break, or continue)",
            );
            let id = self.id();
            return Stmt::LetElse {
                pat,
                init,
                else_block: Block {
                    stmts: vec![],
                    span,
                    id: self.id(),
                },
                span,
                id,
            };
        }
        self.bump(); // `else`
        self.skip_newlines();
        let else_block = self.block().unwrap_or_else(|| Block {
            stmts: vec![],
            span: self.span(),
            id: self.id(),
        });
        let span = kw.to(else_block.span);
        let id = self.id();
        self.terminate_stmt();
        Stmt::LetElse {
            pat,
            init,
            else_block,
            span,
            id,
        }
    }

    // -------------------------------------------------------- expressions

    fn expr(&mut self) -> Expr {
        self.assign_expr()
    }

    /// Parse with `f` scoped to a struct-literal restriction, restoring
    /// the previous one afterwards.
    ///
    /// The restriction is why `if p { }` parses as an `if` with an empty
    /// body rather than a struct literal `p { }`: in a header position a
    /// `{` starts the body. Delimiters re-enable it, because inside
    /// parens or brackets there is no body to be ambiguous with.
    ///
    /// A scoped call rather than eight hand-balanced save/restore pairs:
    /// a missed restore silently disabled struct literals for an entire
    /// subtree, with no diagnostic.
    fn with_struct_lits<T>(&mut self, allowed: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.no_struct_lit, !allowed);
        let out = f(self);
        self.no_struct_lit = saved;
        out
    }

    fn expr_no_struct_lit(&mut self) -> Expr {
        self.with_struct_lits(false, |p| p.assign_expr())
    }

    fn mk(&mut self, kind: ExprKind, span: Span) -> Expr {
        Expr {
            kind,
            span,
            id: self.id(),
        }
    }

    /// `[T, U: Ord]` — a fn declaration's type-parameter list (cursor on
    /// the `[`). Bounds are single identifiers; the checker validates
    /// them (only `Eq`, `Ord`, `Clone` in this release).
    fn type_param_list(&mut self) -> Vec<TypeParam> {
        let open = self.bump().span; // `[`
        let out = self.list(
            open,
            Brackets::Bracket,
            Sep::Comma,
            "type parameter list",
            |p| {
                let name = p.expect_ident("type parameter name")?;
                let bound = if p.eat(&TokenKind::Colon) {
                    p.expect_ident("bound name after `:` (e.g. `T: Ord`)")
                } else {
                    None
                };
                Some(TypeParam { name, bound })
            },
        );
        if out.is_empty() {
            self.error_help(
                "E0255",
                open.to(self.prev_span()),
                "empty type parameter list",
                "declare at least one parameter (`fn f[T](...)`) or drop the `[]`",
            );
        }
        out
    }

    /// Parse one interpolation hole's pre-lexed tokens (absolute spans,
    /// Eof-terminated) as an expression. Runs on the SAME parser so
    /// NodeIds stay unique and diagnostics accumulate; the outer token
    /// stream is swapped back afterwards.
    fn parse_hole(&mut self, tokens: Vec<Token>) -> Expr {
        let saved_tokens = std::mem::replace(&mut self.tokens, tokens);
        let saved_pos = std::mem::replace(&mut self.pos, 0);
        let saved_no_struct = std::mem::replace(&mut self.no_struct_lit, false);
        let expr = self.expr();
        if !self.at_eof() {
            let got = self.kind().describe();
            let span = self.span();
            self.error_help(
                "E0004",
                span,
                format!("unexpected {got} after the interpolated expression"),
                "an interpolation hole holds exactly one expression",
            );
        }
        self.tokens = saved_tokens;
        self.pos = saved_pos;
        self.no_struct_lit = saved_no_struct;
        expr
    }

    fn assign_expr(&mut self) -> Expr {
        if self.nesting_too_deep() {
            let span = self.span();
            return self.mk(ExprKind::Error, span);
        }
        self.depth += RECURSION_COST;
        let e = self.assign_expr_inner();
        self.depth -= RECURSION_COST;
        e
    }

    fn assign_expr_inner(&mut self) -> Expr {
        let lhs = self.range_expr();
        let op = match self.kind() {
            TokenKind::Eq => None,
            TokenKind::PlusEq => Some(BinOp::Add),
            TokenKind::MinusEq => Some(BinOp::Sub),
            TokenKind::StarEq => Some(BinOp::Mul),
            TokenKind::SlashEq => Some(BinOp::Div),
            TokenKind::PercentEq => Some(BinOp::Rem),
            _ => return lhs,
        };
        self.bump();
        self.skip_newlines();
        // Plain `=` chains right-associatively (`a = b = c`); compound
        // assignment does not chain (`a += b += c` is a type error: the
        // RHS would be unit).
        let value = if op.is_none() {
            self.assign_expr()
        } else {
            self.range_expr()
        };
        let span = lhs.span.to(value.span);
        self.mk(
            ExprKind::Assign {
                target: Box::new(lhs),
                value: Box::new(value),
                op,
            },
            span,
        )
    }

    fn range_expr(&mut self) -> Expr {
        let lo = self.binary_expr(0);
        let inclusive = match self.kind() {
            TokenKind::DotDot => false,
            TokenKind::DotDotEq => true,
            _ => return lo,
        };
        self.bump();
        self.skip_newlines();
        let hi = self.binary_expr(0);
        let span = lo.span.to(hi.span);
        self.mk(
            ExprKind::Range {
                lo: Box::new(lo),
                hi: Box::new(hi),
                inclusive,
            },
            span,
        )
    }

    /// Binding power of each binary operator, tightest last.
    ///
    /// Equality and ordering share a tier: `a < b == c` parses as
    /// `(a < b) == c`, left-associatively, not as `a < (b == c)`. That
    /// differs from C-family languages and is pinned by the `precedence`
    /// parser fixture.
    fn bin_op_prec(kind: &TokenKind) -> Option<(BinOp, u8)> {
        Some(match kind {
            TokenKind::OrOr => (BinOp::Or, 1),
            TokenKind::AndAnd => (BinOp::And, 2),
            TokenKind::EqEq => (BinOp::Eq, 3),
            TokenKind::NotEq => (BinOp::Ne, 3),
            TokenKind::Lt => (BinOp::Lt, 3),
            TokenKind::Le => (BinOp::Le, 3),
            TokenKind::Gt => (BinOp::Gt, 3),
            TokenKind::Ge => (BinOp::Ge, 3),
            TokenKind::Plus => (BinOp::Add, 4),
            TokenKind::Minus => (BinOp::Sub, 4),
            TokenKind::Star => (BinOp::Mul, 5),
            TokenKind::Slash => (BinOp::Div, 5),
            TokenKind::Percent => (BinOp::Rem, 5),
            _ => return None,
        })
    }

    /// Precedence climbing over [`Self::bin_op_prec`], replacing five
    /// byte-identical tier functions that differed only in their operator
    /// set and the tier they called.
    ///
    /// Operator chains deepen the AST without deepening the parse stack,
    /// so each link spends nesting budget too — returned when the chain
    /// ends, so sibling chains do not accumulate. Same in the postfix loop.
    fn binary_expr(&mut self, min_prec: u8) -> Expr {
        let mut lhs = self.unary_expr();
        let mut chain = 0;
        while let Some((op, prec)) = Self::bin_op_prec(self.kind()) {
            if prec < min_prec {
                break;
            }
            if self.nesting_too_deep() {
                break;
            }
            self.depth += 1;
            chain += 1;
            self.bump();
            self.skip_newlines();
            // `prec + 1`: every operator here is left-associative.
            let rhs = self.binary_expr(prec + 1);
            let span = lhs.span.to(rhs.span);
            lhs = self.mk(
                ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                span,
            );
        }
        self.depth -= chain;
        lhs
    }

    fn unary_expr(&mut self) -> Expr {
        let op = match self.kind() {
            TokenKind::Minus => Some(UnOp::Neg),
            TokenKind::Bang => Some(UnOp::Not),
            _ => None,
        };
        if let Some(op) = op {
            // Only an actual operator spends budget — this function is on
            // the path of every expression.
            if self.nesting_too_deep() {
                let span = self.span();
                return self.mk(ExprKind::Error, span);
            }
            self.depth += RECURSION_COST;
            let start = self.bump().span;
            let expr = self.unary_expr();
            self.depth -= RECURSION_COST;
            let span = start.to(expr.span);
            return self.mk(
                ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                span,
            );
        }
        self.postfix_expr()
    }

    fn postfix_expr(&mut self) -> Expr {
        let mut expr = self.primary_expr();
        let mut chain = 0;
        loop {
            match self.kind() {
                TokenKind::LParen | TokenKind::Dot | TokenKind::LBracket | TokenKind::Question => {
                    if self.nesting_too_deep() {
                        break;
                    }
                    self.depth += 1;
                    chain += 1;
                }
                _ => {}
            }
            match self.kind() {
                TokenKind::LParen => {
                    let open = self.bump().span;
                    let args = self.call_args(open);
                    let span = expr.span.to(self.prev_span());
                    expr = self.mk(
                        ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                        },
                        span,
                    );
                }
                TokenKind::Dot => {
                    let dot = self.bump().span;
                    let name = match self.expect_ident("method or field name after `.`") {
                        Some(n) => n,
                        None => {
                            // Keep the receiver in the tree — the LSP needs
                            // its type for `.` completions mid-typing. The
                            // empty name is given the (empty) span just
                            // past the dot, because that is where the
                            // member being completed would go, and that is
                            // where the cursor is (see `check::Index`).
                            let at = Span::new(dot.hi, dot.hi);
                            let span = expr.span.to(dot);
                            expr = self.mk(
                                ExprKind::Field {
                                    obj: Box::new(expr),
                                    name: Ident {
                                        name: String::new(),
                                        span: at,
                                    },
                                },
                                span,
                            );
                            break;
                        }
                    };
                    if self.eat(&TokenKind::LParen) {
                        let open = self.prev_span();
                        let args = self.call_args(open);
                        let span = expr.span.to(self.prev_span());
                        expr = self.mk(
                            ExprKind::MethodCall {
                                recv: Box::new(expr),
                                name,
                                args,
                            },
                            span,
                        );
                    } else {
                        let span = expr.span.to(name.span);
                        expr = self.mk(
                            ExprKind::Field {
                                obj: Box::new(expr),
                                name,
                            },
                            span,
                        );
                    }
                }
                TokenKind::LBracket => {
                    self.bump();
                    let idx = self.expr();
                    self.expect(&TokenKind::RBracket, "`]` to close the index");
                    let span = expr.span.to(self.prev_span());
                    expr = self.mk(
                        ExprKind::Index {
                            obj: Box::new(expr),
                            idx: Box::new(idx),
                        },
                        span,
                    );
                }
                TokenKind::Question => {
                    let q = self.bump().span;
                    let span = expr.span.to(q);
                    expr = self.mk(ExprKind::Try(Box::new(expr)), span);
                }
                // Leading-dot continuation: `expr\n    .method()` — the
                // next line cannot start a statement with `.`, so it is
                // unambiguously a continuation (documented in the grammar).
                TokenKind::Newline => {
                    if self.peek_through_newlines() == &TokenKind::Dot {
                        self.skip_newlines();
                        continue;
                    }
                    break;
                }
                _ => break,
            }
        }
        self.depth -= chain;
        expr
    }

    /// `open`: the span of the `(`, for an unclosed argument list.
    fn call_args(&mut self, open: Span) -> Vec<Expr> {
        self.list(open, Brackets::Paren, Sep::Comma, "argument list", |p| {
            // Struct literals are fine inside call parens.
            Some(p.with_struct_lits(true, |p| p.expr()))
        })
    }

    fn primary_expr(&mut self) -> Expr {
        let start = self.span();
        match self.kind().clone() {
            TokenKind::Int(n, suffix) => {
                self.bump();
                match suffix {
                    None => self.mk(ExprKind::IntLit(n), start),
                    Some(u) => {
                        let unit = Ident {
                            name: u,
                            span: start,
                        };
                        self.mk(
                            ExprKind::QuantityLit {
                                value: LitNum::Int(n),
                                unit,
                            },
                            start,
                        )
                    }
                }
            }
            TokenKind::Float(f, suffix) => {
                self.bump();
                match suffix {
                    None => self.mk(ExprKind::FloatLit(f), start),
                    Some(u) => {
                        let unit = Ident {
                            name: u,
                            span: start,
                        };
                        self.mk(
                            ExprKind::QuantityLit {
                                value: LitNum::Float(f),
                                unit,
                            },
                            start,
                        )
                    }
                }
            }
            TokenKind::Str(s) => {
                self.bump();
                self.mk(ExprKind::StrLit(s), start)
            }
            TokenKind::StrInterp(parts) => {
                self.bump();
                let parts: Vec<InterpPart> = parts
                    .into_iter()
                    .map(|p| match p {
                        crate::token::StrPart::Lit(s) => InterpPart::Lit(s),
                        crate::token::StrPart::Hole(tokens) => {
                            InterpPart::Hole(Box::new(self.parse_hole(tokens)))
                        }
                    })
                    .collect();
                self.mk(ExprKind::StrInterp(parts), start)
            }
            TokenKind::Char(c) => {
                self.bump();
                self.mk(ExprKind::CharLit(c), start)
            }
            TokenKind::KwTrue => {
                self.bump();
                self.mk(ExprKind::BoolLit(true), start)
            }
            TokenKind::KwFalse => {
                self.bump();
                self.mk(ExprKind::BoolLit(false), start)
            }
            TokenKind::KwSelf => {
                self.bump();
                self.mk(
                    ExprKind::Path(vec![Ident {
                        name: "self".into(),
                        span: start,
                    }]),
                    start,
                )
            }
            TokenKind::Ident(_) => self.path_or_struct_lit(),
            TokenKind::LParen => self.paren_expr(),
            TokenKind::LBracket => self.list_lit_expr(),
            TokenKind::HashBrace => self.map_lit_expr(),
            TokenKind::KwIf => self.if_expr(),
            TokenKind::KwMatch => self.match_expr(),
            TokenKind::KwWhile => self.while_expr(),
            TokenKind::KwLoop => {
                self.bump();
                self.skip_newlines();
                let body = self.block_or_error();
                let span = start.to(body.span);
                self.mk(ExprKind::Loop { body }, span)
            }
            TokenKind::KwFor => self.for_expr(),
            TokenKind::LBrace => {
                let block = self.block_or_error();
                let span = block.span;
                self.mk(ExprKind::Block(block), span)
            }
            TokenKind::KwBreak => {
                self.bump();
                self.mk(ExprKind::Break, start)
            }
            TokenKind::KwContinue => {
                self.bump();
                self.mk(ExprKind::Continue, start)
            }
            TokenKind::KwReturn => self.return_expr(),
            TokenKind::Pipe | TokenKind::OrOr => self.closure_expr(),
            other => {
                self.error(
                    "E0111",
                    start,
                    format!("expected an expression, found {}", other.describe()),
                );
                // Consume the offending token so parsing always advances
                // (statement-level recovery resyncs the rest).
                if !matches!(
                    other,
                    TokenKind::RBrace | TokenKind::Newline | TokenKind::Eof
                ) {
                    self.bump();
                }
                self.mk(ExprKind::Error, start)
            }
        }
    }

    fn paren_expr(&mut self) -> Expr {
        let start = self.bump().span; // `(`
        if self.eat(&TokenKind::RParen) {
            let span = start.to(self.prev_span());
            return self.mk(ExprKind::UnitLit, span);
        }
        let inner = self.with_struct_lits(true, |p| p.expr());
        self.expect(
            &TokenKind::RParen,
            "`)` to close the parenthesized expression",
        );
        inner
    }

    fn list_lit_expr(&mut self) -> Expr {
        let start = self.bump().span; // `[`
        let items = self.list(start, Brackets::Bracket, Sep::Comma, "list literal", |p| {
            Some(p.with_struct_lits(true, |p| p.expr()))
        });
        let span = start.to(self.prev_span());
        self.mk(ExprKind::ListLit(items), span)
    }

    fn map_lit_expr(&mut self) -> Expr {
        let start = self.bump().span; // `#{`
        let entries = self.list(start, Brackets::Brace, Sep::Comma, "map literal", |p| {
            p.with_struct_lits(true, |p| {
                let key = p.expr();
                p.expect(&TokenKind::Colon, "`:` between map key and value");
                Some((key, p.expr()))
            })
        });
        let span = start.to(self.prev_span());
        self.mk(ExprKind::MapLit(entries), span)
    }

    fn while_expr(&mut self) -> Expr {
        let start = self.bump().span; // `while`
        let cond = self.expr_no_struct_lit();
        self.skip_newlines();
        let body = self.block_or_error();
        let span = start.to(body.span);
        self.mk(
            ExprKind::While {
                cond: Box::new(cond),
                body,
            },
            span,
        )
    }

    fn for_expr(&mut self) -> Expr {
        let start = self.bump().span; // `for`
        let var = self
            .expect_ident("loop variable after `for`")
            .unwrap_or(Ident {
                name: "_".into(),
                span: start,
            });
        self.expect(&TokenKind::KwIn, "`in` in `for` loop");
        let iter = self.expr_no_struct_lit();
        self.skip_newlines();
        let body = self.block_or_error();
        let span = start.to(body.span);
        self.mk(
            ExprKind::For {
                var,
                iter: Box::new(iter),
                body,
            },
            span,
        )
    }

    fn return_expr(&mut self) -> Expr {
        let start = self.bump().span; // `return`
        let value = if matches!(
            self.kind(),
            TokenKind::Newline
                | TokenKind::Semi
                | TokenKind::RBrace
                | TokenKind::RParen
                | TokenKind::Comma
                | TokenKind::Eof
        ) {
            None
        } else {
            Some(Box::new(self.expr()))
        };
        let span = match &value {
            Some(v) => start.to(v.span),
            None => start,
        };
        self.mk(ExprKind::Return(value), span)
    }

    fn block_or_error(&mut self) -> Block {
        self.block().unwrap_or_else(|| Block {
            stmts: vec![],
            span: self.span(),
            id: self.id(),
        })
    }

    fn path_or_struct_lit(&mut self) -> Expr {
        let start = self.span();
        let mut segments = vec![self.expect_ident("identifier").unwrap()];
        while self.at(&TokenKind::ColonColon) {
            self.bump();
            match self.expect_ident("identifier after `::`") {
                Some(seg) => segments.push(seg),
                None => break,
            }
        }
        // Struct literal? Only when `{` follows and we're not in a
        // condition/scrutinee header position.
        if self.at(&TokenKind::LBrace) && !self.no_struct_lit {
            let open = self.bump().span;
            let fields = self.list(open, Brackets::Brace, Sep::Comma, "struct literal", |p| {
                let name = p.expect_ident("field name")?;
                let value = if p.eat(&TokenKind::Colon) {
                    p.expr()
                } else {
                    // Field shorthand: `Point { x, y }`.
                    let id = p.id();
                    Expr {
                        kind: ExprKind::Path(vec![name.clone()]),
                        span: name.span,
                        id,
                    }
                };
                Some((name, value))
            });
            let span = start.to(self.prev_span());
            return self.mk(
                ExprKind::StructLit {
                    path: segments,
                    fields,
                },
                span,
            );
        }
        let span = start.to(self.prev_span());
        self.mk(ExprKind::Path(segments), span)
    }

    fn if_expr(&mut self) -> Expr {
        // Guarded separately: `else if` chains recurse via `else_tail`
        // without passing through `assign_expr`.
        if self.nesting_too_deep() {
            let span = self.span();
            return self.mk(ExprKind::Error, span);
        }
        self.depth += RECURSION_COST;
        let e = self.if_expr_inner();
        self.depth -= RECURSION_COST;
        e
    }

    fn if_expr_inner(&mut self) -> Expr {
        let start = self.bump().span; // `if`
        // `if let pat = expr { ... }`
        if self.at(&TokenKind::KwLet) {
            self.bump();
            let pat = self.pattern();
            self.expect(&TokenKind::Eq, "`=` in `if let`");
            let scrutinee = self.expr_no_struct_lit();
            self.skip_newlines();
            let then = self.block_or_error();
            let else_ = self.else_tail();
            let span = start.to(self.prev_span());
            return self.mk(
                ExprKind::IfLet {
                    pat,
                    scrutinee: Box::new(scrutinee),
                    then,
                    else_,
                },
                span,
            );
        }
        let cond = self.expr_no_struct_lit();
        self.skip_newlines();
        let then = self.block_or_error();
        let else_ = self.else_tail();
        let span = start.to(self.prev_span());
        self.mk(
            ExprKind::If {
                cond: Box::new(cond),
                then,
                else_,
            },
            span,
        )
    }

    /// Parse an optional `else` clause, looking through newlines (an `else`
    /// on the next line continues the `if` — documented continuation rule).
    fn else_tail(&mut self) -> Option<Box<Expr>> {
        if self.peek_through_newlines() != &TokenKind::KwElse {
            return None;
        }
        self.skip_newlines();
        self.bump(); // `else`
        self.skip_newlines();
        if self.at(&TokenKind::KwIf) {
            return Some(Box::new(self.if_expr()));
        }
        let block = self.block_or_error();
        let span = block.span;
        Some(Box::new(self.mk(ExprKind::Block(block), span)))
    }

    fn match_expr(&mut self) -> Expr {
        let start = self.bump().span; // `match`
        let scrutinee = self.expr_no_struct_lit();
        self.skip_newlines();
        let open = self
            .expect(&TokenKind::LBrace, "`{` to start the match arms")
            .unwrap_or(start);
        // Arms are separated by `,` and/or a newline.
        let arms = self.list(
            open,
            Brackets::Brace,
            Sep::CommaOrNewline,
            "match expression",
            |p| {
                let arm_start = p.span();
                let pat = p.pattern();
                let guard = if p.eat(&TokenKind::KwIf) {
                    Some(p.expr_no_struct_lit())
                } else {
                    None
                };
                p.expect(&TokenKind::FatArrow, "`=>` after the match pattern");
                p.skip_newlines();
                let body = p.expr();
                let span = arm_start.to(body.span);
                Some(MatchArm {
                    pat,
                    guard,
                    body,
                    span,
                })
            },
        );
        let span = start.to(self.prev_span());
        self.mk(
            ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span,
        )
    }

    fn closure_expr(&mut self) -> Expr {
        let start = self.span();
        let mut params = Vec::new();
        if self.eat(&TokenKind::OrOr) {
            // `||` — empty parameter list.
        } else {
            let open = self.bump().span; // `|`
            params = self.list(
                open,
                Brackets::Pipe,
                Sep::Comma,
                "closure parameters",
                |p| {
                    let name = p.expect_ident("closure parameter")?;
                    let ty = if p.eat(&TokenKind::Colon) {
                        Some(p.type_expr())
                    } else {
                        None
                    };
                    Some((name, ty))
                },
            );
        }
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.type_expr())
        } else {
            None
        };
        if ret.is_some() && self.peek_through_newlines() != &TokenKind::LBrace {
            let span = self.span();
            self.error_help(
                "E0112",
                span,
                "a closure with a declared return type needs a block body",
                "write `|x| -> int { ... }`",
            );
        }
        let body = self.expr();
        let span = start.to(body.span);
        self.mk(
            ExprKind::Closure {
                params,
                ret,
                body: Box::new(body),
            },
            span,
        )
    }

    // ------------------------------------------------------------ patterns

    fn pattern(&mut self) -> Pattern {
        let first = self.pattern_single();
        if !self.at(&TokenKind::Pipe) {
            return first;
        }
        let mut alts = vec![first];
        while self.eat(&TokenKind::Pipe) {
            self.skip_newlines();
            alts.push(self.pattern_single());
        }
        let span = alts[0].span.to(alts[alts.len() - 1].span);
        Pattern {
            span,
            id: self.id(),
            kind: PatternKind::Or(alts),
        }
    }

    fn pattern_single(&mut self) -> Pattern {
        if self.nesting_too_deep() {
            return Pattern {
                span: self.span(),
                id: self.id(),
                kind: PatternKind::Error,
            };
        }
        self.depth += RECURSION_COST;
        let p = self.pattern_single_inner();
        self.depth -= RECURSION_COST;
        p
    }

    fn pattern_single_inner(&mut self) -> Pattern {
        let start = self.span();
        let kind = match self.kind().clone() {
            TokenKind::Underscore => {
                self.bump();
                PatternKind::Wildcard
            }
            TokenKind::Int(n, suffix) => {
                self.bump();
                match suffix {
                    None => PatternKind::IntLit(n),
                    Some(u) => PatternKind::QuantityLit {
                        value: LitNum::Int(n),
                        unit: Ident {
                            name: u,
                            span: start,
                        },
                    },
                }
            }
            TokenKind::Float(f, Some(u)) => {
                self.bump();
                PatternKind::QuantityLit {
                    value: LitNum::Float(f),
                    unit: Ident {
                        name: u,
                        span: start,
                    },
                }
            }
            TokenKind::Minus => {
                self.bump();
                match self.kind().clone() {
                    TokenKind::Int(n, None) => {
                        self.bump();
                        PatternKind::IntLit(-n)
                    }
                    TokenKind::Int(n, Some(u)) => {
                        self.bump();
                        PatternKind::QuantityLit {
                            value: LitNum::Int(-n),
                            unit: Ident {
                                name: u,
                                span: start,
                            },
                        }
                    }
                    TokenKind::Float(f, Some(u)) => {
                        self.bump();
                        PatternKind::QuantityLit {
                            value: LitNum::Float(-f),
                            unit: Ident {
                                name: u,
                                span: start,
                            },
                        }
                    }
                    other => {
                        let span = self.span();
                        self.error(
                            "E0113",
                            span,
                            format!(
                                "expected integer after `-` in pattern, found {}",
                                other.describe()
                            ),
                        );
                        PatternKind::Error
                    }
                }
            }
            TokenKind::KwTrue => {
                self.bump();
                PatternKind::BoolLit(true)
            }
            TokenKind::KwFalse => {
                self.bump();
                PatternKind::BoolLit(false)
            }
            TokenKind::Char(c) => {
                self.bump();
                PatternKind::CharLit(c)
            }
            TokenKind::Str(s) => {
                self.bump();
                PatternKind::StrLit(s)
            }
            TokenKind::Ident(_) => {
                let mut segments = vec![self.expect_ident("identifier").unwrap()];
                while self.at(&TokenKind::ColonColon) {
                    self.bump();
                    match self.expect_ident("identifier after `::`") {
                        Some(seg) => segments.push(seg),
                        None => break,
                    }
                }
                if self.eat(&TokenKind::LParen) {
                    // tuple variant pattern
                    let open = self.prev_span();
                    let pats = self.list(open, Brackets::Paren, Sep::Comma, "pattern", |p| {
                        Some(p.pattern())
                    });
                    PatternKind::Variant {
                        path: segments,
                        args: VariantPatArgs::Tuple(pats),
                    }
                } else if self.at(&TokenKind::LBrace) {
                    let open = self.bump().span;
                    let (fields, has_rest) = self.struct_pattern_fields(open);
                    // `Name { ... }` — struct or struct-variant pattern;
                    // the checker disambiguates by what `Name` resolves to.
                    if segments.len() >= 2 {
                        PatternKind::Variant {
                            path: segments,
                            args: VariantPatArgs::Struct { fields, has_rest },
                        }
                    } else {
                        PatternKind::Struct {
                            path: segments,
                            fields,
                            has_rest,
                        }
                    }
                } else if segments.len() > 1 {
                    PatternKind::Variant {
                        path: segments,
                        args: VariantPatArgs::Unit,
                    }
                } else {
                    // Single identifier: binding, or a bare unit-variant
                    // name (`None`) — resolved by the checker.
                    PatternKind::Binding(segments.pop().unwrap())
                }
            }
            other => {
                self.error(
                    "E0113",
                    start,
                    format!("expected a pattern, found {}", other.describe()),
                );
                if !matches!(
                    other,
                    TokenKind::RBrace | TokenKind::Newline | TokenKind::Eof | TokenKind::FatArrow
                ) {
                    self.bump();
                }
                PatternKind::Error
            }
        };
        Pattern {
            kind,
            span: start.to(self.prev_span()),
            id: self.id(),
        }
    }

    /// `open`: the span of the `{`, for an unclosed struct pattern.
    fn struct_pattern_fields(&mut self, open: Span) -> (Vec<(Ident, Pattern)>, bool) {
        let mut has_rest = false;
        let fields = self.list(open, Brackets::Brace, Sep::Comma, "struct pattern", |p| {
            // `..` stands for every remaining field, so nothing may follow
            // it. Returning `None` hands the rest of the list back to the
            // combinator, which closes it on the `}` we left in place.
            if p.eat(&TokenKind::DotDot) {
                has_rest = true;
                p.skip_newlines();
                if !p.at(&TokenKind::RBrace) {
                    p.expect(&TokenKind::RBrace, "`}` after `..` in pattern");
                }
                return None;
            }
            let name = p.expect_ident("field name in pattern")?;
            let pat = if p.eat(&TokenKind::Colon) {
                p.pattern()
            } else {
                // Shorthand `Point { x }` desugars to `x: x` (a binding).
                Pattern {
                    kind: PatternKind::Binding(name.clone()),
                    span: name.span,
                    id: p.id(),
                }
            };
            Some((name, pat))
        });
        (fields, has_rest)
    }
}
