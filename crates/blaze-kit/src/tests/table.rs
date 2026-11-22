//! `Table`, `TableSet`, `TableMap` and the branded reads, against `Vec`, `BTreeSet` and
//! `BTreeMap`.

use std::{
    collections::{BTreeMap, BTreeSet},
    format,
    vec::Vec as StdVec,
};

use blaze_sim::{Broke, Config, Tape};

use super::{Why, counted, ensure, no_leaks, planted, small, step, under_every_failure};
use crate::{Id, Table, TableMap, TableSet, plant::Plant};

id! {
    /// A row of the tables under test.
    struct Row;
}

fn table_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut table: Table<Row, u64, _> = Table::new_in(&alloc);
        let mut model: StdVec<u64> = StdVec::new();
        for _ in 0..tape.below(60, "ops") {
            match tape.below(7, "op") {
                0..=2 => {
                    let value = tape.next_u64("value");
                    let id = table.push(value);
                    ensure(id.index() == model.len(), || format!("push gave {id:?} at length {}", model.len()))?;
                    model.push(value);
                }
                3 => {
                    let raw = small(tape, 70, "id");
                    let got = table.get(Row::from_u32(raw)).copied();
                    let want = model.get(blaze_int::idx(raw)).copied();
                    ensure(got == want, || format!("get of {raw} gave {got:?}, the model {want:?}"))?;
                }
                4 => {
                    ensure(table.pop() == model.pop(), || format!("pop differs from the model"))?;
                }
                5 => {
                    let len = tape.index(model.len() + 1, "len");
                    table.truncate(len);
                    model.truncate(len);
                }
                _ => {
                    // The branded reads and `at` agree with the slice.
                    let sum = table.checked(|t| t.ids().map(|ix| *t.get(ix)).fold(0u64, u64::wrapping_add));
                    let want = model.iter().copied().fold(0u64, u64::wrapping_add);
                    ensure(sum == want, || format!("the branded sum {sum} is not the model's {want}"))?;
                    for (id, value) in table.iter_with_ids() {
                        ensure(*table.at(id) == *value, || format!("at({id:?}) differs from iter_with_ids"))?;
                    }
                }
            }
            ensure(table.as_slice() == model.as_slice(), || format!("the table differs from the model"))?;
            ensure(table.ids().map(Id::index).eq(0..model.len()), || format!("ids() is not 0..len"))?;
        }
    }
    no_leaks(&alloc)
}

/// The refinement: `Table`, `TableSet`, and `TableMap` match `Vec`, `BTreeSet`, and `BTreeMap` under any sequence of operations and reject out-of-bounds indices.
#[test]
fn table_matches_a_vec() {
    blaze_sim::check(Config::cases(300), table_case);
}

#[test]
fn the_oracle_catches_get_past_the_end() {
    blaze_sim::control(Config::cases(300), Plant::Clean, Plant::TableGetPastEnd, "get of", |tape, p| {
        planted(p, tape, table_case)
    });
}

fn side_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut set: TableSet<Row, _> = TableSet::new_in(&alloc);
        let mut map: TableMap<Row, u64, _> = TableMap::new_in(&alloc);
        let (mut set_model, mut map_model) = (BTreeSet::new(), BTreeMap::new());
        for _ in 0..tape.below(80, "ops") {
            let raw = small(tape, 300, "id");
            let id = Row::from_u32(raw);
            match tape.below(5, "op") {
                0 => ensure(set.insert(id) == set_model.insert(raw), || format!("set insert of {raw}"))?,
                1 => ensure(set.remove(id) == set_model.remove(&raw), || format!("set remove of {raw}"))?,
                2 => {
                    let value = tape.next_u64("value");
                    ensure(map.insert(id, value) == map_model.insert(raw, value), || format!("map insert of {raw}"))?;
                }
                3 => ensure(map.remove(id) == map_model.remove(&raw), || format!("map remove of {raw}"))?,
                _ => ensure(set.contains(id) == set_model.contains(&raw), || format!("set contains {raw}"))?,
            }
            ensure(set.iter().map(Id::to_u32).eq(set_model.iter().copied()), || format!("the set differs"))?;
            ensure(set.count() == set_model.len(), || format!("the set's count differs"))?;
            ensure(
                map.iter().map(|(id, v)| (id.to_u32(), *v)).eq(map_model.iter().map(|(k, v)| (*k, *v))),
                || format!("the map differs"),
            )?;
            ensure(map.len() == map_model.len(), || format!("the map's len differs"))?;
        }
    }
    no_leaks(&alloc)
}

#[test]
fn table_set_and_map_match_btrees() {
    blaze_sim::check(Config::cases(300), side_case);
}

#[test]
fn table_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("Table", |alloc| {
        let mut table: Table<Row, u64, _> = Table::try_with_capacity_in(1, alloc)?;
        let look = |t: &Table<Row, u64, &super::Failing>| StdVec::from(t.as_slice());
        for n in 0..9u64 {
            let id = step(&mut table, look, |t| {
                t.try_push(n).map_err(|r| if r.value == n { Why::from(r) } else { Why::Changed(format!("{} came back", r.value)) })
            })?;
            assert_eq!(id.index(), usize::try_from(n).unwrap_or(usize::MAX));
        }
        step(&mut table, look, |t| t.try_reserve(40))?;
        Ok(())
    })?;
    under_every_failure("TableSet and TableMap", |alloc| {
        let mut set: TableSet<Row, _> = TableSet::try_with_capacity_in(10, alloc)?;
        let mut map: TableMap<Row, u64, _> = TableMap::try_with_capacity_in(2, alloc)?;
        let look_set = |s: &TableSet<Row, &super::Failing>| s.iter().map(Id::to_u32).collect::<StdVec<_>>();
        let look_map =
            |m: &TableMap<Row, u64, &super::Failing>| m.iter().map(|(i, v)| (i.to_u32(), *v)).collect::<StdVec<_>>();
        for raw in [3u32, 70, 200, 1000] {
            step(&mut set, look_set, |s| s.try_insert(Row::from_u32(raw)))?;
            step(&mut map, look_map, |m| m.try_insert(Row::from_u32(raw), u64::from(raw)))?;
        }
        Ok(())
    })?;
    Ok(())
}
