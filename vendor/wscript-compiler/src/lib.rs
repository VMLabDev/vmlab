//! The wscript compiler: lexer → parser → type checker → bytecode emitter
//! (PRD §5.1).

pub mod ast;
pub mod check;
pub mod emit;
pub mod lexer;
pub mod parser;
pub mod token;
pub mod wscripti;

use wscript_core::bytecode::CompiledUnit;
use wscript_core::diag::{Diagnostic, Severity};
use wscript_core::registry::Registry;
use wscript_core::source_map::{SourceFileInfo, SourceMap};

pub use parser::{ParseOutput, parse, parse_file};

/// A successful compilation (possibly with warnings).
pub struct Compiled {
    pub unit: CompiledUnit,
    pub warnings: Vec<Diagnostic>,
    /// Every compiled file's (display path, source text) in span-address
    /// order — what diagnostic/trace renderers need alongside
    /// `unit.source_map`.
    pub sources: Vec<(String, String)>,
}

/// A failed compilation, with everything a renderer needs to point
/// diagnostics at the right file of a multi-file program.
pub struct CompileFailure {
    pub diags: Vec<Diagnostic>,
    pub sources: Vec<(String, String)>,
    pub source_map: SourceMap,
}

/// How a script import is spelled at the `use` site.
pub enum ImportSpec<'a> {
    /// `use helpers` (bare name; only consulted when no registered host
    /// module has the name).
    Name(&'a str),
    /// `use "./helpers.wscript"` (explicit path, relative to the
    /// importing file).
    Path(&'a str),
}

/// A resolved script import.
pub struct ResolvedSource {
    /// Canonical identity for dedup (e.g. the canonicalized path).
    pub key: String,
    /// Display path (diagnostics, stack traces).
    pub path: String,
    pub src: String,
}

/// Resolves script-to-script imports for [`compile_entry`]. `from` is
/// the display path of the importing file.
pub trait SourceResolver: Sync {
    fn resolve(&self, from: &str, spec: ImportSpec) -> Result<ResolvedSource, String>;
}

/// A resolver that refuses every import — plain `compile()` uses it, so
/// single-source compilation reports a pointed error on `use "..."`.
pub struct NoImports;

impl SourceResolver for NoImports {
    fn resolve(&self, _from: &str, _spec: ImportSpec) -> Result<ResolvedSource, String> {
        Err(
            "script-file imports are not available here — compile with a source \
             resolver (Context::compile_entry / `wscript run`)"
                .to_string(),
        )
    }
}

/// The parser and checker recurse (bounded — see the parser's
/// `MAX_NESTING_BUDGET` and the checker's `MAX_EXPR_DEPTH`), and debug
/// frames are big enough that deeply nested scripts need real headroom.
/// Callers can sit on small stacks (tokio gives the LSP's threads 2 MiB),
/// so the pipeline runs on a scoped thread with a dedicated stack.
const PIPELINE_STACK: usize = 32 * 1024 * 1024;

fn on_pipeline_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("wscript-compile".into())
            .stack_size(PIPELINE_STACK)
            .spawn_scoped(scope, f)
            .expect("failed to spawn compile thread");
        match handle.join() {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    })
}

/// Compile a single script against the registered host context. All type
/// errors — including misuse of host-registered APIs — surface here
/// (PRD §1).
pub fn compile(source: &str, registry: &Registry) -> Result<Compiled, Vec<Diagnostic>> {
    compile_entry("script", source, &NoImports, registry).map_err(|f| f.diags)
}

