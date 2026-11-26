//! Builds Berkeley SoftFloat 3e with its `ARM-VFPv2` specialisation, as a
//! second copy beside the `RISCV` one that `vmlab-hv-tcg` builds (R4 of the
//! phase 3d design).
//!
//! Both copies end up in one binary, so this one gets a symbol prefix: a
//! header that renames every SoftFloat name is put in front of every source
//! file. The names come from SoftFloat's own headers, so a new upstream
//! function is renamed without a list here.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The prefix of every symbol of this copy.
const PREFIX: &str = "a64sf_";

/// SoftFloat's multiword helpers are alternatives to FAST_INT64, not extra
/// operations. The list is the one of `vmlab-hv-tcg/build.rs`.
const EXCLUDED: [&str; 18] = [
    "s_addExtF80M.c",
    "s_addF128M.c",
    "s_compareNonnormExtF80M.c",
    "s_invalidF128M.c",
    "s_mulAddF128M.c",
    "s_normRoundPackMToExtF80M.c",
    "s_normRoundPackMToF128M.c",
    "s_normSubnormalF128SigM.c",
    "s_roundPackMToExtF80M.c",
    "s_roundPackMToF128M.c",
    "s_shiftLeftM.c",
    "s_shiftNormSigF128M.c",
    "s_shiftRightJamM.c",
    "s_shiftRightM.c",
    "s_tryPropagateNaNExtF80M.c",
    "s_tryPropagateNaNF128M.c",
    "s_propagateNaNF128M.c",
    "s_commonNaNToF128M.c",
];

/// Whether `name` is in SoftFloat's namespaces: `softfloat_...`, or a type
/// prefix and an underscore (`f32_add`, `ui64_to_f16`, `extF80_roundingPrecision`).
fn is_softfloat_name(name: &str) -> bool {
    const TYPES: [&str; 13] = [
        "softfloat", "bf16", "f16", "f32", "f64", "f128", "f128M", "extF80", "extF80M", "i32",
        "i64", "ui32", "ui64",
    ];
    let Some((head, rest)) = name.split_once('_') else {
        return false;
    };
    !rest.is_empty() && TYPES.contains(&head)
}

/// The SoftFloat names of the headers, without the names that a header
/// defines as a macro: a macro is no symbol, and it is expanded in each file.
fn names(headers: &[PathBuf]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut macros = BTreeSet::new();
    for header in headers {
        let text = std::fs::read_to_string(header).unwrap();
        for line in text.lines() {
            let mut tokens = line
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|token| !token.is_empty());
            let is_define = line.trim_start().starts_with('#')
                && line.trim_start()[1..].trim_start().starts_with("define");
            if is_define {
                // `# define NAME`: the token after `define`.
                if let Some(name) = tokens.clone().nth(1) {
                    macros.insert(name.to_string());
                }
            }
            for token in &mut tokens {
                if is_softfloat_name(token) {
                    found.insert(token.to_string());
                }
            }
        }
    }
    found.retain(|name| !macros.contains(name));
    found
}

/// The operations of the wrapper: the SoftFloat function name, and the C
/// expression over `a`, `b`, `c` and `mode` that gives its result as 64 bits.
fn operations() -> Vec<(String, String)> {
    let mut operations = Vec::new();
    let float = |width: u32, value: &str| format!("(float{width}_t){{ .v = (uint{width}_t){value} }}");
    for width in [32, 64] {
        let (a, b, c) = (float(width, "a"), float(width, "b"), float(width, "c"));
        for name in ["add", "sub", "mul", "div"] {
            operations.push((
                format!("f{width}_{name}"),
                format!("f{width}_{name}({a}, {b}).v"),
            ));
        }
        operations.push((format!("f{width}_sqrt"), format!("f{width}_sqrt({a}).v")));
        operations.push((
            format!("f{width}_mulAdd"),
            format!("f{width}_mulAdd({a}, {b}, {c}).v"),
        ));
        // b: the rounding mode of this call. c: whether inexact is raised.
        operations.push((
            format!("f{width}_roundToInt"),
            format!("f{width}_roundToInt({a}, (uint_fast8_t)b, c != 0).v"),
        ));
        for name in ["eq", "lt", "le", "lt_quiet", "le_quiet"] {
            operations.push((
                format!("f{width}_{name}"),
                format!("f{width}_{name}({a}, {b})"),
            ));
        }
        for (integer, cast) in [
            ("i32", "(uint64_t)(uint32_t)"),
            ("ui32", "(uint64_t)"),
            ("i64", "(uint64_t)"),
            ("ui64", "(uint64_t)"),
        ] {
            operations.push((
                format!("f{width}_to_{integer}"),
                format!("{cast}f{width}_to_{integer}({a}, (uint_fast8_t)mode, true)"),
            ));
        }
        for (integer, cast) in [
            ("i32", "(int32_t)"),
            ("ui32", "(uint32_t)"),
            ("i64", "(int64_t)"),
            ("ui64", ""),
        ] {
            operations.push((
                format!("{integer}_to_f{width}"),
                format!("{integer}_to_f{width}({cast}a).v"),
            ));
        }
    }
    for (from, to) in [(16, 32), (16, 64), (32, 16), (32, 64), (64, 16), (64, 32)] {
        operations.push((
            format!("f{from}_to_f{to}"),
            format!("f{from}_to_f{to}({}).v", float(from, "a")),
        ));
    }
    operations
}

