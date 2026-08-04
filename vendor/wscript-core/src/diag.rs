use crate::span::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

/// A structured diagnostic (PRD §5.1): code, span, message, optional help.
///
/// Rendered prettily by the CLI and consumed raw by the LSP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Stable diagnostic code, e.g. `E0003`.
    pub code: &'static str,
    pub severity: Severity,
    /// Primary span the diagnostic points at.
    pub span: Span,
    pub message: String,
    /// Extra labelled spans (secondary notes attached to source locations).
    pub labels: Vec<(Span, String)>,
    /// A "help:" suggestion shown under the diagnostic.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: &'static str, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            code,
            severity: Severity::Error,
            span,
            message: message.into(),
            labels: Vec::new(),
            help: None,
        }
    }

    pub fn warning(code: &'static str, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Warning,
            ..Diagnostic::error(code, span, message)
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help = Some(help.into());
        self
    }

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.labels.push((span, message.into()));
        self
    }
}

/// Fallback help text per diagnostic code, used by renderers when a
/// diagnostic carries no site-specific help (M7: every error explains
/// itself).
pub fn default_help(code: &str) -> Option<&'static str> {
    Some(match code {
        "E0001" => "close the comment with `*/`",
        "E0002" => "strings cannot span lines; close the `\"` or use \\n escapes",
        "E0003" => "unicode escapes look like `\\u{1F600}`",
        "E0004" => "supported escapes: \\n \\t \\r \\0 \\\\ \\\" \\' \\u{...}",
        "E0005" => "char literals hold exactly one character, e.g. 'a'",
        "E0006" => "numeric literals: 42, 0xFF, 3.14, 1e9 (int is 64-bit signed)",
        "E0007" => "this character is not part of wscript's syntax",
        "E0100" => "the parser expected different syntax here; see the language tour",
        "E0101" => "attributes only apply to struct and enum declarations",
        "E0102" => "top-level code lives in functions; execution starts at `fn main()`",
        "E0103" => "supported attributes: #[derive(...)] (and #[opaque] in .wscripti files)",
        "E0104" => "`self` is only the first parameter of methods in impl/trait blocks",
        "E0105" => "function parameters need type annotations: `name: type`",
        "E0106" => "trait methods take `self` first: `fn name(self, ...)`",
        "E0107" => "v1 traits declare signatures only; implement bodies in impl blocks",
        "E0108" => "types look like: int, string, List[int], fn(int) -> bool, dyn Trait",
        "E0109" => "statements end at a newline; use `;` to put several on one line",
        "E0110" => "destructuring `let` needs `else { ... }` that diverges (v1)",
        "E0111" => "the parser expected an expression here",
        "E0112" => "closures with a declared return type need a block body",
        "E0113" => "patterns: literals, bindings, _, Enum::Variant(...), Struct { .. }",
        "E0200" => "modules must be registered by the host before scripts can `use` them",
        "E0201" => "check the module's .wscripti interface for its items",
        "E0202" => "type names are unique across script, host and builtin types",
        "E0203" => "names must be unique within a declaration",
        "E0204" => "derivable traits: Eq, Ord, Display, Clone (Ord also needs Eq)",
        "E0205" => "this name is already defined; pick a different one",
        "E0206" => "impl blocks target script-declared struct or enum types",
        "E0207" => "methods take `self` first; associated functions are not in v1",
        "E0208" => "impl methods must match the trait's declared signatures exactly",
        "E0209" => "every field must support the derived trait's operation",
        "E0210" => "builtin generics: List[T], Map[K, V], Option[T], Result[T, E], weak[T]",
        "E0211" => "traits are not types; use `dyn Trait` for a dispatchable value",
        "E0212" => "check the spelling; types must be declared or host-registered",
        "E0213" => "weak references apply to reference types (structs, enums, List, Map, fn)",
        "E0214" => "map keys must be int, bool, char, or string",
        "E0215" => "user-defined generics are not in v1 (PRD §3.6)",
        "E0220" => "the value's type must match what the context expects",
        "E0221" => "`break`/`continue` only work inside while/loop/for bodies",
        "E0222" => "end the else block with return, break, or continue",
        "E0223" => "add an `impl Trait for Type { ... }` block",
        "E0224" => "an `if` without `else` is unit-typed; add an else branch",
        "E0225" => "ranges only appear as `for i in a..b` iterables in v1",
        "E0226" => "return a value matching the function's declared return type",
        "E0227" => "wscript has no truthiness: write an explicit comparison",
        "E0228" => "`self` only exists inside methods (fns in impl blocks)",
        "E0229" => "wrap host functions in a closure to use them as values: |x| f(x)",
        "E0230" => "check the spelling; declare variables with `let` before use",
        "E0231" => "paths are at most `module::Type::Variant`",
        "E0232" => "check the enum declaration for its variants",
        "E0233" => {
            "unit variants take no payload; tuple variants use (...); struct variants use { ... }"
        }
        "E0234" => "operators work on matching primitive types or via operator traits",
        "E0235" => "`==` needs Eq; ordering needs Ord (derive or impl them)",
        "E0236" => "only variables, fields, and list/map elements can be assigned",
        "E0237" => "only functions and closures can be called",
        "E0238" => "check the function's signature for its parameters",
        "E0239" => "each `{}` consumes one argument; escape braces as {{ and }}",
        "E0240" => {
            "int() takes int/float/char; float() takes int/float; parse strings with .parse_int()"
        }
        "E0241" => "see the stdlib reference for the methods of this type",
        "E0242" => "the element type does not support this operation",
        "E0243" => "multiple traits provide this method; rename one trait method",
        "E0244" => "only struct values expose fields (opaque host types expose methods)",
        "E0245" => "indexing works on List (int) and Map (key), or via an Index impl",
        "E0246" => "construct structs as `Name { field: value, ... }`",
        "E0247" => "initialize every declared field exactly once",
        "E0248" => "`for` iterates ranges, List elements, Map keys, and string chars",
        "E0249" => "`?` early-returns None/Err; the function must return Option/Result",
        "E0250" => "annotate the closure parameter: |x: int| ...",
        "E0251" => "add a type annotation: `let name: Type = ...`",
        "E0260" => "cover every case; guarded arms never count toward exhaustiveness",
        "E0261" => "destructure the payload or ignore it with `_`",
        "E0262" => "split the alternatives into separate match arms (v1)",
        "E0263" => "the pattern's type must match the scrutinee",
        "E0264" => "match the variant's payload shape",
        "E0270" => "this form belongs to .wscripti interface files, not scripts",
        "E0271" => "regenerate the interface with Context::write_interface",
        "W0001" => "this pattern always matches",
        "W0002" => "remove the unreachable arm or reorder the patterns",
        _ => return None,
    })
}
