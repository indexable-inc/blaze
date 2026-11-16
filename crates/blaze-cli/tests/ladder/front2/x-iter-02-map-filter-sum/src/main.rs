//! x-iter-02: `map`, `filter` and `sum` from core over a range and over a slice.
//!
//! core's docs: `filter` "creates an iterator which uses a closure to determine if an element
//! should be yielded"; `map` "takes a closure and creates an iterator which calls that closure
//! on each element"; `sum` "Sums the elements of an iterator". The Reference, "Range
//! expressions": `1..=10` is 1 <= x <= 10.
//! 1. (1..=10).filter(even).map(square).sum() = 4 + 16 + 36 + 64 + 100 = 220.
//! 2. (0..5).map(|x| x * 3).sum() = 0 + 3 + 6 + 9 + 12 = 30 (the end 5 excluded).
//! 3. over a slice [3, 8, 1, 9, 4]: filter(> 3).sum() = 8 + 9 + 4 = 21.
//! 4. map then filter on the slice: x * 2 > 10: 16, 18: their sum 34.
//! A wrong builder that runs the range one past its end exits 2 (45).

fn main() -> std::entry::Status {
    let s = [3u32, 8, 1, 9, 4];
    let one: u32 = (1..=10u32).filter(|n| n % 2 == 0).map(|n| n * n).sum();
    let two: u32 = (0..5u32).map(|x| x * 3).sum();
    let three: u32 = s.iter().filter(|x| **x > 3).sum();
    let four: u32 = s.iter().map(|x| x * 2).filter(|x| *x > 10).sum();
    let status = if one != 220 {
        1
    } else if two != 30 {
        2
    } else if three != 21 {
        3
    } else if four != 34 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
