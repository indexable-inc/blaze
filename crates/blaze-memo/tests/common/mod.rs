//! The front end in miniature, on the real attributes: crates (arenas) of leaf inputs, a
//! query per leaf, a sum over a crate and its dependencies, a `cycle_result` ring and a
//! `cycle_initial` reachability, and the model each is checked against. Shared by the
//! simulation (`sim.rs`) and the property test (`model.rs`).

#![allow(dead_code)]

use std::{
    alloc::Global,
    collections::{BTreeMap, BTreeSet},
};

use blaze_core::lock::Lock;
use blaze_ids::ContentId;
use blaze_mem::Tag;
use blaze_memo::{ArenaId, Builder, Database, Fact, Gid, Id, Memo};

/// Crates are arenas `1..=CRATES`; the session arena is 0.
pub const CRATES: u32 = 4;
/// Leaves per crate: also the nodes of `reach`'s graph.
pub const LEAVES: usize = 4;
/// The ring's length.
pub const RING: u32 = 3;
/// The deepest `total`.
pub const DEPTH: u32 = 2;

/// Query kinds counted by [`Db::ran`]; only the acyclic ones must run once per snapshot.
pub const VALUE: u8 = 0;
pub const TOTAL: u8 = 1;

pub trait Db: Database {
    /// A point where the simulation may switch tasks.
    fn pause(&self);
    /// Notes that query `kind` ran for (`id`, `extra`) to its end.
    fn ran(&self, kind: u8, id: Gid, extra: u32);
}

#[blaze_memo::input(debug)]
pub struct Leaf {
    pub n: u64,
}

#[blaze_memo::input]
pub struct Crate {
    pub leaves: Vec<Leaf>,
    pub deps: Vec<Crate>,
}

#[blaze_memo::tracked(returns(copy))]
pub fn value(db: &dyn Db, leaf: Leaf) -> u64 {
    db.pause();
    let n = *leaf.n(db);
    db.ran(VALUE, leaf.gid(), 0);
    n
}

#[blaze_memo::tracked(returns(copy))]
pub fn total(db: &dyn Db, krate: Crate, depth: u32) -> u64 {
    let mut sum = 0_u64;
    for leaf in krate.leaves(db) {
        sum = sum.wrapping_add(value(db, *leaf));
    }
    db.pause();
    if depth > 0 {
        for dep in krate.deps(db) {
            sum = sum.wrapping_add(total(db, *dep, depth - 1).wrapping_mul(3));
        }
    }
    db.ran(TOTAL, krate.gid(), depth);
    sum
}

fn ring_fallback(_db: &dyn Db, _krate: Crate, at: u32) -> u64 {
    1000 + u64::from(at)
}

#[blaze_memo::tracked(returns(copy), cycle_result = ring_fallback)]
pub fn ring(db: &dyn Db, krate: Crate, at: u32) -> u64 {
    db.pause();
    ring(db, krate, (at + 1) % RING) + 1
}

#[blaze_memo::tracked(returns(copy), cycle_initial = |_db, _krate, _node| 0)]
pub fn reach(db: &dyn Db, krate: Crate, node: u32) -> u64 {
    db.pause();
    let leaves = krate.leaves(db);
    let n = leaves.len() as u32;
    let own = if value(db, leaves[node as usize]) % 2 == 1 { 1 << node } else { 0 };
    own | reach(db, krate, (node + 1) % n) | reach(db, krate, (node * 2) % n)
}

/// The dependencies of crate `c`.
pub fn deps_of(c: u32) -> Vec<u32> {
    (c.saturating_sub(2).max(1)..c).collect()
}

pub fn leaf_fact(c: u32, l: usize, n: u64) -> Fact {
    Fact {
        id: ContentId::of("blaze-memo sim leaf", &[c.to_le_bytes(), (l as u32).to_le_bytes()].concat()),
        digest: ContentId::of("blaze-memo sim value", &n.to_le_bytes()),
    }
}

pub fn crate_fact(c: u32) -> Fact {
    Fact {
        id: ContentId::of("blaze-memo sim crate", &c.to_le_bytes()),
        digest: ContentId::of("blaze-memo sim crate digest", &c.to_le_bytes()),
    }
}

/// What the driver knows: each crate's leaf values, and the ids of the snapshot being built.
pub struct World {
    pub values: Vec<Vec<u64>>,
    pub crates: Vec<Option<Crate>>,
}

impl World {
    pub fn new(values: Vec<Vec<u64>>) -> World {
        let crates = vec![None; values.len()];
        World { values, crates }
    }

