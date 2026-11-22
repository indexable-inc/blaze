//! Cleanup that never blocks, on the deterministic simulation (rules 43 and 64): a
//! [`Liveness::Reached`] store driven by many short processes, as the command's store is. Each
//! command opens, names its checkout's records under its root (putting what its view lacks,
//! reusing the rest by key, an older version of a record now and then), writes one cleanup slice
//! with no lock ([`Plain::fold_slice`]), lets go of its lease and commits once; three run at a
//! time. One command commits records and dies holding its lease. The clock moves a minute a wave.
//!
//! The oracle:
//! - every record an unexpired root of the head names reads back with its own bytes (the model
//!   `value(n)`), after every twentieth wave, after the run, and after a cut at every completion
//!   of a wave that folds (the processes dying alone, and a power cut);
//! - no lock wait finds cleanup holding the flip lock (counted at every `Contended`: 0);
//! - a live lease keeps its holder's records; a lease whose holder died is gone once
//!   [`LEASE_NS`] passed, and its records are swept;
//! - a root past its life is gone;
//! - no commit is refused: one whose roots name a record a slice swept under it carries the
//!   record forward (the reach point: records carried > 0);
//! - after 1,000 commands, the store's segment bytes are at most twice the live bytes, plus each
//!   segment's header and footer and the last command's own segment.
//!
//! The controls plant one bug each and show the oracle fails: a reach that skips a root
//! (`Plant::ReachSkipsRoot`), a slice's segments unlinked while the generation on disk names
//! them (`Plant::UnlinkEarly`), a lease that never expires (`Plant::LeaseNeverExpires`),
//! cleanup flipping under the lock on its own (`Plant::SliceFlipsAlone`), and a commit that
//! flips without carrying what was swept under it (`Plant::CarrySkipped`).

use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, Tag as EffectTag,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Namespace, Tag},
    store::{ENTRY, FOOTER, HEADER, StoreError},
};
use blaze_mem::Frame;

use super::{
    core::{GRACE_NS, LEASE_NS, Liveness, Plain, PlainConfig, Plant, ROOT_LIFE_NS, TAGS},
    format::aligned,
    test_root,
};
use crate::Key;

/// A tag no store owns: the test's own listing.
const OWN: u64 = 1 << 60;
const TAG: Tag = Tag(1);
/// Records a checkout's answer names.
const SLOTS: u32 = 20;
const CHECKOUTS: u64 = 4;
const MINUTE: Duration = Duration::from_secs(60);

fn dir() -> FileName {
    FileName::new("plain").expect("a name")
}

fn cfg(n: u64, boot: u64, root: &Frame) -> PlainConfig<&Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: dir(),
        tags: (n + 1) * TAGS,
        cap_bytes: u64::MAX,
        boot,
        nonce: 5000 + n,
        // A power cut is part of the claim, so a commit syncs (as root_commit_tests.rs).
        durable: true,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: Liveness::Reached,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain cleanup test key", &n.to_le_bytes()))
}

/// Record `n`'s one value: 40 to 440 bytes.
fn value(n: u32) -> Vec<u8> {
    (0..40 + (n as usize * 37) % 400).map(|i| (n as usize).wrapping_mul(17).wrapping_add(i) as u8).collect()
}

/// The record of checkout `c`'s slot `slot` at `version`.
fn record(c: u64, slot: u32, version: u32) -> u32 {
    c as u32 * 100_000 + slot * 1_000 + version
}

/// splitmix64: the history's choices, from its seed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        (z ^ (z >> 31)) % n
    }
}

/// One command.
enum Job {
    /// A checkout's: names `records` under `root` for `life_ns`, putting what its view lacks;
    /// with `fold`, one slice before its commit.
    Answer { root: String, records: Vec<u32>, life_ns: u64, fold: bool },
    /// Commits `records` (taking a lease) and dies before any answer.
    Dies { records: Vec<u32> },
}

