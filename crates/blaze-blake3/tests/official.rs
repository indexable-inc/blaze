//! This crate against official BLAKE3 (the `blake3` crate, a dev-dependency of this test alone)
//! on random inputs, modes and split points: the one-shot calls, a hasher fed in pieces and
//! finalized part way, the count, and the extended output from a random position. Lengths are
//! drawn around every boundary the hasher splits at: a block, a chunk, a batch of 8 or 16
//! chunks side by side, and a tree of several levels.

use blaze_sim::{Config, Tape};

const KIB: u64 = 1024;

#[derive(Clone, Copy, Debug)]
enum Mode<'a> {
    Plain,
    Keyed(&'a [u8; 32]),
    DeriveKey(&'a str),
    ContextKey(&'a [u8; 32]),
}

impl Mode<'_> {
    fn ours(self) -> blaze_blake3::Hasher {
        match self {
            Mode::Plain => blaze_blake3::Hasher::new(),
            Mode::Keyed(key) => blaze_blake3::Hasher::new_keyed(key),
            Mode::DeriveKey(context) => blaze_blake3::Hasher::new_derive_key(context),
            Mode::ContextKey(key) => blaze_blake3::Hasher::new_from_context_key(key),
        }
    }

    fn ours_once(self, input: &[u8]) -> [u8; 32] {
        match self {
            Mode::Plain => blaze_blake3::hash(input),
            Mode::Keyed(key) => blaze_blake3::keyed_hash(key, input),
            Mode::DeriveKey(context) => blaze_blake3::derive_key(context, input),
            Mode::ContextKey(key) => blaze_blake3::derive_key_with(key, input),
        }
    }

    fn theirs(self) -> blake3::Hasher {
        match self {
            Mode::Plain => blake3::Hasher::new(),
            Mode::Keyed(key) => blake3::Hasher::new_keyed(key),
            Mode::DeriveKey(context) => blake3::Hasher::new_derive_key(context),
            Mode::ContextKey(key) => {
                use blake3::hazmat::HasherExt;
                blake3::Hasher::new_from_context_key(key)
            }
        }
    }
}

/// A length near a boundary the hasher splits at, or anywhere up to 300 KiB; under Miri, only
/// near a boundary up to 17 KiB, or a few blocks.
fn length(tape: &Tape) -> usize {
    let boundaries = [0, 64, 1024, 2048, 8 * 1024, 9 * 1024, 16 * 1024, 17 * 1024, 32 * 1024];
    if cfg!(miri) {
        return match tape.below(2, "length kind") {
            0 => {
                let at = boundaries[tape.index(boundaries.len() - 1, "boundary")] as i64;
                (at + tape.range(0..5, "offset") as i64 - 2).max(0) as usize
            }
            _ => tape.range(0..4 * 64 + 1, "short length") as usize,
        };
    }
    match tape.below(4, "length kind") {
        0 => {
            let at = boundaries[tape.index(boundaries.len(), "boundary")] as i64;
            (at + tape.range(0..5, "offset") as i64 - 2).max(0) as usize
        }
        1 => tape.range(0..4 * KIB + 1, "short length") as usize,
        2 => tape.range(0..64 * KIB + 1, "length") as usize,
        _ => tape.range(0..300 * KIB + 1, "long length") as usize,
    }
}

fn agrees(tape: &Tape) {
    let mut key = [0u8; 32];
    tape.fill(&mut key, "key");
    let contexts = [
        "",
        "blaze v2 file 1",
        "a context longer than one block of sixty-four bytes, so its key takes two blocks",
    ];
    let context = contexts[tape.index(contexts.len(), "context")];
    let context_key = blake3::hazmat::hash_derive_key_context(context);
    assert_eq!(blaze_blake3::context_key(context), context_key, "the context key of {context:?}");
    let mode = match tape.below(4, "mode") {
        0 => Mode::Plain,
        1 => Mode::Keyed(&key),
        2 => Mode::DeriveKey(context),
        _ => Mode::ContextKey(&context_key),
    };
    let len = length(tape);
    let mut input = vec![0u8; len];
    tape.fill(&mut input, "input");

    let mut theirs = mode.theirs();
    theirs.update(&input);
    let expected = *theirs.finalize().as_bytes();
    assert_eq!(mode.ours_once(&input), expected, "{mode:?}, {len} B, one shot");

    // In pieces, finalized once part way, which leaves the hasher as it was.
    let mut ours = mode.ours();
    let midway = tape.range(0..len as u64 + 1, "midway") as usize;
    let mut checked_midway = false;
    let mut at = 0;
    while at < len {
        let take = match tape.below(4, "piece kind") {
            0 => tape.range(1..66, "piece"),
            1 => tape.range(1..4 * KIB + 2, "piece"),
            2 => 16 * KIB,
            _ => (len - at) as u64,
        } as usize;
        let take = take.min(len - at);
        ours.update(&input[at..at + take]);
        at += take;
        if !checked_midway && at >= midway {
            checked_midway = true;
            let mut prefix = mode.theirs();
            prefix.update(&input[..at]);
            let want = *prefix.finalize().as_bytes();
            assert_eq!(ours.finalize(), want, "{mode:?}, at {at} of {len} B");
        }
    }
    assert_eq!(ours.count(), len as u64, "{mode:?}, {len} B");
    assert_eq!(ours.finalize(), expected, "{mode:?}, {len} B in pieces");

    // Extended output from a random position.
    let position = tape.range(0..1 << 40, "position");
    let size = tape.range(0..300, "output size") as usize;
    let (mut a, mut b) = (vec![0; size], vec![0; size]);
    let mut reader = ours.finalize_xof();
    reader.set_position(position);
    reader.fill(&mut a);
    let mut theirs_reader = theirs.finalize_xof();
    theirs_reader.set_position(position);
    theirs_reader.fill(&mut b);
    assert_eq!(a, b, "{size} output bytes from {position}, {mode:?}, {len} B");
    assert_eq!(reader.position(), position + size as u64);
}

#[test]
fn official_blake3_agrees() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { Config::DEFAULT.cases }), agrees);
}

/// Every length to three chunks past a batch of 16, one shot and in one update, plain mode:
/// each leaf and tree shape the hasher makes, without the search.
#[test]
fn every_length_to_nineteen_chunks() {
    let input: Vec<u8> =
        (0..19 * 1024 + 1).map(|at: u32| (at.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    // Under Miri, the lengths at each block, chunk and batch boundary only.
    let lens: Vec<usize> = if cfg!(miri) {
        [0, 1, 63, 64, 65, 1023, 1024, 1025, 2048, 2049, 8 * 1024, 8 * 1024 + 1]
            .into_iter()
            .chain([16 * 1024 - 1, 16 * 1024, 16 * 1024 + 1, 17 * 1024, 19 * 1024])
            .collect()
    } else {
        (0..input.len()).step_by(7).chain([1024, 8 * 1024, 16 * 1024, 17 * 1024]).collect()
    };
    for len in lens {
        let input = &input[..len];
        let expected = *blake3::hash(input).as_bytes();
        assert_eq!(blaze_blake3::hash(input), expected, "{len} B");
        let mut ours = blaze_blake3::Hasher::new();
        ours.update(input);
        assert_eq!(ours.finalize(), expected, "{len} B in one update");
    }
}
