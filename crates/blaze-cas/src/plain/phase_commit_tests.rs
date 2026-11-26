//! A command that commits after each phase's puts (blaze-cli engine_check.rs: `commit_puts`
//! after each `put_all`), then its answer: the answer's record and the pointer to it ride the
//! last commit. The claim (rules 43, 26): a crash anywhere in the command (process death alone,
//! and a power cut, at every completion) leaves a home where
//!
//! - every record a reopen reads is the bytes its key was put with (valid by its key);
//! - the answer's pointer reads nothing, or the answer, and then every phase's record and the
//!   answer's are there; never a refusal;
//! - the next request (the whole command again on that home) is refused nothing, and reads
//!   exactly what a fresh build reads.
//!
//! The planted control: the answer and its pointer written by the first phase's commit (the
//! pointer not last). A cut before the second phase's commit leaves an answer standing whose
//! second phase's records are gone, which the oracle sees.

use std::path::Path;

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_mem::Frame;

use super::{
    core::{PlainConfig, Plant, TAGS},
    driven::{Cut, Driven},
    lent::{Lent, LentError, LentPlant},
    test_root,
};
use crate::Key;

/// A process's store, its arena in the run's frame.
type Store<'f> = Lent<Sim, &'f Frame>;

const TAG: Tag = Tag(1);
/// Two phases of eight records each.
const PHASES: [core::ops::Range<u32>; 2] = [0..8, 8..16];

fn cfg(n: u64, root: &Frame) -> PlainConfig<&Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: n * TAGS,
        cap_bytes: u64::MAX,
        boot: 1,
        nonce: 900 + n,
        // A power cut is part of the claim, so the commit syncs (as pointer_tests.rs).
        durable: true,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: super::Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn process(sim: Sim, n: u64, root: &Frame) -> Store<'_> {
    Lent::planted(Driven::planted(sim, Path::new("/sim"), cfg(n, root), root, Plant::None).expect("the store opens"), root, LentPlant::None)
}

fn record(n: u32) -> Key {
    Key(ContentId::of("plain phase commit test record", &n.to_le_bytes()))
}

/// A record's bytes: some larger than a block, one empty.
fn value(n: u32) -> Vec<u8> {
    (0..(n as usize * 37) % 300).map(|i| (n as usize).wrapping_mul(13).wrapping_add(i) as u8).collect()
}

fn answer_name() -> Key {
    Key(ContentId::of("plain phase commit test answer name", b"request"))
}

/// The answer's record: the keys of every record the phases put, in order.
fn answer() -> (Key, Vec<u8>) {
    let mut bytes = Vec::new();
    for n in PHASES.iter().flat_map(|phase| phase.clone()) {
        bytes.extend_from_slice(&record(n).0.0);
    }
    (Key(ContentId::of("plain phase commit test answer", &bytes)), bytes)
}

/// The command: each phase's puts then a commit; then the answer's record, its pointer, and the
/// last commit. `early`: the planted shape, the pointer written by the first phase's commit.
fn command(lent: &Store<'_>, early: bool) -> Result<(), LentError> {
    let (answer_key, answer_bytes) = answer();
    for (at, phase) in PHASES.iter().enumerate() {
        for n in phase.clone() {
            lent.put(&record(n), Namespace::LOCAL, TAG, &value(n))?;
        }
        if early && at == 0 {
            // The plant: the answer and its pointer ride the first phase's commit.
            lent.put(&answer_key, Namespace::LOCAL, TAG, &answer_bytes)?;
            lent.point(TAG, &answer_name(), &answer_key)?;
        }
        lent.commit()?;
    }
    if !early {
        lent.put(&answer_key, Namespace::LOCAL, TAG, &answer_bytes)?;
        lent.point(TAG, &answer_name(), &answer_key)?;
        lent.commit()?;
    }
    Ok(())
}

/// What a home reads: each record's bytes (or absent), the pointer, and the answer's bytes.
type Read = (Vec<Option<Vec<u8>>>, Option<Key>, Option<Vec<u8>>);

fn read(lent: &Store<'_>) -> Result<Read, LentError> {
    let mut records = Vec::new();
    for n in PHASES.iter().flat_map(|phase| phase.clone()) {
        records.push(lent.get(&record(n))?.map(|got| got.bytes.to_vec()));
    }
    let pointed = lent.pointed(TAG, &answer_name())?;
    let answer = match pointed {
        Some(key) => lent.get(&key)?.map(|got| got.bytes.to_vec()),
        None => None,
    };
    Ok((records, pointed, answer))
}

/// Why a reopened home breaks the claim, if it does.
fn broken(read: &Read, reached: bool) -> Option<&'static str> {
    let (records, pointed, answer_read) = read;
    let all = PHASES.iter().flat_map(|phase| phase.clone());
    if records.iter().zip(all.clone()).any(|(got, n)| got.as_ref().is_some_and(|bytes| *bytes != value(n))) {
        return Some("a record read other bytes than its key was put with");
    }
    match pointed {
        None if reached => Some("the command returned, and its answer's pointer reads nothing"),
        None => None,
        Some(key) if *key != answer().0 => Some("the pointer reads a key no commit pointed it at"),
        Some(_) if answer_read.as_deref() != Some(&answer().1[..]) => Some("the pointer reads an answer the store does not hold"),
        Some(_) if records.iter().any(Option::is_none) => Some("the answer stands and a phase's record is gone"),
        Some(_) => None,
    }
}

/// Every cut of the command, each followed by process death alone and by a power cut, then the
/// reopen's oracle and the next request: how many homes broke the claim, of how many cuts. Each
/// cut is a run of its own, its processes' arenas in a frame of that run, given back before the
/// next.
fn torn(early: bool, fresh: &Read) -> (u32, u32) {
    let (mut bad, mut cuts) = (0, 0);
    for at in 0..1024u32 {
        for power in [false, true] {
            let root = test_root();
            let first = process(Sim::new(53, Faults::default()), 1, &root);
            first.cut(Some(Cut::After(at)));
            let reached = command(&first, early).is_ok();
            let mut sim = first.into_store().into_driver();
            // The process is gone either way, and its flip lock with it.
            if power {
                sim.crash();
            } else {
                sim.processes_died();
            }
            let after = process(sim, 2, &root);
            let why = match read(&after) {
                Ok(home) => broken(&home, reached),
                Err(error) => {
                    eprintln!("phase commit: a reopen after a cut at {at} (power {power}) refused a read: {error}");
                    Some("a reopen refused a read")
                }
            };
            // The next request: the whole command again on this home, then what it reads.
            let why = why.or_else(|| match command(&after, false).and_then(|()| read(&after)) {
                Ok(next) if next == *fresh => None,
                Ok(_) => Some("the next request read other than a fresh build"),
                Err(error) => {
                    eprintln!("phase commit: the next request after a cut at {at} (power {power}) was refused: {error}");
                    Some("the next request was refused")
                }
            });
            if let Some(why) = why {
                if !early {
                    eprintln!("phase commit: cut at {at} (power {power}): {why}");
                }
                bad += 1;
            }
            cuts += 1;
            if reached && power {
                // The command finished before this cut: every later cut is the same run.
                return (bad, cuts);
            }
        }
    }
    (bad, cuts)
}

/// Prints `phase commit: N cuts (...)` and `phase commit, planted pointer in a phase's commit:
/// caught (N homes ...)`.
#[test]
fn a_commit_after_each_phase_leaves_a_home_the_next_request_finishes() {
    let fresh = {
        let root = test_root();
        let lent = process(Sim::new(53, Faults::default()), 1, &root);
        command(&lent, false).expect("the command on a fresh home");
        let after = process(lent.into_store().into_driver(), 2, &root);
        read(&after).expect("a fresh build's reads")
    };
    assert_eq!(broken(&fresh, true), None, "the fresh build itself breaks the oracle");
    let (bad, cuts) = torn(false, &fresh);
    assert_eq!(bad, 0, "a cut left a home that breaks the claim");
    assert!(cuts > 8, "the command was cut at too few points ({cuts}) to reach its three commits");
    println!("phase commit: {cuts} cuts (process death and power cut, through three commits); every home read its records by their keys, the answer only with every record, and the next request read as a fresh build");
    let (bad, _) = torn(true, &fresh);
    assert!(bad > 0, "the planted pointer written by a phase's commit went unseen");
    println!("phase commit, planted pointer in a phase's commit: caught ({bad} homes held an answer without its records)");
}

