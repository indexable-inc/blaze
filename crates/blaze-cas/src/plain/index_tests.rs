//! Indexed roots on the deterministic simulation (rules 32, 43 and 64): a root that names INDEX
//! records ([`Plain::root_through`]) keeps each index and every key each index holds, through
//! cleanup slices that sweep the segments those records lie in; two roots sharing one index
//! keep it while either lives; an index no unexpired root names keeps nothing.
//!
//! One history, commands one after another over one store, each a process of its own: the
//! first puts a program's records (named by an index through the root `program`, and again by
//! the longer-lived root `toolchain`), a few records under a plain root, and dead records in
//! the same segment, then lets go of its lease; the commands after it put garbage no root
//! names and fold one cleanup slice each, and the clock moves past each root's life.
//!
//! The oracle, after each stage: every record an unexpired root reaches (through its indexes,
//! one level) reads back with its own bytes; the dead records are gone and the segments hold
//! at most twice the live bytes plus each segment's header and footer; once both roots that
//! name the index expired, the program's records are gone too.
//!
//! The control: [`Plant::IndexNotFollowed`] (reach takes the indexed root's record as a plain
//! root's: its indexes live, what they hold not) fails the first claim: the program's records
//! are swept while the root that reaches them lives.

use std::{alloc::Global, time::Duration};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, Tag as EffectTag,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Namespace, ReservedTag, Tag},
    store::{ENTRY, FOOTER, HEADER},
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
/// The program's records, named through the index.
const PROGRAM: u32 = 64;
/// Records under a plain root beside it.
const PLAIN: u32 = 8;
/// Dead records in the first command's segment: larger than the live ones, so the segment is
/// under half live and the first slice that looks at it sweeps it.
const DEAD: u32 = 64;
/// Garbage commands, each folding a slice.
const GARBAGE: u32 = 6;
const MINUTE: Duration = Duration::from_secs(60);

fn dir() -> FileName {
    FileName::new("plain").expect("a name")
}

fn cfg(n: u64, root: &Frame) -> PlainConfig<&Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: dir(),
        tags: (n + 1) * TAGS,
        cap_bytes: u64::MAX,
        boot: 1,
        nonce: 9000 + n,
        durable: false,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: Liveness::Reached,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

pub(super) fn key(n: u32) -> Key {
    Key(ContentId::of("plain index test key", &n.to_le_bytes()))
}

/// Record `n`'s one value: 40 to 440 bytes; a dead one ([`dead`]) 2,000 more.
pub(super) fn value(n: u32) -> Vec<u8> {
    let long = if (DEAD_FROM..DEAD_FROM + DEAD).contains(&n) { 2000 } else { 0 };
    (0..40 + long + (n as usize * 37) % 400).map(|i| (n as usize).wrapping_mul(29).wrapping_add(i) as u8).collect()
}

const PLAIN_FROM: u32 = 10_000;
const DEAD_FROM: u32 = 20_000;
const GARBAGE_FROM: u32 = 30_000;

fn program() -> impl Iterator<Item = u32> {
    0..PROGRAM
}

fn plain() -> impl Iterator<Item = u32> {
    PLAIN_FROM..PLAIN_FROM + PLAIN
}

fn dead() -> impl Iterator<Item = u32> {
    DEAD_FROM..DEAD_FROM + DEAD
}

/// The history's world: the simulation, the next process's number, the plant, and the test's
/// frame every process's store keeps its tables in (made at the top of the test).
pub(super) struct World<'f> {
    pub(super) sim: Sim,
    pub(super) next: u64,
    pub(super) plant: Plant,
    pub(super) root: &'f Frame,
}

/// A process's store, its tables in the test's frame.
pub(super) type Store<'f> = Plain<&'f Frame>;

