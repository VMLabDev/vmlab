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

    /// The help a renderer should show: this diagnostic's own, else the
    /// fallback for its code.
    ///
    /// Every renderer wants the same rule, so it lives with the
    /// diagnostic rather than being restated by each of them — the
    /// terminal renderer and the language server had written it out
    /// separately, which is one edit away from an editor that stops
    /// explaining errors the CLI still explains.
    pub fn help_text(&self) -> Option<&str> {
        self.help.as_deref().or_else(|| default_help(self.code))
    }
}

/// Whether the diagnostic corpus is expected to produce a code.
///
/// Exempting a code hides it from the coverage gate, so the reason is
/// carried as data rather than left in a commit message — the next person
/// to read the registry can judge whether the exemption still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Some fixture under `wscript-compiler/tests/fixtures/diags/` must
    /// render this code.
    Fixture,
    /// No fixture can render it, for the recorded reason.
    Exempt(&'static str),
}

/// One row of the code registry: a diagnostic code, its fallback help, and
/// how it is tested.
///
/// Build entries with [`covered`] or [`exempt`] rather than by hand — the
/// two of them are the whole vocabulary, and which one a row uses is the
/// only decision a new code needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredCode {
    /// Stable diagnostic code, e.g. `E0003`.
    pub code: &'static str,
    /// Help shown when the emission site supplies none. Site help wins
    /// where it exists — it can name the type, the variant or the argument
    /// that actually went wrong — but a code has to read as advice on its
    /// own, because a site can always forget.
    pub help: &'static str,
    /// Whether the fixture corpus is expected to render this code.
    pub coverage: Coverage,
}

/// A code some fixture must render.
const fn covered(code: &'static str, help: &'static str) -> RegisteredCode {
    RegisteredCode {
        code,
        help,
        coverage: Coverage::Fixture,
    }
}

/// A code no fixture can render, and why not.
const fn exempt(code: &'static str, help: &'static str, why: &'static str) -> RegisteredCode {
    RegisteredCode {
        code,
        help,
        coverage: Coverage::Exempt(why),
    }
}

