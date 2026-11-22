//! The record and the reach: the round trip as a property, every limit one over refused by name,
//! a reach equal in bytes whatever its arrival order (rule 25), the reusable byte, the decision's
//! refusals and the audit.

use core::cmp::Reverse;
use std::collections::BTreeMap;
use std::format;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config as SimConfig, Tape, check};

use crate::reach::{MERGE_FLOOR, ROW};
use crate::record::HEADER;
use crate::{
    Audit, Because, CellKind, Digest, Enter, ExecutionPlan, Grants, ImpactIndex, MAX_READS, NotReusable, Out, Reach, ReachError, ReachOpts, Reached, Read, RecordError, Reusable, TestHistory,
    TestRecord, TierAdvice, TierOpts, advise_tier_up, advise_tier_up_for_plan, audit, enter, enter_stored,
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

fn reach_batch_prop(tape: &Tape) -> Result<(), Broke> {
    let mut reads = draw_reads(tape);
    // Optionally make reads large enough to cross `MERGE_FLOOR` multiple times, and mix sorted +
    // unsorted runs to exercise both fast paths and the general settle path.
    let extra = tape.below(180, "extra_reads") as usize;
    let salt = byte(tape, 256, "extra_salt");
    for i in 0..extra {
        let name = u8::try_from(i % 200).unwrap_or(0);
        let kind = CellKind::ALL[i % CellKind::ALL.len()];
        let val = name.wrapping_mul(5).wrapping_add(kind as u8).wrapping_add(salt);
        reads.push(Read { kind, name: Digest([name; 32]), value: Digest([val; 32]) });
    }
    if tape.one_in(2, "sorted_input") {
        reads.sort_unstable();
        if tape.one_in(2, "dedup_input") {
            reads.dedup();
        }
    }

    let expected = reached(&reads);

    let mut batched = Reach::new(ReachOpts { cap: MAX_READS, alloc: alloc() }).map_err(|e| Broke::Harness(format!("{e}")))?;
    let mut rest = reads.as_slice();
    while !rest.is_empty() {
        let chunk = (1 + tape.index(rest.len().min(96), "chunk")).min(rest.len());
        if let Err(err) = batched.add_batch(&rest[..chunk]) {
            return oracle(expected.as_ref().err() == Some(&err), || format!("add_batch err {err:?} vs expected {:?}", expected.err()));
        }
        rest = &rest[chunk..];
    }
    let got = batched.finish();
    match (expected, got) {
        (Ok(a), Ok(b)) => {
            oracle(a.reads() == b.reads(), || "add_batch reads differ from add".into())?;
            oracle(a.digest() == b.digest(), || "add_batch digest differs from add".into())
        }
        (Err(e1), Err(e2)) => oracle(e1 == e2, || format!("add_batch err {e2:?} vs {e1:?}")),
        (Ok(_), Err(e)) => Err(Broke::Oracle(format!("add_batch failed with {e:?} where add succeeded"))),
        (Err(e), Ok(_)) => Err(Broke::Oracle(format!("add_batch succeeded where add failed with {e:?}"))),
    }
}

#[test]
fn reach_add_batch_matches_add_property() {
    check(SimConfig::cases(400), reach_batch_prop);
}

#[test]
fn reach_settle_fast_paths_and_errors() {
    // 1. Strictly ascending arrivals crossing `MERGE_FLOOR` several times (fast path 1).
    let mut asc = Reach::new(ReachOpts { cap: 512, alloc: alloc() }).expect("under max");
    let mut expected_rows = StdVec::new();
    for i in 0..(MERGE_FLOOR * 4 + 17) {
        let mut name = [0u8; 32];
        name[0..8].copy_from_slice(&(i as u64).to_be_bytes());
        let read = Read::code(name, [1; 32]);
        expected_rows.push(read);
        asc.add(read).expect("under cap");
    }
    let done = asc.finish().expect("strictly ascending");
    assert_eq!(done.reads(), expected_rows.as_slice());

    // 2. Sorted prefix followed by an unsorted tail strictly greater than the prefix max (fast path 2),
    //    with internal duplicates.
    let mut split = Reach::new(ReachOpts { cap: 256, alloc: alloc() }).expect("under max");
    for i in 0..=(MERGE_FLOOR as u8) {
        split.add(Read::code([i; 32], [i; 32])).expect("prefix settles on last item");
    }
    // Tail all in `CellKind::Datum` (strictly > `CellKind::Code`), arriving reversed with duplicates.
    let tail = [
        Read::datum([30; 32], [1; 32]),
        Read::datum([10; 32], [1; 32]),
        Read::datum([30; 32], [1; 32]),
        Read::datum([20; 32], [1; 32]),
        Read::datum([10; 32], [1; 32]),
    ];
    split.add_batch(&tail).expect("tail batch");
    let done_split = split.finish().expect("fast path 2");
    assert_eq!(done_split.reads().len(), MERGE_FLOOR + 1 + 3);
    assert!(done_split.reads().windows(2).all(|p| p[0] < p[1]));

    // 3. Fast path 2 catches `TwoValues` inside the tail.
    let mut tail_conflict = Reach::new(ReachOpts { cap: 256, alloc: alloc() }).expect("under max");
    for i in 0..=(MERGE_FLOOR as u8) {
        tail_conflict.add(Read::code([i; 32], [1; 32])).expect("prefix");
    }
    tail_conflict
        .add_batch(&[Read::datum([9; 32], [2; 32]), Read::datum([9; 32], [1; 32])])
        .expect("deferred to finish");
    assert_eq!(
        tail_conflict.finish().err(),
        Some(ReachError::TwoValues {
            kind: CellKind::Datum,
            name: Digest([9; 32]),
            first: Digest([1; 32]),
            second: Digest([2; 32]),
        })
    );

    // 4. Boundary conflict between settled prefix's last element and tail's first element.
    let mut boundary_conflict = Reach::new(ReachOpts { cap: 256, alloc: alloc() }).expect("under max");
    for i in 0..=(MERGE_FLOOR as u8) {
        boundary_conflict.add(Read::code([i; 32], [1; 32])).expect("prefix");
    }
    let last = MERGE_FLOOR as u8;
    boundary_conflict.add(Read::code([last; 32], [2; 32])).expect("queued");
    assert_eq!(
        boundary_conflict.finish().err(),
        Some(ReachError::TwoValues {
            kind: CellKind::Code,
            name: Digest([last; 32]),
            first: Digest([1; 32]),
            second: Digest([2; 32]),
        })
    );

    // 5. `add_batch` enforces cap past `MERGE_FLOOR`.
    let mut batch_over = Reach::new(ReachOpts { cap: 10, alloc: alloc() }).expect("under max");
    let big: StdVec<Read> = (0..100u8).map(|i| Read::code([i; 32], [1; 32])).collect();
    assert_eq!(batch_over.add_batch(&big).err(), Some(ReachError::Over { cap: 10 }));
}

#[test]
fn impact_index_early_cutoff_and_first_moved_order() {
    let t0 = Digest([10; 32]);
    let t1 = Digest([11; 32]);
    let t2 = Digest([12; 32]);
    let t3 = Digest([13; 32]);
    let t4 = Digest([14; 32]);
    let t5 = Digest([15; 32]);

    let c1 = Read::code([1; 32], [100; 32]);
    let c2 = Read::code([2; 32], [101; 32]);
    let d1 = Read::datum([1; 32], [200; 32]);
    let f1 = Read::file(b"src/a.rs", b"hello");

    let r0 = encoded(t0, Out::Passed, Reusable::Yes, &reached(&[c1, c2, d1, f1]).expect("reach"));
    let r1 = encoded(t1, Out::Passed, Reusable::Yes, &reached(&[c2, f1]).expect("reach"));
    let r4_wrong_test = encoded(Digest([99; 32]), Out::Passed, Reusable::Yes, &reached(&[c1]).expect("reach"));
    let r5_clock = encoded(t5, Out::Passed, Reusable::No(NotReusable::Clock), &reached(&[c1]).expect("reach"));
    let corrupt: &[u8] = &r0[..10];

    let records: [(Digest, Option<&[u8]>); 6] = [
        (t0, Some(r0.as_slice())),
        (t1, Some(r1.as_slice())),
        (t2, None),
        (t3, Some(corrupt)),
        (t4, Some(r4_wrong_test.as_slice())),
        (t5, Some(r5_clock.as_slice())),
    ];

    let index = ImpactIndex::build(&records, alloc());
    assert_eq!(index.test_count(), 6);
    assert_eq!(index.always_rerun().len(), 4);

    // Early cutoff: `c1` and `f1` reported in `changed`, but with their EXACT recorded values.
    let no_real_change = index.select(
        &[
            (c1.kind, c1.name, c1.value),
            (f1.kind, f1.name, f1.value),
            (CellKind::Env, Digest([77; 32]), Digest([88; 32])),
        ],
        alloc(),
    );
    assert_eq!(no_real_change.decision_for(0), Enter::Reused);
    assert_eq!(no_real_change.decision_for(1), Enter::Reused);
    assert_eq!(no_real_change.decision_for(2), Enter::Rerun { because: Because::NoRecord });
    assert_eq!(no_real_change.decision_for(3), Enter::Rerun { because: Because::Unreadable(RecordError::Short { len: 10 }) });
    assert_eq!(no_real_change.decision_for(4), Enter::Rerun { because: Because::OtherTest { found: Digest([99; 32]) } });
    assert_eq!(no_real_change.decision_for(5), Enter::Rerun { because: Because::NotReusable(NotReusable::Clock) });
    assert_eq!(no_real_change.rerun_count(), 4);
    assert_eq!(no_real_change.reused_count(), 2);

    // Pass changed cells in REVERSE record order (`f1`, `d1`, `c2`): t0 must name `c2` (row 1),
    // and t1 must name `c2` (row 0), NOT `f1`!
    let sel = index.select(
        &[
            (f1.kind, f1.name, Digest([250; 32])),
            (d1.kind, d1.name, Digest([251; 32])),
            (c2.kind, c2.name, Digest([252; 32])),
            // False positive on `c1` (row 0 of t0): must be ignored so row 1 (`c2`) is named!
            (c1.kind, c1.name, c1.value),
        ],
        alloc(),
    );
    assert_eq!(sel.decision_for(0), Enter::Rerun { because: Because::Moved(c2) });
    assert_eq!(sel.decision_for(1), Enter::Rerun { because: Because::Moved(c2) });
    assert_eq!(sel.rerun_count(), 6);
    assert_eq!(sel.reused_count(), 0);
}

fn impact_equivalence_prop(tape: &Tape) -> Result<(), Broke> {
    let num_tests = tape.index(24, "num_tests");
    let mut test_digests = StdVec::with_capacity(num_tests);
    let mut owned_bytes: StdVec<Option<StdVec<u8>>> = StdVec::with_capacity(num_tests);

    for t in 0..num_tests {
        let test = Digest::of("impact-prop-test", &(t as u32).to_le_bytes());
        test_digests.push(test);

        let kind_of_record = tape.below(10, "record_variant");
        match kind_of_record {
            // Missing record
            0 => owned_bytes.push(None),
            // Unreadable record (truncated or bad magic)
            1 => {
                let mut bad = StdVec::from(*b"BAD!");
                if tape.one_in(2, "short_or_magic") {
                    bad.resize(HEADER, 0);
                }
                owned_bytes.push(Some(bad));
            }
            // Record belonging to another test
            2 => {
                let reach = reached(&draw_reads(tape)).map_err(|e| Broke::Harness(format!("{e}")))?;
                let other = Digest::of("impact-prop-other", &(t as u32).to_le_bytes());
                owned_bytes.push(Some(encoded(other, draw_out(tape), draw_reusable(tape), &reach)));
            }
            // Valid record (reusable or NotReusable)
            _ => {
                let reach = reached(&draw_reads(tape)).map_err(|e| Broke::Harness(format!("{e}")))?;
                let reusable = if tape.one_in(5, "not_reusable") {
                    draw_reusable(tape)
                } else {
                    Reusable::Yes
                };
                owned_bytes.push(Some(encoded(test, draw_out(tape), reusable, &reach)));
            }
        }
    }

    let records: StdVec<(Digest, Option<&[u8]>)> = test_digests
        .iter()
        .zip(owned_bytes.iter())
        .map(|(&test, opt)| (test, opt.as_deref()))
        .collect();

    // Draw a set of unique changed `(kind, name) -> new_value` pairs (including false positives
    // where `new_value` equals a recorded value, and cells outside the tests' universe).
    let mut changed_map: BTreeMap<(CellKind, Digest), Digest> = BTreeMap::new();
    let num_changed = tape.below(24, "num_changed");
    for _ in 0..num_changed {
        let kind = CellKind::ALL[tape.index(CellKind::ALL.len(), "chg_kind")];
        let name = Digest([byte(tape, 20, "chg_name"); 32]);
        let new_val = Digest([byte(tape, 256, "chg_val"); 32]);
        changed_map.insert((kind, name), new_val);
    }
    let mut changed: StdVec<(CellKind, Digest, Digest)> = changed_map
        .iter()
        .map(|(&(k, n), &v)| (k, n, v))
        .collect();
    tape.shuffle(&mut changed, "changed_order");

    let index = ImpactIndex::build(&records, alloc());
    let selection = index.select(&changed, alloc());

    oracle(selection.total_tests() == num_tests, || "total_tests mismatch".into())?;
    oracle(selection.rerun_count() + selection.reused_count() == num_tests, || "counts do not sum".into())?;
    oracle(selection.rerun().windows(2).all(|p| p[0].test_idx < p[1].test_idx), || "rerun not strictly ascending".into())?;

    for (t, &(test, stored)) in records.iter().enumerate() {
        let expected = enter_stored(test, stored, |read| {
            changed_map.get(&(read.kind, read.name)).is_some_and(|&new_val| new_val != read.value)
        });
        let got = selection.decision_for(t as u32);
        oracle(got == expected, || format!("test {t}: ImpactIndex gave {got:?}, enter_stored gave {expected:?}"))?;
    }
    Ok(())
}

#[test]
fn impact_index_matches_enter_stored_property() {
    check(SimConfig::cases(500), impact_equivalence_prop);
}

#[test]
fn impact_index_large_scale_zero_leak() {
    // 2,000 tests * 50 reads = 100,000 postings; verify exact selective impact and zero leaks
    // using a dedicated `Counting` allocator.
    let counting = blaze_own::testing::Counting::new(alloc());
    {
        let num_tests = 2_000usize;
        let reads_per_test = 50usize;
        let mut owned: StdVec<StdVec<u8>> = StdVec::with_capacity(num_tests);
        let mut test_keys: StdVec<Digest> = StdVec::with_capacity(num_tests);

        for t in 0..num_tests {
            let test = Digest::of("large-scale-test", &(t as u32).to_le_bytes());
            test_keys.push(test);
            let mut reach = Reach::new(ReachOpts { cap: MAX_READS, alloc: alloc() }).expect("reach");
            for r in 0..reads_per_test {
                // Cell id: shared hot symbols (0..5) + test-local symbols.
                let cell_id = if r < 5 { r as u64 } else { 100 + ((t * 17 + r) % 4_000) as u64 };
                let mut sym = [0u8; 32];
                sym[..8].copy_from_slice(&cell_id.to_le_bytes());
                reach.add(Read::code(sym, [1; 32])).expect("add");
            }
            let done = reach.finish().expect("finish");
            let mut buf = blaze_own::Vec::new_in(alloc());
            TestRecord::encode(test, Out::Passed, Reusable::Yes, &done, &mut buf);
            owned.push(buf.as_slice().to_vec());
        }

        let records: StdVec<(Digest, Option<&[u8]>)> = test_keys
            .iter()
            .zip(owned.iter())
            .map(|(&k, b)| (k, Some(b.as_slice())))
            .collect();

        let index = ImpactIndex::build(&records, &counting);
        assert_eq!(index.test_count(), num_tests);
        assert_eq!(index.posting_count(), num_tests * reads_per_test);

        // Edit one local symbol (`cell_id = 100`) and one false-positive hot symbol (`cell_id = 0` unchanged).
        let mut sym_local = [0u8; 32];
        sym_local[..8].copy_from_slice(&100u64.to_le_bytes());
        let mut sym_hot = [0u8; 32];
        sym_hot[..8].copy_from_slice(&0u64.to_le_bytes());

        let changed = [
            (CellKind::Code, Digest(sym_hot), Digest([1; 32])),   // unchanged -> early cutoff!
            (CellKind::Code, Digest(sym_local), Digest([2; 32])), // moved!
        ];
        let sel = index.select(&changed, &counting);
        assert!(sel.rerun_count() > 0 && sel.rerun_count() < 50);

        let plan = ExecutionPlan::build(&sel, &[], &counting);
        assert_eq!(plan.len(), sel.rerun_count());

        let advice = advise_tier_up_for_plan(&index, &plan, TierOpts { min_fanout: 2, threshold_us: u64::MAX, limit: Some(10) }, &counting);
        assert!(!advice.is_empty());

        for (t, &(test, stored)) in records.iter().enumerate() {
            let expected = enter_stored(test, stored, |read| {
                read.kind == CellKind::Code && read.name == Digest(sym_local) && read.value != Digest([2; 32])
            });
            assert_eq!(sel.decision_for(t as u32), expected);
        }
    }
    counting.check().expect("ImpactIndex, Selection, ExecutionPlan, and TierAdvice must not leak");
}

#[test]
fn execution_plan_failed_first_then_lpt_deterministic() {
    let t0 = Digest([1; 32]);
    let t1 = Digest([2; 32]);
    let t2 = Digest([3; 32]);
    let t3 = Digest([4; 32]);
    let t4 = Digest([5; 32]);
    let t5 = Digest([6; 32]);

    // All 6 tests have no record -> all selected.
    let records = [(t0, None), (t1, None), (t2, None), (t3, None), (t4, None), (t5, None)];
    let index = ImpactIndex::build(&records, alloc());
    let sel = index.select(&[], alloc());

    let history = [
        TestHistory { test: t0, last_duration_us: 50, failed_last: false },
        TestHistory { test: t1, last_duration_us: 200, failed_last: false },
        TestHistory { test: t2, last_duration_us: 200, failed_last: false }, // tie with t1 -> t1 < t2 by Digest
        TestHistory { test: t3, last_duration_us: 10, failed_last: true },
        TestHistory { test: t4, last_duration_us: 300, failed_last: true },
        // Duplicate history for t0: last entry wins (100us, passed).
        TestHistory { test: t0, last_duration_us: 100, failed_last: false },
        // t5 absent -> defaults to (0us, passed).
    ];

    let plan = ExecutionPlan::build(&sel, &history, alloc());
    let got: StdVec<(Digest, u64, bool)> = plan
        .order()
        .iter()
        .map(|p| (p.test, p.estimated_us, p.failed_last))
        .collect();
    assert_eq!(
        got,
        std::vec![
            (t4, 300, true),  // failed-last, longest
            (t3, 10, true),   // failed-last, shorter
            (t1, 200, false), // passed, longest, smaller Digest
            (t2, 200, false), // passed, longest, larger Digest
            (t0, 100, false), // passed, last-wins 100us
            (t5, 0, false),   // unknown -> 0us
        ]
    );
    assert_eq!(plan.total_estimated_us(), 810);
    assert_eq!(plan.estimated_makespan_us(1, alloc()), 810);
    assert_eq!(plan.estimated_makespan_us(16, alloc()), 300);

    // Classic LPT advantage check: jobs [100, 100, 50, 50, 50] on 2 workers -> makespan 200 vs 200?
    // Wait: [100, 100, 50, 50, 50] total 350 -> worker0: 100+50+50=200, worker1: 100+50=150 (optimal 200).
    // Even better: [100, 60, 60, 40, 40] total 300 on 2 workers:
    // LPT order: 100->W0(100), 60->W1(60), 60->W1(120), 40->W0(140), 40->W1(160) -> makespan 160!
    let lpt_records = [
        (Digest([10; 32]), None),
        (Digest([11; 32]), None),
        (Digest([12; 32]), None),
        (Digest([13; 32]), None),
        (Digest([14; 32]), None),
    ];
    let lpt_idx = ImpactIndex::build(&lpt_records, alloc());
    let lpt_sel = lpt_idx.select(&[], alloc());
    let lpt_hist = [
        TestHistory { test: Digest([10; 32]), last_duration_us: 40, failed_last: false },
        TestHistory { test: Digest([11; 32]), last_duration_us: 40, failed_last: false },
        TestHistory { test: Digest([12; 32]), last_duration_us: 60, failed_last: false },
        TestHistory { test: Digest([13; 32]), last_duration_us: 60, failed_last: false },
        TestHistory { test: Digest([14; 32]), last_duration_us: 100, failed_last: false },
    ];
    let lpt_plan = ExecutionPlan::build(&lpt_sel, &lpt_hist, alloc());
    assert_eq!(lpt_plan.estimated_makespan_us(2, alloc()), 160);
}

fn execution_plan_prop(tape: &Tape) -> Result<(), Broke> {
    let n = tape.index(32, "n_tests");
    let mut records = StdVec::with_capacity(n);
    let mut history = StdVec::new();
    for i in 0..n {
        let test = Digest::of("plan-prop", &(i as u32).to_le_bytes());
        records.push((test, None));
        if !tape.one_in(4, "missing_hist") {
            history.push(TestHistory {
                test,
                last_duration_us: tape.below(10_000, "dur"),
                failed_last: tape.one_in(4, "failed"),
            });
        }
    }
    let index = ImpactIndex::build(&records, alloc());
    let sel = index.select(&[], alloc());
    let plan_a = ExecutionPlan::build(&sel, &history, alloc());

    let mut shuffled = history.clone();
    tape.shuffle(&mut shuffled, "hist_order");
    let plan_b = ExecutionPlan::build(&sel, &shuffled, alloc());

    oracle(plan_a.order() == plan_b.order(), || "ExecutionPlan must be invariant to history input order".into())?;
    oracle(plan_a.len() == n, || "plan len mismatch".into())?;

    for pair in plan_a.order().windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let key_a = (!a.failed_last, Reverse(a.estimated_us), a.test, a.test_idx);
        let key_b = (!b.failed_last, Reverse(b.estimated_us), b.test, b.test_idx);
        oracle(key_a < key_b, || format!("plan out of order: {a:?} before {b:?}"))?;
    }
    Ok(())
}

