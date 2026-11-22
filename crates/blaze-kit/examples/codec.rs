//! At three sizes: the delta column (encode and decode) against plain 8-byte little-endian
//! values, with the bytes each takes; and `Dict` against `blaze_hash::HashMap` (FixedState)
//! and `std::collections::HashMap` (SipHash) from value to code.
//!
//!     cargo run --release -p blaze-kit --example codec

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[expect(dead_code, reason = "the examples share the helper, each using part of it")]
mod bench;

use std::{alloc::System, collections::HashMap};

use blaze_kit::{
    Dict,
    codec::{delta_decode, delta_encode},
};

fn main() {
    for size in [1_000usize, 100_000, 10_000_000] {
        let mut mix = bench::Mix(9);
        // Sorted positions with small random gaps: what a delta column is for.
        let mut at = 0i64;
        let values: Vec<i64> = (0..size)
            .map(|_| {
                at += 1 + i64::try_from(mix.below(300)).unwrap_or(0);
                at
            })
            .collect();
        let ops = u64::try_from(size).unwrap_or(1);
        let mut bytes = blaze_own::Vec::new_in(System);
        let encode = bench::ns_per_op(ops, || {
            bytes.clear();
            delta_encode(&values, &mut bytes).unwrap_or_else(|e| panic!("{e}"));
            u64::try_from(bytes.len()).unwrap_or(0)
        });
        let mut back = vec![0i64; size];
        let decode = bench::ns_per_op(ops, || u64::try_from(delta_decode(&bytes, &mut back).unwrap_or_else(|e| panic!("{e}"))).unwrap_or(0));
        let mut plain = Vec::with_capacity(size * 8);
        let plain_encode = bench::ns_per_op(ops, || {
            plain.clear();
            values.iter().for_each(|v| plain.extend_from_slice(&v.to_le_bytes()));
            u64::try_from(plain.len()).unwrap_or(0)
        });
        println!("{size} sorted values: ns per value; {} bytes delta, {} plain", bytes.len(), size * 8);
        bench::row(size, &[("delta encode", encode), ("delta decode", decode), ("plain encode", plain_encode)]);

        let distinct = (size / 16).max(1);
        let rows: Vec<u64> = (0..size).map(|_| mix.below(u64::try_from(distinct).unwrap_or(1)) * 0x9E37_79B9).collect();
        let dict = bench::ns_per_op(ops, || {
            let mut dict: Dict<u32, u64, _> = Dict::new_in(System);
            rows.iter().for_each(|&v| {
                dict.push(v);
            });
            u64::try_from(dict.distinct()).unwrap_or(0)
        });
        let std = bench::ns_per_op(ops, || {
            let mut map: HashMap<u64, u32> = HashMap::new();
            let mut codes: Vec<u32> = Vec::with_capacity(0);
            let mut values: Vec<u64> = Vec::new();
            for &v in &rows {
                let next = u32::try_from(values.len()).unwrap_or(u32::MAX);
                let code = *map.entry(v).or_insert_with(|| {
                    values.push(v);
                    next
                });
                codes.push(code);
            }
            u64::try_from(values.len()).unwrap_or(0)
        });
        let fixed = bench::ns_per_op(ops, || {
            let mut map: blaze_hash::HashMap<u64, u32, System> = blaze_hash::HashMap::new_in(System);
            let mut codes: Vec<u32> = Vec::with_capacity(0);
            let mut values: Vec<u64> = Vec::new();
            for &v in &rows {
                let next = u32::try_from(values.len()).unwrap_or(u32::MAX);
                let code = *map.entry(v).or_insert_with(|| {
                    values.push(v);
                    next
                });
                codes.push(code);
            }
            u64::try_from(values.len()).unwrap_or(0)
        });
        println!("{size} rows of {distinct} values: ns per row coded");
        bench::row(size, &[("Dict", dict), ("blaze_hash::HashMap", fixed), ("std HashMap (SipHash)", std)]);
    }
}
