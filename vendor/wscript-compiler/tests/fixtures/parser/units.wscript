units Duration: int {
    ns = 1
    ms = 1_000_000
    s = 1_000 * ms
}

fn f(d: Duration) -> int {
    match d {
        500ms => 1,
        _ => 1.5s.ms,
    }
}
