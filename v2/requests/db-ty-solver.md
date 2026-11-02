# Requests of db-ty-solver

## 1. db-ty-types (sibling), `crates/hir-ty/src/next_solver/interner.rs`: drop `reinit_cache`

What: `tls_cache::reinit_cache` is gone. The solver cache is now one table shared by every thread
(`interner/tls_cache.rs`). Each slot carries the database state (`nonce_and_revision`) it was
filled in, and `borrow_assume_valid` reads that state on every use, so nothing has to reset a
thread's cache any more. Delete the four calls and fix three comments:

- `DbInterner::new_with` (line ~403): delete `tls_cache::reinit_cache(db);`.
- `tls_db`'s `attach` (line ~2532): delete `super::tls_cache::reinit_cache(db);` before `op()`.
- `attach_allow_change` (lines ~2556 and ~2562): in `Drop for DbGuard` keep only
  `self.state.database.set(self.prev);` (delete the `if let Some(prev) = self.prev { ... }`),
  and delete `super::tls_cache::reinit_cache(db);` before `op()`.
- Comments: on `with_global_cache` (line ~1089) say "The cache of this interner's `Solved`, in
  the database's present state."; in `conjure` (line ~374) and `new_no_crate` (line ~390) delete
  the sentences about reinitializing the cache. On the `solved` field, "Which of the thread's
  solver caches" becomes "Which solver cache".

The calls to `tls_cache::Solved::new()`, `tls_cache::borrow_assume_valid(self.db, self.solved, f)`
and `pub use tls_cache::clear_tls_solver_cache;` stay as they are.

Why: shared rule 4 (no thread-local cache whose validity depends on the database). Until this
lands, `interner.rs` fails to compile on `reinit_cache` and nothing else. A no-op
`reinit_cache` would have been a dead path.

(db-ty-unify: `infer/mod.rs:354-356`'s comment about reinitializing the cache is now stale. The
comment is all that needs to change.)

