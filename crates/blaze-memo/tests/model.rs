//! The memo against a reference model, one task (`Alone`), for any sequence of edits, snapshots
//! and asks Hegel draws: a table (`BTreeSet` of the keys the memo should hold) and the recursive
//! evaluator.
//!
//! The refinement: every answer is the evaluator's; an ask runs exactly the acyclic keys it
//! needs that the table lacks, each once, and no other; `Memo::next` drops exactly the changed
//! crates and their transitive dependents, and the table keeps exactly the rest; a recorded
//! list is exactly the facts the evaluator reads.

mod common;

use std::{collections::BTreeSet, sync::Arc};

use blaze_memo::{Alone, Config, Database, Gid, Memo, Plant, Reads, record};
use hegel::generators as gs;

use common::{CRATES, DEPTH, Db, LEAVES, Model, RING, Runs, TOTAL, VALUE, World, deps_of, new_runs};

struct OneDb<'a> {
    memo: &'a Memo,
    runs: &'a Runs,
}

impl Database for OneDb<'_> {
    fn memo(&self) -> &Memo {
        self.memo
    }
}

impl Db for OneDb<'_> {
    fn pause(&self) {}

    fn ran(&self, kind: u8, id: Gid, extra: u32) {
        *self.runs.lock().entry((kind, id, extra)).or_insert(0) += 1;
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Edit(u32, usize, u64),
    Next,
    Value(u32, usize),
    Total(u32, u32),
    Ring(u32, u32),
    Reach(u32, u32),
    Record(u32, u32),
}

#[hegel::composite]
fn op(tc: &hegel::TestCase) -> Op {
    let c = 1 + u32::from(tc.draw(gs::integers::<u8>().max_value(CRATES as u8 - 1)));
    let l = usize::from(tc.draw(gs::integers::<u8>().max_value(LEAVES as u8 - 1)));
    let d = u32::from(tc.draw(gs::integers::<u8>().max_value(DEPTH as u8)));
    match tc.draw(gs::integers::<u8>().max_value(8)) {
        0 | 1 => Op::Edit(c, l, u64::from(tc.draw(gs::integers::<u8>().max_value(9)))),
        2 => Op::Next,
        3 => Op::Value(c, l),
        4 => Op::Total(c, d),
        5 => Op::Ring(c, u32::from(tc.draw(gs::integers::<u8>().max_value(RING as u8 - 1)))),
        6 => Op::Reach(c, l as u32),
        _ => Op::Record(c, d),
    }
}

/// An acyclic key in the model: (kind, crate, leaf or depth).
type Row = (u8, u32, u32);

/// The acyclic keys `op` needs.
fn needs(op: Op, out: &mut BTreeSet<Row>) {
    match op {
        Op::Value(c, l) => {
            out.insert((VALUE, c, l as u32));
        }
        Op::Total(c, d) | Op::Record(c, d) => {
            out.insert((TOTAL, c, d));
            for l in 0..LEAVES {
                out.insert((VALUE, c, l as u32));
            }
            if d > 0 {
                for dep in deps_of(c) {
                    needs(Op::Total(dep, d - 1), out);
                }
            }
        }
        Op::Reach(c, n) => {
            let mut seen = BTreeSet::new();
            let mut todo = vec![n];
            while let Some(at) = todo.pop() {
                if seen.insert(at) {
                    out.insert((VALUE, c, at));
                    todo.push((at + 1) % LEAVES as u32);
                    todo.push((at * 2) % LEAVES as u32);
                }
            }
        }
        Op::Ring(..) | Op::Edit(..) | Op::Next => {}
    }
}

