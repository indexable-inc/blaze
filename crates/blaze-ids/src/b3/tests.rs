//! `blake3-servil` against official BLAKE3 1.8.7, byte for byte: the plain hash, the keyed hash,
//! key derivation by context string and by context key, incremental updates in pieces of every
//! size, and extended output read from any position, over lengths from 0 to 1 MiB that include
//! every block, chunk, SIMD group, stage and pool boundary either implementation splits at, each
//! input starting at a random alignment.

use blake3::hazmat::HasherExt;
use blake3_servil::{Mode, convenience, performance};

use super::{ContextKey, official, servil};
use crate::{
    ContentId,
    rng::SplitMix64,
};

const MIB: usize = 1 << 20;

/// The lengths either implementation splits at, and one each side: a block, a chunk, the
/// kernels' chunk groups (4, 8, 16 and 32 chunks), the fork's 16 KiB stage and 64 KiB prefetch
/// bound, its pool's 512 KiB, and 1 MiB.
fn boundaries() -> Vec<usize> {
    let mut lens = vec![0, 1, 2, 31, 32, 33, 55, 56];
    for at in [
        64,
        128,
        960,
        1024,
        2048,
        3072,
        4096,
        8192,
        15 * 1024,
        16 * 1024,
        17 * 1024,
        31 * 1024,
        32 * 1024,
        33 * 1024,
        64 * 1024,
        128 * 1024,
        256 * 1024,
        512 * 1024,
        MIB,
    ] {
        lens.extend([at - 1, at, at + 1]);
    }
    lens.retain(|&len| len <= MIB);
    lens
}

/// The boundaries, and random lengths: mostly short, as blaze's inputs are, some up to 1 MiB.
fn lengths(rng: &mut SplitMix64) -> Vec<usize> {
    let mut lens = boundaries();
    lens.extend((0..256).map(|_| rng.range(0..4097) as usize));
    lens.extend((0..64).map(|_| rng.range(0..64 * 1024 + 1) as usize));
    lens.extend((0..24).map(|_| rng.range(0..MIB as u64 + 1) as usize));
    lens
}

