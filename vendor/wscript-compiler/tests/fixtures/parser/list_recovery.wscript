// One broken element per delimited-list shape, so the snapshot records
// what each list keeps, what it drops and where it resyncs. The shapes
// are the ones the list combinator unified; before it, each loop had
// hand-written recovery and they did not agree.

struct S {
    a: int
    b: int,
    : bool,
    c: string,
}

enum E {
    A { x: int  y: bool },
    B(int bool),
    C,
}

units Dur: int {
    ms = 1
    us = 2,
    = 3
}

fn tuple(v: List[int bool], g: fn(int bool) -> int) -> int { 0 }

fn generic[T U](x: T) -> T { x }

fn broken(a: int b: bool) -> int { 0 }

fn main() {
    let list = [1 2, 3]
    let map = #{ "a": 1  "b": 2 }
    let lit = S { a: 1  b: 2, c: "x" }
    let call = broken(1 2)
    let clo = |x y| x
    match lit {
        S { a  b } => 0,
        E::B(1 2) => 1
        _ => 2,
    }
}
