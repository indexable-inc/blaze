//! Deterministic simulation and invariant tests for the per-segment Bloom filter sidecar
//! (`bloom.rs`) and root-generation / mapped-first warm-read locality (`probe.rs`).
//!
//! Properties proven against an oracle model of committed records:
//! 1. **Zero false negatives**: every committed key (and pointer) reads back equal bytes through
//!    `Driven::get`, `Driven::newest`, `Lent::get_root`, and `Snapshot::get_proved` across
//!    commits, compactions, backfills, I/O faults, and power cuts.
//! 2. **Segment skip rate**: on a 120-segment store, absent-key and tiny-segment lookups map
//!    nearly 0 segments instead of all 120; warm root->pointer->lines + journal-root->checkpoint
//!    lookups map at most 2 segments even when `bloom` is absent (Run-1 upgrade path).
//! 3. **Corrupt/truncated sidecar soundness + planted control**: zeroing filter body bits or
//!    truncating `bloom` at any byte offset never causes a false negative under `Plant::None`;
//!    the planted control `Plant::TrustsCorruptBloom` is caught by the oracle.

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_mem::{Frame, Kept};
use blaze_own::Vec as OwnedVec;
use blaze_sim::{Config, Tape};

use super::{
    bloom::BLOOM_ALLOC,
    core::Plant,
    driven::{Cut, Driven},
    lazy_tests::cfg,
    lent::Lent,
    test_root,
};
use crate::Key;

const TAG: Tag = Tag(1);

fn key(n: u32) -> Key {
    Key(ContentId::of("plain bloom test key", &n.to_le_bytes()))
}

fn value(n: u32) -> OwnedVec<u8, Kept> {
    let len = 48 + (n as usize * 29) % 300;
    let mut out = OwnedVec::with_capacity_in(len, BLOOM_ALLOC);
    for i in 0..len {
        out.push((n as usize).wrapping_mul(19).wrapping_add(i) as u8);
    }
    out
}

fn read_bytes(d: &mut Driven<Sim, &Frame>, k: &Key) -> Option<OwnedVec<u8, Kept>> {
    let found = d.get(k).expect("get succeeds")?;
    let mut out = OwnedVec::new_in(BLOOM_ALLOC);
    out.extend_from_slice(d.got(found).bytes);
    Some(out)
}