enum Step {
    Opening,
    Folding,
    Committing,
    Done,
}

/// A process's store, its tables in the history's frame.
type Store<'f> = Plain<&'f Frame>;

struct Proc<'f> {
    store: Store<'f>,
    effects: Vec<Effect>,
    job: Job,
    step: Step,
}

struct World<'f> {
    /// The frame every process's store keeps its tables in, made by the test (or per cut).
    root: &'f Frame,
    sim: Sim,
    boot: u64,
    next: u64,
    plant: Plant,
    /// The model: every key a command named, to its record number.
    model: BTreeMap<Key, u32>,
    /// Lock waits that found cleanup holding the flip lock.
    waits_on_cleanup: u32,
    /// Slices a flip applied.
    slices: u32,
    /// Commits refused as [`StoreError::SweptUnder`]: only past `CARRY_ROUNDS` slices
    /// landing under one commit, which a wave of three (two other slices) cannot reach.
    swept_under: u32,
    /// Records commits carried forward, a slice having swept their segment under the commit
    /// (`Plain::carried`): the reach point of the carry.
    carried: u64,
    /// Every other refusal or break, by what it was.
    errors: Vec<String>,
    /// Completions seen: where a cut is.
    seen: u64,
    /// The nonce of the command that died holding its lease.
    dead: Option<u64>,
}

