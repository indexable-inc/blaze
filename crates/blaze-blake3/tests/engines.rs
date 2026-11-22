//! Every [`Engine`] the crate can be handed against official BLAKE3 (the `blake3` crate, a
//! dev-dependency): each lone-block form ([`Single`]) at each degree ([`Degree`]) on each
//! instruction set this CPU proves ([`Isa`]), in every mode, one shot ([`digest_on`]), in one
//! update and in drawn pieces ([`Hasher::on`]), and many at a time ([`many_checked_on`]). The
//! row compression is held word for word to the scalar one on drawn inputs. Rule 78: each
//! instantiation of the seam is tested equal.
//!
//! What each test covers of the wide tree (`src/tree.rs`): every length to five chunks and a
//! byte runs each leaf shape (two to five chunks side by side, a partial last chunk, one chunk
//! alone); the boundary lengths run subtrees of every power of two to 64 chunks and their
//! parents side by side at every width; the drawn pieces run the hasher's subtree loop from
//! unaligned counts (the halving rule) and its append fast path.

use blaze_blake3::{Degree, Engine, EngineOpts, Hasher, Isa, Mode, Pair, Single, compress, compress_by, digest_on, many_checked_on};
use blaze_sim::{Config, Tape};

const KIB: usize = 1024;

/// The instruction sets this CPU proves: the baseline, the widest detected, and on an AVX-512
/// machine AVX2 too.
fn isas() -> Vec<Isa> {
    let mut isas = vec![Isa::BASELINE];
    let detected = Isa::detected();
    if detected != Isa::BASELINE {
        isas.push(detected);
    }
    #[cfg(target_arch = "x86_64")]
    if detected.name() == "avx512f" {
        // SAFETY: `Isa::detected` gave AVX-512F, which it gives only when `cpu::has` saw AVX2 as
        // well (and the OS saving the registers): this CPU runs AVX2 code.
        isas.push(unsafe { Isa::avx2_unchecked() });
    }
    isas
}

/// Every engine: each form at each degree on each instruction set, and each form of a pair
/// where the degree runs pairs (two or more).
fn engines() -> Vec<Engine> {
    let mut all = Vec::new();
    for isa in isas() {
        for degree in Degree::ALL {
            for single in [Single::Scalar, Single::Rows] {
                for pair in Pair::ALL {
                    if degree == Degree::One && pair != Pair::Lanes {
                        continue;
                    }
                    all.push(Engine::new(EngineOpts { single, degree, pair, isa }));
                }
            }
        }
    }
    all
}

fn name(engine: Engine) -> String {
    format!(
        "{:?} x{} pairs {:?} on {}",
        engine.single(),
        engine.degree().lanes(),
        engine.pair(),
        engine.isa().name()
    )
}

/// The modes, keys and context key drawn or fixed by the caller, and what official BLAKE3
/// gives in each.
fn theirs(mode: Mode) -> blake3::Hasher {
    match mode {
        Mode::Hash => blake3::Hasher::new(),
        Mode::Keyed(key) => blake3::Hasher::new_keyed(&key),
        Mode::DeriveKey(key) => {
            use blake3::hazmat::HasherExt;
            blake3::Hasher::new_from_context_key(&key)
        }
    }
}

fn expected(mode: Mode, input: &[u8]) -> [u8; 32] {
    let mut hasher = theirs(mode);
    hasher.update(input);
    *hasher.finalize().as_bytes()
}

fn modes() -> [Mode; 3] {
    let key: [u8; 32] = core::array::from_fn(|at| (at as u8).wrapping_mul(29).wrapping_add(3));
    [Mode::Hash, Mode::Keyed(key), Mode::DeriveKey(blake3::hazmat::hash_derive_key_context("blaze engines test 1"))]
}

fn bytes(len: usize) -> Vec<u8> {
    (0..len as u32).map(|at| (at.wrapping_mul(2_654_435_761) >> 24) as u8).collect()
}

