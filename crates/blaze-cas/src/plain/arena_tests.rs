//! The shared arena's promise (arena.rs, `Visibility::InArena`), on blaze-core's simulation
//! (rules 43, 64): histories of puts, snapshot takes held by several readers, commits at any
//! point (a checkpoint's or the answer's), process deaths and power cuts (one inside a commit),
//! drawn from one tape. And the seam's equality (rule 78): the two visibilities, given the same
//! puts and the same commit points, write the same segments.
//!
//! The invariant, checked after every step:
//!
//! - through every live snapshot: every key put before its take (committed or not) reads its
//!   exact bytes; every key put after it, and every key never put, is absent (a snapshot taken
//!   at seq N never sees a record of seq N + 1);
//! - through the `Lent`: every key put reads its exact bytes, an uncommitted one in place (the
//!   shared arena's one copy, at a block boundary: `Lend::in_place`);
//! - after a kill (process death or power cut) at any point: the store holds a prefix of whole
//!   commits (every commit that returned, all of a cut commit or none of it, nothing put after
//!   the last), and every record a later process finds by key reads its exact bytes.
//!
//! The planted controls, each caught in its own words: a view that reads what was published by
//! the time of the read, not the take (`SnapshotPlant::SeesLater`), and a take whose block
//! watermark misses the last record's bytes, as an index published before its bytes would
//! leave it (`SnapshotPlant::ShortWatermark`). Chunk reuse while a reader holds the arena has no
//! control here: it cannot be written in safe code (a shared `Arena` has no `&mut`, and a commit
//! starts a new arena), the type is the guard (rule 73).
//!
//! The model is `value(n)` under `key(n)`, never another store's answer.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Namespace, Tag},
    store::StoreError,
};
use blaze_mem::{Frame, Kept};
use blaze_sim::{Broke, Config as SimConfig, Tape};

use super::{
    Visibility,
    core::{Liveness, PlainConfig, Plant, TAGS},
    driven::{Cut, Driven},
    lent::{Lent, LentError},
    snapshot::{Snapshot, SnapshotError, SnapshotPlant},
    test_root,
};
use crate::Key;

type Store<'f> = Lent<Sim, &'f Frame>;

const TAG: Tag = Tag(1);
const KEYS: u32 = 40;
const READERS: usize = 3;

const OTHER_BYTES: &str = "read another key's bytes, or bytes no key was put with";
const LATER: &str = "a snapshot taken at seq N saw a record put after it";
const ABSENT: &str = "a key put before the take was absent through the snapshot";
const UNPUBLISHED: &str = "a record the snapshot's runs name had bytes past its block watermark";
const LOST: &str = "a kill lost a record of a commit that returned, or kept part of a commit";
const KEPT_UNCOMMITTED: &str = "a kill kept a record put after the last commit";

const REACH: &[&str] = &[
    "a key put and not committed was read through a snapshot",
    "a key put after a live snapshot was taken was absent through it",
    "an own put was read in place from the shared arena",
    "a kill lost the puts after the last commit",
    "a cut commit was resolved",
];

const IN_ARENA: Visibility = Visibility::InArena { shared: Kept(blaze_mem::Tag::Store) };

fn cfg(process: u64, boot: u64, root: &Frame, durable: bool) -> PlainConfig<&Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: process * TAGS,
        cap_bytes: u64::MAX,
        boot,
        nonce: 1700 + process,
        durable,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain arena test key", &n.to_le_bytes()))
}

/// Record `n`'s one value: its number first; 4 to 403 bytes, some ending inside a block, some
/// past a block exactly.
fn value(n: u32) -> Vec<u8> {
    let mut bytes = n.to_le_bytes().to_vec();
    bytes.extend((0..(n as usize * 53) % 400).map(|i| (n as usize).wrapping_mul(31).wrapping_add(i) as u8));
    bytes
}

fn open(sim: Sim, process: u64, boot: u64, root: &Frame, visibility: Visibility) -> Result<Store<'_>, Broke> {
    open_durable(sim, process, boot, root, visibility, true)
}

