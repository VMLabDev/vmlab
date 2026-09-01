//! Checker positive cases (PRD §11), plus the two assertions that fixtures
//! cannot express.
//!
//! The negative cases live in `tests/fixtures/diags/` and are asserted by
//! `diag_snapshots.rs`, which pins each one's full diagnostic — code, location,
//! message and help — rather than only its code. What stays here:
//!
//! - `ok(...)` snippets, whose whole assertion is "this compiles". Snapshotting
//!   them would commit 48 files reading `(no diagnostics)`.
//! - `pathological_nesting_errors_instead_of_overflowing`, whose sources are
//!   generated (5,000 nested parens); as fixtures they would be 10 KB of
//!   punctuation.
//! - `units_are_zero_cost`, a differential *bytecode* assertion, not a
//!   diagnostic one.

use wscript_core::registry::Registry;

fn codes(src: &str) -> Vec<&'static str> {
    match wscript_compiler::compile(src, &Registry::new()) {
        Ok(_) => vec![],
        Err(diags) => diags
            .iter()
            .filter(|d| d.severity == wscript_core::diag::Severity::Error)
            .map(|d| d.code)
            .collect(),
    }
}

fn ok(src: &str) {
    let result = codes(src);
    assert!(
        result.is_empty(),
        "expected clean compile, got {result:?}\n--- src ---\n{src}"
    );
}

fn fails_with(src: &str, code: &str) {
    let result = codes(src);
    assert!(
        result.contains(&code),
        "expected {code}, got {result:?}\n--- src ---\n{src}"
    );
}

#[test]
fn arithmetic_types() {
    ok("fn main() -> int { 1 + 2 * 3 }");
    ok("fn main() -> string { \"a\" + \"b\" }");
}

#[test]
fn annotations_required_on_fns() {
    ok("fn f(x: int) -> int { x }\nfn main() { f(1); }");
}

#[test]
fn let_inference() {
    ok("fn main() -> int { let x = 5\n x }");
    ok("fn main() -> int { let x: int = 5\n x }");
    ok("fn main() -> int { let x = []\n x.push(1)\n x.len() }");
}

#[test]
fn call_arity_and_types() {
    ok("fn area(w: int, h: int) -> int { w * h }\nfn main() -> int { area(2, 3) }");
}

#[test]
fn struct_literals() {
    ok("struct P { x: int, y: int }\nfn main() -> int { let p = P { x: 1, y: 2 }\n p.x }");
}

#[test]
fn enums_and_match() {
    ok("enum E { A, B(int), C { v: bool } }\n\
         fn main() -> int { match E::B(4) { E::A => 0, E::B(n) => n, E::C { v } => if v { 1 } else { 2 } } }");
    ok("fn main() -> int { match true { true => 1, false => 0 } }");
    ok("fn main() -> int { match 1 { 0 => 1, _ => 2 } }");
}

#[test]
fn nested_exhaustiveness() {
    // Exact at depth > 1 (better than the PRD's one-level guarantee).
    ok(
        "fn main() -> int { match Some(Some(1)) { Some(Some(n)) => n, Some(None) => 0, None => -1 } }",
    );
}

#[test]
fn option_result_and_try() {
    ok("fn f() -> Option[int] { Some(1) }\nfn g() -> Option[int] { Some(f()? + 1) }\nfn main() {}");
    ok(
        "fn f() -> Result[int, string] { Ok(1) }\nfn g() -> Result[int, string] { Ok(f()? + 1) }\nfn main() {}",
    );
}

#[test]
fn containers() {
    ok("fn main() -> int { let m = #{ \"a\": 1 }\n m[\"a\"] }");
    ok("fn main() -> List[int] { [1, 2].map(|x| x * 2) }");
}

#[test]
fn strings_have_methods_not_indices() {
    ok("fn main() -> string { \"abc\".chars()\n \"abc\".slice(0, 1) }");
}

