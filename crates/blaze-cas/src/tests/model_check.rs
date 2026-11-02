//! The store against a map: any run of puts, flushes, settles and reopens reads back what the
//! map holds, refuses other bytes under a key it has, and verifies clean.

use std::collections::BTreeMap;

use hegel::{Hegel, Settings, TestCase};
use hegel::generators as gs;

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
        _ => text(tc.draw(gs::integers().max_value(1 << 20)), tc.draw(gs::integers().max_value(64 << 10))),
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

fn reads_back_what_a_map_holds(tc: TestCase) {
    let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(40));
    let mut sim = Sim::new(tc.draw(gs::integers()), quiet());
    let mut store = open(&mut sim, "store");
    let mut model: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for op in ops {
        match op {
            Op::Put(n, bytes) => match (store.put(key(n), kind_of(n), bytes.clone()), model.get(&n)) {
                (Ok(_), None) => {
                    model.insert(n, bytes);
                }
                (Ok(_), Some(kept)) => assert_eq!(kept, &bytes, "key {n}: other bytes were taken"),
                (Err(mismatch), Some(kept)) => {
                    assert_ne!(kept, &bytes, "key {n}: the same bytes were refused");
                    assert_eq!((mismatch.key, mismatch.kept), (key(n), content_of(kept)));
                    assert_eq!(mismatch.given, content_of(&bytes));
                }
                (Err(mismatch), None) => panic!("key {n}: a first put was refused: {mismatch:?}"),
            },
            Op::Get(n) => {
                let found = store.get(&key(n)).unwrap();
                let want = model.get(&n).map(|it| (kind_of(n), it.clone()));
                assert_eq!(found.map(|it| (it.kind, it.bytes.to_vec())), want, "key {n}");
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
            }
        }
        assert!(store.broken().is_none(), "{:?}", store.broken());
    }
    for n in 0..16 {
        assert_eq!(bytes_of(&store, &key(n)).as_ref(), model.get(&n), "key {n}");
    }
    flush(&mut sim, &mut store);
    assert_eq!(store.verify(None), Vec::new());
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn the_store_reads_back_what_a_map_holds() {
    run_cases(48, reads_back_what_a_map_holds);
}