/// One of BLAKE3's modes, as both crates name it.
#[derive(Clone, Copy, Debug)]
enum Of<'a> {
    Plain,
    Keyed(&'a [u8; 32]),
    DeriveKey(&'a str),
    ContextKey(&'a ContextKey),
}

impl<'a> Of<'a> {
    fn servil(self) -> Mode<'a> {
        match self {
            Of::Plain => Mode::Plain,
            Of::Keyed(key) => Mode::Keyed(key),
            Of::DeriveKey(context) => Mode::DeriveKey(context),
            Of::ContextKey(key) => Mode::DeriveKeyWith(key),
        }
    }

    fn official(self) -> blake3::Hasher {
        match self {
            Of::Plain => blake3::Hasher::new(),
            Of::Keyed(key) => blake3::Hasher::new_keyed(key),
            Of::DeriveKey(context) => blake3::Hasher::new_derive_key(context),
            Of::ContextKey(key) => blake3::Hasher::new_from_context_key(key),
        }
    }

    /// Official BLAKE3's one-shot call for the mode, where it has one.
    fn official_once(self, input: &[u8]) -> [u8; 32] {
        match self {
            Of::Plain => *blake3::hash(input).as_bytes(),
            Of::Keyed(key) => *blake3::keyed_hash(key, input).as_bytes(),
            Of::DeriveKey(context) => blake3::derive_key(context, input),
            Of::ContextKey(key) => *blake3::Hasher::new_from_context_key(key).update(input).finalize().as_bytes(),
        }
    }
}

/// Piece lengths a caller feeds a hasher: bytes at a time, a few blocks, the 64 KiB a reader
/// reads, exactly the fork's 16 KiB stage, or most of the input.
fn piece(rng: &mut SplitMix64, left: usize) -> usize {
    let len = match rng.below(6) {
        0 => rng.range(0..65) as usize,
        1 => rng.range(0..4097) as usize,
        2 => 16 * 1024,
        3 => 64 * 1024,
        4 => rng.range(0..256 * 1024 + 1) as usize,
        _ => left,
    };
    len.min(left)
}

/// Every check of one mode and one input.
fn check(rng: &mut SplitMix64, of: Of<'_>, input: &[u8]) {
    let len = input.len();
    let mode = of.servil();
    let expected = of.official_once(input);
    let mut reference = of.official();
    reference.update(input);
    assert_eq!(*reference.finalize().as_bytes(), expected, "official BLAKE3's two calls, {of:?}, {len} B");

    assert_eq!(*convenience::hash_with(mode, input).as_bytes(), expected, "hash_with, {of:?}, {len} B");
    if let Of::Plain = of {
        assert_eq!(*convenience::hash(input).as_bytes(), expected, "hash, {len} B");
        assert_eq!(servil::hash(input), expected, "the backend's hash, {len} B");
    }
    if let Of::ContextKey(key) = of {
        assert_eq!(servil::derive_key_with(key, input), expected, "the backend's derive_key_with, {len} B");
    }
    let mut whole = performance::Hasher::new(mode);
    whole.update(input);
    assert_eq!(*whole.finalize().as_bytes(), expected, "one update, {of:?}, {len} B");

    // In pieces, finalized once part way (which must leave the hasher as it was) and at the end.
    let mut pieces = performance::Hasher::new(mode);
    let midway = rng.range(0..len as u64 + 1) as usize;
    let mut at = 0;
    let mut checked_midway = false;
    while at < len {
        let take = piece(rng, len - at).max(1);
        pieces.update(&input[at..at + take]);
        at += take;
        if !checked_midway && at >= midway {
            checked_midway = true;
            assert_eq!(
                *pieces.finalize().as_bytes(),
                of.official_once(&input[..at]),
                "finalized part way, at {at} of {len} B, {of:?}"
            );
        }
    }
    assert_eq!(pieces.count(), len as u64);
    assert_eq!(*pieces.finalize().as_bytes(), expected, "in pieces, {of:?}, {len} B");

    // Extended output: from the start, then from a random position, then on from there.
    let mut ours = pieces.finalize_xof();
    let mut theirs = reference.finalize_xof();
    let mut a = vec![0; rng.range(0..2049) as usize];
    let mut b = vec![0; a.len()];
    ours.fill(&mut a);
    theirs.fill(&mut b);
    assert_eq!(a, b, "the first {} output bytes, {of:?}, {len} B", a.len());
    let position = rng.range(0..1 << 40);
    ours.set_position(position);
    theirs.set_position(position);
    for _ in 0..2 {
        let size = rng.range(0..1025) as usize;
        let (a_len, b_len) = (size.min(a.len()), size.min(b.len()));
        let (a, b) = (&mut a[..a_len], &mut b[..b_len]);
        ours.fill(a);
        theirs.fill(b);
        assert_eq!(a, b, "{size} output bytes from {position}, {of:?}, {len} B");
    }
    assert_eq!(ours.position(), theirs.position());
    let mut a = [0; 131];
    let mut b = [0; 131];
    convenience::hash_xof(mode, input).fill(&mut a);
    let mut again = of.official();
    again.update(input).finalize_xof().fill(&mut b);
    assert_eq!(a, b, "hash_xof, {of:?}, {len} B");
}

#[test]
fn servil_is_official_blake3() {
    let mut rng = SplitMix64::new(0x5e41_17b1_a4e3);
    let mut bytes = vec![0; MIB + 64];
    rng.fill(&mut bytes);
    let key: &'static [u8; 32] = Box::leak(Box::new(std::array::from_fn(|i| bytes[i] ^ 0x5a)));
    let contexts: [&'static str; 4] = [
        "",
        "blaze v2 file 1",
        "sixty-four bytes of context, exactly, so its key is a full block",
        "a context longer than one block of sixty-four bytes, so its key takes two blocks to hash",
    ];
    assert_eq!(contexts[2].len(), 64);
    let context_keys: Vec<&'static ContextKey> = contexts
        .iter()
        .map(|context| {
            let key = servil::context_key(context);
            assert_eq!(key, blake3::hazmat::hash_derive_key_context(context), "the context key of {context:?}");
            assert_eq!(performance::context_key(context), key);
            assert_eq!(official::context_key(context), key);
            &*Box::leak(Box::new(key))
        })
        .collect();
    for len in lengths(&mut rng) {
        let start = rng.index(64);
        let input = &bytes[start..start + len];
        check(&mut rng, Of::Plain, input);
        check(&mut rng, Of::Keyed(key), input);
        let at = rng.index(contexts.len());
        check(&mut rng, Of::DeriveKey(contexts[at]), input);
        check(&mut rng, Of::ContextKey(context_keys[at]), input);
    }
}

/// The two backends' hashers and one-shot calls, as blaze's IDs use them: a context key, then
/// short parts.
#[test]
fn the_backends_hash_alike() {
    let mut rng = SplitMix64::new(7);
    let mut bytes = vec![0; 70_000];
    rng.fill(&mut bytes);
    for context in ["blaze v2 a 1", "", "blaze v2 a domain of some length 7"] {
        let key = official::context_key(context);
        for _ in 0..200 {
            let mut ours = servil::Hasher::new_from_context_key(&key);
            let mut theirs = official::Hasher::new_from_context_key(&key);
            let mut derived = servil::Hasher::new_derive_key(context);
            let mut plain = (servil::Hasher::new(), official::Hasher::new());
            let mut whole = Vec::new();
            for _ in 0..rng.range(0..12) {
                let len = if rng.one_in(8) { rng.range(0..70_000) } else { rng.range(0..80) } as usize;
                let start = rng.index(bytes.len() - len + 1);
                let part = &bytes[start..start + len];
                ours.update(part);
                theirs.update(part);
                derived.update(part);
                plain.0.update(part);
                plain.1.update(part);
                whole.extend_from_slice(part);
            }
            assert_eq!(ours.finalize(), theirs.finalize(), "{context:?}, {} B", whole.len());
            assert_eq!(derived.finalize(), theirs.finalize(), "{context:?}, {} B", whole.len());
            assert_eq!(plain.0.finalize(), plain.1.finalize(), "{} B", whole.len());
            assert_eq!(servil::derive_key_with(&key, &whole), theirs.finalize());
            assert_eq!(official::derive_key_with(&key, &whole), theirs.finalize());
            assert_eq!(servil::hash(&whole), official::hash(&whole));
        }
    }
}

/// [`ContentId::of_many`] by each backend's way, at every one-chunk length and group size, and
/// beyond a chunk.
#[cfg(not(blaze_check))]
#[test]
fn of_many_is_alike_on_both_backends() {
    let bytes: Vec<u8> = (0..2100u32).map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    for copies in [1, 2, 3, 7, 8, 9, 16, 17, 64, 65] {
        let inputs: Vec<&[u8]> = (0..=1100)
            .step_by(if copies > 16 { 7 } else { 1 })
            .flat_map(|len| (0..copies).map(move |copy| (len, copy)))
            .map(|(len, copy)| &bytes[copy * 13..copy * 13 + len])
            .collect();
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        servil::of_many_into("blaze v2 a 1", &inputs, &mut ours);
        official::of_many_into("blaze v2 a 1", &inputs, &mut theirs);
        assert_eq!(ours, theirs, "{copies} of each length");
        for (input, id) in inputs.iter().zip(&ours) {
            let mut expected = blake3::Hasher::new_derive_key("blaze v2 a 1");
            expected.update(input);
            assert_eq!(*id, ContentId(*expected.finalize().as_bytes()), "{} bytes", input.len());
        }
    }
}

/// blaze's hashing never starts the fork's pool: its threads start in `initialize_multithreaded`
/// alone, which blaze never calls, and a multithreaded call without it panics rather than
/// starting them.
#[cfg(not(blaze_check))]
#[test]
fn hashing_starts_no_thread() {
    let big = vec![7u8; 4 * MIB];
    let _ = servil::hash(&big);
    let mut hasher = servil::Hasher::new();
    hasher.update(&big);
    let _ = hasher.finalize();
    let _ = ContentId::of("blaze v2 a 1", &big);
    let mut out = Vec::new();
    servil::of_many_into("blaze v2 a 1", &[&big[..100], &big[..2000]], &mut out);
    let refused = std::panic::catch_unwind(|| {
        let blocks = vec![performance::Block::ZERO; MIB / 64];
        performance::hash_multithreaded(Mode::Plain, &blocks, MIB)
    });
    assert!(refused.is_err(), "a multithreaded call before initialize_multithreaded panics");
    #[cfg(target_os = "linux")]
    for task in std::fs::read_dir("/proc/self/task").expect("this process's threads") {
        let comm = std::fs::read_to_string(task.expect("a thread").path().join("comm")).unwrap_or_default();
        assert!(!comm.starts_with("blake3-"), "a thread of the fork's pool: {comm:?}");
    }
}