fn reached_cfg(n: u64, root: &Frame) -> PlainConfig<&Frame> {
    let mut c = cfg(n, root);
    c.durable = false;
    c.liveness = super::Liveness::Reached;
    c
}

/// 250+ consecutive phase commits in a single `Liveness::Reached` command (holding a live lease
/// throughout) must keep named segments bounded `<= named_most` at every single commit via
/// automatic lossless tiered merging, preserve every single committed record byte-for-byte, and
/// stay bounded after releasing the lease and running `compact`.
#[test]
fn consecutive_phase_commits_keep_segment_count_bounded_under_named_most() {
    use super::core::ROOT_LIFE_NS;

    let root = test_root();
    let lent = Lent::new(
        Driven::with(Sim::new(77, Faults::default()), Path::new("/sim"), reached_cfg(1, &root), &root).expect("open"),
        &root,
    );
    let mut all_keys = blaze_own::Vec::new_in(&root);
    let mut peak_named = 0u32;
    for phase in 0..300u32 {
        let base = phase * 4;
        for j in 0..4u32 {
            let n = base + j;
            let k = record(n);
            lent.put(&k, Namespace::LOCAL, TAG, &value(n)).expect("put");
            all_keys.push((n, k));
        }
        lent.commit().expect("phase commit");
        let (_, named) = lent.mapped_of_named();
        let most = lent.named_most();
        peak_named = peak_named.max(named);
        assert!(named <= most, "phase {phase}: named {named} segments exceeded named_most {most}");
    }
    assert!(peak_named <= 140, "expected foreground auto-merge safety ceiling (128) to keep open segments well below named_most (peak_named={peak_named})");
    // Every single phase record must read back intact while the lease is still live.
    for &(n, k) in &all_keys {
        let got = lent.get(&k).expect("get").expect("phase record must survive lossless leased tier merge");
        assert_eq!(got.bytes, value(n).as_slice(), "corrupt bytes for record {n}");
    }
    // Root all keys, release lease, commit, and verify reopen reads every record cleanly.
    let mut key_list = blaze_own::Vec::with_capacity_in(all_keys.len(), &root);
    key_list.extend(all_keys.iter().map(|&(_, k)| k));
    lent.root_for("answer", &key_list, ROOT_LIFE_NS).expect("root");
    lent.release_lease();
    lent.commit().expect("final commit");
    let (_, final_named) = lent.mapped_of_named();
    assert!(final_named <= lent.named_most(), "final named {final_named} exceeded named_most");

    let sim = lent.into_store().into_driver();
    let reopen = Lent::new(
        Driven::with(sim, Path::new("/sim"), reached_cfg(2, &root), &root).expect("reopen"),
        &root,
    );
    for &(n, k) in &all_keys {
        let got = reopen.get(&k).expect("reopen get").expect("rooted record must survive");
        assert_eq!(got.bytes, value(n).as_slice());
    }
}

/// A crashed/dead process holding an active lease must NOT stall tiered compaction (neither
/// explicit `compact` nor automatic tier merging across 250+ subsequent phase commits), yet its
/// unrooted records MUST remain intact until `LEASE_NS` expires and MUST be swept once it expires.
#[test]
fn stale_lease_does_not_block_tiered_compaction_and_preserves_leased_records_until_expiry() {
    use core::time::Duration;
    use super::core::{GRACE_NS, LEASE_NS, ROOT_LIFE_NS};

    let root = test_root();
    let sim = Sim::new(88, Faults::default());

    // Process 1 commits enough unrooted records under a live lease that once its lease expires
    // the merged segment is under half live (<50% live) and therefore eligible for sweep.
    let mut dead_keys = blaze_own::Vec::new_in(&root);
    dead_keys.extend((100_000..100_400u32).map(|n| (n, record(n))));
    let sim = {
        let p1 = Driven::with(sim, Path::new("/sim"), reached_cfg(1, &root), &root).expect("p1 open");
        let lent1 = Lent::new(p1, &root);
        for &(n, k) in &dead_keys {
            lent1.put(&k, Namespace::LOCAL, TAG, &value(n)).expect("dead put");
        }
        lent1.commit().expect("dead commit with lease");
        assert_eq!(lent1.leases_len(), 1, "p1 must leave an active lease");
        let mut s = lent1.into_store().into_driver();
        s.processes_died();
        s
    };

    // Process 2 runs 260 phase commits while p1's stale lease is active.
    let mut live_keys = blaze_own::Vec::new_in(&root);
    live_keys.extend((0..260u32).map(|n| (n, record(n))));
    let sim = {
        let lent2 = Lent::new(
            Driven::with(sim, Path::new("/sim"), reached_cfg(2, &root), &root).expect("p2 open"),
            &root,
        );
        for &(n, k) in &live_keys {
            lent2.put(&k, Namespace::LOCAL, TAG, &value(n)).expect("p2 put");
            lent2.commit().expect("p2 phase commit");
            let (_, named) = lent2.mapped_of_named();
            assert!(named <= lent2.named_most(), "phase {n}: stale lease caused {named} > named_most {}", lent2.named_most());
        }
        let mut rooted = blaze_own::Vec::with_capacity_in(live_keys.len(), &root);
        rooted.extend(live_keys.iter().map(|&(_, k)| k));
        lent2.root_for("p2.answer", &rooted, ROOT_LIFE_NS).expect("p2 root");
        lent2.release_lease();
        lent2.commit().expect("p2 final commit");
        // While p1's lease is live, compact() still compacts any tiers > K, preserves p1's records,
        // and does not report spurious housekeeping for retired files held by p1's lease.
        let _ = lent2.compact_foreground(32).expect("compact under stale lease");
        let _ = lent2.seal_foreground().expect("seal after compact");
        let (_, after_named) = lent2.mapped_of_named();
        assert!(after_named <= 8, "tiered compaction under stale lease should collapse segments (got {after_named})");
        for &(n, k) in &dead_keys {
            let got = lent2.get(&k).expect("get dead").expect("leased dead record must be kept before LEASE_NS");
            assert_eq!(got.bytes, value(n).as_slice());
        }
        lent2.into_store().into_driver()
    };

    // Advance past GRACE_NS (still within LEASE_NS): retired files are held by p1's lease, so
    // needs_housekeeping must be false (no spurious background GC spin).
    let mut sim = sim;
    sim.advance(Duration::from_nanos(GRACE_NS + 1_000_000_000));
    let mut sim = {
        let mut check = Driven::with(sim, Path::new("/sim"), reached_cfg(3, &root), &root).expect("check open");
        let now = check.driver().wall_ns();
        assert!(!check.needs_housekeeping(now), "retired files held by active lease must not trigger spurious housekeeping");
        check.into_driver()
    };

    // Advance past LEASE_NS: p1's lease expires and compact sweeps p1's unrooted records cleanly.
    sim.advance(Duration::from_nanos(LEASE_NS));
    let mut gc = Driven::with(sim, Path::new("/sim"), reached_cfg(4, &root), &root).expect("gc open");
    let _ = gc.compact_foreground(32).expect("post-expiry gc");
    for &(n, k) in &live_keys {
        let got = gc.get(&k).expect("get live").expect("rooted record must survive post-expiry gc");
        assert_eq!(gc.got(got).bytes, value(n).as_slice());
    }
    for &(_, k) in &dead_keys {
        assert!(gc.get(&k).expect("get dead after expiry").is_none(), "expired dead lease's unrooted records must be swept");
    }
}

