//! x-drop-10: `mem::replace` and `mem::swap` move values without dropping any.
//!
//! core's docs: `mem::replace`: "Moves `src` into the referenced `dest`, returning the previous
//! `dest` value. Neither value is dropped."; `mem::swap`: "Swaps the values at two mutable
//! locations, without deinitializing either one." Each drop appends its id:
//! log = log * 10 + id.
//! 1. `let old = replace(&mut a, D(2))` over a = D(1): no drop; old.0 = 1, a.0 = 2: log 0.
//! 2. `swap(&mut a, &mut b)` with b = D(3): a.0 = 3, b.0 = 2, log still 0.
//! 3. the scope's end drops in reverse declaration: b (holds 2), old (1), a (3): 213.
//! A wrong builder whose `replace` drops the old value exits 1; one that swaps only one way
//! exits 2.

use core::cell::Cell;
use core::mem::{replace, swap};

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let (first, second) = {
        let mut a = D(1, &log);
        let old = replace(&mut a, D(2, &log));
        let first = old.0 * 10 + a.0 + log.get() * 100;
        let mut b = D(3, &log);
        swap(&mut a, &mut b);
        (first, a.0 * 10 + b.0 + log.get() * 100)
    };
    let status = if first != 12 {
        1
    } else if second != 32 {
        2
    } else if log.get() != 213 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