impl<'f> World<'f> {
    /// A process's store, opened whole on the head as it is now.
    pub(super) fn open(&mut self, plant: Plant) -> Result<Store<'f>, String> {
        let n = self.next;
        self.next += 1;
        let mut store = Plain::planted(cfg(n, self.root), self.root, plant);
        let mut effects = Vec::new_in(Global);
        store.open_whole(&mut effects);
        self.settle(&mut store, &mut effects)?;
        store.handed();
        Ok(store)
    }

    /// Every effect `store` asks, done, until it asks nothing more.
    fn settle(&mut self, store: &mut Store<'f>, effects: &mut Vec<Effect>) -> Result<(), String> {
        let mut done: Vec<Completion> = Vec::new_in(Global);
        for _ in 0..1_000_000u32 {
            self.sim.submit(effects);
            if self.sim.in_flight() == 0 {
                if let Some(why) = store.broken() {
                    return Err(format!("the store broke: {why}"));
                }
                return Ok(());
            }
            self.sim.poll(&mut done, Deadline::after(self.sim.now(), Duration::from_secs(1)));
            let wall = self.sim.wall_ns();
            for completion in done.drain(..) {
                store.complete(completion, wall, effects);
            }
        }
        Err("a store did not settle in a million polls".to_owned())
    }

    /// One command: `puts` then `roots` (each given the store), one slice folded when `fold`,
    /// the lease let go of, one commit.
    pub(super) fn command(&mut self, puts: &[u32], roots: impl FnOnce(&mut Store<'f>) -> Result<(), String>, fold: bool) -> Result<(), String> {
        let mut store = self.open(self.plant)?;
        let mut effects = Vec::new_in(Global);
        for &n in puts {
            store.put(key(n), Namespace::LOCAL, TAG, &value(n)).map_err(|why| format!("a put of record {n}: {why:?}"))?;
        }
        roots(&mut store)?;
        if fold {
            store.fold_slice(&mut effects).map_err(|why| format!("a slice: {why}"))?;
            self.settle(&mut store, &mut effects)?;
        }
        store.release_lease();
        let now = self.sim.wall_ns();
        store.commit(&[], now, &mut effects).map_err(|why| format!("a commit: {why}"))?;
        self.settle(&mut store, &mut effects)?;
        match store.take_committed() {
            Some(Ok(_)) => Ok(()),
            Some(Err(why)) => Err(format!("a commit failed: {why}")),
            None => Err("a commit ended with no outcome".to_owned()),
        }
    }

    /// The segments' bytes on disk and their count.
    fn disk(&mut self) -> (u64, u64) {
        let mut effects = vec![Effect::List { dir: dir().into(), tag: EffectTag(OWN) }];
        let mut done: Vec<Completion> = Vec::new_in(Global);
        while done.is_empty() {
            self.sim.submit(&mut effects);
            self.sim.poll(&mut done, Deadline::after(self.sim.now(), Duration::from_secs(1)));
        }
        let Some(Completion { done: Done::Listed(listed), .. }) = done.pop() else { panic!("a listing") };
        let sizes: Vec<u64> = listed.into_iter().filter(|(name, meta)| !meta.dir && name.as_str().contains("/seg-")).map(|(name, _)| self.sim.contents(&name).len() as u64).collect();
        (sizes.iter().sum(), sizes.len() as u64)
    }
}

/// Which of `records` do not read back with their own bytes in a fresh reader.
fn unreadable(w: &mut World<'_>, records: impl Iterator<Item = u32>) -> Result<Vec<u32>, String> {
    let r = w.open(Plant::None)?;
    Ok(records.filter(|&n| r.get(&key(n)).map(|it| it.bytes.to_vec()) != Some(value(n))).collect())
}

/// Which of `records` still read in a fresh reader.
pub(super) fn readable(w: &mut World<'_>, records: impl Iterator<Item = u32>) -> Result<Vec<u32>, String> {
    let r = w.open(Plant::None)?;
    Ok(records.filter(|&n| r.get(&key(n)).is_some()).collect())
}

/// The live bytes: each record reached (its aligned payload and index entry), the roots' and
/// the index's records too, read in a fresh reader.
fn live_bytes(w: &mut World<'_>, records: impl Iterator<Item = u32>, extra: &[Key]) -> Result<u64, String> {
    let r = w.open(Plant::None)?;
    let mut live = 0u64;
    for k in records.map(key).chain(extra.iter().copied()) {
        let Some(got) = r.get(&k) else { return Err(format!("a live record {:?} does not read", k.0)) };
        live += aligned(got.bytes.len() as u64) + ENTRY as u64;
    }
    for (_, root) in r.roots() {
        let Some(got) = r.get(&root) else { return Err("a root's record does not read".to_owned()) };
        live += aligned(got.bytes.len() as u64) + ENTRY as u64;
    }
    Ok(live)
}

