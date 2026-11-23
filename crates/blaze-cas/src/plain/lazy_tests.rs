//! The lazy open (D114 piece 3, probe.rs) on the simulation, through `Driven`: a segment is
//! mapped at its first probe, an earlier boot's unsealed segment is checked whole at its first
//! hit, the probe order is largest first, and a root answers by name. Each property has its
//! planted control: `Plant::MapsAllAtOpen` (the open maps every row) and
//! `Plant::TrustsUnchecked` (a first hit served unchecked). Rule 8: the oracle is the model of
//! what was put (`value(n)` under `key(n)`), never another store's answer.

use std::path::Path;

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Namespace, Tag},
    store::{HEADER, StoreError},
};
use blaze_mem::Frame;

use super::{
    core::{GRACE_NS, PlainConfig, Plant, TAGS},
    driven::Driven,
    test_root,
};
use crate::Key;

const TAG: Tag = Tag(1);

/// A process's store, its tables in the test's frame.
type Store<'f> = Driven<Sim, &'f Frame>;

pub(super) fn cfg(n: u64, boot: u64, root: &Frame) -> PlainConfig<&Frame> {
    let mut writes = Vec::new_in(root);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(root);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: n * TAGS,
        cap_bytes: u64::MAX,
        boot,
        nonce: 500 + n,
        durable: false,
        machine: Vec::new_in(root),
        writes,
        reads,
        liveness: super::Liveness::Everything,
        lock_step: crate::plain::LockStep::Backoff,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain lazy test key", &n.to_le_bytes()))
}

