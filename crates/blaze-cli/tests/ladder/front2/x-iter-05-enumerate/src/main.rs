//! x-iter-05: `enumerate` counts from zero beside each element.
//!
//! core's docs: `enumerate` "Creates an iterator which gives the current iteration count as
//! well as the next value"; "The iterator returned yields pairs `(i, val)`, where `i` is the
//! current index of iteration and `val` is the value returned by the iterator."
//! v = [5, 7, 9]:
//! 1. sum of i * val = 0 * 5 + 1 * 7 + 2 * 9 = 25.
//! 2. the index of the first element > 6: 1 (by `find`), and of 9 by `position`: Some(2).
//! 3. the last pair seen in a `for` loop: (2, 9); the count of pairs 3.
//! 4. enumerate after skip(1): indices restart at 0: [(0, 7), (1, 9)]: 0 * 7 + 1 * 9 = 9.
//! A wrong builder that starts counting at 1 exits 1 (5 + 14 + 27 = 46).

fn main() -> std::entry::Status {
    let v = [5u32, 7, 9];
    let one: usize = v.iter().enumerate().map(|(i, x)| i * *x as usize).sum();
    let first_big = v.iter().enumerate().find(|(_, x)| **x > 6).map(|(i, _)| i);
    let mut last = (99usize, 0u32);
    let mut n = 0;
    for (i, x) in v.iter().enumerate() {
        last = (i, *x);
        n += 1;
    }
    let skipped: usize = v.iter().skip(1).enumerate().map(|(i, x)| i * *x as usize).sum();
    let status = if one != 25 {
        1
    } else if first_big != Some(1) || v.iter().position(|x| *x == 9) != Some(2) {
        2
    } else if last != (2, 9) || n != 3 {
        3
    } else if skipped != 9 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
