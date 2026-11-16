//! Judge-changed findings (docs/v2/plan/design/test.md, "End state: property, differential and
//! end-to-end tests", D, piece 19): for drawn pairs of a base's and a tree's tests, every
//! deleted test and every weakened test the change reaches is reported exactly once, in item
//! order, and a test whose judge did not weaken (untouched, its `#[ignore]` removed, its budget
//! raised) or that the change does not reach never is; every finding's record round-trips. The
//! planted bug: a judge that pairs the two sides by position, not by the test's item, misses a
//! deletion followed by an addition.
//!
//! Expected lines (`lanecheck.sh blaze-run test judge`):
//! `test sim::judge::every_weakened_test_is_reported_exactly_once ... ok`,
//! `test sim::judge::a_judge_pairing_by_position_misses_a_deletion_then_an_addition ... ok`,
//! `test sim::judge::each_kind_says_its_words_and_round_trips ... ok`.

use blaze_own::{AVec, Alloc, Vec as OwnVec};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use super::{harness, key, oracle};
use crate::head::{Head, HeadRow, ReadDigest, encode};
use crate::judge::{
    Budget, Change, JudgeFinding, JudgeRecordError, JudgeRow, Kind, RECORD, TestItem, judge, open,
};
use crate::plan::{Address, Asked, Last, ShouldPanic};
use crate::postings::{Ask, FnName, ReadName};
use crate::{CheckoutKey, Nanos, TestId};

const CODE: u64 = 4;
const READS: u64 = 3;
const BUDGET: Budget = Budget { cases: 256, seeds: 256, time: Nanos(30_000_000_000) };
const PANICS: [ShouldPanic<'static>; 3] = [ShouldPanic::No, ShouldPanic::Yes, ShouldPanic::WithText("boom")];

/// Whether bit `k` of `mask` is set.
fn bit(mask: u64, k: u64) -> bool {
    mask & (1u64 << k) != 0
}

fn code(k: u64) -> FnName {
    FnName(key(10 + k))
}

fn read(k: u64) -> ReadName {
    ReadName(key(70 + k))
}

fn own(i: u64) -> FnName {
    FnName(key(40 + i))
}

fn item(i: u64) -> TestItem {
    TestItem(key(2 * i + 2))
}

fn base_id(i: u64) -> TestId {
    TestId(key(100 + i))
}

fn tree_id(i: u64) -> TestId {
    TestId(key(150 + i))
}

fn row(item: TestItem, test: TestId, ignore: Option<&'static str>, should_panic: ShouldPanic<'static>, own: FnName, budget: Option<Budget>) -> JudgeRow<'static> {
    JudgeRow { item, asked: Asked { test, path: "m::t", ignore, should_panic, bench: false }, own, budget }
}

/// What the change does to a test's budget.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BudgetMove {
    Keep,
    FewerCases,
    FewerSeeds,
    LessTime,
    Dropped,
    Raised,
}

impl BudgetMove {
    fn apply(self, from: Budget) -> Option<Budget> {
        match self {
            BudgetMove::Keep => Some(from),
            BudgetMove::FewerCases => Some(Budget { cases: 16, ..from }),
            BudgetMove::FewerSeeds => Some(Budget { seeds: from.seeds - 1, ..from }),
            BudgetMove::LessTime => Some(Budget { time: Nanos(from.time.0 / 2), ..from }),
            BudgetMove::Dropped => None,
            BudgetMove::Raised => Some(Budget { cases: from.cases * 2, ..from }),
        }
    }

    fn lowers(self) -> bool {
        !matches!(self, BudgetMove::Keep | BudgetMove::Raised)
    }
}

/// One base test as drawn, with what the change does to it.
#[derive(Clone, Copy, Debug)]
struct Drawn {
    i: u64,
    added_before: bool,
    has_head: bool,
    failed: bool,
    /// Bit k: its last run entered `code(k)`.
    entered: u64,
    /// Bit k: its last run read `read(k)`.
    reads: u64,
    ignored: bool,
    panics: usize,
    budget: bool,
    deleted: bool,
    ignore_flips: bool,
    panic_changes: bool,
    budget_move: BudgetMove,
    own_moved: bool,
}

