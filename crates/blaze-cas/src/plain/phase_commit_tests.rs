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
