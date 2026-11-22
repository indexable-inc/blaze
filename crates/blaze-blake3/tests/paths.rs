//! Every path this crate hashes by gives the same digest: the one-chunk root, the hasher fed
//! whole and in pieces, `hash_many` side by side in lanes, at every length from 0 to 4,097 bytes
//! and at every alignment of the input to 16 bytes, held to official BLAKE3 (the `blake3`
//! crate, test-only). And the scalar compression's lane twin, at 1, 4, 8 and 16 lanes, equal to
//! it word for word.

use blaze_blake3::lanes;
use blaze_sim::{Config, Tape};

/// Lengths: every one to 4,097 bytes; under Miri, those at each block and chunk boundary.
fn lengths() -> Vec<usize> {
    if cfg!(miri) {
        vec![0, 1, 7, 8, 9, 63, 64, 65, 127, 128, 1023, 1024, 1025, 2049]
    } else {
        (0..=4097).collect()
    }
}

/// Offsets of the input from a 16-byte boundary: all 16; under Miri, three.
fn alignments() -> Vec<usize> {
    if cfg!(miri) { vec![0, 1, 7] } else { (0..16).collect() }
}

/// A buffer starting on a 16-byte boundary, holding `len` pattern bytes from `offset`.
#[repr(C, align(16))]
struct Aligned([u8; 4097 + 16]);

fn pattern(at: usize) -> u8 {
    ((at as u32).wrapping_mul(2_654_435_761) >> 24) as u8
}

#[test]
fn every_path_at_every_length_and_alignment() {
    let lens = lengths();
    let expected: Vec<[u8; 32]> = lens
        .iter()
        .map(|&len| {
            let input: Vec<u8> = (0..len).map(pattern).collect();
            *blake3::hash(&input).as_bytes()
        })
        .collect();
    let mut buffer = Box::new(Aligned([0; 4097 + 16]));
    for offset in alignments() {
        for (at, byte) in buffer.0[offset..].iter_mut().enumerate() {
            *byte = pattern(at);
        }
        let inputs: Vec<&[u8]> = lens.iter().map(|&len| &buffer.0[offset..offset + len]).collect();
        for ((&len, input), want) in lens.iter().zip(&inputs).zip(&expected) {
            assert_eq!(&blaze_blake3::hash(input), want, "{len} B at offset {offset}, one shot");
            let mut whole = blaze_blake3::Hasher::new();
            whole.update(input);
            assert_eq!(&whole.finalize(), want, "{len} B at offset {offset}, one update");
            let pieces: &[usize] = if cfg!(miri) { &[1, 65] } else { &[1, 63, 65, 1000] };
            for &piece in pieces {
                let mut fed = blaze_blake3::Hasher::new();
                for part in input.chunks(piece) {
                    fed.update(part);
                }
                assert_eq!(&fed.finalize(), want, "{len} B at offset {offset}, pieces of {piece}");
            }
        }
        let mut out = vec![[0; 32]; inputs.len()];
        blaze_blake3::hash_many(&inputs, &mut out);
        for ((&len, got), want) in lens.iter().zip(&out).zip(&expected) {
            assert_eq!(got, want, "{len} B at offset {offset}, hash_many");
        }
    }
}

/// A tape of inputs of mixed lengths, longer than a chunk too, in every mode: `*_many` equal to
/// one at a time, in the inputs' order.
fn many_agrees(tape: &Tape) {
    let count = tape.range(0..if cfg!(miri) { 12 } else { 80 }, "inputs") as usize;
    let most = if cfg!(miri) { 300 } else { 3000 };
    let datas: Vec<Vec<u8>> = (0..count)
        .map(|_| {
            let len = match tape.below(3, "length kind") {
                0 => tape.range(0..65, "short"),
                1 => tape.range(0..1025, "a chunk at most"),
                _ => tape.range(0..most + 1, "any"),
            } as usize;
            let mut data = vec![0; len];
            tape.fill(&mut data, "input");
            data
        })
        .collect();
    let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
    let mut key = [0; 32];
    tape.fill(&mut key, "key");
    let mut out = vec![[0; 32]; count];

    blaze_blake3::hash_many(&inputs, &mut out);
    for (input, got) in inputs.iter().zip(&out) {
        assert_eq!(*got, blaze_blake3::hash(input), "hash_many, {} B", input.len());
    }
    blaze_blake3::keyed_hash_many(&key, &inputs, &mut out);
    for (input, got) in inputs.iter().zip(&out) {
        assert_eq!(*got, blaze_blake3::keyed_hash(&key, input), "keyed, {} B", input.len());
    }
    blaze_blake3::derive_key_many(&key, &inputs, &mut out);
    for (input, got) in inputs.iter().zip(&out) {
        assert_eq!(*got, blaze_blake3::derive_key_with(&key, input), "derive, {} B", input.len());
    }
}

#[test]
fn many_is_one_at_a_time() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 4 } else { Config::DEFAULT.cases }), many_agrees);
}

/// The lane compression at `N` lanes, each lane its own inputs, equal to the scalar one lane by
/// lane.
fn twin_at<const N: usize>(tape: &Tape) {
    let words = |tape: &Tape, name: &'static str| -> u32 { tape.range(0..1 << 32, name) as u32 };
    let cvs: [[u32; 8]; N] = std::array::from_fn(|_| std::array::from_fn(|_| words(tape, "cv")));
    let blocks: [[u32; 16]; N] =
        std::array::from_fn(|_| std::array::from_fn(|_| words(tape, "block")));
    let counters: [u64; N] = std::array::from_fn(|_| tape.range(0..u64::MAX, "counter"));
    let lens: [u32; N] = std::array::from_fn(|_| tape.range(0..65, "block length") as u32);
    let flags: [u32; N] = std::array::from_fn(|_| tape.range(0..128, "flags") as u32);
    let got = lanes::compress_each(
        &std::array::from_fn(|word| std::array::from_fn(|lane| cvs[lane][word])),
        &std::array::from_fn(|word| std::array::from_fn(|lane| blocks[lane][word])),
        counters.map(|counter| counter as u32),
        counters.map(|counter| (counter >> 32) as u32),
        lens,
        flags,
    );
    for lane in 0..N {
        let want =
            blaze_blake3::compress(&cvs[lane], &blocks[lane], counters[lane], lens[lane], flags[lane]);
        let got: [u32; 8] = std::array::from_fn(|word| got[word][lane]);
        assert_eq!(got[..], want[..8], "lane {lane} of {N}");
    }
}

#[test]
fn lane_compression_is_the_scalar_one() {
    let cases = Config::cases(if cfg!(miri) { 2 } else { 64 });
    blaze_sim::check(cases.clone(), twin_at::<1>);
    blaze_sim::check(cases.clone(), twin_at::<4>);
    blaze_sim::check(cases.clone(), twin_at::<8>);
    blaze_sim::check(cases, twin_at::<16>);
}

/// The last-block reader: every length to a block, at every offset, equal to copying the bytes
/// into a zeroed block.
#[test]
fn padded_words_is_a_zeroed_copy() {
    let bytes: Vec<u8> = (0..64 + 16).map(pattern).collect();
    let offsets = if cfg!(miri) { 2 } else { 16 };
    for offset in 0..offsets {
        for len in 0..=64 {
            let tail = &bytes[offset..offset + len];
            let mut block = [0u8; 64];
            block[..len].copy_from_slice(tail);
            assert_eq!(lanes::padded_words(tail), lanes::block_words(&block), "{len} B at {offset}");
        }
    }
}
