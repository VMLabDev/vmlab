use wscript_core::span::Span;

/// One piece of an interpolated string literal.
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    Lit(String),
    /// A `{expr}` hole, pre-lexed by the string lexer with absolute
    /// source spans; terminated by an `Eof` token.
    Hole(Vec<Token>),
}

/// Declares the keyword spellings once: the lexer's lookup and the list an
/// editor offers for completion are generated together, so a new keyword
/// cannot reach the language and miss the editor.
///
/// Interface-only keywords are separated because they are not part of the
/// script surface — `mod` and `const` appear in `.wscripti` files (PRD §9.1)
/// and nowhere a script author writes.
macro_rules! keywords {
    (
        script { $( $text:literal => $variant:ident, )* }
        interface { $( $itext:literal => $ivariant:ident, )* }
    ) => {
        impl TokenKind {
            /// The keyword `text` spells, if it spells one.
            pub fn keyword(text: &str) -> Option<TokenKind> {
                Some(match text {
                    $( $text => TokenKind::$variant, )*
                    $( $itext => TokenKind::$ivariant, )*
                    _ => return None,
                })
            }
        }

        /// Every keyword a script may contain.
        pub const SCRIPT_KEYWORDS: &[&str] = &[ $($text),* ];
    };
}

keywords! {
    script {
        "let" => KwLet,
        "fn" => KwFn,
        "struct" => KwStruct,
        "enum" => KwEnum,
        "trait" => KwTrait,
        "impl" => KwImpl,
        "for" => KwFor,
        "in" => KwIn,
        "while" => KwWhile,
        "loop" => KwLoop,
        "if" => KwIf,
        "else" => KwElse,
        "match" => KwMatch,
        "return" => KwReturn,
        "break" => KwBreak,
        "continue" => KwContinue,
        "use" => KwUse,
        "true" => KwTrue,
        "false" => KwFalse,
        "dyn" => KwDyn,
        "self" => KwSelf,
    }
    interface {
        "mod" => KwMod,
        "const" => KwConst,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // literals
    // Numeric literals carry an optional unit suffix (`500ms`, `1.5s`),
    // resolved against the unit families in scope by the checker.
    Int(i64, Option<String>),
    Float(f64, Option<String>),
    Str(String),
    /// A string literal containing `{expr}` interpolation holes.
    StrInterp(Vec<StrPart>),
    Char(char),
    Ident(String),
    /// `/// ...` documentation comment (attached to declarations).
    DocComment(String),

    // keywords
    KwLet,
    KwFn,
    KwStruct,
    KwEnum,
    KwTrait,
    KwImpl,
    KwFor,
    KwIn,
    KwWhile,
    KwLoop,
    KwIf,
    KwElse,
    KwMatch,
    KwReturn,
    KwBreak,
    KwContinue,
    KwUse,
    KwTrue,
    KwFalse,
    KwDyn,
    KwSelf,
    /// Interface files (`.wscripti`) only.
    KwMod,
    KwConst,

    // punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    HashBrace, // `#{` — map literal
    Hash,      // `#` — attributes
    Comma,
    Dot,
    DotDot,   // `..`
    DotDotEq, // `..=`
    Colon,
    ColonColon,
    Semi,
    Arrow,    // `->`
    FatArrow, // `=>`
    Eq,
    EqEq,
    NotEq,
    PlusEq,    // `+=`
    MinusEq,   // `-=`
    StarEq,    // `*=`
    SlashEq,   // `/=`
    PercentEq, // `%=`
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    AndAnd,
    OrOr,
    Pipe,
    Question,
    Underscore,

    /// Statement-significant newline (suppressed inside `(` / `[`).
    Newline,
    Eof,
}

impl TokenKind {
    /// Human-readable name for diagnostics.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Int(_, Some(u)) | TokenKind::Float(_, Some(u)) => {
                format!("`{u}` literal")
            }
            TokenKind::Int(..) => "integer literal".into(),
            TokenKind::Float(..) => "float literal".into(),
            TokenKind::Str(_) => "string literal".into(),
            TokenKind::StrInterp(_) => "interpolated string literal".into(),
            TokenKind::Char(_) => "char literal".into(),
            TokenKind::Ident(name) => format!("`{name}`"),
            TokenKind::DocComment(_) => "doc comment".into(),
            TokenKind::KwLet => "`let`".into(),
            TokenKind::KwFn => "`fn`".into(),
            TokenKind::KwStruct => "`struct`".into(),
            TokenKind::KwEnum => "`enum`".into(),
            TokenKind::KwTrait => "`trait`".into(),
            TokenKind::KwImpl => "`impl`".into(),
            TokenKind::KwFor => "`for`".into(),
            TokenKind::KwIn => "`in`".into(),
            TokenKind::KwWhile => "`while`".into(),
            TokenKind::KwLoop => "`loop`".into(),
            TokenKind::KwIf => "`if`".into(),
            TokenKind::KwElse => "`else`".into(),
            TokenKind::KwMatch => "`match`".into(),
            TokenKind::KwReturn => "`return`".into(),
            TokenKind::KwBreak => "`break`".into(),
            TokenKind::KwContinue => "`continue`".into(),
            TokenKind::KwUse => "`use`".into(),
            TokenKind::KwTrue => "`true`".into(),
            TokenKind::KwFalse => "`false`".into(),
            TokenKind::KwDyn => "`dyn`".into(),
            TokenKind::KwMod => "`mod`".into(),
            TokenKind::KwConst => "`const`".into(),
            TokenKind::KwSelf => "`self`".into(),
            TokenKind::LParen => "`(`".into(),
            TokenKind::RParen => "`)`".into(),
            TokenKind::LBrace => "`{`".into(),
            TokenKind::RBrace => "`}`".into(),
            TokenKind::LBracket => "`[`".into(),
            TokenKind::RBracket => "`]`".into(),
            TokenKind::HashBrace => "`#{`".into(),
            TokenKind::Hash => "`#`".into(),
            TokenKind::Comma => "`,`".into(),
            TokenKind::Dot => "`.`".into(),
            TokenKind::DotDot => "`..`".into(),
            TokenKind::DotDotEq => "`..=`".into(),
            TokenKind::Colon => "`:`".into(),
            TokenKind::ColonColon => "`::`".into(),
            TokenKind::Semi => "`;`".into(),
            TokenKind::Arrow => "`->`".into(),
            TokenKind::FatArrow => "`=>`".into(),
            TokenKind::Eq => "`=`".into(),
            TokenKind::EqEq => "`==`".into(),
            TokenKind::NotEq => "`!=`".into(),
            TokenKind::PlusEq => "`+=`".into(),
            TokenKind::MinusEq => "`-=`".into(),
            TokenKind::StarEq => "`*=`".into(),
            TokenKind::SlashEq => "`/=`".into(),
            TokenKind::PercentEq => "`%=`".into(),
            TokenKind::Lt => "`<`".into(),
            TokenKind::Le => "`<=`".into(),
            TokenKind::Gt => "`>`".into(),
            TokenKind::Ge => "`>=`".into(),
            TokenKind::Plus => "`+`".into(),
            TokenKind::Minus => "`-`".into(),
            TokenKind::Star => "`*`".into(),
            TokenKind::Slash => "`/`".into(),
            TokenKind::Percent => "`%`".into(),
            TokenKind::Bang => "`!`".into(),
            TokenKind::AndAnd => "`&&`".into(),
            TokenKind::OrOr => "`||`".into(),
            TokenKind::Pipe => "`|`".into(),
            TokenKind::Question => "`?`".into(),
            TokenKind::Underscore => "`_`".into(),
            TokenKind::Newline => "end of line".into(),
            TokenKind::Eof => "end of file".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}
