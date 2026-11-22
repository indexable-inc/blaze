//! [`Snapshot`]'s promise (snapshot.rs), on blaze-core's simulation (rules 43, 64): histories of
//! puts, phase commits, snapshots held by several readers, compactions, crashes (process death
//! and power cuts, one inside a commit), drawn from one tape.
//!
//! The invariant, checked through every live snapshot after every step:
//!
//! - every key committed before the snapshot was taken reads its exact bytes through it, for as
//!   long as it lives (through later puts, commits, and compactions that replace the segments
//!   it lists);
//! - every other key (never put, put and not committed at the take, or committed after it) is
//!   absent through it;
//! - after every snapshot is dropped and a later commit and compaction ran, a NEW snapshot reads
//!   everything committed, and nothing else.
//!
//! A commit cut by a power failure is all or nothing: after the reopen its records are all there
//! or none (read through the `Lent`, the model's independent source), and the model takes what
//! the store kept.
//!
//! The planted controls, each caught by the oracle in its own words: a snapshot that commits the
//! arena first (sees a put not yet committed), a lookup by the key's first eight bytes alone
//! (keys here share their first eight bytes in four groups, so it returns another key's bytes),
//! and a snapshot taken without making the store whole (a lazy reopen's rows not mapped).
//!
//! The model is `value(n)` under `key(n)`, never another store's answer.

use std::{collections::BTreeSet, path::Path};

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_mem::Frame;
use blaze_sim::{Broke, Config as SimConfig, Tape};

use super::super::{
    core::{Liveness, PlainConfig, TAGS, TIER_SEGMENTS_MOST},
    driven::{Cut, Driven},
    lent::Lent,
    test_root,
};
use super::{Snapshot, SnapshotPlant};
use crate::Key;

/// A process's store, its arena in the run's frame.
type Store<'f> = Lent<Sim, &'f Frame>;

const TAG: Tag = Tag(1);
/// The keys a history draws from: `0..KEYS`.
const KEYS: u32 = 48;
/// A fill commits this many segments of one fresh key each: tiny segments all of tier 0, past
/// its `K` ([`TIER_SEGMENTS_MOST`]), so the next compaction merges.
const FILL: u32 = TIER_SEGMENTS_MOST as u32 + 1;
/// At most this many fills a history; the fills' keys are `KEYS..ALL`.
const FILLS_MOST: u32 = 2;
/// Every key a history may put is below `ALL`; `ALL` itself is never put (an absent key).
const ALL: u32 = KEYS + FILL * FILLS_MOST;
/// Keys share their first eight bytes in this many groups (`n % PREFIXES`).
const PREFIXES: u32 = 4;
/// Readers, each holding at most one snapshot.
const READERS: usize = 4;

/// The oracle's words, one per way the invariant breaks (each control names its own).
const OTHER_BYTES: &str = "read another key's bytes, or bytes no key was put with";
const UNCOMMITTED: &str = "a key not committed when the snapshot was taken read through it";
const ABSENT: &str = "a committed key was absent through the snapshot";

/// What a passing run has to have reached at least once.
const REACH: &[&str] = &[
    "a compaction merged segments while a snapshot lived",
    "a key committed after a live snapshot was taken was absent through it",
    "a snapshot was taken right after a reopen",
    "a cut commit was resolved",
];

fn cfg(process: u64, boot: u64, root: &Frame) -> PlainConfig<&Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: process * TAGS,
        cap_bytes: u64::MAX,
        boot,
        nonce: 1300 + process,
        // A power cut is part of the history, so a commit that returned is durable.
        durable: true,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

/// Key `n`: a digest whose first eight bytes are its group's (`n % PREFIXES`), so the index's
/// interpolation prefix collides between keys of one group.
fn key(n: u32) -> Key {
    let mut bytes = ContentId::of("plain snapshot test key", &n.to_le_bytes()).0;
    let prefix = ContentId::of("plain snapshot test prefix", &(n % PREFIXES).to_le_bytes()).0;
    bytes[..8].copy_from_slice(&prefix[..8]);
    Key(ContentId(bytes))
}

/// Record `n`'s one value (rule 21): its number first, so no two keys hold equal bytes; 4 to
/// 303 bytes, some ending inside a block.
fn value(n: u32) -> Vec<u8> {
    let mut bytes = n.to_le_bytes().to_vec();
    bytes.extend((0..(n as usize * 37) % 300).map(|i| (n as usize).wrapping_mul(29).wrapping_add(i) as u8));
    bytes
}