fn open_durable(sim: Sim, process: u64, boot: u64, root: &Frame, visibility: Visibility, durable: bool) -> Result<Store<'_>, Broke> {
    let store = Driven::with(sim, Path::new("/sim"), cfg(process, boot, root, durable), root).map_err(|why| Broke::Oracle(format!("the store did not open in process {process}: {why}")))?;
    Ok(Lent::with_visibility(store, root, visibility))
}

#[derive(Clone, Debug)]
enum Op {
    Put(u32),
    /// A commit: a checkpoint's (by bytes or age) or the answer's; where it falls is drawn.
    Commit,
    Take(usize),
    Drop(usize),
    Crash { power: bool },
    CutCommit(u32),
}

fn op(tape: &Tape) -> Op {
    match tape.below(14, "op") {
        0..=5 => Op::Put(tape.below(u64::from(KEYS), "key") as u32),
        6 | 7 => Op::Commit,
        8..=10 => Op::Take(tape.index(READERS, "reader")),
        11 => Op::Drop(tape.index(READERS, "reader")),
        12 => Op::Crash { power: tape.coin("power cut") },
        _ => Op::CutCommit(tape.int_in(0..=40u32, "cut after")),
    }
}

struct Reader<'f> {
    snapshot: Snapshot<&'f Frame>,
    /// Every key put (committed or not) when it was taken.
    seen: BTreeSet<u32>,
}

fn sweep(tape: &Tape, reader: &Reader<'_>, put: &BTreeSet<u32>, committed: &BTreeSet<u32>) -> Result<(), Broke> {
    for n in 0..=KEYS {
        let got = match reader.snapshot.get(&key(n)) {
            Ok(got) => got,
            Err(SnapshotError::Unpublished { .. }) => return Err(Broke::Oracle(format!("{UNPUBLISHED} (key {n}, arena seen {:?})", reader.snapshot.arena_seen()))),
            Err(why) => return Err(Broke::Oracle(format!("a snapshot read of key {n} was refused: {why}"))),
        };
        let was = reader.seen.contains(&n);
        let why = match got {
            Some(bytes) if bytes != value(n).as_slice() => Some(OTHER_BYTES),
            Some(_) if !was => Some(LATER),
            None if was => Some(ABSENT),
            _ => None,
        };
        if let Some(why) = why {
            return Err(Broke::Oracle(format!("{why} (key {n}, arena seen {:?}, {} segments)", reader.snapshot.arena_seen(), reader.snapshot.segments())));
        }
        if got.is_some() && !committed.contains(&n) {
            tape.reached("a key put and not committed was read through a snapshot");
        }
        if got.is_none() && put.contains(&n) {
            tape.reached("a key put after a live snapshot was taken was absent through it");
        }
    }
    Ok(())
}

/// Through the `Lent`: every key put reads its bytes (an uncommitted one in place, from the
/// shared arena), every other key is absent.
fn through_lent(tape: &Tape, lent: &Store<'_>, put: &BTreeSet<u32>, committed: &BTreeSet<u32>) -> Result<(), Broke> {
    for n in 0..=KEYS {
        let got = lent.get(&key(n)).map_err(|why| Broke::Oracle(format!("a read of key {n} was refused: {why}")))?;
        match got {
            Some(got) if got.bytes != value(n).as_slice() => return Err(Broke::Oracle(format!("through the Lent, key {n} {OTHER_BYTES}"))),
            Some(got) => {
                if !put.contains(&n) {
                    return Err(Broke::Oracle(format!("through the Lent, key {n} read but never put")));
                }
                if got.own {
                    if committed.contains(&n) {
                        return Err(Broke::Oracle(format!("key {n} was read from the arena after its commit")));
                    }
                    let in_place = got.in_place().map_err(|why| Broke::Oracle(format!("an own put of key {n} was not in place under InArena: {why}")))?;
                    if in_place.bytes().as_ptr() != got.bytes.as_ptr() {
                        return Err(Broke::Oracle(format!("key {n}: in place is not the bytes lent")));
                    }
                    tape.reached("an own put was read in place from the shared arena");
                }
            }
            None if put.contains(&n) => return Err(Broke::Oracle(format!("through the Lent, key {n} put and absent"))),
            None => {}
        }
    }
    Ok(())
}

