units Duration: int { ns = 1, us = 1_000, ms = 1_000 * us, s = 1_000 * ms }
impl Duration { fn ms(self) -> int { 1 } }
fn main() {}