    /// Adds crate `c`'s arena and inputs to `builder`, its dependencies made already.
    pub fn make(&mut self, builder: &mut Builder, c: u32) {
        let deps: Vec<Crate> =
            deps_of(c).into_iter().map(|d| self.crates[d as usize].expect("a dependency first")).collect();
        let dep_arenas: Vec<ArenaId> = deps.iter().map(|it| it.gid().arena()).collect();
        let values = self.values[c as usize].clone();
        let inputs = builder.arena(c, Tag::Query);
        for arena in dep_arenas {
            inputs.borrows(arena);
        }
        let leaves: Vec<Leaf> = values
            .iter()
            .enumerate()
            .map(|(l, n)| Leaf::new(&inputs, leaf_fact(c, l, *n), *n))
            .collect();
        self.crates[c as usize] = Some(Crate::new(&inputs, crate_fact(c), leaves, deps));
    }

    /// The first snapshot.
    pub fn first(&mut self, config: blaze_memo::Config) -> Memo {
        let mut builder = Memo::builder(config);
        builder.arena(0, Tag::Query);
        for c in 1..=CRATES {
            self.make(&mut builder, c);
        }
        builder.build()
    }

    /// The next snapshot after the crates in `changed` took new values: the indices it dropped.
    pub fn next(&mut self, memo: &Memo, changed: &[u32]) -> (Memo, Vec<u32>) {
        let ids: Vec<ArenaId> =
            changed.iter().map(|c| self.crates[*c as usize].expect("made").gid().arena()).collect();
        let next = memo.next(&ids);
        let dropped = next.dropped().to_vec();
        let mut builder = next.builder;
        for c in 1..=CRATES {
            if !builder.has(c) {
                self.make(&mut builder, c);
            }
        }
        (builder.build(), dropped)
    }

    pub fn krate(&self, c: u32) -> Crate {
        self.crates[c as usize].expect("made")
    }
}

/// The recursive evaluator every answer is checked against.
pub struct Model<'a> {
    pub values: &'a [Vec<u64>],
}

impl Model<'_> {
    pub fn value(&self, c: u32, l: usize) -> u64 {
        self.values[c as usize][l]
    }

    pub fn total(&self, c: u32, depth: u32) -> u64 {
        let mut sum = 0_u64;
        for n in &self.values[c as usize] {
            sum = sum.wrapping_add(*n);
        }
        if depth > 0 {
            for d in deps_of(c) {
                sum = sum.wrapping_add(self.total(d, depth - 1).wrapping_mul(3));
            }
        }
        sum
    }

    pub fn ring(&self, at: u32) -> u64 {
        1000 + u64::from(at)
    }

    pub fn reach(&self, c: u32, node: u32) -> u64 {
        let n = self.values[c as usize].len() as u32;
        let mut seen = BTreeSet::new();
        let mut todo = vec![node];
        while let Some(at) = todo.pop() {
            if seen.insert(at) {
                todo.push((at + 1) % n);
                todo.push((at * 2) % n);
            }
        }
        seen.iter().filter(|m| self.values[c as usize][**m as usize] % 2 == 1).map(|m| 1 << m).sum()
    }

    /// The facts `total(c, depth)` reads, sorted.
    pub fn total_facts(&self, c: u32, depth: u32) -> Vec<Fact> {
        let mut out = BTreeSet::new();
        self.facts_into(c, depth, &mut out);
        out.into_iter().collect()
    }

    fn facts_into(&self, c: u32, depth: u32, out: &mut BTreeSet<Fact>) {
        out.insert(crate_fact(c));
        for (l, n) in self.values[c as usize].iter().enumerate() {
            out.insert(leaf_fact(c, l, *n));
        }
        if depth > 0 {
            for d in deps_of(c) {
                self.facts_into(d, depth - 1, out);
            }
        }
    }

    /// The arenas a change of `changed` must drop: them and, transitively, their dependents.
    pub fn closure(changed: &[u32]) -> Vec<u32> {
        let mut out: BTreeSet<u32> = changed.iter().copied().collect();
        for c in 1..=CRATES {
            if deps_of(c).iter().any(|d| out.contains(d)) {
                out.insert(c);
            }
        }
        out.into_iter().collect()
    }
}

/// Runs counted per snapshot: (kind, id, extra) to how many runs ended.
pub type Runs = Lock<BTreeMap<(u8, Gid, u32), u32>>;

pub fn new_runs() -> Runs {
    Lock::new("blaze-memo test runs", BTreeMap::new())
}

/// The keys of an acyclic kind that ran more than once.
pub fn twice(runs: &Runs) -> Vec<(u8, Gid, u32)> {
    runs.lock().iter().filter(|(_, n)| **n > 1).map(|(key, _)| *key).collect()
}

pub fn global<T>(it: Vec<T>) -> Vec<T, Global> {
    let mut out = Vec::new_in(Global);
    out.extend(it);
    out
}
