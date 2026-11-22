//! x-ref-01: writes through `&mut` reach the place, through one and two levels of reference.
//!
//! The Reference, "The dereference operator": "On non-pointer types `*x` is equivalent to
//! `*std::ops::Deref::deref(&x)`" and for references `*r` is the place `r` points to; a write
//! through it changes that place.
//! 1. `let r = &mut x; *r += 4;` with x = 1: x = 5.
//! 2. `let pp: &mut &mut u32 = &mut r; **pp = 9;`: x = 9.
//! 3. a function `bump(p: &mut &mut u32)` adding 10 through two levels: x = 19.
//! 4. retargeting the outer reference: `*pp = &mut y` then `**pp = 3` writes y, not x:
//!    x stays 19, y = 3.
//! A wrong builder that writes to a copy (the pointer itself, or a temporary) exits 1.

fn bump(p: &mut &mut u32) {
    **p += 10;
}

fn main() -> std::entry::Status {
    let mut x = 1u32;
    let mut y = 0u32;
    let (one, two, three) = {
        let mut r = &mut x;
        *r += 4;
        let one = *r;
        let pp: &mut &mut u32 = &mut r;
        **pp = 9;
        let two = **pp;
        bump(pp);
        let three = **pp;
        *pp = &mut y;
        **pp = 3;
        (one, two, three)
    };
    let status = if one != 5 {
        1
    } else if two != 9 {
        2
    } else if three != 19 {
        3
    } else if x != 19 || y != 3 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
