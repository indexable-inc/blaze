//! x-clo-08: a table of `fn` pointers called by index; a non-capturing closure as a `fn` pointer.
//!
//! The Reference, "Function pointer types": "Function pointer types ... refer to a function
//! whose identity is not necessarily known at compile-time"; "Type coercions": "Function item
//! types to fn pointers" and "Non capturing closures to fn pointers".
//! table = [double, square, inc, |x| x + 100]:
//! 1. table[0](7) = 14.   2. table[1](7) = 49.   3. table[2](7) = 8.   4. table[3](7) = 107.
//! 5. an index chosen at run time in a loop, folding v = table[i](v) from v = 1 for i = 0..4:
//!    1 -> 2 -> 4 -> 5 -> 105.
//! 6. a fn pointer passed to and returned from a function: pick(true) is square: 3 -> 9.
//! A wrong builder that calls the wrong entry exits 1 to 4.

fn double(x: u32) -> u32 {
    x * 2
}

fn square(x: u32) -> u32 {
    x * x
}

fn inc(x: u32) -> u32 {
    x + 1
}

fn pick(sq: bool) -> fn(u32) -> u32 {
    if sq { square } else { double }
}

fn main() -> std::entry::Status {
    let table: [fn(u32) -> u32; 4] = [double, square, inc, |x| x + 100];
    let mut v = 1;
    let mut i = 0;
    while i < table.len() {
        v = table[i](v);
        i += 1;
    }
    let status = if table[0](7) != 14 {
        1
    } else if table[1](7) != 49 {
        2
    } else if table[2](7) != 8 {
        3
    } else if table[3](7) != 107 {
        4
    } else if v != 105 {
        5
    } else if pick(true)(3) != 9 || pick(false)(3) != 6 {
        6
    } else {
        0
    };
    std::entry::Status(status)
}
