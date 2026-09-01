enum E { A(int), B(int) }
fn main() -> int { match E::A(1) { E::A(n) | E::B(n) => n } }