#[test]
fn let_else_must_diverge() {
    ok("fn main() -> int { let Some(x) = Some(1) else { return 0 }\n x }");
}

#[test]
fn closure_param_inference() {
    ok("fn apply(f: fn(int) -> int) -> int { f(1) }\nfn main() -> int { apply(|x| x + 1) }");
}

#[test]
fn assignment_targets() {
    ok("fn main() { let x = 1\n x = 2 }");
}

#[test]
fn traits_and_impls() {
    ok(
        "trait Speak { fn speak(self) -> string }\nstruct D { x: int }\n\
         impl Speak for D { fn speak(self) -> string { \"hi\" } }\n\
         fn f(s: dyn Speak) -> string { s.speak() }\n\
         fn main() { f(D { x: 1 }); }",
    );
}

#[test]
fn eq_via_derive() {
    ok("#[derive(Eq)]\nstruct P { x: int }\nfn main() -> bool { P { x: 1 } == P { x: 1 } }");
}

#[test]
fn weak_refs() {
    ok(
        "struct N { v: int }\nfn main() { let n = N { v: 1 }\n let w: weak[N] = weak(n)\n w.upgrade(); }",
    );
}

#[test]
fn annotated_let_checks_init() {
    ok("fn main() { let x: float = 1.5 }");
    // dyn coercion at annotated-let boundaries
    ok("trait T { fn f(self) -> int }\nstruct S { v: int }\n\
        impl T for S { fn f(self) -> int { self.v } }\n\
        fn main() { let d: dyn T = S { v: 1 }\n d.f(); }");
}

#[test]
fn pathological_nesting_errors_instead_of_overflowing() {
    // Sources are generated, so these stay here rather than becoming fixtures.
    // Parser recursion: parens, chained assignment, unary chains, patterns,
    // types.
    fails_with(
        &format!(
            "fn main() {{ let x = {}1{}; }}",
            "(".repeat(5000),
            ")".repeat(5000)
        ),
        "E0114",
    );
    fails_with(
        &format!("fn main() {{ let mut a = 1; {}1; }}", "a = ".repeat(5000)),
        "E0114",
    );
    fails_with(
        &format!("fn main() {{ let x = {}1; }}", "-".repeat(5000)),
        "E0114",
    );
    fails_with(
        &format!(
            "fn main() {{ match None {{ {}_{} => (), _ => () }} }}",
            "Some(".repeat(5000),
            ")".repeat(5000)
        ),
        "E0114",
    );
    fails_with(
        &format!(
            "fn f(x: {}int{}) {{}}\nfn main() {{}}",
            "list[".repeat(5000),
            "]".repeat(5000)
        ),
        "E0114",
    );
    // Operator/postfix chains are parsed iteratively but deepen the AST,
    // so they spend the same budget.
    fails_with(
        &format!("fn main() {{ let x = 1{}; }}", " + 1".repeat(5000)),
        "E0114",
    );
    fails_with(
        &format!("fn main() {{ let l = [1]{}; }}", "[0]".repeat(5000)),
        "E0114",
    );
    // Alternating parens and index chains keep the parser's counter low
    // while the AST still deepens — the checker's backstop catches it.
    let mut e = String::from("x");
    for _ in 0..60 {
        e = format!("({e}){}", "[0]".repeat(20));
    }
    fails_with(
        &format!("fn main() {{ let x = [[1]]\n let y = {e}; }}"),
        "E0114",
    );
    // Sane nesting still compiles.
    ok(&format!(
        "fn main() {{ let x = {}1{}; }}",
        "(".repeat(80),
        ")".repeat(80)
    ));
    ok(&format!("fn main() {{ let x = 1{}; }}", " + 1".repeat(150)));
}

// ------------------------------------------------------------ unit families

/// Every unit test snippet needs a family in scope; this is the standard one.
const DURATION: &str = "units Duration: int { ns = 1, us = 1_000, ms = 1_000 * us, \
                        s = 1_000 * ms }\n";

