//! `SparseSet` against an ordered `Vec`, `IntervalSet` and `ChunkedBitSet` against `BTreeSet`.

use std::{collections::BTreeSet, format, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape};

use super::{Failing, counted, ensure, no_leaks, planted, small, step, under_every_failure};
use crate::{ChunkedBitSet, Id, IntervalSet, SparseSet, plant::Plant};

id! {
    /// A member of the sets under test.
    struct Point;
}

fn pt(raw: u32) -> Point {
    Point::from_u32(raw)
}

fn sparse_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let domain = 1 + small(tape, 300, "domain");
        let mut set: SparseSet<Point, _> = SparseSet::new_in(domain, &alloc);
        let mut model: StdVec<u32> = StdVec::new();
        for _ in 0..tape.below(400, "ops") {
            let raw = small(tape, domain, "id");
            match tape.below(8, "op") {
                0..=2 => {
                    let new = !model.contains(&raw);
                    ensure(set.insert(pt(raw)) == new, || format!("insert of {raw} differs from the model"))?;
                    if new {
                        model.push(raw);
                    }
                }
                3..=4 => {
                    let at = model.iter().position(|&m| m == raw);
                    // A stale position may point past `dense` and panic: that, too, is the set
                    // differing from the model.
                    let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| set.remove(pt(raw))));
                    ensure(matches!(removed, Ok(r) if r == at.is_some()), || format!("remove of {raw} differs from the model"))?;
                    if let Some(at) = at {
                        model.remove(at);
                    }
                }
                5 => {
                    let want = (!model.is_empty()).then(|| model.remove(0));
                    ensure(set.pop_front().map(Id::to_u32) == want, || format!("pop_front differs from the model"))?;
                }
                6 => {
                    ensure(set.pop_back().map(Id::to_u32) == model.pop(), || format!("pop_back differs from the model"))?;
                }
                _ => {
                    if tape.one_in(6, "clear") {
                        set.clear();
                        model.clear();
                    } else if tape.coin("burst") {
                        // A run of inserts, then three in four of it removed: enough tombstones
                        // that the next insert compacts.
                        let end = domain.min(raw + 100);
                        for x in raw..end {
                            if !model.contains(&x) {
                                set.insert(pt(x));
                                model.push(x);
                            }
                        }
                        for x in (raw..end).filter(|x| x % 4 != 0) {
                            set.remove(pt(x));
                            model.retain(|&m| m != x);
                        }
                    }
                }
            }
            ensure(set.len() == model.len(), || format!("len differs from the model"))?;
            ensure(set.iter().map(Id::to_u32).eq(model.iter().copied()), || format!("the order differs from the model"))?;
            let probe = small(tape, domain, "probe");
            ensure(set.contains(pt(probe)) == model.contains(&probe), || format!("contains {probe} differs from the model"))?;
        }
        // Remove every member one at a time, newest first, so every position is read.
        while let Some(raw) = model.pop() {
            let removed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| set.remove(pt(raw))));
            ensure(matches!(removed, Ok(true)), || format!("remove of {raw} at the end differs from the model"))?;
            ensure(set.iter().map(Id::to_u32).eq(model.iter().copied()), || format!("the order at the end differs from the model"))?;
        }
    }
    no_leaks(&alloc)
}

/// The refinement: `SparseSet` matches an ordered `Vec`, and `IntervalSet` and `ChunkedBitSet` match `BTreeSet` under any sequence of operations.
#[test]
fn sparse_set_matches_an_ordered_vec() {
    blaze_sim::check(Config::cases(300), sparse_case);
}

#[test]
fn the_oracle_catches_a_stale_position_after_compaction() {
    blaze_sim::control(Config::cases(600), Plant::Clean, Plant::CompactKeepsStalePosition, "differs from the model", |tape, p| {
        planted(p, tape, sparse_case)
    });
}

