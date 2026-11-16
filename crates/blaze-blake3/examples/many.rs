//! Many short inputs, one at a time against side by side: 200,000 inputs of 50 to 400 bytes
//! (the outline's item headers and short bodies), hashed by `derive_key_with` each and by
//! `derive_key_many` in batches of 64 (the outline's mode and batch), best of five runs, hot in
//! cache. Prints two lines, nanoseconds an input and bytes a second each way. Run it with
//! `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/mixcheck.sh blaze-blake3 run many`.
//!
//! What to expect, and why: one input's compressions are a chain, each waiting on the last, so
//! one at a time costs the latency of a block (46 ns on the owner's M5 Max, the crate docs'
//! table) per block, about 3.5 blocks an input here: about 160 ns. Side by side, 8 lanes (two
//! NEON registers a word on aarch64, one AVX2 register on x86-64) cost about 16 ns a block a
//! lane at the floor and 24 measured (41.6 M inputs/s of 64 B): about 85 to 90 ns an input, so
//! about 2 times faster, not 8: the scalar chain already keeps the core's several vector units
//! half busy, and what lanes save is the waiting, not the operations. The outline measured
//! the same ratio on library/'s entries (blaze-file-items digest.rs: 88 ns side by side in
//! batches of 64, 168 ns one shot). Inputs of equal block counts gain a little more than mixed
//! ones only at a batch's end, where lanes go idle: a lane takes the next input the step after
//! its last block, so mixed lengths cost little.

use std::{hint::black_box, time::Instant};

const INPUTS: usize = 200_000;
const BATCH: usize = 64;

/// The inputs: lengths 50 to 400 from a fixed linear congruential sequence, bytes from it too,
/// in one buffer; the same on every run and machine.
fn tape() -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as usize
    };
    let mut bytes = Vec::new();
    let mut spans = Vec::with_capacity(INPUTS);
    for _ in 0..INPUTS {
        let len = 50 + next() % 351;
        let at = bytes.len();
        bytes.extend((0..len).map(|_| next() as u8));
        spans.push((at, len));
    }
    (bytes, spans)
}

/// Best of five runs of `f`, in seconds.
fn best(mut f: impl FnMut()) -> f64 {
    f();
    (0..5)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min)
}

fn line(name: &str, seconds: f64, total: usize) -> String {
    format!(
        "{name}: {:.1} ns an input, {:.0} MB/s ({INPUTS} inputs of 50 to 400 B, {total} B)",
        seconds * 1e9 / INPUTS as f64,
        total as f64 / seconds / 1e6
    )
}

fn main() {
    let (bytes, spans) = tape();
    let inputs: Vec<&[u8]> = spans.iter().map(|&(at, len)| &bytes[at..at + len]).collect();
    let key = blaze_blake3::context_key("blaze-blake3 example many");
    let mut out = vec![[0u8; 32]; INPUTS];

    let one = best(|| {
        for (input, out) in inputs.iter().zip(out.iter_mut()) {
            *out = blaze_blake3::derive_key_with(&key, black_box(input));
        }
        black_box(&out);
    });
    let want = out.clone();
    let many = best(|| {
        for (inputs, out) in inputs.chunks(BATCH).zip(out.chunks_mut(BATCH)) {
            blaze_blake3::derive_key_many(&key, black_box(inputs), out);
        }
        black_box(&out);
    });
    assert_eq!(out, want, "side by side gave other digests than one at a time");

    println!("{}", line("one at a time (derive_key_with)", one, bytes.len()));
    println!("{} = {:.2}x", line("side by side (derive_key_many, 64 a call)", many, bytes.len()), one / many);
}
