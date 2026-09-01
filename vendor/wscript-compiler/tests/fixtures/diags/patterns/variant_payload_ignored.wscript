enum E { A(int), B }
fn main() { match E::B { A => (), B => () } }