impl<'f> World<'f> {
    fn new(seed: u64, plant: Plant, root: &'f Frame) -> World<'f> {
        World {
            root,
            sim: Sim::new(seed, Faults::default()),
            boot: 1,
            next: 0,
            plant,
            model: BTreeMap::new(),
            waits_on_cleanup: 0,
            slices: 0,
            swept_under: 0,
            carried: 0,
            errors: Vec::new_in(Global),
            seen: 0,
            dead: None,
        }
    }

    fn process(&mut self, plant: Plant) -> (Store<'f>, Vec<Effect>, u64) {
        let n = self.next;
        self.next += 1;
        let mut store = Plain::planted(cfg(n, self.boot, self.root), self.root, plant);
        let mut effects = Vec::new_in(Global);
        store.open_whole(&mut effects);
        (store, effects, 5000 + n)
    }
}

fn commit(w: &mut World<'_>, p: &mut Proc<'_>) {
    let now = w.sim.wall_ns();
    match p.store.commit(&[], now, &mut p.effects) {
        Ok(()) => p.step = Step::Committing,
        Err(why) => {
            w.errors.push(format!("a commit was refused at once: {why}"));
            p.step = Step::Done;
        }
    }
}

/// `p`'s next step, its store ready.
fn next_step(w: &mut World<'_>, p: &mut Proc<'_>) {
    if let Some(why) = p.store.broken() {
        w.errors.push(format!("a command's store broke: {why}"));
        p.step = Step::Done;
        return;
    }
    match p.step {
        Step::Opening => {
            p.store.handed();
            let (records, root) = match &p.job {
                Job::Answer { root, records, life_ns, fold } => (records.clone(), Some((root.clone(), *life_ns, *fold))),
                Job::Dies { records } => (records.clone(), None),
            };
            let mut named = Vec::new_in(w.root);
            for n in records {
                let k = key(n);
                w.model.insert(k, n);
                if p.store.get(&k).is_none() {
                    p.store.put(k, Namespace::LOCAL, TAG, &value(n)).expect("a put");
                }
                named.push(k);
            }
            let Some((root, life_ns, fold)) = root else {
                // Dies: its records committed under its lease, never rooted.
                return commit(w, p);
            };
            if let Err(why) = p.store.root_for(&root, &named, life_ns) {
                w.errors.push(format!("a root was refused: {why}"));
                p.step = Step::Done;
                return;
            }
            if fold {
                match p.store.fold_slice(&mut p.effects) {
                    Ok(_) => p.step = Step::Folding,
                    Err(why) => {
                        w.errors.push(format!("a slice was refused: {why}"));
                        p.step = Step::Done;
                    }
                }
                return;
            }
            p.store.release_lease();
            commit(w, p);
        }
        Step::Folding => {
            p.store.release_lease();
            commit(w, p);
        }
        Step::Committing => {
            match p.store.take_committed() {
                Some(Ok(_)) => {}
                Some(Err(StoreError::SweptUnder { .. })) => w.swept_under += 1,
                Some(Err(why)) => w.errors.push(format!("a commit failed: {why}")),
                None => w.errors.push("a commit ended with no outcome".to_owned()),
            }
            // One commit a process: its count is this commit's.
            w.carried += p.store.carried();
            if let Some(slice) = p.store.take_sliced()
                && slice.segments > 0
                && !slice.aborted
            {
                w.slices += 1;
            }
            p.step = Step::Done;
        }
        Step::Done => {}
    }
}

/// `jobs` run at once, each a process of its own, until each is done; or, at completion `cut`
/// (counted over the whole history), every process dies there. Whether the cut came.
fn wave(w: &mut World<'_>, jobs: Vec<Job>, cut: Option<u64>) -> bool {
    let mut procs: Vec<Proc<'_>, &Frame> = Vec::new_in(w.root);
    for job in jobs {
        let (store, effects, nonce) = w.process(w.plant);
        if matches!(job, Job::Dies { .. }) {
            w.dead = Some(nonce);
        }
        procs.push(Proc { store, effects, job, step: Step::Opening });
    }
    let mut done: Vec<Completion> = Vec::new_in(Global);
    for _ in 0..10_000_000u32 {
        // A step may end with nothing asked (a commit with nothing left to write ends at once,
        // as one does after a `SliceFlipsAlone` slice flipped its root and lease): the process
        // is ready again, and the next round steps it. A stall is a round that stepped no one
        // with nothing in flight.
        let mut stepped = false;
        for p in procs.iter_mut() {
            if p.store.ready() && p.effects.is_empty() && !matches!(p.step, Step::Done) {
                next_step(w, p);
                stepped = true;
            }
            if p.store.broken().is_some() && !matches!(p.step, Step::Done) {
                next_step(w, p);
                stepped = true;
            }
            w.sim.submit(&mut p.effects);
        }
        if procs.iter().all(|p| matches!(p.step, Step::Done)) {
            drain(&mut w.sim);
            return false;
        }
        if w.sim.in_flight() == 0 {
            if stepped {
                continue;
            }
            panic!("a wave stalled: a process neither done nor asking anything");
        }
        w.sim.poll(&mut done, Deadline::after(w.sim.now(), Duration::from_secs(1)));
        let wall = w.sim.wall_ns();
        for completion in done.drain(..) {
            if cut == Some(w.seen) {
                return true;
            }
            w.seen += 1;
            if matches!(completion.done, Done::Contended) && procs.iter().any(|q| q.store.cleanup_holds_lock()) {
                w.waits_on_cleanup += 1;
            }
            let p = procs.iter_mut().find(|it| it.store.owns(completion.tag)).expect("a store asked for it");
            p.store.complete(completion, wall, &mut p.effects);
        }
    }
    panic!("a wave did not end in ten million polls");
}

/// What is still in flight, landed and let go of: the processes that asked are gone.
fn drain(sim: &mut Sim) {
    let mut done: Vec<Completion> = Vec::new_in(Global);
    for _ in 0..100_000u32 {
        if sim.in_flight() == 0 {
            return;
        }
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
        done.clear();
    }
    panic!("the simulation did not drain");
}

/// A reader opened on the head as it is now (no plant: the reader is the oracle's).
fn reader<'f>(w: &mut World<'f>) -> Result<Store<'f>, String> {
    let (mut store, mut effects, _) = w.process(Plant::None);
    let mut done: Vec<Completion> = Vec::new_in(Global);
    for _ in 0..1_000_000u32 {
        w.sim.submit(&mut effects);
        if w.sim.in_flight() == 0 {
            break;
        }
        w.sim.poll(&mut done, Deadline::after(w.sim.now(), Duration::from_secs(1)));
        let wall = w.sim.wall_ns();
        for completion in done.drain(..) {
            store.complete(completion, wall, &mut effects);
        }
    }
    if let Some(why) = store.broken() {
        return Err(format!("the reader's open broke: {why}"));
    }
    store.handed();
    Ok(store)
}