/// Breadth-first loader of the script-import graph, deduped by canonical
/// key. `use` name resolution order: registered host module wins; the
/// path form is always a file. Bare-name resolver misses fall through to
/// the checker's E0200; path-form misses error here.
#[allow(clippy::too_many_arguments)]
fn load_imports(
    files: &mut Vec<LoadedFile>,
    keys: &mut std::collections::HashMap<String, usize>,
    base: &mut u32,
    next_id: &mut u32,
    diags: &mut Vec<Diagnostic>,
    resolver: &dyn SourceResolver,
    registry: &Registry,
) {
    let mut i = 0;
    while i < files.len() {
        let imports: Vec<(String, Option<String>, wscript_core::Span)> = files[i]
            .parse
            .file
            .items
            .iter()
            .filter_map(|item| match item {
                ast::Item::Use(u) => Some((u.module.name.clone(), u.path_lit.clone(), u.span)),
                _ => None,
            })
            .collect();
        let from = files[i].display.clone();
        for (module_name, path_lit, span) in imports {
            let is_path = path_lit.is_some();
            if !is_path && registry.modules.iter().any(|m| m.name == module_name) {
                continue; // host module wins
            }
            let spec = match &path_lit {
                Some(p) => ImportSpec::Path(p),
                None => ImportSpec::Name(&module_name),
            };
            let resolved = match resolver.resolve(&from, spec) {
                Ok(r) => r,
                Err(msg) => {
                    if is_path {
                        diags.push(
                            Diagnostic::error(
                                "E0200",
                                span,
                                format!(
                                    "cannot load `{}`: {msg}",
                                    path_lit.as_deref().unwrap_or(&module_name)
                                ),
                            )
                            .with_help("the path is resolved relative to the importing file"),
                        );
                    }
                    continue;
                }
            };
            if let Some(&existing) = keys.get(&resolved.key) {
                let existing_name = files[existing].module_name.clone();
                if existing_name != module_name && !module_name.is_empty() {
                    diags.push(
                        Diagnostic::error(
                            "E0200",
                            span,
                            format!("file already imported as `{existing_name}`; use that name"),
                        )
                        .with_help("a script file has one module name program-wide"),
                    );
                }
                continue;
            }
            if files.iter().any(|f| f.module_name == module_name) {
                diags.push(
                    Diagnostic::error(
                        "E0200",
                        span,
                        format!("two imported files share the module name `{module_name}`"),
                    )
                    .with_help("disambiguate with `use \"path\" as other_name`"),
                );
                continue;
            }
            let parse = parse_file(&resolved.src, *base, next_id);
            *base += resolved.src.len() as u32 + 1;
            keys.insert(resolved.key, files.len());
            files.push(LoadedFile {
                module_name,
                display: resolved.path,
                src: resolved.src,
                parse,
            });
        }
        i += 1;
    }
}

/// One loaded file of a multi-file program.
struct LoadedFile {
    module_name: String,
    display: String,
    src: String,
    parse: ParseOutput,
}

/// Compile a script that may import other script files (`use helpers` /
/// `use "./sub/x.wscript" as x`). The whole import graph is compiled
/// into ONE merged unit: spans live in a global address space described
/// by `unit.source_map`, and the VM stays single-unit. Cycles between
/// files are allowed (no top-level statements ⇒ no initialization
/// order); only the ENTRY file's fns are exported to the host.
pub fn compile_entry(
    entry_path: &str,
    entry_src: &str,
    resolver: &dyn SourceResolver,
    registry: &Registry,
) -> Result<Compiled, CompileFailure> {
    on_pipeline_stack(|| {
        let mut diags: Vec<Diagnostic> = Vec::new();
        let mut next_id = 0;
        let mut base: u32 = 0;
        let mut files: Vec<LoadedFile> = Vec::new();
        let mut keys: std::collections::HashMap<String, usize> = Default::default();

        let entry_parse = parse_file(entry_src, base, &mut next_id);
        base += entry_src.len() as u32 + 1;
        files.push(LoadedFile {
            module_name: String::new(),
            display: entry_path.to_string(),
            src: entry_src.to_string(),
            parse: entry_parse,
        });

        load_imports(
            &mut files,
            &mut keys,
            &mut base,
            &mut next_id,
            &mut diags,
            resolver,
            registry,
        );

        let mut map_base = 0u32;
        let source_map = SourceMap {
            files: files
                .iter()
                .map(|f| {
                    let info = SourceFileInfo {
                        path: f.display.clone(),
                        base: map_base,
                        len: f.src.len() as u32,
                    };
                    map_base += f.src.len() as u32 + 1;
                    info
                })
                .collect(),
        };
        let refs: Vec<(String, &ast::SourceFile)> = files
            .iter()
            .map(|f| (f.module_name.clone(), &f.parse.file))
            .collect();
        let mut checked = check::check_files(&refs, registry);
        for f in &files {
            diags.extend(f.parse.diags.iter().cloned());
        }
        diags.append(&mut checked.diags);
        diags.sort_by_key(|d| (d.span.lo, d.span.hi));
        if diags.iter().any(|d| d.severity == Severity::Error) {
            return Err(CompileFailure {
                diags,
                sources: files.into_iter().map(|f| (f.display, f.src)).collect(),
                source_map,
            });
        }
        let asts: Vec<&ast::SourceFile> = files.iter().map(|f| &f.parse.file).collect();
        let mut unit = emit::emit_files(&asts, &checked);
        unit.source_map = source_map;
        Ok(Compiled {
            unit,
            warnings: diags,
            sources: files.into_iter().map(|f| (f.display, f.src)).collect(),
        })
    })
}