/// Never empty: 37 to 736 bytes.
fn value(n: u32) -> Vec<u8> {
    let len = 37 + (n as usize * 37) % 700;
    (0..len).map(|i| (n as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

/// A writer of boot 1 on a fresh simulation, committing each range of `commits` as one segment
/// of `LOCAL` (no seal: an unsealed tail): the simulation back, and each commit's segment id.
fn written(seed: u64, commits: &[std::ops::Range<u32>], root: &Frame) -> (Sim, Vec<u64>) {
    let mut w = Driven::with(Sim::new(seed, Faults::default()), Path::new("/sim"), cfg(0, 1, root), root).expect("the writer opens");
    let mut ids = Vec::new();
    for range in commits {
        for n in range.clone() {
            w.put(key(n), Namespace::LOCAL, TAG, &value(n)).expect("a put");
        }
        let committed = w.commit_with(&[]).expect("a commit");
        assert_eq!(committed.segments.len(), 1, "one namespace, one segment");
        ids.push(committed.segments[0]);
    }
    (w.into_driver(), ids)
}

/// A reader process (`n`) of `boot` over `sim`, opened lazily, with `plant`.
fn reader(sim: Sim, n: u64, boot: u64, plant: Plant, root: &Frame) -> Store<'_> {
    Driven::planted(sim, Path::new("/sim"), cfg(n, boot, root), root, plant).expect("the reader opens")
}

/// The bytes under `k` through get and got.
fn read(d: &mut Store<'_>, k: &Key) -> Option<Vec<u8>> {
    let found = d.get(k).expect("a get with no failure")?;
    Some(d.got(found).bytes.to_vec())
}

/// The open maps nothing; one get maps the one segment its key is in (largest first), and the
/// planted open that maps every row is caught by the "mapped of named" oracle.
#[test]
fn the_open_maps_nothing_and_a_get_maps_one_segment() {
    let root = test_root();
    let mapped = |plant: Plant| {
        let (sim, _) = written(11, &[0..2, 10..20, 30..35], &root);
        let mut r = reader(sim, 1, 1, plant, &root);
        let open = r.mapped_of_named();
        assert_eq!(read(&mut r, &key(15)), Some(value(15)), "record 15");
        (open, r.mapped_of_named())
    };
    let (open, after) = mapped(Plant::None);
    assert_eq!(open, (0, 3), "the open mapped a segment");
    assert_eq!(after, (1, 3), "a get in the largest segment mapped more than it");
    println!("lazy open: segments mapped {} of {} named after the open; {} of {} after one get", open.0, open.1, after.0, after.1);
    let (planted, _) = mapped(Plant::MapsAllAtOpen);
    assert_ne!(planted, (0, 3), "the planted open that maps every row went unseen");
    println!("lazy open, planted MapsAllAtOpen: caught (mapped {} of {} after the open)", planted.0, planted.1);
}

/// An earlier boot's unsealed segment whose bytes did not survive (rotted here, standing for a
/// power cut's tear) is mapped at its first probe and checked whole at its first hit: lost by
/// name with its count, the get absent, one full check. The plant that serves the hit
/// unchecked returns bytes that are not the record.
#[test]
fn a_first_hit_checks_the_crash_tail_and_the_plant_serves_it_torn() {
    let root = test_root();
    let outcome = |plant: Plant| {
        let (mut sim, ids) = written(12, &[1..11], &root);
        let file = FileName::new(&format!("plain/seg-{:016x}", ids[0])).unwrap();
        // The first payload's bytes; the index (past the payloads) is left whole. Through
        // `change`, not `rot`: the writer never synced, and `rot` changes only the durable
        // image, under the unsynced bytes that stay on top of it.
        let mut torn = sim.contents(&file);
        for (at, byte) in [0xffu8, 0x00, 0xff].into_iter().enumerate() {
            torn[HEADER + 3 + at] ^= byte | 0x5a;
        }
        sim.change(&file, &torn);
        let mut r = reader(sim, 1, 2, plant, &root);
        let reads: Vec<Option<Vec<u8>>> = (1..11).map(|n| read(&mut r, &key(n))).collect();
        (reads, r.report().clone(), r.core().checks().full, file)
    };
    let (reads, report, full, file) = outcome(Plant::None);
    assert!(reads.iter().all(Option::is_none), "a record of a torn segment was served: {reads:?}");
    assert_eq!(report.lost.iter().map(|it| (it.file.clone(), it.records)).collect::<Vec<_>>(), vec![(file, Some(10))]);
    assert_eq!(full, 1, "one full check at the first hit, none more");
    println!("first hit: torn segment lost by name, get absent, full checks {full}");
    let (planted, ..) = outcome(Plant::TrustsUnchecked);
    let wrong = (1..11u32).zip(planted.iter()).any(|(n, got)| got.as_ref().is_some_and(|it| *it != value(n)));
    assert!(wrong, "a first hit served unchecked went unseen");
    println!("first hit, planted TrustsUnchecked: caught (served bytes that are not the record)");
}

/// One probe order, largest first: a get in the largest segment probes one run. A root is a
/// name in the newest manifest (the per-shape last answer goes through `Driven::root`, never a
/// fixed key with a moving value): set twice, it answers the second key whatever the order.
#[test]
fn the_largest_segment_is_probed_first_and_a_root_answers_by_name() {
    let root = test_root();
    let mut w = Driven::with(Sim::new(13, Faults::default()), Path::new("/sim"), cfg(0, 1, &root), &root).expect("the writer opens");
    for n in 100..103 {
        w.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
    }
    let first = w.root("shape", &[key(100)]).unwrap();
    w.commit_with(&[]).unwrap();
    for n in 200..230 {
        w.put(key(n), Namespace::LOCAL, TAG, &value(n)).unwrap();
    }
    let second = w.root("shape", &[key(200)]).unwrap();
    w.commit_with(&[]).unwrap();
    assert_ne!(first, second);
    let mut r = reader(w.into_driver(), 1, 1, Plant::None, &root);
    assert_eq!(read(&mut r, &key(201)), Some(value(201)));
    assert_eq!(r.probes().most, 1, "a get in the largest segment probed another first");
    let named: Vec<Key> = r.core().roots().into_iter().filter(|(name, _)| name == "shape").map(|(_, k)| k).collect();
    assert_eq!(named, vec![second], "the root's name answers its newest key");
    assert_eq!(read(&mut r, &second), Some(key(200).0.0.to_vec()), "the root's record holds the key it names");
    println!("probe order: largest first, roots by name: the answer is the newest root's key");
}

/// A miss probes every row, mapping and parking the earlier boot's segments with no full
/// check (a miss in an unproved run is a miss); `map_all` then checks each parked segment once,
/// and the store is whole.
#[test]
fn a_miss_parks_and_map_all_checks_each_once() {
    let root = test_root();
    let (sim, _) = written(14, &[1..3, 10..20, 30..35], &root);
    let mut r = reader(sim, 1, 2, Plant::None, &root);
    assert_eq!(read(&mut r, &key(9999)), None);
    let after_miss = (r.mapped_of_named(), r.core().checks().full);
    assert_eq!(after_miss, ((3, 3), 0), "a miss checked a segment");
    r.map_all().unwrap();
    assert!(r.core().whole());
    let checked = r.core().fully_checked().to_vec();
    let mut unique = checked.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!((checked.len(), unique.len()), (3, 3), "each parked segment checked once: {checked:?}");
    for n in (1..3).chain(10..20).chain(30..35) {
        assert_eq!(read(&mut r, &key(n)), Some(value(n)), "record {n}");
    }
    println!("lazy then whole: a miss mapped 3 of 3 with 0 full checks; map_all checked 3, none twice");
}

/// The long-lived holder's rule (`StoreError::Lost`'s doc): a holder that maps every segment
/// within the grace reads every record after another process's compaction unlinked the files
/// (a mapping outlives an unlink); one that kept segments lazily unmapped past the grace is
/// refused `Lost` at its first probe, naming a segment its generation names, never served a
/// miss as if a power cut took it.
#[test]
fn a_holder_that_maps_all_in_the_grace_outlives_the_unlink_and_a_lazy_one_is_refused() {
    let root = test_root();
    let held = |map_first: bool| {
        let (sim, ids) = written(16, &[1..4, 10..20, 30..36], &root);
        let mut r = reader(sim, 1, 1, Plant::None, &root);
        if map_first {
            r.driver().advance(std::time::Duration::from_secs(60));
            r.map_all().expect("map_all inside the grace");
        }
        r.driver().advance(std::time::Duration::from_nanos(GRACE_NS + 1));
        for id in &ids {
            let file = FileName::new(&format!("plain/seg-{id:016x}")).unwrap();
            r.driver().delete(&file);
        }
        let mut reads: Vec<Result<Option<Vec<u8>>, StoreError>> = Vec::new();
        for n in (1..4).chain(10..20).chain(30..36) {
            reads.push(match r.get(&key(n)) {
                Ok(Some(found)) => Ok(Some(r.got(found).bytes.to_vec())),
                Ok(None) => Ok(None),
                Err(why) => Err(why),
            });
        }
        (reads, ids)
    };
    let (reads, _) = held(true);
    for (n, got) in (1..4u32).chain(10..20).chain(30..36).zip(&reads) {
        assert_eq!(got.as_ref().ok(), Some(&Some(value(n))), "record {n} after the unlink, mapped in the grace");
    }
    println!("holder: mapped in the grace, read {} of {} records after the files were unlinked", reads.len(), reads.len());
    let (reads, ids) = held(false);
    match &reads[0] {
        Err(StoreError::Lost { segment, .. }) => assert!(ids.contains(segment), "Lost named segment {segment}, not one of {ids:?}"),
        other => panic!("a lazy holder past the grace was not refused Lost: {other:?}"),
    }
    println!("holder: lazy past the grace, refused Lost naming its segment");
}

/// A check made at a first hit is recorded for the boot: after the reader's seal (which flips
/// what it checked even with nothing to seal), the next process of the same boot checks nothing.
#[test]
fn a_first_hit_check_is_recorded_for_the_boot() {
    let root = test_root();
    let (sim, _) = written(15, &[1..6], &root);
    let mut r = reader(sim, 1, 2, Plant::None, &root);
    assert_eq!(read(&mut r, &key(3)), Some(value(3)));
    assert_eq!(r.core().checks().full, 1);
    assert!(r.seal().unwrap(), "the check's stamp did not flip");
    let mut next = reader(r.into_driver(), 2, 2, Plant::None, &root);
    assert_eq!(read(&mut next, &key(4)), Some(value(4)));
    assert_eq!(next.core().checks().full, 0, "a segment checked once this boot was checked again");
    println!("first-hit check recorded: the next process of the boot checked 0");
}
