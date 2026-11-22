//! The keyed pointer ([`ReservedTag::Pointer`], `Lent::point` and `Lent::pointed`) under the
//! simulation. The model: each name maps to the target of the last commit that pointed it.
//!
//! - Every cut of a command's commit (process death alone, and a power cut) is followed by a
//!   reopen, and the oracle is checked there: the names read either all the old values or all
//!   the new ones (one flip), the new ones whenever the commit returned, and never a target the
//!   store does not hold.
//! - Two processes point one name, each from a view older than the other's commit: the later
//!   commit wins, in either order, a value pointed again included (its segment's bytes equal an
//!   older segment's).
//! - Compaction merges between commits and keeps only the newest value.
//!
//! The planted controls: `LentPlant::PointerFirst` makes a pointer durable before its target,
//! and `Plant::PointerOldest` reads a pointer from the oldest row.

use std::{alloc::Global, path::Path, time::Duration};

use blaze_core::{
    Completion, Driver, Effect, FileName,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, ReservedTag, Tag};
use blaze_mem::Frame;

use super::{
    core::{Batch, Plain, PlainConfig, Plant, TAGS},
    driven::{Cut, Driven},
    lent::{Lent, LentError, LentPlant},
    test_root,
};
use crate::Key;

/// A process's store, its arena in the run's frame.
type Store<'f> = Lent<Sim, &'f Frame>;

const TAG: Tag = Tag(1);

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
        nonce: 700 + n,
        // A power cut is part of the claim, so the commit syncs (as root_commit_tests.rs).
        durable: true,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: super::Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn name(n: u32) -> Key {
    Key(ContentId::of("plain pointer test name", &n.to_le_bytes()))
}

