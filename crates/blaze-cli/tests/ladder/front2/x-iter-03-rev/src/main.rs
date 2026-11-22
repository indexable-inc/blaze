//! x-iter-03: `iter().rev()` walks a slice from the back; `rev` on a range too.
//!
//! core's docs: `rev` "Reverses an iterator's direction. Usually, iterators iterate from left
//! to right. After using `rev()`, an iterator will instead iterate from right to left."
//! 1. [1, 2, 3].iter().rev() folded as digits: 321.
//! 2. (1..5).rev() folded as digits: 4321 (5 excluded).
//! 3. the first element of a reversed slice: [7, 8, 9].iter().rev().next() = Some(&9).
//! 4. `next_back` on a forward iterator: [7, 8, 9].iter(): next_back 9, next 7, next_back 8,
//!    then None from both ends.
//! A wrong builder whose `rev` still runs forwards exits 1 (123).

fn main() -> std::entry::Status {
    let a = [1u32, 2, 3];
    let mut one = 0;
    for x in a.iter().rev() {
        one = one * 10 + x;
    }
    let mut two = 0;
    for x in (1..5u32).rev() {
        two = two * 10 + x;
    }
    let b = [7u32, 8, 9];
    let mut it = b.iter();
    let ends = (it.next_back(), it.next(), it.next_back(), it.next(), it.next_back());
    let status = if one != 321 {
        1
    } else if two != 4321 {
        2
    } else if b.iter().rev().next() != Some(&9) {
        3
    } else if ends != (Some(&9), Some(&7), Some(&8), None, None) {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