#[test]
fn execution_plan_order_invariance_property() {
    check(SimConfig::cases(300), execution_plan_prop);
}

#[test]
fn advise_tier_up_fanout_and_critical_path() {
    let sym_fanout = Digest([1; 32]); // reached by 4 fast affected tests (fanout = 4, weighted = 400us)
    let sym_crit = Digest([2; 32]);   // reached by 1 very slow affected test (fanout = 1, weighted = 50_000us)
    let sym_cold = Digest([3; 32]);   // reached by 1 fast affected test (fanout = 1, weighted = 100us) -> excluded
    let sym_reused = Digest([4; 32]); // reached by 4 REUSED tests -> fanout = 0 -> excluded
    let sym_clock = Digest([5; 32]);  // reached by a NotReusable::Clock test (weighted = 20_000us) -> included!
    let datum_cell = Read::datum([6; 32], [1; 32]); // Datum cell, NOT Code -> never advised!
    let trigger = Read::env(b"TRIGGER", Some(b"old"));

    let t = |i: u8| Digest([100 + i; 32]);
    let rec = |i: u8, reusable: Reusable, reads: &[Read]| encoded(t(i), Out::Passed, reusable, &reached(reads).expect("reach"));

    // 4 fast affected tests hitting `sym_fanout` + `datum_cell` + `trigger`
    let r0 = rec(0, Reusable::Yes, &[Read::code(sym_fanout.0, [1; 32]), Read::code(sym_cold.0, [1; 32]), datum_cell, trigger]);
    let r1 = rec(1, Reusable::Yes, &[Read::code(sym_fanout.0, [1; 32]), datum_cell, trigger]);
    let r2 = rec(2, Reusable::Yes, &[Read::code(sym_fanout.0, [1; 32]), datum_cell, trigger]);
    let r3 = rec(3, Reusable::Yes, &[Read::code(sym_fanout.0, [1; 32]), datum_cell, trigger]);
    // 1 slow affected test hitting `sym_crit` + `trigger`
    let r4 = rec(4, Reusable::Yes, &[Read::code(sym_crit.0, [1; 32]), trigger]);
    // 4 reused tests (do NOT read `trigger`) hitting `sym_reused`
    let r5 = rec(5, Reusable::Yes, &[Read::code(sym_reused.0, [1; 32])]);
    let r6 = rec(6, Reusable::Yes, &[Read::code(sym_reused.0, [1; 32])]);
    let r7 = rec(7, Reusable::Yes, &[Read::code(sym_reused.0, [1; 32])]);
    let r8 = rec(8, Reusable::Yes, &[Read::code(sym_reused.0, [1; 32])]);
    // 1 NotReusable::Clock test hitting `sym_clock`
    let r9 = rec(9, Reusable::No(NotReusable::Clock), &[Read::code(sym_clock.0, [1; 32])]);

    let records: StdVec<(Digest, Option<&[u8]>)> = [
        (t(0), Some(r0.as_slice())),
        (t(1), Some(r1.as_slice())),
        (t(2), Some(r2.as_slice())),
        (t(3), Some(r3.as_slice())),
        (t(4), Some(r4.as_slice())),
        (t(5), Some(r5.as_slice())),
        (t(6), Some(r6.as_slice())),
        (t(7), Some(r7.as_slice())),
        (t(8), Some(r8.as_slice())),
        (t(9), Some(r9.as_slice())),
    ]
    .into();

    let index = ImpactIndex::build(&records, alloc());
    let sel = index.select(&[(trigger.kind, trigger.name, Digest([255; 32]))], alloc());
    assert_eq!(sel.rerun_count(), 6); // t0..t4 + t9

    let history = [
        TestHistory { test: t(0), last_duration_us: 100, failed_last: false },
        TestHistory { test: t(1), last_duration_us: 100, failed_last: false },
        TestHistory { test: t(2), last_duration_us: 100, failed_last: false },
        TestHistory { test: t(3), last_duration_us: 100, failed_last: false },
        TestHistory { test: t(4), last_duration_us: 50_000, failed_last: false },
        TestHistory { test: t(5), last_duration_us: 90_000, failed_last: false },
        TestHistory { test: t(9), last_duration_us: 20_000, failed_last: false },
    ];

    let opts = TierOpts { min_fanout: 3, threshold_us: 10_000, limit: None };
    let advice = advise_tier_up(&index, &sel, &history, opts, alloc());
    assert_eq!(
        advice.as_slice(),
        &[
            TierAdvice { symbol: sym_crit, fanout: 1, weighted_us: 50_000, max_test_us: 50_000 },
            TierAdvice { symbol: sym_clock, fanout: 1, weighted_us: 20_000, max_test_us: 20_000 },
            TierAdvice { symbol: sym_fanout, fanout: 4, weighted_us: 400, max_test_us: 100 },
        ]
    );

    // `advise_tier_up_for_plan` must produce the exact same advice.
    let plan = ExecutionPlan::build(&sel, &history, alloc());
    let via_plan = advise_tier_up_for_plan(&index, &plan, opts, alloc());
    assert_eq!(via_plan.as_slice(), advice.as_slice());

    // Limit truncation and limit = Some(0).
    let top2 = advise_tier_up(&index, &sel, &history, TierOpts { limit: Some(2), ..opts }, alloc());
    assert_eq!(top2.as_slice(), &advice.as_slice()[..2]);
    let top0 = advise_tier_up(&index, &sel, &history, TierOpts { limit: Some(0), ..opts }, alloc());
    assert!(top0.is_empty());
}