/// Every key an unexpired root names, the roots' own records too, each once.
fn rooted(w: &World<'_>, r: &Store<'_>) ->Result<BTreeSet<Key>, String> {
    let now = w.sim.wall_ns();
    let mut out = BTreeSet::new();
    for (name, root, expires_ns) in r.roots_expiring() {
        if expires_ns <= now {
            continue;
        }
        out.insert(root);
        let Some(got) = r.get(&root) else { return Err(format!("the root {name}'s record is gone")) };
        for k in got.bytes.chunks_exact(32) {
            out.insert(Key(ContentId(k.try_into().expect("32 bytes"))));
        }
    }
    Ok(out)
}

/// The oracle's first claim: what does not read back of what an unexpired root names.
fn unreadable(w: &mut World<'_>) -> Vec<String> {
    let r = match reader(w) {
        Ok(r) => r,
        Err(why) => return vec![why],
    };
    let keys = match rooted(w, &r) {
        Ok(keys) => keys,
        Err(why) => return vec![why],
    };
    let mut out = Vec::new_in(Global);
    for k in keys {
        let Some(&n) = w.model.get(&k) else { continue };
        if r.get(&k).map(|it| it.bytes.to_vec()) != Some(value(n)) {
            out.push(format!("record {n}, named by an unexpired root, does not read back"));
        }
    }
    out
}

/// The live bytes (each rooted record's aligned payload and index entry) and the segments'
/// bytes on disk, with their count. A root whose record, or a record it names, does not read
/// is the oracle's finding (a planted run's), returned, not a panic.
fn bytes(w: &mut World<'_>) ->Result<(u64, u64, u64), String> {
    let r = reader(w)?;
    let mut live = 0u64;
    for k in rooted(w, &r)? {
        let Some(got) = r.get(&k) else { return Err(format!("a rooted record {:?} does not read", k.0)) };
        live += aligned(got.bytes.len() as u64) + ENTRY as u64;
    }
    drop(r);
    let mut effects = vec![Effect::List { dir: dir().into(), tag: EffectTag(OWN) }];
    let mut done: Vec<Completion> = Vec::new_in(Global);
    while done.is_empty() {
        w.sim.submit(&mut effects);
        w.sim.poll(&mut done, Deadline::after(w.sim.now(), Duration::from_secs(1)));
    }
    let Some(Completion { done: Done::Listed(listed), .. }) = done.pop() else { panic!("a listing") };
    let segments: Vec<u64> = listed.into_iter().filter(|(name, meta)| !meta.dir && name.as_str().contains("/seg-")).map(|(name, _)| w.sim.contents(&name).len() as u64).collect();
    Ok((live, segments.iter().sum(), segments.len() as u64))
}

/// What a history showed.
struct Outcome {
    problems: Vec<String>,
    waits_on_cleanup: u32,
    slices: u32,
    swept_under: u32,
    carried: u64,
    errors: Vec<String>,
    /// The dead holder's lease, still named after it expired.
    dead_lease_left: bool,
    /// Of the dead holder's records: read back while its lease lived, and after it expired.
    dead_kept: (usize, usize),
    short_root_left: bool,
    /// Live bytes, segment bytes, segments.
    disk: (u64, u64, u64),
}

impl Outcome {
    fn caught(&self) -> bool {
        !self.problems.is_empty() || !self.errors.is_empty()
    }
}

/// The dead command's records.
fn dead_records() -> Vec<u32> {
    (9_000_000..9_000_012).collect()
}

