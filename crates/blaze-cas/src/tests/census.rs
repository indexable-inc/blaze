//! [`Store::census`]: what a store holds by kind, counted from its rows alone.

use super::*;
use crate::census::{Census, INDEX_SLOT};
use crate::disk;
use crate::read::Place;

/// A value of every kind, each in the encoding its kind's readers expect.
fn every_kind() -> Vec<(Key, Kind, Vec<u8>)> {
    let pin = crate::collect::Pin { name: "a pin".into(), why: "a census test".into(), when: 1, keys: vec![key(1)] };
    vec![
        (key(1), Kind::Query, text(1, 3_000)),
        (key(2), Kind::Code, noise(64 << 10, 2)),
        (key(3), Kind::Debug, text(3, 800)),
        (key(4), Kind::Run, run_output(4)),
        (key(5), Kind::Output, noise(2 << 10, 5)),
        (key(6), Kind::Image, text(6, 5_000)),
        (key(7), Kind::Machine, noise(12 << 10, 7)),
        (key(8), Kind::Derived, crate::derived::encode(&key(5), 2 << 10, &[(16, b"written over".to_vec())]).unwrap()),
        (key(9), Kind::Pin, pin.encode()),
        (key(10), Kind::Set, crate::sets::encode_leaf(&[key(1), key(2)])),
    ]
}

/// The oracle. Each kind's records and raw bytes are the values put of it (`put`, the ones
/// flushed, and `late`, still in a writer's segment); the one value kept as a file of its own
/// is the whole files; and what the census puts in packs, the flushed records' stored bytes and
/// overhead less their index slots, is the packs' files' bytes but for what no record owns:
/// each pack's header and the padding of its last block.
fn check(
    census: &Census,
    put: &[(Key, Kind, Vec<u8>)],
    late: &[(Key, Kind, Vec<u8>)],
    sim: &Sim,
) -> Result<(), String> {
    for it in &census.kinds {
        let of: Vec<_> = put.iter().chain(late).filter(|(_, kind, _)| *kind == it.kind).collect();
        let raw: u64 = of.iter().map(|(_, _, bytes)| bytes.len() as u64).sum();
        if it.records != of.len() as u64 || it.raw != raw {
            return Err(format!("{:?}: {} records of {} bytes counted, {} of {raw} put", it.kind, it.records, it.raw, of.len()));
        }
    }
    let machine = put.iter().find(|(_, kind, _)| *kind == Kind::Machine).map_or(0, |(_, _, it)| it.len() as u64);
    if census.whole_files != (1, machine) {
        return Err(format!("whole files {:?}, not the one of {machine} bytes", census.whole_files));
    }
    let packs: Vec<FileName> = (0..)
        .map(|at| name(&format!("store/{}", disk::pack_file(at))))
        .take_while(|it| sim.exists(it))
        .collect();
    let files: u64 = packs.iter().map(|it| sim.contents(it).len() as u64).sum();
    let index: u64 = (0..2u8)
        .map(|slot| name(&format!("store/{}", disk::index_file(slot))))
        .filter(|it| sim.exists(it))
        .map(|it| sim.contents(&it).len() as u64)
        .max()
        .unwrap_or(0);
    let late_bytes: u64 = late.iter().map(|(_, _, it)| disk::span_of(it.len() as u64).unwrap()).sum();
    let counted: u64 = census.kinds.iter().map(|it| it.stored + it.overhead).sum();
    let slots = census.kinds.iter().map(|it| it.records).sum::<u64>() * INDEX_SLOT;
    // What the flushed records take in packs, at least (no index slot) and at most (all).
    let (least, most) = (counted.saturating_sub(late_bytes).saturating_sub(slots), counted.saturating_sub(late_bytes));
    let unowned = packs.len() as u64 * (disk::PACK_HEADER + disk::BLOCK);
    if most > files + index || least + unowned + index < files {
        return Err(format!(
            "the census puts {least} to {most} bytes in packs of {files} bytes and an index of {index}, \
             {unowned} of them owned by no record at most"
        ));
    }
    Ok(())
}

/// The invariant: the census counts every record of every kind once, flushed or still in a
/// writer's segment, with its raw length, the store's whole files as they are, and stored
/// bytes plus overhead that sum to the packs' files' sizes (and the index's) but for each
/// pack's header and last block's padding. No value is read for it. The control: a census that
/// leaves out the records still in a segment is caught.
#[test]
fn the_census_counts_every_kind_and_sums_to_the_files() {
    let mut sim = Sim::new(21, quiet());
    let mut store = open(&mut sim, "store");
    let put = every_kind();
    for (key, kind, bytes) in &put {
        store.put(*key, *kind, bytes).unwrap();
    }
    flush(&mut sim, &mut store);
    // Noise, so a segment keeps it as it is and its span is known.
    let late = vec![(key(100), Kind::Code, noise(300, 100)), (key(101), Kind::Query, noise(700, 101))];
    for (key, kind, bytes) in &late {
        store.put(*key, *kind, bytes).unwrap();
    }
    let read = sim.bytes_read();
    let census = store.census();
    assert_eq!(sim.bytes_read(), read, "the census read a file");
    assert_eq!(check(&census, &put, &late, &sim), Ok(()), "{census:#?}");
    assert!(census.layers.is_empty());

    let planted = store.census_of(|found| !matches!(found.place, Place::Segment(_)));
    assert!(check(&planted, &put, &late, &sim).is_err(), "the oracle missed a census that skips a segment's records");
}