fn take<'f>(lent: &Store<'f>, plant: SnapshotPlant) -> Result<Snapshot<&'f Frame>, Broke> {
    lent.snapshot_planted(*lent.alloc(), plant).map_err(|why| Broke::Oracle(format!("a snapshot could not be taken: {why}")))
}

fn reopen<'f>(lent: Store<'f>, power: bool, process: &mut u64, boot: &mut u64) -> Result<Store<'f>, Broke> {
    let root: &'f Frame = *lent.alloc();
    let mut sim = lent.into_store().into_driver();
    if power {
        sim.crash();
        *boot += 1;
    } else {
        sim.processes_died();
    }
    *process += 1;
    open(sim, *process, *boot, root, IN_ARENA)
}

/// After a kill: the store holds exactly `committed` (a prefix of whole commits) and nothing
/// of `lost` (put after the last commit), and what it finds reads exact bytes.
fn after_kill(lent: &Store<'_>, committed: &BTreeSet<u32>, lost: &BTreeSet<u32>) -> Result<(), Broke> {
    for n in 0..=KEYS {
        let got = lent.get(&key(n)).map_err(|why| Broke::Oracle(format!("a read after a kill of key {n} was refused: {why}")))?;
        match got {
            Some(got) if got.bytes != value(n).as_slice() => return Err(Broke::Oracle(format!("after a kill, key {n} {OTHER_BYTES}"))),
            Some(_) if lost.contains(&n) && !committed.contains(&n) => return Err(Broke::Oracle(format!("{KEPT_UNCOMMITTED} (key {n})"))),
            None if committed.contains(&n) => return Err(Broke::Oracle(format!("{LOST} (key {n})"))),
            _ => {}
        }
    }
    Ok(())
}

fn history(tape: &Tape, plant: SnapshotPlant) -> Result<(), Broke> {
    let seed = tape.next_u64("sim seed");
    let ops = tape.vec(1..=60, "ops", op);
    let (mut process, mut boot) = (1u64, 1u64);
    let root = test_root();
    let mut lent = open(Sim::new(seed, Faults::default()), process, boot, &root, IN_ARENA)?;
    let mut committed: BTreeSet<u32> = BTreeSet::new();
    let mut pending: BTreeSet<u32> = BTreeSet::new();
    let mut readers: Vec<Option<Reader<'_>>> = (0..READERS).map(|_| None).collect();
    for op in ops {
        match op {
            Op::Put(n) => {
                lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|why| Broke::Oracle(format!("a put of key {n} was refused: {why}")))?;
                if !committed.contains(&n) {
                    pending.insert(n);
                }
            }
            Op::Commit => {
                lent.commit().map_err(|why| Broke::Oracle(format!("a commit failed: {why}")))?;
                committed.append(&mut pending);
            }
            Op::Take(at) => {
                readers[at] = None;
                let seen: BTreeSet<u32> = committed.union(&pending).copied().collect();
                readers[at] = Some(Reader { snapshot: take(&lent, plant)?, seen });
            }
            Op::Drop(at) => readers[at] = None,
            Op::Crash { power } => {
                readers.iter_mut().for_each(|reader| *reader = None);
                let lost = core::mem::take(&mut pending);
                lent = reopen(lent, power, &mut process, &mut boot)?;
                after_kill(&lent, &committed, &lost)?;
                if !lost.is_empty() {
                    tape.reached("a kill lost the puts after the last commit");
                }
            }
            Op::CutCommit(after) => {
                readers.iter_mut().for_each(|reader| *reader = None);
                let cut: BTreeSet<u32> = core::mem::take(&mut pending);
                lent.cut(Some(Cut::After(after)));
                let returned = lent.commit().is_ok();
                lent = reopen(lent, true, &mut process, &mut boot)?;
                // All of the cut commit or none of it (all when it returned: durable).
                let mut kept = 0usize;
                for &n in &cut {
                    match lent.get(&key(n)).map_err(|why| Broke::Oracle(format!("a read after a cut commit was refused: {why}")))? {
                        Some(got) if got.bytes != value(n).as_slice() => return Err(Broke::Oracle(format!("after a cut commit, key {n} {OTHER_BYTES}"))),
                        Some(_) => kept += 1,
                        None => {}
                    }
                }
                if (kept != 0 && kept != cut.len()) || (returned && kept != cut.len()) {
                    return Err(Broke::Oracle(format!("{LOST} (a cut commit kept {kept} of {}, returned: {returned})", cut.len())));
                }
                if kept == cut.len() {
                    committed.extend(cut);
                }
                after_kill(&lent, &committed, &BTreeSet::new())?;
                tape.reached("a cut commit was resolved");
            }
        }
        let put: BTreeSet<u32> = committed.union(&pending).copied().collect();
        for reader in readers.iter().flatten() {
            sweep(tape, reader, &put, &committed)?;
        }
        through_lent(tape, &lent, &put, &committed)?;
    }
    Ok(())
}

