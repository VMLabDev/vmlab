units Duration: int { ns = 1, us = 1_000, ms = 1_000 * us, s = 1_000 * ms }
units Size: int { b = 1, kb = 1_000 }
fn main() { println(1s / 1kb) }
