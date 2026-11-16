//! x-drop-01: locals are dropped in REVERSE order of declaration when their scope ends.
//!
//! The Reference, "Destructors", "Drop scopes": "When control flow leaves a drop scope all
//! variables associated to that scope are dropped in reverse order of declaration (for
//! variables) or creation (for temporaries)." Each drop appends its id as a decimal digit:
//! log = log * 10 + id.
//! 1. inside the block nothing is dropped yet: log 0 before its end.
//! 2. a, b, c declared 1, 2, 3 are dropped 3, 2, 1: log 321.
//! 3. a nested block's local (4) drops at the nested block's end, before the outer's (5):
//!    log 321 then 4 then 5: 32145.
//! A wrong builder that drops in declaration order exits 2 (123).

use core::cell::Cell;

struct D<'a> {
    id: u32,
    log: &'a Cell<u32>,
}

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.log.set(self.log.get() * 10 + self.id);
    }
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let mut seen_inside = 99;
    {
        let _a = D { id: 1, log: &log };
        let _b = D { id: 2, log: &log };
        let _c = D { id: 3, log: &log };
        seen_inside = log.get();
    }
    let after_three = log.get();
    {
        let _outer = D { id: 5, log: &log };
        {
            let _inner = D { id: 4, log: &log };
        }
    }
    let status = if seen_inside != 0 {
        1
    } else if after_three != 321 {
        2
    } else if log.get() != 32145 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
