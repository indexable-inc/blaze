//! x-match-07: slice patterns with `..` and a rest binding, on arrays and on slices.
//!
//! The Reference, "Slice patterns": "Slice patterns can match both arrays of fixed size and
//! slices of dynamic size"; "Rest patterns": `..` "matches zero or more elements"; `rest @ ..`
//! binds the elements it matched as a slice (or array).
//! 1. [1, 2, 3, 4, 5] against `[first, .., last]`: 1 * 10 + 5 = 15.
//! 2. against `[_, rest @ ..]`: rest has 4 elements, rest[0] = 2.
//! 3. `shape` picks its arm by length: [] -> 0, [9] -> 1.
//! 4. [1, 2, 3] reaches `[x, .., y]`: 1000 + 1 + 3 = 1004; [7, 8] meets `[x, y]` first (it
//!    would match `[x, .., y]` too): 100 + 7 * 8 = 156.
//! 5. the array as a slice, [1, 2, 3, 4, 5]: 1000 + 1 + 5 = 1006.
//! A wrong builder that reads `last` from a fixed offset rather than the length exits 1 or 4.

fn shape(s: &[u32]) -> u32 {
    match s {
        [] => 0,
        [_] => 1,
        [x, y] => 100 + x * y,
        [x, .., y] => 1000 + x + y,
    }
}

fn main() -> std::entry::Status {
    let a = [1u32, 2, 3, 4, 5];
    let [first, .., last] = a;
    let [_, rest @ ..] = &a;
    let status = if first * 10 + last != 15 {
        1
    } else if rest.len() != 4 || rest[0] != 2 {
        2
    } else if shape(&[]) != 0 || shape(&[9]) != 1 {
        3
    } else if shape(&[1, 2, 3]) != 1004 || shape(&[7, 8]) != 156 {
        4
    } else if shape(&a[..]) != 1006 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
