//! VM behavior through the public API: fault traces, unwinding, the call
//! depth limit and fuel metering.

use wscript_core::bytecode::CompiledUnit;
use wscript_core::registry::Registry;
use wscript_vm::{Vm, VmConfig};

fn compile(src: &str) -> CompiledUnit {
    wscript_compiler::compile(src, &Registry::new())
        .unwrap_or_else(|d| panic!("test script failed to compile: {d:?}"))
        .unit
}

fn vm() -> Vm {
    Vm::new(&Registry::new(), VmConfig::default())
}

#[test]
fn fault_carries_span_and_trace() {
    let unit = compile("fn main() -> int {\n    let z = 0\n    10 / z\n}");
    let err = vm().call_name(&unit, "main", vec![]).unwrap_err();
    assert_eq!(err.message, "division by zero");
    assert!(!err.trace.is_empty());
    assert_eq!(err.trace[0].function, "main");
    assert!(err.trace[0].span.is_some());
    assert_eq!(err.span, err.trace[0].span);
}

#[test]
fn fault_trace_is_innermost_first() {
    let unit = compile(
        "fn inner() -> int { 1 / 0 }\n\
         fn outer() -> int { inner() }\n\
         fn main() -> int { outer() }",
    );
    let err = vm().call_name(&unit, "main", vec![]).unwrap_err();
    let names: Vec<&str> = err.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, ["inner", "outer", "main"]);
}

#[test]
fn fault_unwinds_frames_vm_reusable() {
    let unit = compile("fn boom() -> int { 1 / 0 }\nfn ok() -> int { 42 }");
    let mut vm = vm();
    assert!(vm.call_name(&unit, "boom", vec![]).is_err());
    // The fault must have unwound its frames; the same VM keeps working.
    let v = vm.call_name(&unit, "ok", vec![]).unwrap();
    assert!(matches!(v, wscript_core::value::Value::Int(42)));
}

#[test]
fn call_depth_limit_faults() {
    let unit = compile("fn rec() -> int { rec() }\nfn main() -> int { rec() }");
    let err = vm().call_name(&unit, "main", vec![]).unwrap_err();
    assert!(err.message.contains("stack overflow"), "{}", err.message);
}

#[test]
fn call_depth_limit_zero_faults_immediately() {
    let unit = compile("fn main() -> int { 1 }");
    let mut vm = vm();
    vm.set_call_depth_limit(0);
    assert!(vm.call_name(&unit, "main", vec![]).is_err());
}

#[test]
fn fuel_exhaustion_faults_and_reads_zero() {
    let unit = compile("fn main() -> int {\n    let n = 0\n    while true { n = n + 1 }\n    n\n}");
    let mut vm = vm();
    vm.set_fuel(Some(100));
    let err = vm.call_name(&unit, "main", vec![]).unwrap_err();
    assert_eq!(err.message, "fuel exhausted");
    assert_eq!(vm.fuel(), Some(0));
}

#[test]
fn fuel_zero_edge_faults() {
    let unit = compile("fn main() -> int {\n    let n = 0\n    while true { n = n + 1 }\n    n\n}");
    let mut vm = vm();
    vm.set_fuel(Some(0));
    assert!(vm.call_name(&unit, "main", vec![]).is_err());
}

#[test]
fn fuel_persists_across_calls() {
    let unit = compile("fn f() -> int { 1 + 2 }");
    let mut vm = vm();
    vm.set_fuel(Some(1_000_000));
    vm.call_name(&unit, "f", vec![]).unwrap();
    let after_one = vm.fuel().unwrap();
    assert!(after_one < 1_000_000);
    vm.call_name(&unit, "f", vec![]).unwrap();
    let after_two = vm.fuel().unwrap();
    assert!(after_two < after_one);
}

#[test]
fn fuel_none_unmetered() {
    let unit =
        compile("fn main() -> int {\n    let n = 0\n    for i in 0..1000 { n = n + i }\n    n\n}");
    let mut vm = vm();
    let v = vm.call_name(&unit, "main", vec![]).unwrap();
    assert!(matches!(v, wscript_core::value::Value::Int(499500)));
    assert_eq!(vm.fuel(), None);
}

#[test]
fn call_name_unknown_fn_errs() {
    let unit = compile("fn main() -> int { 0 }");
    let err = vm().call_name(&unit, "nope", vec![]).unwrap_err();
    assert!(err.message.contains("no function named"), "{}", err.message);
}

// ------------------------------------------------------------ memory limit
//
// vmlab delta (card #1086): `VmConfig::memory`. The limits here are small so
// that no test holds much; none of them has a wall-clock limit.

const MIB: u64 = 1024 * 1024;

fn vm_with_memory(limit: u64) -> Vm {
    Vm::new(
        &Registry::new(),
        VmConfig {
            memory: Some(limit),
            ..VmConfig::default()
        },
    )
}

/// Doubling a string asks for any amount in a few instructions. The
/// concatenation that does not fit is refused before it allocates.
#[test]
fn memory_limit_stops_a_doubling_string() {
    let unit = compile(
        "fn main() -> int {\n\
             let s = \"x\"\n\
             let i = 0\n\
             while i < 40 {\n\
                 s = s + s\n\
                 i = i + 1\n\
             }\n\
             i\n\
         }",
    );
    let err = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap_err();
    assert_eq!(err.message, format!("memory limit of {MIB} bytes exceeded"));
    assert_eq!(err.trace[0].function, "main");
}