/// Every diagnostic code wscript can emit.
///
/// This is the canonical list — `default_help` reads it, and
/// `wscript-compiler/tests/diag_codes.rs` gates the source and the fixture
/// corpus against it. Adding a code means adding a row here, and the row
/// keeps a promise: **every code carries help.** M7's "every error explains
/// itself" is that sentence, mechanised — no diagnostic can render mute,
/// whichever of a code's emission sites raised it and whether or not the
/// corpus happens to exercise that one.
pub static CODES: &[RegisteredCode] = &[
    // ---------------------------------------------------------- lexer
    covered("E0001", "close the comment with `*/`"),
    covered(
        "E0002",
        "strings cannot span lines; close the `\"` or use \\n escapes",
    ),
    covered("E0003", "unicode escapes look like `\\u{1F600}`"),
    covered(
        "E0004",
        "supported escapes: \\n \\t \\r \\0 \\\\ \\\" \\' \\u{...}",
    ),
    covered(
        "E0005",
        "char literals hold exactly one character, e.g. 'a'",
    ),
    covered(
        "E0006",
        "numeric literals: 42, 0xFF, 3.14, 1e9 (int is 64-bit signed)",
    ),
    covered("E0007", "this character is not part of wscript's syntax"),
    // --------------------------------------------------------- parser
    covered(
        "E0100",
        "the parser expected different syntax here; see the language tour",
    ),
    covered(
        "E0101",
        "attributes only apply to struct and enum declarations",
    ),
    covered(
        "E0102",
        "top-level code lives in functions; execution starts at `fn main()`",
    ),
    covered(
        "E0103",
        "supported attributes: #[derive(...)] (and #[opaque] in .wscripti files)",
    ),
    covered(
        "E0104",
        "`self` is only the first parameter of methods in impl/trait blocks",
    ),
    covered(
        "E0105",
        "function parameters need type annotations: `name: type`",
    ),
    covered(
        "E0106",
        "trait methods take `self` first: `fn name(self, ...)`",
    ),
    covered(
        "E0107",
        "v1 traits declare signatures only; implement bodies in impl blocks",
    ),
    covered(
        "E0108",
        "types look like: int, string, List[int], fn(int) -> bool, dyn Trait",
    ),
    covered(
        "E0109",
        "statements end at a newline; use `;` to put several on one line",
    ),
    covered(
        "E0110",
        "destructuring `let` needs `else { ... }` that diverges (v1)",
    ),
    covered("E0111", "the parser expected an expression here"),
    covered(
        "E0112",
        "closures with a declared return type need a block body",
    ),
    covered(
        "E0113",
        "patterns: literals, bindings, _, Enum::Variant(...), Struct { .. }",
    ),
    exempt(
        "E0114",
        "split this into smaller statements; the compiler bounds how deeply \
         source may nest",
        "the nesting backstop needs thousands of generated tokens to trip; as a \
         fixture that is 10 KB of punctuation. Covered by \
         `diags.rs::pathological_nesting_errors_instead_of_overflowing`, which \
         generates both the parser's and the checker's depth limits.",
    ),
    // ----------------------------------------------------- resolution
    covered(
        "E0200",
        "modules must be registered by the host before scripts can `use` them",
    ),
    covered(
        "E0201",
        "check the module's .wscripti interface for its items",
    ),
    covered(
        "E0202",
        "type names are unique across script, host and builtin types",
    ),
    covered("E0203", "names must be unique within a declaration"),
    covered(
        "E0204",
        "derivable traits: Eq, Ord, Display, Clone (Ord also needs Eq)",
    ),
    covered(
        "E0205",
        "this name is already defined; pick a different one",
    ),
    covered(
        "E0206",
        "impl blocks target script-declared struct or enum types",
    ),
    covered(
        "E0207",
        "methods take `self` first; associated functions are not in v1",
    ),
    covered(
        "E0208",
        "impl methods must match the trait's declared signatures exactly",
    ),
    covered(
        "E0209",
        "every field must support the derived trait's operation",
    ),
    covered(
        "E0210",
        "builtin generics: List[T], Map[K, V], Option[T], Result[T, E], weak[T]",
    ),
    covered(
        "E0211",
        "traits are not types; use `dyn Trait` for a dispatchable value",
    ),
    covered(
        "E0212",
        "check the spelling; types must be declared or host-registered",
    ),
    covered(
        "E0213",
        "weak references apply to reference types (structs, enums, List, Map, fn)",
    ),
    covered("E0214", "map keys must be int, bool, char, or string"),
    covered("E0215", "user-defined generics are not in v1 (PRD §3.6)"),
    // -------------------------------------------------------- checker
    covered(
        "E0220",
        "the value's type must match what the context expects",
    ),
    covered(
        "E0221",
        "`break`/`continue` only work inside while/loop/for bodies",
    ),
    covered(
        "E0222",
        "end the else block with return, break, or continue",
    ),
    covered("E0223", "add an `impl Trait for Type { ... }` block"),
    covered(
        "E0224",
        "an `if` without `else` is unit-typed; add an else branch",
    ),
    covered(
        "E0225",
        "ranges only appear as `for i in a..b` iterables in v1",
    ),
    covered(
        "E0226",
        "return a value matching the function's declared return type",
    ),
    covered(
        "E0227",
        "wscript has no truthiness: write an explicit comparison",
    ),
    covered(
        "E0228",
        "`self` only exists inside methods (fns in impl blocks)",
    ),
    covered(
        "E0229",
        "wrap host functions in a closure to use them as values: |x| f(x)",
    ),
    covered(
        "E0230",
        "check the spelling; declare variables with `let` before use",
    ),
    covered("E0231", "paths are at most `module::Type::Variant`"),
    covered("E0232", "check the enum declaration for its variants"),
    covered(
        "E0233",
        "unit variants take no payload; tuple variants use (...); struct variants use { ... }",
    ),
    covered(
        "E0234",
        "operators work on matching primitive types or via operator traits",
    ),
    covered(
        "E0235",
        "`==` needs Eq; ordering needs Ord (derive or impl them)",
    ),
    covered(
        "E0236",
        "only variables, fields, and list/map elements can be assigned",
    ),
    covered("E0237", "only functions and closures can be called"),
    covered("E0238", "check the function's signature for its parameters"),
    covered(
        "E0239",
        "each `{}` consumes one argument; escape braces as {{ and }}",
    ),
    covered(
        "E0240",
        "int() takes int/float/char; float() takes int/float; parse strings with .parse_int()",
    ),
    covered(
        "E0241",
        "see the stdlib reference for the methods of this type",
    ),
    covered("E0242", "the element type does not support this operation"),
    covered(
        "E0243",
        "multiple traits provide this method; rename one trait method",
    ),
    covered(
        "E0244",
        "only struct values expose fields (opaque host types expose methods)",
    ),
    covered(
        "E0245",
        "indexing works on List (int) and Map (key), or via an Index impl",
    ),
    covered("E0246", "construct structs as `Name { field: value, ... }`"),
    covered("E0247", "initialize every declared field exactly once"),
    covered(
        "E0248",
        "`for` iterates ranges, List elements, Map keys, and string chars",
    ),
    covered(
        "E0249",
        "`?` early-returns None/Err; the function must return Option/Result",
    ),
    covered("E0250", "annotate the closure parameter: |x: int| ..."),
    covered("E0251", "add a type annotation: `let name: Type = ...`"),
    // ------------------------------------------------------- generics
    covered(
        "E0252",
        "annotate the binding, or a surrounding expression, so every type \
         parameter is determined",
    ),
    covered(
        "E0253",
        "type parameters carry only the built-in `Eq`, `Ord` and `Clone` \
         bounds; arithmetic on them arrives in a later release",
    ),
    covered(
        "E0254",
        "v1 generics: top-level `fn`s only, bounded by `Eq`, `Ord` or `Clone`",
    ),
    covered(
        "E0255",
        "a type parameter is declared once, must appear in the signature, and \
         must not share a name with a type",
    ),
    // ------------------------------------------------------- patterns
    covered(
        "E0260",
        "cover every case; guarded arms never count toward exhaustiveness",
    ),
    covered("E0261", "destructure the payload or ignore it with `_`"),
    covered(
        "E0262",
        "split the alternatives into separate match arms (v1)",
    ),
    covered("E0263", "the pattern's type must match the scrutinee"),
    covered("E0264", "match the variant's payload shape"),
    // ---------------------------------------------------------- units
    covered("E0265", "each unit may be declared once per family"),
    covered(
        "E0266",
        "a factor says how many base units one of this unit is worth: \
         positive, finite, and within the backing type",
    ),
    covered(
        "E0267",
        "exactly one unit in a family has the factor 1 — values are stored in it",
    ),
    covered(
        "E0268",
        "a conversion factor is a constant expression over numeric literals, \
         units declared earlier in the family, and `+ - * /`",
    ),
    covered(
        "E0269",
        "the value must land on a whole number of the family's base unit, \
         within the range of the backing type",
    ),
    // ------------------------------------------------------ interface
    covered(
        "E0270",
        "this form belongs to .wscripti interface files, not scripts",
    ),
    covered(
        "E0271",
        "regenerate the interface with Context::write_interface",
    ),
    // ------------------------------------------------------- internal
    exempt(
        "E9999",
        "this is a bug in the wscript compiler, not in your script — please \
         report it with the script that triggered it",
        "an internal compiler error. Emit runs only after a clean check, so no \
         script can reach it by construction; the unit test in `emit.rs` \
         provokes it directly.",
    ),
    // ------------------------------------------------------- warnings
    covered("W0001", "this pattern always matches"),
    covered(
        "W0002",
        "remove the unreachable arm or reorder the patterns",
    ),
];

/// Fallback help text per diagnostic code, used by renderers when a
/// diagnostic carries no site-specific help (M7: every error explains
/// itself). `None` only for a code outside the registry, which
/// `diag_codes.rs` makes impossible for anything the compiler emits.
pub fn default_help(code: &str) -> Option<&'static str> {
    CODES.iter().find(|c| c.code == code).map(|c| c.help)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_ordered() {
        let mut sorted: Vec<&str> = CODES.iter().map(|c| c.code).collect();
        let listed = sorted.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            listed, sorted,
            "CODES must be sorted by code with no duplicates"
        );
    }

    /// Help is what the reader acts on, so an empty string would satisfy
    /// the type and none of the intent.
    #[test]
    fn every_code_says_something() {
        let terse: Vec<&str> = CODES
            .iter()
            .filter(|c| c.help.len() < 20)
            .map(|c| c.code)
            .collect();
        assert!(terse.is_empty(), "help too short to act on: {terse:?}");
    }
}