/// The invariant: under `Visibility::InArena`, every snapshot reads exactly what was put before
/// its take, the `Lent` reads every put (an own one in place), and a kill at any point leaves a
/// prefix of whole commits whose every record reads its exact bytes.
#[test]
fn the_shared_arena_reads_every_put_before_the_take_and_a_kill_keeps_whole_commits() {
    blaze_sim::check(SimConfig::DEFAULT.must_reach(REACH), |tape| history(tape, SnapshotPlant::None));
}

/// Control: a view bounded by nothing it took sees a record put (and sealed) after its take.
#[test]
fn the_oracle_catches_a_snapshot_that_sees_a_later_put() {
    blaze_sim::control(SimConfig::cases(256), SnapshotPlant::None, SnapshotPlant::SeesLater, LATER, history);
}

/// Control: a block watermark short of the last record's bytes (its run published first).
#[test]
fn the_oracle_catches_a_run_published_before_its_bytes() {
    blaze_sim::control(SimConfig::cases(256), SnapshotPlant::None, SnapshotPlant::ShortWatermark, UNPUBLISHED, history);
}

/// Under `InArena`, a snapshot of puts NOT committed is read from eight threads at once (only
/// `&Snapshot` crosses; the `Lent` stays here): 20 keys committed, 20 put after into the shared
/// arena, the take, then 9 more put and sealed by a second take. Through the first snapshot,
/// every key of the 40 reads its bytes (the arena's in place, at a block boundary) and the 9
/// later ones, and a key never put, read absent, on every thread, 64 rounds each.
#[test]
fn an_arena_snapshot_is_read_from_eight_threads_at_once() {
    let root = test_root();
    let lent = open(Sim::new(11, Faults::default()), 1, 1, &root, IN_ARENA).expect("the store opens");
    for n in 0..20 {
        lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put");
    }
    lent.commit().expect("a commit");
    for n in 20..40 {
        lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put to the shared arena");
    }
    let snapshot = lent.snapshot_in(Kept(blaze_mem::Tag::Store)).expect("a snapshot of an arena");
    assert_ne!(snapshot.arena_seen(), (0, 0), "the take saw the shared arena");
    for n in 40..KEYS {
        lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put after the take");
    }
    let later = lent.snapshot_in(Kept(blaze_mem::Tag::Store)).expect("a second take seals the later puts");
    assert_eq!(later.get(&key(KEYS - 1)).expect("a read"), Some(value(KEYS - 1).as_slice()), "the second take sees the later puts");
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
                                if n >= 20 {
                                    assert_eq!(got.expect("read").as_ptr() as usize % 16, 0, "thread {t}, key {n}: an arena record at a block boundary");
                                }
                            } else {
                                assert_eq!(got, None, "thread {t}, key {n}: put after the take, or never put");
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
    println!("arena snapshot: 8 threads read {} keys 64 times each; seen (runs, blocks) {:?}", KEYS + 1, snapshot.arena_seen());
}

