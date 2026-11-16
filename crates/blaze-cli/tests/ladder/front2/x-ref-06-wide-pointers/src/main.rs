//! x-ref-06: wide pointers (`&[T]`, `&str`) passed to and returned from functions keep both
//! words: the data address and the length.
//!
//! The Reference, "Dynamically Sized Types": "Pointer types to DSTs are sized but have twice
//! the size of pointers to sized types": "Pointers to slices also store the number of elements
//! of the slice." A returned slice is the same address and length that went in.
//! 1. longer(&[1, 2, 3], &[4, 5]) returns the first: len 3, first element 1.
//! 2. longer(&[1], &[7, 8, 9, 10]) returns the second: len 4, last element 10.
//! 3. the returned slice is the argument itself: `core::ptr::eq` of the two is true.
//! 4. a `&str` through a function: `pick("hello", "hi", true)` has len 5; with false, 2.
//! 5. a pair of slices in a tuple returned: (&a[..1], &a[1..]) has lens 1 and 2.
//! A wrong builder that drops or swaps the length word exits 1 (or 2).

fn longer<'a>(a: &'a [u32], b: &'a [u32]) -> &'a [u32] {
    if a.len() >= b.len() { a } else { b }
}

fn pick<'a>(a: &'a str, b: &'a str, first: bool) -> &'a str {
    if first { a } else { b }
}

fn halves(a: &[u8]) -> (&[u8], &[u8]) {
    (&a[..1], &a[1..])
}

fn main() -> std::entry::Status {
    let x = [1u32, 2, 3];
    let y = [7u32, 8, 9, 10];
    let one = longer(&x, &y[..2]);
    let two = longer(&x[..1], &y);
    let (h, t) = halves(&[5, 6, 7]);
    let status = if one.len() != 3 || one[0] != 1 {
        1
    } else if two.len() != 4 || two[3] != 10 {
        2
    } else if !core::ptr::eq(two, &y[..]) {
        3
    } else if pick("hello", "hi", true).len() != 5 || pick("hello", "hi", false).len() != 2 {
        4
    } else if h.len() != 1 || t.len() != 2 || t[1] != 7 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