/// On a 120-segment store (large old segments + tiny newest answer & journal-checkpoint segments):
/// - With `bloom`, an absent key maps <= 5 of 120 segments (expected ~0.6), and a deep key in
///   a 2-record segment maps <= 2 more segments.
/// - Even with `bloom` completely deleted (Run 1 on a pre-bloom store), warm no-op root reads
///   (`answer_root -> pointer -> lines` and `journal_root -> checkpoint`) map ONLY the 2 tiny
///   segments that hold them (never the 118 larger older segments).
#[test]
fn bloom_skips_non_matching_segments_and_root_hint_maps_two_without_bloom() {
    let root = test_root();
    let mut w = Driven::with(Sim::new(41, Faults::default()), "/sim".as_ref(), cfg(0, 1, &root), &root).expect("writer opens");
    let mut next_id = 1u32;
    // 118 larger older segments (20..40 records each).
    for seg in 0..118u32 {
        let count = 20 + (seg % 21);
        for _ in 0..count {
            w.put(key(next_id), Namespace::LOCAL, TAG, &value(next_id)).unwrap();
            next_id += 1;
        }
        w.commit_with(&[]).unwrap();
    }
    // Segment #119: tiny 3-record answer segment (pointer + lines + answer root).
    let pointer_key = key(next_id);
    let lines_key = key(next_id + 1);
    next_id += 2;
    w.put(pointer_key, Namespace::LOCAL, TAG, &value(900_001)).unwrap();
    w.put(lines_key, Namespace::LOCAL, TAG, &value(900_002)).unwrap();
    w.root("answer", &[pointer_key]).unwrap();
    w.commit_with(&[]).unwrap();

    // Segment #120: tiny 2-record journal checkpoint segment (checkpoint + journal root).
    let checkpoint_key = key(next_id);
    w.put(checkpoint_key, Namespace::LOCAL, TAG, &value(900_003)).unwrap();
    w.root("answer.journal", &[checkpoint_key]).unwrap();
    w.commit_with(&[]).unwrap();
    w.seal().unwrap();

    let sim = w.into_driver();

    // 1. With `bloom` intact: absent key maps almost nothing out of 120 segments.
    let sim = {
        let mut r = Driven::with(sim, "/sim".as_ref(), cfg(1, 1, &root), &root).expect("reader opens");
        assert_eq!(r.mapped_of_named(), (0, 120));
        assert_eq!(read_bytes(&mut r, &key(u32::MAX - 7)), None);
        let (mapped_absent, named) = r.mapped_of_named();
        assert_eq!(named, 120);
        assert!(mapped_absent <= 5, "absent key mapped {mapped_absent} of {named} segments (expected <= 5 with bloom)");

        // Reading a key in the tiny 2-record segment maps at most 2 more segments.
        assert_eq!(read_bytes(&mut r, &checkpoint_key), Some(value(900_003)));
        let (mapped_hit, _) = r.mapped_of_named();
        assert!(mapped_hit <= mapped_absent + 2, "hit mapped too many segments: {mapped_hit}");
        r.into_driver()
    };

    // 2. With `bloom` deleted (Run 1 on an existing pre-bloom store): root-generation locality +
    //    mapped-first probing STILL maps only the 2 tiny segments for the entire warm no-op sequence!
    {
        let mut sim_no_bloom = sim;
        sim_no_bloom.delete(&FileName::new("plain/bloom").unwrap());
        let lent = Lent::new(
            Driven::with(sim_no_bloom, "/sim".as_ref(), cfg(2, 1, &root), &root).expect("reader opens without bloom"),
            &root,
        );
        assert_eq!(lent.mapped_of_named(), (0, 120));
        // Read answer root -> pointer -> lines (all in segment #119 -> 1 mmap!).
        let ans_root = lent.get_root("answer").unwrap().expect("answer root");
        assert_eq!(ans_root.bytes, &pointer_key.0.0);
        assert_eq!(lent.get(&pointer_key).unwrap().expect("pointer").bytes, value(900_001).as_slice());
        assert_eq!(lent.get(&lines_key).unwrap().expect("lines").bytes, value(900_002).as_slice());
        assert_eq!(lent.mapped_of_named(), (1, 120), "answer root + pointer + lines must map only 1 segment");

        // Read journal root -> checkpoint (all in segment #120 -> 1 more mmap!).
        let j_root = lent.get_root("answer.journal").unwrap().expect("journal root");
        assert_eq!(j_root.bytes, &checkpoint_key.0.0);
        assert_eq!(lent.get(&checkpoint_key).unwrap().expect("checkpoint").bytes, value(900_003).as_slice());
        assert_eq!(lent.mapped_of_named(), (2, 120), "warm no-op sequence must map at most 2 of 120 segments even without bloom");
    }
}

/// Deleting `bloom` on an existing store, then running a single commit (which maps all segments)
/// backfills `bloom` for all existing segments; subsequent lazy readers and snapshots skip >95%
/// of segments and have 0 false negatives across compaction too.
#[test]
fn bloom_backfills_existing_store_and_survives_compaction() {
    let root = test_root();
    let mut w = Driven::with(Sim::new(42, Faults::default()), "/sim".as_ref(), cfg(0, 1, &root), &root).expect("writer opens");
    let mut all_keys = OwnedVec::new_in(BLOOM_ALLOC);
    for seg in 0..40u32 {
        for j in 0..10u32 {
            let id = seg * 10 + j + 1;
            w.put(key(id), Namespace::LOCAL, TAG, &value(id)).unwrap();
            all_keys.push(id);
        }
        w.commit_with(&[]).unwrap();
    }
    w.seal().unwrap();
    let mut sim = w.into_driver();

    // Remove bloom to simulate an existing pre-bloom store.
    let bloom_file = FileName::new("plain/bloom").unwrap();
    sim.delete(&bloom_file);
    assert!(sim.contents(&bloom_file).is_empty());

    // One commit on an eager writer backfills `bloom` for every live segment.
    let mut w2 = Driven::with(sim, "/sim".as_ref(), cfg(1, 1, &root), &root).expect("writer2 opens");
    w2.put(key(9999), Namespace::LOCAL, TAG, &value(9999)).unwrap();
    all_keys.push(9999);
    w2.commit_with(&[]).unwrap();
    w2.seal().unwrap();

    let sim = w2.into_driver();
    assert!(!sim.contents(&bloom_file).is_empty(), "commit must backfill plain/bloom");

    // Lazy reader sees the backfilled bloom: absent key maps <= 3 of 41 segments.
    let sim = {
        let mut r = Driven::with(sim, "/sim".as_ref(), cfg(2, 1, &root), &root).expect("reader opens");
        assert_eq!(read_bytes(&mut r, &key(888_888)), None);
        let (mapped, named) = r.mapped_of_named();
        assert_eq!(named, 41);
        assert!(mapped <= 3, "backfilled bloom mapped {mapped} of {named} on absent key");
        r.into_driver()
    };

    // Compact the store and verify every key is still found through both `Driven` and `Snapshot`.
    let mut c = Driven::with(sim, "/sim".as_ref(), cfg(3, 1, &root), &root).expect("compactor opens");
    let _ = c.compact(8).unwrap();
    c.seal().unwrap();
    let sim_after = c.into_driver();

    let lent = Lent::new(
        Driven::with(sim_after, "/sim".as_ref(), cfg(4, 1, &root), &root).expect("post-compact reader opens"),
        &root,
    );
    let snap = lent.snapshot_in(&root).expect("snapshot succeeds");
    for &id in &all_keys {
        let k = key(id);
        let want = value(id);
        assert_eq!(lent.get(&k).unwrap().expect("lent hit").bytes, want.as_slice(), "lent key {id}");
        assert_eq!(snap.get(&k).unwrap().expect("snapshot hit"), want.as_slice(), "snapshot key {id}");
    }
    assert_eq!(snap.get(&key(777_777)).unwrap(), None);
}