> forwarded to db: db-ty-types' `interner.rs`, inside lane db.
> checked by db-audit-dbsub-tysmall (22:33): in the tree: `rg reinit_cache crates vendor` finds nothing (db-ty-types' "Applied requests of others" in `db-ty-types.md` says the same).

## 2. Lane db, `vendor/rustc_type_ir/src/search_graph/global_cache.rs`: goals and results as `u32`s

**Revised 17:50 under the "Fast code, line by line" bar; this version replaces the earlier
one.** Changes from it: nested goals move out of the hot row, each value is hashed once (and
the hash is kept for probing and growing), and `new` stays `const`.

What: replace the file with the one below. Nothing else changes: `GlobalCache<X>`, `new`,
`Default`, `insert`, `get` and `CacheData` keep their signatures, and `Cx` is untouched (the
`Tracked` a result is kept in now holds a `u32`, which `mk_tracked`/`get_tracked` already accept
for any `T: Debug + Clone`).

Why: `docs/v2/plan/design/data.md`, row "trait solving: goals and results interned as `u32`s; the
solver's cache an open-addressed table on them", and lane db's "Done when". Today every entry is
a SipHash-free but fat `HashMap<Input, CacheEntry>` bucket: the whole canonical input, an
`Option<Success>` that inlines a 32-byte `NestedGoals` map, and a 32-byte
`HashMap<usize, WithOverflow>`, about 120 bytes a goal. Each result is stored again in every goal
that has it. Most goals end in one of a few results (`Ok(Certainty::Yes)` with no constraints),
and `NestedGoals` is empty unless the goal overflowed or was in a cycle (the comment on it in
`search_graph/mod.rs`). With this file:

- a goal is a `u32` index into columns, found through an open-addressed `u32` slot array that
  compares a stored 32-bit hash before the input;
- the hot row a lookup reads is `Option<Success>` = depth plus tracked result id (24 bytes);
- nested goals sit in a side map keyed by goal, filled only when non-empty, and a lookup without
  them borrows one shared empty `NestedGoals`;
- results are interned the same way, so goals that share a result share one copy;
- results that hit the recursion limit, which are rare, sit in one map keyed by goal and depth;
- each value is hashed once with FxHash, on insert. Growing reuses the stored hashes, and a hit
  allocates nothing.

Once db-ty-types' interned kinds are `u32` indices, an input is a handful of `u32`s. The cache's
behavior is unchanged: the same lookups and the same applicability checks, and both overflow
asserts kept (equal results now have equal indices).

On `docs/v2/plan/design/concurrency.md` ("never rehashed in place"): a `GlobalCache` belongs to
one interner and is reached only through `with_global_cache`'s `&mut`. In hir-ty that is under
its slot's lock in `interner/tls_cache.rs`, which only that query's task takes. No reader waits
on one of its resizes, so growing `Interned` in place is fine there (request 4 changes this when
the cache is shared across queries).

```rust
use std::hash::{BuildHasher, Hash};

use derive_where::derive_where;
use rustc_hash::FxBuildHasher;

use super::{AvailableDepth, Cx, NestedGoals};
use crate::data_structures::HashMap;
use crate::search_graph::EvaluationResult;

struct Success<X: Cx> {
    required_depth: usize,
    /// The result's index in `GlobalCache::results`.
    result: X::Tracked<u32>,
}

struct WithOverflow<X: Cx> {
    nested_goals: NestedGoals<X>,
    /// The result's index in `GlobalCache::results`.
    result: X::Tracked<u32>,
}

#[derive_where(Debug; X: Cx)]
pub(super) struct CacheData<'a, X: Cx> {
    pub(super) result: X::Result,
    pub(super) required_depth: usize,
    pub(super) encountered_overflow: bool,
    pub(super) nested_goals: &'a NestedGoals<X>,
}

/// The goals solved and their results, each goal and each distinct result interned as a `u32`.
///
/// `success` holds a goal's result whose computation never hit the recursion limit, and
/// `success_nested` its nested goals when it has any; `with_overflow` the results which hit it,
/// by goal and the depth they were computed with.
pub struct GlobalCache<X: Cx> {
    goals: Interned<X::Input>,
    /// By goal.
    success: Vec<Option<Success<X>>>,
    success_nested: HashMap<u32, NestedGoals<X>>,
    with_overflow: HashMap<(u32, usize), WithOverflow<X>>,
    results: Interned<X::Result>,
    /// What a goal in `success` but not `success_nested` has.
    no_nested_goals: NestedGoals<X>,
}

impl<X: Cx> Default for GlobalCache<X> {
    fn default() -> Self {
        GlobalCache::new()
    }
}

impl<X: Cx> GlobalCache<X> {
    #[inline]
    pub const fn new() -> Self {
        GlobalCache {
            goals: Interned::new(),
            success: Vec::new(),
            success_nested: HashMap::with_hasher(FxBuildHasher),
            with_overflow: HashMap::with_hasher(FxBuildHasher),
            results: Interned::new(),
            no_nested_goals: NestedGoals { nested_goals: HashMap::with_hasher(FxBuildHasher) },
        }
    }

    /// Insert a final result into the global cache.
    pub(super) fn insert(
        &mut self,
        cx: X,
        input: X::Input,
        evaluation_result: EvaluationResult<X>,
        dep_node: X::DepNodeIndex,
    ) {
        let EvaluationResult { encountered_overflow, required_depth, heads, nested_goals, result } =
            evaluation_result;
        debug_assert!(heads.is_empty());
        let goal = self.goals.intern(input);
        if self.success.len() <= goal as usize {
            self.success.resize_with(goal as usize + 1, || None);
        }
        let index = self.results.intern(result);
        let tracked = cx.mk_tracked(index, dep_node);
        if encountered_overflow {
            let with_overflow = WithOverflow { nested_goals, result: tracked };
            let prev = self.with_overflow.insert((goal, required_depth), with_overflow);
            if let Some(prev) = &prev {
                cx.assert_evaluation_is_concurrent();
                let prev = cx.get_tracked(&prev.result);
                assert_eq!(prev, index, "{input:?}: {:?}, {result:?}", self.results.get(prev));
            }
        } else {
            let prev = self.success[goal as usize].replace(Success { required_depth, result: tracked });
            if nested_goals.is_empty() {
                if !self.success_nested.is_empty() {
                    self.success_nested.remove(&goal);
                }
            } else {
                self.success_nested.insert(goal, nested_goals);
            }
            if let Some(prev) = &prev {
                cx.assert_evaluation_is_concurrent();
                let prev = cx.get_tracked(&prev.result);
                assert_eq!(prev, index, "{input:?}: {:?}, {result:?}", self.results.get(prev));
            }
        }
    }

    /// Try to fetch a cached result, checking the recursion limit
    /// and handling root goals of coinductive cycles.
    ///
    /// If this returns `Some` the cache result can be used.
    pub(super) fn get<'a>(
        &'a self,
        cx: X,
        input: X::Input,
        available_depth: AvailableDepth,
        mut candidate_is_applicable: impl FnMut(&NestedGoals<X>) -> bool,
    ) -> Option<CacheData<'a, X>> {
        let goal = self.goals.find(&input)?;
        if let Some(Success { required_depth, ref result }) = self.success[goal as usize]
            && available_depth.cache_entry_is_applicable(required_depth)
        {
            let nested_goals = match self.success_nested.is_empty() {
                true => &self.no_nested_goals,
                false => self.success_nested.get(&goal).unwrap_or(&self.no_nested_goals),
            };
            if candidate_is_applicable(nested_goals) {
                return Some(CacheData {
                    result: *self.results.get(cx.get_tracked(result)),
                    required_depth,
                    encountered_overflow: false,
                    nested_goals,
                });
            }
        }

        let additional_depth = available_depth.0;
        if !self.with_overflow.is_empty()
            && let Some(WithOverflow { nested_goals, result }) =
                self.with_overflow.get(&(goal, additional_depth))
            && candidate_is_applicable(nested_goals)
        {
            return Some(CacheData {
                result: *self.results.get(cx.get_tracked(result)),
                required_depth: additional_depth,
                encountered_overflow: true,
                nested_goals,
            });
        }

        None
    }
}

/// Values interned as their index in `values`, found through `slots`: open-addressed on the
/// value's hash with linear probing, each slot an index plus one or 0 for none, at most half of
/// them full. A probe compares the stored hash before the value; growing rehashes nothing.
/// Cost: 4 bytes a slot, 4 bytes plus the value a value; a lookup is one hash and, at half
/// load, about 1.5 slot reads.
#[derive_where(Default)]
struct Interned<T> {
    slots: Vec<u32>,
    values: Vec<T>,
    /// By value: its hash.
    hashes: Vec<u32>,
}

impl<T: Hash + Eq> Interned<T> {
    const fn new() -> Self {
        Interned { slots: Vec::new(), values: Vec::new(), hashes: Vec::new() }
    }

    fn get(&self, index: u32) -> &T {
        &self.values[index as usize]
    }

    fn find(&self, value: &T) -> Option<u32> {
        self.find_hashed(value, hash_of(value))
    }

    fn find_hashed(&self, value: &T, hash: u32) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        loop {
            let filled = self.slots[at];
            if filled == 0 {
                return None;
            }
            let index = filled as usize - 1;
            if self.hashes[index] == hash && self.values[index] == *value {
                return Some(filled - 1);
            }
            at = (at + 1) & mask;
        }
    }

    fn intern(&mut self, value: T) -> u32 {
        let hash = hash_of(&value);
        if let Some(found) = self.find_hashed(&value, hash) {
            return found;
        }
        if (self.values.len() + 1) * 2 > self.slots.len() {
            let len = (self.slots.len() * 2).max(16);
            self.slots.clear();
            self.slots.resize(len, 0);
            for index in 0..self.hashes.len() {
                self.place(self.hashes[index], index as u32);
            }
        }
        let index = u32::try_from(self.values.len())
            .ok()
            .filter(|&it| it < u32::MAX)
            .expect("the solver's cache holds `u32::MAX - 1` values of one kind");
        self.place(hash, index);
        self.values.push(value);
        self.hashes.push(hash);
        index
    }

    fn place(&mut self, hash: u32, index: u32) {
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        while self.slots[at] != 0 {
            at = (at + 1) & mask;
        }
        self.slots[at] = index + 1;
    }
}

/// FxHash of `value`, its high bits folded down so linear probing on the low bits spreads.
fn hash_of<T: Hash>(value: &T) -> u32 {
    (FxBuildHasher.hash_one(value).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32
}

#[cfg(test)]
mod tests {
    use super::Interned;

    #[test]
    fn a_value_interned_twice_has_one_index() {
        let mut interned = Interned::<u64>::default();
        let indices: Vec<u32> = (0..1000u64).map(|it| interned.intern(it * 7919)).collect();
        assert_eq!(indices, (0..1000).collect::<Vec<u32>>());
        for (it, &index) in indices.iter().enumerate() {
            assert_eq!(interned.intern(it as u64 * 7919), index);
            assert_eq!(interned.find(&(it as u64 * 7919)), Some(index));
            assert_eq!(*interned.get(index), it as u64 * 7919);
        }
        assert_eq!(interned.find(&1), None);
        assert!(interned.slots.len() >= 2 * interned.values.len());
        assert_eq!(interned.hashes.len(), interned.values.len());
    }

    #[test]
    fn an_empty_table_finds_nothing() {
        assert_eq!(Interned::<u64>::default().find(&0), None);
    }
}
```

Behavior compared with today's file, line by line: `get` returns `None` for an unknown input
(the old `self.map.get(&input)?`), then tries `success` with the goal's nested goals (empty when
it has none, as an entry with an empty `NestedGoals` was), then `with_overflow` at
`available_depth.0` (the old `entry.with_overflow.get(&additional_depth)`). `insert` replaces
the success row and its nested goals together, as `entry.success.replace` did, and keeps both
asserts. `NestedGoals`' private field is visible here: `global_cache` is a child of
`search_graph`.

