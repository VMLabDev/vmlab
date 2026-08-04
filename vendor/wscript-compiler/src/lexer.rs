//! Hand-written lexer.
//!
//! Statement termination (PRD §3.1, precise rule): the lexer emits a
//! `Newline` token at each physical line break **except** when the break is
//! inside an unclosed `(` or `[` delimiter (where expressions obviously
//! continue). Consecutive breaks collapse into one token. The parser then
//! treats `Newline` as a statement terminator unless the construct it is
//! parsing cannot end there (see `parser.rs` for the two continuation
//! special cases: leading `.` method chains and `else` after `}`).

use wscript_core::diag::Diagnostic;
use wscript_core::span::Span;

use crate::token::{StrPart, Token, TokenKind};

pub struct LexOutput {
    pub tokens: Vec<Token>,
    pub diags: Vec<Diagnostic>,
}

pub fn lex(src: &str) -> LexOutput {
    lex_at(src, 0)
}

/// Lex with every span offset by `base` — one file of a multi-file
/// compilation occupies `[base, base + len]` of the global span address
/// space (see `wscript_core::source_map`).
pub fn lex_at(src: &str, base: u32) -> LexOutput {
    let mut out = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        tokens: Vec::new(),
        diags: Vec::new(),
        delims: Vec::new(),
    }
    .run();
    if base != 0 {
        for t in &mut out.tokens {
            shift_token(t, base);
        }
        for d in &mut out.diags {
            d.span = d.span.shifted(base);
            for (span, _) in &mut d.labels {
                *span = span.shifted(base);
            }
        }
    }
    out
}

/// Shift a token's span (recursing into interpolation holes' pre-lexed
/// token vectors).
fn shift_token(t: &mut Token, base: u32) {
    t.span = t.span.shifted(base);
    if let TokenKind::StrInterp(parts) = &mut t.kind {
        for p in parts {
            if let StrPart::Hole(tokens) = p {
                for ht in tokens {
                    shift_token(ht, base);
                }
            }
        }
    }
}

#[derive(PartialEq)]
enum Delim {
    Paren,
    Bracket,
    Brace,
}

struct Lexer<'s> {
    src: &'s str,
    bytes: &'s [u8],
    pos: usize,
    tokens: Vec<Token>,
    diags: Vec<Diagnostic>,
    delims: Vec<Delim>,
}

