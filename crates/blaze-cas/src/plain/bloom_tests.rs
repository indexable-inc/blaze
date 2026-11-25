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
use blaze_own::{Share, Vec as OwnedVec};
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
    // Deliberately build 120 unmerged segments so this test exercises Bloom skipping and
    // root-generation locality across a high-segment-count store.
    w.disable_auto_merge();
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
    // Deliberately keep all 40+ segments separate to verify Bloom backfill across many segments.
    w.disable_auto_merge();
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
    w2.disable_auto_merge();
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

/// Verifies:
/// 1. `needs_housekeeping` flags unsealed local segments so detached GC barrier-seals them,
///    clears once sealed, and post-reboot open proves sealed segments in O(1) (`checks().full == 0`).
/// 2. Out-of-lock `bloom` compaction stages `bloom-tmp-*` via `Effect::WriteUnsynced` strictly
///    OUTSIDE the exclusive `flip` lock (`!holds_flip_lock()`), NEVER writes `bloom` via
///    `WriteUnsynced` under the lock, and ONLY performs an O(1) `Effect::RenameUnsynced` under
///    `flip`.
/// 3. Crashing (`sim.crash()`) at every completion step of out-of-lock bloom compaction leaves
///    a valid store with zero false negatives in both `Driven::get` and `Snapshot::get`.
/// 4. A concurrent writer committing while a compactor stages `bloom-tmp-*` succeeds with zero
///    lock contention, and the compactor cleanly unlinks its stale temp file without clobbering
///    the concurrent writer's appended bloom record.
#[test]
fn out_of_lock_bloom_compaction_crash_safety_and_unsealed_housekeeping() {
    use core::time::Duration;
    use blaze_core::{Driver, Effect, wait::Deadline};
    use super::{bloom::BloomBuilder, core::Plain};

    let root = test_root();
    let bloom_file = FileName::new("plain/bloom").unwrap();

    // Part 1: Unsealed segment triggers `needs_housekeeping`; sealing clears it and gives O(1)
    // post-reboot verification with 0 full BLAKE3 segment checks.
    {
        let mut w = Driven::with(Sim::new(501, Faults::default()), "/sim".as_ref(), cfg(1, 1, &root), &root).expect("open");
        w.put(key(1), Namespace::LOCAL, TAG, &value(1)).expect("put");
        w.commit_with(&[]).expect("commit");
        let now = w.driver().wall_ns();
        assert!(w.needs_housekeeping(now), "unsealed local segment must trigger housekeeping");
        assert!(w.seal().expect("seal"), "seal must seal the newly committed segment");
        let after_seal = w.driver().wall_ns();
        assert!(!w.needs_housekeeping(after_seal), "sealed clean store must not need housekeeping");
        let sim = w.into_driver();

        // Reboot (boot = 2): sealed segment is proved by seal with 0 full checks!
        let mut r = Driven::with(sim, "/sim".as_ref(), cfg(2, 2, &root), &root).expect("reboot open");
        assert_eq!(read_bytes(&mut r, &key(1)), Some(value(1)));
        assert_eq!(r.core().proved_by_seal().len(), 1, "boot 2 must prove sealed segment without re-hashing");
        assert_eq!(r.core().checks().full, 0, "sealed segment must incur 0 full BLAKE3 checks after reboot");
    }

    // Helper: creates a durable 6-segment store whose `plain/bloom` is bloated (> 2x live + 64 KiB)
    // with dead/retired bloom records so the next `compact` triggers full bloom compaction.
    let make_bloated = |seed: u64| -> (Sim, usize) {
        let mut c = cfg(1, 1, &root);
        c.durable = true;
        let mut w = Driven::with(Sim::new(seed, Faults::default()), "/sim".as_ref(), c, &root).expect("open");
        w.disable_auto_merge();
        for seg in 0..6u32 {
            for j in 0..8u32 {
                let id = seg * 10 + j + 1;
                w.put(key(id), Namespace::LOCAL, TAG, &value(id)).expect("put");
            }
            w.commit_with(&[]).expect("commit");
        }
        w.seal().expect("seal");
        let mut sim = w.into_driver();
        // Append > 128 KiB of valid framed dead-segment bloom records so `needs_compact` triggers.
        let mut bloated = OwnedVec::new_in(BLOOM_ALLOC);
        bloated.extend_from_slice(&sim.contents(&bloom_file));
        for dead in 0..16u32 {
            let dead_digest = ContentId::of("dead retired bloom seg", &dead.to_le_bytes());
            let rec = BloomBuilder::new(8_000).finish(&dead_digest);
            bloated.extend_from_slice(&rec);
        }
        let bloated_len = bloated.len();
        sim.change(&bloom_file, &bloated);
        (sim, bloated_len)
    };

    // Part 2: Step `Plain::compact` effect-by-effect and assert bloom staging runs OUTSIDE `flip`
    // lock while ONLY O(1) `RenameUnsynced` runs under the lock.
    {
        let (mut sim, bloated_len) = make_bloated(502);
        let mut c = cfg(2, 1, &root);
        c.durable = true;
        let mut store = Plain::new(c, &root);
        let mut effects = core::default::Default::default();
        let mut done = core::default::Default::default();
        store.open_whole(&mut effects);
        for _ in 0..20_000u32 {
            sim.submit(&mut effects);
            if store.ready() && sim.in_flight() == 0 {
                break;
            }
            let now = sim.now();
            sim.poll(&mut done, Deadline::after(now, Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for comp in done.drain(..) {
                if store.owns(comp.tag) {
                    store.complete(comp, wall, &mut effects);
                }
            }
        }
        assert!(store.ready(), "compactor opened");

        assert!(store.compact(&mut effects), "compact starts");
        let mut staged_file: Option<FileName> = None;
        let mut bloom_renames = 0u32;
        for _ in 0..20_000u32 {
            for eff in effects.iter() {
                match eff {
                    Effect::WriteUnsynced { file, .. } if file.as_str().contains("bloom") => {
                        assert!(!store.holds_flip_lock(), "bloom WriteUnsynced must NEVER run while holding exclusive flip lock: {}", file.as_str());
                        assert!(file.as_str().contains("bloom-tmp-"), "staged bloom must use temp file: {}", file.as_str());
                        staged_file = Some(file.share());
                    }
                    Effect::RenameUnsynced { from, to, .. } if to.as_str().ends_with("/bloom") => {
                        assert!(store.holds_flip_lock(), "atomic bloom rename must run under flip lock");
                        assert!(from.as_str().contains("bloom-tmp-"), "rename source must be staged temp file: {}", from.as_str());
                        bloom_renames += 1;
                    }
                    _ => {}
                }
            }
            sim.submit(&mut effects);
            if store.ready() && sim.in_flight() == 0 {
                break;
            }
            let now = sim.now();
            sim.poll(&mut done, Deadline::after(now, Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for comp in done.drain(..) {
                if store.owns(comp.tag) {
                    store.complete(comp, wall, &mut effects);
                }
            }
        }
        assert!(store.broken().is_none(), "compactor broke: {:?}", store.broken());
        let staged = staged_file.expect("out-of-lock bloom staging must have executed");
        assert_eq!(bloom_renames, 1, "expected exactly 1 atomic rename under flip lock");
        assert!(!sim.exists(&staged), "staged bloom temp file must be consumed by rename");
        let after_len = sim.contents(&bloom_file).len();
        assert!(after_len > 0 && after_len < bloated_len, "compacted bloom ({after_len}) must be non-empty and smaller than bloated ({bloated_len})");

        let lent = Lent::new(Driven::with(sim, "/sim".as_ref(), cfg(3, 1, &root), &root).expect("reopen"), &root);
        let snap = lent.snapshot_in(&root).expect("snapshot");
        for seg in 0..6u32 {
            for j in 0..8u32 {
                let id = seg * 10 + j + 1;
                let want = value(id);
                assert_eq!(lent.get(&key(id)).expect("lent").expect("hit").bytes, want.as_slice());
                assert_eq!(snap.get(&key(id)).expect("snap").expect("hit"), want.as_slice());
            }
        }
    }

    // Part 3: Crash at every completion step `0..48` during out-of-lock bloom compaction -> 0
    // false negatives on reopen, and a subsequent clean compaction produces a compact bloom file.
    for cut_at in 0..48u32 {
        let (sim, bloated_len) = make_bloated(600 + u64::from(cut_at));
        let mut c = cfg(2, 1, &root);
        c.durable = true;
        let mut comp = Driven::with(sim, "/sim".as_ref(), c, &root).expect("comp open");
        comp.cut(Some(Cut::After(cut_at)));
        let finished = comp.compact(1).is_ok();
        let mut sim = comp.into_driver();
        if !finished {
            sim.crash();
        }

        let mut verify_cfg = cfg(3, 2, &root);
        verify_cfg.durable = true;
        let lent = Lent::new(Driven::with(sim, "/sim".as_ref(), verify_cfg, &root).expect("reopen after crash"), &root);
        let snap = lent.snapshot_in(&root).expect("snap after crash");
        for seg in 0..6u32 {
            for j in 0..8u32 {
                let id = seg * 10 + j + 1;
                let want = value(id);
                assert_eq!(lent.get(&key(id)).expect("lent").expect("hit").bytes, want.as_slice(), "cut {cut_at} key {id}");
                assert_eq!(snap.get(&key(id)).expect("snap").expect("hit"), want.as_slice(), "cut {cut_at} snap key {id}");
            }
        }
        drop(snap);
        if finished {
            let sim_after = lent.into_store().into_driver();
            let len_after = sim_after.contents(&bloom_file).len();
            assert!(len_after > 0 && len_after < bloated_len, "completed bloom compaction must shrink bloom");
            break;
        }
    }

    // Part 4: Concurrent writer commits while compactor is in `Phase::StagingBloom` (outside the
    // flip lock) -> writer acquires `flip` immediately with zero contention, compactor detects
    // `base_gen != staged.base_gen`, cleans up its stale temp file after unlock, and preserves
    // both writers' keys.
    {
        let (mut sim, _) = make_bloated(700);
        let mut c1 = cfg(2, 1, &root);
        c1.durable = true;
        let mut comp = Plain::new(c1, &root);
        let mut ceffects = core::default::Default::default();
        let mut done = core::default::Default::default();
        comp.open_whole(&mut ceffects);
        for _ in 0..20_000u32 {
            sim.submit(&mut ceffects);
            if comp.ready() && sim.in_flight() == 0 {
                break;
            }
            let now = sim.now();
            sim.poll(&mut done, Deadline::after(now, Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for comp_done in done.drain(..) {
                if comp.owns(comp_done.tag) {
                    comp.complete(comp_done, wall, &mut ceffects);
                }
            }
        }
        assert!(comp.compact(&mut ceffects));
        let mut staged_file: Option<FileName> = None;
        let mut paused_for_writer = false;
        let concurrent_key = key(777_001);
        for _ in 0..20_000u32 {
            if !paused_for_writer {
                for eff in ceffects.iter() {
                    if let Effect::WriteUnsynced { file, .. } = eff
                        && file.as_str().contains("bloom-tmp-")
                    {
                        assert!(!comp.holds_flip_lock());
                        staged_file = Some(file.share());
                    }
                }
            }
            sim.submit(&mut ceffects);
            if comp.ready() && sim.in_flight() == 0 {
                break;
            }
            let now = sim.now();
            sim.poll(&mut done, Deadline::after(now, Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for comp_done in done.drain(..) {
                if comp.owns(comp_done.tag) {
                    comp.complete(comp_done, wall, &mut ceffects);
                }
            }
            // Right after `BLOOM_STAGE` finishes, `ceffects` holds `Effect::Lock` (not yet submitted).
            // Run a concurrent writer to completion on the exact same `sim` before submitting `comp`'s lock!
            if !paused_for_writer && staged_file.is_some() && !ceffects.is_empty() && ceffects.iter().any(|e| matches!(e, Effect::Lock { .. })) {
                paused_for_writer = true;
                let mut w_cfg = cfg(3, 1, &root);
                w_cfg.durable = true;
                let mut w = Driven::with(sim, "/sim".as_ref(), w_cfg, &root).expect("concurrent writer open");
                w.put(concurrent_key, Namespace::LOCAL, TAG, &value(777_001)).expect("writer put");
                w.commit_with(&[]).expect("concurrent writer commits without lock contention");
                sim = w.into_driver();
            }
        }
        assert!(paused_for_writer, "must have interleaved concurrent writer right after bloom staging");
        assert!(comp.broken().is_none(), "compactor broke: {:?}", comp.broken());
        let staged = staged_file.expect("staged temp file");
        assert!(!sim.exists(&staged), "stale staged bloom temp file must be cleaned up after unlock");

        let mut check = Driven::with(sim, "/sim".as_ref(), cfg(4, 1, &root), &root).expect("final open");
        assert_eq!(read_bytes(&mut check, &concurrent_key), Some(value(777_001)));
        for seg in 0..6u32 {
            for j in 0..8u32 {
                let id = seg * 10 + j + 1;
                assert_eq!(read_bytes(&mut check, &key(id)), Some(value(id)));
            }
        }
    }
}