> forwarded to db: `vendor/rustc_type_ir` is lane db's.
> checked by db-audit-dbsub-tysmall (22:33): in the tree, and overtaken by its successor: `vendor/rustc_type_ir/src/search_graph/global_cache.rs` has goals and results as `Interned` `u32`s with nested goals in a side column, now in request 4 step 3's lock-free form (published rows, `Chunked` levels, `&self` reads).

## 3. db-ty-hir (sibling), `crates/hir/src/tests.rs`: a fixture test of the shared solver cache

What: one test, using hir-ty's public `next_solver` API on a `TestDB`:

- Fixture: `//- minicore: sized` and a crate with `trait Tr {}`, `struct A; struct B<T>(T);
  struct C;`, `impl Tr for A {}`, `impl<T: Tr> Tr for B<T> {}`.
- Goals: `X: Tr` for `X` in `A`, `C`, `B<A>`, `B<C>`, `B<B<A>>`, `B<B<C>>`, `B<B<B<A>>>`. Build
  the types with `Ty::new_adt(interner, adt, GenericArgs::new_from_slice(&[..]))`.
- Cold: for each goal, a fresh `DbInterner::new_with(&db, krate)` (so a fresh `Solved`, and an
  empty cache) and `interner.infer_ctxt().build(TypingMode::non_body_analysis())`, then
  `infcx.type_implements_trait(tr, [ty], ParamEnv::empty())`.
