//! x-drop-11: an enum drops the fields of its ACTIVE variant only; assigning `None` drops the
//! `Some`'s payload.
//!
//! The Reference, "Destructors": "The fields of the active enum variant are dropped in
//! declaration order." and "Assignment expressions": assigning first drops "the value at the
//! assigned place". Each drop appends its id: log = log * 10 + id.
//! 1. `o = None` over Some(D(1)): drops 1: log 1.
//! 2. `o = Some(D(2))` over None: None has no fields: nothing dropped: still 1.
//! 3. `let t = o.take()` (core: "Takes the value out of the option, leaving a `None` in its
//!    place"): no drop; t holds 2.
//! 4. at the scope's end: t (declared after o) drops 2; o is None and drops nothing: 12.
//! A wrong builder that drops a variant's payload regardless of the discriminant exits 2
//! (or crashes on the garbage).

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
        let mut o = Some(D(1, &log));
        o = None;
        let one = log.get();
        o = Some(D(2, &log));
        let two = log.get();
        let t = o.take();
        let three = log.get() * 10 + if t.is_some() && o.is_none() { 1 } else { 0 };
        (one, two, three)
    };
    let status = if one != 1 {
        1
    } else if two != 1 {
        2
    } else if three != 11 {
        3
    } else if log.get() != 12 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
