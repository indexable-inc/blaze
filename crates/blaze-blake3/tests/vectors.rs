//! The official BLAKE3 test vectors, every mode and the 131 bytes of extended output each.
//!
//! `test_vectors.json` is a byte-for-byte copy of `test_vectors/test_vectors.json` of the
//! BLAKE3 repository (github.com/BLAKE3-team/BLAKE3), taken from this tree's
//! `vendor/blake3-servil/test_vectors/test_vectors.json` (the fork keeps upstream's file
//! unchanged; SHA-256 dcb91ea8accc77e6d6e632af7cdc1a99a9f3ae78cf648da595c7d064db32f624). No
//! digest here was written by hand.

const VECTORS: &str = include_str!("test_vectors.json");

/// One case of the file: an input length and each mode's extended output.
struct Case {
    input_len: usize,
    hash: Vec<u8>,
    keyed_hash: Vec<u8>,
    derive_key: Vec<u8>,
}

fn hex(text: &str) -> Vec<u8> {
    assert!(text.len() % 2 == 0, "hex of odd length: {text:?}");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("a hex byte"))
        .collect()
}

/// The quoted value of a `"name": "value",` line.
fn quoted<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix('"')?.strip_prefix(name)?.strip_prefix("\": \"")?;
    Some(rest.trim_end_matches(',').trim_end_matches('"'))
}

/// The file's key, context string and cases, read line by line: it has one field a line.
fn parse() -> (String, String, Vec<Case>) {
    let (mut key, mut context, mut cases) = (None, None, Vec::new());
    let mut current: Option<Case> = None;
    for line in VECTORS.lines().map(str::trim) {
        if let Some(len) = line.strip_prefix("\"input_len\": ") {
            let input_len = len.trim_end_matches(',').parse().expect("an input length");
            if let Some(done) = current.replace(Case {
                input_len,
                hash: Vec::new(),
                keyed_hash: Vec::new(),
                derive_key: Vec::new(),
            }) {
                cases.push(done);
            }
        } else if let Some(value) = quoted(line, "keyed_hash") {
            current.as_mut().expect("a case").keyed_hash = hex(value);
        } else if let Some(value) = quoted(line, "derive_key") {
            current.as_mut().expect("a case").derive_key = hex(value);
        } else if let Some(value) = quoted(line, "hash") {
            current.as_mut().expect("a case").hash = hex(value);
        } else if let Some(value) = quoted(line, "key") {
            key = Some(value.to_owned());
        } else if let Some(value) = quoted(line, "context_string") {
            context = Some(value.to_owned());
        }
    }
    cases.extend(current);
    (key.expect("the key field"), context.expect("the context_string field"), cases)
}

/// The input of a case: `0, 1, ..., 250` repeated.
fn input(len: usize) -> Vec<u8> {
    (0..len).map(|at| (at % 251) as u8).collect()
}

/// Feeds `hasher` `input` in pieces of `piece` bytes.
fn fed(mut hasher: blaze_blake3::Hasher, input: &[u8], piece: usize) -> blaze_blake3::Hasher {
    for part in input.chunks(piece.max(1)) {
        hasher.update(part);
    }
    if input.is_empty() {
        hasher.update(&[]);
    }
    hasher
}

/// Under Miri, which interprets every round, only the cases up to one chunk past a batch of
/// 16 chunks (17 KiB): every leaf, batch and tree boundary the hasher splits at, in a few
/// minutes (`cargo miri test -p blaze-blake3`).
fn sized(case: &&Case) -> bool {
    !cfg!(miri) || case.input_len <= 17 * 1024
}

/// The pieces a case is fed in: fewer under Miri, still across a block and a chunk.
const PIECES: &[usize] = if cfg!(miri) {
    &[1, 64, 1025]
} else {
    &[1, 63, 64, 65, 1023, 1024, 1025, 4096, 8 * 1024 + 1, 17 * 1024]
};

fn extended(hasher: &blaze_blake3::Hasher, len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    hasher.finalize_xof().fill(&mut out);
    out
}