impl Drawn {
    fn draw(tape: &Tape, i: u64) -> Drawn {
        let moves = [
            BudgetMove::Keep,
            BudgetMove::FewerCases,
            BudgetMove::FewerSeeds,
            BudgetMove::LessTime,
            BudgetMove::Dropped,
            BudgetMove::Raised,
        ];
        Drawn {
            i,
            added_before: tape.bool("a test is added just before it"),
            has_head: tape.below(4, "the base head has its row (0: no)") != 0,
            failed: tape.bool("its last run failed"),
            entered: tape.below(1 << CODE, "the functions its last run entered"),
            reads: tape.below(1 << READS, "the files its last run read"),
            ignored: tape.below(4, "ignored at the base (0: yes)") == 0,
            panics: tape.below(3, "its should_panic at the base") as usize,
            budget: tape.bool("it has a run spec"),
            deleted: tape.below(4, "deleted (0: yes)") == 0,
            ignore_flips: tape.bool("its ignore flips"),
            panic_changes: tape.below(3, "its should_panic changes (0: yes)") == 0,
            budget_move: moves[tape.below(moves.len() as u64, "what happens to its budget") as usize],
            own_moved: tape.bool("its own body moved"),
        }
    }

    fn base_row(&self) -> JudgeRow<'static> {
        let ignore = if self.ignored { Some("slow") } else { None };
        row(item(self.i), base_id(self.i), ignore, PANICS[self.panics], own(self.i), self.budget.then_some(BUDGET))
    }

    fn tree_row(&self) -> JudgeRow<'static> {
        let ignored = self.ignored != self.ignore_flips;
        let panics = if self.panic_changes { (self.panics + 1) % 3 } else { self.panics };
        let budget = if self.budget { self.budget_move.apply(BUDGET) } else { None };
        let base = self.base_row();
        let moved = ignored != self.ignored || panics != self.panics || budget != base.budget;
        let test = if moved { tree_id(self.i) } else { base_id(self.i) };
        let ignore = if ignored { Some("slow") } else { None };
        row(item(self.i), test, ignore, PANICS[panics], own(self.i), budget)
    }

    fn asks(&self) -> Vec<Ask> {
        let mut asks = vec![Ask::Fn(own(self.i))];
        asks.extend((0..CODE).filter(|k| bit(self.entered, *k)).map(|k| Ask::Fn(code(k))));
        asks.extend((0..READS).filter(|k| bit(self.reads, *k)).map(|k| Ask::Read(read(k))));
        asks
    }

    /// The model (D, as judge.rs's module documentation states it): the kinds this test must be
    /// reported for, in kind code order.
    fn want(&self, code_moved: u64, reads_moved: u64, out: &mut Vec<(TestId, Kind)>) {
        let test = base_id(self.i);
        if self.deleted {
            out.push((test, Kind::TestDeleted));
            return;
        }
        if self.has_head && self.entered & code_moved == 0 {
            return;
        }
        if !self.ignored && self.ignore_flips {
            out.push((test, Kind::IgnoreAdded));
        }
        if self.panic_changes {
            out.push((test, Kind::ShouldPanicChanged));
        }
        if self.budget && self.budget_move.lowers() {
            out.push((test, Kind::BudgetLowered));
        }
        if self.has_head && self.reads & reads_moved != 0 {
            out.push((test, Kind::ExpectationChanged));
        }
        if self.has_head && self.own_moved {
            out.push((test, Kind::JudgeAndCodeMoved));
        }
    }
}

/// How the judge under test pairs a base row with a tree row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pairing {
    /// judge.rs as written: by the test's item.
    ByItem,
    /// The planted bug: the base's i-th test is taken as the tree's i-th, whatever their items.
    ByPosition,
}

