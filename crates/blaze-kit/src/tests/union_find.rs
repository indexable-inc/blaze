//! `UnionFind` against a plain parent array with no ranks, no halving and no log, cloned whole
//! at each snapshot.

use std::{format, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape};

use super::{Failing, Why, counted, ensure, no_leaks, planted, step, under_every_failure};
use crate::{
    Id, UnionFind,
    plant::Plant,
    union_find::Snapshot,
};

id! {
    /// A variable under test.
    struct Var;
}

/// The slow form: a parent per key, a value per root.
#[derive(Clone, Debug, PartialEq)]
struct Model {
    parent: StdVec<usize>,
    value: StdVec<u64>,
}

impl Model {
    fn root(&self, mut at: usize) -> usize {
        while self.parent[at] != at {
            at = self.parent[at];
        }
        at
    }
}

/// A merge that refuses sums past a bound, so a refused merge is tried too.
fn merge(a: &u64, b: &u64) -> Result<u64, ()> {
    let sum = a + b;
    if sum > 5000 { Err(()) } else { Ok(sum) }
}

fn case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut uf: UnionFind<Var, u64, _> = UnionFind::new_in(&alloc);
        let mut model = Model { parent: StdVec::new(), value: StdVec::new() };
        let mut open: StdVec<(Snapshot, Model)> = StdVec::new();
        for _ in 0..tape.below(80, "ops") {
            let len = model.parent.len();
            match tape.below(10, "op") {
                0..=2 => {
                    let value = tape.below(1000, "value");
                    let key = uf.new_key(value);
                    ensure(key.index() == len, || format!("new_key gave {key:?} at {len}"))?;
                    model.parent.push(len);
                    model.value.push(value);
                }
                3..=4 if len > 0 => {
                    let (a, b) = (tape.index(len, "a"), tape.index(len, "b"));
                    let got = uf.unify(Var::from_u32(a.truncate()), Var::from_u32(b.truncate()), merge);
                    let (ra, rb) = (model.root(a), model.root(b));
                    let want = if ra == rb { Ok(()) } else { merge(&model.value[ra], &model.value[rb]).map(|sum| {
                        model.parent[rb] = ra;
                        model.value[ra] = sum;
                    }) };
                    ensure(got.is_ok() == want.is_ok(), || format!("unify of {a} and {b}: {got:?}, the model {want:?}"))?;
                }
                5 if len > 0 => {
                    let (a, b) = (tape.index(len, "a"), tape.index(len, "b"));
                    let same = uf.same(Var::from_u32(a.truncate()), Var::from_u32(b.truncate()));
                    ensure(same == (model.root(a) == model.root(b)), || format!("same of {a} and {b} differs from the model"))?;
                }
                6 if len > 0 => {
                    let a = tape.index(len, "a");
                    let value = tape.below(1000, "value");
                    uf.set_value(Var::from_u32(a.truncate()), value);
                    let root = model.root(a);
                    model.value[root] = value;
                }
                7 => {
                    open.push((uf.snapshot(), model.clone()));
                }
                8 => {
                    if let Some((snapshot, before)) = open.pop() {
                        uf.rollback_to(snapshot);
                        model = before;
                    }
                }
                _ => {
                    if let Some((snapshot, _)) = open.pop() {
                        uf.commit(snapshot);
                    }
                }
            }
            ensure(uf.len() == model.parent.len(), || format!("len differs from the model"))?;
            // A parent left pointing past the end (a rolled-back key) panics in a find: that is
            // the table differing from the model, said as such.
            let reach = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (0..model.parent.len()).for_each(|key| {
                    uf.find_root(Var::from_u32(key.truncate()));
                });
            }));
            ensure(reach.is_ok(), || format!("a find walked off the table: it differs from the model"))?;
            for key in 0..model.parent.len() {
                let id = Var::from_u32(key.truncate());
                let want = model.value[model.root(key)];
                ensure(*uf.value_of(id) == want, || format!("the value of {key} differs from the model"))?;
            }
            for a in 0..model.parent.len() {
                for b in a + 1..model.parent.len() {
                    let (ia, ib) = (Var::from_u32(a.truncate()), Var::from_u32(b.truncate()));
                    let same = uf.find_root(ia) == uf.find_root(ib);
                    ensure(same == (model.root(a) == model.root(b)), || format!("same of {a} and {b} differs from the model"))?;
                }
            }
            if open.is_empty() {
                ensure(uf.logged() == 0, || format!("{} records logged with no snapshot open", uf.logged()))?;
            }
        }
        while let Some((snapshot, _)) = open.pop() {
            uf.commit(snapshot);
        }
    }
    no_leaks(&alloc)
}

/// A key's index as a `u32`: keys number below 80 here.
trait Narrow32 {
    fn truncate(self) -> u32;
}

impl Narrow32 for usize {
    fn truncate(self) -> u32 {
        blaze_int::Int::truncate_to::<u32>(self)
    }
}

/// The refinement: `UnionFind` with ranks, path halving, and rollback snapshots matches a plain parent array cloned at every snapshot.
#[test]
fn union_find_matches_a_parent_array() {
    blaze_sim::check(Config::cases(400), case);
}

#[test]
fn the_oracle_catches_halving_left_out_of_the_log() {
    blaze_sim::control(Config::cases(600), Plant::Clean, Plant::HalvingUnlogged, "differs from the model", |tape, p| {
        planted(p, tape, case)
    });
}

#[test]
fn union_find_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("UnionFind", |alloc| {
        let mut uf: UnionFind<Var, u64, _> = UnionFind::try_with_capacity_in(2, alloc)?;
        let look = |uf: &UnionFind<Var, u64, &Failing>| {
            let n: u32 = blaze_int::Int::truncate_to::<u32>(uf.len());
            (0..n).map(|k| (uf.find_root(Var::from_u32(k)).to_u32(), *uf.value_of(Var::from_u32(k)))).collect::<StdVec<_>>()
        };
        let mut keys = StdVec::new();
        for n in 0..4u64 {
            keys.push(step(&mut uf, look, |uf| uf.try_new_key(n))?);
        }
        let outer = uf.snapshot();
        for n in 4..10u64 {
            keys.push(step(&mut uf, look, |uf| uf.try_new_key(n))?);
        }
        for pair in keys.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            match step(&mut uf, look, |uf| uf.try_unify(a, b, |x, y| Ok::<u64, ()>(x + y)))? {
                Ok(_) => {}
                Err(()) => return Err(Why::Changed(format!("the merge refused"))),
            }
        }
        let inner = uf.snapshot();
        step(&mut uf, look, |uf| uf.try_set_value(keys[0], 7))?;
        for &key in &keys {
            step(&mut uf, look, |uf| uf.try_find(key))?;
        }
        uf.rollback_to(inner);
        uf.commit(outer);
        Ok(())
    })?;
    Ok(())
}