/// Every length from 0 to five chunks and a byte, every engine, one shot and in one update, in
/// the plain mode; the keyed and derive modes at every block boundary of that range.
#[test]
fn every_engine_every_length_to_five_chunks() {
    let input = bytes(5 * KIB + 2);
    let lens: Vec<usize> = if cfg!(miri) {
        vec![0, 1, 64, 65, 1024, 1025, 2048, 2049, 3072, 4096, 4097, 5 * KIB + 1]
    } else {
        (0..=5 * KIB + 1).collect()
    };
    let want: Vec<[u8; 32]> = lens.iter().map(|&len| *blake3::hash(&input[..len]).as_bytes()).collect();
    for engine in engines() {
        for (&len, want) in lens.iter().zip(&want) {
            let input = &input[..len];
            assert_eq!(&digest_on(engine, Mode::Hash, input), want, "{}, {len} B, one shot", name(engine));
            let mut hasher = Hasher::on(engine, Mode::Hash);
            hasher.update(input);
            assert_eq!(&hasher.finalize(), want, "{}, {len} B, one update", name(engine));
        }
        for mode in modes() {
            for len in (0..=5 * KIB + 1).filter(|len| len % 64 <= 1 || len % 64 == 63) {
                let input = &input[..len];
                let want = expected(mode, input);
                assert_eq!(digest_on(engine, mode, input), want, "{}, {mode:?}, {len} B", name(engine));
                let mut hasher = Hasher::on(engine, mode);
                hasher.update(input);
                assert_eq!(hasher.finalize(), want, "{}, {mode:?}, {len} B, one update", name(engine));
            }
        }
    }
}

/// The lengths where the tree's shape turns: a power of two of chunks and one byte either side,
/// to 64 chunks, and every whole number of chunks to 40 and a byte past it: each subtree and
/// each width of parents side by side, every engine.
#[test]
fn every_engine_at_the_subtree_boundaries() {
    let mut lens = Vec::new();
    for chunks in 1..=40 {
        lens.extend([chunks * KIB - 1, chunks * KIB, chunks * KIB + 1]);
    }
    for power in [64, 128, 256] {
        if cfg!(miri) && power > 64 {
            break;
        }
        lens.extend([power * KIB - 1, power * KIB, power * KIB + 1, power * KIB + 3 * KIB + 5]);
    }
    if cfg!(miri) {
        lens.retain(|&len| len <= 9 * KIB);
    }
    let input = bytes(*lens.iter().max().expect("lengths"));
    for engine in engines() {
        for &len in &lens {
            let input = &input[..len];
            let want = *blake3::hash(input).as_bytes();
            assert_eq!(digest_on(engine, Mode::Hash, input), want, "{}, {len} B, one shot", name(engine));
            let mut hasher = Hasher::on(engine, Mode::Hash);
            hasher.update(input);
            assert_eq!(hasher.finalize(), want, "{}, {len} B, one update", name(engine));
        }
    }
}

/// A drawn engine and mode, a drawn length (to five chunks and a byte, near a subtree
/// boundary, or to 80 KiB), fed in drawn pieces: a few bytes (the append fast path), a block or
/// so, an odd count of chunks (so the next subtree starts unaligned and the halving rule
/// runs), a power of two of chunks, or the rest; finalized once part way, which leaves the
/// hasher as it was.
fn pieces_agree(tape: &Tape) {
    let engines = engines();
    let engine = engines[tape.index(engines.len(), "engine")];
    let modes = modes();
    let mode = modes[tape.index(modes.len(), "mode")];
    let len = match tape.below(3, "length kind") {
        0 => tape.range(0..5 * KIB as u64 + 2, "short length") as usize,
        1 => {
            let chunks = 1usize << tape.below(7, "power of two chunks");
            (chunks * KIB + tape.range(0..5, "offset") as usize).saturating_sub(2)
        }
        _ => tape.range(0..if cfg!(miri) { 9 * KIB as u64 } else { 80 * KIB as u64 }, "long length") as usize,
    };
    let mut input = vec![0u8; len];
    tape.fill(&mut input, "input");
    let want = expected(mode, &input);
    assert_eq!(digest_on(engine, mode, &input), want, "{}, {mode:?}, {len} B, one shot", name(engine));

    let mut ours = Hasher::on(engine, mode);
    let midway = tape.range(0..len as u64 + 1, "midway") as usize;
    let mut checked = false;
    let mut at = 0;
    while at < len {
        let take = match tape.below(5, "piece kind") {
            0 => tape.range(0..9, "a few bytes") as usize,
            1 => tape.range(1..3 * 64, "a block or so") as usize,
            2 => (2 * tape.range(0..4, "odd chunks") as usize + 1) * KIB,
            3 => (1usize << tape.below(5, "power of two chunks")) * KIB,
            _ => len - at,
        };
        let take = take.min(len - at);
        ours.update(&input[at..at + take]);
        at += take;
        if !checked && at >= midway {
            checked = true;
            assert_eq!(ours.finalize(), expected(mode, &input[..at]), "{}, {mode:?}, at {at} of {len} B", name(engine));
        }
    }
    assert_eq!(ours.count(), len as u64, "{}, {mode:?}, {len} B, count", name(engine));
    assert_eq!(ours.finalize(), want, "{}, {mode:?}, {len} B in pieces", name(engine));

    let position = tape.range(0..1 << 40, "position");
    let size = tape.range(0..200, "output size") as usize;
    let (mut a, mut b) = (vec![0; size], vec![0; size]);
    let mut reader = ours.finalize_xof();
    reader.set_position(position);
    reader.fill(&mut a);
    let mut official = theirs(mode);
    official.update(&input);
    let mut official = official.finalize_xof();
    official.set_position(position);
    official.fill(&mut b);
    assert_eq!(a, b, "{}, {mode:?}, {size} output bytes from {position}", name(engine));
}