fn target(n: u32) -> Key {
    Key(ContentId::of("plain pointer test target", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    (0..24 + n as usize % 100).map(|i| (n as usize).wrapping_mul(29).wrapping_add(i) as u8).collect()
}

fn process(sim: Sim, n: u64, lent: LentPlant, core: Plant, root: &Frame) -> Store<'_> {
    Lent::planted(Driven::planted(sim, Path::new("/sim"), cfg(n, root), root, core).expect("the store opens"), root, lent)
}

/// The targets `ns` put and committed by a process of their own.
fn targets(sim: Sim, n: u64, ns: &[u32], root: &Frame) -> Sim {
    let lent = process(sim, n, LentPlant::None, Plant::None, root);
    for &t in ns {
        lent.put(&target(t), Namespace::LOCAL, TAG, &value(t)).expect("a target put");
    }
    lent.commit().expect("the targets' commit");
    lent.into_store().into_driver()
}

/// What names 0, 1 and 2 point at in a store opened on `sim`.
fn read_names(lent: &Store<'_>) -> Result<[Option<Key>; 3], LentError> {
    Ok([lent.pointed(TAG, &name(0))?, lent.pointed(TAG, &name(1))?, lent.pointed(TAG, &name(2))?])
}

/// The second command's commit cut at every completion, each cut followed by the process's
/// death alone and by a power cut, then a reopen: how many reopens broke the oracle, of how
/// many cuts. Each cut is a run of its own, its processes' arenas in a frame of that run, given
/// back before the next.
fn torn(lent_plant: LentPlant, core: Plant) -> (u32, u32) {
    let old = [Some(target(0)), Some(target(1)), None];
    let new = [Some(target(10)), Some(target(1)), Some(target(11))];
    let (mut bad, mut cuts) = (0, 0);
    for at in 0..256u32 {
        for power in [false, true] {
            let root = test_root();
            let sim = targets(Sim::new(41, Faults::default()), 1, &[0, 1], &root);
            let first = process(sim, 2, LentPlant::None, Plant::None, &root);
            first.point(TAG, &name(0), &target(0)).expect("a point");
            first.point(TAG, &name(1), &target(1)).expect("a point");
            first.commit().expect("the first command's commit");
            let second = process(first.into_store().into_driver(), 3, lent_plant, core, &root);
            second.cut(Some(Cut::After(at)));
            let command = || -> Result<(), LentError> {
                for t in [10, 11] {
                    second.put(&target(t), Namespace::LOCAL, TAG, &value(t))?;
                }
                second.point(TAG, &name(0), &target(10))?;
                second.point(TAG, &name(1), &target(1))?;
                second.point(TAG, &name(2), &target(11))?;
                second.commit().map(|_| ())
            };
            let reached = command().is_ok();
            let mut sim = second.into_store().into_driver();
            if power {
                sim.crash();
            }
            let after = process(sim, 4, LentPlant::None, core, &root);
            let fine = match read_names(&after) {
                Ok(read) => read == new || (!reached && read == old),
                Err(error) => {
                    eprintln!("pointer: a reopen after a cut at {at} (power {power}) refused a read: {error}");
                    false
                }
            };
            if !fine {
                bad += 1;
            }
            cuts += 1;
            if reached && power {
                // The commit finished before this cut: every later cut is the same run.
                return (bad, cuts);
            }
        }
    }
    (bad, cuts)
}

/// Every cut of a commit of pointers and their targets: a reopen reads the old values or the
/// new, never a target gone; both plants caught. Prints `pointer: N cuts (process death and
/// power cut, through the commit); every reopen read all the old pointers or all the new, never
/// a gone target`, then `pointer, planted PointerFirst: caught (N reopens ...)` and `pointer,
/// planted PointerOldest: caught (N reopens ...)`.
#[test]
fn a_pointer_and_its_target_ride_one_flip_through_every_cut() {
    let (bad, cuts) = torn(LentPlant::None, Plant::None);
    assert_eq!(bad, 0, "a reopen read a mix, a gone target, or the old value after the commit returned");
    assert!(cuts > 4, "the commit was cut at too few points ({cuts}) to cover the flip");
    println!("pointer: {cuts} cuts (process death and power cut, through the commit); every reopen read all the old pointers or all the new, never a gone target");
    let (bad, _) = torn(LentPlant::PointerFirst, Plant::None);
    assert!(bad > 0, "the planted pointer made durable before its target went unseen");
    println!("pointer, planted PointerFirst: caught ({bad} reopens read a gone target)");
    let (bad, _) = torn(LentPlant::None, Plant::PointerOldest);
    assert!(bad > 0, "the planted read of the oldest row went unseen");
    println!("pointer, planted PointerOldest: caught ({bad} reopens read an older value)");
}

/// A process at the core: the store's state machine and the effects it asks.
struct Proc<'r> {
    store: Plain<&'r Frame>,
    effects: Vec<Effect>,
}