/// `commands` commands of checkouts, `per_wave` at a time, every one folding a slice.
fn history(seed: u64, plant: Plant, commands: u32, per_wave: u32, root: &Frame) -> Outcome {
    let mut w = World::new(seed, plant, root);
    let mut rng = Rng(seed);
    let mut versions = [[0u32; SLOTS as usize]; CHECKOUTS as usize];
    let mut problems = Vec::new_in(Global);
    let short = (8_000_000..8_000_010).collect();
    wave(&mut w, vec![Job::Answer { root: "short".to_owned(), records: short, life_ns: 30 * 60 * 1_000_000_000, fold: false }], None);
    let mut dead_kept = (0, 0);
    let (mut ran, mut waves) = (0u32, 0u32);
    while ran < commands {
        let mut jobs = Vec::new_in(Global);
        for _ in 0..per_wave {
            let c = rng.below(CHECKOUTS);
            for _ in 0..2 {
                let slot = rng.below(u64::from(SLOTS)) as usize;
                let v = &mut versions[c as usize][slot];
                // Now and then back to an older version: a record reused by key, perhaps dead.
                if *v > 0 && rng.below(4) == 0 { *v -= 1 } else { *v += 1 }
            }
            let records = (0..SLOTS).map(|slot| record(c, slot, versions[c as usize][slot as usize])).collect();
            jobs.push(Job::Answer { root: format!("checkout-{c}"), records, life_ns: ROOT_LIFE_NS, fold: true });
            ran += 1;
        }
        if waves == 5 {
            jobs.push(Job::Dies { records: dead_records() });
        }
        wave(&mut w, jobs, None);
        waves += 1;
        w.sim.advance(MINUTE);
        if waves % 20 == 0 {
            problems.extend(unreadable(&mut w));
        }
        if waves == 30 {
            // 25 minutes after the death: only its lease keeps its records.
            let r = reader(&mut w).expect("the reader opens");
            dead_kept.0 = dead_records().into_iter().filter(|n| r.get(&key(*n)).map(|it| it.bytes.to_vec()) == Some(value(*n))).count();
        }
    }
    problems.extend(unreadable(&mut w));
    // Past every lease, then slices with no new records, then past the grace, and one commit
    // whose flip unlinks what they retired.
    w.sim.advance(Duration::from_nanos(LEASE_NS));
    for i in 0..6 {
        wave(&mut w, vec![Job::Answer { root: format!("final-{i}"), records: Vec::new_in(Global), life_ns: ROOT_LIFE_NS, fold: true }], None);
        w.sim.advance(Duration::from_secs(1));
    }
    w.sim.advance(Duration::from_nanos(GRACE_NS) + Duration::from_secs(1));
    wave(&mut w, vec![Job::Answer { root: "unlink".to_owned(), records: Vec::new_in(Global), life_ns: ROOT_LIFE_NS, fold: false }], None);
    problems.extend(unreadable(&mut w));
    let r = reader(&mut w).expect("the reader opens");
    let dead = w.dead.expect("a command died holding its lease");
    let dead_lease_left = r.leases().iter().any(|(holder, _)| *holder == dead);
    dead_kept.1 = dead_records().into_iter().filter(|n| r.get(&key(*n)).is_some()).count();
    let short_root_left = r.roots_expiring().iter().any(|(name, _, _)| name == "short");
    drop(r);
    let disk = match bytes(&mut w) {
        Ok(disk) => disk,
        Err(why) => {
            problems.push(why);
            (0, 0, 0)
        }
    };
    Outcome { problems, waits_on_cleanup: w.waits_on_cleanup, slices: w.slices, swept_under: w.swept_under, carried: w.carried, errors: w.errors, dead_lease_left, dead_kept, short_root_left, disk }
}

