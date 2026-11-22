//! [`ContentId::of_array`] and [`ContentId::of_each`]: many inputs' IDs under one domain into
//! the caller's slots, side by side, allocating nothing.
//!
//! Both run blaze-blake3's one side-by-side path ([`blaze_blake3::derive_key_many`]) under the
//! domain's context key, the key [`ContentId::of`] derives under: an input of at most a chunk
//! is one SIMD lane, a longer one is hashed one shot where it comes. So each ID is the bytes
//! [`ContentId::of`] gives that input, whatever the lengths and their order (the tests below
//! hold it on drawn inputs). [`ContentId::of_many`] gives the same IDs into a `Vec` it
//! allocates, through a thread's kept buffers; these take the slots from the caller and keep
//! nothing, so a caller with a fixed count (two or three sibling digests) or its own slots pays
//! no allocation and no thread-local.
//!
//! The meter (D169) is told once a call, the bytes and digests of all its inputs: the totals
//! one [`ContentId::of`] an input would add.

use blaze_blake3::CountMismatch;

use crate::{ContentId, Domain, b3::ContextKey, context_key, hashed};

/// The inputs [`ContentId::of_each`] hashes in one batch into a stack array of digests before
/// copying them to the caller's slots: blaze-blake3's batch, 2 KiB of stack. The copy (32
/// bytes an ID) is there because `ContentId` slots cannot be lent as `[u8; 32]` ones without
/// unsafe, which this crate denies.
const BATCH: usize = blaze_blake3::MANY_DEFAULT;

impl ContentId {
    /// `inputs.map(|it| ContentId::of(domain, it))`, the inputs side by side: for a fixed count
    /// of sibling digests (an item's signature and body, a record's three tables).
    pub fn of_array<const N: usize>(domain: &'static str, inputs: [&[u8]; N]) -> [ContentId; N] {
        array_under(context_key(domain), inputs)
    }

    /// [`ContentId::of_array`] for a [`Domain`].
    pub fn of_array_domain<const N: usize>(domain: &Domain, inputs: [&[u8]; N]) -> [ContentId; N] {
        array_under(domain.key(), inputs)
    }

    /// [`ContentId::of`] of each of `inputs` under `domain`, into the same place of `out`, the
    /// inputs side by side. Refuses, writing nothing, when `out` is not one slot an input.
    pub fn of_each(
        domain: &'static str,
        inputs: &[&[u8]],
        out: &mut [ContentId],
    ) -> Result<(), CountMismatch> {
        each_under(context_key(domain), inputs, out)
    }

    /// [`ContentId::of_each`] for a [`Domain`].
    pub fn of_each_domain(
        domain: &Domain,
        inputs: &[&[u8]],
        out: &mut [ContentId],
    ) -> Result<(), CountMismatch> {
        each_under(domain.key(), inputs, out)
    }
}

/// [`ContentId::of_array`] under the context key `key`.
fn array_under<const N: usize>(key: &ContextKey, inputs: [&[u8]; N]) -> [ContentId; N] {
    hashed(inputs.iter().map(|it| it.len()).sum(), N);
    let mut digests = [[0u8; 32]; N];
    blaze_blake3::derive_key_many(key, &inputs, &mut digests);
    digests.map(ContentId)
}

