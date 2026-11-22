//! x-drop-05: assigning to an initialized place drops the old value first; to an uninitialized
//! local, nothing.
//!
//! The Reference, "Assignment expressions": "It then has the effect of first dropping the
//! value at the assigned place, unless the place is an uninitialized local variable or an
//! uninitialized field of a local variable. Next it either copies or moves the assigned value
//! to the assigned place." Each drop appends its id: log = log * 10 + id.
//! 1. `let mut a = D(1); a = D(2);`: the old 1 is dropped at the assignment: log 1.
//! 2. `let b; b = D(3);`: b was uninitialized: nothing dropped: still 1.
//! 3. a field: `p.0 = D(5)` over D(4) drops 4: log 14.
//! 4. at the block's end, reverse declaration: p (holding 5), then b (3), then a (2): 14532.
//! A wrong builder that forgets the old value exits 1 (log 0); one that drops the
//! uninitialized local's garbage exits 2 (or crashes).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let (one, two, three) = {
        let mut a = D(1, &log);
        a = D(2, &log);
        let one = log.get();
        let b;
        b = D(3, &log);
        let two = log.get();
        let mut p = (D(4, &log), 0u8);
        p.0 = D(5, &log);
        let _keep = (&a, &b, &p);
        (one, two, log.get())
    };
    let status = if one != 1 {
        1
    } else if two != 1 {
        2
    } else if three != 14 {
        3
    } else if log.get() != 14532 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