/// Concurrent background GC merging a running writer's leased segments must not cause redundant
/// `carry` or `SweptUnder` when the writer later roots its records and releases its lease.
#[test]
fn concurrent_gc_merging_leased_segments_does_not_trigger_carry_or_swept_under() {
    use core::time::Duration;
    use blaze_core::{Driver, wait::Deadline};
    use super::core::{Plain, ROOT_LIFE_NS};

    let root = test_root();
    let mut sim = Sim::new(99, Faults::default());

    let settle = |sim: &mut Sim, store: &mut Plain<&Frame>, effects: &mut _| {
        let mut done = core::iter::empty().collect();
        for _ in 0..100_000u32 {
            sim.submit(effects);
            if store.ready() && sim.in_flight() == 0 {
                return;
            }
            let now = sim.now();
            sim.poll(&mut done, Deadline::after(now, Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for c in done.drain(..) {
                if store.owns(c.tag) {
                    store.complete(c, wall, effects);
                }
            }
            if store.broken().is_some() {
                return;
            }
        }
        panic!("store did not settle");
    };

    // Writer opens and commits 12 phase segments under its live lease.
    let mut writer = Plain::new(reached_cfg(1, &root), &root);
    let mut weffects = core::iter::empty().collect();
    writer.open_whole(&mut weffects);
    settle(&mut sim, &mut writer, &mut weffects);
    writer.handed();

    let mut keys = blaze_own::Vec::new_in(&root);
    keys.extend((200_000..200_012u32).map(|n| (n, record(n))));
    for &(n, k) in &keys {
        writer.put(k, Namespace::LOCAL, TAG, &value(n)).expect("put");
        let wall = sim.wall_ns();
        writer.commit(&[], wall, &mut weffects).expect("phase commit");
        settle(&mut sim, &mut writer, &mut weffects);
        assert!(matches!(writer.take_committed(), Some(Ok(_))));
    }

    // Concurrent GC opens on the same `sim` while writer's lease is live and merges all 12 leased
    // segments down to 1 segment while preserving every leased record.
    let mut gc = Plain::new(reached_cfg(2, &root), &root);
    let mut geffects = core::iter::empty().collect();
    gc.open_whole(&mut geffects);
    settle(&mut sim, &mut gc, &mut geffects);
    gc.handed();
    assert!(gc.compact(&mut geffects), "gc compact starts");
    settle(&mut sim, &mut gc, &mut geffects);
    let sliced = gc.take_sliced().expect("gc sliced");
    assert!(sliced.segments > 0 && sliced.merged && !sliced.aborted, "gc must merge leased segments: {sliced:?}");
    assert_eq!(gc.segments().len(), 1, "12 leased segments must merge into 1");

    // Writer (still holding its old pre-merge view!) now sets its answer root referencing all 12
    // keys from its now-merged segments, releases its lease, and commits — must succeed with 0
    // carried records and 0 SweptUnder errors!
    let mut key_list = blaze_own::Vec::with_capacity_in(keys.len(), &root);
    key_list.extend(keys.iter().map(|&(_, k)| k));
    writer.root_for("writer.answer", &key_list, ROOT_LIFE_NS).expect("root");
    writer.release_lease();
    let wall = sim.wall_ns();
    writer.commit(&[], wall, &mut weffects).expect("writer final commit");
    settle(&mut sim, &mut writer, &mut weffects);
    match writer.take_committed() {
        Some(Ok(_)) => {}
        other => panic!("writer final commit failed: {other:?}"),
    }
    assert_eq!(writer.carried(), 0, "segments covered by writer's own live lease must not trigger redundant carry");

    // Fresh reader verifies every rooted key reads back cleanly from the merged store.
    let mut reader = Driven::with(sim, Path::new("/sim"), reached_cfg(3, &root), &root).expect("reader open");
    for &(n, k) in &keys {
        let got = reader.get(&k).expect("get").expect("record must be present");
        assert_eq!(reader.got(got).bytes, value(n).as_slice());
    }
}

/// Automatic tiered merging inside `commit_records` (`Plain::fold_tiered_slice`) MUST be strictly
/// lossless (`Keep::All`) even under `Liveness::Reached` for segments whose lease has already been
/// released and whose records are not yet covered by a committed root (e.g. records committed by
/// an earlier phase or another process before its answer root flips).
#[test]
fn auto_tier_merge_during_commit_is_lossless_for_unleased_records() {
    let root = test_root();
    let sim = Sim::new(113, Faults::default());

    // Process 1 writes 6 segments of records without registering a root, releases its lease,
    // and exits (simulating records from an earlier phase/build before root flip).
    let mut p1_keys = blaze_own::Vec::new_in(&root);
    let sim = {
        let mut p1 = Driven::with(sim, Path::new("/sim"), reached_cfg(1, &root), &root).expect("p1 open");
        for i in 0..6u32 {
            let n = 80_000 + i;
            let k = record(n);
            p1.put(k, Namespace::LOCAL, TAG, &value(n)).expect("p1 put");
            p1.commit_with(&[]).expect("p1 commit");
            p1_keys.push((n, k));
        }
        p1.release_lease();
        p1.commit_with(&[]).expect("p1 release commit");
        p1.into_driver()
    };

    // Process 2 performs 130 phase commits under Liveness::Reached, crossing the emergency
    // foreground auto-merge ceiling (`AUTO_TIER_MERGE_FG` = 128) and folding Process 1's unleased
    // segments losslessly.
    let mut p2 = Driven::with(sim, Path::new("/sim"), reached_cfg(2, &root), &root).expect("p2 open");
    for i in 0..130u32 {
        let n = 90_000 + i;
        let k = record(n);
        p2.put(k, Namespace::LOCAL, TAG, &value(n)).expect("p2 put");
        p2.commit_with(&[]).expect("p2 commit");
    }
    assert!(p2.core().segments().len() < 130, "expected emergency foreground auto-merge to fold segments");

    // Every single unleased, unrooted record from Process 1 MUST still be intact!
    for &(n, k) in &p1_keys {
        let got = p2.get(&k).expect("get").expect("auto-merge during commit must never drop unleased records");
        assert_eq!(p2.got(got).bytes, value(n).as_slice());
    }
}

/// Foreground `commit_records` MUST NOT stall on synchronous `fold_tiered_slice` while open
/// segment count is below the emergency ceiling (`Plain::AUTO_TIER_MERGE_FG` = 128), even past
/// the soft background housekeeping threshold (`Plain::AUTO_TIER_MERGE_BG` = 24); post-answer
/// housekeeping (`compact_foreground`) must then fold the tiers cleanly with exact byte-for-byte
/// round-trip preservation.
///
/// Control (`Plant::ForegroundFoldsAtBgThreshold`): folding synchronously at 24 segments is caught
/// by the oracle (rules 43, 53).
#[test]
fn foreground_commits_defer_tiered_fold_below_ceiling_and_housekeeping_folds_cleanly() {
    use super::core::{Plain, ROOT_LIFE_NS};

    const COMMITS: u32 = 40;
    assert!(usize::try_from(COMMITS).unwrap_or(0) > Plain::<&Frame>::AUTO_TIER_MERGE_BG);
    assert!(usize::try_from(COMMITS).unwrap_or(0) < Plain::<&Frame>::AUTO_TIER_MERGE_FG);

    let run = |plant: Plant| {
        let root = test_root();
        let mut store = Driven::planted(Sim::new(119, Faults::default()), Path::new("/sim"), reached_cfg(1, &root), &root, plant).expect("open");
        let mut keys = blaze_own::Vec::new_in(&root);
        let mut sync_folded = false;
        for i in 0..COMMITS {
            let n = 110_000 + i;
            let k = record(n);
            store.put(k, Namespace::LOCAL, TAG, &value(n)).expect("put");
            store.commit_with(&[]).expect("foreground commit");
            keys.push((n, k));
            let expected = usize::try_from(i + 1).unwrap_or(0);
            if store.core().segments().len() < expected {
                sync_folded = true;
            }
        }
        let segs_before_hk = store.core().segments().len();
        let needs_fg = store.core().needs_auto_tier_merge();
        let needs_bg = store.core().needs_auto_tier_merge_bg();
        let now = store.driver().wall_ns();
        let needs_hk = store.needs_housekeeping(now);

        let mut key_list = blaze_own::Vec::with_capacity_in(keys.len(), &root);
        key_list.extend(keys.iter().map(|&(_, k)| k));
        store.root_for("fg.answer", &key_list, ROOT_LIFE_NS).expect("root");
        store.release_lease();
        store.commit_with(&[]).expect("final commit");

        let _ = store.compact_foreground(8).expect("post-answer housekeeping compact");
        let _ = store.seal_foreground().expect("post-answer housekeeping seal");
        let segs_after_hk = store.core().segments().len();
        let sim = store.into_driver();

        let mut reopen = Driven::with(sim, Path::new("/sim"), reached_cfg(2, &root), &root).expect("reopen");
        for &(n, k) in &keys {
            let got = reopen.get(&k).expect("reopen get").expect("record must survive deferred housekeeping fold");
            assert_eq!(reopen.got(got).bytes, value(n).as_slice(), "byte mismatch for record {n}");
        }
        (segs_before_hk, sync_folded, needs_fg, needs_bg, needs_hk, segs_after_hk)
    };

    let (segs_before_hk, sync_folded, needs_fg, needs_bg, needs_hk, segs_after_hk) = run(Plant::None);
    assert!(!sync_folded, "foreground commits below AUTO_TIER_MERGE_FG must not synchronously fold");
    assert_eq!(segs_before_hk, usize::try_from(COMMITS).unwrap_or(0), "all 40 foreground commits must publish without synchronous merging");
    assert!(!needs_fg, "40 segments must remain below emergency foreground ceiling");
    assert!(needs_bg && needs_hk, "40 segments must flag background housekeeping");
    assert!(segs_after_hk <= 4, "post-answer housekeeping must fold 40 tier-0 segments down (got {segs_after_hk})");

    // Planted-bug control: reproducing the old 24-segment foreground stall is caught by the oracle.
    let (planted_segs, planted_folded, _, _, _, _) = run(Plant::ForegroundFoldsAtBgThreshold);
    assert!(planted_folded && planted_segs < usize::try_from(COMMITS).unwrap_or(0), "oracle must catch premature synchronous folding at AUTO_TIER_MERGE_BG");
}

/// Crashing (both process death and durable power cut) at every completion step while an emergency
/// foreground `fold_tiered_slice` + `commit_records` runs at `Plain::AUTO_TIER_MERGE_FG` (128)
/// must never lose any prior committed record or fail with `TooManySegments`, and a subsequent
/// commit + housekeeping pass must converge to byte-identical state (rules 26, 43).
#[test]
fn emergency_foreground_fold_survives_crashes_at_every_cut() {
    use super::core::{Plain, ROOT_LIFE_NS};

    let root = test_root();
    let ceiling = u32::try_from(Plain::<&Frame>::AUTO_TIER_MERGE_FG).unwrap_or(128);
    fn durable_reached<'a>(n: u64, r: &'a Frame) -> PlainConfig<&'a Frame> {
        let mut c = reached_cfg(n, r);
        c.durable = true;
        c
    }

    // Build a durable store right at the emergency foreground ceiling (128 segments, 0 sync folds).
    let mut base_keys = blaze_own::Vec::new_in(&root);
    for i in 0..ceiling {
        let n = 120_000 + i;
        base_keys.push((n, record(n)));
    }
    let seed_sim = |r: &Frame| -> Sim {
        let mut w = Driven::with(Sim::new(121, Faults::default()), Path::new("/sim"), durable_reached(1, r), r).expect("seed open");
        for &(n, k) in &base_keys {
            w.put(k, Namespace::LOCAL, TAG, &value(n)).expect("seed put");
            if n + 1 == 120_000 + ceiling {
                let mut key_list = blaze_own::Vec::with_capacity_in(base_keys.len(), r);
                key_list.extend(base_keys.iter().map(|&(_, key)| key));
                w.root_for("seed.answer", &key_list, ROOT_LIFE_NS).expect("seed root");
                w.release_lease();
            }
            w.commit_with(&[]).expect("seed commit");
        }
        assert_eq!(w.core().segments().len(), usize::try_from(ceiling).unwrap_or(128), "seed must reach ceiling without early sync fold");
        assert!(w.core().needs_auto_tier_merge(), "store at ceiling must arm emergency foreground fold");
        w.into_driver()
    };

    let trigger_n = 120_000 + ceiling;
    let trigger_k = record(trigger_n);
    let mut cuts = 0u32;
    let mut reached_once = false;
    for cut_at in 0..256u32 {
        for power in [false, true] {
            let cut_root = test_root();
            let mut p2 = Driven::with(seed_sim(&cut_root), Path::new("/sim"), durable_reached(2, &cut_root), &cut_root).expect("p2 open");
            p2.map_all().expect("map_all");
            p2.put(trigger_k, Namespace::LOCAL, TAG, &value(trigger_n)).expect("trigger put");
            p2.cut(Some(Cut::After(cut_at)));
            let reached = p2.commit_with(&[]).is_ok();
            let mut sim = p2.into_driver();
            if power {
                sim.crash();
            } else {
                sim.processes_died();
            }
            let mut after = Driven::with(sim, Path::new("/sim"), durable_reached(3, &cut_root), &cut_root).expect("reopen after crash");
            for &(n, k) in &base_keys {
                let got = after.get(&k).expect("get base").expect("base record must survive emergency fold crash");
                assert_eq!(after.got(got).bytes, value(n).as_slice(), "corrupt base record {n} at cut {cut_at} (power={power})");
            }
            if reached {
                let got = after.get(&trigger_k).expect("get trigger").expect("committed trigger record must be readable");
                assert_eq!(after.got(got).bytes, value(trigger_n).as_slice());
                assert!(after.core().segments().len() < usize::try_from(ceiling).unwrap_or(128), "emergency fold must have reduced segment count");
            } else {
                // Next request completes cleanly and converges to exact full state.
                after.put(trigger_k, Namespace::LOCAL, TAG, &value(trigger_n)).expect("retry put");
                after.commit_with(&[]).expect("retry commit");
                let _ = after.compact_foreground(8).expect("post-crash housekeeping");
                let got = after.get(&trigger_k).expect("get after retry").expect("retry record must be readable");
                assert_eq!(after.got(got).bytes, value(trigger_n).as_slice());
                assert!(after.core().segments().len() <= 8, "housekeeping must fold store cleanly after crash recovery");
            }
            cuts += 1;
            if reached && power {
                reached_once = true;
                break;
            }
        }
        if reached_once {
            break;
        }
    }
    assert!(reached_once && cuts > 8, "expected crash cuts across emergency fold + commit (cuts={cuts}, reached={reached_once})");
}

