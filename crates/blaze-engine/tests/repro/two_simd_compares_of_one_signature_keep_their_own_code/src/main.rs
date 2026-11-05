#![feature(portable_simd)]

use std::hint::black_box;
use std::simd::{cmp::SimdPartialOrd, i8x8};

fn main() {
    let v = black_box(i8x8::from_array([-1, 0, -2, 1, -128, 127, 0, 0]));
    let zero = i8x8::splat(0);
    let lt = v.simd_lt(zero).to_bitmask();
    let ge = v.simd_ge(zero).to_bitmask();
    let set: std::collections::HashSet<u64> = (0..3).collect();
    println!("{lt:08b} {ge:08b} {}", set.len());
}
