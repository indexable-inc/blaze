//! The store against a map: any run of puts, flushes, settles (whole, and in steps) and reopens reads back what the
//! map holds, refuses other bytes under a key it has, and verifies clean.

use std::collections::BTreeMap;

use blaze_sim::{Config as SimConfig, Tape};

use super::*;

#[derive(Debug)]
enum Op {
    /// Bytes under one of a few keys, so later puts meet earlier ones.
    Put(u64, Vec<u8>),
    Get(u64),
    Flush,
    /// Settles until it has nothing more to do now.
    Settle,
    /// A period ended and its moves made in steps of at most this many bytes, as the daemon's
    /// loop does between requests.
    Steps(u64),
    /// A flush, then the store dropped and opened again.
    Reopen,
}

/// Small bytes, noise past a chunk, or text that compresses.
fn bytes(tape: &Tape) -> Vec<u8> {
    match tape.int_in(0..=2u8, "bytes kind") {
        0 => tape.bytes(0..=300, "small bytes"),
        1 => noise(tape.int_in(0..=400usize << 10, "noise len"), tape.int::<u64>("noise seed")),
        _ => text(tape.int_in(0..=1u64 << 20, "text"), tape.int_in(0..=64usize << 10, "text len")),
    }
}

fn op(tape: &Tape) -> Op {
    match tape.int_in(0..=10u8, "op") {
        0..5 => Op::Put(tape.int_in(0..=15u64, "key"), bytes(tape)),
        5 | 6 => Op::Get(tape.int_in(0..=15u64, "key")),
        7 => Op::Flush,
        8 => Op::Settle,
        9 => Op::Steps(tape.int_in(1..=300u64 << 10, "step bytes")),
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
            Op::Steps(most) => {
                settle_in_steps(&mut sim, &mut store, most);
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

fn reads_back_what_a_map_holds(tape: &Tape) {
    let ops = tape.vec(1..=40, "ops", op);
    let seed = tape.next_u64("sim seed");
    let said = format!("{ops:?}");
    if let Err(why) = runs_as_a_map_does(ops, seed, false) {
        panic!("seed {seed}, {said}: {why}");
    }
}

/// The refinement: the store is a map. Any run of puts, gets, flushes, settles and reopens
/// reads back what a `BTreeMap` given the same puts holds, refuses other bytes under a key it
/// has, and verifies clean. `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn the_store_reads_back_what_a_map_holds() {
    blaze_sim::check(SimConfig::cases(48), reads_back_what_a_map_holds);
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
