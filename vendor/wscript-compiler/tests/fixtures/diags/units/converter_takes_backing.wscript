units Duration: int { ns = 1, us = 1_000, ms = 1_000 * us, s = 1_000 * ms }
fn main() { println(Duration::ms(1s)) }