impl<'r> Proc<'r> {
    fn open(sim: &mut Sim, n: u64, root: &'r Frame) -> Proc<'r> {
        let mut it = [Proc { store: Plain::new(cfg(n, root), root), effects: Vec::new_in(Global) }];
        it[0].store.open_whole(&mut it[0].effects);
        settle(sim, &mut it);
        let [mut it] = it;
        assert!(it.store.broken().is_none(), "the open broke");
        it.store.handed();
        it
    }

    /// `name(0)` pointed at `target(t)` from this process's view, committed and settled.
    fn point(&mut self, sim: &mut Sim, t: u32) {
        let mut batch = Batch::new(Namespace::LOCAL);
        batch.put(name(0), ReservedTag::Pointer.tag(),&target(t).0.0).expect("a pointer laid");
        self.store.commit(&[&batch], sim.wall_ns(), &mut self.effects).expect("a commit asked");
        settle(sim, std::slice::from_mut(self));
        assert!(matches!(self.store.take_committed(), Some(Ok(_))), "the commit of name 0 to target {t}");
    }
}

/// Runs until nothing is in flight; bounded.
fn settle(sim: &mut Sim, procs: &mut [Proc<'_>]) {
    let mut done: Vec<Completion> = Vec::new_in(Global);
    for _ in 0..1_000_000 {
        for p in procs.iter_mut() {
            sim.submit(&mut p.effects);
        }
        if sim.in_flight() == 0 {
            return;
        }
        sim.poll(&mut done, Deadline::after(sim.now(), Duration::from_secs(1)));
        let wall = sim.wall_ns();
        for completion in done.drain(..) {
            let p = procs.iter_mut().find(|it| it.store.owns(completion.tag)).expect("a store asked for it");
            p.store.complete(completion, wall, &mut p.effects);
        }
    }
    panic!("the simulation did not settle in a million polls");
}

/// What `name(0)` points at, read by a new process `n`.
fn pointed_now(sim: Sim, n: u64, root: &Frame) -> (Sim, Option<Key>) {
    let lent = process(sim, n, LentPlant::None, Plant::None, root);
    let got = lent.pointed(TAG, &name(0)).expect("a pointer read");
    (lent.into_store().into_driver(), got)
}

/// Two processes, both open before either commits, point one name in turn: the later commit
/// wins each time, whichever process makes it, and a value one process pointed before and
/// points again over the other's stands. Prints `pointer: two processes, each from a view older
/// than the other's commit: the later commit won 4 times, a value pointed again included`.
#[test]
fn of_two_processes_the_later_commit_wins() {
    let root = test_root();
    let mut sim = targets(Sim::new(43, Faults::default()), 1, &[0, 1, 2, 3], &root);
    let mut a = Proc::open(&mut sim, 2, &root);
    let mut b = Proc::open(&mut sim, 3, &root);
    let mut won = 0;
    for (n, (writer, t)) in [(0u8, 0u32), (1, 1), (0, 0), (1, 2)].into_iter().enumerate() {
        let p = if writer == 0 { &mut a } else { &mut b };
        p.point(&mut sim, t);
        let (back, got) = pointed_now(sim, 10 + n as u64, &root);
        sim = back;
        assert_eq!(got, Some(target(t)), "commit {n}, by process {writer}, to target {t}: not the newest value");
        won += 1;
    }
    println!("pointer: two processes, each from a view older than the other's commit: the later commit won {won} times, a value pointed again included");
}

/// One name pointed in 48 commits, each a segment of its own, its 20 targets in turn (more
/// distinct tiny segments than tier 0's `K`, so merges run; from the 21st commit on a segment's
/// bytes repeat an older one's), compaction after every eighth: the newest value is read after
/// each, and after a reopen. Prints `pointer: N merges between 48 commits kept the newest value
/// of the name`.
#[test]
fn compaction_between_commits_keeps_the_newest_value() {
    let all: [u32; 20] = std::array::from_fn(|i| i as u32);
    let root = test_root();
    let sim = targets(Sim::new(47, Faults::default()), 1, &all, &root);
    let lent = process(sim, 2, LentPlant::None, Plant::None, &root);
    let mut merges = 0;
    let mut last = 0;
    for round in 0..48u32 {
        last = round % 20;
        lent.point(TAG, &name(0), &target(last)).expect("a point");
        lent.commit().expect("a commit");
        if round % 8 == 7 {
            let slices = lent.compact(8).expect("compaction");
            merges += slices.iter().filter(|it| it.segments > 0 && !it.aborted).count();
            assert_eq!(lent.pointed(TAG, &name(0)).expect("a pointer read"), Some(target(last)), "after the compaction of round {round}");
        }
    }
    assert!(merges > 0, "no compaction merged: the test did not cover a merge");
    let (_, got) = pointed_now(lent.into_store().into_driver(), 3, &root);
    assert_eq!(got, Some(target(last)), "after a reopen");
    println!("pointer: {merges} merges between 48 commits kept the newest value of the name");
}
