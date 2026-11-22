//! x-iter-01: a struct implementing `Iterator` by hand, driven by `for` and by core's methods.
//!
//! The Reference, "Iterator loops": "A `for` expression ... the iterator expression ... must
//! implement `IntoIterator`" and the loop runs the body once per `Some` from `next`, stopping
//! at the first `None`. Every `Iterator` is `IntoIterator` (core's blanket impl).
//! Countdown { n: 4 } yields 4, 3, 2, 1, then None.
//! 1. a `for` loop summing: 4 + 3 + 2 + 1 = 10; and its digits in order: 4321.
//! 2. core's `count()`: 4.     3. core's `sum::<u32>()`: 10.
//! 4. `next` by hand after the end keeps giving None: n = 0 then None twice.
//! A wrong builder that tests `next` once and loops forever is a hang (the run's watchdog);
//! one that runs the body for the None exits 1.

struct Countdown {
    n: u32,
}

impl Iterator for Countdown {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.n == 0 {
            return None;
        }
        self.n -= 1;
        Some(self.n + 1)
    }
}

fn main() -> std::entry::Status {
    let (mut sum, mut digits) = (0, 0);
    for v in (Countdown { n: 4 }) {
        sum += v;
        digits = digits * 10 + v;
    }
    let mut c = Countdown { n: 1 };
    let first = c.next();
    let status = if sum != 10 || digits != 4321 {
        1
    } else if (Countdown { n: 4 }).count() != 4 {
        2
    } else if (Countdown { n: 4 }).sum::<u32>() != 10 {
        3
    } else if first != Some(1) || c.next().is_some() || c.next().is_some() {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
