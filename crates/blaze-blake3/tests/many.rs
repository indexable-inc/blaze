//! The many-inputs entries against the one-at-a-time ones: [`blaze_blake3::many_checked`] and
//! the batcher [`blaze_blake3::Many`] give each input exactly the digest its mode's one-shot
//! entry gives it, in the caller's order and slots, at every length boundary (empty, one byte,
//! 63, 64, 65 bytes, 1,023, 1,024, 1,025, several chunks), in batches of all-equal lengths and
//! of all-different ones, whatever the batcher's capacity (so whatever its flush points). The
//! lane path these run is the crate's one dispatch (on aarch64 the `core::simd` body compiled
//! to NEON; under Miri the same body interpreted, the portable path).
//!
//! Expected, from `lanecheck.sh blaze-blake3 test '"--test many"'`:
//! `test many_is_one_shot_at_every_boundary ... ok`,
//! `test batcher_is_one_shot_whatever_the_flush_points ... ok`,
//! `test a_count_mismatch_is_a_typed_error ... ok`.

use blaze_blake3::{CountMismatch, Many, Mode, many_checked};
use blaze_sim::{Config, Tape};

/// The lengths each boundary of a block, a chunk and a few chunks sits at.
const BOUNDARIES: &[usize] = &[0, 1, 63, 64, 65, 127, 128, 129, 1023, 1024, 1025, 2048, 2049, 3073];

/// One shot in `mode`: what every many-inputs path must equal.
fn one_shot(mode: Mode, input: &[u8]) -> [u8; 32] {
    match mode {
        Mode::Hash => blaze_blake3::hash(input),
        Mode::Keyed(key) => blaze_blake3::keyed_hash(&key, input),
        Mode::DeriveKey(key) => blaze_blake3::derive_key_with(&key, input),
    }
}

/// A length the tape draws: a boundary, a boundary give or take a few, or anything to three
/// chunks (to 300 bytes under Miri, which interprets every round).
fn length(tape: &Tape) -> usize {
    match tape.below(3, "length kind") {
        0 => BOUNDARIES[tape.below(BOUNDARIES.len() as u64, "boundary") as usize],
        1 => {
            let at = BOUNDARIES[tape.below(BOUNDARIES.len() as u64, "near boundary") as usize];
            (at + tape.range(0..5, "past it") as usize).saturating_sub(2)
        }
        _ => tape.range(0..if cfg!(miri) { 301 } else { 3 * 1024 + 1 }, "any") as usize,
    }
}

/// A mode the tape draws, its key random.
fn mode(tape: &Tape) -> Mode {
    let mut key = [0; 32];
    tape.fill(&mut key, "key");
    match tape.below(3, "mode") {
        0 => Mode::Hash,
        1 => Mode::Keyed(key),
        _ => Mode::DeriveKey(key),
    }
}

/// A batch the tape draws: mixed lengths, all one length, or all different lengths.
fn inputs(tape: &Tape) -> Vec<Vec<u8>> {
    let count = tape.range(0..if cfg!(miri) { 12 } else { 70 }, "inputs") as usize;
    let lens: Vec<usize> = match tape.below(3, "batch shape") {
        0 => (0..count).map(|_| length(tape)).collect(),
        1 => vec![length(tape); count],
        _ => {
            // All different: a strictly rising run from a drawn start, by drawn steps.
            let mut at = length(tape);
            (0..count)
                .map(|_| {
                    let len = at;
                    at += 1 + tape.range(0..if cfg!(miri) { 20 } else { 90 }, "step") as usize;
                    len
                })
                .collect()
        }
    };
    lens.into_iter()
        .map(|len| {
            let mut data = vec![0; len];
            tape.fill(&mut data, "input");
            data
        })
        .collect()
}

fn checked_agrees(tape: &Tape) {
    let mode = mode(tape);
    let datas = inputs(tape);
    let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
    let mut out = vec![[0; 32]; inputs.len()];
    many_checked(mode, &inputs, &mut out).expect("one slot an input");
    for (at, (input, got)) in inputs.iter().zip(&out).enumerate() {
        assert_eq!(*got, one_shot(mode, input), "{mode:?}, input {at} of {} B", input.len());
    }
}

#[test]
fn many_is_one_shot_at_every_boundary() {
    // Every boundary in one call first, in order and reversed, then the search.
    let datas: Vec<Vec<u8>> =
        BOUNDARIES.iter().map(|&len| (0..len).map(|at| (at % 251) as u8).collect()).collect();
    for forward in [true, false] {
        let mut inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
        if !forward {
            inputs.reverse();
        }
        let mut out = vec![[0; 32]; inputs.len()];
        for mode in [Mode::Hash, Mode::Keyed([7; 32]), Mode::DeriveKey([9; 32])] {
            many_checked(mode, &inputs, &mut out).expect("one slot an input");
            for (input, got) in inputs.iter().zip(&out) {
                assert_eq!(*got, one_shot(mode, input), "{mode:?}, {} B", input.len());
            }
        }
    }
    blaze_sim::check(Config::cases(if cfg!(miri) { 4 } else { Config::DEFAULT.cases }), checked_agrees);
}

/// The batcher at capacity `N` over the tape's batch: every slot its input's one-shot digest.
fn batcher_at<const N: usize>(mode: Mode, inputs: &[&[u8]]) {
    let mut out = vec![[0xAA; 32]; inputs.len()];
    {
        let mut many = Many::<N>::new(mode);
        for (input, slot) in inputs.iter().zip(out.iter_mut()) {
            many.push(*input, slot);
            assert!(many.pending() < N, "a full batch is hashed at once");
        }
        many.finish();
    }
    for (at, (input, got)) in inputs.iter().zip(&out).enumerate() {
        assert_eq!(*got, one_shot(mode, input), "{mode:?}, at {N}, input {at} of {} B", input.len());
    }
}

fn batcher_agrees(tape: &Tape) {
    let mode = mode(tape);
    let datas = inputs(tape);
    let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
    batcher_at::<1>(mode, &inputs);
    batcher_at::<3>(mode, &inputs);
    batcher_at::<8>(mode, &inputs);
    batcher_at::<17>(mode, &inputs);
    batcher_at::<64>(mode, &inputs);
    // Dropped without `finish`: every slot written still.
    let mut out = vec![[0; 32]; inputs.len()];
    {
        let mut many = Many::<5>::new(mode);
        for (input, slot) in inputs.iter().zip(out.iter_mut()) {
            many.push(*input, slot);
        }
    }
    for (input, got) in inputs.iter().zip(&out) {
        assert_eq!(*got, one_shot(mode, input), "{mode:?}, dropped, {} B", input.len());
    }
}

#[test]
fn batcher_is_one_shot_whatever_the_flush_points() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 4 } else { Config::DEFAULT.cases }), batcher_agrees);
}

#[test]
fn a_count_mismatch_is_a_typed_error() {
    let inputs: [&[u8]; 2] = [b"a", b"b"];
    let mut out = [[0x55; 32]; 3];
    assert_eq!(
        many_checked(Mode::Hash, &inputs, &mut out),
        Err(CountMismatch { inputs: 2, digests: 3 })
    );
    assert_eq!(out, [[0x55; 32]; 3], "nothing written on a refusal");
    let message = many_checked(Mode::Hash, &inputs, &mut out[..1]).expect_err("1 slot, 2 inputs").to_string();
    assert!(message.contains("2 inputs but 1 digest slots"), "{message}");
}
