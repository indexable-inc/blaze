//! x-drop-02: a struct's own `drop` runs first, then its fields in DECLARATION order.
//!
//! The Reference, "Destructors": "When a value of type T is dropped: If T: Drop, calling
//! <T as core::ops::Drop>::drop; Recursively running the destructor of all of its fields." and
//! "The fields of a struct are dropped in declaration order." Each drop appends its id:
//! log = log * 10 + id.
//! 1. Pair { a: 1, b: 2 } (no Drop of its own): fields a then b: 12.
//! 2. Outer (its Drop appends 9) { first: 3, second: 4 }: 9 then 3 then 4: log 12 then 12934.
//! 3. A struct literal written with its fields in the other order, `Pair { b: .., a: .. }`,
//!    still drops a (5) then b (6): log 129345 6 = 1293456 (declaration order, not
//!    initializer order).
//! A wrong builder that drops fields in reverse exits 1 (21).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

struct Pair<'a> {
    a: D<'a>,
    b: D<'a>,
}

struct Outer<'a> {
    log: &'a Cell<u32>,
    first: D<'a>,
    second: D<'a>,
}

impl Drop for Outer<'_> {
    fn drop(&mut self) {
        self.log.set(self.log.get() * 10 + 9);
    }
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    drop(Pair { a: D(1, &log), b: D(2, &log) });
    let one = log.get();
    drop(Outer { log: &log, first: D(3, &log), second: D(4, &log) });
    let two = log.get();
    drop(Pair { b: D(6, &log), a: D(5, &log) });
    let status = if one != 12 {
        1
    } else if two != 12934 {
        2
    } else if log.get() != 1293456 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
