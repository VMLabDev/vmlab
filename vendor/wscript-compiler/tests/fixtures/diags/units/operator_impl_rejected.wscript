units Duration: int { ns = 1, us = 1_000, ms = 1_000 * us, s = 1_000 * ms }
impl Add for Duration { fn add(self, o: Duration) -> Duration { self } }
fn main() {}