/// The invariant: concurrent commands folding cleanup slices without a lock keep every record reached by an unexpired root, never wait on cleanup, expire dead holders' leases, and keep segment bytes bounded.
/// Prints `cleanup: 1000 commands, 3 at a time: S slices folded into commits, 0 waits on
/// cleanup, C records carried forward under a slice, 0 refused; segments B bytes for L live (ratio X, N segments); every
/// rooted record read back; the dead holder's lease and records gone; the short root gone`.
#[test]
fn a_thousand_commands_keep_what_roots_reach_wait_on_no_cleanup_and_hold_twice_the_live_bytes() {
    let root = test_root();
    let out = history(1, Plant::None, 1000, 3, &root);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.problems.is_empty(), "{:?}", out.problems);
    assert_eq!(out.waits_on_cleanup, 0, "a command waited on cleanup for the flip lock");
    assert!(out.slices > 10, "only {} slices landed: the sweep was not exercised", out.slices);
    // No commit after its answer is refused: one that met a slice under it carried forward.
    assert_eq!(out.swept_under, 0, "a commit was refused as swept under");
    assert!(out.carried > 0, "no commit carried a record forward: the race of a slice landing under a commit was not reached");
    assert_eq!(out.dead_kept.0, dead_records().len(), "a live lease's records were swept");
    assert!(!out.dead_lease_left, "the dead holder's lease outlived LEASE_NS");
    assert_eq!(out.dead_kept.1, 0, "the dead holder's records were not swept after its lease expired");
    assert!(!out.short_root_left, "a root past its life is still named");
    let (live, on_disk, segments) = out.disk;
    // Each segment's header and footer, and the last command's own segment (one root record).
    let slack = segments * (HEADER + FOOTER) as u64 + 4096;
    assert!(on_disk <= 2 * live + slack, "segments hold {on_disk} bytes for {live} live ({segments} segments): past twice the live bytes");
    println!(
        "cleanup: 1000 commands, 3 at a time: {} slices folded into commits, 0 waits on cleanup, {} records carried forward under a slice, 0 refused; segments {on_disk} bytes for {live} live (ratio {:.2}, {segments} segments); every rooted record read back; the dead holder's lease and records gone; the short root gone",
        out.slices,
        out.carried,
        on_disk as f64 / live.max(1) as f64
    );
}

/// The four controls, each on a shorter history. Prints one `cleanup, planted X: caught (...)`
/// line each.
#[test]
fn the_oracle_catches_a_skipped_root_a_lease_that_never_expires_and_cleanup_under_the_lock() {
    let root = test_root();
    let out = history(2, Plant::ReachSkipsRoot, 240, 3, &root);
    assert!(out.caught(), "the planted reach that skips a root went unseen");
    println!("cleanup, planted ReachSkipsRoot: caught ({} unreadable, {} errors)", out.problems.len(), out.errors.len());
    let out = history(3, Plant::LeaseNeverExpires, 240, 3, &root);
    assert!(out.dead_lease_left || out.dead_kept.1 > 0, "the planted lease that never expires went unseen");
    println!("cleanup, planted LeaseNeverExpires: caught (the dead holder's lease left: {}, its records left: {})", out.dead_lease_left, out.dead_kept.1);
    let out = history(4, Plant::SliceFlipsAlone, 240, 4, &root);
    assert!(out.waits_on_cleanup > 0, "the planted cleanup flip under the lock went unseen");
    println!("cleanup, planted SliceFlipsAlone: caught ({} waits on cleanup)", out.waits_on_cleanup);
}

/// A commit that meets a slice under it carries the swept records forward, and the control
/// that flips without carrying them is caught: the root names a record no live segment holds.
/// Prints `cleanup: N records carried over M commands, every rooted record read back`, then
/// `cleanup, planted CarrySkipped: caught (U unreadable)`.
#[test]
fn a_commit_a_slice_lands_under_carries_its_records_forward_and_a_skipped_carry_is_caught() {
    let root = test_root();
    let out = history(5, Plant::None, 600, 4, &root);
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert!(out.problems.is_empty(), "{:?}", out.problems);
    assert_eq!(out.swept_under, 0, "a commit was refused as swept under with three other slices at most");
    assert!(out.carried > 0, "no commit carried a record forward: the race was not reached");
    println!("cleanup: {} records carried over 600 commands, 4 at a time, every rooted record read back", out.carried);
    let out = history(5, Plant::CarrySkipped, 600, 4, &root);
    assert!(out.caught(), "the planted commit that skips its carry went unseen");
    println!("cleanup, planted CarrySkipped: caught ({} unreadable, {} errors)", out.problems.len(), out.errors.len());
}