/// Post-reboot foreground commits MUST NOT run `map_all()` + full-segment BLAKE3 verification over
/// unrelated unsealed prior-boot segments; instead `Driven::commit_records` maps and verifies ONLY
/// segments whose Bloom filter matches a key being committed (or segments lacking a Bloom filter),
/// while still detecting and refusing `TwoValues` conflicts on matching keys (rules 21, 43).
///
/// Control (`Plant::CommitMapsAll`): reproducing the old eager `map_all()` hashes all 24 prior-boot
/// unsealed segments on a single disjoint foreground commit and is caught by the oracle.
#[test]
fn post_reboot_foreground_commit_only_maps_bloom_matching_segments_and_refuses_two_values() {
    use blaze_keyed::store::StoreError;

    const SEGS: u32 = 24;
    fn boot_cfg<'a>(n: u64, boot: u64, root: &'a Frame) -> PlainConfig<&'a Frame> {
        let mut c = cfg(n, root);
        c.boot = boot;
        c.durable = false;
        c
    }

    let seed_boot1 = |root: &Frame| -> Sim {
        let mut w = Driven::with(Sim::new(131, Faults::default()), Path::new("/sim"), boot_cfg(1, 1, root), root).expect("boot1 open");
        w.disable_auto_merge();
        for s in 0..SEGS {
            let k = record(300_000 + s);
            w.put(k, Namespace::LOCAL, TAG, &value(300_000 + s)).expect("boot1 put");
            w.commit_with(&[]).expect("boot1 commit");
        }
        assert_eq!(w.core().segments().len(), usize::try_from(SEGS).unwrap_or(24));
        w.into_driver()
    };

    // 1. Disjoint foreground commit after reboot (boot 2): targeted Bloom mapping vs Plant::CommitMapsAll.
    let run_disjoint = |plant: Plant| {
        let root = test_root();
        let sim = seed_boot1(&root);
        let mut w2 = Driven::planted(sim, Path::new("/sim"), boot_cfg(2, 2, &root), &root, plant).expect("boot2 open");
        assert_eq!(w2.mapped_of_named(), (0, SEGS), "lazy open maps nothing");
        assert_eq!(w2.core().checks().full, 0, "lazy open hashes nothing");

        let fresh_n = 399_001u32;
        let fresh_k = record(fresh_n);
        w2.put(fresh_k, Namespace::LOCAL, TAG, &value(fresh_n)).expect("fresh put");
        w2.commit_with(&[]).expect("fresh foreground commit");
        let (mapped, named) = w2.mapped_of_named();
        let full_checks = w2.core().checks().full;
        (mapped, named, full_checks, w2.into_driver())
    };

    let (mapped, named, full_checks, sim_after) = run_disjoint(Plant::None);
    assert_eq!(named, SEGS + 1, "one new segment committed");
    assert!(
        full_checks <= 2 && mapped <= 3,
        "targeted commit must skip unrelated prior-boot segments (mapped={mapped}/{named}, full_checks={full_checks})"
    );

    let (planted_mapped, _, planted_checks, _) = run_disjoint(Plant::CommitMapsAll);
    assert_eq!(
        planted_checks,
        u64::from(SEGS),
        "planted CommitMapsAll must hash every prior-boot unsealed segment (got {planted_checks}, mapped={planted_mapped})"
    );

    // 2. Conflicting TwoValues commit on an existing key in an unmapped prior-boot segment MUST be
    //    caught and refused while STILL leaving unrelated prior-boot segments unmapped/unhashed.
    {
        let root = test_root();
        let mut w3 = Driven::with(sim_after, Path::new("/sim"), boot_cfg(3, 2, &root), &root).expect("boot2 reopen");
        assert_eq!(w3.mapped_of_named(), (0, SEGS + 1));
        let target_n = 300_007u32;
        let target_k = record(target_n);
        let mut bad_bytes = value(target_n);
        bad_bytes.push(0xfe);
        w3.put(target_k, Namespace::LOCAL, TAG, &bad_bytes).expect("conflicting put buffered before map");
        let refused = w3.commit_with(&[]);
        assert!(
            matches!(refused, Err(StoreError::TwoValues(_))),
            "conflicting foreground commit against unmapped prior-boot segment must refuse TwoValues: {refused:?}"
        );
        let (mapped_conflict, named_conflict) = w3.mapped_of_named();
        let checks_conflict = w3.core().checks().full;
        assert!(
            checks_conflict >= 1 && checks_conflict <= 3 && mapped_conflict < named_conflict,
            "conflict check must verify the matching segment without hashing all unrelated segments (mapped={mapped_conflict}/{named_conflict}, checks={checks_conflict})"
        );
    }

    // 3. Missing Bloom sidecar: falls back to mapping Bloom-less segments so TwoValues is still refused.
    {
        let root = test_root();
        let mut sim = seed_boot1(&root);
        sim.delete(&FileName::new("plain/bloom").expect("bloom name"));
        let mut w4 = Driven::with(sim, Path::new("/sim"), boot_cfg(4, 2, &root), &root).expect("boot2 no-bloom open");
        let target_n = 300_011u32;
        let target_k = record(target_n);
        let mut bad_bytes = value(target_n);
        bad_bytes.push(0xaa);
        w4.put(target_k, Namespace::LOCAL, TAG, &bad_bytes).expect("no-bloom conflicting put");
        let refused = w4.commit_with(&[]);
        assert!(
            matches!(refused, Err(StoreError::TwoValues(_))),
            "without Bloom sidecar, conflicting foreground commit must still map and refuse TwoValues: {refused:?}"
        );
        assert_eq!(w4.core().checks().full, u64::from(SEGS), "all Bloom-less prior-boot segments checked");
    }
}