fn advise_tier_up_prop(tape: &Tape) -> Result<(), Broke> {
    let num_tests = tape.index(20, "tier_tests");
    let mut test_digests = StdVec::with_capacity(num_tests);
    let mut owned_bytes: StdVec<Option<StdVec<u8>>> = StdVec::with_capacity(num_tests);
    let mut history = StdVec::new();
    let mut hist_map: BTreeMap<Digest, u64> = BTreeMap::new();

    for t in 0..num_tests {
        let test = Digest::of("tier-prop-test", &(t as u32).to_le_bytes());
        test_digests.push(test);
        if tape.one_in(6, "no_rec") {
            owned_bytes.push(None);
        } else {
            let reach = reached(&draw_reads(tape)).map_err(|e| Broke::Harness(format!("{e}")))?;
            let reusable = if tape.one_in(5, "not_reusable") {
                Reusable::No(NotReusable::Clock)
            } else {
                Reusable::Yes
            };
            owned_bytes.push(Some(encoded(test, Out::Passed, reusable, &reach)));
        }
        if !tape.one_in(4, "no_hist") {
            let dur = tape.below(5_000, "dur");
            history.push(TestHistory { test, last_duration_us: dur, failed_last: tape.one_in(4, "fail") });
            hist_map.insert(test, dur);
        }
    }

    let records: StdVec<(Digest, Option<&[u8]>)> = test_digests
        .iter()
        .zip(owned_bytes.iter())
        .map(|(&test, opt)| (test, opt.as_deref()))
        .collect();

    let mut changed = StdVec::new();
    for _ in 0..tape.below(10, "changed") {
        let kind = CellKind::ALL[tape.index(CellKind::ALL.len(), "k")];
        let name = Digest([byte(tape, 16, "n"); 32]);
        let val = Digest([byte(tape, 256, "v"); 32]);
        changed.push((kind, name, val));
    }

    let index = ImpactIndex::build(&records, alloc());
    let sel = index.select(&changed, alloc());
    let opts = TierOpts {
        min_fanout: 1 + tape.index(5, "min_fanout"),
        threshold_us: tape.below(8_000, "threshold_us"),
        limit: if tape.one_in(3, "has_limit") { Some(tape.index(6, "limit")) } else { None },
    };

    let got = advise_tier_up(&index, &sel, &history, opts, alloc());
    let plan = ExecutionPlan::build(&sel, &history, alloc());
    let got_plan = advise_tier_up_for_plan(&index, &plan, opts, alloc());
    oracle(got.as_slice() == got_plan.as_slice(), || "advise_tier_up_for_plan mismatch".into())?;

    // Brute-force oracle: open every affected test's record directly and tally `CellKind::Code`.
    let mut agg: BTreeMap<Digest, (usize, u64, u64)> = BTreeMap::new();
    for item in sel.rerun() {
        let dur = hist_map.get(&item.test).copied().unwrap_or(0);
        if let Some(bytes) = records[item.test_idx as usize].1
            && let Ok(rec) = TestRecord::open(bytes)
            && rec.test() == item.test
        {
            for read in rec.reads() {
                if read.kind == CellKind::Code {
                    let entry = agg.entry(read.name).or_insert((0, 0, 0));
                    entry.0 += 1;
                    entry.1 = entry.1.saturating_add(dur);
                    entry.2 = entry.2.max(dur);
                }
            }
        }
    }
    let mut expected: StdVec<TierAdvice> = agg
        .into_iter()
        .filter(|&(_, (fanout, weighted_us, _))| fanout > 0 && (fanout >= opts.min_fanout || weighted_us >= opts.threshold_us))
        .map(|(symbol, (fanout, weighted_us, max_test_us))| TierAdvice { symbol, fanout, weighted_us, max_test_us })
        .collect();
    expected.sort_unstable_by(|a, b| {
        b.weighted_us
            .cmp(&a.weighted_us)
            .then_with(|| b.fanout.cmp(&a.fanout))
            .then_with(|| b.max_test_us.cmp(&a.max_test_us))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    if let Some(limit) = opts.limit {
        expected.truncate(limit);
    }

    oracle(got.as_slice() == expected.as_slice(), || format!("tier advice mismatch:\ngot:      {:?}\nexpected: {:?}", got.as_slice(), expected))
}

#[test]
fn advise_tier_up_matches_bruteforce_property() {
    check(SimConfig::cases(300), advise_tier_up_prop);
}

#[test]
fn persist_impact_view_matches_index_and_rejects_corruption() {
    use crate::persist::{ImpactView, PersistError, encode_impact};

    // 1. Property test: zero-copy BTI1 ImpactView::select matches ImpactIndex::select bit-for-bit.
    check(SimConfig::cases(300), |tape| {
        let n_tests = tape.index(18, "tests");
        let n_syms = 1 + tape.index(14, "syms");
        let syms: StdVec<Digest> = (0..n_syms).map(|i| Digest::of("p-sym", &(i as u32).to_le_bytes())).collect();
        let mut raw_records: StdVec<(Digest, Option<StdVec<u8>>)> = StdVec::new();

        for t in 0..n_tests {
            let test = Digest::of("p-test", &(t as u32).to_le_bytes());
            match tape.index(6, "rec kind") {
                0 => raw_records.push((test, None)),
                1 => raw_records.push((test, Some(StdVec::from([1, 2, 3])))),
                2 => {
                    let other = Digest::of("p-other", &(t as u32).to_le_bytes());
                    let r = Reach::new(ReachOpts { cap: 16, alloc: alloc() }).expect("reach").finish().expect("finish");
                    raw_records.push((test, Some(encoded(other, Out::Passed, Reusable::Yes, &r).as_slice().to_vec())));
                }
                3 => {
                    let r = Reach::new(ReachOpts { cap: 16, alloc: alloc() }).expect("reach").finish().expect("finish");
                    raw_records.push((
                        test,
                        Some(encoded(test, Out::Passed, Reusable::No(NotReusable::Clock), &r).as_slice().to_vec()),
                    ));
                }
                _ => {
                    let mut r = Reach::new(ReachOpts { cap: 32, alloc: alloc() }).expect("reach");
                    let reads = tape.index(8, "reads");
                    for _ in 0..reads {
                        let kind = match tape.index(4, "kind") {
                            0 => CellKind::Code,
                            1 => CellKind::Datum,
                            2 => CellKind::File,
                            _ => CellKind::Env,
                        };
                        let name = syms[tape.index(n_syms, "sym")];
                        let val = Digest::of("p-val", &[tape.index(3, "v") as u8]);
                        let _ = r.add(Read { kind, name, value: val });
                    }
                    if let Ok(fin) = r.finish() {
                        raw_records.push((test, Some(encoded(test, Out::Passed, Reusable::Yes, &fin).as_slice().to_vec())));
                    } else {
                        raw_records.push((test, None));
                    }
                }
            }
        }
        let borrowed: StdVec<(Digest, Option<&[u8]>)> =
            raw_records.iter().map(|(d, b)| (*d, b.as_deref())).collect();
        let index = ImpactIndex::build(&borrowed, alloc());
        let bytes = encode_impact(&index, alloc());
        let view = ImpactView::open(&bytes).map_err(|e| Broke::Oracle(format!("open failed: {e}")))?;
        view.verify_digest().map_err(|e| Broke::Oracle(format!("digest failed: {e}")))?;

        let n_changes = tape.index(6, "changes");
        let mut changed = StdVec::new();
        for _ in 0..n_changes {
            let kind = match tape.index(4, "ck") {
                0 => CellKind::Code,
                1 => CellKind::Datum,
                2 => CellKind::File,
                _ => CellKind::Env,
            };
            let name = syms[tape.index(n_syms, "csym")];
            let val = Digest::of("p-val", &[tape.index(4, "cv") as u8]);
            changed.push((kind, name, val));
        }

        let got = view.select(&changed, alloc()).map_err(|e| Broke::Oracle(format!("select failed: {e}")))?;
        let want = index.select(&changed, alloc());
        oracle(
            got.total_tests() == want.total_tests() && got.rerun() == want.rerun(),
            || format!("persisted ImpactView mismatch:\ngot:  {:?}\nwant: {:?}", got.rerun(), want.rerun()),
        )
    });

    // 2. Corruption refusal checks.
    assert_eq!(ImpactView::open(&[0u8; 10]).unwrap_err(), PersistError::TooShort);
    let idx = ImpactIndex::build(&[(Digest::of("t", b"1"), None)], alloc());
    let mut bytes = encode_impact(&idx, alloc());
    let mut bad_magic = bytes.as_slice().to_vec();
    bad_magic[0] = b'X';
    assert_eq!(ImpactView::open(&bad_magic).unwrap_err(), PersistError::BadMagic);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let view = ImpactView::open(&bytes).expect("header still valid");
    assert_eq!(view.verify_digest().unwrap_err(), PersistError::BadDigest);
}
