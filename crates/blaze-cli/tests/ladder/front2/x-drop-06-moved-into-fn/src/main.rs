//! x-drop-06: a value moved into a function is dropped there, once; the caller drops nothing.
//!
//! The Reference, "Destructors": "When an initialized variable or temporary goes out of scope,
//! its destructor is run"; "Scopes of function parameters": "All function parameters are in
//! the scope of the entire function body, so are dropped last when evaluating a function."; a
//! moved-from local is no longer initialized. Each drop appends its id: log = log * 10 + id.
//! 1. `take(a)` with a = D(1): dropped at the end of `take`: log 1 right after the call.
//! 2. `take` declares a local D(2) after its parameter: locals of the body drop before the
//!    parameter, so the call itself logs 2 then 1: log after the call is 21.
//! 3. at the end of `main`'s block nothing more from a: log stays 21 (a second drop would make
//!    it 211).
//! A wrong builder that drops the argument in the caller too exits 3 (211).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn take(d: D<'_>) -> u32 {
    let _local = D(2, d.1);
    d.0
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let (id, after_call) = {
        let a = D(1, &log);
        let id = take(a);
        (id, log.get())
    };
    let status = if id != 1 {
        1
    } else if after_call != 21 {
        2
    } else if log.get() != 21 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
