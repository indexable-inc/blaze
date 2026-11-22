//! x-gen-08: an impl for `&T` beside an impl for `T`: which one a call reaches.
//!
//! trait Tag { fn tag(&self) -> u32; } with `impl Tag for u32` (the value) and
//! `impl<T: Tag> Tag for &T` (1000 + the pointee's tag).
//! 1. a generic `call<T: Tag>(t: T)` at T = u32: 5 -> 5.
//! 2. at T = &u32: the `&T` impl: 1000 + 5 = 1005.     3. at T = &&u32: 1000 + 1005 = 2005.
//! 4. a METHOD call on a `&u32` receiver: the Reference, "Method call expressions": "The first
//!    step is to build a list of candidate receiver types" starting with the receiver's own type
//!    `&u32`, then `&&u32`, ...; "for each candidate type T, search for a visible method with a
//!    receiver of that type". At the first candidate `&u32`, `<u32 as Tag>::tag` has receiver
//!    type `&u32` (its `&self` with Self = u32): it is found first: 5 (as `(&x).clone()` gives
//!    a u32). `<&u32 as Tag>::tag` would need receiver `&&u32`.
//! A wrong builder that picks the `&T` impl for the method call exits 4 (1005).

trait Tag {
    fn tag(&self) -> u32;
}

impl Tag for u32 {
    fn tag(&self) -> u32 {
        *self
    }
}

impl<T: Tag> Tag for &T {
    fn tag(&self) -> u32 {
        1000 + (**self).tag()
    }
}

fn call<T: Tag>(t: T) -> u32 {
    t.tag()
}

fn main() -> std::entry::Status {
    let x = 5u32;
    let r: &u32 = &x;
    let status = if call(x) != 5 {
        1
    } else if call(r) != 1005 {
        2
    } else if call(&r) != 2005 {
        3
    } else if r.tag() != 5 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