/// Live leases MUST protect retired segments past `GRACE_NS` from being unlinked, while once
/// `LEASE_NS` expires a single GC slice unlinks them immediately (`removed > 0`, `bytes_out == 0`)
/// and preserves every live rooted record byte-for-byte.
///
/// Control (`Plant::LeaseNeverExpires`): expired lease still pins retired files forever (`retired_ready == 0`).
#[test]
fn live_lease_protects_retired_segments_and_expired_lease_unlinks_immediately() {
    use core::time::Duration;
    use super::core::{GRACE_NS, LEASE_NS, ROOT_LIFE_NS};

    assert!(GRACE_NS <= 120 * 1_000_000_000, "GRACE_NS must stay bounded (got {GRACE_NS} ns)");
    assert!(LEASE_NS <= 30 * 60 * 1_000_000_000, "LEASE_NS must stay bounded (got {LEASE_NS} ns)");

    let run = |plant: Plant| {
        let root = test_root();
        let sim = Sim::new(141, Faults::default());

        // Process 1 writes a live rooted record and releases its lease.
        let live_k = record(400_001);
        let sim = {
            let mut p1 = Driven::with(sim, Path::new("/sim"), reached_cfg(1, &root), &root).expect("p1 open");
            p1.put(live_k, Namespace::LOCAL, TAG, &value(400_001)).expect("live put");
            p1.root_for("live.root", &[live_k], ROOT_LIFE_NS).expect("live root");
            p1.release_lease();
            p1.commit_with(&[]).expect("p1 commit");
            p1.into_driver()
        };

        // Process 2 opens and commits 6 small unrooted segments (creating a tier-0 merge candidate
        // AND leaving a live lease when it dies).
        let sim = {
            let mut p2 = Driven::with(sim, Path::new("/sim"), reached_cfg(2, &root), &root).expect("p2 open");
            for i in 0..6u32 {
                let n = 400_100 + i;
                p2.put(record(n), Namespace::LOCAL, TAG, &value(n)).expect("dead put");
                p2.commit_with(&[]).expect("dead commit");
            }
            let mut s = p2.into_driver();
            s.processes_died();
            s
        };

        // GC compacts the 6 tier-0 segments into 1 merged segment, retiring the 6 originals.
        let mut sim = {
            let mut gc = Driven::planted(sim, Path::new("/sim"), reached_cfg(3, &root), &root, plant).expect("gc1 open");
            let ran = gc.compact_foreground(4).expect("gc1 compact");
            assert!(ran.iter().any(|s| s.segments >= 5 && !s.aborted), "tier-0 merge must retire originals: {ran:?}");
            gc.into_driver()
        };

        // Advance past GRACE_NS (still inside LEASE_NS): p2's live lease protects the retired files!
        sim.advance(Duration::from_nanos(GRACE_NS + 1_000_000_000));
        let (ready_during_lease, removed_during_lease, mut sim) = {
            let mut gc = Driven::planted(sim, Path::new("/sim"), reached_cfg(4, &root), &root, plant).expect("gc2 open");
            let plan = gc.plan_gc().expect("plan during lease");
            let ran = gc.compact_foreground(2).expect("compact during lease");
            let removed: u32 = ran.iter().map(|s| s.removed).sum();
            (plan.retired_ready, removed, gc.into_driver())
        };

        // Advance past LEASE_NS: p2's lease expires -> retired files become immediately unlinkable.
        sim.advance(Duration::from_nanos(LEASE_NS + 1_000_000_000));
        let (ready_after_expiry, removed_after_expiry, bytes_out_unlink, sim) = {
            let mut gc = Driven::planted(sim, Path::new("/sim"), reached_cfg(5, &root), &root, plant).expect("gc3 open");
            let plan = gc.plan_gc().expect("plan after expiry");
            // Reuse `plan_gc` cached reachability on a second immediate call.
            let plan2 = gc.plan_gc().expect("plan2 after expiry");
            assert_eq!(plan, plan2, "cached plan_gc must match identically");
            let ran = gc.compact_foreground(1).expect("unlink slice after expiry");
            let removed: u32 = ran.iter().map(|s| s.removed).sum();
            let bytes_out: u64 = ran.iter().map(|s| s.bytes_out).sum();
            (plan.retired_ready, removed, bytes_out, gc.into_driver())
        };

        // Live rooted record must read back intact.
        let mut reader = Driven::with(sim, Path::new("/sim"), reached_cfg(6, &root), &root).expect("reader open");
        let got = reader.get(&live_k).expect("get live").expect("live record must survive");
        assert_eq!(reader.got(got).bytes, value(400_001).as_slice());

        (ready_during_lease, removed_during_lease, ready_after_expiry, removed_after_expiry, bytes_out_unlink)
    };

    let (ready_live, removed_live, ready_exp, removed_exp, bytes_out_unlink) = run(Plant::None);
    assert_eq!(ready_live, 0, "live lease must keep retired_ready == 0 past GRACE_NS");
    assert_eq!(removed_live, 0, "live lease must prevent unlinking retired segments");
    assert!(ready_exp >= 5, "expired lease must expose all retired segments as ready (got {ready_exp})");
    assert!(removed_exp >= 5, "first post-expiry slice must unlink retired segments immediately (got {removed_exp})");
    assert_eq!(bytes_out_unlink, 0, "post-expiry retired unlink must be zero-copy metadata-only");

    // Planted-bug control: `Plant::LeaseNeverExpires` pins retired segments forever.
    let (_, _, planted_ready, planted_removed, _) = run(Plant::LeaseNeverExpires);
    assert_eq!((planted_ready, planted_removed), (0, 0), "oracle must catch non-expiring lease pinning retired segments");
}

