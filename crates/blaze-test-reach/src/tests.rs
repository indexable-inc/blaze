//! The record and the reach: the round trip as a property, every limit one over refused by name,
//! a reach equal in bytes whatever its arrival order (rule 25), the reusable byte, the decision's
//! refusals and the audit.

use std::format;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config as SimConfig, Tape, check};

use crate::reach::ROW;
use crate::record::HEADER;
use crate::{
    Audit, Because, CellKind, Digest, Enter, Grants, MAX_READS, NotReusable, Out, Reach, ReachError, ReachOpts, Reached, Read, RecordError, Reusable, TestRecord, audit, enter, enter_stored,
};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn oracle(holds: bool, why: impl FnOnce() -> std::string::String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}

fn reached(reads: &[Read]) -> Result<Reached<Alloc<'static>>, ReachError> {
    let mut reach = Reach::new(ReachOpts { cap: MAX_READS, alloc: alloc() })?;
    for read in reads {
        reach.add(*read)?;
    }
    reach.finish()
}

fn encoded(test: Digest, out: Out, reusable: Reusable, reach: &Reached<Alloc<'static>>) -> StdVec<u8> {
    let mut bytes = blaze_own::Vec::new_in(alloc());
    TestRecord::encode(test, out, reusable, reach, &mut bytes);
    bytes.as_slice().to_vec()
}

/// A byte of `tape`, below `n`.
fn byte(tape: &Tape, n: u64, label: &'static str) -> u8 {
    u8::try_from(tape.below(n, label)).unwrap_or(u8::MAX)
}

/// Reads over a small universe, so names repeat; a cell's value is fixed for the case (one value
/// a name), salted so cases differ.
fn draw_reads(tape: &Tape) -> StdVec<Read> {
    let salt = byte(tape, 256, "salt");
    let n = tape.below(48, "reads");
    (0..n)
        .map(|_| {
            let kind = CellKind::ALL[tape.index(CellKind::ALL.len(), "kind")];
            let name = byte(tape, 16, "name");
            let value = name.wrapping_mul(3).wrapping_add(kind as u8).wrapping_add(salt);
            Read { kind, name: Digest([name; 32]), value: Digest([value; 32]) }
        })
        .collect()
}

fn draw_out(tape: &Tape) -> Out {
    match tape.index(3, "out") {
        0 => Out::Passed,
        1 => Out::Failed { output: Digest([1 + byte(tape, 255, "output"); 32]) },
        _ => Out::Ignored,
    }
}

fn draw_reusable(tape: &Tape) -> Reusable {
    match NotReusable::from_byte(byte(tape, 7, "reusable")) {
        Some(why) => Reusable::No(why),
        None => Reusable::Yes,
    }
}

fn round_trip(tape: &Tape) -> Result<(), Broke> {
    let reads = draw_reads(tape);
    let test = Digest([byte(tape, 256, "test"); 32]);
    let out = draw_out(tape);
    let reusable = draw_reusable(tape);
    let reach = reached(&reads).map_err(|error| Broke::Harness(format!("{error}")))?;
    let bytes = encoded(test, out, reusable, &reach);
    let record = TestRecord::open(&bytes).map_err(|error| Broke::Oracle(format!("an encoded record does not open: {error}")))?;
    oracle(record.test() == test && record.out() == out && record.reusable() == reusable, || format!("fields: {record:?}"))?;
    oracle(record.digest() == reach.digest(), || "digest".into())?;
    let back: StdVec<Read> = record.reads().collect();
    oracle(back.as_slice() == reach.reads(), || format!("rows: {back:?} against {:?}", reach.reads()))?;
    oracle(record.len() == back.len(), || "len".into())?;
    // Re-encoded from what was opened: equal bytes.
    let again = reached(&back).map_err(|error| Broke::Oracle(format!("an opened reach is refused: {error}")))?;
    oracle(encoded(record.test(), record.out(), record.reusable(), &again) == bytes, || "re-encoded bytes differ".into())?;
    // Sorted, each cell once.
    oracle(back.windows(2).all(|pair| pair[0] < pair[1]), || "rows not strictly ascending".into())
}

#[test]
fn record_round_trips() {
    check(SimConfig::cases(400), round_trip);
}

fn order_free(tape: &Tape) -> Result<(), Broke> {
    let reads = draw_reads(tape);
    let mut other = reads.clone();
    // Some cells again, as a slot asked twice or a file read twice.
    for _ in 0..tape.below(8, "again") {
        if let Some(read) = tape.pick(&reads, "repeated") {
            other.push(*read);
        }
    }
    tape.shuffle(&mut other, "arrival");
    let a = reached(&reads).map_err(|error| Broke::Harness(format!("{error}")))?;
    let b = reached(&other).map_err(|error| Broke::Oracle(format!("the reordered reach is refused: {error}")))?;
    let (out, test) = (Out::Passed, Digest([7; 32]));
    oracle(a.digest() == b.digest(), || "two arrival orders of one set: digests differ".into())?;
    oracle(encoded(test, out, Reusable::Yes, &a) == encoded(test, out, Reusable::Yes, &b), || "two arrival orders of one set: bytes differ".into())
}