fn open(sim: Sim, process: u64, boot: u64, root: &Frame) -> Result<Store<'_>, Broke> {
    let store = Driven::with(sim, Path::new("/sim"), cfg(process, boot, root), root).map_err(|why| Broke::Oracle(format!("the store did not open in process {process}: {why}")))?;
    Ok(Lent::new(store, root))
}

#[derive(Clone, Debug)]
enum Op {
    /// Puts, then a commit: one phase of the engine.
    Phase(Vec<u32>),
    /// A put left in the arena (committed by a later commit, or lost by a crash).
    Put(u32),
    Commit,
    /// The reader takes a new snapshot (its old one dropped).
    Take(usize),
    /// The reader drops its snapshot.
    Drop(usize),
    /// [`FILL`] phases of one fresh key each (nothing after [`FILLS_MOST`] fills).
    Fill,
    Compact,
    /// The process ends: by its death alone, or by a power cut.
    Crash { power: bool },
    /// A commit cut after this many completions, then a power cut.
    CutCommit(u32),
}

fn draw_key(tape: &Tape) -> u32 {
    tape.below(u64::from(KEYS), "key") as u32
}

fn op(tape: &Tape) -> Op {
    match tape.below(16, "op") {
        0..=4 => Op::Phase(tape.vec(1..=3, "phase keys", draw_key)),
        5 | 6 => Op::Put(draw_key(tape)),
        7 => Op::Commit,
        8 | 9 => Op::Take(tape.index(READERS, "reader")),
        10 => Op::Fill,
        11 => Op::Drop(tape.index(READERS, "reader")),
        12 | 13 => Op::Compact,
        14 => Op::Crash { power: tape.coin("power cut") },
        _ => Op::CutCommit(tape.int_in(0..=40u32, "cut after")),
    }
}

/// A reader's snapshot and the keys committed when it was taken.
struct Reader<'f> {
    snapshot: Snapshot<&'f Frame>,
    committed: BTreeSet<u32>,
}

/// The invariant for one snapshot: `committed` (at its take) read exactly, everything else
/// absent. `now`: what is committed now, to mark a key committed since that it does not see.
fn sweep(tape: &Tape, reader: &Reader<'_>, now: &BTreeSet<u32>) -> Result<(), Broke> {
    for n in 0..=ALL {
        let got = reader.snapshot.get(&key(n)).map_err(|why| Broke::Oracle(format!("a snapshot read of key {n} was refused: {why}")))?;
        let was = reader.committed.contains(&n);
        let why = match got {
            Some(bytes) if bytes != value(n).as_slice() => Some(OTHER_BYTES),
            Some(_) if !was => Some(UNCOMMITTED),
            None if was => Some(ABSENT),
            _ => None,
        };
        if let Some(why) = why {
            return Err(Broke::Oracle(format!("{why} (key {n}, snapshot of generation {}, {} segments)", reader.snapshot.generation(), reader.snapshot.segments())));
        }
        if got.is_none() && !was && now.contains(&n) {
            tape.reached("a key committed after a live snapshot was taken was absent through it");
        }
    }
    Ok(())
}

/// A snapshot of `lent`, its list in the same frame as the store's arena.
fn take<'f>(lent: &Store<'f>, plant: SnapshotPlant) -> Result<Snapshot<&'f Frame>, Broke> {
    lent.snapshot_planted(*lent.alloc(), plant).map_err(|why| Broke::Oracle(format!("a snapshot could not be taken: {why}")))
}

/// The process gone (every snapshot with it), the machine after `power`, the next process.
fn reopen<'f>(lent: Store<'f>, power: bool, process: &mut u64, boot: &mut u64) -> Result<Store<'f>, Broke> {
    let root: &'f Frame = *lent.alloc();
    let mut sim = lent.into_store().into_driver();
    if power {
        sim.crash();
        // A power cut boots the machine again (plain_place_tests.rs `process_in_boot`).
        *boot += 1;
    } else {
        sim.processes_died();
    }
    *process += 1;
    open(sim, *process, *boot, root)
}

