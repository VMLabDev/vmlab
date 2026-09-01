enum E { A, B(int), C { v: bool } }

fn f(e: E) -> int {
    match e {
        E::A => 0,
        E::B(n) | E::B(_) => n,
        E::C { v } if v => 1,
        _ => 2,
    }
}
