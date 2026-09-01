enum E { A, B(int), C { v: bool } }
fn main() { match E::A { E::A => (), E::B(_) => () } }