#[test]
fn every_official_vector() {
    let (key, context, cases) = parse();
    let key: [u8; 32] = key.as_bytes().try_into().expect("a 32-byte key");
    assert_eq!(cases.len(), 35, "the file's cases");
    let lens: Vec<usize> = cases.iter().map(|case| case.input_len).collect();
    for expected in [0, 1, 1023, 1024, 1025, 2048, 2049, 8192, 16384, 31744, 102400] {
        assert!(lens.contains(&expected), "a case of {expected} bytes");
    }
    for case in cases.iter().filter(sized) {
        let len = case.input_len;
        let input = input(len);
        for (mode, want) in
            [("hash", &case.hash), ("keyed_hash", &case.keyed_hash), ("derive_key", &case.derive_key)]
        {
            assert_eq!(want.len(), 131, "{mode}, {len} B: the file's extended output");
            let short: &[u8] = &want[..32];
            let one_shot = match mode {
                "hash" => blaze_blake3::hash(&input),
                "keyed_hash" => blaze_blake3::keyed_hash(&key, &input),
                _ => blaze_blake3::derive_key(&context, &input),
            };
            assert_eq!(one_shot, short, "{mode}, {len} B, one shot");
            for &piece in [len].iter().chain(PIECES) {
                let hasher = match mode {
                    "hash" => blaze_blake3::Hasher::new(),
                    "keyed_hash" => blaze_blake3::Hasher::new_keyed(&key),
                    _ => blaze_blake3::Hasher::new_derive_key(&context),
                };
                let hasher = fed(hasher, &input, piece);
                assert_eq!(hasher.count(), len as u64, "{mode}, {len} B in pieces of {piece}");
                assert_eq!(hasher.finalize(), short, "{mode}, {len} B in pieces of {piece}");
                assert_eq!(&extended(&hasher, 131), want, "{mode}, {len} B in pieces of {piece}");
            }
        }
    }
}

/// The extended output read from a position is the same bytes as read from the start.
#[test]
fn extended_output_from_any_position() {
    let (_, _, cases) = parse();
    for case in cases.iter().filter(sized) {
        let hasher = fed(blaze_blake3::Hasher::new(), &input(case.input_len), case.input_len);
        for start in [0, 1, 31, 32, 63, 64, 65, 100, 130] {
            let mut reader = hasher.finalize_xof();
            reader.set_position(start as u64);
            let mut out = vec![0; 131 - start];
            reader.fill(&mut out);
            assert_eq!(out, case.hash[start..], "{} B from {start}", case.input_len);
            assert_eq!(reader.position(), 131);
        }
    }
}

/// A many-inputs entry in each mode: the inputs, and the slots their digests go to.
type ManyFn = fn(&[u8; 32], &[&[u8]], &mut [[u8; 32]]);

/// The plain mode's `hash_many` as a [`ManyFn`] (it takes no key).
fn plain_many(_: &[u8; 32], inputs: &[&[u8]], out: &mut [[u8; 32]]) {
    blaze_blake3::hash_many(inputs, out);
}

/// The batcher at 4 inputs a batch, so a batch of the file's cases flushes several times.
fn batched(mode: fn(&[u8; 32]) -> blaze_blake3::Mode, key: &[u8; 32], inputs: &[&[u8]], out: &mut [[u8; 32]]) {
    let mut many = blaze_blake3::Many::<4>::new(mode(key));
    for (input, slot) in inputs.iter().zip(out.iter_mut()) {
        many.push(*input, slot);
    }
    many.finish();
}

