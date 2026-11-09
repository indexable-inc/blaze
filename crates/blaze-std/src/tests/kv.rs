//! `Kv` against a model map, through crashes: after any sequence of puts, deletes and crashes
//! (with completions that come late), a get gives exactly what the model holds and a list
//! exactly its keys. A put or a delete that returned is kept by a crash, whole: never a part
//! of a value, never an older one.
//!
//! Plant: [`Plant::KvPutUnsynced`], a put that empties the key's file and appends the value
//! with no sync, which a crash tears.

use std::{collections::BTreeMap, time::Duration};

use blaze_core::sim::Faults;
use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, path, root};
use crate::{
    Cx, Plant,
    fs::ReadWrite,
    io::{Io, SimWorld},
    kv::{Key, Kv},
};

const KEYS: [&str; 4] = ["alpha", "beta", "gamma.v2", "never"];

#[derive(Clone, Copy, Debug)]
enum Act {
    Put(usize, usize),
    Delete(usize),
    Get(usize),
    List,
    Crash,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    latency_us: u64,
    acts: Vec<Act>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let latency_us = d.pick(&[0, 50, 2_000]);
    let acts = (0..d.int(1, 32))
        .map(|_| match d.int(0, 9) {
            // "never" is never put: a key the store has not.
            0..=3 => Act::Put(d.int(0, 2) as usize, d.int(1, 64) as usize),
            4 => Act::Delete(d.int(0, 3) as usize),
            5 | 6 => Act::Get(d.int(0, 3) as usize),
            7 => Act::List,
            _ => Act::Crash,
        })
        .collect();
    Plan { seed, latency_us, acts }
}

fn key(at: usize) -> Key {
    Key::new(KEYS[at]).expect("a test's key")
}

/// That the store holds exactly what `model` does, by gets and by a list.
fn agrees(cx: &Cx, kv: &Kv<ReadWrite>, model: &BTreeMap<usize, Vec<u8>>, at: usize, when: &str) -> Result<(), String> {
    for (index, name) in KEYS.iter().enumerate() {
        let mut into = cx.bytes();
        let got = kv.get(cx, &key(index), &mut into).map_err(|e| format!("harness: act {at}: {e}"))?;
        let held = got.map(|read| &into.as_slice()[into.len() - read..]);
        if held != model.get(&index).map(Vec::as_slice) {
            return Err(format!(
                "act {at} ({when}): key {name} holds {:?}, and the model {:?}",
                held.map(<[u8]>::len),
                model.get(&index).map(Vec::len)
            ));
        }
        let has = kv.has(cx, &key(index)).map_err(|e| format!("harness: act {at}: {e}"))?;
        if has != model.contains_key(&index) {
            return Err(format!("act {at} ({when}): has({name}) is {has}, against the model"));
        }
    }
    let listed = kv.list(cx, cx.frame()).map_err(|e| format!("harness: act {at}: {e}"))?;
    let listed: Vec<&str> = listed.iter().map(Key::as_str).collect();
    let mut wanted: Vec<&str> = model.keys().map(|index| KEYS[*index]).collect();
    wanted.sort_unstable();
    if listed != wanted {
        return Err(format!("act {at} ({when}): the store lists {listed:?}, and the model {wanted:?}"));
    }
    Ok(())
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let mut world = SimWorld::new(plan.seed);
    world.faults = Faults { max_latency: Duration::from_micros(plan.latency_us), ..Faults::default() };
    let io = Io::planted(world, plant);
    let cx = root(&io);
    let fs = io.need_fs().map_err(|e| format!("harness: {e}"))?;
    let dir = fs.open_dir(&cx, &path("store")).map_err(|e| format!("harness: {e}"))?;
    let kv = Kv::beneath(&dir);
    let mut model: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    let mut written: u8 = 0;
    for (at, &act) in plan.acts.iter().enumerate() {
        match act {
            Act::Put(index, len) => {
                let value: Vec<u8> = (0..len)
                    .map(|_| {
                        written = written.wrapping_add(1);
                        written
                    })
                    .collect();
                kv.put(&cx, &key(index), &value).map_err(|e| format!("harness: act {at}: {e}"))?;
                model.insert(index, value);
            }
            Act::Delete(index) => {
                kv.delete(&cx, &key(index)).map_err(|e| format!("harness: act {at}: {e}"))?;
                model.remove(&index);
            }
            Act::Get(_) | Act::List => agrees(&cx, &kv, &model, at, "read")?,
            Act::Crash => {
                if !io.crash() {
                    return Err("harness: the world is not simulated".to_owned());
                }
                agrees(&cx, &kv, &model, at, "after a crash")?;
            }
        }
    }
    agrees(&cx, &kv, &model, plan.acts.len(), "at the end")?;
    // A read-only store reads the same, and its type has no put.
    let reader = kv.read_only();
    let mut into = cx.bytes();
    for (index, name) in KEYS.iter().enumerate() {
        let got = reader.get(&cx, &key(index), &mut into).map_err(|e| format!("harness: {e}"))?;
        if got.is_some() != model.contains_key(&index) {
            return Err(format!("the read-only store disagrees on key {name}"));
        }
    }
    Ok(())
}

#[hegel::test(test_cases = 150)]
fn kv_agrees_with_a_model_through_crashes(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_a_put_without_a_sync() {
    control(Plant::KvPutUnsynced, |seeds: &mut Seeds| plan(seeds), run);
}
