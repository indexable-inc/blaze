//! x-drop-04: temporaries are dropped at the end of their statement, in reverse creation order.
//!
//! The Reference, "Destructors", "Temporary scopes": "the temporary scope of an expression is
//! the smallest scope that contains the expression and is one of: The entire function. A
//! statement. ..."; and "Drop scopes": temporaries are dropped "in reverse order of ...
//! creation". Each drop appends its id: log = log * 10 + id.
//! 1. `let n = mk(1).0 + mk(2).0;` creates temporaries 1 then 2 and drops them at the `;`:
//!    log 21 on the next statement, n = 3.
//! 2. `let k = mk(4);` is a variable, not a temporary: nothing dropped yet: still 21.
//! 3. at the end of the block k drops: 214.
//! A wrong builder that keeps temporaries to the end of the block exits 1 (log 0 then); one
//! that drops them in creation order exits 1 too (12).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn mk(id: u32, log: &Cell<u32>) -> D<'_> {
    D(id, log)
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let (after_stmt, n, after_let) = {
        let n = mk(1, &log).0 + mk(2, &log).0;
        let after_stmt = log.get();
        let _k = mk(4, &log);
        (after_stmt, n, log.get())
    };
    let status = if after_stmt != 21 || n != 3 {
        1
    } else if after_let != 21 {
        2
    } else if log.get() != 214 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
