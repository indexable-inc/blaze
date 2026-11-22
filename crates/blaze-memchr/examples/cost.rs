//! The cost of a one-needle search (std.md, "SIMD", rule 5): bytes per nanosecond at haystacks
//! of 16, 256 and 65,536 bytes, the needle absent until the last byte (a whole scan), for the
//! portable vector at 16 and 32 lanes, the word (SWAR) and a naive loop (the NEON intrinsics it
//! was measured against are deleted; their numbers are in the crate's doc). Each is timed over about 256 MiB of haystack, best of five rounds.
//!
//! A measurement tool, not a test: `cargo run --release -p blaze-memchr --example cost`.

#![feature(allocator_api)]

use std::{alloc::Global, hint::black_box, time::Instant, vec::Vec};

use blaze_memchr::paths;

/// About this many haystack bytes per round.
const ROUND_BYTES: usize = 256 << 20;

#[inline(never)]
fn naive(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&byte| byte == needle)
}

/// Bytes per nanosecond, in hundredths, of the best of five rounds of `search`.
fn rate(search: fn(u8, &[u8]) -> Option<usize>, haystack: &[u8]) -> u128 {
    let reps = (ROUND_BYTES / haystack.len()).max(1);
    let want = Some(haystack.len() - 1);
    let mut best = u128::MAX;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..reps {
            let found = search(black_box(b'z'), black_box(haystack));
            assert_eq!(found, want, "a search that missed the needle");
        }
        best = best.min(start.elapsed().as_nanos().max(1));
    }
    let bytes = u128::try_from(reps * haystack.len()).expect("a round's bytes fit in u128");
    bytes * 100 / best
}

fn main() {
    let mut searches: Vec<(&str, fn(u8, &[u8]) -> Option<usize>), Global> = Vec::new_in(Global);
    searches.push(("portable 16", paths::portable_16));
    searches.push(("portable 32", paths::portable_32));
    searches.push(("word (swar)", paths::word));
    searches.push(("naive loop", naive));
    println!("{:<16} {:>12} {:>12} {:>12}   (bytes per ns, needle at the last byte)", "search", "16 B", "256 B", "64 KiB");
    let mut haystacks: Vec<Vec<u8, Global>, Global> = Vec::new_in(Global);
    for size in [16usize, 256, 65_536] {
        let mut haystack = Vec::with_capacity_in(size, Global);
        haystack.resize(size - 1, b'a');
        haystack.push(b'z');
        haystacks.push(haystack);
    }
    for (name, search) in &searches {
        let mut row = std::string::String::new();
        for haystack in &haystacks {
            let hundredths = rate(*search, haystack);
            row.push_str(&format!(" {:>9}.{:02}", hundredths / 100, hundredths % 100));
        }
        println!("{name:<16}{row}");
    }
}