- Warm: one interner and inference context for all goals, every goal asked twice (the second
  answer comes from the cache), and then again from another thread inside `std::thread::scope`
  with the same interner (needs the database to be `Sync`, which db-salsa's v2 database is).
- Assert that the warm `EvaluationResult`s equal the cold ones at every step, and that the `A`,
  `B<A>`, `B<B<A>>`, `B<B<B<A>>>` goals hold while the rest do not.

Why: the tests in `interner/tls_cache.rs` check the shared table with plain values (a goal is not
reused by another query or state, a cache filled on one thread serves another, answers equal a
per-thread cache's). This one checks the same through the real solver. hir-ty has no test
database of its own (shared rule 5).

> forwarded to db: db-ty-hir's `crates/hir/src/tests.rs`, inside lane db.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir/src/tests.rs:452` (`a_solved_goal_answers_the_same_from_the_cache_and_on_another_thread`: cold per goal, warm twice, then in `std::thread::scope`).

## 4. Lane db (db-salsa, then db-ty-types, rustc_type_ir): share solved goals across queries, as rustc does

What it would take to match and beat rustc's new solver cache, which today's design does not:

- **The benchmark:** rustc's `tcx.new_solver_evaluation_cache`. It is one `GlobalCache` for the
  whole session: every query reuses every goal any query solved. Its soundness comes from
  `Cx::with_cached_task` (an anonymous dep-graph task recording what the goal read) and
  `get_tracked` (replaying that dep node into whichever query uses the result). Its weakness is
  one `Lock` around the whole map.
- **Ours today:** one cache per query (`Solved`), lock-free across threads, `u32`-interned once
  request 2 lands. It can't reuse across queries: `DbInterner::with_cached_task` records nothing
  (`DepNodeIndex` is `()`), so a hit in another query would miss the goal's edges. That
  separation is exactly what `Solved` enforces.

Changes, in order:

1. **db-salsa, `vendor/salsa`:** `pub fn capture_reads<R>(db: &dyn Database, f: impl FnOnce() ->
   R) -> (R, Option<Reads>)`. It runs `f` in a frame on the query stack carrying the active
   query's `DatabaseKeyIndex` (so identity, cycle detection and tracked-struct disambiguation
   are the parent's). On exit, every read of the frame (edge, durability, changed_at,
   fingerprint) is merged into the parent, as if read directly, and also returned as `Reads`.
   `Reads` is a `u32` into a per-revision arena of edge lists stored as CSR, `Copy`. The result
   is `None`, so the goal may not be shared, when the frame read a provisional value of a
   fixpoint cycle (non-empty cycle heads), created a tracked struct, wrote an output or
   accumulated. Also `pub fn replay_reads(db: &dyn Database, reads: Reads)`, which appends a
   capture's edges to the active query, deduplicated by its index set. Neither takes a lock;
   the arena is per-worker segments published at task end (concurrency.md, "Dependencies are
   recorded per task").

   > applied by db-salsa-reads: `vendor/salsa/src/capture.rs`. `Reads` (32 bytes, `Copy`) is a
   > range of S5's one edge array (the frame's edges, then its fingerprints), plus durability,
   > `changed_at` and revision, rather than a second arena; `replay_reads` in another revision
   > panics naming both. Report: `/tmp/blaze-lanes/v2-db-salsa-reads.md`.
2. **db-ty-types, `interner.rs`:** `type DepNodeIndex = Option<salsa::Reads>`;
   `with_cached_task` calls `capture_reads`; `Tracked<T>` holds `(T, Option<Reads>)`;
   `get_tracked` calls `replay_reads` when `Some`.
3. **Lane db, `rustc_type_ir` search graph:** a `GlobalCache` read through `&self` with no lock.
   Its slot arrays become append-only levels (concurrency.md, "a full table grows by adding a
   level"), each row published once with a release store, and `insert` claims a row with a
   compare-and-swap. `Cx::with_global_cache` takes `FnOnce(&GlobalCache<Self>)`. A goal whose
   capture was `None` goes into the per-query cache only.
4. **db-ty-solver (me), `tls_cache.rs`:** key the shared cache by `(krate, regions)` and the
   database state instead of `Solved` (impls, lang items and the recursion limit depend on the
   crate; `regions` changes the answer). Keep the per-query table for goals whose capture was
   `None`. I write this as soon as 1 to 3 exist. It is about 40 lines on top of today's table.

Why: a body's inference asks the same goals (`T: Sized`, `Vec<u8>: Clone`, `String: Display`)
that every other body asked. With per-query caches each body solves them again; rustc solves
them once per session. With 1 to 4, blaze solves them once per revision across every thread,
with lock-free reads where rustc takes a lock. That beats the benchmark on both counts.

> forwarded to db: a design inside lane db (db-salsa, db-ty-types, `rustc_type_ir`).
> checked by db-audit-dbsub-tysmall (22:33): in the tree, all four steps: `vendor/salsa/src/capture.rs`; `type DepNodeIndex = Option<salsa::Reads>` with `capture_reads`/`replay_reads` in `crates/hir-ty/src/next_solver/interner.rs:1176-1261`; the lock-free `GlobalCache` in `vendor/rustc_type_ir/src/search_graph/global_cache.rs`; the shared cache keyed by `Scope { nonce, krate, regions }` in `interner/tls_cache.rs:152-164`.