fn check(ops: &[Op], plant: Plant) -> Result<(), String> {
    let values: Vec<Vec<u64>> = (0..=CRATES).map(|c| (0..LEAVES).map(|l| u64::from(c) + l as u64).collect()).collect();
    let mut world = World::new(values);
    let config = Config {
        arenas: CRATES + 1,
        reads: Reads::Kept,
        flight: Arc::new(Alone::new()),
        cap_bytes: u64::MAX,
        plant,
    };
    let mut memo = world.first(config);
    let mut table: BTreeSet<Row> = BTreeSet::new();
    let mut changed: BTreeSet<u32> = BTreeSet::new();
    let mut runs = new_runs();
    for (step, op) in ops.iter().copied().enumerate() {
        let model = Model { values: &world.values };
        let db = OneDb { memo: &memo, runs: &runs };
        let before: u32 = runs.lock().values().sum();
        let (got, want) = match op {
            Op::Edit(c, l, n) => {
                if world.values[c as usize][l] != n {
                    world.values[c as usize][l] = n;
                    changed.insert(c);
                }
                continue;
            }
            Op::Next => {
                let changed_now: Vec<u32> = changed.iter().copied().collect();
                let (next, dropped) = world.next(&memo, &changed_now);
                let want = Model::closure(&changed_now);
                if dropped != want {
                    return Err(format!(
                        "step {step}: carried an arena whose input changed, or dropped one that \
                         did not: changed {changed_now:?} dropped {dropped:?}, the closure {want:?}"
                    ));
                }
                table.retain(|(_, c, _)| !dropped.contains(c));
                changed.clear();
                memo = next;
                runs = new_runs();
                continue;
            }
            // Asks of a snapshot whose crates the driver edited after making it read the
            // values it was made with: the model answers with those.
            _ if !changed.is_empty() => continue,
            Op::Value(c, l) => {
                let leaf = world.krate(c).leaves(&db)[l];
                (common::value(&db, leaf), model.value(c, l))
            }
            Op::Total(c, d) => (common::total(&db, world.krate(c), d), model.total(c, d)),
            Op::Ring(c, at) => (common::ring(&db, world.krate(c), at), model.ring(at)),
            Op::Reach(c, n) => (common::reach(&db, world.krate(c), n), model.reach(c, n)),
            Op::Record(c, d) => {
                let (got, reads) = record(&memo, || common::total(&db, world.krate(c), d))
                    .map_err(|it| it.to_string())?;
                let want = model.total_facts(c, d);
                if reads.as_slice() != want.as_slice() {
                    return Err(format!("step {step}: {op:?}: the recorder listed {reads:?}, the evaluator reads {want:?}"));
                }
                (got, model.total(c, d))
            }
        };
        if got != want {
            return Err(format!("step {step}: {op:?} answered {got}, the evaluator {want}"));
        }
        let mut needed = BTreeSet::new();
        needs(op, &mut needed);
        let fresh = needed.difference(&table).count() as u32;
        let ran: u32 = runs.lock().values().sum::<u32>() - before;
        if ran != fresh {
            return Err(format!("step {step}: {op:?} ran {ran} keys, the table lacked {fresh}"));
        }
        table.extend(needed);
    }
    Ok(())
}

#[hegel::test(test_cases = 300)]
fn the_memo_refines_the_model(tc: hegel::TestCase) {
    let ops: Vec<Op> = tc.draw(gs::vecs(op()).max_size(40));
    if let Err(why) = check(&ops, Plant::None) {
        panic!("{why}");
    }
}

/// The table oracle is not vacuous: carrying a changed crate is refused.
#[test]
fn the_model_catches_a_carried_changed_arena() {
    let ops = [Op::Total(2, 1), Op::Edit(1, 0, 99), Op::Next, Op::Total(2, 1)];
    check(&ops, Plant::None).unwrap();
    let why = check(&ops, Plant::CarryChanged).unwrap_err();
    assert!(why.contains("carried an arena whose input changed"), "{why}");
}

/// The recorder oracle is not vacuous: a hit that does not replay is refused.
#[test]
fn the_model_catches_a_hit_that_does_not_replay() {
    let ops = [Op::Value(1, 0), Op::Record(1, 0)];
    check(&ops, Plant::None).unwrap();
    let why = check(&ops, Plant::HitSkipsReplay).unwrap_err();
    assert!(why.contains("the recorder listed"), "{why}");
}