/// One history under `plant`: see the module's docs. Each history is a run of its own: its
/// processes' arenas and its snapshots in a frame of that run, given back before the next.
fn history(tape: &Tape, plant: SnapshotPlant) -> Result<(), Broke> {
    let seed = tape.next_u64("sim seed");
    let ops = tape.vec(1..=80, "ops", op);
    let (mut process, mut boot) = (1u64, 1u64);
    let root = test_root();
    let mut lent = open(Sim::new(seed, Faults::default()), process, boot, &root)?;
    let mut committed: BTreeSet<u32> = BTreeSet::new();
    let mut pending: BTreeSet<u32> = BTreeSet::new();
    let mut readers: Vec<Option<Reader<'_>>> = (0..READERS).map(|_| None).collect();
    // No commit, put or compaction since the open: the store may not be whole yet.
    let mut fresh = false;
    let mut fills = 0u32;
    let put = |lent: &Store<'_>, n: u32, committed: &BTreeSet<u32>, pending: &mut BTreeSet<u32>| -> Result<(), Broke> {
        lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|why| Broke::Oracle(format!("a put of key {n} was refused: {why}")))?;
        if !committed.contains(&n) {
            pending.insert(n);
        }
        Ok(())
    };
    for op in ops {
        match op {
            Op::Phase(keys) => {
                for n in keys {
                    put(&lent, n, &committed, &mut pending)?;
                }
                lent.commit().map_err(|why| Broke::Oracle(format!("a phase's commit failed: {why}")))?;
                committed.append(&mut pending);
                fresh = false;
            }
            Op::Put(n) => {
                put(&lent, n, &committed, &mut pending)?;
                fresh = false;
            }
            Op::Commit => {
                lent.commit().map_err(|why| Broke::Oracle(format!("a commit failed: {why}")))?;
                committed.append(&mut pending);
                fresh = false;
            }
            Op::Take(at) => {
                if fresh {
                    tape.reached("a snapshot was taken right after a reopen");
                }
                readers[at] = None;
                readers[at] = Some(Reader { snapshot: take(&lent, plant)?, committed: committed.clone() });
            }
            Op::Drop(at) => readers[at] = None,
            Op::Fill if fills < FILLS_MOST => {
                for i in 0..FILL {
                    put(&lent, KEYS + fills * FILL + i, &committed, &mut pending)?;
                    lent.commit().map_err(|why| Broke::Oracle(format!("a fill's commit failed: {why}")))?;
                    committed.append(&mut pending);
                }
                fills += 1;
                fresh = false;
            }
            Op::Fill => {}
            Op::Compact => {
                let slices = lent.compact(8).map_err(|why| Broke::Oracle(format!("compaction failed: {why}")))?;
                let merged = slices.iter().any(|it| it.segments > 0 && !it.aborted);
                if merged && readers.iter().any(Option::is_some) {
                    tape.reached("a compaction merged segments while a snapshot lived");
                }
                fresh = false;
            }
            Op::Crash { power } => {
                readers.iter_mut().for_each(|reader| *reader = None);
                pending.clear();
                lent = reopen(lent, power, &mut process, &mut boot)?;
                fresh = true;
            }
            Op::CutCommit(after) => {
                readers.iter_mut().for_each(|reader| *reader = None);
                let cut: BTreeSet<u32> = core::mem::take(&mut pending);
                lent.cut(Some(Cut::After(after)));
                let returned = lent.commit().is_ok();
                lent = reopen(lent, true, &mut process, &mut boot)?;
                fresh = true;
                // What the store kept of the cut commit, read through the `Lent` (not a
                // snapshot): all of it, or none, and all of it when the commit returned (durable).
                let mut kept = 0usize;
                for &n in &cut {
                    match lent.get(&key(n)).map_err(|why| Broke::Oracle(format!("a read after a cut commit was refused: {why}")))? {
                        Some(got) if got.bytes != value(n).as_slice() => return Err(Broke::Oracle(format!("after a cut commit, key {n} {OTHER_BYTES}"))),
                        Some(_) => kept += 1,
                        None => {}
                    }
                }
                if kept != 0 && kept != cut.len() {
                    return Err(Broke::Oracle(format!("a cut commit left {kept} of its {} records: a commit is all or nothing", cut.len())));
                }
                if returned && kept != cut.len() {
                    return Err(Broke::Oracle(format!("a commit that returned lost its {} records to a power cut (durable)", cut.len())));
                }
                if kept == cut.len() {
                    committed.extend(cut);
                }
                tape.reached("a cut commit was resolved");
            }
        }
        for reader in readers.iter().flatten() {
            sweep(tape, reader, &committed)?;
        }
    }
    // Every snapshot dropped; a later commit and a compaction; a NEW snapshot reads everything.
    readers.clear();
    lent.commit().map_err(|why| Broke::Oracle(format!("the last commit failed: {why}")))?;
    committed.append(&mut pending);
    lent.compact(8).map_err(|why| Broke::Oracle(format!("the last compaction failed: {why}")))?;
    let last = Reader { snapshot: take(&lent, plant)?, committed: committed.clone() };
    sweep(tape, &last, &committed)
}

