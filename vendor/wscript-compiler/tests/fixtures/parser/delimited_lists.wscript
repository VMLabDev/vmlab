struct P { x: int, y: int }
enum E { A, B(int, bool), C { v: bool } }

fn f(a: int, b: string) -> List[int] {
    let list = [1, 2, 3]
    let map = #{ "a": 1, "b": 2 }
    let lit = P { x: 1, y: 2 }
    let call = f(1, "two")
    let closure = |x: int, y: int| x + y
    match E::A {
        E::B(n, b) => n,
        E::C { v } => 1,
        _ => 0,
    }
    [1]
}
