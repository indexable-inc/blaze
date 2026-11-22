//! The cost of making, filling and dropping many short vectors: `InlineVec<u32, 4>` against
//! `Vec<u32>`, at three lengths (inline, just spilled, long).
//!
//!     cargo run --release -p blaze-kit --example inline_vec

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[expect(dead_code, reason = "the examples share the helper, each using part of it")]
mod bench;

use std::alloc::System;

use blaze_kit::InlineVec;

fn main() {
    println!("ns per vector made, filled to the length, summed and dropped");
    let count = 200_000u64;
    for len in [3u32, 8, 64] {
        let inline = bench::ns_per_op(count, || {
            let mut total = 0u64;
            for _ in 0..count {
                let mut v: InlineVec<u32, 4, _> = InlineVec::new_in(System);
                for x in 0..len {
                    v.push(std::hint::black_box(x));
                }
                total = total.wrapping_add(v.iter().map(|&x| u64::from(x)).sum::<u64>());
            }
            total
        });
        let std = bench::ns_per_op(count, || {
            let mut total = 0u64;
            for _ in 0..count {
                let mut v: Vec<u32> = Vec::new();
                for x in 0..len {
                    v.push(std::hint::black_box(x));
                }
                total = total.wrapping_add(v.iter().map(|&x| u64::from(x)).sum::<u64>());
            }
            total
        });
        bench::row(blaze_int::idx(len), &[("InlineVec<_, 4>", inline), ("Vec", std)]);
    }
}