/// [`ContentId::of_each`] under the context key `key`.
fn each_under(key: &ContextKey, inputs: &[&[u8]], out: &mut [ContentId]) -> Result<(), CountMismatch> {
    if inputs.len() != out.len() {
        return Err(CountMismatch { inputs: inputs.len(), digests: out.len() });
    }
    hashed(inputs.iter().map(|it| it.len()).sum(), inputs.len());
    let mut digests = [[0u8; 32]; BATCH];
    for (inputs, out) in inputs.chunks(BATCH).zip(out.chunks_mut(BATCH)) {
        let digests = &mut digests[..inputs.len()];
        blaze_blake3::derive_key_many(key, inputs, digests);
        for (slot, digest) in out.iter_mut().zip(digests.iter()) {
            *slot = ContentId(*digest);
        }
    }
    Ok(())
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use blaze_sim::{Config, Tape};

    use super::*;
    use crate::Hasher;

    const DOMAINS: [&str; 3] = ["blaze v2 a 1", "", "blaze v2 a domain of some length 7"];

    /// Lengths where a BLAKE3 path changes: a block, a chunk, past one.
    const EDGES: [usize; 10] = [0, 1, 63, 64, 65, 1023, 1024, 1025, 2048, 3000];

    fn cases() -> Config {
        Config::cases(if cfg!(miri) { 4 } else { Config::DEFAULT.cases })
    }

    /// 0 to 80 inputs of 0 to 3000 bytes (300 under Miri), the edges drawn often.
    fn drawn(tape: &Tape) -> Vec<Vec<u8>> {
        let count = tape.index(if cfg!(miri) { 13 } else { 81 }, "inputs");
        let most = if cfg!(miri) { 300 } else { 3000 };
        (0..count)
            .map(|_| {
                let len = match tape.coin("an edge") {
                    true => *tape.pick(&EDGES, "edge").expect("EDGES is not empty"),
                    false => tape.index(most + 1, "any"),
                }
                .min(most);
                let mut data = vec![0; len];
                tape.fill(&mut data, "input");
                data
            })
            .collect()
    }

    /// Each ID of [`ContentId::of_each`] and [`ContentId::of_each_domain`] is the one
    /// [`ContentId::of`] gives its input alone, in the inputs' order.
    #[test]
    fn of_each_is_of_one_at_a_time() {
        blaze_sim::check(cases(), |tape| {
            let datas = drawn(tape);
            let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
            let domain = DOMAINS[tape.index(DOMAINS.len(), "domain")];
            let named = Domain::new(domain);
            let alone: Vec<ContentId> = inputs.iter().map(|it| ContentId::of(domain, it)).collect();
            let mut out = vec![ContentId::default(); inputs.len()];
            ContentId::of_each(domain, &inputs, &mut out).expect("one slot an input");
            assert_eq!(out, alone, "of_each, {domain:?}");
            let mut out = vec![ContentId::default(); inputs.len()];
            ContentId::of_each_domain(&named, &inputs, &mut out).expect("one slot an input");
            assert_eq!(out, alone, "of_each_domain, {domain:?}");
        });
    }

    /// Lengths around every chunk count where the tree changes shape: 1, 2, 4, 8 and 16
    /// chunks, a byte either side, and the odd counts between.
    fn chunk_edges() -> Vec<usize> {
        let mut lens = Vec::new();
        for chunks in [1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17] {
            lens.extend([chunks * 1024 - 1, chunks * 1024, chunks * 1024 + 1]);
        }
        if cfg!(miri) {
            lens.retain(|&len| len <= 3 * 1024);
        }
        lens
    }

    /// Batched IDs of inputs that cross 1, 2, 4, 8 and 16 chunks, against official BLAKE3's
    /// `derive_key` (another implementation) and against the same bytes streamed through
    /// [`Hasher`] in drawn pieces (the lazily merged stack): a wrong tree in either path is a
    /// different key, never a crash.
    #[test]
    fn of_each_across_chunk_edges_is_official_and_streamed() {
        let lens = chunk_edges();
        let most = *lens.iter().max().expect("lengths");
        let bytes: Vec<u8> = (0..most as u32).map(|at| (at.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let inputs: Vec<&[u8]> = lens.iter().map(|&len| &bytes[..len]).collect();
        for domain in DOMAINS {
            let mut out = vec![ContentId::default(); inputs.len()];
            ContentId::of_each(domain, &inputs, &mut out).expect("one slot an input");
            let three: [ContentId; 3] = ContentId::of_array(domain, [inputs[0], inputs[4], inputs[inputs.len() - 1]]);
            assert_eq!(three, [out[0], out[4], out[inputs.len() - 1]], "of_array, {domain:?}");
            for (input, id) in inputs.iter().zip(&out) {
                let official = ContentId(blake3::derive_key(domain, input));
                assert_eq!(*id, official, "of_each against official, {domain:?}, {} B", input.len());
                assert_eq!(ContentId::of(domain, input), official, "of, {domain:?}, {} B", input.len());
            }
        }
        blaze_sim::check(cases(), |tape| {
            let input = inputs[tape.index(inputs.len(), "length")];
            let domain = DOMAINS[tape.index(DOMAINS.len(), "domain")];
            let mut streamed = Hasher::new(domain);
            let mut at = 0;
            while at < input.len() {
                let take = match tape.below(4, "piece kind") {
                    0 => tape.range(0..9, "a few bytes") as usize,
                    1 => tape.range(1..200, "a block or so") as usize,
                    2 => (1usize << tape.below(5, "chunks")) * 1024,
                    _ => input.len() - at,
                }
                .min(input.len() - at);
                streamed.update(&input[at..at + take]);
                at += take;
            }
            let once = streamed.finish();
            assert_eq!(streamed.finish(), once, "finish twice, {domain:?}, {} B", input.len());
            assert_eq!(once, ContentId(blake3::derive_key(domain, input)), "streamed, {domain:?}, {} B", input.len());
        });
    }

    /// `out` one slot short or long: refused by name, nothing written.
    #[test]
    fn of_each_refuses_a_count_mismatch() {
        blaze_sim::check(cases(), |tape| {
            let datas = drawn(tape);
            let inputs: Vec<&[u8]> = datas.iter().map(Vec::as_slice).collect();
            let slots = match tape.coin("short") {
                true if !inputs.is_empty() => inputs.len() - 1,
                _ => inputs.len() + 1,
            };
            let mut out = vec![ContentId([7; 32]); slots];
            let refused = ContentId::of_each("blaze v2 a 1", &inputs, &mut out);
            assert_eq!(refused, Err(CountMismatch { inputs: inputs.len(), digests: slots }));
            assert!(out.iter().all(|id| *id == ContentId([7; 32])), "nothing written");
        });
    }

    /// The first `N` drawn inputs (empty ones past the draw) through [`ContentId::of_array`]
    /// and [`ContentId::of_array_domain`], each equal to [`ContentId::of`] alone.
    fn array_is_of<const N: usize>(tape: &Tape) {
        let datas = drawn(tape);
        let inputs: [&[u8]; N] = core::array::from_fn(|at| datas.get(at).map_or(&[][..], Vec::as_slice));
        let domain = DOMAINS[tape.index(DOMAINS.len(), "domain")];
        let alone = inputs.map(|it| ContentId::of(domain, it));
        assert_eq!(ContentId::of_array(domain, inputs), alone, "of_array::<{N}>");
        assert_eq!(ContentId::of_array_domain(&Domain::new(domain), inputs), alone, "of_array_domain::<{N}>");
    }

    #[test]
    fn of_array_is_of_one_at_a_time() {
        blaze_sim::check(cases(), |tape| {
            array_is_of::<0>(tape);
            array_is_of::<1>(tape);
            array_is_of::<2>(tape);
            array_is_of::<3>(tape);
            array_is_of::<17>(tape);
            array_is_of::<80>(tape);
        });
    }

    /// A warm [`ContentId::of_each`] and [`ContentId::of_array`] allocate nothing, past a
    /// batch's worth of inputs too.
    #[test]
    fn of_each_and_of_array_allocate_nothing() {
        let bytes: Vec<u8> = (0..2_000u32).map(|n| n.to_le_bytes()[0]).collect();
        let count = if cfg!(miri) { 70 } else { 300 };
        let inputs: Vec<&[u8]> = (0..count).map(|n| &bytes[n % 7..n % 7 + n % 1_100]).collect();
        let mut out = vec![ContentId::default(); count];
        ContentId::of_each("blaze v2 a 1", &inputs, &mut out).expect("one slot an input");
        let alone: Vec<ContentId> = inputs.iter().map(|it| ContentId::of("blaze v2 a 1", it)).collect();
        assert_eq!(out, alone);
        blaze_alloc_count::assert_at_most(0, "a warm of_each", || {
            ContentId::of_each("blaze v2 a 1", &inputs, &mut out).expect("one slot an input");
        });
        assert_eq!(out, alone);
        let three = blaze_alloc_count::assert_at_most(0, "a warm of_array", || {
            ContentId::of_array("blaze v2 a 1", [inputs[1], inputs[2], inputs[3]])
        });
        assert_eq!(three, [alone[1], alone[2], alone[3]]);
    }
}