/// Without a limit the same script is not stopped by one: it runs until the
/// string is 1 MiB and ends.
#[test]
fn memory_none_is_no_limit() {
    let unit = compile(
        "fn main() -> int {\n\
             let s = \"x\"\n\
             let i = 0\n\
             while i < 20 {\n\
                 s = s + s\n\
                 i = i + 1\n\
             }\n\
             s.bytes_len()\n\
         }",
    );
    let v = vm().call_name(&unit, "main", vec![]).unwrap();
    assert!(matches!(v, wscript_core::value::Value::Int(1048576)));
}

/// A list that grows one element at a time is stopped too: each `push` is
/// small, the list is not.
#[test]
fn memory_limit_stops_a_growing_list() {
    let unit = compile(
        "fn main() -> int {\n\
             let l = [0]\n\
             while true { l.push(1) }\n\
             0\n\
         }",
    );
    let err = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap_err();
    assert!(err.message.starts_with("memory limit of"), "{}", err.message);
}

/// A list of fresh strings is stopped by what the strings take, not by the
/// size of the list alone.
#[test]
fn memory_limit_counts_what_a_list_holds() {
    let unit = compile(
        "fn main() -> int {\n\
             let big = \"0123456789abcdef\".repeat(1024)\n\
             let l = [\"\"]\n\
             while true { l.push(big + \"x\") }\n\
             0\n\
         }",
    );
    let err = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap_err();
    assert!(err.message.starts_with("memory limit of"), "{}", err.message);
}

/// The limit is on what the script holds, not on what it has allocated in
/// total: 4000 strings of 16 KiB, each dropped at the next pass, are 64 MiB
/// of allocation under a limit of 1 MiB.
#[test]
fn memory_limit_does_not_count_dropped_values() {
    let unit = compile(
        "fn main() -> int {\n\
             let big = \"0123456789abcdef\".repeat(1024)\n\
             let n = 0\n\
             let i = 0\n\
             while i < 4000 {\n\
                 let t = big + \"x\"\n\
                 n = n + t.bytes_len()\n\
                 i = i + 1\n\
             }\n\
             n\n\
         }",
    );
    let v = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap();
    assert!(matches!(v, wscript_core::value::Value::Int(65540000)));
}

/// The same value in many places is counted once: 10,000 references to one
/// string of 16 KiB are a list of 10,000 slots, not 160 MiB.
#[test]
fn memory_limit_counts_a_shared_value_once() {
    let unit = compile(
        "fn main() -> int {\n\
             let big = \"0123456789abcdef\".repeat(1024)\n\
             let l = [big]\n\
             let i = 0\n\
             while i < 10000 {\n\
                 l.push(big)\n\
                 i = i + 1\n\
             }\n\
             l.len()\n\
         }",
    );
    let v = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap();
    assert!(matches!(v, wscript_core::value::Value::Int(10001)));
}

/// What `map` collects from its callback is in no register until `map`
/// returns. It is counted while `map` runs.
#[test]
fn memory_limit_counts_what_map_collects() {
    let unit = compile(
        "fn main() -> int {\n\
             let big = \"0123456789abcdef\".repeat(1024)\n\
             let l = [0]\n\
             let i = 0\n\
             while i < 1000 {\n\
                 l.push(i)\n\
                 i = i + 1\n\
             }\n\
             let all = l.map(|x| big + \"x\")\n\
             all.len()\n\
         }",
    );
    let err = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap_err();
    assert!(err.message.starts_with("memory limit of"), "{}", err.message);
}

/// `fold` keeps one value, however many its callback returned: joining 2000
/// lines into 128 KiB returns 128 MiB of strings in total and holds one.
#[test]
fn memory_limit_does_not_count_what_fold_dropped() {
    let unit = compile(
        "fn main() -> int {\n\
             let line = \"0123456789abcdef\".repeat(4)\n\
             let l = [line]\n\
             let i = 1\n\
             while i < 2000 {\n\
                 l.push(line)\n\
                 i = i + 1\n\
             }\n\
             let all = l.fold(\"\", |acc, x| acc + x)\n\
             all.bytes_len()\n\
         }",
    );
    let v = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap();
    assert!(matches!(v, wscript_core::value::Value::Int(128000)));
}

/// A pad to a width the script names is refused before it allocates.
#[test]
fn memory_limit_stops_a_pad_to_any_width() {
    let unit = compile("fn main() -> int { \"x\".pad_left(1000000000000, \" \").bytes_len() }");
    let err = vm_with_memory(MIB)
        .call_name(&unit, "main", vec![])
        .unwrap_err();
    assert!(err.message.starts_with("memory limit of"), "{}", err.message);
}

/// A chain of structs nested to any depth is walked without recursion, and
/// is stopped when it is too long.
#[test]
fn memory_limit_walks_a_deep_value() {
    let unit = compile(
        "struct Node { next: Option[Node] }\n\
         fn main() -> int {\n\
             let cur = Node { next: None }\n\
             while true { cur = Node { next: Some(cur) } }\n\
             0\n\
         }",
    );
    let err = vm_with_memory(64 * MIB)
        .call_name(&unit, "main", vec![])
        .unwrap_err();
    assert!(err.message.starts_with("memory limit of"), "{}", err.message);
}
