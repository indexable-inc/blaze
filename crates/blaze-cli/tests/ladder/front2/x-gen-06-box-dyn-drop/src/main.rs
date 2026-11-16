//! x-gen-06: `Box<dyn Trait>` calls through the vtable and drops its value exactly once.
//!
//! The Reference, "Destructors": "When a value of type T is dropped: If T: Drop, calling <T as
//! core::ops::Drop>::drop"; a `Box<dyn Trait>` drops its pointee through the vtable's drop
//! entry, then frees the box. Each drop appends its id: log = log * 10 + id.
//! 1. b1 = Box<dyn Area> of Counted(3): area 6; nothing dropped while it lives: log 0.
//! 2. dropping b1 by `drop(b1)`: log 3.
//! 3. two boxes in a scope, Counted(4) then Counted(5): reverse order at the end: 3 then 5, 4:
//!    354; areas 8 + 10 = 18.
//! 4. a Box<dyn Area> of a type with no Drop (Plain): area 7, log unchanged 354.
//! A wrong builder whose vtable has no drop entry exits 2 (0); one that drops twice exits 2 (33).

use core::cell::Cell;

trait Area {
    fn area(&self) -> u32;
}

struct Counted<'a>(u32, &'a Cell<u32>);
struct Plain;

impl Area for Counted<'_> {
    fn area(&self) -> u32 {
        self.0 * 2
    }
}

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

impl Area for Plain {
    fn area(&self) -> u32 {
        7
    }
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let b1: Box<dyn Area + '_> = Box::new(Counted(3, &log));
    let one = b1.area() == 6 && log.get() == 0;
    drop(b1);
    let two = log.get();
    let areas = {
        let x: Box<dyn Area + '_> = Box::new(Counted(4, &log));
        let y: Box<dyn Area + '_> = Box::new(Counted(5, &log));
        x.area() + y.area()
    };
    let p: Box<dyn Area> = Box::new(Plain);
    let status = if !one {
        1
    } else if two != 3 {
        2
    } else if log.get() != 354 || areas != 18 {
        3
    } else if p.area() != 7 || log.get() != 354 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
