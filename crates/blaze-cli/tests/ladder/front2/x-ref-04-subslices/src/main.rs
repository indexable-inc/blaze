//! x-ref-04: sub-slices by range: `&a[1..3]`, `&a[..2]`, `&a[3..]`, and `split_at`.
//!
//! core's `Index<Range<usize>>` for slices: `a[i..j]` is the elements at i, i + 1, .., j - 1
//! (the Reference, "Range expressions": `start..end` is "start <= x < end"); `..end` starts at
//! 0 and `start..` ends at the length. core's `split_at(mid)`: "The first will contain all
//! indices from `[0, mid)` ... and the second will contain all indices from `[mid, len)`".
//! a = [10, 20, 30, 40, 50]:
//! 1. &a[1..3] = [20, 30]: len 2, s[0] = 20, s[1] = 30.
//! 2. &a[..2] = [10, 20]: len 2, last 20.     3. &a[3..] = [40, 50]: len 2, s[0] = 40.
//! 4. a slice of a slice: (&a[1..])[1..3] = [30, 40].
//! 5. split_at(2): left len 2 (ends 20), right len 3 (starts 30).
//! A wrong builder that takes `j` as a length (a[1..1+3]) exits 1 (len 3).

fn main() -> std::entry::Status {
    let a = [10u32, 20, 30, 40, 50];
    let s = &a[1..3];
    let head = &a[..2];
    let tail = &a[3..];
    let inner = &(&a[1..])[1..3];
    let (left, right) = a.split_at(2);
    let status = if s.len() != 2 || s[0] != 20 || s[1] != 30 {
        1
    } else if head.len() != 2 || head[1] != 20 {
        2
    } else if tail.len() != 2 || tail[0] != 40 {
        3
    } else if inner[0] != 30 || inner[1] != 40 {
        4
    } else if left.len() != 2 || left[1] != 20 || right.len() != 3 || right[0] != 30 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
