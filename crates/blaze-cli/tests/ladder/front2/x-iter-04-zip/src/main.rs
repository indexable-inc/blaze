//! x-iter-04: `zip` pairs two iterators and stops at the shorter.
//!
//! core's docs: `zip` "'Zips up' two iterators into a single iterator of pairs"; "If either
//! iterator returns `None`, `next` from the zipped iterator will return `None`."
//! 1. a dot product [1, 2, 3] . [4, 5, 6] = 4 + 10 + 18 = 32.
//! 2. uneven lengths [1, 2, 3, 4] with [10, 20]: 2 pairs; the sum of products 10 + 40 = 50.
//! 3. pairs in order: zip([1, 2], [3, 4]) folded as (a * 10 + b) digits: 13 then 24: 1324.
//! 4. a range zipped with a slice: (100..).zip([7, 8]) gives (100, 7), (101, 8): 100 * 7 +
//!    101 * 8 = 700 + 808 = 1508.
//! A wrong builder that pairs each element with itself exits 1 (14).

fn main() -> std::entry::Status {
    let (a, b) = ([1u32, 2, 3], [4u32, 5, 6]);
    let dot: u32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let long = [1u32, 2, 3, 4];
    let short = [10u32, 20];
    let pairs = long.iter().zip(short.iter()).count();
    let uneven: u32 = long.iter().zip(short.iter()).map(|(x, y)| x * y).sum();
    let mut order = 0;
    for (x, y) in [1u32, 2].iter().zip([3u32, 4].iter()) {
        order = order * 100 + x * 10 + y;
    }
    let ranged: u32 = (100u32..).zip([7u32, 8]).map(|(i, v)| i * v).sum();
    let status = if dot != 32 {
        1
    } else if pairs != 2 || uneven != 50 {
        2
    } else if order != 1324 {
        3
    } else if ranged != 1508 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
