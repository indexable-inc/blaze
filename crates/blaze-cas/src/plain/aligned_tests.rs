//! Every record a get lends from a segment starts at a multiple of sixteen ([`Aligned16`]), so a
//! reader reads it, and the sections it cuts at multiples of sixteen inside it, where they lie in
//! the mapping (blaze-cli engine_records.rs; runtime.md C3). Proven on the simulation through
//! `Lent`: after commits of records of every length mod 16, after compactions that merge their
//! segments, and after a cut at every completion of a commit (process death alone and a power
//! cut), each record that reads back is in place, aligned and byte-equal to its model. The
//! planted control (`Plant::Unpadded`) lays a commit's payloads end to end: a recovery reads a
//! record misaligned or refused.

use std::{alloc::Global, path::Path};

use blaze_core::{
    FileName,
    sim::{Faults, Sim},
};
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, Tag};
use blaze_mem::Frame;

use super::{
    core::{Liveness, PlainConfig, Plant, TAGS},
    driven::{Cut, Driven},
    lent::{Aligned16, Lent, LentPlant, NotInPlace},
    test_root,
};
use crate::Key;

/// A process's store, its arena in the run's frame.
type Store<'f> = Lent<Sim, &'f Frame>;

const TAG: Tag = Tag(1);

fn cfg(n: u64) -> PlainConfig {
    let mut writes = Vec::new_in(Global);
    writes.push(Namespace::LOCAL);
    let mut reads = Vec::new_in(Global);
    reads.push(Namespace::LOCAL);
    PlainConfig {
        dir: FileName::new("plain").expect("a name"),
        tags: n * TAGS,
        cap_bytes: u64::MAX,
        boot: 1,
        nonce: 1100 + n,
        // A power cut is part of the claim, so the commit syncs (as root_commit_tests.rs).
        durable: true,
        machine: Vec::new_in(Global),
        writes,
        reads,
        liveness: Liveness::Everything,
    }
}

fn key(n: u32) -> Key {
    Key(ContentId::of("plain aligned test key", &n.to_le_bytes()))
}