/// The seam's equality (rule 78): the same puts and the same commit points through
/// `AfterCommit` and through `InArena` write the same segments (each named by the digest of its
/// bytes, so equal names are equal bytes) and read the same bytes after.
#[test]
fn the_two_visibilities_write_the_same_segments() {
    blaze_sim::check(SimConfig::cases(128), |tape| {
        let seed = tape.next_u64("sim seed");
        let ops = tape.vec(1..=60, "ops", |tape| if tape.below(5, "commit or put") == 0 { None } else { Some(tape.below(u64::from(KEYS), "key") as u32) });
        let root = test_root();
        let after = open(Sim::new(seed, Faults::default()), 1, 1, &root, Visibility::AfterCommit)?;
        let arena = open(Sim::new(seed, Faults::default()), 1, 1, &root, IN_ARENA)?;
        let mut commits = 0u32;
        for op in ops.iter().copied().chain([None]) {
            match op {
                Some(n) => {
                    let a = after.put(&key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|why| Broke::Oracle(format!("AfterCommit put {n}: {why}")))?;
                    let b = arena.put(&key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|why| Broke::Oracle(format!("InArena put {n}: {why}")))?;
                    if a != b {
                        return Err(Broke::Oracle(format!("key {n}: AfterCommit's put said {a:?}, InArena's {b:?}")));
                    }
                }
                None => {
                    let a = after.commit().map_err(|why| Broke::Oracle(format!("AfterCommit commit: {why}")))?;
                    let b = arena.commit().map_err(|why| Broke::Oracle(format!("InArena commit: {why}")))?;
                    if a.segments[..] != b.segments[..] {
                        return Err(Broke::Oracle(format!("commit {commits}: AfterCommit wrote segments {:x?}, InArena {:x?}", &a.segments[..], &b.segments[..])));
                    }
                    commits += 1;
                }
            }
        }
        for n in 0..=KEYS {
            let a = after.get(&key(n)).map_err(|why| Broke::Oracle(format!("AfterCommit get {n}: {why}")))?.map(|got| got.bytes.to_vec());
            let b = arena.get(&key(n)).map_err(|why| Broke::Oracle(format!("InArena get {n}: {why}")))?.map(|got| got.bytes.to_vec());
            if a != b {
                return Err(Broke::Oracle(format!("key {n}: the two visibilities read different bytes after the last commit")));
            }
        }
        Ok(())
    });
}

/// The checkpoint rule changes only where a segment boundary falls (blaze-cli
/// store/plain_place.rs `checkpoint_due`): the same puts, committed at drawn points (any
/// checkpoints) or once at the end, give the same set of (key, bytes) and the same answer
/// pointer, written last.
#[test]
fn checkpoints_at_any_points_give_the_same_records_and_answer_pointer() {
    const ANSWER: u32 = 1000;
    blaze_sim::check(SimConfig::cases(128), |tape| {
        let seed = tape.next_u64("sim seed");
        let puts = tape.vec(1..=50, "puts", |tape| (tape.below(u64::from(KEYS), "key") as u32, tape.coin("a checkpoint after it")));
        let root = test_root();
        let checkpoints = open(Sim::new(seed, Faults::default()), 1, 1, &root, IN_ARENA)?;
        let once = open(Sim::new(seed ^ 1, Faults::default()), 1, 1, &root, IN_ARENA)?;
        let mut boundaries = 0u32;
        for &(n, checkpoint) in &puts {
            for lent in [&checkpoints, &once] {
                lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|why| Broke::Oracle(format!("put {n}: {why}")))?;
            }
            if checkpoint {
                checkpoints.commit().map_err(|why| Broke::Oracle(format!("a checkpoint: {why}")))?;
                boundaries += 1;
            }
        }
        // The answer's pointer: to the last record put, written by the commit after the answer.
        let target = key(puts.last().expect("one put at least").0);
        for lent in [&checkpoints, &once] {
            lent.point(TAG, &key(ANSWER), &target).map_err(|why| Broke::Oracle(format!("the answer's pointer: {why}")))?;
            lent.commit().map_err(|why| Broke::Oracle(format!("the commit after the answer: {why}")))?;
        }
        for n in 0..=KEYS {
            let a = checkpoints.get(&key(n)).map_err(|why| Broke::Oracle(format!("get {n}: {why}")))?.map(|got| got.bytes.to_vec());
            let b = once.get(&key(n)).map_err(|why| Broke::Oracle(format!("get {n}: {why}")))?.map(|got| got.bytes.to_vec());
            if a != b || a.as_ref().is_some_and(|bytes| bytes[..] != value(n)[..]) {
                return Err(Broke::Oracle(format!("key {n}: {boundaries} checkpoints and one commit hold different bytes")));
            }
        }
        let a = checkpoints.pointed(TAG, &key(ANSWER)).map_err(|why| Broke::Oracle(format!("pointed: {why}")))?;
        let b = once.pointed(TAG, &key(ANSWER)).map_err(|why| Broke::Oracle(format!("pointed: {why}")))?;
        if a != b || a != Some(target) {
            return Err(Broke::Oracle(format!("the answer pointer differs: {a:?} after {boundaries} checkpoints, {b:?} after one commit")));
        }
        Ok(())
    });
}