#[test]
fn every_engine_in_drawn_pieces() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 4 * Config::DEFAULT.cases }), pieces_agree);
}

/// The feeds that leave the hasher's stack unmerged under a chunk with bytes in it: whole
/// chunks fed one update each, then a tail (the stack `[c0, c1]` under chunk 2, which must
/// finalize as `parent(parent(c0, c1), c2)`), and an exactly fed subtree followed by a few bytes
/// through the `append` fast path. 2026-11-20: blaze-cas's segment digests (streamed through
/// the hasher) stopped matching their one-shot check after the wide-tree hasher folded such a
/// stack into the chunk one subtree at a time; this holds every engine to official BLAKE3 on
/// them.
#[test]
fn a_tail_after_whole_chunks_finalizes_over_the_merged_stack() {
    let feeds: [&[usize]; 6] = [
        &[KIB, KIB, 1],
        &[KIB, KIB, KIB, 5],
        &[2 * KIB, 1],
        &[2 * KIB, KIB, KIB, 3],
        &[128, 300, 900, 1500, 4],
        &[KIB, 2 * KIB, 64, 4 * KIB, 1],
    ];
    for engine in engines() {
        for mode in modes() {
            for feed in feeds {
                let input = bytes(feed.iter().sum());
                let mut ours = Hasher::on(engine, mode);
                let mut at = 0;
                for &take in feed {
                    ours.update(&input[at..at + take]);
                    at += take;
                }
                assert_eq!(ours.finalize(), expected(mode, &input), "{}, {mode:?}, fed {feed:?}", name(engine));
            }
        }
    }
}

/// Every reader of the lazily merged stack, on stacks with surplus subtrees (a tail fed after
/// whole chunks, and inputs ending exactly on 2, 4, 8 and 16 chunks so the chunk is empty), in
/// every mode, by every engine: `finalize` twice gives the same bytes (reading the stack
/// changes nothing a second read depends on); a clone taken mid-stream and fed the rest
/// finalizes as the original does; the extended output from 0 and from a block past 0 is
/// official BLAKE3's; and feeding on after a finalize still gives the whole input's hash.
#[test]
fn every_stack_reader_is_official_and_leaves_the_hasher_as_it_was() {
    let feeds: [&[usize]; 9] = [
        &[KIB, KIB, 1],
        &[2 * KIB, 1],
        &[2 * KIB],
        &[4 * KIB],
        &[KIB, 3 * KIB],
        &[8 * KIB, 7],
        &[KIB, KIB, KIB, KIB, KIB, KIB, KIB, KIB],
        &[16 * KIB, 64],
        &[3 * KIB, 5 * KIB, 8 * KIB, 1],
    ];
    for engine in engines() {
        for mode in modes() {
            for feed in feeds {
                let total: usize = feed.iter().sum();
                let input = bytes(total + 100);
                let (input, more) = input.split_at(total);
                let mut ours = Hasher::on(engine, mode);
                let mut clone_at = None;
                let mut at = 0;
                for (step, &take) in feed.iter().enumerate() {
                    ours.update(&input[at..at + take]);
                    at += take;
                    if step == 0 {
                        clone_at = Some((ours.clone(), at));
                    }
                }
                let what = format!("{}, {mode:?}, fed {feed:?}", name(engine));
                let want = expected(mode, input);
                let first = ours.finalize();
                assert_eq!(first, want, "{what}, finalize");
                assert_eq!(ours.finalize(), first, "{what}, finalize twice");

                let mut official = theirs(mode);
                official.update(input);
                for position in [0, 64, 1000] {
                    let (mut a, mut b) = ([0; 150], [0; 150]);
                    let mut reader = ours.finalize_xof();
                    reader.set_position(position);
                    reader.fill(&mut a);
                    let mut official = official.finalize_xof();
                    official.set_position(position);
                    official.fill(&mut b);
                    assert_eq!(a, b, "{what}, extended output from {position}");
                }
                assert_eq!(ours.finalize(), first, "{what}, finalize after the extended output");

                let (mut clone, from) = clone_at.expect("a first step");
                clone.update(&input[from..]);
                assert_eq!(clone.finalize(), want, "{what}, a clone after the first piece");
                assert_eq!(ours.finalize(), first, "{what}, the original after its clone fed on");

                ours.update(more);
                let mut whole = input.to_vec();
                whole.extend_from_slice(more);
                assert_eq!(ours.finalize(), expected(mode, &whole), "{what}, fed on after finalizing");
            }
        }
    }
}