fn with_duration(body: &str) -> String {
    format!("{DURATION}{body}")
}

#[test]
fn unit_declarations() {
    ok(&with_duration(
        "fn main() { let d: Duration = 500ms\n println(d) }",
    ));
    // Factors are const-evaluated over the units declared above them.
    ok("units X: int { a = 1, b = 2 * a, c = b * 3 + 1 }\nfn main() {}");
    // int-backed families take whole factors only; float-backed take both.
    ok("units X: float { a = 1.0, b = 0.5, c = 2 }\nfn main() {}");
    // `units` is contextual — still usable as an ordinary name.
    ok("fn main() { let units = 3\n println(units) }");
}

#[test]
fn unit_literals() {
    ok(&with_duration("fn main() { println(500ms) }"));
    ok(&with_duration("fn main() { println(1.5s) }"));
    ok(&with_duration("fn main() { println(0.5us) }"));
    // Ambiguous suffixes resolve given an expected type.
    let two = "units A: int { x = 1, u = 10 }\nunits B: int { y = 1, u = 20 }\n";
    ok(&format!("{two}fn main() {{ let v: A = 5u\n println(v) }}"));
    ok(&format!("{two}fn main() {{ println(A::u(5)) }}"));
}

#[test]
fn unit_arithmetic() {
    ok(&with_duration("fn main() { println((1s + 500ms).ms) }"));
    ok(&with_duration("fn main() { println((1s * 3).ms) }"));
    ok(&with_duration("fn main() { println((3 * 1s).ms) }"));
    ok(&with_duration("fn main() { println((1s / 3).ms) }"));
    ok(&with_duration("fn main() -> int { 1s / 1ms }"));
    ok(&with_duration("fn main() -> bool { 1s > 1ms }"));
    ok(&with_duration(
        "fn main() { let mut_a = 1s\n mut_a += 1ms\n mut_a *= 2\n println(mut_a) }",
    ));
}

#[test]
fn unit_conversions() {
    ok(&with_duration("fn main() -> int { 1s.ms }"));
    ok(&with_duration(
        "fn main() -> Duration { Duration::ms(250) }",
    ));
}

#[test]
fn unit_impls() {
    ok(&with_duration(
        "impl Display for Duration { fn fmt(self) -> string { \"d\" } }\nfn main() {}",
    ));
    ok(&with_duration(
        "impl Duration { fn long(self) -> bool { self > 1s } }\nfn main() {}",
    ));
}

/// Units are erased at compile time: a function written with a unit family
/// must emit exactly the bytecode of the same function written with bare
/// `int`s. This is the zero-cost claim, asserted rather than assumed.
#[test]
fn units_are_zero_cost() {
    fn code_of(src: &str, fn_name: &str) -> Vec<String> {
        let compiled = wscript_compiler::compile(src, &Registry::new())
            .unwrap_or_else(|d| panic!("compile failed: {d:?}\n--- src ---\n{src}"));
        let proto = compiled
            .unit
            .protos
            .iter()
            .find(|p| p.name == fn_name)
            .unwrap_or_else(|| panic!("no proto named {fn_name}"));
        proto.code.iter().map(|i| format!("{i:?}")).collect()
    }

    let with_units = "\
units Duration: int { ns = 1, us = 1_000, ms = 1_000 * us, s = 1_000 * ms }
fn work(d: Duration) -> int {
    let total = d + 500ms
    let scaled = total * 3
    (scaled / 2).us
}
fn main() { println(work(1s)) }
";
    // The same arithmetic spelled out in base units by hand.
    let by_hand = "\
fn work(d: int) -> int {
    let total = d + 500000000
    let scaled = total * 3
    (scaled / 2) / 1000
}
fn main() { println(work(1000000000)) }
";
    assert_eq!(code_of(with_units, "work"), code_of(by_hand, "work"));
}