/// Parse + check without emitting — the LSP's entry point: always returns
/// the (possibly partial) AST, the check tables and every diagnostic.
pub struct Analysis {
    pub parse: ParseOutput,
    pub check: check::CheckResult,
    /// File layout of the analysis (entry first). Single-file analyses
    /// have one entry at base 0.
    pub source_map: SourceMap,
}

pub fn analyze(source: &str, registry: &Registry) -> Analysis {
    analyze_entry("script", source, &NoImports, registry)
}

/// [`analyze`] with script-import resolution — the LSP's entry point for
/// files that `use` other script files. The entry file's AST/tables are
/// returned as usual; `source_map` locates diagnostics that land in
/// imported files (the entry file occupies `[0, len]` as before, so all
/// single-file consumers keep working unchanged).
pub fn analyze_entry(
    entry_path: &str,
    entry_src: &str,
    resolver: &dyn SourceResolver,
    registry: &Registry,
) -> Analysis {
    on_pipeline_stack(|| {
        let mut next_id = 0;
        let mut base: u32 = 0;
        let mut files: Vec<LoadedFile> = Vec::new();
        let mut keys: std::collections::HashMap<String, usize> = Default::default();
        let mut load_diags: Vec<Diagnostic> = Vec::new();

        let entry_parse = parse_file(entry_src, base, &mut next_id);
        base += entry_src.len() as u32 + 1;
        files.push(LoadedFile {
            module_name: String::new(),
            display: entry_path.to_string(),
            src: entry_src.to_string(),
            parse: entry_parse,
        });
        load_imports(
            &mut files,
            &mut keys,
            &mut base,
            &mut next_id,
            &mut load_diags,
            resolver,
            registry,
        );

        let refs: Vec<(String, &ast::SourceFile)> = files
            .iter()
            .map(|f| (f.module_name.clone(), &f.parse.file))
            .collect();
        let mut check = check::check_files(&refs, registry);
        check.diags.extend(load_diags);
        // Parse diags of IMPORTED files surface too (they explain
        // downstream errors); the entry's own parse diags stay in
        // `parse.diags` as before.
        for f in files.iter().skip(1) {
            check.diags.extend(f.parse.diags.iter().cloned());
        }
        let mut map_base = 0u32;
        let source_map = SourceMap {
            files: files
                .iter()
                .map(|f| {
                    let info = SourceFileInfo {
                        path: f.display.clone(),
                        base: map_base,
                        len: f.src.len() as u32,
                    };
                    map_base += f.src.len() as u32 + 1;
                    info
                })
                .collect(),
        };
        let parse = files.swap_remove(0).parse;
        Analysis {
            parse,
            check,
            source_map,
        }
    })
}
