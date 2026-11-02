// The hot loop both bins run, `include!`d into each: a library unit never gets `HeapToStack`
// (one build of it serves programs with different allocators), so the step lives in the bin.
use std::hint::black_box;
use std::rc::Rc;

/// Three allocations that never leave the call: a small `vec!` summed, a `Box` read through,
/// and an `Rc` that is never cloned. Every call it makes is std's with std's types, so it never
/// runs the program's code: what `HeapToStack` asks of a body it moves allocations in.
#[inline(never)]
fn step(i: u64) -> u64 {
    let v = vec![i, i ^ 1, i ^ 2, i ^ 3, i ^ 4, i ^ 5, i ^ 6, i ^ 7];
    let b = Box::new([black_box(i); 4]);
    let r = Rc::new((i, black_box(3u64)));
    let mut sum = 0u64;
    for x in &v {
        sum = sum.wrapping_add(*x);
    }
    sum = sum.wrapping_add(b[0]).wrapping_add(b[3]);
    sum.wrapping_add(r.0).wrapping_mul(r.1)
}

/// `step` `n` times, `n` from the first argument (10 million by default).
fn run() -> (u64, u64, std::time::Duration) {
    let n = std::env::args().nth(1).and_then(|it| it.parse().ok()).unwrap_or(10_000_000u64);
    let start = std::time::Instant::now();
    let mut acc = 0u64;
    for i in 0..n {
        acc = acc.wrapping_add(step(black_box(i)));
    }
    (n, black_box(acc), start.elapsed())
}