/// On a compaction conflict (`slice.aborted`), `Plain` refreshes its manifest in-place and maps
/// only the delta segment(s) written by the concurrent commit, preserving existing `mmap`s and
/// `ReachCache` so the next `compact` slice on the SAME handle succeeds without reopening the store.
#[test]
fn compaction_conflict_refreshes_manifest_in_place_without_reopen() {
    use core::time::Duration;
    use blaze_core::{Driver, wait::Deadline};
    use super::core::{Plain, ROOT_LIFE_NS};

    let root = test_root();
    let mut sim = Sim::new(151, Faults::default());

    let settle = |sim: &mut Sim, store: &mut Plain<&Frame>, effects: &mut _| {
        let mut done = core::iter::empty().collect();
        for _ in 0..100_000u32 {
            sim.submit(effects);
            if store.ready() && sim.in_flight() == 0 {
                return;
            }
            let now = sim.now();
            sim.poll(&mut done, Deadline::after(now, Duration::from_secs(1)));
            let wall = sim.wall_ns();
            for c in done.drain(..) {
                if store.owns(c.tag) {
                    store.complete(c, wall, effects);
                }
            }
            if store.broken().is_some() {
                return;
            }
        }
        panic!("store did not settle");
    };

    // Seed 6 tier-0 segments and a root with a writer, releasing its lease.
    let mut keys = blaze_own::Vec::new_in(&root);
    let mut seed_w = Plain::new(reached_cfg(1, &root), &root);
    let mut weffects = core::iter::empty().collect();
    seed_w.open_whole(&mut weffects);
    settle(&mut sim, &mut seed_w, &mut weffects);
    seed_w.handed();
    for i in 0..6u32 {
        let n = 500_000 + i;
        let k = record(n);
        seed_w.put(k, Namespace::LOCAL, TAG, &value(n)).expect("seed put");
        let wall = sim.wall_ns();
        seed_w.commit(&[], wall, &mut weffects).expect("seed commit");
        settle(&mut sim, &mut seed_w, &mut weffects);
        assert!(matches!(seed_w.take_committed(), Some(Ok(_))));
        keys.push((n, k));
    }
    let mut key_list = blaze_own::Vec::with_capacity_in(keys.len(), &root);
    key_list.extend(keys.iter().map(|&(_, k)| k));
    seed_w.root_for("conflict.root", &key_list, ROOT_LIFE_NS).expect("root");
    seed_w.release_lease();
    let wall = sim.wall_ns();
    seed_w.commit(&[], wall, &mut weffects).expect("root commit");
    settle(&mut sim, &mut seed_w, &mut weffects);
    assert!(matches!(seed_w.take_committed(), Some(Ok(_))));

    // Open two concurrent compactors (`gc1`, `gc2`) on the exact same generation.
    let mut gc1 = Plain::new(reached_cfg(2, &root), &root);
    let mut g1_effects = core::iter::empty().collect();
    gc1.open_whole(&mut g1_effects);
    settle(&mut sim, &mut gc1, &mut g1_effects);
    gc1.handed();

    let mut gc2 = Plain::new(reached_cfg(3, &root), &root);
    let mut g2_effects = core::iter::empty().collect();
    gc2.open_whole(&mut g2_effects);
    settle(&mut sim, &mut gc2, &mut g2_effects);
    gc2.handed();

    let gen_before = gc1.generation();
    let maps_before = gc1.mapped_of_named().0;
    let plan = gc1.plan_gc(sim.wall_ns()).expect("plan_gc populates reach cache");
    assert_eq!(plan.segments, 7);

    // Start `gc1` compacting and advance it until it is about to acquire the flip lock (so its
    // merged copy is already written, `sim.in_flight() == 0`, and its `Effect::Lock` is parked in
    // `g1_effects` — deterministic conflict with zero dropped completions!).
    assert!(gc1.compact(&mut g1_effects));
    let mut done = core::iter::empty().collect();
    let mut parked_lock = false;
    for _ in 0..10_000u32 {
        if g1_effects.iter().any(|e| matches!(e, blaze_core::Effect::Lock { .. })) {
            parked_lock = true;
            break;
        }
        sim.submit(&mut g1_effects);
        if sim.in_flight() == 0 {
            break;
        }
        let now = sim.now();
        sim.poll(&mut done, Deadline::after(now, Duration::from_secs(1)));
        let wall = sim.wall_ns();
        for c in done.drain(..) {
            if gc1.owns(c.tag) {
                gc1.complete(c, wall, &mut g1_effects);
            }
        }
    }
    assert!(parked_lock && sim.in_flight() == 0, "gc1 must pause right before acquiring flip lock");

    // `gc2` runs to completion while `gc1`'s flip lock is parked, merging all 7 segments into 1!
    assert!(gc2.compact(&mut g2_effects));
    settle(&mut sim, &mut gc2, &mut g2_effects);
    let s2 = gc2.take_sliced().expect("gc2 sliced");
    assert!(!s2.aborted && s2.segments >= 6, "gc2 must land merge first: {s2:?}");

    // Now release `gc1`'s parked flip lock: its slice MUST abort AND refresh `gc1` in-place to
    // `gc2`'s new generation!
    settle(&mut sim, &mut gc1, &mut g1_effects);
    let s1 = gc1.take_sliced().expect("gc1 sliced");
    assert!(s1.aborted, "gc1 must detect conflict and abort: {s1:?}");
    assert_eq!(gc1.generation(), gc2.generation(), "aborted compaction must refresh generation in-place without reopen");
    assert!(gc1.generation() > gen_before);
    assert_eq!(gc1.segments().len(), gc2.segments().len(), "refreshed manifest must match latest segments");
    let (mapped_after, named_after) = gc1.mapped_of_named();
    assert_eq!(mapped_after, named_after, "refreshed eager handle must have every latest segment mapped");
    assert!(maps_before >= 6, "pre-conflict mappings existed");

    // Subsequent read & plan on the SAME `gc1` handle work cleanly without reopening!
    let plan_after = gc1.plan_gc(sim.wall_ns()).expect("plan_gc after in-place refresh");
    assert_eq!(plan_after.segments, 1, "refreshed handle sees single merged segment");
    for &(n, k) in &keys {
        let got = gc1.get(&k).expect("record readable on refreshed handle");
        assert_eq!(got.bytes, value(n).as_slice());
    }
}

