//! Refinement simulation tests (`blaze-sim`) for persistent CoW `PMap` and `PVec` (rules 43 & 53).
//!
//! Runs random sequences of inserts, updates, removes, pushes, pops, sets, and O(1) forks
//! across a pool of live snapshots, verifying that every snapshot matches its `BTreeMap` / `Vec`
//! reference model after every operation and that mutating one fork never mutates another.

use core::fmt;
use std::collections::BTreeMap;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_sim::{Broken, Config, Tape, check, controls};

use crate::persist::{PMap, PVec};

fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PersistBroken {
    MapLen { branch: usize, want: usize, got: usize },
    MapEntry { branch: usize, key: u32, want: Option<u32>, got: Option<u32> },
    VecLen { branch: usize, want: usize, got: usize },
    VecElem { branch: usize, idx: usize, want: Option<u32>, got: Option<u32> },
}

impl fmt::Display for PersistBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    enum PersistPlant breaks PersistBroken {
        None,
        /// Mutating a fork leaks into its parent branch if CoW isolation is skipped.
        ForkNotIsolated => PersistBroken::MapEntry { .. } | PersistBroken::MapLen { .. } | PersistBroken::VecElem { .. } | PersistBroken::VecLen { .. },
    }
}

fn check_pmap_model(tape: &Tape, plant: PersistPlant) -> Result<(), Broken<PersistBroken>> {
    let alloc = heap();

    let mut maps: StdVec<PMap<'_, u32, u32>> = StdVec::new();
    let mut models: StdVec<BTreeMap<u32, u32>> = StdVec::new();
    maps.push(PMap::new(alloc));
    models.push(BTreeMap::new());

    let n_ops = tape.int_in(1..=80usize, "ops");
    for _ in 0..n_ops {
        let b = tape.below(maps.len() as u64, "branch") as usize;
        let kind = tape.below(4, "op");
        match kind {
            0 => {
                let k = tape.int_in(0..=48u32, "key");
                let v = tape.int_in(0..=1000u32, "val");
                let old_got = maps[b].insert(k, v);
                let old_want = models[b].insert(k, v);
                if old_got != old_want {
                    return Err(Broken::Oracle(PersistBroken::MapEntry {
                        branch: b,
                        key: k,
                        want: old_want,
                        got: old_got,
                    }));
                }
                if plant == PersistPlant::ForkNotIsolated && maps.len() > 1 {
                    let other = (b + 1) % maps.len();
                    let _ = maps[other].insert(k, v);
                }
            }
            1 => {
                let k = tape.int_in(0..=48u32, "key");
                let old_got = maps[b].remove(&k);
                let old_want = models[b].remove(&k);
                if old_got != old_want {
                    return Err(Broken::Oracle(PersistBroken::MapEntry {
                        branch: b,
                        key: k,
                        want: old_want,
                        got: old_got,
                    }));
                }
            }
            2 if maps.len() < 8 => {
                let forked_map = maps[b].fork();
                assert!(forked_map.ptr_eq(&maps[b]), "fork must share root pointer in O(1)");
                let forked_model = models[b].clone();
                maps.push(forked_map);
                models.push(forked_model);
                tape.reached("forked");
            }
            _ => {
                let k = tape.int_in(0..=200u32, "wide_key");
                let v = tape.int_in(0..=1000u32, "wide_val");
                maps[b].insert(k, v);
                models[b].insert(k, v);
            }
        }

        for bi in 0..maps.len() {
            if maps[bi].len() != models[bi].len() {
                return Err(Broken::Oracle(PersistBroken::MapLen {
                    branch: bi,
                    want: models[bi].len(),
                    got: maps[bi].len(),
                }));
            }
            for (&k, &v) in &models[bi] {
                if maps[bi].get(&k).copied() != Some(v) {
                    return Err(Broken::Oracle(PersistBroken::MapEntry {
                        branch: bi,
                        key: k,
                        want: Some(v),
                        got: maps[bi].get(&k).copied(),
                    }));
                }
            }
            let mut entries = blaze_own::AVec::new_in(alloc);
            maps[bi].collect_entries(&mut entries);
            if entries.len() != models[bi].len() {
                return Err(Broken::Oracle(PersistBroken::MapLen {
                    branch: bi,
                    want: models[bi].len(),
                    got: entries.len(),
                }));
            }
        }
    }
    drop(maps);
    Ok(())
}