/// Every length mod 16 among any 16 consecutive `n`, so no payload but one in sixteen ends where
/// the next would start aligned without padding.
fn value(n: u32) -> Vec<u8> {
    (0..1 + n as usize % 97).map(|i| (n as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
}

fn process(sim: Sim, n: u64, core: Plant, root: &Frame) -> Store<'_> {
    Lent::planted(Driven::planted(sim, Path::new("/sim"), cfg(n), core).expect("the store opens"), root, LentPlant::None)
}

fn command(lent: &Store<'_>, range: std::ops::Range<u32>) -> Result<(), super::lent::LentError> {
    for n in range {
        lent.put(&key(n), Namespace::LOCAL, TAG, &value(n))?;
    }
    lent.commit().map(|_| ())
}

/// How the record of `n` reads back from `lent`: in place, aligned and its model's bytes.
#[derive(Debug, PartialEq, Eq)]
enum Read {
    InPlace,
    Absent,
    /// Lent, but from the arena or misaligned.
    NotInPlace(NotInPlace),
    /// Lent in place with other bytes, or the get refused.
    Wrong,
}

fn read(lent: &Store<'_>, n: u32) -> Read {
    match lent.get(&key(n)) {
        Ok(None) => Read::Absent,
        Ok(Some(lend)) => match lend.in_place() {
            Ok(aligned) if aligned.bytes() == &value(n)[..] => Read::InPlace,
            Ok(_) => Read::Wrong,
            Err(why) => Read::NotInPlace(why),
        },
        Err(error) => {
            eprintln!("aligned: the get of key {n} was refused: {error:?}");
            Read::Wrong
        }
    }
}

/// THE TYPE (rule 76): a slice starting at a multiple of sixteen is taken; one starting a byte
/// past it is refused, naming by how much; an empty one is taken; a cut keeps the property.
#[test]
fn aligned16_takes_only_bytes_at_a_multiple_of_sixteen() {
    #[repr(C, align(16))]
    struct Blocks([u8; 64]);
    let blocks = Blocks([7; 64]);
    let whole = Aligned16::new(&blocks.0).expect("a 16-aligned array");
    assert_eq!(whole.len(), 64);
    let refused = Aligned16::new(&blocks.0[1..]).expect_err("one byte past a multiple of sixteen");
    assert_eq!((refused.by, refused.len), (1, 63));
    assert!(Aligned16::new(&blocks.0[3..3]).is_ok(), "an empty slice reads no byte");
    assert_eq!(whole.cut(16, 32).expect("a cut at 16").map(|part| part.len()), Some(32));
    assert_eq!(whole.cut(8, 8).map(|part| part.map(|part| part.len())).map_err(|why| why.by), Err(8), "a cut at 8 is misaligned");
    assert!(whole.cut(48, 32).expect("past the end is no cut").is_none());
    println!("aligned: Aligned16 took 16-aligned and empty slices, refused one 1 byte past and a cut 8 past");
}

/// An own put is the arena's copy, refused as not in place; after its commit the same key reads
/// from the segment's mapping, aligned.
#[test]
fn an_own_put_is_refused_in_place_until_its_commit() {
    let root = test_root();
    let lent = process(Sim::new(51, Faults::default()), 1, Plant::None, &root);
    lent.put(&key(1), Namespace::LOCAL, TAG, &value(1)).expect("a put");
    assert_eq!(read(&lent, 1), Read::NotInPlace(NotInPlace::Arena { len: value(1).len() }));
    // The lend above marked its chunk kept; a key no get lent from is freed by the commit.
    lent.put(&key(2), Namespace::LOCAL, TAG, &value(2)).expect("a put");
    lent.commit().expect("the commit");
    let lent = process(lent.into_store().into_driver(), 2, Plant::None, &root);
    assert_eq!((read(&lent, 1), read(&lent, 2)), (Read::InPlace, Read::InPlace));
}

/// Asking whether a put is held (`Lent::holds`) lends nothing: after the commit, in the SAME
/// process, the key asked about and the key put beside it read from the segment's mapping. The
/// control (rule 23): the same question asked by a get keeps the chunk, and the key beside it,
/// never read before, is refused in place for the rest of the command. That was blaze-cli
/// `put_record`'s equal-bytes check on a real project: an expansion record two units put kept
/// the names records beside it in the arena, and every reader of them was refused.
#[test]
fn asking_whether_a_put_is_held_keeps_no_chunk_past_its_commit() {
    let root = test_root();
    let lent = process(Sim::new(61, Faults::default()), 1, Plant::None, &root);
    lent.put(&key(1), Namespace::LOCAL, TAG, &value(1)).expect("a put");
    lent.put(&key(2), Namespace::LOCAL, TAG, &value(2)).expect("a put");
    assert!(lent.holds(&key(1), &value(1)).expect("asked"), "an own put is held with its bytes");
    assert!(!lent.holds(&key(1), &value(2)).expect("asked"), "and not with other bytes");
    assert!(!lent.holds(&key(3), &value(3)).expect("asked"), "a key never put is not held");
    lent.commit().expect("the commit");
    assert_eq!((read(&lent, 1), read(&lent, 2)), (Read::InPlace, Read::InPlace), "asked by `holds`: both in place after the commit");
    assert!(lent.holds(&key(1), &value(1)).expect("asked"), "a committed record is held with its bytes");
    assert!(!lent.holds(&key(1), &value(2)).expect("asked"), "and not with other bytes");

    let control = process(Sim::new(63, Faults::default()), 1, Plant::None, &root);
    control.put(&key(1), Namespace::LOCAL, TAG, &value(1)).expect("a put");
    control.put(&key(2), Namespace::LOCAL, TAG, &value(2)).expect("a put");
    assert!(control.get(&key(1)).expect("a get").is_some_and(|lend| lend.bytes == &value(1)[..]), "the same question, asked by a get");
    control.commit().expect("the commit");
    assert_eq!(read(&control, 2), Read::NotInPlace(NotInPlace::Arena { len: value(2).len() }), "the control: a get kept the chunk, so the key beside it is not in place");
    println!("holds: asked by comparison, both keys in place after the commit; asked by a get, the key beside it stayed in the arena");
}

/// Commits of records of every length mod 16, a compaction after every eighth (more segments
/// than one slice keeps, so merges run): every record reads in place and aligned after each, and
/// after a reopen.
#[test]
fn every_record_is_aligned_after_commits_and_compactions() {
    let root = test_root();
    let lent = process(Sim::new(53, Faults::default()), 1, Plant::None, &root);
    let mut merges = 0;
    let mut upto = 0u32;
    for round in 0..40u32 {
        command(&lent, upto..upto + 17).expect("a commit");
        upto += 17;
        if round % 8 == 7 {
            let slices = lent.compact(8).expect("compaction");
            merges += slices.iter().filter(|it| it.segments > 0 && !it.aborted).count();
            let bad: Vec<u32> = (0..upto).filter(|n| read(&lent, *n) != Read::InPlace).collect();
            assert!(bad.is_empty(), "after the compaction of round {round}: keys not in place {bad:?}");
        }
    }
    assert!(merges > 0, "no compaction merged: the test did not cover a merge");
    let lent = process(lent.into_store().into_driver(), 2, Plant::None, &root);
    let bad: Vec<u32> = (0..upto).filter(|n| read(&lent, *n) != Read::InPlace).collect();
    assert!(bad.is_empty(), "after a reopen: keys not in place {bad:?}");
    println!("aligned: {upto} records of every length mod 16 in place and aligned after 40 commits and {merges} merges, and a reopen");
}

/// Every cut of the second command's commit, each with and without a power cut after it: how
/// many recoveries read a record of either command misaligned, refused or with other bytes (a
/// second command's record may be absent when its commit did not finish). Each cut is a run of
/// its own, its three processes' arenas in a frame of that run, given back before the next.
fn broken(core: Plant) -> (u32, u32) {
    let (mut broken, mut cuts) = (0, 0);
    for at in 0..256u32 {
        for power in [false, true] {
            let root = test_root();
            let first = process(Sim::new(57, Faults::default()), 1, Plant::None, &root);
            command(&first, 0..40).expect("the first command");
            let second = process(first.into_store().into_driver(), 2, core, &root);
            second.cut(Some(Cut::After(at)));
            let reached = command(&second, 100..160).is_ok();
            let mut sim = second.into_store().into_driver();
            if power {
                sim.crash();
            }
            let after = process(sim, 3, Plant::None, &root);
            let first_bad = (0..40).any(|n| read(&after, n) != Read::InPlace);
            let second_bad = (100..160).any(|n| match read(&after, n) {
                Read::InPlace => false,
                Read::Absent => reached,
                Read::NotInPlace(_) | Read::Wrong => true,
            });
            if first_bad || second_bad {
                broken += 1;
            }
            cuts += 1;
            if reached && power {
                // The commit finished before this cut: every later cut is the same run.
                return (broken, cuts);
            }
        }
    }
    (broken, cuts)
}

/// Prints `aligned: N cuts (process death and power cut, through the commit); every record that
/// read back was in place and aligned`, then `aligned, planted Unpadded: caught (N recoveries)`.
#[test]
fn every_record_is_aligned_after_a_crash_at_every_cut() {
    let (bad, cuts) = broken(Plant::None);
    assert_eq!(bad, 0, "a recovery read a record misaligned, refused or with other bytes");
    assert!(cuts > 4, "the commit was cut at too few points ({cuts}) to cover the flip");
    println!("aligned: {cuts} cuts (process death and power cut, through the commit); every record that read back was in place and aligned");
    // The planted layout is caught at the first place that can see it: in a debug build that is the
    // store's own assertion where an entry is encoded (format.rs `Entry::encode_into`: an offset that is
    // not a multiple of 16), before any recovery; in a release build the recoveries below see it.
    match std::panic::catch_unwind(|| broken(Plant::Unpadded)) {
        Ok((bad, _)) => {
            assert!(bad > 0, "the planted unpadded layout went unseen");
            println!("aligned, planted Unpadded: caught ({bad} recoveries read a record misaligned or refused)");
        }
        Err(_) => println!("aligned, planted Unpadded: caught by the store's alignment assertion at encode"),
    }
}
