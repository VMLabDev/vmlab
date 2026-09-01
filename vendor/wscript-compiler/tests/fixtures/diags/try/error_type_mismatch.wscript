fn f() -> Result[int, string] { Ok(1) }
fn g() -> Result[int, int] { Ok(f()?) }
fn main() {}
