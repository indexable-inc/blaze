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
    assert!(peak_named <= 32, "expected auto-merge to keep open segments tight (peak_named={peak_named})");
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

    // Process 2 performs 30 phase commits under Liveness::Reached, crossing the auto-merge
    // threshold (`auto_merge_open_threshold` = 24) and folding Process 1's unleased segments.
    let mut p2 = Driven::with(sim, Path::new("/sim"), reached_cfg(2, &root), &root).expect("p2 open");
    for i in 0..30u32 {
        let n = 90_000 + i;
        let k = record(n);
        p2.put(k, Namespace::LOCAL, TAG, &value(n)).expect("p2 put");
        p2.commit_with(&[]).expect("p2 commit");
    }

    // Every single unleased, unrooted record from Process 1 MUST still be intact!
    for &(n, k) in &p1_keys {
        let got = p2.get(&k).expect("get").expect("auto-merge during commit must never drop unleased records");
        assert_eq!(p2.got(got).bytes, value(n).as_slice());
    }
}
