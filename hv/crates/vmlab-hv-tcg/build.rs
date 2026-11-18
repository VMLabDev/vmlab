use std::{fmt::Write as _, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=vendor/softfloat");
    println!("cargo:rerun-if-changed=build.rs");
    let root = PathBuf::from("vendor/softfloat");
    let mut build = cc::Build::new();
    build
        .include(root.join("source/include"))
        .include(root.join("source/RISCV"))
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
    // The rounding mode and the exception flags are per-thread, so harts on
    // separate host threads need no lock around a SoftFloat call.
    build.define("THREAD_LOCAL", "__thread");
    build.warnings(false).opt_level(2);
    // SoftFloat's multiword helpers are alternatives to FAST_INT64, not extra operations.
    let excluded = [
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
    for directory in [root.join("source"), root.join("source/RISCV")] {
        let mut files: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        for file in files {
            if file.extension().is_some_and(|e| e == "c")
                && !excluded.contains(&file.file_name().unwrap().to_str().unwrap())
            {
                build.file(file);
            }
        }
    }
    // (Sail name, C expression), dispatched by position in the sorted list.
    let mut operations: Vec<(String, String)> = Vec::new();
    for width in [16, 32, 64] {
        for (sail, sf, arity, rounding, boolean) in [
            ("Add", "add", 2, true, false),
            ("Sub", "sub", 2, true, false),
            ("Mul", "mul", 2, true, false),
            ("Div", "div", 2, true, false),
            ("MulAdd", "mulAdd", 3, true, false),
            ("Sqrt", "sqrt", 1, true, false),
            ("Lt", "lt", 2, false, true),
            ("Le", "le", 2, false, true),
            ("Eq", "eq", 2, false, true),
            ("Lt_quiet", "lt_quiet", 2, false, true),
            ("Le_quiet", "le_quiet", 2, false, true),
        ] {
            let start = usize::from(rounding);
            let args: Vec<_> = (start..start + arity)
                .map(|i| format!("(float{width}_t){{ .v = p{i} }}"))
                .collect();
            operations.push((
                format!("zriscv_f{width}{sail}"),
                format!(
                    "f{width}_{sf}({}){}",
                    args.join(", "),
                    if boolean { "" } else { ".v" }
                ),
            ));
        }
        operations.push((
            format!("zriscv_f{width}roundToInt"),
            format!("f{width}_roundToInt((float{width}_t){{ .v = p1 }}, p0, p2).v"),
        ));
    }
    for (src, src_ty) in [
        ("f16", "float16_t"),
        ("f32", "float32_t"),
        ("f64", "float64_t"),
        ("i32", "int32_t"),
        ("ui32", "uint32_t"),
        ("i64", "int64_t"),
        ("ui64", "uint64_t"),
    ] {
        for dst in ["f16", "f32", "f64", "bf16", "i32", "ui32", "i64", "ui64"] {
            if src == dst
                || (!src.starts_with('f') && !dst.starts_with('f'))
                || (dst == "bf16" && src != "f32")
            {
                continue;
            }
            let value = if src.starts_with('f') {
                format!("({src_ty}){{ .v = p1 }}")
            } else {
                format!("({src_ty})p1")
            };
            let extra = if dst.starts_with('i') || dst.starts_with("ui") {
                ", p0, true"
            } else {
                ""
            };
            let suffix = if dst.starts_with('f') || dst == "bf16" {
                ".v"
            } else {
                ""
            };
            let sail_dst = if dst == "bf16" {
                "BF16".into()
            } else {
                format!("{}{}", dst[..1].to_ascii_uppercase(), &dst[1..])
            };
            operations.push((
                format!("zriscv_{src}To{sail_dst}"),
                format!("{src}_to_{dst}({value}{extra}){suffix}"),
            ));
        }
    }
    operations.sort();
    operations.dedup_by(|a, b| a.0 == b.0);
    let mut c = String::from(
        "#include <stdint.h>\n#include \"softfloat.h\"\nuint64_t hv_softfloat(uint32_t operation, uint64_t p0, uint64_t p1, uint64_t p2, uint64_t p3, uint8_t *flags) {\nsoftfloat_exceptionFlags = 0;\nsoftfloat_roundingMode = p0;\nsoftfloat_detectTininess = softfloat_tininess_afterRounding;\nuint64_t result = 0;\n(void)p2; (void)p3;\nswitch (operation) {\n",
    );
    // The Rust side finds an operation's index by binary search in the same
    // sorted list, so the two cannot disagree about which index is which.
    let mut names = String::from("pub(crate) const NAMES: &[&str] = &[\n");
    for (index, (name, expression)) in operations.iter().enumerate() {
        writeln!(c, "case {index}: result = {expression}; break;").unwrap();
        writeln!(names, "    {name:?},").unwrap();
    }
    names.push_str("];\n");
    c.push_str("default: *flags = 255; return 0;\n}\n*flags = softfloat_exceptionFlags;\nreturn result;\n}\n");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("softfloat_names.rs"), names).unwrap();
    let wrapper = out.join("softfloat_wrapper.c");
    std::fs::write(&wrapper, c).unwrap();
    build.file(wrapper).compile("hv_softfloat");
}