// ---- a root never names what a kill lost ----
//
// The promise a reader of roots relies on (blaze-cli store/take.rs, roots.rs): after any kill
// and the open that follows, EVERY root the store still names has its own record and every key
// it names held, each reading its exact bytes. A root may be lost with its commit; a surviving
// root with a hole is the breach. Histories of puts and commits, each commit setting a root
// (in the same commit as records it names, or in the commit after them), process deaths and
// power cuts at any step (one inside a commit), under both visibilities, durable or not.

const HOLE: &str = "a root that survived a kill names a record the store does not hold";

#[derive(Clone, Debug)]
enum RootOp {
    Put(u32),
    /// A commit, then a root naming every key held: set before the commit (the same commit as
    /// its newest records) or after it (a commit of its own).
    Commit { same: bool },
    Crash { power: bool },
    CutCommit(u32),
}

fn root_op(tape: &Tape) -> RootOp {
    match tape.below(10, "op") {
        0..=4 => RootOp::Put(tape.below(u64::from(KEYS), "key") as u32),
        5..=7 => RootOp::Commit { same: tape.coin("the root in the same commit") },
        8 => RootOp::Crash { power: tape.coin("power cut") },
        _ => RootOp::CutCommit(tape.int_in(0..=40u32, "cut after")),
    }
}

/// The keys `lent` holds now, each read whole (a record read with other bytes is the oracle's).
fn held_now(lent: &Store<'_>) -> Result<BTreeSet<u32>, Broke> {
    let mut held = BTreeSet::new();
    for n in 0..KEYS {
        match lent.get(&key(n)).map_err(|why| Broke::Oracle(format!("a read of key {n} after a kill was refused: {why}")))? {
            Some(got) if got.bytes != value(n).as_slice() => return Err(Broke::Oracle(format!("after a kill, key {n} {OTHER_BYTES}"))),
            Some(_) => {
                held.insert(n);
            }
            None => {}
        }
    }
    Ok(held)
}

/// The oracle after a kill: every root the store names is one this history set, its record
/// is held, and every key it named reads its exact bytes.
fn roots_whole(tape: &Tape, lent: &Store<'_>, set: &BTreeMap<String, BTreeSet<u32>>) -> Result<(), Broke> {
    for (name, record) in lent.roots().iter() {
        let Some(names) = set.get(name.as_str()) else {
            return Err(Broke::Oracle(format!("the store names a root `{name}` this history never set")));
        };
        if lent.get(record).map_err(|why| Broke::Oracle(format!("a read of root `{name}`'s record was refused: {why}")))?.is_none() {
            return Err(Broke::Oracle(format!("{HOLE}: root `{name}`'s own record")));
        }
        for &n in names {
            match lent.get(&key(n)).map_err(|why| Broke::Oracle(format!("a read of key {n} named by root `{name}` was refused: {why}")))? {
                Some(got) if got.bytes == value(n).as_slice() => {}
                Some(_) => return Err(Broke::Oracle(format!("root `{name}` names key {n}, which {OTHER_BYTES}"))),
                None => return Err(Broke::Oracle(format!("{HOLE}: root `{name}` names key {n}"))),
            }
        }
        tape.reached("a root that survived a kill was walked whole");
    }
    Ok(())
}

/// The process gone (by its death, or by a power cut that boots the machine again), the next
/// one opened on the same simulated disk.
fn killed<'f>(lent: Store<'f>, power: bool, process: &mut u64, boot: &mut u64, how: Rooted) -> Result<Store<'f>, Broke> {
    let root: &'f Frame = *lent.alloc();
    let mut sim = lent.into_store().into_driver();
    if power {
        sim.crash();
        *boot += 1;
    } else {
        sim.processes_died();
    }
    *process += 1;
    open_rooted(sim, *process, *boot, root, how)
}

