//! x-drop-08: an early `return` and a `break` drop what their scopes hold, and nothing more.
//!
//! The Reference, "Destructors", "Drop scopes": "When control flow leaves a drop scope all
//! variables associated to that scope are dropped in reverse order of declaration"; `return`
//! leaves every scope of the function, `break` every scope of the loop body. Each drop appends
//! its id: log = log * 10 + id.
//! 1. early(true): a (1) is live at the `return`, b was never made: log 1, result 7.
//! 2. early(false): b (2) and a (1) at the end: 21, result 8.
//! 3. a loop whose body makes D(3) then breaks on its second pass: two drops, 33; the D(4)
//!    made after the `break` point on the first pass only: the first pass logs 4 then 3
//!    (reverse declaration), the second only 3: 433.
//! A wrong builder that skips drops on `return` exits 1 (log 0).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn early(go: bool, log: &Cell<u32>) -> u32 {
    let _a = D(1, log);
    if go {
        return 7;
    }
    let _b = D(2, log);
    8
}

fn main() -> std::entry::Status {
    let (one, two, three) = (Cell::new(0), Cell::new(0), Cell::new(0));
    let r1 = early(true, &one);
    let r2 = early(false, &two);
    let mut pass = 0;
    loop {
        let _x = D(3, &three);
        pass += 1;
        if pass == 2 {
            break;
        }
        let _y = D(4, &three);
    }
    let status = if one.get() != 1 || r1 != 7 {
        1
    } else if two.get() != 21 || r2 != 8 {
        2
    } else if three.get() != 433 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
