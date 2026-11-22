//! x-clo-06: an `FnOnce` closure that moves a captured value out consumes it once; the value is
//! dropped where the closure's body puts it, not twice.
//!
//! The Reference, "Closure types", "Call traits and coercions": "`FnOnce`: The closure can be
//! called once"; a closure that moves a captured value out of its body is only `FnOnce`.
//! "Destructors": a value is dropped once, when its owner goes out of scope. Each drop appends
//! its id: log = log * 10 + id.
//! 1. d = D(1); f = move || { let x = d; x.0 + 10 }: nothing dropped when f is made: log 0.
//! 2. once(f) = 11; x (holding D(1)) is dropped at the end of the body: log 1.
//! 3. a second closure g = move || d2 returns its D(2) to the caller, who keeps it: after the
//!    call log is still 1; at the scope's end 12.
//! 4. an FnOnce never called: its captured D(3) is dropped with the closure: log 123.
//! A wrong builder that drops the capture again with the closure exits 2 or 3 (11..).

use core::cell::Cell;

struct D<'a>(u32, &'a Cell<u32>);

impl Drop for D<'_> {
    fn drop(&mut self) {
        self.1.set(self.1.get() * 10 + self.0);
    }
}

fn once<F: FnOnce() -> R, R>(f: F) -> R {
    f()
}

fn main() -> std::entry::Status {
    let log = Cell::new(0);
    let d = D(1, &log);
    let f = move || {
        let x = d;
        x.0 + 10
    };
    let made = log.get();
    let r = once(f);
    let two = log.get();
    let three = {
        let d2 = D(2, &log);
        let kept = once(move || d2);
        let seen = log.get();
        let _k = kept;
        seen
    };
    {
        let d3 = D(3, &log);
        let _unused = move || d3;
    }
    let status = if made != 0 {
        1
    } else if r != 11 || two != 1 {
        2
    } else if three != 1 || log.get() / 10 != 12 {
        3
    } else if log.get() != 123 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