/// How a rooted history's store is made: its visibility, whether it syncs, and whether its
/// open skips D200's unrooting (the control, [`Plant::KeepsHoledRoot`]).
#[derive(Clone, Copy, Debug)]
struct Rooted {
    visibility: Visibility,
    durable: bool,
    keeps_holed_root: bool,
}

fn open_rooted(sim: Sim, process: u64, boot: u64, root: &Frame, how: Rooted) -> Result<Store<'_>, Broke> {
    let plant = if how.keeps_holed_root { Plant::KeepsHoledRoot } else { Plant::None };
    let store = Driven::planted(sim, Path::new("/sim"), cfg(process, boot, root, how.durable), root, plant).map_err(|why| Broke::Oracle(format!("the store did not open in process {process}: {why}")))?;
    Ok(Lent::with_visibility(store, root, how.visibility))
}

fn rooted_history(tape: &Tape, how: Rooted) -> Result<(), Broke> {
    let seed = tape.next_u64("sim seed");
    let ops = tape.vec(1..=50, "ops", root_op);
    let (mut process, mut boot) = (1u64, 1u64);
    let root = test_root();
    let mut lent = open_rooted(Sim::new(seed, Faults::default()), process, boot, &root, how)?;
    // What the store holds as far as this process knows: read back after each kill.
    let mut held: BTreeSet<u32> = BTreeSet::new();
    let mut pending: BTreeSet<u32> = BTreeSet::new();
    // Every root set, by its one name, and the keys it names.
    let mut set: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut roots = 0u32;
    let mut name_root = |names: &BTreeSet<u32>, set: &mut BTreeMap<String, BTreeSet<u32>>| {
        roots += 1;
        let name = format!("r{roots}");
        set.insert(name.clone(), names.clone());
        name
    };
    let kill = |lent, power, process: &mut u64, boot: &mut u64| killed(lent, power, process, boot, how);
    for op in ops {
        match op {
            RootOp::Put(n) => {
                lent.put(&key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|why| Broke::Oracle(format!("a put of key {n} was refused: {why}")))?;
                if !held.contains(&n) {
                    pending.insert(n);
                }
            }
            RootOp::Commit { same } => {
                let all: BTreeSet<u32> = held.union(&pending).copied().collect();
                if same {
                    let keys: Vec<Key> = all.iter().map(|&n| key(n)).collect();
                    let name = name_root(&all, &mut set);
                    lent.root(&name, &keys).map_err(|why| Broke::Oracle(format!("root `{name}` was refused: {why}")))?;
                    lent.commit().map_err(|why| Broke::Oracle(format!("a commit with its root failed: {why}")))?;
                } else {
                    lent.commit().map_err(|why| Broke::Oracle(format!("a commit failed: {why}")))?;
                    let keys: Vec<Key> = all.iter().map(|&n| key(n)).collect();
                    let name = name_root(&all, &mut set);
                    lent.root(&name, &keys).map_err(|why| Broke::Oracle(format!("root `{name}` was refused: {why}")))?;
                    lent.commit().map_err(|why| Broke::Oracle(format!("a root's own commit failed: {why}")))?;
                }
                held = all;
                pending.clear();
            }
            RootOp::Crash { power } => {
                lent = kill(lent, power, &mut process, &mut boot)?;
                roots_whole(tape, &lent, &set)?;
                held = held_now(&lent)?;
                pending.clear();
            }
            RootOp::CutCommit(after) => {
                let all: BTreeSet<u32> = held.union(&pending).copied().collect();
                let keys: Vec<Key> = all.iter().map(|&n| key(n)).collect();
                let name = name_root(&all, &mut set);
                lent.root(&name, &keys).map_err(|why| Broke::Oracle(format!("root `{name}` was refused: {why}")))?;
                lent.cut(Some(Cut::After(after)));
                let _ = lent.commit();
                lent = kill(lent, true, &mut process, &mut boot)?;
                roots_whole(tape, &lent, &set)?;
                held = held_now(&lent)?;
                pending.clear();
            }
        }
    }
    Ok(())
}