/// The invariant (rules 26, 43):
/// 1. Under [`CommitDurability::Full`] (`durable: true`), foreground phase commits order every
///    segment and manifest log via [`Effect::Barrier`] (`F_BARRIERFSYNC` on macOS, `fdatasync` on
///    Linux) instead of per-segment/per-log `Effect::Sync` (`F_FULLFSYNC`), flushing all prior
///    barriers together in `Effect::SyncDir` — cutting `F_FULLFSYNC` count while surviving power
///    cuts and process deaths at every completion step.
/// 2. Under [`CommitDurability::Barrier`], foreground commits emit ONLY `Effect::Barrier` before
///    writing the head slot (zero `Effect::Sync` / `Effect::SyncDir`), and after any power cut on
///    a new boot never serve torn or corrupt bytes; the next request converges byte-for-byte.
/// 3. Planted-bug control (`Plant::SkipDataSync`): removing the segment `Effect::Barrier` is
///    caught by the crash oracle under `Sim::crash()`.
#[test]
fn barrier_ordered_commits_cut_full_syncs_and_survive_power_cuts_at_every_step() {
    use blaze_sim::Config;
    use super::core::CommitDurability;

    // 1. Verify Full durability uses `Effect::Barrier` (not `Effect::Sync`) on segments & logs.
    let fresh = {
        let root = test_root();
        let lent = process(Sim::new(161, Faults::default()), 1, &root);
        command(&lent, false).expect("fresh full-durable command");
        let tally = lent.put_tally();
        assert!(
            tally.disk.barriers >= 6,
            "expected >= 6 barriers (3 segments + 3 manifest logs), got {}",
            tally.disk.barriers
        );
        assert_eq!(
            tally.disk.syncs, 0,
            "segments/logs must use Effect::Barrier instead of Effect::Sync (got {} syncs)",
            tally.disk.syncs
        );
        let after = process(lent.into_store().into_driver(), 2, &root);
        read(&after).expect("fresh read")
    };
    assert_eq!(broken(&fresh, true), None);

    // 2. Planted-bug control on Full durability: `Plant::SkipDataSync` omits segment barriers
    //    and MUST be caught by power-cut crash recovery!
    let mut skip_barrier_caught = false;
    for seed in 0..32u64 {
        let root = test_root();
        let mut w = Driven::planted(Sim::new(seed, Faults::default()), Path::new("/sim"), cfg(1, &root), &root, Plant::SkipDataSync).expect("open");
        for n in 0..8u32 {
            w.put(record(600_000 + n), Namespace::LOCAL, TAG, &value(600_000 + n)).expect("put");
        }
        w.commit_with(&[]).expect("planted commit");
        let mut sim = w.into_driver();
        sim.crash();
        let mut r = Driven::with(sim, Path::new("/sim"), cfg(2, &root), &root).expect("reopen");
        for n in 0..8u32 {
            let k = record(600_000 + n);
            match r.get(&k) {
                Ok(Some(got)) if r.got(got).bytes == value(600_000 + n).as_slice() => {}
                _ => {
                    skip_barrier_caught = true;
                    break;
                }
            }
        }
        if skip_barrier_caught {
            break;
        }
    }
    assert!(skip_barrier_caught, "oracle must catch Plant::SkipDataSync (omitted segment Effect::Barrier)");

    // 3. Randomized DST for `CommitDurability::Barrier` (pure F_BARRIERFSYNC, zero F_FULLFSYNC).
    blaze_sim::check(Config::cases(48), |tape| {
        let seed = tape.next_u64("sim seed");
        let cut_at = u32::try_from(tape.int_in(0..=96, "cut completion")).unwrap_or(0);
        let power = tape.coin("power cut");
        let root = test_root();
        let mut base_cfg = cfg(1, &root);
        base_cfg.durable = false;
        let mut driven = Driven::with(Sim::new(seed, Faults::default()), Path::new("/sim"), base_cfg, &root).expect("open");
        driven.set_commit_durability(CommitDurability::Barrier);
        let lent = Lent::new(driven, &root);
        lent.cut(Some(Cut::After(cut_at)));
        let _reached = command(&lent, false).is_ok();
        let tally = lent.put_tally();
        assert_eq!(tally.disk.syncs, 0, "Barrier mode must issue zero Effect::Sync");
        let mut sim = lent.into_store().into_driver();
        let next_boot = if power {
            sim.crash();
            2
        } else {
            sim.processes_died();
            1
        };
        let mut reopen_cfg = cfg(2, &root);
        reopen_cfg.boot = next_boot;
        reopen_cfg.durable = false;
        let mut after_driven = Driven::with(sim, Path::new("/sim"), reopen_cfg, &root).expect("reopen after barrier crash");
        after_driven.set_commit_durability(CommitDurability::Barrier);
        let after = Lent::new(after_driven, &root);
        // Every readable record must match its exact put bytes (never torn/corrupt).
        let (records, pointed, answer_read) = read(&after).expect("reads after barrier crash");
        for (got, n) in records.iter().zip(PHASES.iter().flat_map(|p| p.clone())) {
            if let Some(bytes) = got {
                assert_eq!(bytes.as_slice(), value(n).as_slice(), "torn record {n} after barrier crash");
            }
        }
        if let Some(ans_key) = pointed {
            assert_eq!(ans_key, answer().0, "pointer must name the real answer key");
            if let Some(ans_bytes) = answer_read {
                assert_eq!(ans_bytes.as_slice(), answer().1.as_slice(), "torn answer record after barrier crash");
            }
        }
        // Next request converges byte-for-byte to a fresh build.
        command(&after, false).expect("next request after barrier crash");
        let converged = read(&after).expect("converged read");
        assert_eq!(converged, fresh, "next request after barrier crash must match fresh build");
    });
}

/// Simulated [`PidCheck`] backed by an explicit slice of alive PIDs (rules 14, 75).
struct SimPidCheck<'a> {
    alive: &'a [u32],
    unknown: &'a [u32],
}

impl super::core::PidCheck for SimPidCheck<'_> {
    fn check_pid(&self, pid: u32) -> super::core::PidLiveness {
        if self.alive.contains(&pid) {
            super::core::PidLiveness::Alive
        } else if self.unknown.contains(&pid) {
            super::core::PidLiveness::Unknown
        } else {
            super::core::PidLiveness::Dead
        }
    }
}

