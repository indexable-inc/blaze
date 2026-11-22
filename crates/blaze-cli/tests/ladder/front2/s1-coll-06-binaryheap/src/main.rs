//! s1-coll-06: `BinaryHeap` (a max-heap: "pop: Removes the greatest item"), `Reverse` for a
//! min-heap, `into_sorted_vec` ("in ascending order"), and tuples ordered lexicographically
//! (the derived `Ord` of a tuple compares field by field: core/src/tuple.rs).

use std::cmp::Reverse;
use std::collections::BinaryHeap;

fn main() {
    let mut h = BinaryHeap::from(vec![4, 1, 7, 3, 9]);
    println!("{:?}", h.peek());
    h.push(8);
    let mut out = Vec::new();
    while let Some(x) = h.pop() {
        out.push(x);
    }
    println!("{:?}", out);
    let mut mh = BinaryHeap::new();
    for x in [5, 2, 8] {
        mh.push(Reverse(x));
    }
    println!("{:?}", mh.pop().map(|Reverse(x)| x));
    let sorted = BinaryHeap::from(vec![3, 1, 2]).into_sorted_vec();
    println!("{:?}", sorted);
    let mut tasks = BinaryHeap::new();
    tasks.push((2, "write"));
    tasks.push((5, "fix"));
    tasks.push((2, "read"));
    while let Some((p, t)) = tasks.pop() {
        println!("{p} {t}");
    }
}
