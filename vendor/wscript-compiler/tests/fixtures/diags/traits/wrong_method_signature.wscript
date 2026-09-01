trait Speak { fn speak(self) -> string }
struct D { x: int }
impl Speak for D { fn speak(self) -> int { 1 } }
fn main() {}
