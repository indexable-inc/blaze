//! x-drop-12: shadowing a variable does not drop it; both live to the end of the scope.
//!
//! The Reference, "Destructors", "Drop scopes": variables are dropped "When control flow
//! leaves a drop scope", "in reverse order of declaration". A `let` that reuses a name declares
//! a NEW variable; the old one is still declared in the same scope, only no longer nameable.
//! Each drop appends its id: log = log * 10 + id.
//! 1. after `let a = D(1); let a = D(2);` nothing is dropped: log 0, and `a` reads 2.
//! 2. at the scope's end: the second `a` (2), then the first (1): 21.
//! A wrong builder that drops a shadowed variable at the shadowing exits 1 (log 1).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let seen = {
        let a = D(1, &log);
        let _first = &a;
        let a = D(2, &log);
        log.get() * 10 + a.0
    };
    let status = if seen != 2 {
        1
    } else if log.get() != 21 {
        2
    } else {
        0
    };
    std::entry::Status(status)
}