const ROOT_REACH: &[&str] = &["a root that survived a kill was walked whole"];

/// Durable: every segment, log and head synced before its flip completes, so a root's records
/// reach the disk before it: every surviving root is whole, under both visibilities.
#[test]
fn a_durable_store_never_keeps_a_root_with_a_hole_under_either_visibility() {
    blaze_sim::check(SimConfig::DEFAULT.must_reach(ROOT_REACH), |tape| {
        rooted_history(tape, Rooted { visibility: Visibility::AfterCommit, durable: true, keeps_holed_root: false })?;
        rooted_history(tape, Rooted { visibility: IN_ARENA, durable: true, keeps_holed_root: false })
    });
}

/// Not durable (the production default, `Settings::default`): the same oracle. A non-durable
/// flip is not ordered after its records' bytes (core.rs `Synced::not_durable`, `LOG_WRITE`,
/// `SLOT_WRITE`: no sync or barrier before the head; barriers only in `Plain::seal`, after
/// the answer), so a power cut may keep a head naming a root and tear a segment it reaches;
/// D200 keeps the promise at the open instead: the next boot's open checks the tail and drops
/// every root that reaches a lost record before any reader sees it (driven.rs `unroot_holed`).
#[test]
fn a_store_that_is_not_durable_never_keeps_a_root_with_a_hole_under_either_visibility() {
    blaze_sim::check(SimConfig::DEFAULT.must_reach(ROOT_REACH), |tape| {
        rooted_history(tape, Rooted { visibility: Visibility::AfterCommit, durable: false, keeps_holed_root: false })?;
        rooted_history(tape, Rooted { visibility: IN_ARENA, durable: false, keeps_holed_root: false })
    });
}

/// Control: the open's unrooting skipped (`Plant::KeepsHoledRoot`), a power cut that tears a
/// segment a root reaches leaves that root for a reader: the oracle names the hole.
#[test]
fn the_oracle_catches_an_open_that_keeps_a_root_with_a_hole() {
    blaze_sim::control(SimConfig::cases(256), false, true, HOLE, |tape, keeps_holed_root| {
        rooted_history(tape, Rooted { visibility: Visibility::AfterCommit, durable: false, keeps_holed_root })
    });
}

/// D200's set-time guard (driven.rs `root_names_held`), with no crash: a planted writer naming
/// a key no one put is refused when its root is set, by the root's name and that key, under
/// both visibilities; its twin, naming a key put in the same commit (still in the arena, not
/// yet the store's), is set; and a key committed before is found in the rows.
#[test]
fn a_root_naming_a_key_never_put_is_refused_when_set_and_its_twin_is_set() {
    for visibility in [Visibility::AfterCommit, IN_ARENA] {
        let root = test_root();
        let Ok(lent) = open_durable(Sim::new(71, Faults::default()), 1, 1, &root, visibility, false) else {
            panic!("{visibility:?}: the store did not open");
        };
        lent.put(&key(1), Namespace::LOCAL, TAG, &value(1)).expect("a put");
        lent.root("twin", &[key(1)]).unwrap_or_else(|why| panic!("{visibility:?}: a root naming this commit's own put was refused: {why}"));
        match lent.root("planted", &[key(1), key(2)]) {
            Err(LentError::Store { why: StoreError::RootNamesUnheld { root: name, key: at }, .. }) => {
                assert_eq!((name.as_str(), at), ("planted", key(2).0), "{visibility:?}: the refusal names another root or key");
            }
            other => panic!("{visibility:?}: a root naming a key never put was not refused when set: {other:?}"),
        }
        lent.commit().unwrap_or_else(|why| panic!("{visibility:?}: the commit failed: {why}"));
        let named: Vec<String> = lent.roots().into_iter().map(|(name, _)| name).collect();
        assert_eq!(named, vec!["twin".to_owned()], "{visibility:?}: the refused root was set anyway");
        lent.root("after", &[key(1)]).unwrap_or_else(|why| panic!("{visibility:?}: a root naming a committed key was refused: {why}"));
        assert!(lent.put_tally().disk.root_probes >= 4, "{visibility:?}: the root names looked for are not counted");
    }
    println!("set-time guard: a never-put name refused by root and key under both visibilities; the twin set");
}
