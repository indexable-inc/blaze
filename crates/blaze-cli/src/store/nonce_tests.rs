//! The store's nonce (main's ruling, 2026-11-16 21:3x): the next draw of the granted `Rng`.
//! The store's promise is that two processes never name a segment alike, so: under the
//! simulation one seed draws equal nonces (replayable) and two seeds different ones; two opens in
//! one process draw different nonces. Control: the planted constant fails the last.
//!
//! Prints `nonce: replayed by its seed, two seeds differ, 64 opens in one process all differ`
//! and `nonce, planted Constant: caught (two opens drew one nonce)`.

use bstd::io::{Io, Limits, SimWorld};

use super::{NoncePlant, draw_nonce};

fn opens(seed: u64, n: usize, plant: NoncePlant) -> std::vec::Vec<u64> {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    (0..n).map(|_| draw_nonce(&mut io.need_rng().expect("the simulation offers a generator"), plant)).collect()
}

fn all_differ(nonces: &[u64]) -> bool {
    let mut sorted = nonces.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).all(|pair| pair[0] != pair[1])
}

#[test]
fn a_nonce_is_replayed_by_its_seed_and_differs_between_opens() {
    assert_eq!(opens(7, 4, NoncePlant::None), opens(7, 4, NoncePlant::None), "one seed drew other nonces");
    assert_ne!(opens(7, 1, NoncePlant::None), opens(8, 1, NoncePlant::None), "two seeds drew one nonce");
    assert!(all_differ(&opens(7, 64, NoncePlant::None)), "two opens in one process drew one nonce");
    println!("nonce: replayed by its seed, two seeds differ, 64 opens in one process all differ");
    assert!(!all_differ(&opens(7, 64, NoncePlant::Constant)), "the planted constant went unseen");
    println!("nonce, planted Constant: caught (two opens drew one nonce)");
}
