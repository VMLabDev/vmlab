trait Speak { fn speak(self) -> string }
struct D { x: int }
impl Speak for D { fn speak() -> string { "hi" } }
fn main() {}