#[test]
fn reach_is_order_free() {
    check(SimConfig::cases(400), order_free);
}

#[test]
fn reach_settles_many_arrivals() {
    // Past `MERGE_FLOOR` several times: the merging path, each cell once, sorted.
    let mut reach = Reach::new(ReachOpts { cap: 1000, alloc: alloc() }).expect("a cap under the max");
    for round in 0..5u8 {
        for i in 0..200u8 {
            let i = i.wrapping_add(round.wrapping_mul(7)) % 200;
            reach.add(Read::code([i; 32], [i.wrapping_add(1); 32])).expect("under the cap");
        }
    }
    let done = reach.finish().expect("under the cap");
    assert_eq!(done.reads().len(), 200);
    assert!(done.reads().windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn reach_cap_one_over_is_refused() {
    let read = |i: u8| Read::file(&[i], b"bytes");
    let mut at_cap = Reach::new(ReachOpts { cap: 4, alloc: alloc() }).expect("a cap under the max");
    for i in 0..4 {
        at_cap.add(read(i)).expect("at the cap");
        at_cap.add(read(i)).expect("a repeat counts once");
    }
    assert_eq!(at_cap.finish().map(|done| done.reads().len()), Ok(4));
    let mut over = Reach::new(ReachOpts { cap: 4, alloc: alloc() }).expect("a cap under the max");
    for i in 0..5 {
        over.add(read(i)).expect("settled only at the finish");
    }
    assert_eq!(over.finish().err(), Some(ReachError::Over { cap: 4 }));
    assert_eq!(Reach::new(ReachOpts { cap: MAX_READS + 1, alloc: alloc() }).err(), Some(ReachError::CapAboveMax { cap: MAX_READS + 1 }));
    assert!(Reach::new(ReachOpts { cap: MAX_READS, alloc: alloc() }).is_ok());
}

#[test]
fn one_cell_two_values_is_refused() {
    let mut reach = Reach::new(ReachOpts { cap: 8, alloc: alloc() }).expect("a cap under the max");
    reach.add(Read::env(b"HOME", Some(b"/a"))).expect("first");
    reach.add(Read::env(b"HOME", None)).expect("settled at the finish");
    assert!(matches!(reach.finish().err(), Some(ReachError::TwoValues { kind: CellKind::Env, .. })));
}

/// Three rows (two code, one datum), a pass, reusable.
fn base() -> StdVec<u8> {
    let reads = [Read::code([1; 32], [2; 32]), Read::code([3; 32], [4; 32]), Read::datum([5; 32], [6; 32])];
    let reach = reached(&reads).expect("three cells");
    encoded(Digest([9; 32]), Out::Passed, Reusable::Yes, &reach)
}

fn refused(edit: impl FnOnce(&mut StdVec<u8>)) -> Option<RecordError> {
    let mut bytes = base();
    edit(&mut bytes);
    TestRecord::open(&bytes).err()
}

fn put_count(bytes: &mut [u8], count: u32) {
    bytes[112..116].copy_from_slice(&count.to_le_bytes());
}

#[test]
fn record_limits_one_over_are_refused_by_name() {
    assert!(TestRecord::open(&base()).is_ok());
    assert_eq!(base().len(), HEADER + 3 * ROW);
    assert_eq!(refused(|b| b.truncate(HEADER - 1)), Some(RecordError::Short { len: HEADER - 1 }));
    assert_eq!(refused(|b| b[0] = b'X'), Some(RecordError::Magic));
    assert_eq!(refused(|b| b[4] = 2), Some(RecordError::Version { found: 2 }));
    assert_eq!(refused(|b| b[8] = 3), Some(RecordError::OutTag { found: 3 }));
    assert_eq!(refused(|b| b[9] = 7), Some(RecordError::ReusableTag { found: 7 }));
    assert_eq!(refused(|b| b[10] = 1), Some(RecordError::NotZero { at: 10 }));
    assert_eq!(refused(|b| b[15] = 1), Some(RecordError::NotZero { at: 15 }));
    assert_eq!(refused(|b| b[116] = 1), Some(RecordError::NotZero { at: 116 }));
    // A pass carries no output key.
    assert_eq!(refused(|b| b[80] = 1), Some(RecordError::NotZero { at: 80 }));
    let over = u32::try_from(MAX_READS + 1).expect("the cap fits a u32");
    assert_eq!(refused(|b| put_count(b, over)), Some(RecordError::TooManyReads { count: over }));
    // At the cap the count is trusted, and the length is what refuses.
    let at_cap = u32::try_from(MAX_READS).expect("the cap fits a u32");
    let len = HEADER + 3 * ROW;
    assert_eq!(refused(|b| put_count(b, at_cap)), Some(RecordError::Length { len, want: HEADER + MAX_READS * ROW }));
    assert_eq!(refused(|b| b.push(0)), Some(RecordError::Length { len: len + 1, want: len }));
    assert_eq!(refused(|b| b.truncate(len - 1)), Some(RecordError::Length { len: len - 1, want: len }));
    assert_eq!(refused(|b| b[HEADER] = 4), Some(RecordError::RowKind { row: 0, found: 4 }));
    assert_eq!(refused(|b| b[HEADER + 7] = 1), Some(RecordError::RowPadding { row: 0 }));
    assert_eq!(
        refused(|b| {
            let (first, second) = b[HEADER..HEADER + 2 * ROW].split_at_mut(ROW);
            first.swap_with_slice(second);
        }),
        Some(RecordError::Order { row: 1 })
    );
    assert_eq!(refused(|b| b[HEADER + ROW + 8..HEADER + ROW + 40].copy_from_slice(&[1; 32])), Some(RecordError::TwoValues { row: 1 }));
    assert_eq!(refused(|b| b[48] ^= 1), Some(RecordError::Digest));
    assert_eq!(refused(|b| b[HEADER + 2 * ROW + 71] ^= 1), Some(RecordError::Digest));
}

#[test]
fn reusable_from_grants_and_its_byte() {
    let none = Grants { clock: false, network: false, os_randomness: false };
    assert_eq!(Reusable::of(none), Reusable::Yes);
    assert_eq!(Reusable::of(Grants { clock: true, ..none }), Reusable::No(NotReusable::Clock));
    assert_eq!(Reusable::of(Grants { network: true, ..none }), Reusable::No(NotReusable::Network));
    assert_eq!(Reusable::of(Grants { os_randomness: true, ..none }), Reusable::No(NotReusable::Randomness));
    assert_eq!(Reusable::Yes.byte(), 0);
    for byte in 1..=6u8 {
        let why = NotReusable::from_byte(byte).expect("a reason");
        assert_eq!(Reusable::No(why).byte(), byte);
    }
    assert_eq!(NotReusable::from_byte(7), None);
    assert_eq!(NotReusable::from_byte(0), None);
}

#[test]
fn enter_names_the_first_moved_cell_and_every_refusal() {
    let bytes = base();
    let test = Digest([9; 32]);
    let record = TestRecord::open(&bytes).expect("the base opens");
    assert_eq!(enter(&record, |_| false), Enter::Reused);
    // Both code rows moved: the first in the record's order is named.
    let first = Read::code([1; 32], [2; 32]);
    assert_eq!(enter(&record, |read| read.kind == CellKind::Code), Enter::Rerun { because: Because::Moved(first) });
    let datum = Read::datum([5; 32], [6; 32]);
    assert_eq!(enter(&record, |read| read.kind == CellKind::Datum), Enter::Rerun { because: Because::Moved(datum) });
    assert_eq!(enter_stored(test, None, |_| false), Enter::Rerun { because: Because::NoRecord });
    assert_eq!(enter_stored(test, Some(&bytes[..10]), |_| false), Enter::Rerun { because: Because::Unreadable(RecordError::Short { len: 10 }) });
    assert_eq!(enter_stored(Digest([8; 32]), Some(bytes.as_slice()), |_| false), Enter::Rerun { because: Because::OtherTest { found: test } });
    assert_eq!(enter_stored(test, Some(bytes.as_slice()), |_| false), Enter::Reused);
    // Not reusable: reruns with nothing moved.
    let reach = reached(&[first]).expect("one cell");
    let clock = encoded(test, Out::Passed, Reusable::No(NotReusable::Clock), &reach);
    assert_eq!(enter_stored(test, Some(clock.as_slice()), |_| false), Enter::Rerun { because: Because::NotReusable(NotReusable::Clock) });
}

#[test]
fn audit_compares_out_and_reach() {
    let bytes = base();
    let record = TestRecord::open(&bytes).expect("the base opens");
    let same = reached(&record.reads().collect::<StdVec<_>>()).expect("its own reach");
    assert_eq!(audit(&record, Out::Passed, &same), Audit::Same);
    let failed = Out::Failed { output: Digest([3; 32]) };
    assert_eq!(audit(&record, failed, &same), Audit::OutDiffers { was: Out::Passed, now: failed });
    let other = Read::code([3; 32], [4; 32]);
    let fewer = reached(&[Read::code([1; 32], [2; 32]), Read::datum([5; 32], [6; 32])]).expect("two cells");
    assert_eq!(audit(&record, Out::Passed, &fewer), Audit::ReachDiffers { was: Some(other), now: Some(Read::datum([5; 32], [6; 32])) });
}
