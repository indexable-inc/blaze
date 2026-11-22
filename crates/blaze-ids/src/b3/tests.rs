//! blaze's BLAKE3 against official BLAKE3 (the `blake3` crate), byte for byte, as blaze's IDs
//! use it: a context key, then parts of every length, and the one-shot calls. `blaze-blake3`'s
//! own tests hold it to the official test vectors and every mode; these hold the wrappers here.

use super::{context_key, derive_key_with, hash};
use crate::{ContentId, rng::SplitMix64};

/// The hasher and the one-shot calls agree with official BLAKE3 over random parts, mostly
/// short as blaze's are, some past many chunks.
#[test]
fn hashes_as_official_blake3() {
    let mut rng = SplitMix64::new(7);
    // Under Miri, a few chunks (1 KiB each) and a few rounds a context.
    let (most, rounds) = if cfg!(miri) { (4_100, 6) } else { (70_000, 200) };
    let mut bytes = vec![0; most];
    rng.fill(&mut bytes);
    for context in ["blaze v2 a 1", "", "blaze v2 a domain of some length 7"] {
        let key = context_key(context);
        assert_eq!(key, blake3::hazmat::hash_derive_key_context(context), "{context:?}");
        for _ in 0..rounds {
            let mut ours = super::Hasher::new_from_context_key(&key);
            let mut derived = super::Hasher::new_derive_key(context);
            let mut theirs = blake3::Hasher::new_derive_key(context);
            let mut whole = Vec::new();
            for _ in 0..rng.range(0..12) {
                let len =
                    if rng.one_in(8) { rng.range(0..most as u64) } else { rng.range(0..80) } as usize;
                let start = rng.index(bytes.len() - len + 1);
                let part = &bytes[start..start + len];
                ours.update(part);
                derived.update(part);
                theirs.update(part);
                whole.extend_from_slice(part);
            }
            let want = *theirs.finalize().as_bytes();
            assert_eq!(ours.finalize(), want, "{context:?}, {} B", whole.len());
            assert_eq!(derived.finalize(), want, "{context:?}, {} B", whole.len());
            assert_eq!(derive_key_with(&key, &whole), want, "{context:?}, {} B", whole.len());
            assert_eq!(hash(&whole), *blake3::hash(&whole).as_bytes(), "{} B", whole.len());
        }
    }
}

/// [`ContentId::of_many`] at every one-chunk length and group size, and beyond a chunk, is
/// official BLAKE3's derived key of each input.
#[test]
fn of_many_is_official_blake3() {
    let bytes: Vec<u8> =
        (0..2100u32).map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    // Under Miri, each side of 8 and 16 lanes, at one length in 37 across a chunk and past it.
    let all: &[usize] =
        if cfg!(miri) { &[1, 8, 9, 16, 17] } else { &[1, 2, 3, 7, 8, 9, 16, 17, 64, 65] };
    for &copies in all {
        let inputs: Vec<&[u8]> = (0..=1100)
            .step_by(if cfg!(miri) { 37 } else if copies > 16 { 7 } else { 1 })
            .flat_map(|len| (0..copies).map(move |copy| (len, copy)))
            .map(|(len, copy)| &bytes[copy * 13..copy * 13 + len])
            .collect();
        let ids = ContentId::of_many("blaze v2 a 1", &inputs);
        for (input, id) in inputs.iter().zip(&ids) {
            let mut expected = blake3::Hasher::new_derive_key("blaze v2 a 1");
            expected.update(input);
            assert_eq!(*id, ContentId(*expected.finalize().as_bytes()), "{} bytes", input.len());
        }
    }
}
