//! x-drop-09: after a field is moved out of a struct, only the fields left are dropped with it.
//!
//! The Reference, "Destructors": "If a variable is only partially initialized, only its
//! initialized fields are dropped." and "The fields of a struct are dropped in declaration
//! order." Moving `p.a` out leaves `p` partially initialized (a struct with no `Drop` of its
//! own may be moved out of). Each drop appends its id: log = log * 10 + id.
//! 1. `let a = p.a; drop(a);`: a (1) is dropped at the call: log 1.
//! 2. at the scope's end p drops its remaining fields only, b (2) then c (3): 123.
//! A wrong builder that drops p whole drops 1 twice: exits 2 (1123).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

struct P<'a> {
    a: D<'a>,
    b: D<'a>,
    c: D<'a>,
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let after_drop = {
        let p = P { a: D(1, &log), b: D(2, &log), c: D(3, &log) };
        let a = p.a;
        drop(a);
        let seen = log.get();
        let _still = (&p.b, &p.c);
        seen
    };
    let status = if after_drop != 1 {
        1
    } else if log.get() != 123 {
        2
    } else {
        0
    };
    std::entry::Status(status)
}