/// The invariant: through commits, compactions that replace the segments a live
/// snapshot lists, process deaths and power cuts (one inside a commit), every key committed
/// before a snapshot reads its exact bytes through it and every other key is absent; a new
/// snapshot after a commit and a compaction reads everything committed.
#[test]
fn a_snapshot_reads_what_was_committed_before_it_through_commits_compactions_and_crashes() {
    blaze_sim::check(SimConfig::DEFAULT.must_reach(REACH), |tape| history(tape, SnapshotPlant::None));
}

/// Control: a snapshot that commits the arena first sees a put not committed when it was asked.
#[test]
fn the_oracle_catches_a_snapshot_that_sees_an_uncommitted_put() {
    blaze_sim::control(SimConfig::cases(256), SnapshotPlant::None, SnapshotPlant::CommitsFirst, UNCOMMITTED, history);
}

/// Control: a lookup by the first eight bytes of the key returns another key's bytes when two
/// keys share them.
#[test]
fn the_oracle_catches_a_lookup_by_the_short_prefix() {
    blaze_sim::control(SimConfig::cases(256), SnapshotPlant::None, SnapshotPlant::ShortPrefix, OTHER_BYTES, history);
}

/// Control: a snapshot taken without making the store whole (a lazy reopen maps a segment at
/// its first probe) misses committed keys of the rows not mapped yet.
#[test]
fn the_oracle_catches_a_snapshot_over_a_lazy_store() {
    blaze_sim::control(SimConfig::cases(256), SnapshotPlant::None, SnapshotPlant::SkipsMapAll, ABSENT, history);
}

/// `Snapshot` is read from eight threads at once (std's scoped threads: the `Lent` stays on this
/// one, and only `&Snapshot` crosses), each thread every key in its own order 64 times: each
/// committed key reads its bytes, equal to the `Lent`'s read on this thread; a key put and
/// committed after the take, and a key never put, read absent.
#[test]
fn a_snapshot_is_read_from_eight_threads_at_once() {
    let root = test_root();
    let lent = open(Sim::new(7, Faults::default()), 1, 1, &root).expect("the store opens");
    for phase in 0..4u32 {
        for n in phase * 10..phase * 10 + 10 {
            lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put");
        }
        lent.commit().expect("a phase's commit");
    }
    // Shared by eight threads, the snapshot's list needs a `Sync` allocator, which a frame is
    // not: blaze-mem's class pools, counted to the store's tag. The one test that names them:
    // the tree has no owned `Sync` allocator yet (the missing piece: `Allocator` for
    // `&blaze_mem::Arena`, or a `Headed + Sync` arena handing out `SharedAlloc`).
    let snapshot = lent.snapshot_in(blaze_mem::Kept(blaze_mem::Tag::Store)).expect("a snapshot after the commits");
    // After the take: put and committed, absent through the snapshot.
    for n in 40..KEYS {
        lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put after the snapshot");
    }
    lent.commit().expect("a commit after the snapshot");
    for n in 0..40 {
        let through = snapshot.get(&key(n)).expect("a snapshot read");
        let lent_read = lent.get(&key(n)).expect("a lent read").map(|got| got.bytes);
        assert_eq!(through, lent_read, "key {n}: the snapshot and the Lent disagree");
    }
    let snapshot = &snapshot;
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..8u32)
            .map(|t| {
                scope.spawn(move || {
                    for round in 0..64u32 {
                        for i in 0..=KEYS {
                            let n = (i + t * 5 + round) % (KEYS + 1);
                            let got = snapshot.get(&key(n)).expect("a snapshot read");
                            if n < 40 {
                                assert_eq!(got, Some(value(n).as_slice()), "thread {t}, key {n}");
                            } else {
                                assert_eq!(got, None, "thread {t}, key {n}: committed after the take, or never put");
                            }
                        }
                    }
                })
            })
            .collect();
        for reader in readers {
            reader.join().expect("a reader thread");
        }
    });
    println!("snapshot: 8 threads read {} keys 64 times each through one snapshot of {} segments: every committed key's bytes, every later key absent", KEYS + 1, snapshot.segments());
}