/// Many inputs of drawn lengths (empty to past a chunk, so the lanes start and end at different
/// steps and a long one is hashed where it comes), every engine: each digest is the one-shot
/// one.
fn many_agree(tape: &Tape) {
    let engines = engines();
    let engine = engines[tape.index(engines.len(), "engine")];
    let modes = modes();
    let mode = modes[tape.index(modes.len(), "mode")];
    let count = tape.range(0..if cfg!(miri) { 6 } else { 40 }, "inputs") as usize;
    let datas: Vec<Vec<u8>> = (0..count)
        .map(|_| {
            let len = match tape.below(3, "length kind") {
                0 => tape.range(0..65, "a block") as usize,
                1 => tape.range(0..KIB as u64 + 1, "a chunk") as usize,
                _ => tape.range(0..3 * KIB as u64, "past a chunk") as usize,
            };
            let mut data = vec![0; len];
            tape.fill(&mut data, "input");
            data
        })
        .collect();
    let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
    let mut out = vec![[0u8; 32]; count];
    many_checked_on(engine, mode, &inputs, &mut out).expect("a slot for each input");
    for (input, got) in inputs.iter().zip(&out) {
        assert_eq!(*got, expected(mode, input), "{}, {mode:?}, {} B of {count}", name(engine), input.len());
    }
    let mut short = vec![[0u8; 32]; count + 1];
    assert!(many_checked_on(engine, mode, &inputs, &mut short).is_err(), "a count mismatch refused");
}

#[test]
fn every_engine_many_at_a_time() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 8 } else { 2 * Config::DEFAULT.cases }), many_agree);
}

/// The row compression on every instruction set gives the scalar compression's sixteen words
/// on drawn chaining values, blocks, counters (all 64 bits), lengths and flags.
fn rows_agree(tape: &Tape) {
    let cv: [u32; 8] = core::array::from_fn(|_| tape.next_u32("cv"));
    let block: [u32; 16] = core::array::from_fn(|_| tape.next_u32("block"));
    let counter = tape.next_u64("counter");
    let block_len = tape.range(0..65, "block length") as u32;
    let flags = tape.below(128, "flags") as u32;
    let want = compress(&cv, &block, counter, block_len, flags);
    for isa in isas() {
        for degree in [Degree::One, Degree::Eight] {
            let rows = Engine::new(EngineOpts { single: Single::Rows, degree, pair: Pair::Lanes, isa });
            assert_eq!(compress_by(rows, &cv, &block, counter, block_len, flags), want, "{}", name(rows));
        }
        let scalar = Engine::new(EngineOpts { single: Single::Scalar, degree: Degree::One, pair: Pair::Lanes, isa });
        assert_eq!(compress_by(scalar, &cv, &block, counter, block_len, flags), want, "{}", name(scalar));
    }
}

#[test]
fn single_forms_are_the_scalar_one() {
    blaze_sim::check(Config::cases(if cfg!(miri) { 16 } else { 4 * Config::DEFAULT.cases }), rows_agree);
}
