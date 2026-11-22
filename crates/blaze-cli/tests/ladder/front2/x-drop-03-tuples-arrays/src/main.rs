//! x-drop-03: tuples drop their fields in order; arrays drop from the first element to the last.
//!
//! The Reference, "Destructors": "The fields of a tuple are dropped in order." and "The
//! elements of an array or owned slice are dropped from the first element to the last."
//! Each drop appends its id: log = log * 10 + id.
//! 1. (1, 2): 12.
//! 2. [3, 4, 5]: 345, so log 12345.
//! 3. An enum's active variant's fields in declaration order: Two(6, 7): 67, log 1234567.
//! A wrong builder that drops array elements last to first exits 2 (12543).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

enum E<'a> {
    Two(D<'a>, D<'a>),
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    drop((D(1, &log), D(2, &log)));
    let one = log.get();
    drop([D(3, &log), D(4, &log), D(5, &log)]);
    let two = log.get();
    drop(E::Two(D(6, &log), D(7, &log)));
    let status = if one != 12 {
        1
    } else if two != 12345 {
        2
    } else if log.get() != 1234567 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
