//! [`Tests`] against tests recorded and recorded again with other entered sets; the oracle is a
//! naive scan of each test's last record. And [`entered`] against a naive read of the marks.

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet};

use blaze_own::Vec as Own;
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{key, oracle};
use crate::suite::{Tests, Verdict, entered};
use crate::{CodeId, Plant, SlotId, TestId};

const TESTS: u64 = 6;
const BODIES: u64 = 8;

fn body(n: u64) -> CodeId {
    CodeId(key(n))
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut tests = Tests::new(Global);
    tests.plant = plant;
    let mut model: BTreeMap<TestId, BTreeSet<CodeId>> = BTreeMap::new();
    let mut known: BTreeSet<TestId> = BTreeSet::new();
    let steps = tape.range(1..60, "steps");
    for _ in 0..steps {
        let test = TestId(key(100 + tape.below(TESTS, "test")));
        match tape.below(3, "action") {
            0 => {
                tests.declare(test);
                known.insert(test);
            }
            1 => {
                let set: BTreeSet<CodeId> =
                    (0..BODIES).filter(|_| tape.coin("entered")).map(body).collect();
                let list: Vec<CodeId> = set.iter().copied().collect();
                let verdict = if tape.coin("failed") { Verdict::Failed } else { Verdict::Passed };
                tests.record(test, verdict, &list);
                known.insert(test);
                model.insert(test, set);
            }
            _ => {
                let changed: Vec<CodeId> =
                    (0..BODIES).filter(|_| tape.one_in(3, "changed")).map(body).collect();
                let mut got = Own::new_in(Global);
                tests.affected(&changed, &mut got);
                let want: Vec<TestId> = known
                    .iter()
                    .filter(|test| match model.get(*test) {
                        None => true,
                        Some(set) => changed.iter().any(|id| set.contains(id)),
                    })
                    .copied()
                    .collect();
                oracle(got.as_slice() == want.as_slice(), || {
                    format!("affected gave {} tests where a scan gives {}", got.len(), want.len())
                })?;
            }
        }
    }
    Ok(())
}

#[test]
fn affected_is_exactly_the_tests_that_entered_a_changed_body() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

#[test]
fn the_oracle_catches_postings_kept_from_an_old_record() {
    control(Config::cases(4000), Plant::None, Plant::StalePostings, "affected gave", run);
}

#[test]
fn entered_reads_the_marks_and_what_optimized_bodies_absorbed() {
    check(Config::cases(300), |tape| {
        let slots = 6usize;
        let marks: Vec<u8> = (0..slots).map(|_| u8::from(tape.coin("marked"))).collect();
        let ids: Vec<CodeId> = (0..slots as u64).map(|n| body(n % 4)).collect();
        let absorbed: Vec<Vec<CodeId>> = (0..slots)
            .map(|_| (0..BODIES).filter(|_| tape.one_in(4, "absorbed")).map(body).collect())
            .collect();
        let mut got = Own::new_in(Global);
        entered(&marks, &ids, |slot: SlotId| absorbed[slot.index()].as_slice(), &mut got);
        let mut want = BTreeSet::new();
        for at in 0..slots {
            if marks[at] != 0 {
                want.insert(ids[at]);
                want.extend(absorbed[at].iter().copied());
            }
        }
        let want: Vec<CodeId> = want.into_iter().collect();
        oracle(got.as_slice() == want.as_slice(), || format!("entered gave {got:?}, wanted {want:?}"))
    });
}