/// The invariant (rules 43, 53, 75): same-boot dead-PID lease reclamation MUST:
/// (a) NEVER reap a live process's lease before `LEASE_NS` (control: `Plant::ReapLivePid`),
/// (b) IMMEDIATELY reclaim a provably dead same-boot process's lease so retired segments past
///     `GRACE_NS` unlink without waiting 30-min `LEASE_NS` (control: `Plant::IgnoreDeadPid`),
/// (c) NEVER reap a different-boot or unknown-PID (`pid == 0`) lease before `LEASE_NS`
///     (control: `Plant::ReapOtherBootPid`),
/// (d) ALWAYS preserve every live rooted record byte-for-byte.
#[test]
fn dead_pid_same_boot_lease_reclamation_dst() {
    use core::time::Duration;
    use blaze_sim::Config;
    use super::core::{GRACE_NS, LEASE_NS, ROOT_LIFE_NS};

    const DEAD_PID: u32 = 10_001;
    const LIVE_PID: u32 = 10_002;
    const OTHER_BOOT_PID: u32 = 10_003;
    const UNKNOWN_STATE_PID: u32 = 10_004;

    struct Outcome {
        dead_retired_ready: u32,
        dead_removed: u32,
        dead_reclaimed_count: u32,
        live_lease_kept: bool,
        other_boot_lease_kept: bool,
        unknown_pid_zero_kept: bool,
        unknown_liveness_kept: bool,
        all_expired_after_lease_ns: bool,
    }

    let run = |seed: u64, gc_boot: u64, plant: Plant| -> Outcome {
        let root = test_root();
        let sim = Sim::new(seed, Faults::default());

        let boot_reached = |nonce_id: u64, boot: u64| {
            let mut c = reached_cfg(nonce_id, &root);
            c.boot = boot;
            c
        };

        // 1. Live rooted record (must always survive).
        let rooted_k = record(700_001);
        let sim = {
            let mut w = Driven::with(sim, Path::new("/sim"), boot_reached(1, gc_boot), &root).expect("root writer");
            w.set_pid(20_001);
            w.put(rooted_k, Namespace::LOCAL, TAG, &value(700_001)).expect("put rooted");
            w.root_for("live.root", &[rooted_k], ROOT_LIFE_NS).expect("root");
            w.release_lease();
            w.commit_with(&[]).expect("root commit");
            w.into_driver()
        };

        // 2. Dead same-boot process (`DEAD_PID` on `gc_boot`): commits 6 tier-0 segments and dies
        //    holding a lease (would pin retired segments for 30 min without dead-PID reclamation).
        let sim = {
            let mut dead = Driven::with(sim, Path::new("/sim"), boot_reached(2, gc_boot), &root).expect("dead writer");
            dead.set_pid(DEAD_PID);
            for i in 0..6u32 {
                let n = 700_100 + i;
                dead.put(record(n), Namespace::LOCAL, TAG, &value(n)).expect("dead put");
                dead.commit_with(&[]).expect("dead commit");
            }
            let mut s = dead.into_driver();
            s.processes_died();
            s
        };

        // Merge those 6 tier-0 segments into 1 segment, retiring the 6 originals.
        let sim = {
            let mut comp = Driven::with(sim, Path::new("/sim"), boot_reached(3, gc_boot), &root).expect("comp open");
            comp.set_pid(20_002);
            let ran = comp.compact_foreground(4).expect("merge tier 0");
            assert!(ran.iter().any(|s| s.segments >= 5 && !s.aborted), "tier-0 merge must retire originals");
            comp.into_driver()
        };

        // Advance past GRACE_NS (still 29 minutes BEFORE LEASE_NS!).
        let mut sim = sim;
        sim.advance(Duration::from_nanos(GRACE_NS + 1_000_000_000));

        // 3. Open 4 additional non-dead-reclaimable lease holders AFTER the retirement grace
        //    (so they don't protect the earlier retired files, letting us verify BOTH retired-file
        //    unlinking for DEAD_PID AND exact lease retention for live/other-boot/unknown leases).
        let live_holder_nonce = reached_cfg(4, &root).nonce;
        let other_boot_nonce = reached_cfg(5, &root).nonce;
        let zero_pid_nonce = reached_cfg(6, &root).nonce;
        let unknown_state_nonce = reached_cfg(7, &root).nonce;

        let sim = {
            let mut live_p = Driven::with(sim, Path::new("/sim"), boot_reached(4, gc_boot), &root).expect("live p");
            live_p.set_pid(LIVE_PID);
            live_p.put(record(700_201), Namespace::LOCAL, TAG, &value(700_201)).expect("live put");
            live_p.commit_with(&[]).expect("live commit");
            live_p.into_driver()
        };
        let sim = {
            let mut other_b = Driven::with(sim, Path::new("/sim"), boot_reached(5, gc_boot + 1), &root).expect("other boot");
            other_b.set_pid(OTHER_BOOT_PID);
            other_b.put(record(700_202), Namespace::LOCAL, TAG, &value(700_202)).expect("other boot put");
            other_b.commit_with(&[]).expect("other boot commit");
            other_b.into_driver()
        };
        let sim = {
            let mut zero_p = Driven::with(sim, Path::new("/sim"), boot_reached(6, gc_boot), &root).expect("zero pid");
            // pid left at 0 (unknown PID).
            zero_p.put(record(700_203), Namespace::LOCAL, TAG, &value(700_203)).expect("zero pid put");
            zero_p.commit_with(&[]).expect("zero pid commit");
            zero_p.into_driver()
        };
        let sim = {
            let mut unk_p = Driven::with(sim, Path::new("/sim"), boot_reached(7, gc_boot), &root).expect("unk state");
            unk_p.set_pid(UNKNOWN_STATE_PID);
            unk_p.put(record(700_204), Namespace::LOCAL, TAG, &value(700_204)).expect("unk state put");
            unk_p.commit_with(&[]).expect("unk state commit");
            unk_p.into_driver()
        };

        // 4. Run GC on `gc_boot` with injected `SimPidCheck`:
        //    `LIVE_PID` is Alive, `UNKNOWN_STATE_PID` is Unknown, `DEAD_PID` and `OTHER_BOOT_PID` are Dead.
        let check = SimPidCheck { alive: &[LIVE_PID], unknown: &[UNKNOWN_STATE_PID] };
        let (dead_retired_ready, dead_removed, dead_reclaimed_count, live_lease_kept, other_boot_lease_kept, unknown_pid_zero_kept, unknown_liveness_kept, mut sim) = {
            let mut gc = Driven::planted(sim, Path::new("/sim"), boot_reached(8, gc_boot), &root, plant).expect("gc open");
            gc.set_pid(20_008);
            let reclaimed_count = gc.reclaim_dead_leases(&check);
            let plan = gc.plan_gc().expect("plan_gc after dead-PID reclaim");
            let ran = gc.compact_foreground(4).expect("gc compact after dead-PID reclaim");
            let removed: u32 = ran.iter().map(|s| s.removed).sum();
            let leases = gc.core().leases();
            let has = |holder: u64| leases.iter().any(|(h, _)| *h == holder);
            (
                plan.retired_ready,
                removed,
                reclaimed_count,
                has(live_holder_nonce),
                has(other_boot_nonce),
                has(zero_pid_nonce),
                has(unknown_state_nonce),
                gc.into_driver(),
            )
        };

        // 5. Advance past LEASE_NS: remaining leases expire normally by wall clock.
        sim.advance(Duration::from_nanos(LEASE_NS + 1_000_000_000));
        let all_expired_after_lease_ns = {
            let mut gc2 = Driven::with(sim, Path::new("/sim"), boot_reached(9, gc_boot), &root).expect("gc2 open");
            let _ = gc2.compact_foreground(4).expect("post-expiry compact");
            // Live rooted record MUST still read back byte-for-byte intact.
            let got = gc2.get(&rooted_k).expect("get rooted").expect("rooted record must survive");
            assert_eq!(gc2.got(got).bytes, value(700_001).as_slice());
            gc2.core().leases_len() == 0
        };

        Outcome {
            dead_retired_ready,
            dead_removed,
            dead_reclaimed_count,
            live_lease_kept,
            other_boot_lease_kept,
            unknown_pid_zero_kept,
            unknown_liveness_kept,
            all_expired_after_lease_ns,
        }
    };

    // Randomized simulation across seeds and boot tokens.
    blaze_sim::check(Config::cases(16), |tape| {
        let seed = tape.next_u64("sim seed");
        let gc_boot = tape.int_in(1..=100, "gc boot");
        let out = run(seed, gc_boot, Plant::None);
        assert_eq!(out.dead_reclaimed_count, 1, "only the single dead same-boot PID lease must be reclaimed");
        assert!(out.dead_retired_ready >= 5, "dead same-boot lease must unblock retired files immediately (got {})", out.dead_retired_ready);
        assert!(out.dead_removed >= 5, "GC must immediately unlink retired files after dead-PID reclaim (got {})", out.dead_removed);
        assert!(out.live_lease_kept, "live same-boot PID lease must NEVER be reaped early");
        assert!(out.other_boot_lease_kept, "different-boot lease must NEVER be reaped early");
        assert!(out.unknown_pid_zero_kept, "unknown-PID (pid=0) lease must NEVER be reaped early");
        assert!(out.unknown_liveness_kept, "Unknown-liveness PID lease must NEVER be reaped early");
        assert!(out.all_expired_after_lease_ns, "all leases must expire cleanly once LEASE_NS passes");
    });

    // Planted-bug control 1: `Plant::IgnoreDeadPid` keeps retired segments pinned for 30 min.
    let ignored = run(201, 1, Plant::IgnoreDeadPid);
    assert_eq!(
        (ignored.dead_reclaimed_count, ignored.dead_retired_ready, ignored.dead_removed),
        (0, 0, 0),
        "oracle must catch Plant::IgnoreDeadPid pinning retired segments"
    );

    // Planted-bug control 2: `Plant::ReapLivePid` falsely reaps the live PID's lease.
    let reaped_live = run(202, 1, Plant::ReapLivePid);
    assert!(!reaped_live.live_lease_kept, "oracle must catch Plant::ReapLivePid reaping a live process's lease");

    // Planted-bug control 3: `Plant::ReapOtherBootPid` falsely reaps a different boot's lease.
    let reaped_boot = run(203, 1, Plant::ReapOtherBootPid);
    assert!(!reaped_boot.other_boot_lease_kept, "oracle must catch Plant::ReapOtherBootPid reaping another boot's lease");
}