fn check_pvec_model(tape: &Tape, plant: PersistPlant) -> Result<(), Broken<PersistBroken>> {
    let alloc = heap();

    let mut vecs: StdVec<PVec<'_, u32>> = StdVec::new();
    let mut models: StdVec<StdVec<u32>> = StdVec::new();
    vecs.push(PVec::new(alloc));
    models.push(StdVec::new());

    let n_ops = tape.int_in(1..=120usize, "ops");
    for _ in 0..n_ops {
        let b = tape.below(vecs.len() as u64, "branch") as usize;
        let kind = tape.below(5, "op");
        match kind {
            0 | 1 => {
                let v = tape.int_in(0..=10_000u32, "val");
                vecs[b].push(v);
                models[b].push(v);
                if plant == PersistPlant::ForkNotIsolated && vecs.len() > 1 {
                    let other = (b + 1) % vecs.len();
                    vecs[other].push(v);
                }
            }
            2 => {
                let got = vecs[b].pop();
                let want = models[b].pop();
                if got != want {
                    return Err(Broken::Oracle(PersistBroken::VecElem {
                        branch: b,
                        idx: models[b].len(),
                        want,
                        got,
                    }));
                }
            }
            3 if !models[b].is_empty() => {
                let idx = tape.below(models[b].len() as u64, "idx") as usize;
                let v = tape.int_in(0..=10_000u32, "new_val");
                let old_got = vecs[b].set(idx, v);
                let old_want = Some(core::mem::replace(&mut models[b][idx], v));
                if old_got != old_want {
                    return Err(Broken::Oracle(PersistBroken::VecElem {
                        branch: b,
                        idx,
                        want: old_want,
                        got: old_got,
                    }));
                }
            }
            _ if vecs.len() < 8 => {
                let forked = vecs[b].fork();
                let fm = models[b].clone();
                vecs.push(forked);
                models.push(fm);
                tape.reached("forked");
            }
            _ => {}
        }

        for bi in 0..vecs.len() {
            if vecs[bi].len() != models[bi].len() {
                return Err(Broken::Oracle(PersistBroken::VecLen {
                    branch: bi,
                    want: models[bi].len(),
                    got: vecs[bi].len(),
                }));
            }
            for (idx, &want) in models[bi].iter().enumerate() {
                let got = vecs[bi].get(idx).copied();
                if got != Some(want) {
                    return Err(Broken::Oracle(PersistBroken::VecElem {
                        branch: bi,
                        idx,
                        want: Some(want),
                        got,
                    }));
                }
            }
        }
    }
    drop(vecs);
    Ok(())
}

/// The refinement: persistent `PMap` with O(1) CoW forks matches `BTreeMap` across every branch.
#[test]
fn pmap_refines_btreemap_across_forks() {
    check(Config::cases(256).must_reach(&["forked"]), |tape| {
        check_pmap_model(tape, PersistPlant::None)
    });
}

/// The control: oracle catches broken CoW isolation on `PMap`.
#[test]
fn the_oracle_catches_pmap_fork_leak() {
    controls(Config::cases(256), |tape, plant: PersistPlant| {
        check_pmap_model(tape, plant)
    });
}

/// The refinement: persistent `PVec` with O(1) CoW forks matches `Vec` across every branch.
#[test]
fn pvec_refines_vec_across_forks() {
    check(Config::cases(256).must_reach(&["forked"]), |tape| {
        check_pvec_model(tape, PersistPlant::None)
    });
}

/// The control: oracle catches broken CoW isolation on `PVec`.
#[test]
fn the_oracle_catches_pvec_fork_leak() {
    controls(Config::cases(256), |tape, plant: PersistPlant| {
        check_pvec_model(tape, plant)
    });
}

/// Crosses 32 and 1024 trie level boundaries cleanly.
#[test]
fn pvec_deep_trie_and_cow_fork_zero_leaks() {
    let alloc = heap();
    let mut v = PVec::new(alloc);
    for i in 0..1200u32 {
        v.push(i);
    }
    let mut forked = v.fork();
    forked.set(500, 999_999);
    forked.push(1200);
    assert_eq!(v.len(), 1200);
    assert_eq!(v.get(500), Some(&500));
    assert_eq!(forked.len(), 1201);
    assert_eq!(forked.get(500), Some(&999_999));
    assert_eq!(forked.get(1200), Some(&1200));
    while forked.pop().is_some() {}
    assert!(forked.is_empty());
    assert_eq!(v.len(), 1200);
    assert_eq!(v.get(1199), Some(&1199));
}
