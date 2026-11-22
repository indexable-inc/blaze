//! A root rides the same flip as the records it names (main's ruling on the command's `last`
//! answer, 2026-11-16): the command's root update and its new records are committed by ONE flip
//! after the answer is printed, so no generation names a root whose records are absent. Proven
//! on the simulation through `Lent` with a cut at every completion of the commit: from before
//! its first effect (a cut between the answer and the flip) through the flip itself, each
//! followed by the process's death alone and by a power cut. Oracle: after recovery, every key
//! the root `last` names reads back with its own value (rule 8: the model `value(n)`). The
//! planted control (`LentPlant::RootFirst`) flips the root in a commit of its own before the
//! records'.

use std::path::Path;

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_mem::Frame;

use super::{
    core::{PlainConfig, TAGS},
    driven::{Cut, Driven},
    lent::{Lent, LentPlant},
    test_root,
};
use crate::Key;

/// A process's store, its arena in the run's frame.
type Store<'f> = Lent<Sim, &'f Frame>;

const TAG: Tag = Tag(1);
const ROOT: &str = "last";

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
        // A power cut is part of the claim, so the commit syncs: with `durable: false` (the edit
        // path's cache setting) a power cut may tear a segment the manifest names, which the
        // read path reports as `Changed`; that is the cache's promise, not this one's.
        durable: true,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: super::Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain root commit test key", &n.to_le_bytes()))
}

fn value(n: u32) -> Vec<u8> {
    (0..20 + n as usize % 200).map(|i| (n as usize).wrapping_mul(13).wrapping_add(i) as u8).collect()
}

/// The model: the value of the key `k` names, when `k` is one of the test's.
fn model(k: &Key) -> Option<Vec<u8>> {
    (0..200u32).find(|n| key(*n) == *k).map(value)
}

fn process(sim: Sim, n: u64, plant: LentPlant, root: &Frame) -> Store<'_> {
    Lent::planted(Driven::with(sim, Path::new("/sim"), cfg(n, root), root).expect("the store opens"), root, plant)
}

/// One command: the records of `range` put, the root `last` set to name them, one commit.
fn command(lent: &Store<'_>, range: std::ops::Range<u32>) -> Result<(), super::lent::LentError> {
    let mut keys = Vec::new();
    for n in range {
        lent.put(&key(n), Namespace::LOCAL, TAG, &value(n))?;
        keys.push(key(n));
    }
    lent.root(ROOT, &keys)?;
    lent.commit().map(|_| ())
}

/// The keys the root `last` names in the store `lent` opened, each read back: the keys whose
/// bytes are absent, refused by the read path, or not their model's. `None`: no root `last`.
fn unreadable(lent: &Store<'_>) -> Option<Vec<Key>> {
    let (_, at) = lent.roots().into_iter().find(|(name, _)| name == ROOT)?;
    let read = |k: &Key| match lent.get(k) {
        Ok(found) => Ok(found.map(|lend| lend.bytes.to_vec())),
        Err(error) => {
            eprintln!("root commit: the get of a key the root names was refused: {error:?}");
            Err(())
        }
    };
    let Ok(Some(named)) = read(&at) else { return Some(vec![at]) };
    Some(
        named
            .chunks_exact(32)
            .map(|k| Key(ContentId(k.try_into().expect("32 bytes"))))
            .filter(|k| read(k).map_or(true, |bytes| bytes != model(k)))
            .collect(),
    )
}

/// Every cut point of the second command's commit, each with and without a power cut after
/// it: how many recoveries left the root naming a record that does not read back. Each cut is a
/// run of its own, its processes' arenas in a frame of that run, given back before the next.
fn broken(plant: LentPlant) -> (u32, u32) {
    let (mut broken, mut cuts) = (0, 0);
    for at in 0..256u32 {
        for power in [false, true] {
            let root = test_root();
            let first = process(Sim::new(31, Faults::default()), 1, LentPlant::None, &root);
            command(&first, 0..10).expect("the first command");
            let second = process(first.into_store().into_driver(), 2, plant, &root);
            second.cut(Some(Cut::After(at)));
            let reached = command(&second, 100..140).is_ok();
            let mut sim = second.into_store().into_driver();
            if power {
                sim.crash();
            }
            let after = process(sim, 3, LentPlant::None, &root);
            if unreadable(&after).is_some_and(|keys| !keys.is_empty()) {
                broken += 1;
            }
            cuts += 1;
            if reached && power {
                // The commit finished before this cut: every later count is the same run.
                return (broken, cuts);
            }
        }
    }
    (broken, cuts)
}

#[test]
fn a_root_and_its_records_ride_one_flip_through_every_cut() {
    let (bad, cuts) = broken(LentPlant::None);
    assert_eq!(bad, 0, "a recovery left the root naming a record that does not read back");
    assert!(cuts > 4, "the commit was cut at too few points ({cuts}) to cover the flip");
    println!("root commit: {cuts} cuts (process death and power cut, the answer to the flip and inside it); the root named only records that read back");
    let (bad, _) = broken(LentPlant::RootFirst);
    assert!(bad > 0, "the planted root flipped first went unseen");
    println!("root commit, planted RootFirst: caught ({bad} recoveries named absent records)");
}
