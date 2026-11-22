//! What [`ContentId::of_each`] and [`ContentId::of_array`] cost beside [`ContentId::of`] one input
//! at a time, at the input lengths blaze hashes most (16, 64, 256 and 1024 bytes): nanoseconds
//! an input, best of a few rounds, and the ratio. Every ID is checked equal to `of`'s first.
//!
//! A measurement tool, not a test. Run:
//! `cargo run --release -p blaze-ids --example cost`

use std::{hint::black_box, time::Instant};

use blaze_ids::ContentId;

const DOMAIN: &str = "blaze v2 cost example 1";

/// Inputs a round hashes.
const COUNT: usize = 4096;

/// Rounds a measurement takes the best of.
const ROUNDS: usize = 9;

/// The best wall time of `ROUNDS` runs of `run`, in nanoseconds an input.
fn best(mut run: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let start = Instant::now();
        run();
        best = best.min(start.elapsed().as_nanos() as f64 / COUNT as f64);
    }
    best
}

fn main() {
    println!("{:>6} {:>12} {:>12} {:>8} {:>14} {:>8}", "bytes", "of ns", "of_each ns", "ratio", "of_array<3> ns", "ratio");
    for len in [16usize, 64, 256, 1024] {
        let bytes: Vec<u8> = (0..len * COUNT).map(|n| (n % 251) as u8).collect();
        let inputs: Vec<&[u8]> = bytes.chunks(len).collect();
        let alone: Vec<ContentId> = inputs.iter().map(|it| ContentId::of(DOMAIN, it)).collect();
        let mut out = vec![ContentId::default(); COUNT];
        ContentId::of_each(DOMAIN, &inputs, &mut out).expect("one slot an input");
        assert_eq!(out, alone, "of_each at {len} B");
        let three = ContentId::of_array(DOMAIN, [inputs[0], inputs[1], inputs[2]]);
        assert_eq!(three, [alone[0], alone[1], alone[2]], "of_array at {len} B");

        let of = best(|| {
            for (slot, input) in out.iter_mut().zip(&inputs) {
                *slot = ContentId::of(DOMAIN, black_box(input));
            }
            black_box(&out);
        });
        let each = best(|| {
            ContentId::of_each(DOMAIN, black_box(&inputs), &mut out).expect("one slot an input");
            black_box(&out);
        });
        let array = best(|| {
            for (slots, group) in out.chunks_exact_mut(3).zip(inputs.chunks_exact(3)) {
                slots.copy_from_slice(&ContentId::of_array(DOMAIN, [group[0], group[1], group[2]]));
            }
            black_box(&out);
        });
        println!("{len:>6} {of:>12.1} {each:>12.1} {:>8.2} {array:>14.1} {:>8.2}", of / each, of / array);
    }
}
