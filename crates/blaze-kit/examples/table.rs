//! The cost of a read by id: `Table::at` (checked), a branded read (checked once, when the id is
//! proven), and a raw slice index, in a tight loop over random ids, at three sizes.
//!
//!     cargo run --release -p blaze-kit --example table

#![expect(clippy::print_stdout, reason = "an example prints its measurements")]

#[expect(dead_code, reason = "the examples share the helper, each using part of it")]
mod bench;

use std::alloc::System;

use blaze_int::Int;
use blaze_kit::{Id, Table, id};

id! {
    /// A row of the table timed.
    struct Row;
}

fn main() {
    println!("ns per read of a random id: at, branded, slice[usize]");
    for size in [1_000usize, 100_000, 10_000_000] {
        let mut table: Table<Row, u64, _> = Table::with_capacity_in(size, System);
        let mut mix = bench::Mix(7);
        for _ in 0..size {
            table.push(mix.next());
        }
        let n = size.truncate_to::<u32>();
        let ids: Vec<Row> = (0..1_000_000).map(|_| Row::from_u32(mix.below(u64::from(n)).truncate_to::<u32>())).collect();
        let ops = 1_000_000u64;
        let at = bench::ns_per_op(ops, || ids.iter().map(|&id| *table.at(id)).fold(0, u64::wrapping_add));
        // The ids are proven once, outside the loop timed, as a hot loop over proven ids would.
        let branded = table.checked(|t| {
            let proven: Vec<_> = ids.iter().filter_map(|&id| t.check(id)).collect();
            bench::ns_per_op(ops, || proven.iter().map(|&ix| *t.get(ix)).fold(0, u64::wrapping_add))
        });
        let slice = table.as_slice();
        let raw: Vec<usize> = ids.iter().map(|id| id.index()).collect();
        let indexed = bench::ns_per_op(ops, || raw.iter().map(|&i| slice[i]).fold(0, u64::wrapping_add));
        bench::row(size, &[("at", at), ("branded", branded), ("slice", indexed)]);
    }
}
