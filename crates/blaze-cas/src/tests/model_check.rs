//! The store against a map: any run of puts, flushes, settles and reopens reads back what the
//! map holds, refuses other bytes under a key it has, and verifies clean.

use std::collections::BTreeMap;

use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use super::*;

#[derive(Debug)]
enum Op {
    /// Bytes under one of a few keys, so later puts meet earlier ones.
    Put(u64, Vec<u8>),
    Get(u64),
    Flush,
    /// Settles until it has nothing more to do now, as the daemon's idle loop does.
    Settle,
    /// A flush, then the store dropped and opened again.
    Reopen,
}

/// Small bytes, noise past a chunk, or text that compresses.
fn bytes(tc: &TestCase) -> Vec<u8> {
    match tc.draw(gs::integers::<u8>().max_value(2)) {
        0 => tc.draw(gs::binary().max_size(300)),
        1 => noise(tc.draw(gs::integers().max_value(400 << 10)), tc.draw(gs::integers())),
        _ => text(
            tc.draw(gs::integers().max_value(1 << 20)),
            tc.draw(gs::integers().max_value(64 << 10)),
        ),
    }
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    match tc.draw(gs::integers::<u8>().max_value(9)) {
        0..5 => Op::Put(tc.draw(gs::integers().max_value(15)), bytes(tc)),
        5 | 6 => Op::Get(tc.draw(gs::integers().max_value(15))),
        7 => Op::Flush,
        8 => Op::Settle,
        _ => Op::Reopen,
    }
}

/// The kind a key's puts are of: one per key, so only bytes can disagree.
fn kind_of(n: u64) -> Kind {
    [Kind::Query, Kind::Code, Kind::Debug, Kind::Run, Kind::Output][n as usize % 5]
}

/// What a read found or the map holds, for a finding.
fn said(it: &Option<(Kind, Vec<u8>)>) -> String {
    match it {
        Some((kind, bytes)) => format!("{} bytes of kind {}", bytes.len(), kind.name()),
        None => "nothing".to_owned(),
    }
}

/// Runs `ops` against a store on the simulation and a map beside it; `Err` says where the two
/// first differ. `plant` plants `Store::plant_no_drain` in the store, each time it is opened.
fn runs_as_a_map_does(ops: Vec<Op>, seed: u64, plant: bool) -> Result<(), String> {
    let mut sim = Sim::new(seed, quiet());
    let mut store = open(&mut sim, "store");
    store.plant_no_drain = plant;
    let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let read = |store: &Store, n: u64| -> Result<Option<(Kind, Vec<u8>)>, String> {
        let found = store.get(&key(n)).map_err(|corrupt| format!("key {n}: {corrupt}"))?;
        Ok(found.map(|it| (it.kind, it.bytes.to_vec())))
    };
    for op in ops {
        match op {
            Op::Put(n, bytes) => match (store.put(key(n), kind_of(n), &bytes), model.get(&n)) {
                (Ok(_), None) => {
                    model.insert(n, bytes);
                }
                (Ok(_), Some(kept)) => {
                    if kept != &bytes {
                        return Err(format!("key {n}: other bytes were taken"));
                    }
                }
                (Err(mismatch), Some(kept)) => {
                    if kept == &bytes {
                        return Err(format!("key {n}: the same bytes were refused"));
                    }
                    let named = (mismatch.key, mismatch.kept, mismatch.given);
                    if named != (key(n), content_of(kept), content_of(&bytes)) {
                        return Err(format!("key {n}: its refusal names other values: {mismatch:?}"));
                    }
                }
                (Err(mismatch), None) => {
                    return Err(format!("key {n}: a first put was refused: {mismatch:?}"));
                }
            },
            Op::Get(n) => {
                let found = read(&store, n)?;
                let want = model.get(&n).map(|it| (kind_of(n), it.clone()));
                if found != want {
                    let (found, want) = (said(&found), said(&want));
                    return Err(format!("key {n}: read {found} where the map holds {want}"));
                }
            }
            Op::Flush => {
                flush(&mut sim, &mut store);
            }
            Op::Settle => {
                for _ in 0..4 {
                    if !settle(&mut sim, &mut store).more {
                        break;
                    }
                }
            }
            Op::Reopen => {
                flush(&mut sim, &mut store);
                drop(store);
                store = open(&mut sim, "store");
                store.plant_no_drain = plant;
            }
        }
        if let Some(broken) = store.broken() {
            return Err(format!("the store stopped: {broken}"));
        }
    }
    for n in 0..16 {
        let found = read(&store, n)?.map(|(_, bytes)| bytes);
        if found.as_ref() != model.get(&n) {
            return Err(format!("key {n}: the store and the map end apart"));
        }
    }
    flush(&mut sim, &mut store);
    match store.verify(None) {
        corrupt if corrupt.is_empty() => Ok(()),
        corrupt => Err(format!("the store does not verify clean: {corrupt:?}")),
    }
}

fn reads_back_what_a_map_holds(tc: &TestCase) {
    let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(40));
    let seed = tc.draw(gs::integers());
    let said = format!("{ops:?}");
    if let Err(why) = runs_as_a_map_does(ops, seed, false) {
        panic!("seed {seed}, {said}: {why}");
    }
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
pub(super) fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// The refinement: the store is a map. Any run of puts, gets, flushes, settles and reopens
/// reads back what a `BTreeMap` given the same puts holds, refuses other bytes under a key it
/// has, and verifies clean. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn the_store_reads_back_what_a_map_holds() {
    run_cases(48, |tc| reads_back_what_a_map_holds(&tc));
}

/// The planted-bug control: a commit that drains no writer's segment (`Store::plant_no_drain`)
/// makes a flush durable in name only. The directed case (a put, a reopen, a get) passes with
/// nothing planted and fails with it as that bug does: the key reads nothing where the map
/// holds its bytes.
#[test]
fn the_oracle_catches_a_flush_that_commits_nothing() {
    let directed = || vec![Op::Put(1, b"kept across a reopen".to_vec()), Op::Reopen, Op::Get(1)];
    assert_eq!(runs_as_a_map_does(directed(), 1, false), Ok(()), "the store as it is must pass");
    let why = runs_as_a_map_does(directed(), 1, true)
        .expect_err("the oracle missed the planted flush that commits nothing");
    assert!(why.contains("key 1: read nothing where the map holds"), "{why}");
}
