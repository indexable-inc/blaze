//! x-drop-07: a value moved on one path only is dropped exactly once on every path (drop flags).
//!
//! The Reference, "Destructors": "When an initialized variable or temporary goes out of scope,
//! its destructor is run, or it is dropped." Whether `a` is still initialized at the end of
//! the scope is decided when the program runs, so each run drops it once: in `take` when moved,
//! at the scope's end otherwise. Each drop appends its id: log = log * 10 + id.
//! 1. moved = true: `take` drops a (1) at once; the scope's end then drops b (2): 12.
//! 2. moved = false: nothing at the `if`; the scope's end drops b (2) then a (1): 21.
//! 3. a value moved into another local (`let c = a;`) and the scope ends: one drop, log 1.
//! A wrong builder with no drop flag drops twice (exits 1, 121) or never (exits 2, 2).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn take(_d: D<'_>) {}

fn run(moved: bool) -> u32 {
    let log = Cell::new(0);
    {
        let a = D(1, &log);
        let _b = D(2, &log);
        if moved {
            take(a);
        }
    }
    log.get()
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    {
        let a = D(1, &log);
        let _c = a;
    }
    let status = if run(true) != 12 {
        1
    } else if run(false) != 21 {
        2
    } else if log.get() != 1 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
