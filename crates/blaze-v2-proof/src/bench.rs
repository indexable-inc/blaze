//! `blaze-v2-proof '{"bench": "first-difference"}'` and `'{"bench": "join"}'`: the reference
//! workloads of the proof lane's speed of light (docs/v2/plan/lanes/proof/speed-of-light.md, "The
//! oracle and the harness"), each made in memory, run once and timed, printed as `{"value": MS}`
//! for `harness/v2/bounds.py`. Only the work the bound counts is timed, not making the inputs.

use std::time::Instant;

use blaze_ids::{ContentId, Fingerprint, bits::BitSet};
use blaze_query::{QueryKey, QueryKind};
use blaze_v2_proof::{Rng, Trace, bytes, join, wire::View};
use serde::Deserialize;

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Workload {
    /// Two outputs of [`OUTPUT_BYTES`] that differ in their last byte, so every byte is read:
    /// [`bytes::first_difference_across_cores`].
    FirstDifference,
    /// A trace of [`KEYS`] keys and [`EDGES_PER_KEY`] edges a key, nushell's size, joined with
    /// itself as the oracle joins an incremental build's trace with one from scratch: both
    /// parsed and checked in place ([`View::parse`]), then [`join::under`].
    Join,
}

pub(crate) const OUTPUT_BYTES: usize = 512 << 20;
pub(crate) const KEYS: usize = 5_000_000;
pub(crate) const EDGES_PER_KEY: usize = 10;

/// The milliseconds `workload` takes.
pub(crate) fn run(workload: Workload) -> f64 {
    match workload {
        Workload::FirstDifference => {
            let mut rng = Rng::new(0x5eed);
            // alloc: the bench's inputs, made once a run before the timer starts.
            let a: Vec<u8> = (0..OUTPUT_BYTES / 8).flat_map(|_| rng.next_u64().to_le_bytes()).collect();
            // clone: as above.
            let mut b = a.clone();
            *b.last_mut().expect("the output is not empty") ^= 1;
            let began = Instant::now();
            let found = bytes::first_difference_across_cores(&a, &b);
            let ms = began.elapsed().as_secs_f64() * 1000.0;
            assert_eq!(found, Some(OUTPUT_BYTES - 1), "the bench's outputs differ in their last byte");
            ms
        }
        Workload::Join => {
            let encoded = blaze_protocol::cbor::to_vec(&nushell_sized()).expect("a trace encodes");
            let began = Instant::now();
            let incremental = View::parse(&encoded).expect("the bench's trace parses");
            let scratch = View::parse(&encoded).expect("the bench's trace parses");
            let stale = join::under(&incremental, &scratch, &[]);
            let ms = began.elapsed().as_secs_f64() * 1000.0;
            assert!(stale.is_empty(), "a trace joined with itself has nothing stale");
            ms
        }
    }
}

/// [`KEYS`] keys, each with a result and [`EDGES_PER_KEY`] edges to keys chosen at random
/// (content hashes are), each seeing the fingerprint it reads, so every kept result's edges are
/// all probed; one in a hundred ran.
fn nushell_sized() -> Trace {
    let mut rng = Rng::new(0x7ace);
    let mut keys: Vec<QueryKey> = (0..KEYS)
        .map(|_| {
            let mut input = [0; 32];
            for word in input.chunks_exact_mut(8) {
                word.copy_from_slice(&rng.next_u64().to_le_bytes());
            }
            QueryKey { kind: QueryKind(0x2000 + rng.below(16) as u16), input: ContentId(input) }
        })
        // alloc: the bench's inputs, made once a run before the timer starts.
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let n = keys.len();
    // alloc: as above.
    let fingerprints: Vec<Fingerprint> = (0..n).map(|_| Fingerprint(u128::from(rng.next_u64()))).collect();
    let mut has = BitSet::<u32>::new(n);
    let mut ran = BitSet::<u32>::new(n);
    for i in 0..n as u32 {
        has.insert(i);
        if rng.below(100) == 0 {
            ran.insert(i);
        }
    }
    // alloc: as above.
    let reads: Vec<u32> = (0..n * EDGES_PER_KEY).map(|_| rng.below(n as u64) as u32).collect();
    // alloc: as above.
    let saw = reads.iter().map(|&r| fingerprints[r as usize]).collect();
    // alloc: as above.
    let edges_from = (0..=n).map(|i| (i * EDGES_PER_KEY) as u32).collect();
    Trace { keys, has, ran, fingerprints, edges_from, reads, saw }
}
