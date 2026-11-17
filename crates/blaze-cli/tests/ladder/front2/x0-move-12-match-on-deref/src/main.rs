//! x0-move-12: matches on a place behind a reference move nothing (no_core; success 42, else
//! the failed check's number). The Reference, "Match expressions": "If the scrutinee expression
//! is a place expression, the match does not allocate a temporary location; however, a
//! by-value binding may copy or move from the memory location"; "Binding modes": `ref` binds a
//! reference to the place; "Field access expressions": `self.reach` with `self: &Io` is a place
//! behind the reference. Shapes from the hello-world path: core's `Result::is_ok`
//! (`matches!(*self, Ok(_))`) and bstd's `Io::write_out` (`if let Reach::Hosted = self.reach`).
//! log = log * 10 + id.
//! 1, 2. `is_ok` (`match *self` binding nothing) is right for both variants.
//! 3. `id` (`match *self` binding `ref d` and a `u32` by copy) reads the place.
//! 4. `is_hosted` (`if let` on `self.reach`) is right for both variants.
//! 5. none of these dropped anything: log is 0.
//! 6. the scope's end drops `io`'s `D(2)` then `ok`'s `D(1)`, once each: 21.
//! A builder that owns the scrutinee as a temporary drops it at each call (5 fails).
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

struct D(u32, *mut u32);

impl Drop for D {
    fn drop(&mut self) {
        // SAFETY: the pointer is `check`'s live local; one thread.
        unsafe { *self.1 = *self.1 * 10 + self.0 };
    }
}

enum R {
    Good(D),
    Bad(u32),
}

impl R {
    /// What `matches!(*self, R::Good(_))` expands to.
    fn is_ok(&self) -> bool {
        match *self {
            R::Good(_) => true,
            _ => false,
        }
    }

    fn id(&self) -> u32 {
        match *self {
            R::Good(ref d) => d.0,
            R::Bad(n) => n,
        }
    }
}

enum Reach {
    Shared(D),
    Hosted,
}

struct Io {
    reach: Reach,
}

impl Io {
    fn is_hosted(&self) -> bool {
        if let Reach::Hosted = self.reach { true } else { false }
    }
}

fn check() -> i32 {
    let mut cell = 0u32;
    let log: *mut u32 = &raw mut cell;
    let failed = {
        let ok = R::Good(D(1, log));
        let bad = R::Bad(7);
        let io = Io { reach: Reach::Shared(D(2, log)) };
        let hosted = Io { reach: Reach::Hosted };
        if ok.is_ok() {
            if bad.is_ok() {
                2
            } else if ok.id() != 1 {
                3
            } else if bad.id() != 7 {
                3
            } else if io.is_hosted() {
                4
            } else if hosted.is_hosted() {
                // SAFETY: as in `D::drop`.
                if unsafe { *log } != 0 { 5 } else { 0 }
            } else {
                4
            }
        } else {
            1
        }
    };
    if failed != 0 {
        return failed;
    }
    // SAFETY: as in `D::drop`.
    let end = unsafe { *log };
    if end != 21 { 6 } else { 0 }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let failed = check();
    if failed == 0 { 42 } else { failed }
}