/// What the history showed: each claim broken, in words. Empty: every claim held.
fn history(plant: Plant, root: &Frame) -> Result<Vec<String>, String> {
    let mut w = World { sim: Sim::new(41, Faults::default()), next: 0, plant, root };
    let mut problems = Vec::new_in(Global);
    // 1. The program's records, their index named by two roots of two lives; a plain root's
    // few; and dead records, all in one segment.
    let mut puts: Vec<u32> = program().chain(plain()).chain(dead()).collect();
    puts.sort_unstable();
    let mut index = None;
    w.command(
        &puts,
        |store| {
            let named: Vec<Key> = program().map(key).collect();
            let at = store.index(&named).map_err(|why| format!("the index: {why}"))?;
            store.root_through("program", &[at], ROOT_LIFE_NS).map_err(|why| format!("the program's root: {why}"))?;
            store.root_through("toolchain", &[at], 2 * ROOT_LIFE_NS).map_err(|why| format!("the toolchain's root: {why}"))?;
            let direct: Vec<Key> = plain().map(key).collect();
            store.root_for("plain", &direct, ROOT_LIFE_NS).map_err(|why| format!("the plain root: {why}"))?;
            index = Some(at);
            Ok(())
        },
        false,
    )?;
    let index = index.expect("the first command made the index");
    {
        let r = w.open(Plant::None)?;
        match r.get(&index) {
            Some(got) if ReservedTag::Index.is(got.tag) && got.bytes.len() == PROGRAM as usize * 32 => {}
            other => problems.push(format!("the index does not read as an index of {PROGRAM} keys: {:?}", other.map(|it| (it.tag, it.bytes.len())))),
        }
        let indexed = r.roots().into_iter().filter(|(name, _)| name == "program" || name == "toolchain").filter(|(_, root)| r.get(root).is_some_and(|it| ReservedTag::IndexedRoot.is(it.tag))).count();
        if indexed != 2 {
            problems.push(format!("{indexed} of the two indexed roots are filed as indexed roots"));
        }
    }
    // 2. Garbage commands, each folding a slice; the clock past every lease and grace.
    for g in 0..GARBAGE {
        let garbage: Vec<u32> = (GARBAGE_FROM + g * 32..GARBAGE_FROM + (g + 1) * 32).collect();
        w.command(&garbage, |_| Ok(()), true)?;
        w.sim.advance(MINUTE);
    }
    w.sim.advance(Duration::from_nanos(LEASE_NS.max(GRACE_NS)) + MINUTE);
    for _ in 0..4 {
        w.command(&[], |_| Ok(()), true)?;
        w.sim.advance(Duration::from_secs(1));
    }
    let lost = unreadable(&mut w, program().chain(plain()))?;
    if !lost.is_empty() {
        problems.push(format!("{} records a live root reaches do not read back after the sweep (first: {})", lost.len(), lost[0]));
    }
    let kept = readable(&mut w, dead())?;
    if !kept.is_empty() {
        problems.push(format!("{} dead records were not swept", kept.len()));
    }
    // The unlink of what the slices retired, a grace later: a folding command's flip unlinks.
    w.sim.advance(Duration::from_nanos(GRACE_NS) + MINUTE);
    w.command(&[], |_| Ok(()), true)?;
    if problems.is_empty() {
        let live = live_bytes(&mut w, program().chain(plain()), &[index])?;
        let (disk, segments) = w.disk();
        let slack = segments * (HEADER + FOOTER) as u64;
        if disk > 2 * live + slack {
            problems.push(format!("the segments hold {disk} bytes, past twice the live {live} plus {slack} of headers and footers"));
        }
    }
    // 3. The program's root expires; the toolchain's, naming the same index, keeps it.
    w.sim.advance(Duration::from_nanos(ROOT_LIFE_NS) + MINUTE);
    for _ in 0..4 {
        w.command(&[], |_| Ok(()), true)?;
        w.sim.advance(Duration::from_secs(1));
    }
    let lost = unreadable(&mut w, program())?;
    if !lost.is_empty() {
        problems.push(format!("{} records of an index the toolchain's root still names are gone after the program's root expired", lost.len()));
    }
    // 4. Both expired: the index keeps nothing by itself.
    w.sim.advance(Duration::from_nanos(ROOT_LIFE_NS) + MINUTE);
    for _ in 0..4 {
        w.command(&[], |_| Ok(()), true)?;
        w.sim.advance(Duration::from_secs(1));
    }
    let kept = readable(&mut w, program())?;
    if !kept.is_empty() {
        problems.push(format!("{} records of an index no unexpired root names were kept", kept.len()));
    }
    Ok(problems)
}

/// Asserts: every record an indexed root reaches survives the slices that sweep its segment;
/// the dead records around it are swept and the disk holds at most twice the live bytes plus
/// headers and footers; an index shared by two roots lives while either does, and keeps nothing
/// once neither does.
/// Prints "indexed roots: 64 records through one index kept by 2 roots; 64 dead swept; expiry lets go".
#[test]
fn an_indexed_root_keeps_what_its_indexes_hold() {
    let root = test_root();
    let problems = history(Plant::None, &root).expect("the history runs");
    assert!(problems.is_empty(), "{problems:?}");
    std::println!("indexed roots: {PROGRAM} records through one index kept by 2 roots; {DEAD} dead swept; expiry lets go");
}

/// The control (rule 23): reach that does not follow the indexes sweeps the program's records
/// while the roots reaching them live, and the oracle says so.
/// Prints "indexed roots control: the index not followed: <the oracle's finding>".
#[test]
fn reach_that_does_not_follow_an_index_is_caught() {
    let root = test_root();
    let problems = history(Plant::IndexNotFollowed, &root).expect("the history runs");
    assert!(problems.iter().any(|it| it.contains("a live root reaches do not read back")), "the planted reach was not caught: {problems:?}");
    std::println!("indexed roots control: the index not followed: {}", problems[0]);
}