/// The ids of an interval set and whether its ranges are sorted, non-empty and apart.
fn interval_ok<A: core::alloc::Allocator>(set: &IntervalSet<Point, A>) -> Result<(), Broke> {
    let ranges: StdVec<_> = set.ranges().map(|r| (r.start.to_u32(), r.end.to_u32())).collect();
    for pair in ranges.windows(2) {
        ensure(pair[0].1 < pair[1].0, || format!("ranges touch or overlap: {ranges:?}"))?;
    }
    ensure(ranges.iter().all(|r| r.0 < r.1), || format!("an empty range: {ranges:?}"))
}

fn interval_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut set: IntervalSet<Point, _> = IntervalSet::new_in(&alloc);
        let mut other: IntervalSet<Point, _> = IntervalSet::new_in(&alloc);
        let (mut model, mut other_model) = (BTreeSet::new(), BTreeSet::new());
        for _ in 0..tape.below(60, "ops") {
            let start = small(tape, 120, "start");
            let end = start + small(tape, 12, "length");
            match tape.below(6, "op") {
                0..=1 => {
                    let new = (start..end).any(|x| !model.contains(&x));
                    ensure(set.insert_range(pt(start)..pt(end)) == new, || format!("insert_range {start}..{end} said wrong"))?;
                    model.extend(start..end);
                }
                2 => {
                    let was = (start..end).any(|x| model.contains(&x));
                    ensure(set.remove_range(pt(start)..pt(end)) == was, || format!("remove_range {start}..{end} said wrong"))?;
                    (start..end).for_each(|x| {
                        model.remove(&x);
                    });
                }
                3 => {
                    other.insert_range(pt(start)..pt(end));
                    other_model.extend(start..end);
                }
                4 => {
                    set.try_union(&other).map_err(|e| Broke::Harness(format!("{e}")))?;
                    model.extend(other_model.iter().copied());
                }
                _ => {
                    let all = (start..end).all(|x| model.contains(&x));
                    ensure(set.contains_range(pt(start)..pt(end)) == all, || format!("contains_range {start}..{end} said wrong"))?;
                    let next = model.range(start..).next().copied();
                    ensure(set.next_from(pt(start)).map(Id::to_u32) == next, || format!("next_from {start} said wrong"))?;
                }
            }
            interval_ok(&set)?;
            ensure(set.iter().map(Id::to_u32).eq(model.iter().copied()), || format!("the set differs from the model"))?;
            ensure(set.count() == u64::try_from(model.len()).unwrap_or(u64::MAX), || format!("count differs"))?;
        }
    }
    no_leaks(&alloc)
}

#[test]
fn interval_set_matches_a_btree_set() {
    blaze_sim::check(Config::cases(400), interval_case);
}

#[test]
fn the_oracle_catches_touching_ranges_left_apart() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::IntervalMissesAdjacent, "touch", |tape, p| {
        planted(p, tape, interval_case)
    });
}

fn chunked_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        // Up to three chunks, the last often short, so whole chunks fill and empty.
        let domain = 1 + small(tape, 5000, "domain");
        let mut sets: StdVec<ChunkedBitSet<Point, _>> = StdVec::new();
        let mut models: StdVec<BTreeSet<u32>> = StdVec::new();
        for _ in 0..2 {
            sets.push(ChunkedBitSet::new_empty_in(domain, &alloc));
            models.push(BTreeSet::new());
        }
        for _ in 0..tape.below(80, "ops") {
            let s = tape.index(sets.len(), "set");
            let raw = small(tape, domain, "id");
            match tape.below(11, "op") {
                0..=2 => {
                    // A run of inserts, so chunks become full.
                    let run = small(tape, 3000, "run");
                    for x in raw..domain.min(raw + run + 1) {
                        ensure(sets[s].insert(pt(x)) == models[s].insert(x), || format!("insert of {x} said wrong"))?;
                    }
                }
                3..=4 => {
                    ensure(sets[s].remove(pt(raw)) == models[s].remove(&raw), || format!("remove of {raw} said wrong"))?;
                }
                5 => {
                    let copy = sets[s].clone();
                    sets.push(copy);
                    models.push(models[s].clone());
                }
                6..=8 => {
                    let o = tape.index(sets.len(), "other");
                    let other = sets[o].clone();
                    let other_model = models[o].clone();
                    let before = models[s].clone();
                    let changed = match tape.below(3, "combine") {
                        0 => {
                            models[s].extend(other_model.iter().copied());
                            sets[s].try_union(&other)
                        }
                        1 => {
                            models[s].retain(|x| !other_model.contains(x));
                            sets[s].try_subtract(&other)
                        }
                        _ => {
                            models[s].retain(|x| other_model.contains(x));
                            sets[s].try_intersect(&other)
                        }
                    }
                    .map_err(|e| Broke::Harness(format!("{e}")))?;
                    ensure(changed == (before != models[s]), || format!("a combine said changed {changed} wrongly"))?;
                }
                9 => {
                    sets[s].insert_all();
                    models[s] = (0..domain).collect();
                }
                _ => {
                    sets[s].clear();
                    models[s].clear();
                }
            }
            for (k, (set, model)) in sets.iter().zip(&models).enumerate() {
                ensure(set.iter().map(Id::to_u32).eq(model.iter().copied()), || format!("set {k} differs from the model"))?;
                ensure(set.count() == model.len(), || format!("the count of set {k} differs from the model"))?;
                let probe = small(tape, domain, "probe");
                ensure(set.contains(pt(probe)) == model.contains(&probe), || format!("contains {probe} in set {k} said wrong"))?;
            }
        }
    }
    no_leaks(&alloc)
}

