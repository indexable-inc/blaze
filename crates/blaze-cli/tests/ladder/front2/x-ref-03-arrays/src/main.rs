//! x-ref-03: arrays: length, indexing, iteration by index, and copy semantics.
//!
//! The Reference, "Array and slice indexing expressions": "Indices are zero-based"; "Array
//! types": `[T; N]` has N elements; an array of a `Copy` type is `Copy`, so `let b = a` copies.
//! 1. a = [3, 1, 4, 1, 5]: len 5, a[0] = 3, a[4] = 5.
//! 2. the sum by index 0..len: 3 + 1 + 4 + 1 + 5 = 14.
//! 3. `let mut b = a; b[0] = 9;`: a[0] stays 3, b[0] = 9.
//! 4. `[7u16; 4]` (the repeat form): four sevens, sum 28.
//! 5. writing through an index in a loop: c[i] = i * i for i in 0..4: c = [0, 1, 4, 9], c[3] = 9.
//! A wrong builder that scales the index by the wrong element size exits 1 or 4.

fn sum(a: &[u32; 5]) -> u32 {
    let mut i = 0;
    let mut s = 0;
    while i < a.len() {
        s += a[i];
        i += 1;
    }
    s
}

fn main() -> std::entry::Status {
    let a = [3u32, 1, 4, 1, 5];
    let mut b = a;
    b[0] = 9;
    let r = [7u16; 4];
    let mut c = [0usize; 4];
    let mut i = 0;
    while i < 4 {
        c[i] = i * i;
        i += 1;
    }
    let status = if a.len() != 5 || a[0] != 3 || a[4] != 5 {
        1
    } else if sum(&a) != 14 {
        2
    } else if a[0] != 3 || b[0] != 9 {
        3
    } else if r[0] + r[1] + r[2] + r[3] != 28 {
        4
    } else if c[3] != 9 || c[2] != 4 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