fn sources(directory: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_str().unwrap();
            // The ARM-VFPv2 specialisation has no bfloat16.
            path.extension().is_some_and(|extension| extension == "c")
                && !EXCLUDED.contains(&name)
                && !name.to_ascii_lowercase().contains("bf16")
        })
        .collect();
    files.sort();
    files
}

fn main() {
    let root = PathBuf::from("../vmlab-hv-tcg/vendor/softfloat");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let include = root.join("source/include");
    let specialize = root.join("source/ARM-VFPv2");

    // The renaming header.
    let headers = [
        include.join("softfloat.h"),
        include.join("internals.h"),
        include.join("primitives.h"),
        specialize.join("specialize.h"),
    ];
    let mut prefix = String::from("/* Generated by build.rs: the symbol prefix of this SoftFloat copy. */\n");
    for name in names(&headers) {
        writeln!(prefix, "#define {name} {PREFIX}{name}").unwrap();
    }
    let prefix_header = out.join("a64sf_prefix.h");
    std::fs::write(&prefix_header, prefix).unwrap();

    let mut build = cc::Build::new();
    build
        .include(&include)
        .include(&specialize)
        .include(root.join("build"));
    for name in [
        "SOFTFLOAT_ROUND_ODD",
        "SOFTFLOAT_FAST_DIV32TO16",
        "SOFTFLOAT_FAST_DIV64TO32",
        "SOFTFLOAT_FAST_INT64",
    ] {
        build.define(name, None);
    }
    build.define("INLINE_LEVEL", "5");
    // The rounding mode and the exception flags are per-thread, so vCPUs on
    // separate host threads need no lock around a SoftFloat call.
    build.define("THREAD_LOCAL", "__thread");
    build.flag("-include").flag(&prefix_header);
    build.warnings(false).opt_level(2);
    for directory in [root.join("source"), specialize] {
        for file in sources(&directory) {
            build.file(file);
        }
    }

    // The wrapper: one entry for every operation, by index.
    let operations = operations();
    let mut c = String::from(
        "#include <stdbool.h>\n#include <stdint.h>\n#include \"softfloat.h\"\n\
         uint64_t a64sf_call(uint32_t operation, uint32_t mode, uint64_t a, uint64_t b, uint64_t c, uint8_t *flags) {\n\
         softfloat_exceptionFlags = 0;\n\
         softfloat_roundingMode = (uint_fast8_t)mode;\n\
         softfloat_detectTininess = softfloat_tininess_beforeRounding;\n\
         uint64_t result = 0;\n\
         (void)b; (void)c;\n\
         switch (operation) {\n",
    );
    let mut rust = String::from("// Generated by build.rs: the operations of `a64sf_call`.\n");
    for (index, (name, expression)) in operations.iter().enumerate() {
        writeln!(c, "case {index}: result = {expression}; break;").unwrap();
        writeln!(
            rust,
            "pub const {}: u32 = {index};",
            name.to_ascii_uppercase()
        )
        .unwrap();
    }
    c.push_str("default: *flags = 255; return 0;\n}\n*flags = (uint8_t)softfloat_exceptionFlags;\nreturn result;\n}\n");
    rust.push_str("/// Every operation by the name of its SoftFloat function.\npub const NAMES: &[(&str, u32)] = &[\n");
    for (index, (name, _)) in operations.iter().enumerate() {
        writeln!(rust, "    ({name:?}, {index}),").unwrap();
    }
    rust.push_str("];\n");
    std::fs::write(out.join("a64sf_ops.rs"), rust).unwrap();
    let wrapper = out.join("a64sf_wrapper.c");
    std::fs::write(&wrapper, c).unwrap();
    build.file(wrapper).compile("a64_softfloat");
}
