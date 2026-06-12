# wisp language essentials (for vmlab scripts)

wisp is a statically typed, Rust-flavored scripting language. Think Rust
minus the borrow checker, lifetimes, and user generics. Full tour:
`../wisp/docs/tour.md` (sibling repo). vmlab compiles scripts with full
type checking at `vmlab validate` time.

## Types & values

```rust
let x = 5                  // int (64-bit signed, wrapping); inference everywhere
let name: string = "wil"   // annotations allowed on lets, required only on fn signatures
let pi = 3.14              // float
let log = "hp: " + str(99)         // + concatenates strings; str() converts
let msg = fmt("{} of {}", 3, 10)   // {} formatting (no string interpolation)
```

- **No implicit numeric conversion**: `1 + 2.0` is a type error — use
  `int(x)` / `float(x)`.
- **No truthiness**: conditions must be `bool`.
- Statements end at newlines; semicolons optional. Lines starting with `.`
  continue a method chain.
- Reference semantics for `string`/structs/lists/maps (assignment copies
  the reference); `clone()` for deep copies.

## Functions

```rust
fn area(w: int, h: int) -> int { w * h }   // blocks evaluate to last expression
fn log_it(msg: string) { println(msg) }    // omitted return type = unit
let double = |x| x * 2                     // closures
```

## Control flow

```rust
if cond { } else { }
for i in 0..10 { }          // exclusive; 0..=10 inclusive
for x in [1, 2, 3] { }      // list elements (also map keys, string chars)
while cond { }
loop { if done { break } }
```

## Pattern matching & errors (the idioms vmlab scripts live on)

`Option[T]` and `Result[T, E]` are built in. All fallible vmlab API calls
return `Result[..., string]`.

```rust
// let-else: bail early (block must diverge — return/break)
let Ok(dc) = lab.vm("dc01") else {
    lab.log("dc01 is not defined")
    return
}

// match (exhaustiveness-checked at compile time)
match dc.wait_ready(600) {
    Ok(_)  => lab.log("ready"),
    Err(e) => { lab.log("not ready: " + e); return }
}

// ? propagates Err/None out of a function with a matching return type
fn step(lab: Lab) -> Result[unit, string] {
    let dc = lab.vm("dc01")?
    dc.wait_ready(600)?
    Ok(())
}
```

Methods: `is_some is_none unwrap unwrap_or expect` /
`is_ok is_err unwrap unwrap_or unwrap_err expect`.

NOTE: a function with no return type must not end on a non-unit
expression — bind unused Results (`let _ = vm.mouse_click("left")`) or
add `;` to discard.

## Containers & strings

```rust
let xs = [1, 2, 3]                       // List[int]
xs.push(4); xs.get(99)                   // .get -> Option (never faults); xs[99] faults
xs.map(|x| x * 2).filter(|x| x > 2).fold(0, |a, x| a + x)
let ages = #{ "alice": 30 }              // Map[string, int]; keys: int/bool/char/string
```

List: `len is_empty push pop get set insert remove clear contains index_of
reverse sort join map filter fold first last slice concat clone`.
Map: `len is_empty insert remove get contains_key keys values clear clone`.
String (immutable): `len bytes_len is_empty split trim trim_start trim_end
to_upper to_lower starts_with ends_with contains find replace repeat
pad_left pad_right chars slice parse_int parse_float`.

## Modules & prelude

`use vmlab` imports the vmlab host module (registered types like `Lab`,
`Vm`, `Match` are ambient — no `use` needed for type names). Prelude,
always available: `print println str fmt same weak int float`. Scripts are
single files in v1 — no script-to-script imports.

## Not in wisp (v1)

No `&`/`&mut`, lifetimes, user generics, exceptions, async, threads,
truthiness, implicit conversions, string interpolation, `+=`, bitwise ops.

Source of truth: `/home/wil/dev/wisp/docs/tour.md`.