/// Twelve commands of two checkouts that each change half their records, then a wave of two
/// folding commands cut at completion `at` (`None`: not cut).
fn cut_history(plant: Plant, at: Option<u64>, power: bool, root: &Frame) -> (Vec<String>, bool) {
    let mut w = World::new(17, plant, root);
    let mut rng = Rng(17);
    let mut versions = [[0u32; SLOTS as usize]; 2];
    let jobs_of = |rng: &mut Rng, versions: &mut [[u32; SLOTS as usize]; 2], fold: bool| -> Vec<Job> {
        let mut jobs = Vec::new_in(Global);
        for c in 0..2u64 {
            for slot in 0..SLOTS as usize {
                if rng.below(2) == 0 {
                    versions[c as usize][slot] += 1;
                }
            }
            let records = (0..SLOTS).map(|slot| record(c, slot, versions[c as usize][slot as usize])).collect();
            jobs.push(Job::Answer { root: format!("checkout-{c}"), records, life_ns: ROOT_LIFE_NS, fold });
        }
        jobs
    };
    for _ in 0..6 {
        let jobs = jobs_of(&mut rng, &mut versions, false);
        wave(&mut w, jobs, None);
        w.sim.advance(MINUTE);
    }
    let jobs = jobs_of(&mut rng, &mut versions, true);
    let at = at.map(|it| w.seen + it);
    let cut = wave(&mut w, jobs, at);
    drain(&mut w.sim);
    if power {
        w.sim.crash();
        w.boot += 1;
    } else {
        w.sim.processes_died();
    }
    let mut problems = unreadable(&mut w);
    problems.extend(w.errors.iter().cloned());
    (problems, cut)
}

/// Every cut of a wave whose commands fold slices: the processes dying alone, and a power cut.
/// How many recoveries broke the oracle, of how many cuts. Each cut is a run of its own, its
/// processes' stores in a frame of that run, given back before the next.
fn torn(plant: Plant) -> (u32, u32) {
    let (mut bad, mut cuts) = (0, 0);
    for power in [false, true] {
        for at in 0..2_000u64 {
            let root = test_root();
            let (problems, cut) = cut_history(plant, Some(at), power, &root);
            if !problems.is_empty() {
                bad += 1;
            }
            cuts += 1;
            if !cut {
                break;
            }
        }
    }
    (bad, cuts)
}

/// Prints `cleanup: N cuts of a folding wave (process death and power cut); every rooted record
/// read back`, then `cleanup, planted UnlinkEarly: caught (N recoveries ...)`.
#[test]
fn every_cut_of_a_folded_slice_keeps_what_roots_reach_and_an_early_unlink_is_caught() {
    let root = test_root();
    let (problems, _) = cut_history(Plant::None, None, false, &root);
    assert!(problems.is_empty(), "the uncut history: {problems:?}");
    let (bad, cuts) = torn(Plant::None);
    assert_eq!(bad, 0, "a recovery after a cut lost a record an unexpired root names");
    assert!(cuts > 20, "the wave was cut at too few points ({cuts})");
    println!("cleanup: {cuts} cuts of a folding wave (process death and power cut); every rooted record read back");
    let (bad, _) = torn(Plant::UnlinkEarly);
    assert!(bad > 0, "the planted unlink before the flip went unseen");
    println!("cleanup, planted UnlinkEarly: caught ({bad} recoveries lost a rooted record)");
}