#[test]
fn chunked_bit_set_matches_a_btree_set() {
    blaze_sim::check(Config::cases(200), chunked_case);
}

#[test]
fn the_oracle_catches_a_write_into_a_shared_chunk() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::ChunkWritesShared, "differs from the model", |tape, p| {
        planted(p, tape, chunked_case)
    });
}

#[test]
fn set_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("SparseSet", |alloc| {
        let mut set: SparseSet<Point, _> = SparseSet::try_new_in(100, alloc)?;
        let look = |s: &SparseSet<Point, &Failing>| s.iter().map(Id::to_u32).collect::<StdVec<_>>();
        for raw in [5u32, 9, 2, 77, 40, 13, 61, 99, 0] {
            step(&mut set, look, |s| s.try_insert(pt(raw)))?;
        }
        Ok(())
    })?;
    under_every_failure("IntervalSet", |alloc| {
        let mut set: IntervalSet<Point, _> = IntervalSet::new_in(alloc);
        let look = |s: &IntervalSet<Point, &Failing>| s.ranges().map(|r| (r.start.to_u32(), r.end.to_u32())).collect::<StdVec<_>>();
        for start in [10u32, 30, 50, 70, 90] {
            step(&mut set, look, |s| s.try_insert_range(pt(start)..pt(start + 5)))?;
        }
        step(&mut set, look, |s| s.try_remove_range(pt(11)..pt(12)))?;
        step(&mut set, look, |s| s.try_remove_range(pt(31)..pt(32)))?;
        step(&mut set, look, |s| s.try_insert(pt(0)))?;
        Ok(())
    })?;
    under_every_failure("ChunkedBitSet", |alloc| {
        let look = |s: &ChunkedBitSet<Point, &Failing>| s.iter().map(Id::to_u32).collect::<StdVec<_>>();
        let mut a: ChunkedBitSet<Point, _> = ChunkedBitSet::try_new_empty_in(5000, alloc)?;
        let mut b: ChunkedBitSet<Point, _> = ChunkedBitSet::try_new_full_in(5000, alloc)?;
        for raw in [1u32, 2100, 4500] {
            step(&mut a, look, |s| s.try_insert(pt(raw)))?;
            step(&mut b, look, |s| s.try_remove(pt(raw + 1)))?;
        }
        let shared = a.try_clone()?;
        step(&mut a, look, |s| s.try_insert(pt(3)))?;
        step(&mut a, look, |s| s.try_union(&b))?;
        let mut c = shared.try_clone()?;
        step(&mut c, look, |s| s.try_subtract(&b))?;
        let mut d = b.try_clone()?;
        step(&mut d, look, |s| s.try_subtract(&shared))?;
        Ok(())
    })?;
    Ok(())
}
