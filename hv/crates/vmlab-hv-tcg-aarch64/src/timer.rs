//! The generic timer as a value (DESIGN.md G3, A6): two timers that compare
//! the system counter with a compare value, and their output lines. The core
//! knows no interrupt controller: a machine wires the lines.
//!
//! Sources read: S6, the answers of a running QEMU to reads and writes of
//! the timer registers (`tests/qemu-cortex-a53-sysregs.txt`, and the
//! lockstep). No file of S2 to S5 was opened.

/// One timer: the physical one or the virtual one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timer {
    /// ENABLE and IMASK, the two bits of the control register that a write
    /// changes.
    pub control: u8,
    pub compare: u64,
}

impl Timer {
    pub const ENABLE: u8 = 1;
    pub const IMASK: u8 = 2;
    /// ISTATUS, which a read computes.
    pub const ISTATUS: u8 = 4;

    /// Whether the timer is enabled and the counter has reached its compare
    /// value.
    pub fn fired(&self, count: u64) -> bool {
        self.control & Self::ENABLE != 0 && count >= self.compare
    }

    /// The control register as a read gives it, with ISTATUS.
    pub fn control(&self, count: u64) -> u64 {
        let status = if self.fired(count) { Self::ISTATUS } else { 0 };
        u64::from(self.control | status)
    }

    pub fn set_control(&mut self, value: u64) {
        self.control = value as u8 & (Self::ENABLE | Self::IMASK);
    }

    /// The timer value: the 32 bits that are left until the compare value.
    pub fn value(&self, count: u64) -> u64 {
        u64::from(self.compare.wrapping_sub(count) as u32)
    }

    /// A write of the timer value: the compare value is the counter plus the
    /// signed 32 bits.
    pub fn set_value(&mut self, count: u64, value: u64) {
        self.compare = count.wrapping_add(i64::from(value as u32 as i32) as u64);
    }

    /// The output line: fired and not masked.
    pub fn line(&self, count: u64) -> bool {
        self.fired(count) && self.control & Self::IMASK == 0
    }

    /// The count at which the line rises, when it is low and can rise.
    pub fn deadline(&self, count: u64) -> Option<u64> {
        let armed = self.control & Self::ENABLE != 0 && self.control & Self::IMASK == 0;
        (armed && count < self.compare).then_some(self.compare)
    }
}

/// The output lines of the two timers. On the `virt` machine the physical
/// line is PPI 30 and the virtual line PPI 27.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    pub physical: bool,
    pub virtual_: bool,
}

/// The lines of the timers `[physical, virtual]` at `count`.
pub fn lines(timers: &[Timer; 2], count: u64) -> Lines {
    Lines {
        physical: timers[0].line(count),
        virtual_: timers[1].line(count),
    }
}

/// The earliest count at which a line rises. `None`: no line can rise
/// without a write to a timer register.
pub fn deadline(timers: &[Timer; 2], count: u64) -> Option<u64> {
    timers
        .iter()
        .filter_map(|timer| timer.deadline(count))
        .min()
}

/// Where the count of the system counter comes from (DESIGN.md V4).
#[derive(Clone, Copy, Debug)]
pub enum Clock {
    /// A count that the owner of the CPU sets: a test, or the lockstep,
    /// where the leader gives it.
    Manual(u64),
    /// Host time: `base` at `origin`, then `frequency` counts per second. A
    /// machine keeps `base` over a pause and in a snapshot.
    Host {
        origin: std::time::Instant,
        base: u64,
        frequency: u64,
    },
}

impl Clock {
    /// A clock of host time that starts at `base` now.
    pub fn host(base: u64, frequency: u64) -> Self {
        Self::Host {
            origin: std::time::Instant::now(),
            base,
            frequency,
        }
    }

    pub fn count(&self) -> u64 {
        match *self {
            Self::Manual(count) => count,
            Self::Host {
                origin,
                base,
                frequency,
            } => {
                let elapsed = origin.elapsed().as_nanos();
                let ticks = elapsed * u128::from(frequency) / 1_000_000_000;
                base.wrapping_add(ticks as u64)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timer_fires_at_its_compare_value_and_the_mask_holds_the_line() {
        let mut timer = Timer::default();
        timer.compare = 100;
        // Disabled: no status and no line, whatever the count.
        assert_eq!(timer.control(500), 0);
        assert!(!timer.line(500));
        assert_eq!(timer.deadline(0), None);
        timer.set_control(1);
        assert_eq!(timer.control(99), 1);
        assert!(!timer.line(99));
        assert_eq!(timer.deadline(99), Some(100));
        assert_eq!(timer.control(100), 5);
        assert!(timer.line(100));
        assert_eq!(timer.deadline(100), None);
        // Masked: the status shows, and the line stays low.
        timer.set_control(0xffff_ffff_ffff_ffff);
        assert_eq!(timer.control, 3);
        assert_eq!(timer.control(100), 7);
        assert!(!timer.line(100));
        assert_eq!(timer.deadline(0), None);
        // The compare value is unsigned.
        timer.set_control(1);
        timer.compare = u64::MAX;
        assert!(!timer.line(1 << 63));
        assert!(timer.line(u64::MAX));
    }

    #[test]
    fn the_timer_value_is_32_signed_bits_from_the_counter() {
        let mut timer = Timer::default();
        timer.set_value(1000, 50);
        assert_eq!(timer.compare, 1050);
        assert_eq!(timer.value(1000), 50);
        assert_eq!(timer.value(1051), 0xffff_ffff);
        // A negative value is in the past. The bits above 31 are not read.
        timer.set_value(1000, 0xdead_beef_ffff_fff0);
        assert_eq!(timer.compare, 984);
        timer.set_control(1);
        assert!(timer.line(1000));
    }

    #[test]
    fn the_two_lines_and_the_next_deadline() {
        let mut timers = [Timer::default(); 2];
        timers[0].compare = 300;
        timers[0].set_control(1);
        timers[1].compare = 200;
        timers[1].set_control(1);
        assert_eq!(lines(&timers, 100), Lines::default());
        assert_eq!(deadline(&timers, 100), Some(200));
        assert_eq!(
            lines(&timers, 250),
            Lines {
                physical: false,
                virtual_: true
            }
        );
        assert_eq!(deadline(&timers, 250), Some(300));
        assert_eq!(deadline(&timers, 300), None);
    }

    #[test]
    fn a_manual_clock_stands_and_a_host_clock_runs_from_its_base() {
        assert_eq!(Clock::Manual(7).count(), 7);
        let clock = Clock::host(1 << 40, 1_000_000_000);
        let first = clock.count();
        assert!(first >= 1 << 40);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = clock.count();
        assert!(second >= first + 1_000_000, "{first} {second}");
    }
}