fn run(tape: &Tape, pairing: Pairing) -> Result<(), Broke> {
    let n = tape.range(1..8, "base tests");
    let drawn: Vec<Drawn> = (0..n).map(|i| Drawn::draw(tape, i)).collect();
    let added_last = tape.bool("a test is added after the last");
    let code_moved = tape.below(1 << CODE, "the functions the change moved");
    let reads_moved = tape.below(1 << READS, "the expectations the change moved");

    let base: Vec<JudgeRow<'static>> = drawn.iter().map(Drawn::base_row).collect();
    let mut tree = Vec::new();
    for one in &drawn {
        if one.added_before {
            // An odd item: between the one before and this one.
            tree.push(row(TestItem(key(2 * one.i + 1)), TestId(key(240 + one.i)), None, ShouldPanic::No, FnName(key(60 + one.i)), None));
        }
        if !one.deleted {
            tree.push(one.tree_row());
        }
    }
    if added_last {
        tree.push(row(TestItem(key(2 * n + 1)), TestId(key(250)), None, ShouldPanic::No, FnName(key(59)), None));
    }

    let asks: Vec<(u64, Vec<Ask>)> = drawn.iter().filter(|one| one.has_head).map(|one| (one.i, one.asks())).collect();
    let rows: Vec<HeadRow<'_>> = asks
        .iter()
        .map(|(i, asks)| {
            let failed = drawn[*i as usize].failed;
            let last = if failed { Last::Failed } else { Last::Passed { address: Address(key(200 + i)) } };
            HeadRow { test: base_id(*i), last, micros: 1, asks }
        })
        .collect();
    let asked_reads = drawn.iter().filter(|one| one.has_head).fold(0, |all, one| all | one.reads);
    let reads: Vec<(ReadName, ReadDigest)> =
        (0..READS).filter(|k| bit(asked_reads, *k)).map(|k| (read(k), ReadDigest(key(80 + k)))).collect();
    let head_bytes = encode(&rows, &reads, Alloc::global()).map_err(harness)?;
    let head = Head::open(head_bytes.as_slice()).map_err(harness)?;

    let mut moved: Vec<FnName> = (0..CODE).filter(|k| bit(code_moved, *k)).map(code).collect();
    moved.extend(drawn.iter().filter(|one| one.own_moved || one.deleted).map(|one| own(one.i)));
    moved.sort();
    let expectations: Vec<ReadName> = (0..READS).filter(|k| bit(reads_moved, *k)).map(read).collect();

    let paired: Vec<JudgeRow<'static>> = match pairing {
        Pairing::ByItem => tree.clone(),
        Pairing::ByPosition => tree.iter().zip(&base).map(|(t, b)| JudgeRow { item: b.item, ..*t }).collect(),
    };
    let change = Change {
        base: &base,
        tree: &paired,
        head,
        moved: &moved,
        expectations: &expectations,
        base_checkout: CheckoutKey(key(1)),
        tree_checkout: CheckoutKey(key(2)),
    };
    let mut out: AVec<'_, JudgeFinding> = OwnVec::new_in(Alloc::global());
    judge(&change, &mut out).map_err(harness)?;
    let got: Vec<(TestId, Kind)> = out.as_slice().iter().map(|finding| (finding.test, finding.kind())).collect();

    let mut want = Vec::new();
    for one in &drawn {
        one.want(code_moved, reads_moved, &mut want);
    }
    for wanted in &want {
        let times = got.iter().filter(|found| *found == wanted).count();
        oracle(times > 0, || format!("test {:?} {:?} was not reported; got {got:?}", wanted.0.0[0], wanted.1))?;
        oracle(times == 1, || format!("test {:?} {:?} was reported {times} times", wanted.0.0[0], wanted.1))?;
    }
    for found in &got {
        oracle(want.contains(found), || {
            format!("test {:?} {:?} was reported, but its judge did not weaken where the change reaches it", found.0.0[0], found.1)
        })?;
    }
    oracle(got == want, || format!("the findings are not in item then kind order: {got:?}, wanted {want:?}"))?;
    for finding in out.as_slice() {
        oracle(open(&finding.encode()) == Ok(*finding), || format!("{finding:?} did not round-trip through its record"))?;
    }
    Ok(())
}

#[test]
fn every_weakened_test_is_reported_exactly_once() {
    check(SimConfig::cases(2000), |tape| run(tape, Pairing::ByItem));
}

#[test]
fn a_judge_pairing_by_position_misses_a_deletion_then_an_addition() {
    control(SimConfig::cases(4000), Pairing::ByItem, Pairing::ByPosition, "was not reported", run);
}

