//! x-match-06: `Option<&T>` and default binding modes through references.
//!
//! The Reference, "Binding modes": "When a reference value is matched by a non-reference
//! pattern, it will be automatically dereferenced and the default binding mode updated": to
//! `ref` through `&`, to `ref mut` through `&mut`.
//! 1. Some(&5) against `Some(&5)`: matches the pointee: 1.
//! 2. Some(&9) against `Some(&5)` fails, `Some(&n)` binds n = 9 by value.
//! 3. `&Some(3)` against `Some(n)`: n is a `&u32` to 3; *n + 1 = 4.
//! 4. `&(1, 2)` against `(a, b)`: *a * 10 + *b = 12.
//! 5. `&mut (1, 2)` against `(a, _)`: a is `&mut`; `*a = 7` writes the tuple: p.0 = 7.
//! 6. None::<&u32> against `Some(_)` fails: the `None` arm, 0 (a null pointer read as Some
//!    would give 1).
//! A wrong builder that matches the reference's address instead of the pointee exits 1.

fn opt(o: Option<&u32>) -> u32 {
    match o {
        Some(&5) => 1,
        Some(&n) => n,
        None => 0,
    }
}

fn main() -> std::entry::Status {
    let five = 5;
    let nine = 9;
    let r: &Option<u32> = &Some(3);
    let via_ref = match r {
        Some(n) => *n + 1,
        None => 0,
    };
    let (a, b) = &(1u32, 2u32);
    let mut p = (1u32, 2u32);
    match &mut p {
        (a, _) => *a = 7,
    }
    let status = if opt(Some(&five)) != 1 {
        1
    } else if opt(Some(&nine)) != 9 {
        2
    } else if via_ref != 4 {
        3
    } else if *a * 10 + *b != 12 {
        4
    } else if p.0 != 7 {
        5
    } else if opt(None) != 0 {
        6
    } else {
        0
    };
    std::entry::Status(status)
}