impl<'s> Lexer<'s> {
    fn run(mut self) -> LexOutput {
        while self.pos < self.bytes.len() {
            let start = self.pos;
            let c = self.bytes[self.pos];
            match c {
                b' ' | b'\t' | b'\r' => {
                    self.pos += 1;
                }
                b'\n' => {
                    self.pos += 1;
                    self.emit_newline(start);
                }
                b'/' if self.peek(1) == Some(b'/') => {
                    let is_doc = self.peek(2) == Some(b'/') && self.peek(3) != Some(b'/');
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                    if is_doc {
                        let text = self.src[start + 3..self.pos].trim().to_string();
                        self.push(TokenKind::DocComment(text), start);
                    }
                }
                b'/' if self.peek(1) == Some(b'*') => self.block_comment(start),
                b'"' => self.string_lit(start),
                b'\'' => self.char_lit(start),
                b'0'..=b'9' => self.number(start),
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.ident(start),
                _ => self.punct(start),
            }
        }
        // A trailing newline before EOF keeps "last statement" handling
        // uniform in the parser.
        let end = self.src.len() as u32;
        if !matches!(
            self.tokens.last().map(|t| &t.kind),
            Some(TokenKind::Newline) | None
        ) {
            self.tokens.push(Token {
                kind: TokenKind::Newline,
                span: Span::new(end, end),
            });
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(end, end),
        });
        LexOutput {
            tokens: self.tokens,
            diags: self.diags,
        }
    }

    fn peek(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(start as u32, self.pos as u32)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        let span = self.span_from(start);
        self.tokens.push(Token { kind, span });
    }

    /// Newlines are statement-significant except inside `(` / `[`.
    fn newline_significant(&self) -> bool {
        !matches!(
            self.delims.last(),
            Some(Delim::Paren) | Some(Delim::Bracket)
        )
    }

    fn emit_newline(&mut self, start: usize) {
        if self.newline_significant()
            && !matches!(
                self.tokens.last().map(|t| &t.kind),
                Some(TokenKind::Newline)
            )
        {
            self.push(TokenKind::Newline, start);
        }
    }

    fn block_comment(&mut self, start: usize) {
        self.pos += 2;
        let mut depth = 1usize;
        let mut saw_newline = false;
        while self.pos < self.bytes.len() && depth > 0 {
            match self.bytes[self.pos] {
                b'/' if self.peek(1) == Some(b'*') => {
                    depth += 1;
                    self.pos += 2;
                }
                b'*' if self.peek(1) == Some(b'/') => {
                    depth -= 1;
                    self.pos += 2;
                }
                b'\n' => {
                    saw_newline = true;
                    self.pos += 1;
                }
                _ => self.pos += 1,
            }
        }
        if depth > 0 {
            self.diags.push(
                Diagnostic::error("E0001", self.span_from(start), "unterminated block comment")
                    .with_help("close the comment with `*/`"),
            );
        }
        // A block comment spanning lines still separates statements.
        if saw_newline {
            self.emit_newline(self.pos.saturating_sub(1));
        }
    }

    fn string_lit(&mut self, start: usize) {
        self.pos += 1;
        let mut value = String::new();
        let mut parts: Vec<StrPart> = Vec::new();
        loop {
            match self.bytes.get(self.pos) {
                None | Some(b'\n') => {
                    self.diags.push(
                        Diagnostic::error(
                            "E0002",
                            self.span_from(start),
                            "unterminated string literal",
                        )
                        .with_help("close the string with `\"` before the end of the line"),
                    );
                    break;
                }
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    if let Some(c) = self.escape(start) {
                        value.push(c);
                    }
                }
                // Interpolation. Escapes and fmt-template compatibility:
                // `{{`/`}}` are literal braces, `{}` and `{:spec}` stay
                // literal text (fmt placeholders), `{expr}` is a hole.
                Some(b'{') if self.peek(1) == Some(b'{') => {
                    value.push('{');
                    self.pos += 2;
                }
                Some(b'}') if self.peek(1) == Some(b'}') => {
                    value.push('}');
                    self.pos += 2;
                }
                Some(b'{') if matches!(self.peek(1), Some(b'}') | Some(b':')) => {
                    value.push('{');
                    self.pos += 1;
                }
                Some(b'{') => {
                    self.pos += 1;
                    if !value.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut value)));
                    }
                    parts.push(StrPart::Hole(self.lex_hole(start)));
                }
                Some(_) => {
                    let c = self.src[self.pos..].chars().next().unwrap();
                    value.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
        if parts.is_empty() {
            // Fast path: a plain string — identical tokens to before.
            self.push(TokenKind::Str(value), start);
        } else {
            if !value.is_empty() {
                parts.push(StrPart::Lit(value));
            }
            self.push(TokenKind::StrInterp(parts), start);
        }
    }

    /// Lex one `{expr}` interpolation hole (cursor just past the `{`).
    /// The hole's tokens are lexed with the main dispatcher — nested
    /// strings (and their own holes) recurse naturally — and returned
    /// with absolute spans, `Eof`-terminated. Ends at the matching `}`
    /// at brace depth 0. A single `:` at depth 0 is reserved for future
    /// format specs and rejected.
    fn lex_hole(&mut self, lit_start: usize) -> Vec<Token> {
        let mark = self.tokens.len();
        let mut depth = 0i32;
        loop {
            let Some(c) = self.bytes.get(self.pos).copied() else {
                self.diags.push(
                    Diagnostic::error(
                        "E0004",
                        self.span_from(lit_start),
                        "unterminated `{` interpolation hole",
                    )
                    .with_help("close the hole with `}` (escape a literal brace as `{{`)"),
                );
                break;
            };
            match c {
                b'}' if depth == 0 => {
                    self.pos += 1;
                    break;
                }
                b'\n' => {
                    self.diags.push(
                        Diagnostic::error(
                            "E0004",
                            self.span_from(lit_start),
                            "unterminated `{` interpolation hole",
                        )
                        .with_help("interpolation holes cannot span lines"),
                    );
                    break;
                }
                b':' if depth == 0 && self.peek(1) != Some(b':') => {
                    // Reserved: `{value:spec}` format specs.
                    let spec_start = self.pos;
                    while let Some(c) = self.bytes.get(self.pos).copied() {
                        if c == b'}' || c == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                    if self.bytes.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                    }
                    self.diags.push(
                        Diagnostic::error(
                            "E0004",
                            Span::new(spec_start as u32, self.pos as u32),
                            "format specs are not supported in interpolation yet",
                        )
                        .with_help("use `fmt(\"{:spec}\", value)` for width/precision formatting"),
                    );
                    break;
                }
                b' ' | b'\t' | b'\r' => {
                    self.pos += 1;
                }
                _ => {
                    // One token via the main dispatcher (handles nested
                    // strings, chars, numbers, idents, punctuation).
                    let start = self.pos;
                    match c {
                        b'"' => self.string_lit(start),
                        b'\'' => self.char_lit(start),
                        b'0'..=b'9' => self.number(start),
                        b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.ident(start),
                        _ => self.punct(start),
                    }
                    if self.pos == start {
                        // Defensive: dispatcher made no progress.
                        self.pos += 1;
                    }
                    // Track brace depth from the hole's tokens (holes are
                    // short; the rescan is cheap and simple).
                    depth = self.tokens[mark..]
                        .iter()
                        .map(|t| match t.kind {
                            TokenKind::LBrace | TokenKind::HashBrace => 1,
                            TokenKind::RBrace => -1,
                            _ => 0,
                        })
                        .sum();
                }
            }
        }
        let mut toks = self.tokens.split_off(mark);
        let end = self.pos as u32;
        toks.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(end.saturating_sub(1), end.saturating_sub(1)),
        });
        toks
    }

    /// Consume a `\x` escape (cursor on the backslash).
    fn escape(&mut self, lit_start: usize) -> Option<char> {
        self.pos += 1; // backslash
        let c = match self.bytes.get(self.pos) {
            None => return None,
            Some(c) => *c,
        };
        self.pos += 1;
        match c {
            b'n' => Some('\n'),
            b't' => Some('\t'),
            b'r' => Some('\r'),
            b'0' => Some('\0'),
            b'\\' => Some('\\'),
            b'"' => Some('"'),
            b'\'' => Some('\''),
            b'u' => {
                // \u{1F600}
                if self.bytes.get(self.pos) != Some(&b'{') {
                    self.diags.push(
                        Diagnostic::error(
                            "E0003",
                            self.span_from(lit_start),
                            "invalid unicode escape: expected `\\u{...}`",
                        )
                        .with_help("write the code point in braces, e.g. `\\u{1F600}`"),
                    );
                    return None;
                }
                self.pos += 1;
                let hex_start = self.pos;
                while self
                    .bytes
                    .get(self.pos)
                    .is_some_and(|c| c.is_ascii_hexdigit())
                {
                    self.pos += 1;
                }
                let hex = &self.src[hex_start..self.pos];
                if self.bytes.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                } else {
                    self.diags.push(
                        Diagnostic::error(
                            "E0003",
                            self.span_from(lit_start),
                            "invalid unicode escape: missing closing `}`",
                        )
                        .with_help("write the code point in braces, e.g. `\\u{1F600}`"),
                    );
                    return None;
                }
                u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .or_else(|| {
                        self.diags.push(Diagnostic::error(
                            "E0003",
                            self.span_from(lit_start),
                            format!("`\\u{{{hex}}}` is not a valid unicode code point"),
                        ));
                        None
                    })
            }
            other => {
                self.diags.push(
                    Diagnostic::error(
                        "E0004",
                        self.span_from(lit_start),
                        format!("unknown escape sequence `\\{}`", other as char),
                    )
                    .with_help("supported escapes: \\n \\t \\r \\0 \\\\ \\\" \\' \\u{...}"),
                );
                None
            }
        }
    }

    fn char_lit(&mut self, start: usize) {
        self.pos += 1;
        let value = match self.bytes.get(self.pos) {
            Some(b'\\') => self.escape(start),
            Some(b'\'') | None => None,
            Some(_) => {
                let c = self.src[self.pos..].chars().next().unwrap();
                self.pos += c.len_utf8();
                Some(c)
            }
        };
        if self.bytes.get(self.pos) == Some(&b'\'') {
            self.pos += 1;
        } else {
            self.diags.push(
                Diagnostic::error("E0005", self.span_from(start), "unterminated char literal")
                    .with_help("char literals hold exactly one character, e.g. `'a'`"),
            );
        }
        match value {
            Some(c) => self.push(TokenKind::Char(c), start),
            None => {
                self.diags.push(
                    Diagnostic::error("E0005", self.span_from(start), "empty char literal")
                        .with_help("char literals hold exactly one character, e.g. `'a'`"),
                );
                self.push(TokenKind::Char('\0'), start);
            }
        }
    }

    fn number(&mut self, start: usize) {
        if self.bytes[self.pos] == b'0' && matches!(self.peek(1), Some(b'x') | Some(b'X')) {
            self.pos += 2;
            let digits_start = self.pos;
            while self
                .bytes
                .get(self.pos)
                .is_some_and(|c| c.is_ascii_hexdigit() || *c == b'_')
            {
                self.pos += 1;
            }
            let digits: String = self.src[digits_start..self.pos]
                .chars()
                .filter(|c| *c != '_')
                .collect();
            let suffix = self.unit_suffix();
            match i64::from_str_radix(&digits, 16) {
                Ok(n) => self.push(TokenKind::Int(n, suffix), start),
                Err(_) => {
                    self.diags.push(
                        Diagnostic::error("E0006", self.span_from(start), "invalid hex literal")
                            .with_help("hex literals look like `0xFF` and must fit in 64 bits"),
                    );
                    self.push(TokenKind::Int(0, None), start);
                }
            }
            return;
        }
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_digit() || *c == b'_')
        {
            self.pos += 1;
        }
        let mut is_float = false;
        // `1.5` is a float; `1..5` is a range; `1.foo()` is a method call.
        if self.bytes.get(self.pos) == Some(&b'.')
            && self.peek(1).is_some_and(|c| c.is_ascii_digit())
        {
            is_float = true;
            self.pos += 1;
            while self
                .bytes
                .get(self.pos)
                .is_some_and(|c| c.is_ascii_digit() || *c == b'_')
            {
                self.pos += 1;
            }
        }
        if matches!(self.bytes.get(self.pos), Some(b'e') | Some(b'E'))
            && self
                .peek(1)
                .is_some_and(|c| c.is_ascii_digit() || c == b'+' || c == b'-')
        {
            is_float = true;
            self.pos += 2;
            while self.bytes.get(self.pos).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text: String = self.src[start..self.pos]
            .chars()
            .filter(|c| *c != '_')
            .collect();
        let suffix = self.unit_suffix();
        if is_float {
            match text.parse::<f64>() {
                Ok(f) => self.push(TokenKind::Float(f, suffix), start),
                Err(_) => {
                    self.diags.push(Diagnostic::error(
                        "E0006",
                        self.span_from(start),
                        "invalid float literal",
                    ));
                    self.push(TokenKind::Float(0.0, None), start);
                }
            }
        } else {
            match text.parse::<i64>() {
                Ok(n) => self.push(TokenKind::Int(n, suffix), start),
                Err(_) => {
                    self.diags.push(
                        Diagnostic::error(
                            "E0006",
                            self.span_from(start),
                            "integer literal too large",
                        )
                        .with_help("`int` is a signed 64-bit integer"),
                    );
                    self.push(TokenKind::Int(0, None), start);
                }
            }
        }
    }

    /// A unit suffix glued to a numeric literal (`500ms`, `4MiB`).
    ///
    /// Must start with an ASCII letter, so digit separators (`1_000`) are
    /// never mistaken for one. Nothing is lost by claiming these bytes: a
    /// number followed directly by an identifier was two tokens no
    /// production accepted.
    fn unit_suffix(&mut self) -> Option<String> {
        if !self
            .bytes
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_alphabetic())
        {
            return None;
        }
        let start = self.pos;
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            self.pos += 1;
        }
        Some(self.src[start..self.pos].to_string())
    }

    fn ident(&mut self, start: usize) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        let kind = match text {
            "let" => TokenKind::KwLet,
            "fn" => TokenKind::KwFn,
            "struct" => TokenKind::KwStruct,
            "enum" => TokenKind::KwEnum,
            "trait" => TokenKind::KwTrait,
            "impl" => TokenKind::KwImpl,
            "for" => TokenKind::KwFor,
            "in" => TokenKind::KwIn,
            "while" => TokenKind::KwWhile,
            "loop" => TokenKind::KwLoop,
            "if" => TokenKind::KwIf,
            "else" => TokenKind::KwElse,
            "match" => TokenKind::KwMatch,
            "return" => TokenKind::KwReturn,
            "break" => TokenKind::KwBreak,
            "continue" => TokenKind::KwContinue,
            "use" => TokenKind::KwUse,
            "true" => TokenKind::KwTrue,
            "false" => TokenKind::KwFalse,
            "dyn" => TokenKind::KwDyn,
            "mod" => TokenKind::KwMod,
            "const" => TokenKind::KwConst,
            "self" => TokenKind::KwSelf,
            "_" => TokenKind::Underscore,
            _ => TokenKind::Ident(text.to_string()),
        };
        self.push(kind, start);
    }

    fn punct(&mut self, start: usize) {
        let c = self.bytes[self.pos];
        let two = self.peek(1);
        let (kind, len) = match (c, two) {
            (b':', Some(b':')) => (TokenKind::ColonColon, 2),
            (b'-', Some(b'>')) => (TokenKind::Arrow, 2),
            (b'=', Some(b'>')) => (TokenKind::FatArrow, 2),
            (b'=', Some(b'=')) => (TokenKind::EqEq, 2),
            (b'!', Some(b'=')) => (TokenKind::NotEq, 2),
            (b'<', Some(b'=')) => (TokenKind::Le, 2),
            (b'>', Some(b'=')) => (TokenKind::Ge, 2),
            (b'&', Some(b'&')) => (TokenKind::AndAnd, 2),
            (b'|', Some(b'|')) => (TokenKind::OrOr, 2),
            (b'+', Some(b'=')) => (TokenKind::PlusEq, 2),
            (b'-', Some(b'=')) => (TokenKind::MinusEq, 2),
            (b'*', Some(b'=')) => (TokenKind::StarEq, 2),
            (b'/', Some(b'=')) => (TokenKind::SlashEq, 2),
            (b'%', Some(b'=')) => (TokenKind::PercentEq, 2),
            (b'.', Some(b'.')) => {
                if self.peek(2) == Some(b'=') {
                    (TokenKind::DotDotEq, 3)
                } else {
                    (TokenKind::DotDot, 2)
                }
            }
            (b'#', Some(b'{')) => (TokenKind::HashBrace, 2),
            (b'(', _) => (TokenKind::LParen, 1),
            (b')', _) => (TokenKind::RParen, 1),
            (b'{', _) => (TokenKind::LBrace, 1),
            (b'}', _) => (TokenKind::RBrace, 1),
            (b'[', _) => (TokenKind::LBracket, 1),
            (b']', _) => (TokenKind::RBracket, 1),
            (b'#', _) => (TokenKind::Hash, 1),
            (b',', _) => (TokenKind::Comma, 1),
            (b'.', _) => (TokenKind::Dot, 1),
            (b':', _) => (TokenKind::Colon, 1),
            (b';', _) => (TokenKind::Semi, 1),
            (b'=', _) => (TokenKind::Eq, 1),
            (b'<', _) => (TokenKind::Lt, 1),
            (b'>', _) => (TokenKind::Gt, 1),
            (b'+', _) => (TokenKind::Plus, 1),
            (b'-', _) => (TokenKind::Minus, 1),
            (b'*', _) => (TokenKind::Star, 1),
            (b'/', _) => (TokenKind::Slash, 1),
            (b'%', _) => (TokenKind::Percent, 1),
            (b'!', _) => (TokenKind::Bang, 1),
            (b'|', _) => (TokenKind::Pipe, 1),
            (b'?', _) => (TokenKind::Question, 1),
            _ => {
                let ch = self.src[self.pos..].chars().next().unwrap();
                self.pos += ch.len_utf8();
                self.diags.push(
                    Diagnostic::error(
                        "E0007",
                        self.span_from(start),
                        format!("unexpected character `{ch}`"),
                    )
                    .with_help("this character is not part of wscript's syntax"),
                );
                return;
            }
        };
        self.pos += len;
        // Track delimiter nesting for newline significance.
        match kind {
            TokenKind::LParen => self.delims.push(Delim::Paren),
            TokenKind::LBracket => self.delims.push(Delim::Bracket),
            TokenKind::LBrace | TokenKind::HashBrace => self.delims.push(Delim::Brace),
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                self.delims.pop();
            }
            _ => {}
        }
        self.push(kind, start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).tokens.into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn newline_suppressed_in_parens() {
        let ks = kinds("f(1,\n2)");
        assert!(!ks[..ks.len() - 2].contains(&TokenKind::Newline));
    }

    #[test]
    fn newline_significant_in_braces() {
        let ks = kinds("{ a\nb }");
        assert!(ks.contains(&TokenKind::Newline));
    }

    #[test]
    fn range_vs_float() {
        assert_eq!(
            kinds("1..5")[..3],
            [
                TokenKind::Int(1, None),
                TokenKind::DotDot,
                TokenKind::Int(5, None)
            ]
        );
        assert_eq!(kinds("1.5")[0], TokenKind::Float(1.5, None));
    }

    #[test]
    fn unit_suffixes() {
        assert_eq!(kinds("500ms")[0], TokenKind::Int(500, Some("ms".into())));
        assert_eq!(kinds("1.5s")[0], TokenKind::Float(1.5, Some("s".into())));
        assert_eq!(kinds("4MiB")[0], TokenKind::Int(4, Some("MiB".into())));
        // Digit separators are not suffixes, and a bare number stays bare.
        assert_eq!(kinds("1_000")[0], TokenKind::Int(1000, None));
        assert_eq!(kinds("1_000ms")[0], TokenKind::Int(1000, Some("ms".into())));
        // Exponents bind before the suffix.
        assert_eq!(kinds("1e3s")[0], TokenKind::Float(1000.0, Some("s".into())));
        // `.` still separates: `1.foo()` is a method call, not a suffix.
        assert_eq!(
            kinds("1.foo")[..3],
            [
                TokenKind::Int(1, None),
                TokenKind::Dot,
                TokenKind::Ident("foo".into())
            ]
        );
    }

    #[test]
    fn string_escapes() {
        assert_eq!(kinds(r#""a\nb\u{41}""#)[0], TokenKind::Str("a\nbA".into()));
    }
}