/// One planted case per kind, with the exact words each prints:
///
/// ```text
/// test 6400000000000000 was deleted; its last run failed
/// test 6500000000000000 gained #[ignore] in the change that moved function 0a00000000000000 it entered
/// test 6600000000000000's should_panic went from no to yes with a text in the change that moved function 0a00000000000000 it entered
/// test 6700000000000000's budget fell from 256 cases, 256 seeds, 30.000 s to 16 cases, 256 seeds, 30.000 s in the change that moved function 0a00000000000000 it entered
/// test 6800000000000000 read expectation 4600000000000000, which changed (and 1 more) in the change that moved function 0a00000000000000 it entered
/// test 6900000000000000's own body moved in the change that moved function 0a00000000000000 it entered
/// test 6b00000000000000 gained #[ignore] in a change the head cannot place: it has no last run of the test
/// ```
///
/// Test 6 (`6a..`) is untouched though its code moved, and the added test (item 5) has no
/// judge to weaken: neither prints. Every record round-trips; a cut one and a reserved kind are
/// refused by name.
#[test]
fn each_kind_says_its_words_and_round_trips() {
    let base: Vec<JudgeRow<'static>> =
        (0..8).map(|i| row(item(i), base_id(i), None, ShouldPanic::No, own(i), Some(BUDGET))).collect();
    let mut tree = Vec::new();
    for (i, base_row) in base.iter().enumerate() {
        let i = i as u64;
        let mut now = *base_row;
        match i {
            // Deleted.
            0 => continue,
            1 | 7 => now.asked.ignore = Some(""),
            2 => now.asked.should_panic = ShouldPanic::WithText("boom"),
            3 => now.budget = Some(Budget { cases: 16, ..BUDGET }),
            _ => {}
        }
        if now != *base_row {
            now.asked.test = tree_id(i);
        }
        tree.push(now);
        if i == 1 {
            tree.push(row(TestItem(key(5)), TestId(key(245)), Some(""), ShouldPanic::No, FnName(key(59)), None));
        }
    }
    // Test 7 has no head row; test 4 read two expectations.
    let asks: Vec<Vec<Ask>> = (0..7)
        .map(|i| {
            let mut asks = vec![Ask::Fn(code(0)), Ask::Fn(own(i))];
            if i == 4 {
                asks.extend([Ask::Read(read(0)), Ask::Read(read(1))]);
            }
            asks
        })
        .collect();
    let rows: Vec<HeadRow<'_>> = asks
        .iter()
        .enumerate()
        .map(|(i, asks)| {
            let last = if i == 0 { Last::Failed } else { Last::Passed { address: Address(key(200 + i as u64)) } };
            HeadRow { test: base_id(i as u64), last, micros: 1, asks }
        })
        .collect();
    let reads = [(read(0), ReadDigest(key(80))), (read(1), ReadDigest(key(81)))];
    let head_bytes = encode(&rows, &reads, Alloc::global()).expect("the head encodes");
    let head = Head::open(head_bytes.as_slice()).expect("the head opens");
    let mut moved = vec![code(0), own(0), own(5)];
    moved.sort();
    let expectations = [read(0), read(1)];
    let change = Change {
        base: &base,
        tree: &tree,
        head,
        moved: &moved,
        expectations: &expectations,
        base_checkout: CheckoutKey(key(1)),
        tree_checkout: CheckoutKey(key(2)),
    };
    let mut out: AVec<'_, JudgeFinding> = OwnVec::new_in(Alloc::global());
    judge(&change, &mut out).expect("the change is judged");
    let mut text = String::new();
    for finding in out.as_slice() {
        finding.message(&mut text).expect("a String takes every write");
        text.push('\n');
        assert_eq!(open(&finding.encode()), Ok(*finding), "{finding:?} round-trips");
    }
    assert_eq!(
        text,
        "test 6400000000000000 was deleted; its last run failed\n\
         test 6500000000000000 gained #[ignore] in the change that moved function 0a00000000000000 it entered\n\
         test 6600000000000000's should_panic went from no to yes with a text in the change that moved function 0a00000000000000 it entered\n\
         test 6700000000000000's budget fell from 256 cases, 256 seeds, 30.000 s to 16 cases, 256 seeds, 30.000 s in the change that moved function 0a00000000000000 it entered\n\
         test 6800000000000000 read expectation 4600000000000000, which changed (and 1 more) in the change that moved function 0a00000000000000 it entered\n\
         test 6900000000000000's own body moved in the change that moved function 0a00000000000000 it entered\n\
         test 6b00000000000000 gained #[ignore] in a change the head cannot place: it has no last run of the test\n"
    );
    let rules: Vec<u16> = out.as_slice().iter().map(JudgeFinding::rule).collect();
    assert_eq!(rules, [0xC0FA_u16, 0xC0FB, 0xC0FC, 0xC0FD, 0xC0FE, 0xC101, 0xC0FB]);

    let record = out.as_slice()[0].encode();
    assert_eq!(open(&record[..RECORD - 1]), Err(JudgeRecordError::Length { len: RECORD - 1 }));
    let mut reserved = record;
    reserved[8] = 5;
    assert_eq!(open(&reserved), Err(JudgeRecordError::Kind { code: 5 }));
    let mut stray = out.as_slice()[1].encode();
    stray[RECORD - 1] = 1;
    assert_eq!(open(&stray), Err(JudgeRecordError::NotCanonical));
}
