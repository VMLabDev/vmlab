trait Speak { fn speak(self) -> string }
struct D { x: int }
fn f(s: dyn Speak) {}
fn main() { f(D { x: 1 }) }