/// Corrupting `bloom` filter body bits (turning 1s to 0s) or truncating `bloom` at any byte
/// never causes a false negative under `Plant::None` (CRC catches corruption and falls back to
/// probing the segment). Under `Plant::TrustsCorruptBloom`, the oracle catches the false negative.
#[test]
fn corrupt_or_truncated_bloom_is_never_trusted_and_plant_is_caught() {
    let root = test_root();
    let mut w = Driven::with(Sim::new(43, Faults::default()), "/sim".as_ref(), cfg(0, 1, &root), &root).expect("writer opens");
    for seg in 0..6u32 {
        for j in 0..8u32 {
            let id = seg * 10 + j + 1;
            w.put(key(id), Namespace::LOCAL, TAG, &value(id)).unwrap();
        }
        w.commit_with(&[]).unwrap();
    }
    w.seal().unwrap();
    let mut sim = w.into_driver();
    let bloom_file = FileName::new("plain/bloom").unwrap();
    let original = sim.contents(&bloom_file);
    assert!(!original.is_empty());

    // Zero all filter body bytes (keeping headers intact) -> guaranteed false negatives if trusted!
    let mut zeroed = OwnedVec::new_in(BLOOM_ALLOC);
    zeroed.extend_from_slice(&original);
    let mut at = 0usize;
    while at + 48 <= zeroed.len() {
        let blocks = u32::from_le_bytes(zeroed[at + 40..at + 44].try_into().unwrap()) as usize;
        let body_end = (at + 48 + blocks * 64).min(zeroed.len());
        zeroed[at + 48..body_end].fill(0);
        at = body_end;
    }

    let check_all = |sim: Sim, plant: Plant, bloom_bytes: &[u8]| -> (Sim, usize, usize) {
        let mut s = sim;
        s.change(&bloom_file, bloom_bytes);
        let lent = Lent::new(Driven::planted(s, "/sim".as_ref(), cfg(1, 1, &root), &root, plant).expect("reader opens"), &root);
        // Take Snapshot first on a fresh open so `Snapshot::get` hits `STATE_UNVERIFIED` directly.
        let snap = lent.snapshot_in(&root).expect("snapshot opens");
        let mut snap_misses = 0usize;
        for seg in 0..6u32 {
            for j in 0..8u32 {
                let id = seg * 10 + j + 1;
                if snap.get(&key(id)).expect("snap get") != Some(value(id).as_slice()) {
                    snap_misses += 1;
                }
            }
        }
        drop(snap);
        let mut misses = 0usize;
        for seg in 0..6u32 {
            for j in 0..8u32 {
                let id = seg * 10 + j + 1;
                if lent.get(&key(id)).expect("lent get").map(|g| g.bytes) != Some(value(id).as_slice()) {
                    misses += 1;
                }
            }
        }
        (lent.into_store().into_driver(), misses, snap_misses)
    };

    let (misses, snap_misses);
    (sim, misses, snap_misses) = check_all(sim, Plant::None, &zeroed);
    assert_eq!(misses, 0, "corrupt bloom caused a false negative in Lent");
    assert_eq!(snap_misses, 0, "corrupt bloom caused a false negative in Snapshot");
    let (planted_misses, planted_snap_misses);
    (sim, planted_misses, planted_snap_misses) = check_all(sim, Plant::TrustsCorruptBloom, &zeroed);
    assert!(planted_misses > 0, "planted TrustsCorruptBloom went uncaught in Lent");
    assert!(planted_snap_misses > 0, "planted TrustsCorruptBloom went uncaught in Snapshot");

    // Truncating `bloom` at various offsets (mid-header, mid-body, empty) -> 0 false negatives.
    for cut in [0, 1, 16, 47, 48, 60, original.len() / 2, original.len().saturating_sub(1)] {
        let (cut_misses, cut_snap_misses);
        (sim, cut_misses, cut_snap_misses) = check_all(sim, Plant::None, &original[..cut]);
        assert_eq!(cut_misses, 0, "truncated bloom at {cut} caused a false negative in Lent");
        assert_eq!(cut_snap_misses, 0, "truncated bloom at {cut} caused a false negative in Snapshot");
    }
}