/// Where `hash_many`-shaped `entries` disagree with the file's 32-byte answers (each case alone,
/// then all cases in one call, forward and reversed: one-block, one-chunk and many-chunk inputs
/// side by side), or `None`. Keyed and derive-key cases take the file's key and the context's key.
fn first_wrong(entries: [ManyFn; 3]) -> Option<String> {
    let (key, context, cases) = parse();
    let key: [u8; 32] = key.as_bytes().try_into().expect("a 32-byte key");
    let context_key = blaze_blake3::context_key(&context);
    let cases: Vec<&Case> = cases.iter().filter(sized).collect();
    let inputs: Vec<Vec<u8>> = cases.iter().map(|case| input(case.input_len)).collect();
    for (mode, entry) in ["hash", "keyed_hash", "derive_key"].into_iter().zip(entries) {
        let key = if mode == "derive_key" { context_key } else { key };
        let want = |case: &Case| -> [u8; 32] {
            let all = match mode {
                "hash" => &case.hash,
                "keyed_hash" => &case.keyed_hash,
                _ => &case.derive_key,
            };
            all[..32].try_into().expect("the file's 131 bytes")
        };
        for (case, input) in cases.iter().zip(&inputs) {
            let mut out = [[0; 32]];
            entry(&key, &[input.as_slice()], &mut out);
            if out[0] != want(case) {
                return Some(format!("{mode}, {} B alone", case.input_len));
            }
        }
        for forward in [true, false] {
            let mut order: Vec<usize> = (0..cases.len()).collect();
            if !forward {
                order.reverse();
            }
            let batch: Vec<&[u8]> = order.iter().map(|&at| inputs[at].as_slice()).collect();
            let mut out = vec![[0; 32]; batch.len()];
            entry(&key, &batch, &mut out);
            for (&at, got) in order.iter().zip(&out) {
                if *got != want(cases[at]) {
                    return Some(format!("{mode}, {} B in a batch (forward {forward})", cases[at].input_len));
                }
            }
        }
    }
    None
}

/// The official vectors through the many-inputs entries, alone and in mixed batches: `*_many`,
/// `many_checked` and the batcher. Expected: `test every_official_vector_through_many ... ok`.
#[test]
fn every_official_vector_through_many() {
    assert_eq!(
        first_wrong([plain_many, blaze_blake3::keyed_hash_many, blaze_blake3::derive_key_many]),
        None,
        "*_many"
    );
    assert_eq!(
        first_wrong([
            |_, inputs, out| blaze_blake3::many_checked(blaze_blake3::Mode::Hash, inputs, out).expect("counts"),
            |key, inputs, out| blaze_blake3::many_checked(blaze_blake3::Mode::Keyed(*key), inputs, out).expect("counts"),
            |key, inputs, out| {
                blaze_blake3::many_checked(blaze_blake3::Mode::DeriveKey(*key), inputs, out).expect("counts")
            },
        ]),
        None,
        "many_checked"
    );
    assert_eq!(
        first_wrong([
            |key, inputs, out| batched(|_| blaze_blake3::Mode::Hash, key, inputs, out),
            |key, inputs, out| batched(|key| blaze_blake3::Mode::Keyed(*key), key, inputs, out),
            |key, inputs, out| batched(|key| blaze_blake3::Mode::DeriveKey(*key), key, inputs, out),
        ]),
        None,
        "Many::<4>"
    );
}

/// The planted bug: a lane hashing a one-block input (at most 64 bytes) without `ROOT` on its
/// last block, everything else as `hash_many`.
fn rootless_many(_: &[u8; 32], inputs: &[&[u8]], out: &mut [[u8; 32]]) {
    blaze_blake3::hash_many(inputs, out);
    for (input, out) in inputs.iter().zip(out.iter_mut()) {
        if input.len() <= blaze_blake3::BLOCK_LEN {
            let flags = blaze_blake3::CHUNK_START | blaze_blake3::CHUNK_END;
            let words = blaze_blake3::lanes::padded_words(input);
            let state = blaze_blake3::compress(&blaze_blake3::IV, &words, 0, input.len() as u32, flags);
            for (bytes, word) in out.chunks_exact_mut(4).zip(&state[..8]) {
                bytes.copy_from_slice(&word.to_le_bytes());
            }
        }
    }
}

/// The control for [`every_official_vector_through_many`]: the check catches a lane left
/// without `ROOT`, at the file's first one-block case (0 bytes). Expected:
/// `test a_rootless_lane_is_caught ... ok`.
#[test]
fn a_rootless_lane_is_caught() {
    let caught = first_wrong([rootless_many, blaze_blake3::keyed_hash_many, blaze_blake3::derive_key_many]);
    assert_eq!(caught.as_deref(), Some("hash, 0 B alone"));
}