/// Randomized DST: durable commits, compactions, and real power cuts (`sim.crash()`, which
/// tears/drops unsynced `bloom` writes while keeping durable commits) never produce a false
/// negative or wrong value in `Driven::get` or `Snapshot::get_proved`.
#[test]
fn bloom_dst_zero_false_negatives_across_cuts_faults_and_compactions() {
    blaze_sim::check(Config::DEFAULT, |tape: &Tape| {
        let root = test_root();
        let seed = tape.int_in(1..=u64::MAX, "seed");
        let mut sim = Sim::new(seed, Faults::default());
        let mut model = OwnedVec::new_in(BLOOM_ALLOC);
        let mut next_id = 1u32;
        let mut actor = 1u64;
        let mut boot = 1u64;

        let durable_cfg = |actor: u64, boot: u64| {
            let mut c = cfg(actor, boot, &root);
            c.durable = true;
            c
        };

        let rounds = tape.int_in(4..=10u32, "rounds");
        for _ in 0..rounds {
            actor += 1;
            let mut w = Driven::with(sim, "/sim".as_ref(), durable_cfg(actor, boot), &root).expect("open");
            let puts = tape.int_in(1..=6u32, "puts");
            let mut batch_ids = OwnedVec::new_in(BLOOM_ALLOC);
            for _ in 0..puts {
                let id = next_id;
                next_id += 1;
                let v = value(id);
                w.put(key(id), Namespace::LOCAL, TAG, &v).unwrap();
                batch_ids.push(id);
            }
            let do_cut = tape.int_in(0..=3u32, "cut_coin") == 0;
            if do_cut {
                let after = tape.int_in(0..=14u32, "cut_after");
                w.cut(Some(Cut::After(after)));
            }
            let cut_hit = match w.commit_with(&[]) {
                Ok(_) => {
                    for id in batch_ids {
                        model.push(id);
                    }
                    if tape.int_in(0..=2u32, "compact_coin") == 0 {
                        let _ = w.compact(2);
                    }
                    let _ = w.seal();
                    false
                }
                Err(_) => true,
            };
            sim = w.into_driver();
            if do_cut && cut_hit {
                sim.crash();
                boot += 1;
            }

            // Verify every previously committed key is readable lazily AND via Snapshot.
            actor += 1;
            let lent = Lent::new(
                Driven::with(sim, "/sim".as_ref(), durable_cfg(actor, boot), &root).expect("verify open"),
                &root,
            );
            for &id in &model {
                let want = value(id);
                let got = lent.get(&key(id)).expect("lent get").expect("committed key must not be missed");
                assert_eq!(got.bytes, want.as_slice(), "lent bytes mismatch for key {id}");
            }
            let snap = lent.snapshot_in(&root).expect("snapshot");
            for &id in &model {
                let want = value(id);
                let got = snap.get(&key(id)).expect("snap get").expect("committed key missing in snapshot");
                assert_eq!(got, want.as_slice(), "snapshot bytes mismatch for key {id}");
            }
            drop(snap);
            sim = lent.into_store().into_driver();
        }
    });
}
