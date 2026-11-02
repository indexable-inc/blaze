# Requests of mem-db-solver

mem-db-solver's part of mem-db's contract (`docs/v2/plan/lanes/mem/db.md`, `v2/requests/mem-db.md`):
`crates/hir-ty/src/next_solver/` (except `interner/tls_cache.rs`, mem-db's), `vendor/rustc_type_ir/`
and `vendor/rustc_next_trait_solver/`. Notes, the design argument and the speed-of-light bound:
`docs/v2/plan/lanes/mem/db-solver.md`. Every path here belongs to lane db and its children
(solver-sync and its children, db-ty-port-solver), none of them DONE as a lane, so every change is
a request: who, file and line, the code before and after, why, and the allocation-budget test.

Lines are as of 2026-10-29 20:20. The vendored crates are a patch series that
`tools/v2/solver_sync.py` replays onto upstream rustc 16d1ffac daily. The changes to them here are
new patches with a `PATCHES.md` heading each: rustc_type_ir 0006 and rustc_next_trait_solver 0004.

## The one design decision: recycled per-worker storage, not `&'f Frame`, for the solver

mem-db's contract threads the run's `&'f Frame` into context structs and names the solver's search
graph. For the solver that can't work as asked. Instead, its per-goal temporaries live in storage
that each worker keeps and reuses: emptied with capacity kept, and given back when a goal (or an
`InferCtxt`) ends. This is what upstream already does for the canonicalizer, with
`SolverDelegate::obtain_canonicalizer_state` and `release_canonicalizer_state`. mem-db was told by
tmux at 20:05. There are four reasons:

1. **The frame would hold the sum of every goal's temporaries, not the largest goal's.**
   - `build_with_canonical` builds a fresh `InferCtxt`, with all its tables, for every canonical
     goal it computes: thousands per `infer` run.
   - Code holding `&Frame` can't `reset_to` or `scope` (both need `&mut Frame`). A goal's tables
     would therefore stay until the run ends.
   - Recycled storage is bounded by the deepest stack of goals alive at once.
2. **Lifetimes.**
   - A frame-allocator type parameter means a new lifetime on `EvalCtxt` (160 mentions in the
     vendored code, which is rebased daily) and on `InferCtxt` (124 mentions in hir-ty).
   - `'db` can't carry the frame either: query results are `InferenceResult<'db>`, which outlives
     the run.
3. **Cache warmth.** A reused buffer is hot in L1/L2. A frame bump moves forward over cold lines for
   each goal.
4. **No allocations warm.** Once the high water is reached, nothing allocates, which is the
   contract's actual goal for hit paths and the warm steady state.

So the contract's "where the `&'f Frame` comes from" has this answer for the solver: no frame.
- **Root goals:** the storage comes through `SolverDelegate` (two new provided methods, request
  3).
- **Each canonical goal's `InferCtxt`:** the storage comes from a worker shelf in hir-ty
  (request 5).
- **Trait items that change:**
  - `rustc_type_ir::search_graph::Delegate` gets one associated type, `Scratch`.
  - `SolverDelegate` gets `obtain_search_storage` and `release_search_storage`.
  - `Interner` and `InferCtxtLike` don't change.

mem-db-checks' B4 (`v2/requests/mem-db-checks.md`, line 1522) expected these tables to come from
the body's frame. They don't need to: they recycle without one.

> forwarded to db (from requests, pass 16; told by tmux).
> applied: nothing to apply (design note; requests 1 to 11 carry it out) (db-mem-infer-a, 21:58).

## What is made, how often, and where it goes (the inventory)

"Per goal" means per canonical goal computed, that is, a global-cache miss. "Hit" means a goal
answered from the global cache.

| Value | File:line | Type now | How often | After |
|---|---|---|---|---|
| search stack | `rustc_type_ir/src/search_graph/stack.rs` (`Stack::entries`) | `IndexVec<StackDepth, StackEntry>` | 1 per root goal, grows per nested goal | in `SearchGraphStorage`, recycled (req 2) |
| provisional cache | `search_graph/mod.rs:645` | `HashMap<Input, Vec<ProvisionalCacheEntry>>` | map per root; a `Vec` per goal in a cycle | map recycled; emptied lists to `spare_entries` (req 2) |
| cycle heads | `search_graph/mod.rs:364` | `BTreeMap<StackDepth, CycleHead>` | a node per goal on a cycle | `SmallVec<[_; 4]>`, sorted (req 1) |
| candidate head usages | `search_graph/mod.rs:255` | `Option<Box<HashMap<StackDepth, HeadUsages>>>` | a box and map per candidate under a cycle head | `SmallVec<[_; 2]>`, sorted (req 1) |
| nested goals (graph) | `search_graph/mod.rs:529` | `HashMap<Input, PathsToNested>` | only after a cycle or overflow; stored in the global cache | unchanged: owned by the cache entry (see "Caches that outlive a goal") |
| eval ctxt nested goals | `rustc_next_trait_solver/src/solve/eval_ctxt/mod.rs:152` | `Vec<(GoalSource, Goal, Option<GoalStalledOn>)>` | 1 per canonical goal, 1 per probe (`probe.rs:56` clones it), `mem::take` per step (`:1060`) | from the scratch pool (req 3) |
| candidates | `solve/assembly/mod.rs:483` | `Vec<Candidate>` | 1 per trait, normalizes-to or host-effect goal | from the scratch pool (req 3) |
| merge lists | `solve/trait_goals.rs:1678, 1693, 1733` | `Vec<Candidate>` (`extract_if(..).collect()`) | up to 3 per trait goal | from the scratch pool (req 3) |
| original values | `canonical/canonicalizer.rs:277` (`mem::take(&mut state.variables)`) | `ThinVec<GenericArg>` | 1–2 per goal **and per hit** | copied into a pooled `Vec`; the state keeps its buffer (req 3) |
| response instantiation | `canonical/mod.rs:212` | `IndexVec<BoundVar, Option<GenericArg>>` | 1 per goal **and per hit** | from the scratch pool (req 3) |
| response dedupe set | `solve/eval_ctxt/mod.rs:1707` | `HashSet` | 1 per response | from the scratch pool (req 3) |
| param-env universes | `solve/assembly/mod.rs:1400` (`FindParamInClause { universes: vec![] }`) | `Vec<Option<UniverseIndex>>` | 1 per param-env assumption visited with binders | from the scratch pool (req 3) |
| canonicalizer state | hir-ty `infer/mod.rs:296`, `solver.rs:629-636` | `CanonicalizerState` (a `ThinVec`, a `Vec`, two maps, a `DelayedMap`) | fresh in every `InferCtxt`, so 1 per goal | in the recycled `InferCtxt` storage (req 5) |
| `InferCtxtInner` tables | hir-ty `infer/mod.rs:93-163` | 5 `UnionFind`s × 4 columns, `TypeVariableStorage` params, region-constraint columns, opaque storage, 2 obligation `Vec`s | 1 set per `InferCtxt` (per goal, per fork, per body) | recycled with the `InferCtxt` (req 5) |
| placeholder assumptions, overflows | hir-ty `infer/mod.rs:250-296` | `FxIndexMap`, `Vec` | per `InferCtxt` that uses them | recycled with the `InferCtxt` (req 5) |
| fulfillment pending | hir-ty `fulfill.rs:26` (`Pending`, five column `Vec`s) | 5 `Vec`s | 1 set per `FulfillmentCtxt` (31 `ObligationCtxt`/`FulfillmentCtxt::new` sites) | recycled (req 6) |
| fulfillment errors | hir-ty `fulfill.rs:176, 199` | `Vec<NextSolverError>` | empty unless an error | unchanged: an empty `Vec` doesn't allocate; errors are results |
| canonical instantiation cache | hir-ty `infer/canonical/instantiate.rs:140` | `FxHashMap<Ty, Ty>` | 1 per instantiation that reaches a type with bound vars that isn't one itself, **on the hit path** | `DelayedMap`, as rustc and rustc_type_ir's `BoundVarReplacer` (req 7) |
| generalize cache | hir-ty `infer/relate/generalize.rs:350` | `ArrayVec<_, 8>` + `FxHashMap` spill | already allocation-free below 8 entries | unchanged |
| relating goals | `rustc_type_ir/src/relate/solver_relating.rs:50` | `Vec<Goal>` | 1 per relate that emits a goal (alias or ambiguous) | unchanged: empty usually; when not empty it becomes the goals added, i.e. a result |
| folders | `rustc_type_ir/src/fold.rs:558` | `DelayedMap` | inline until 32 visits | unchanged |
| stalled-on lists | `eval_ctxt/mod.rs:922-968` (`GoalStalledOn { stalled_vars, sub_roots }`) | 2 `ThinVec`s | per ambiguous goal without progress | unchanged: it is stored in the fulfillment pending set and the nested-goal lists. It is a result, built from the borrowed original values (req 3) |
| external constraints | `eval_ctxt/mod.rs:1668, 1775` | the `Vec`s of `ExternalConstraintsData` | per response with region, opaque or normalization constraints | built in pooled `Vec`s and interned from a borrow (req 4) |

> forwarded to db (from requests, pass 16; told by tmux).
> applied: nothing to apply (inventory; its rows point at requests 1 to 11) (db-mem-infer-a, 21:58).

### Caches that outlive a goal (not frame values)

- **The `GlobalCache`** (`rustc_type_ir/src/search_graph/global_cache.rs`, patch 0003) is the
  generation-owned table of one `tls_cache` slot:
  - a shared generation per crate and revision, or
  - a query's own, per `DbInterner::solved`.
- Its lifetime is the generation's, so it is a pool value, not frame or scratch storage:
  - mem-db request 4 (`v2/requests/mem-db.md`): generations from a `blaze_mem::Pool`, retired
    through its `Limbo`.
  - solver-sync-glue request 8: the same pool and limbo.
  - solver-sync-glue request 9: `GlobalCache::clear(&mut self)`, which keeps its chunks. The
    chunks (`Chunked`/`Interned`, `OnceLock<Box<[T]>>`) move onto a region tagged `Infer`.
- Not refiled here. The `NestedGoals` maps stored in its `nested_goals` chunk are owned by the
  entry. They exist only for goals that met a cycle or overflow, and request 9's `clear` drops
  them.
- **`tls_cache`** holds the generations themselves (mem-db's file, its request 4). What goes into
  them is the `GlobalCache` above.
- **The canonical param-env cache** (`with_canonical_param_env_cache`, hir-ty `interner.rs:1273`)
  is per thread and per revision, so it is a cache too, not a temporary. Its entries clone
  `variables`, `var_kinds` and `variable_lookup_table` (`canonicalizer.rs:154-188`) once per
  param-env per revision. That is not per goal, and stays.

> forwarded to db (from requests, pass 16; told by tmux).
> applied: nothing to apply (these caches stay as they are) (db-mem-infer-a, 21:58).

## 1. solver-sync / lane db (`vendor/rustc_type_ir`, new patch 0006): cycle heads and candidate usages without heap nodes

- **Who:** solver-sync owns `vendor/` (DONE); lane db (window `db-next2`) is the owner while db is
  not DONE.
- **Why:**
  - `CycleHeads` is a `BTreeMap`. Its first insert allocates a node: one for every goal whose
    result depends on a cycle head.
  - `CandidateHeadUsages` boxes a `HashMap` for every candidate evaluated under a cycle head.
  - Both usually hold 1–3 entries.
- **The fix:**
  - A sorted `SmallVec` keeps the same order (by `StackDepth`), the same
    first/last/`pop_last` semantics and the same determinism, and doesn't allocate up to its
    inline size.
  - rustc_type_ir already depends on smallvec (`vendor/rustc_type_ir/Cargo.toml`).
- **Behavior:** unchanged. Every operation keeps its map semantics:
  - `insert` merges into an existing head;
  - iteration is in key order;
  - `ignore_usages` panics on a missing head, as `get_mut(..).unwrap()` does.

`search_graph/mod.rs:362-418`, before:

```rust
#[derive(Clone, Debug, Default)]
struct CycleHeads {
    heads: BTreeMap<StackDepth, CycleHead>,
}

impl CycleHeads {
    fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    fn highest_cycle_head(&self) -> (StackDepth, CycleHead) {
        self.heads.last_key_value().map(|(k, v)| (*k, *v)).unwrap()
    }
    // ... opt_highest_cycle_head_index, opt_lowest_cycle_head_index, remove_highest_cycle_head
    fn insert(&mut self, head_index: StackDepth, path_from_entry: impl Into<PathsToNested> + Copy, usages: HeadUsages) {
        match self.heads.entry(head_index) {
            btree_map::Entry::Vacant(entry) => {
                entry.insert(CycleHead { paths_to_head: path_from_entry.into(), usages });
            }
            btree_map::Entry::Occupied(entry) => {
                let head = entry.into_mut();
                head.paths_to_head |= path_from_entry.into();
                head.usages.add_usages_from_nested(usages);
            }
        }
    }

    fn ignore_usages(&mut self, head_index: StackDepth, usages: HeadUsages) {
        self.heads.get_mut(&head_index).unwrap().usages.ignore_usages(usages)
    }

    fn iter(&self) -> impl Iterator<Item = (StackDepth, CycleHead)> + '_ {
        self.heads.iter().map(|(k, v)| (*k, *v))
    }
}
```

after:

```rust
/// All cycle heads a given goal depends on, ordered by their stack depth: a sorted vector, as a
/// goal depends on few heads, so the common case allocates nothing.
#[derive(Clone, Debug, Default)]
struct CycleHeads {
    heads: SmallVec<[(StackDepth, CycleHead); 4]>,
}

impl CycleHeads {
    fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    fn highest_cycle_head(&self) -> (StackDepth, CycleHead) {
        *self.heads.last().unwrap()
    }

    fn highest_cycle_head_index(&self) -> StackDepth {
        self.opt_highest_cycle_head_index().unwrap()
    }

    fn opt_highest_cycle_head_index(&self) -> Option<StackDepth> {
        self.heads.last().map(|&(k, _)| k)
    }

    fn opt_lowest_cycle_head_index(&self) -> Option<StackDepth> {
        self.heads.first().map(|&(k, _)| k)
    }

    fn remove_highest_cycle_head(&mut self) -> CycleHead {
        self.heads.pop().unwrap().1
    }

    fn insert(
        &mut self,
        head_index: StackDepth,
        path_from_entry: impl Into<PathsToNested> + Copy,
        usages: HeadUsages,
    ) {
        match self.heads.binary_search_by_key(&head_index, |&(k, _)| k) {
            Ok(at) => {
                let head = &mut self.heads[at].1;
                head.paths_to_head |= path_from_entry.into();
                head.usages.add_usages_from_nested(usages);
            }
            Err(at) => self.heads.insert(
                at,
                (head_index, CycleHead { paths_to_head: path_from_entry.into(), usages }),
            ),
        }
    }

    fn ignore_usages(&mut self, head_index: StackDepth, usages: HeadUsages) {
        let at = self.heads.binary_search_by_key(&head_index, |&(k, _)| k).unwrap();
        self.heads[at].1.usages.ignore_usages(usages)
    }

    fn iter(&self) -> impl Iterator<Item = (StackDepth, CycleHead)> + '_ {
        self.heads.iter().copied()
    }
}
```

`search_graph/mod.rs:253-276`, before: `usages: Option<Box<HashMap<StackDepth, HeadUsages>>>`
with `merge_usages` as shown at those lines. After:

```rust
#[derive(Debug, Default)]
pub struct CandidateHeadUsages {
    /// Sorted by stack depth; a candidate under a cycle usually uses one or two heads.
    usages: SmallVec<[(StackDepth, HeadUsages); 2]>,
}
impl CandidateHeadUsages {
    fn entry(&mut self, head_index: StackDepth) -> &mut HeadUsages {
        let at = match self.usages.binary_search_by_key(&head_index, |&(k, _)| k) {
            Ok(at) => at,
            Err(at) => {
                self.usages.insert(at, (head_index, HeadUsages::default()));
                at
            }
        };
        &mut self.usages[at].1
    }

    pub fn merge_usages(&mut self, other: CandidateHeadUsages) {
        for (head_index, head) in other.usages {
            let HeadUsages { inductive, unknown, coinductive, forced_ambiguity } = head;
            let self_usages = self.entry(head_index);
            self_usages.inductive += inductive;
            self_usages.unknown += unknown;
            self_usages.coinductive += coinductive;
            self_usages.forced_ambiguity += forced_ambiguity;
        }
    }
}
```

The callers:
- `mod.rs:693-699` changes from
  `candidate_usages.usages.get_or_insert_default().entry(head_index).or_default().add_usages_from_nested(head.usages)`
  to `candidate_usages.entry(head_index).add_usages_from_nested(head.usages)`.
- `mod.rs:775-786` changes from `if let Some(usages) = usages.usages { .. for (head_index, usages) in usages.into_iter() { .. } }`
  to `for (head_index, usages) in usages.usages { .. }`, with the `entry_index` and `else`
  arms as they are.
- Iteration is now in depth order rather than hash order. Both loops' comments already say the
  result doesn't depend on order, which this makes literally true.
- `use std::collections::btree_map` and `BTreeMap` go when unused.

- **Budget test (rustc_type_ir, a unit test in `search_graph/mod.rs`):**
  - Use `blaze-alloc-count` as a dev-dependency (the crate's own `Cargo.toml`; mem-db is told
    about the new dependency).
  - Insert three heads in reverse depth order and assert ascending order with `iter()`.
  - Assert `highest_cycle_head` is the deepest and `remove_highest_cycle_head` pops it.
  - Wrap it in `blaze_alloc_count::assert_at_most(0, "three cycle heads", ..)`.
  - Do the same for `CandidateHeadUsages::merge_usages` of two two-head sets.
- **Differential:** the next-solver UI tests in `harness/` (cycles, `coinductive` and `fixpoint`)
  give the same results. They run in the release train.

> forwarded to solver-sync and lane db (windows `solver-sync`, `db-next2`; told by tmux 20:27). The blaze-alloc-count dev-dependency is told to mem-db.
> applied by db-mem-infer-a-solver-vendor (21:39): as written, in `rustc_type_ir` patch 0006 (`0006-solver-storage-recycled`); the merge test uses two overlapping two-head sets so it stays inline; no test-fixture needed.

## 2. solver-sync / lane db (`vendor/rustc_type_ir`, patch 0006): the search graph's storage, recycled

- **Why:**
  - `SearchGraph::new` (`search_graph/mod.rs:666`) makes an empty stack and an empty provisional
    cache for every root goal (`enter_root`, `eval_ctxt/mod.rs:470`).
  - The first nested goal grows the stack. The first cycle grows the map and a `Vec` per input.
  - All of it is freed when the root goal ends, so a body's thousands of root goals each pay
    again.
- **The fix:** give the graph its storage and take it back.
  - The provisional cache's per-input lists are emptied by `retain` (`mod.rs:980, 1038`), and
    the emptied ones go to a spare list.
  - The graph also carries the delegate's scratch (request 3), so the next solver's pools live
    with the graph that every `EvalCtxt` already borrows mutably.

`search_graph/mod.rs`, a new item beside `SearchGraph`:

```rust
/// What a search graph allocates, kept between root goals: emptied, with its capacity, when
/// the graph ends, so the next root goal on this worker grows nothing it grew before.
pub struct SearchGraphStorage<X: Cx, S> {
    stack: Stack<X>,
    provisional_cache: HashMap<X::Input, Vec<ProvisionalCacheEntry<X>>>,
    /// Emptied lists of the provisional cache, reused for the next input that needs one.
    spare_entries: Vec<Vec<ProvisionalCacheEntry<X>>>,
    scratch: S,
}

impl<X: Cx, S: Default> Default for SearchGraphStorage<X, S> {
    fn default() -> Self {
        SearchGraphStorage {
            stack: Default::default(),
            provisional_cache: Default::default(),
            spare_entries: Vec::new(),
            scratch: S::default(),
        }
    }
}

impl<X: Cx, S> SearchGraphStorage<X, S> {
    /// Bytes it holds room for: what a shelf keeps the largest of.
    pub fn held(&self) -> usize {
        self.stack.capacity() * size_of::<StackEntry<X>>()
            + self.provisional_cache.capacity()
                * size_of::<(X::Input, Vec<ProvisionalCacheEntry<X>>)>()
            + self.spare_entries.iter().map(|it| it.capacity()).sum::<usize>()
                * size_of::<ProvisionalCacheEntry<X>>()
    }
}
```

The `SearchGraph` struct (`mod.rs:640-645`) gets `spare_entries` and `scratch` fields beside
`stack` and `provisional_cache`. `SearchGraph::new` (`mod.rs:666`), before:

```rust
    pub fn new(root_depth: usize) -> SearchGraph<D> {
        Self {
            root_depth: AvailableDepth(root_depth),
            stack: Default::default(),
            provisional_cache: Default::default(),
            _marker: PhantomData,
        }
    }
```

after:

```rust
    pub fn new(root_depth: usize) -> SearchGraph<D> {
        SearchGraph::new_in(root_depth, Default::default())
    }

    /// A graph over `storage`, which must be empty (as [`SearchGraph::into_storage`] leaves it).
    pub fn new_in(
        root_depth: usize,
        storage: SearchGraphStorage<D::Cx, D::Scratch>,
    ) -> SearchGraph<D> {
        let SearchGraphStorage { stack, provisional_cache, spare_entries, scratch } = storage;
        debug_assert!(stack.is_empty() && provisional_cache.is_empty());
        Self {
            root_depth: AvailableDepth(root_depth),
            stack,
            provisional_cache,
            spare_entries,
            scratch,
            _marker: PhantomData,
        }
    }

    /// Its storage, emptied with its capacity kept, for the next graph.
    pub fn into_storage(self) -> SearchGraphStorage<D::Cx, D::Scratch> {
        let SearchGraph { mut stack, mut provisional_cache, mut spare_entries, scratch, .. } = self;
        stack.clear();
        spare_entries.extend(provisional_cache.drain().map(|(_, mut entries)| {
            entries.clear();
            entries
        }));
        SearchGraphStorage { stack, provisional_cache, spare_entries, scratch }
    }

    /// The delegate's scratch, kept with the graph.
    pub fn scratch(&mut self) -> &mut D::Scratch {
        &mut self.scratch
    }
```

The rest of the change:
- `Stack` gets `fn clear(&mut self)` and `fn capacity(&self) -> usize` (`stack.rs`, over
  `entries`).
- At `mod.rs:928`, before:
  `let entry = self.provisional_cache.entry(input).or_default();`. After:

```rust
            let spare = &mut self.spare_entries;
            let entry =
                self.provisional_cache.entry(input).or_insert_with(|| spare.pop().unwrap_or_default());
```

- At `mod.rs:980-986` (`clear_dependent_provisional_results_for_rerun`), before:

```rust
        self.provisional_cache.retain(|_, entries| {
            entries.retain(|entry| { .. });
            !entries.is_empty()
        });
```

  after (the closure captures the two fields disjointly):

```rust
        let spare = &mut self.spare_entries;
        self.provisional_cache.retain(|_, entries| {
            entries.retain(|entry| { .. });
            if entries.is_empty() {
                spare.push(mem::take(entries));
                return false;
            }
            true
        });
```

- The outer `retain` in `rebase_provisional_cache_entries` (`mod.rs:1038`) ends the same way:
  where it returns `!entries.is_empty()`, an emptied list goes to `spare_entries`. Its closure
  reads `self.stack`, which is a field disjoint from `spare_entries`.
- `search_graph::Delegate` gets `type Scratch: Default;`, with the doc "per-worker temporaries
  the delegate keeps in the graph".
- **The provisional cache is a std `HashMap` here, and it stays one.** Recycling keeps its
  capacity. The order its entries are visited in is unchanged, because the map is the same type
  and `retain` and `drain` visit every entry.
- **Budget test (rustc_type_ir, `search_graph/mod.rs` tests):**
  - There is no `Cx` test double in rustc_type_ir. The test belongs with request 3's, where the
    real delegate exists: the end-to-end tests in request 8 cover a recycled graph growing
    nothing.
  - A unit test here: `into_storage` of a graph whose stack held entries returns a storage
    whose `held()` is unchanged, and a `new_in` over it starts empty.

> forwarded to solver-sync and lane db (told by tmux 20:27).
> applied by db-mem-infer-a-solver-vendor (21:39): in patch 0006; the proof-tree root (`evaluate_root_goal_for_proof_tree_raw`) still uses `SearchGraph::new` (no delegate reaches it); the cycle test checks `held()` unchanged, and the zero-allocation test is on an acyclic goal (a cycle's `NestedGoals` map allocates).

## 3. solver-sync / lane db (`vendor/rustc_next_trait_solver`, new patch 0004): the eval context's temporaries from the graph's scratch

- **Why (the counts are per goal computed; "hit" is the global-cache hit path):**
  - `nested_goals`: 1 per canonical goal, 1 clone per probe, and 1 per added-goals step.
  - `candidates`: 1 per goal, up to 3 more to merge.
  - `orig_values`: **1–2 per hit**, because the canonicalizer loses its `variables` buffer to
    `mem::take` and the next one grows a new one.
  - `opt_values`: **1 per hit**.
  - The dedupe set: 1 per response.
  - With `SolverScratch` pooled in the search graph (request 2), each becomes a pop and a push
    of a warm buffer.

`solve/mod.rs` (or a new `solve/scratch.rs`), new:

```rust
/// Buffers the solver uses inside one goal, kept in the search graph between goals (and by the
/// delegate between root goals, `SolverDelegate::obtain_search_storage`): each is taken empty
/// and given back emptied, so a warm worker allocates none of them.
pub struct SolverScratch<I: Interner> {
    nested_goals: Vec<Vec<(GoalSource, Goal<I, I::Predicate>, Option<GoalStalledOn<I>>)>>,
    candidates: Vec<Vec<Candidate<I>>>,
    args: Vec<Vec<I::GenericArg>>,
    opt_args: Vec<Vec<Option<I::GenericArg>>>,
    universes: Vec<Vec<Option<ty::UniverseIndex>>>,
    seen_outlives: Vec<HashSet<ty::OutlivesPredicate<I, I::GenericArg>>>,
}

impl<I: Interner> Default for SolverScratch<I> { /* every pool `Vec::new()` */ }

macro_rules! pool {
    ($take:ident, $give:ident, $field:ident: $ty:ty) => {
        pub(crate) fn $take(&mut self) -> $ty {
            self.$field.pop().unwrap_or_default()
        }
        pub(crate) fn $give(&mut self, mut buffer: $ty) {
            buffer.clear();
            self.$field.push(buffer);
        }
    };
}

impl<I: Interner> SolverScratch<I> {
    pool!(take_nested_goals, give_nested_goals, nested_goals: Vec<(GoalSource, Goal<I, I::Predicate>, Option<GoalStalledOn<I>>)>);
    pool!(take_candidates, give_candidates, candidates: Vec<Candidate<I>>);
    pool!(take_args, give_args, args: Vec<I::GenericArg>);
    pool!(take_opt_args, give_opt_args, opt_args: Vec<Option<I::GenericArg>>);
    pool!(take_universes, give_universes, universes: Vec<Option<ty::UniverseIndex>>);
    pool!(take_seen_outlives, give_seen_outlives, seen_outlives: HashSet<ty::OutlivesPredicate<I, I::GenericArg>>);

    /// Bytes it holds room for.
    pub fn held(&self) -> usize { /* sum of each pooled buffer's capacity times its element size */ }
}

pub type SearchStorage<I> = SearchGraphStorage<I, SolverScratch<I>>;
```

The pools' depth is the goal nesting depth, at most the recursion limit. They're emptied
`Vec`s, so they hold only capacity.

**`search_graph` delegate** (`solve/search_graph.rs`, `impl Delegate for SearchGraphDelegate<D>`):
add `type Scratch = SolverScratch<I>;`.

**`SolverDelegate`** (`delegate.rs`, beside `obtain_canonicalizer_state` at line 123):

```rust
    /// The search graph's storage for a root goal. The delegate may keep it between root goals
    /// (on its worker); the default makes a fresh one.
    fn obtain_search_storage(&self) -> SearchStorage<Self::Interner> {
        Default::default()
    }

    /// Takes back what [`Self::obtain_search_storage`] gave, emptied.
    fn release_search_storage(&self, _storage: SearchStorage<Self::Interner>) {}
```

**`enter_root`** (`eval_ctxt/mod.rs:464-500`):
- `let mut search_graph = SearchGraph::new(root_depth);` becomes
  `let mut search_graph = SearchGraph::new_in(root_depth, delegate.obtain_search_storage());`.
- After `f(&mut ecx)` returns and `ecx` is dropped, add
  `delegate.release_search_storage(search_graph.into_storage());`.
- The proof-tree root (`evaluate_root_goal_for_proof_tree`) does the same.
- If `f` unwinds (salsa cancellation), the storage is dropped, not recycled.

**Nested goals:**
- `enter_root` and `enter_canonical` initialize `nested_goals: Default::default()`. Both become
  `nested_goals: search_graph.scratch().take_nested_goals()`.
- Where an `EvalCtxt` ends, the list goes back with
  `ecx.search_graph.scratch().give_nested_goals(mem::take(&mut ecx.nested_goals))`. There are two
  such places: the end of `enter_canonical`'s closure, and `probe.rs` after `nested.delegate.probe(..)`
  returns.
- `probe.rs:56`, before `let nested_goals = self.ecx.nested_goals.clone();`. After:

```rust
        let mut nested_goals = self.ecx.search_graph.scratch().take_nested_goals();
        // clone: a probe starts from its parent's goals; `GoalStalledOn` clones its lists.
        nested_goals.extend(self.ecx.nested_goals.iter().cloned());
```

  `enter_without_propagated_nested_goals` (`probe.rs:62`) takes `take_nested_goals()` in place
  of `Default::default()`.
- `evaluate_added_goals_step` (`eval_ctxt/mod.rs:1060`), before:
  `for (source, goal, stalled_on) in mem::take(&mut self.nested_goals) {`. After:

```rust
        let spare = self.search_graph.scratch().take_nested_goals();
        let mut goals = mem::replace(&mut self.nested_goals, spare);
        for (source, goal, stalled_on) in goals.drain(..) {
            // ... the loop body unchanged ...
        }
        self.search_graph.scratch().give_nested_goals(goals);
```

  The `?` inside the loop would return with `goals` dropped, not given back. That costs only
  capacity on the error path, but write it as `let result = (|| { .. })();` or keep a guard, so
  the warm path gives the list back.
  - The upstream comment ("replacing it has no effect on performance. We tried.") is about time
    on rustc's allocator. Here it is about the zero-allocation budget, and the patch says so in
    `PATCHES.md`.

**Candidates:**
- `assemble_and_evaluate_candidates` (`assembly/mod.rs:483`): `let mut candidates = vec![];`
  becomes `let mut candidates = self.search_graph.scratch().take_candidates();`.
- Every caller gives the list back once it has merged or picked:
  - `trait_goals.rs:1769` (`compute_trait_goal`);
  - `assembly/mod.rs:1320` and `1350` (`assemble_and_merge_candidates`).
- `merge_trait_candidates` (`trait_goals.rs:1678, 1693, 1733`): each
  `candidates.extract_if(.., pred).collect::<Vec<_>>()` becomes

```rust
        let mut picked = self.search_graph.scratch().take_candidates();
        picked.extend(candidates.extract_if(.., pred));
        // ... uses of the list unchanged ...
        self.search_graph.scratch().give_candidates(picked);
```

**Original values:**
- `canonicalize_goal` (`canonical/mod.rs:55`) returns `(ThinVec<I::GenericArg>, I::CanonicalInput)`.
  It becomes `(Vec<I::GenericArg>, I::CanonicalInput)` and takes the buffer:
  `canonicalize_goal(delegate, goal, opaque_types, typing_mode, orig_values: Vec<I::GenericArg>)`.
- `Canonicalizer::finalize` (`canonicalizer.rs:277`), before:
  `let variables = mem::take(&mut self.state.variables);`, returning `variables`. After, it
  takes `mut into: Vec<I::GenericArg>`:

```rust
        into.extend_from_slice(&self.state.variables);
        let var_kinds = self.delegate.cx().mk_canonical_var_kinds(&self.state.var_kinds);
        self.delegate.release_canonicalizer_state(self.state);
        (max_universe, into, var_kinds)
```

  - The state keeps its `variables` buffer (released and cleared).
  - The original values land in the pooled buffer.
  - The response canonicalizer (`canonicalize_response`) and `canonicalize_input` take a buffer
    the same way. A caller that keeps the values as a result (the proof tree's
    `CanonicalVarValues`) interns them from the slice, as now, then gives the buffer back.
- `evaluate_goal_raw` (`eval_ctxt/mod.rs:813, 852`) calls `canonicalize_goal(.., self.search_graph.scratch().take_args())`.
  The erased attempt's buffer goes back when it reruns.
- `build_stalled_on` (`:922`) takes `stalled_vars: &[I::GenericArg]` in place of the
  `ThinVec`, iterating `.iter().copied()`.
- After the stalled-on is built, `orig_values` goes back with `give_args`.
- Its result, `GoalStalledOn`, is unchanged. It is stored in the pending sets, so it is a
  result.

**Response instantiation:**
- `instantiate_and_apply_query_response` (`canonical/mod.rs:101`) and the
  `compute_query_response_instantiation_values` it calls get `scratch: &mut SolverScratch<I>`.
- At `:212`, `let mut opt_values = IndexVec::from_elem_n(None, response.var_kinds.len());`
  becomes

```rust
    let mut opt_values = scratch.take_opt_args();
    opt_values.resize(response.var_kinds.len(), None);
    let mut opt_values = IndexVec::<ty::BoundVar, _>::from_raw(opt_values);
```

  The `CanonicalVarValues` are built from it as now (`ArrayVec` for up to four, then `mk_args`).
  Then `scratch.give_opt_args(opt_values.raw)`.
- Callers pass `self.search_graph.scratch()`. `EvalCtxt` has `search_graph: &mut SearchGraph`
  and the call is not inside another borrow of it.

**Dedupe and universes:**
- `eval_ctxt/mod.rs:1707`: the response's region-constraint dedupe `HashSet` becomes
  `take_seen_outlives()` / `give_seen_outlives()`.
- `assembly/mod.rs:1400`: `FindParamInClause { universes: vec![], .. }` becomes
  `universes: self.search_graph.scratch().take_universes()`, given back after the visit.

**Budget tests:** see request 8 for the end-to-end budget tests. The patch's own unit tests are
under rustc_next_trait_solver's test module, against `SolverScratch` alone:
- take, grow, give, take again: capacity kept;
- `assert_at_most(0, ..)` for a take, push within capacity, give.

> forwarded to solver-sync and lane db (told by tmux 20:27). The blaze-alloc-count dev-dependency is told to mem-db.
> applied by db-mem-infer-a-solver-vendor (21:39): in `rustc_next_trait_solver` patch 0004 (`0004-solver-temporaries-recycled`); also pooled the region-variable set of `filter_irrelevant_region_constraints`; the option list is passed as `&mut Vec` into `instantiate_and_apply_query_response`; the response canonicalizer takes no buffer (it discarded its variables); proof-tree paths keep fresh empty `Vec`s.

## 4. solver-sync / lane db (`vendor/rustc_next_trait_solver` patch 0004 and `vendor/rustc_type_ir`): external constraints interned from a borrow

- **Why:**
  - A response with constraints builds `ExternalConstraintsData { region_constraints: Vec, opaque_types: Vec, normalization_nested_goals: NestedNormalizationGoals(Vec) }`
    (`eval_ctxt/mod.rs:1668, 1775`) and passes it by value to `Interner::mk_external_constraints`.
  - In a warm re-solve the interner already has it (a hit), so the `Vec`s are allocated only to
    be dropped: up to three allocations per such response.
- **What:**
  - `rustc_type_ir::Interner` gets a provided item:

```rust
    /// Interns external constraints given by reference: an implementation that finds them
    /// interned already copies nothing. The default clones them into the owned form.
    fn mk_external_constraints_ref(
        self,
        data: &ExternalConstraintsData<Self>,
    ) -> Self::ExternalConstraints {
        self.mk_external_constraints(data.clone())
    }
```

  - The response builder fills the three lists into pooled scratch (`SolverScratch` gets
    `region_constraints`, `opaque_types` and `normalization_goals` pools, each the same `pool!`).
  - It builds an `ExternalConstraintsData` around them, calls `mk_external_constraints_ref`, then
    takes the lists back out (`mem::take` of each field) and gives them back.
  - hir-ty's impl (`interner.rs:1154`) looks up the borrowed form in `EXTERNAL_CONSTRAINTS`
    (`opaques.rs:39`, a `Values` table) and clones only on a miss. `Values::intern` takes a
    `V` by value now; it gets `intern_ref<V: Clone>(&self, value: &V)`, which hashes and
    compares the borrow, and clones into the column only when the value is new.
- **Budget test:** request 8's warm steady state, with a body whose goals have region
  constraints (a `T: 'a` bound).

> forwarded to solver-sync and lane db (told by tmux 20:27).
> applied by db-mem-infer-a-solver-vendor (21:39): the vendor half (`Interner::mk_external_constraints_ref`, buffers given back) in patches 0006 and 0004; added provided `SolverDelegate::make_deduplicated_region_constraints_into` and `InferCtxtLike::clone_opaque_types_added_since_into` (the request said `InferCtxtLike` doesn't change) so the pooled lists can be filled; hir-ty's overrides are db-mem-infer-a-solver's.

## 5. solver-sync-infer-core (`crates/hir-ty/src/next_solver/infer/mod.rs`, `at.rs`) and solver-sync-glue's files (`solver.rs`): the `InferCtxt`'s tables recycled per worker

- **Who:**
  - `infer/mod.rs` and `infer/at.rs` belong to solver-sync-infer-core (in progress).
  - `solver.rs` belongs to db-ty-port-solver (DONE) under lane db.
  - The new module `next_solver/recycle.rs` is lane db's to place, because `next_solver.rs`
    declares it.
- **Why:**
  - `InferCtxtBuilder::build` (`infer/mod.rs:402`) makes `InferCtxtInner::new()`: empty
    `UnionFind`s, storages and `Vec`s.
  - The first variable of each kind grows its four columns (`parent`, `rank`, `value`, `undo`):
    type equality and sub-unification, const, int, float.
  - The first region grows the region-constraint columns (`tag`, `sub`, `sup`, `regions`,
    `var_infos`). The first opaque grows the opaque map.
  - The canonicalizer state is a field of the `InferCtxt` (`:296`). Each new `InferCtxt` starts
    it empty, so each goal regrows its `ThinVec`, `Vec` and maps.
  - `SolverDelegate::build_with_canonical` (`solver.rs:158`) builds one `InferCtxt` per
    canonical goal computed, and every one of these allocations is made again for every goal:
    roughly 10–25 per goal on a miss.
- **The fix:** an `InferCtxt` takes its tables from the worker's shelf when built and puts them
  back, emptied, when dropped.

`crates/hir-ty/src/next_solver/recycle.rs`, new:

```rust
//! What the solver's inference contexts and search graphs allocate, kept per worker between
//! them (docs/v2/plan/lanes/mem/db-solver.md): each is emptied, its capacity kept, when its
//! context ends, and the next one built on the worker takes it, so a warm worker builds
//! contexts without allocating.

use std::mem;

use blaze_mem::{Tag, scratch::Shelf, tag};
use rustc_next_trait_solver::solve::SearchStorage;
use rustc_type_ir::CanonicalizerState;

use crate::next_solver::{DbInterner, infer::InferTables};

/// Kept up to the deepest nesting of canonical goals (each nested goal's context is alive
/// while it solves), and of bodies inferred inside another's goals.
const INFER_DEPTH: usize = 32;
const SEARCH_DEPTH: usize = 8;

/// A context's tables bigger than this are freed, not kept: a huge body's root context would
/// pin its high water on every worker. Such a body pays its tables' few allocations per run,
/// against at least this much work.
const KEEP_AT_MOST: usize = 4 << 20;

/// `T<'db>` with `'db` replaced by `'static`, for a value that holds no `'db` data.
///
/// # Safety
///
/// `T<'static>` must be `T<'db>` with only the lifetime changed (so the same layout), and the
/// value must hold no value of a type naming `'db` (every container in it is empty).
unsafe fn erase_empty<A, B>(value: A) -> B {
    const { assert!(size_of::<A>() == size_of::<B>() && align_of::<A>() == align_of::<B>()) };
    let value = mem::ManuallyDrop::new(value);
    // SAFETY: the caller's precondition: `B` is `A` with `'db` replaced by `'static`, and the
    // value holds no data of either lifetime, so no reference of `'db` outlives it.
    unsafe { mem::transmute_copy(&*value) }
}

thread_local! {
    static INFER: std::cell::RefCell<Shelf<InferTables<'static>, INFER_DEPTH>> =
        const { std::cell::RefCell::new(Shelf::new()) };
    static SEARCH: std::cell::RefCell<Shelf<SearchStorage<DbInterner<'static>>, SEARCH_DEPTH>> =
        const { std::cell::RefCell::new(Shelf::new()) };
}

/// Empty tables for a new inference context: the worker's, or new ones.
pub(crate) fn infer_tables<'db>() -> InferTables<'db> {
    match INFER.try_with(|shelf| shelf.borrow_mut().take()).ok().flatten() {
        Some(tables) => {
            tag::sub(Tag::Infer, tables.held());
            // SAFETY: shelved tables are emptied (`give_infer_tables`), and `InferTables<'db>`
            // is `InferTables<'static>` with only the lifetime changed.
            unsafe { erase_empty(tables) }
        }
        None => InferTables::default(),
    }
}

/// Puts an ended context's tables on the worker's shelf, emptied.
pub(crate) fn give_infer_tables(mut tables: InferTables<'_>) {
    tables.empty();
    let held = tables.held();
    if held > KEEP_AT_MOST {
        return;
    }
    // SAFETY: `empty` dropped every value, so the tables hold no `'db` data, and
    // `InferTables<'static>` is the same type up to the lifetime.
    let tables: InferTables<'static> = unsafe { erase_empty(tables) };
    tag::add(Tag::Infer, held);
    let _ = INFER.try_with(|shelf| shelf.borrow_mut().give(tables));
}

// `search_storage` and `give_search_storage`: the same over `SEARCH`, with
// `SearchStorage::held` and an `empty` that `into_storage` already did.
```

`Shelf<B, N>` and its counting:
- The shelf keeps its `N` largest buffers when one comes back to a full shelf. Its `give` and
  `take` are the private `Shelf` of `blaze_mem::scratch` (`scratch.rs:26-60`), made public with
  the depth a const parameter. That is request 11, to lane mem.
- `held` bytes are counted into `Tag::Infer` while shelved, so the tag shows the solver's resident
  scratch. `tag::add` happens on give, `tag::sub` on take, and `try_with` covers a thread that is
  ending.

**`infer/mod.rs`:**
- New: `pub(crate) struct InferTables<'db> { inner: InferCtxtInner<'db>, canonicalizer: CanonicalizerState<DbInterner<'db>>, placeholder_assumptions: FxIndexMap<..>, next_solver_overflows: Vec<NextSolverOverflow<'db>> }`.
- `InferTables` gets `Default`, `fn empty(&mut self)` and `fn held(&self) -> usize`.
- `InferCtxtInner` gets `fn empty(&mut self)`, which:
  - empties each storage with capacity kept and resets each counter to its `new()` value
    (`changes`, `floor`, `num_open_snapshots`);
  - sets `region_constraint_storage` to `Some(..)` again: if it was taken (regions resolved), a
    new default, otherwise its storage emptied.
- Each storage gets an `empty(&mut self)` over its fields:
  - `UnionFind` (`union_find.rs:50`): its four columns and `floor`/`changes`;
  - `TypeVariableStorage` (`type_variable.rs`): both union-finds and `params`;
  - `RegionConstraintStorage` (`region_constraints/mod.rs:54`): `var_infos`, both data sets,
    `solver_overwrites`, `lubs`, `glbs`, `combinations`, the column storage at `:173-176`, and
    the unification table;
  - `OpaqueTypeStorage` (`opaque_types/table.rs:11`): its map, `duplicate_entries`, `undo` and
    `changes`.
- An empty `InferCtxtInner` must equal `InferCtxtInner::new()` in every observable field. The
  test below holds it to that.
- `InferCtxt`'s fields `inner`, `placeholder_assumptions`, `next_solver_overflows` and
  `canonicalizer_state` stay as they are.

`InferCtxtBuilder::build` (`:402-420`), before as shown at those lines. After:

```rust
    pub fn build(&mut self, typing_mode: TypingMode<'db>) -> InferCtxt<'db> {
        // Solving needs the crate's trait environment and solver cache.
        self.interner.expect_crate();
        let InferCtxtBuilder { interner, enable_next_solver_overflow_fcw } = *self;
        let InferTables { inner, canonicalizer, placeholder_assumptions, next_solver_overflows } =
            recycle::infer_tables();
        InferCtxt {
            interner,
            typing_mode,
            inner: RefCell::new(inner),
            tainted_by_errors: Cell::new(None),
            universe: Cell::new(UniverseIndex::ROOT),
            placeholder_assumptions: RefCell::new(placeholder_assumptions),
            enable_next_solver_overflow_fcw: Cell::new(enable_next_solver_overflow_fcw),
            next_solver_overflows: RefCell::new(next_solver_overflows),
            obligation_inspector: Cell::new(None),
            canonicalizer_state: RefCell::new(canonicalizer),
        }
    }
```

New `Drop`:

```rust
impl Drop for InferCtxt<'_> {
    fn drop(&mut self) {
        // A context dropped by an unwind may be mid-change; its tables are freed, not kept.
        if std::thread::panicking() {
            return;
        }
        recycle::give_infer_tables(InferTables {
            inner: mem::replace(self.inner.get_mut(), InferCtxtInner::empty_unallocated()),
            canonicalizer: mem::take(self.canonicalizer_state.get_mut()),
            placeholder_assumptions: mem::take(self.placeholder_assumptions.get_mut()),
            next_solver_overflows: mem::take(self.next_solver_overflows.get_mut()),
        });
    }
}
```

- `empty_unallocated` is today's `InferCtxtInner::new()`, renamed. `new` goes away; its callers
  (`build`) use the shelf.
- `Drop` forbids moving fields out of an `InferCtxt`. Today none does: `rg` finds no
  `.inner.into_inner()` and no destructuring of `InferCtxt`.
- `SolverContext` is `#[repr(transparent)]` over `InferCtxt` and is only ever a reference cast,
  so it is unaffected.
- `next_solver_overflows` is read by the caller before the context drops (`take_next_solver_overflows`),
  so recycling it loses nothing.

**Forks** (`infer/at.rs:57-97`):
- `fork` and `fork_with_typing_mode` clone the tables: `inner: self.inner.clone()`.
- With recycling they take emptied tables from the shelf and copy into them with `clone_from`, so
  a warm fork allocates nothing:

```rust
    pub fn fork(&self) -> Self {
        let mut tables = recycle::infer_tables();
        // clone: a fork is a copy of the variable tables, into the worker's warm ones.
        tables.inner.clone_from(&self.inner.borrow());
        tables.placeholder_assumptions.clone_from(&self.placeholder_assumptions.borrow());
        tables.next_solver_overflows.clone_from(&self.next_solver_overflows.borrow());
        let InferTables { inner, canonicalizer, placeholder_assumptions, next_solver_overflows } =
            tables;
        Self {
            interner: self.interner,
            typing_mode: self.typing_mode,
            inner: RefCell::new(inner),
            // ... `tainted_by_errors`, `universe`, the fcw flag and the inspector as now ...
            placeholder_assumptions: RefCell::new(placeholder_assumptions),
            next_solver_overflows: RefCell::new(next_solver_overflows),
            canonicalizer_state: RefCell::new(canonicalizer),
        }
    }
```

  `fork_with_typing_mode` is the same with its `typing_mode`.
- `InferCtxtInner` and each storage get a `Clone` with a field-by-field `clone_from` that reuses
  capacity. `Vec::clone_from` and `HashMap::clone_from` reuse it, but a derived `clone_from`
  calls `clone`.

**`solver.rs:629-636`:** unchanged. `obtain_canonicalizer_state`/`release_canonicalizer_state`
keep taking and putting back the `InferCtxt`'s own state, which now comes warm from the shelf.

**`solver.rs`, new: the search storage:**

```rust
    fn obtain_search_storage(&self) -> SearchStorage<Self::Interner> {
        recycle::search_storage()
    }

    fn release_search_storage(&self, storage: SearchStorage<Self::Interner>) {
        recycle::give_search_storage(storage);
    }
```

**Budget tests** (`infer/mod.rs` tests; they need solver-sync-glue requests 6 and 10:
`blaze-alloc-count` installed and blaze-mem a dependency):
- `a_recycled_context_is_a_new_one`:
  - Build a context on `DbInterner::new_no_crate`'s test interner. Use it: new type, int and
    const variables, unify two, open and roll back a snapshot, register a region obligation.
  - Drop it and build another. Assert every table's observable state equals a never-used
    context's: `num_open_snapshots`, the variable counts, `state_changes()`, the opaque entries
    and the region-constraint data.
- `a_warm_context_allocates_nothing`:
  - Build and use a context as above, then drop it.
  - Inside `blaze_alloc_count::assert_at_most(0, "a warm inference context", ..)`: build one,
    make the same variables, unify, snapshot, roll back, drop.
- `a_warm_fork_allocates_nothing`: the same with `fork()` of a context holding 100 variables.
- `a_huge_context_is_not_kept`: a context grown past `KEEP_AT_MOST` comes back as fresh tables
  (`held() == 0` on the next take).

> forwarded to solver-sync for solver-sync-infer-core (its window is gone) and to lane db for `recycle.rs` and `solver.rs` (told by tmux 20:27).
> applied by db-mem-infer-a-solver (21:52): `recycle.rs` (shelves `INFER` 32, `PENDING` 32, `SEARCH` 8), `InferTables`, `build`/`Drop`/`fork` through the shelf, `solver.rs`'s two delegate methods, the four tests in `infer/mod.rs`. Differences: shelved bytes are counted by a `Kept<T>` wrapper (counted on give, uncounted on take or drop, so a full shelf or an ending thread keeps `Tag::Infer` right); `empty`/`held`/`clone_from` also cover `undo_log`, `const_var_params`, `region_obligations`, `region_assumptions`; nothing given back while panicking. Waits on `mod recycle;` in `next_solver.rs` (asked of db-mem-infer-a) and on `scratch::Shelf` public (section 11).

## 6. db-ty-port-solver / lane db (`crates/hir-ty/src/next_solver/fulfill.rs`): pending columns recycled

- **Why:**
  - `FulfillmentCtxt::new` (`fulfill.rs:133`) makes `Pending::default()`: five empty column
    `Vec`s.
  - The first registered obligation grows all five.
  - An `ObligationCtxt` (31 construction sites, most per coercion, method probe or check) is
    short-lived, so each one with an obligation pays five allocations.
- **What:**
  - `Pending` gets `fn held(&self) -> usize`. Its existing `clear` keeps capacity.
  - `recycle.rs` gets a third shelf, `PENDING: Shelf<Pending<'static>, 32>`, with
    `pending()`/`give_pending()` as for the tables. `Pending` becomes `pub(crate)` for it.
- `fulfill.rs:133-139`, before: `pending: Default::default(),`. After:
  `pending: recycle::pending(),`. New:

```rust
impl Drop for FulfillmentCtxt<'_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            recycle::give_pending(mem::take(&mut self.pending));
        }
    }
}
```

- **The SAFETY argument is the same as for the tables:**
  - `give_pending` calls `clear` (which drops every row's `StalledOn`, the only field with heap
    data) before `erase_empty`.
  - `rg` finds no move out of a `FulfillmentCtxt`'s fields.
- **Budget test** (`fulfill.rs` tests, new): register 50 obligations, `try_evaluate_obligations`,
  drop. Then in `assert_at_most(0, "a warm fulfillment context", ..)` make a new one and register
  50 that the fast path keeps.

> forwarded to lane db (window `db-next2`, told by tmux 20:27).
> applied by db-mem-infer-a-solver (21:52): `Pending` `pub(crate)` with `held` (and a test-only `reserve_for_tests`), `new` takes `recycle::pending()`, `Drop` gives back. Difference: the budget test is `a_warm_fulfillment_context_allocates_nothing` in `next_solver/budget_tests.rs`, not `fulfill.rs`, as its obligations need the fixture's items.

## 7. solver-sync-infer-relate (`crates/hir-ty/src/next_solver/infer/canonical/instantiate.rs`): the instantiation cache that doesn't allocate

- **Why:**
  - `CanonicalInstantiator` (`instantiate.rs:132-141`) caches folded types in an
    `FxHashMap<Ty, Ty>`.
  - It inserts on the first type with canonical bound variables that isn't itself a bound
    variable (`Vec<^0>`), so it allocates once per instantiation of such a value. That includes
    instantiating a cached response, which is the hit path.
  - rustc's instantiator and rustc_type_ir's `BoundVarReplacer` (`fold.rs:558`) use a
    `DelayedMap`, which caches only after 32 visits, so small values never allocate.
- Before (line 140): `cache: FxHashMap<Ty<'db>, Ty<'db>>,`. After:
  `cache: DelayedMap<Ty<'db>, Ty<'db>>,` with `use rustc_type_ir::data_structures::DelayedMap;`.
- In `fold_ty`, before:

```rust
                } else if let Some(&t) = self.cache.get(&t) {
                    t
                } else {
                    let res = t.super_fold_with(self);
                    assert!(self.cache.insert(t, res).is_none());
                    res
                }
```

  after:

```rust
                } else if let Some(&t) = self.cache.get(&t) {
                    t
                } else {
                    let res = t.super_fold_with(self);
                    assert!(self.cache.insert(t, res));
                    res
                }
```

  `DelayedMap::insert` returns whether it is new. The construction site (`:95`) uses
  `cache: Default::default()` either way.
- **Budget test** (`instantiate.rs` tests, new):
  `blaze_alloc_count::assert_at_most(0, "instantiating a small canonical value", ..)` over
  `instantiate` of a canonical `(Vec<^0>, Option<^1>)`, as an ADT pair with two bound type
  variables, with two var values.

> forwarded to solver-sync for solver-sync-infer-relate (its window is gone; told by tmux 20:27).
> applied by db-mem-infer-a-solver (21:27): as written; `FxHashMap` import dropped. Test value is `([^0], *const ^1)` (builtin types on `Db::for_tests`, no fixture), not an ADT pair: same path (cache inserts below bound vars).

## 8. lane db (`crates/hir-ty/src/next_solver/`, a new test module; `crates/hir-ty/Cargo.toml` dev-dependencies): the three budget tests the contract asks for

- **Who:** lane db, because `next_solver.rs` declares the module and `Cargo.toml` is
  db-next2's. They need:
  - solver-sync-glue requests 6 (`blaze-alloc-count`) and 10 (blaze-mem);
  - a dev-dependency `test-fixture.workspace = true`. base-db's test `Db` and test-fixture don't
    depend on hir-ty (checked), so there's no cycle. mem-db is told about the dependency.
- **Fixture** (one crate, `minicore: sized, copy, clone`):

```rust
trait Tr {}
struct S;
struct Wrap<T>(T);
impl<T: Clone> Tr for Wrap<T> {}
fn body<'a, T: Clone + 'a>(x: &'a T) { let _ = x; }
```

- **The helper** `solve(db, krate, goal_of: impl Fn(&InferCtxt) -> Goal, times: usize)`:
  - builds `DbInterner::new_with(db, krate)` and its `InferCtxt`, and evaluates
    `<&SolverContext>::from(&infcx).evaluate_root_goal(goal, Span::Dummy, None)` `times` times;
  - runs outside any salsa query. There `capture_reads` gives `None`, so the goal's scope is the
    interner's own query cache (`tls_cache::of_query`, keyed by `DbInterner::solved`).
- **`a_goal_from_the_global_cache_allocates_nothing`:**
  - Goal: `Wrap<?0>: Tr` with `?0` a fresh type variable, then `Wrap<S>: Tr`. This is a goal with
    an inference variable, so the hit path canonicalizes, instantiates the response and applies
    it.
  - Solve it once (a miss), then
    `assert_at_most(0, "a goal from the global cache", || { for _ in 0..100 { solve_once } })`
    on the same interner and context.
  - It fails today with at least 3 allocations per hit: `orig_values`, the canonicalizer's lost
    `variables`, `opt_values`. Requests 3 and 7 make it 0.
- **`a_cached_canonical_response_allocates_nothing`:**
  - The same goal, solved from a new `InferCtxt` each time on the same interner, so a fresh
    context's variables meet the cached canonical response.
  - The first context is built and dropped before measuring, which warms the shelf.
  - `assert_at_most(0, "a cached canonical response", ..)` over: build a context, make `?0`,
    solve, drop.
  - Needs requests 3, 5 and 7.
- **`resolving_an_unchanged_bodys_goals_warm_allocates_nothing`:**
  - Collect the goals of `body` in the fixture: its where-clauses as obligations (`T: Clone`,
    `T: 'a`) plus `Wrap<T>: Tr` and `&'a T: Copy`.
  - Solve them all with a fresh `DbInterner` (new `Solved`, so a fresh query cache: every goal
    a miss), twice: the first run warms the shelves, the pools, the interner tables and the
    tls generation.
  - Then `assert_at_most(0, "re-solving an unchanged body's goals", ..)` over a third run with a
    fresh interner. This is the warm steady state: every goal computed, on recycled storage, with
    every type, predicate and canonical input interned already.
  - Needs requests 2–5 here, plus mem-db's request 4 and solver-sync-glue's 8 and 9 (a recycled
    generation, `GlobalCache::clear` that keeps chunks).
- **Within salsa:** a goal solved inside a query `replay_reads` its reads into the active query's
  edge list, which is salsa's buffer (mem-db-salsa). The tests above run outside any query, so
  they measure the solver alone. The query-level warm test is mem-db's (its request 6, a
  revalidation in a new view).

> forwarded to lane db (window `db-next2`, told by tmux 20:27); the test-fixture dev-dependency is told to mem-db.
> applied by db-mem-infer-a-solver (21:52): `next_solver/budget_tests.rs` with the fixture, the three tests, section 10's block-impl variant and section 6's fulfillment test. Waits on `#[cfg(test)] mod budget_tests;` in `next_solver.rs` (asked of db-mem-infer-a); the block-impl test passes only once section 10 lands.
> applied by db-mem-infer-a (21:58): `mod recycle;` (section 5) and `#[cfg(test)] mod budget_tests;` in `next_solver.rs` (solver-sync's window is closed; lane db owns the file while db is not done).

## 9. solver-sync (`crates/hir-ty/src/next_solver/interner.rs`, `mod table`): interned storage on regions tagged `Intern`

**What is done:**
- Every interned kind is already in an append-only, never-moved, lock-free-read table of
  hir-ty's own (`interner.rs:2974-3905`): types, regions, consts, predicates, patterns,
  canonical inputs, valtrees, allocations, external constraints, clause lists, and every
  `interned_slice!` list.
- The table kinds are `Rows<K>` (columns `tag`, `flags`, `binder`, `kinds` and an `Index`),
  `Lists<T>` (columns `start`, `len`, `flags`, `binder` and a `Run` of items) and `Values<T>`
  (columns `marks`, `values`).
- A handle is a `u32` row, so the contract's shape (interned data in columns, a `u32` handle) is
  already met.

**What is left:**
- Each `Column<T>` (`:3110`) is 23 segments from `std::alloc::alloc`/`alloc_zeroed` (`:3145`).
  These are untagged, so the interner's bytes appear under no `Tag`, and a read is segment
  arithmetic (`locate`, a `leading_zeros`, two loads).
- `Run` must place a list within one segment (`fit`, `:3287`), so the end of a segment is
  skipped.
- Each `Index` shard's slot table grows by copying into a new `alloc_zeroed` table and leaking
  the old one (`Slots::allocate`, `:3353`), also untagged.

**Why not `blaze_mem::Column`:**
- Its push claims an index with its own `fetch_add` and publishes with its own ready bit.
- A `Rows` writes four columns at one row it claimed once, and publishes the row through the
  index. A `Lists` claims runs of items.
- So the tables need a reserved `Region` per column, not a `blaze_mem::Column`. The claiming and
  publishing stay the tables' own.

**What:**
- `Column<T>` becomes a lazily reserved `blaze_mem::Region` tagged `Tag::Intern`:

```rust
    /// Values by position in one reserved region, committed as positions are written and never
    /// moved or freed: a reference into it lives as long as the column (a `static`). Each
    /// position is written once, by the thread that claimed it, before anything naming it is
    /// published by a release store that the reader's acquire load pairs with.
    struct Column<T> {
        /// The region's base once reserved; null before the first write.
        base: AtomicPtr<T>,
        region: OnceLock<Region>,
        /// Positions it can hold: the reservation, as `Region::reserve` is not `const`.
        max: u64,
        of: PhantomData<T>,
    }

    impl<T> Column<T> {
        const fn new(max: u64) -> Column<T> {
            Column { base: AtomicPtr::new(ptr::null_mut()), region: OnceLock::new(), max, of: PhantomData }
        }

        #[cold]
        fn reserve(&self) -> *mut T {
            let region = self.region.get_or_init(|| {
                Region::reserve_huge(self.max as usize * size_of::<T>(), Tag::Intern)
                    .unwrap_or_else(|error| panic!("reserving an interner column: {error}"))
            });
            let base = region.base().as_ptr().cast::<T>();
            self.base.store(base, Ordering::Release);
            base
        }

        /// The region, committed through `at`, and its base.
        #[inline]
        fn reach(&self, at: u64) -> *mut T {
            let mut base = self.base.load(Ordering::Acquire);
            if base.is_null() {
                base = self.reserve();
            }
            let region = self.region.get().expect("reserved above");
            region
                .ensure((at as usize + 1) * size_of::<T>())
                .unwrap_or_else(|error| panic!("growing an interner column: {error}"));
            base
        }
    }
```

  - `write(at, value)`: `self.reach(at).wrapping_add(at as usize).write(value)`.
  - `place(at)` (zeroed columns): `&*self.reach(at).wrapping_add(at as usize)`. The kernel zeroes
    committed pages, so `Column::zeroed` and its `alloc_zeroed` go away.
  - `peek(at)`: `None` unless `base` is set and
    `(at + 1) * size_of::<T>() <= region.committed()`.
  - `get` and `slice`: `base.load(Relaxed).wrapping_add(at)`. A written position's commit and
    base store happen before the publication that the caller's acquire load pairs with.
- Each `unsafe` keeps a `// SAFETY:` comment saying what is committed and who wrote it.
- `Run::claim` drops `fit`: a run is contiguous in one region. The 2^32-position limit stays as
  `max`.
- `segment_start`, `segment_len`, `locate`, `SEGMENTS` and `FIRST_BITS` go away.
- `max` per column comes from the table: `Rows::new(what, last)` gives `last + 1` rows to its
  row columns; a `Lists` items run gives `1 << 32`; `Values` gives `LAST_ROW + 1`.
- **Address space:** reserved lazily, per column actually written.
  - The largest is `TYS`: `(2^30) × size_of::<TyKind>()`, about 32 GiB virtual.
  - A list items column is `2^32 × 8` = 32 GiB virtual.
  - With about 40 written columns, the total is under 1 TiB virtual, nothing committed until
    written. That fits a 47-bit (128 TiB) user address space.
  - Windows reserves address space without charging commit, and Linux `PROT_NONE` reservations
    are not counted by overcommit (`blaze_mem::os::reserve`).
- **The `Index` slot tables:**
  - The tables a shard outgrows can't be freed: a reader may be probing them. So all of an
    index's tables come from one region per `Index`, tagged `Intern`, bump-allocated under the
    growing shard's `len` lock, which `Vacant::insert` already holds.
  - Doubling means a shard's tables sum to under twice its last. With 64 shards at most half
    full, reserving `4 × max_rows × 8` bytes covers them with room for imbalance between
    shards.
  - `Slots::allocate` takes `&Region` and an `AtomicUsize` cursor instead of calling
    `alloc_zeroed`. Alignment is `SLOTS_ALIGN` (64) by rounding the cursor; committed memory is
    zeroed.
- **Budget and accounting tests** (`mod table` tests, beside
  `each_table_gives_the_rows_and_values_of_its_safe_reference`):
  - `interning_is_counted_under_intern`: `blaze_mem::tag::snapshot().get(Tag::Intern)` grows by
    at least the bytes of 10,000 new rows. A hit (interning the same 10,000 again) grows nothing.
  - `a_hit_allocates_nothing`: `blaze_alloc_count::assert_at_most(0, "interning a value already in", ..)`
    over interning 10,000 existing rows, lists and values. That holds now and must keep holding.
  - `a_miss_allocates_nothing`: the same over 10,000 new rows. Commits are system calls, not
    allocator calls. Today a segment's first write and a shard's growth allocate.
  - The existing safe-reference tests, the parallel test (8 threads) and the
    `a_shard_that_grows_keeps_every_row` test stay. `positions_are_in_the_segment_that_holds_them`
    and `a_run_never_crosses_a_segment` go with the segments.

> forwarded to solver-sync (told by tmux 20:27).
> not applied by db-mem-infer-a-solver (21:52): `interner.rs` is solver-sync's; appended as written to `v2/requests/solver-sync-inbox.md` ("From db-mem-infer-a-solver"). solver-sync's window is closed (STATUS: DONE), so db-mem-infer-a was told instead.

> not applied by db-solver (2026-10-29 23:45): still segmented (`alloc_zeroed` at `interner.rs` ~3408 and ~3616). This rewrites unsafe storage that every interned handle reads, so it waits on a lane that can build and run `mod table`'s tests and the budget tests with it; this pass runs nothing. `interner.rs` is db-solver's now.

## 10. solver-sync (`crates/hir-ty/src/next_solver/interner.rs`): the impl walks' `seen` lists inline

- **Why:**
  - `for_each_relevant_impl` (`:1906`) and `for_each_blanket_impl` (`:2054`) run during candidate
    assembly, once per trait goal computed.
  - Each makes `let mut seen = Vec::new();` and pushes every impl of a block (`const _` and fn
    bodies), so a trait or type defined in a block allocates per goal.
  - A block holds few impls.
- **Before:** `let mut seen = Vec::new();` (both). **After:**
  `let mut seen: SmallVec<[ImplId; 8]> = SmallVec::new();`, or `[BlanketImplId; 8]` for
  `for_each_blanket_impl`, whichever the pushed type is. hir-ty already depends on smallvec;
  `contains` is unchanged.
- **Budget test:** request 8's warm test with the fixture's `impl<T: Clone> Tr for Wrap<T>`
  moved into a `const _: () = { .. };` block.

The other 29 hot-lint findings in `next_solver/` (`opaques.rs`, `persist.rs`, `ty.rs`,
`ir_print.rs`, `util.rs`, `predicate.rs`, `valtree.rs`, and the rest of `interner.rs`) are on
paths a goal hit or a warm re-solve doesn't take. They cover:
- `const_conditions`, `opaque_types_defined_by` and `explicit_implied_const_bounds`, which are
  salsa-memoized per item;
- persistence;
- debug printing;
- reflection;
- `get_tracked`'s `.clone()` of a `Copy`-sized result.

They stay with their owners' annotations.

> forwarded to solver-sync (told by tmux 20:27).
> not applied by db-mem-infer-a-solver (21:52): `interner.rs` is solver-sync's; appended as written to `v2/requests/solver-sync-inbox.md` ("From db-mem-infer-a-solver"). Its budget test is in `next_solver/budget_tests.rs` (`resolving_goals_with_an_impl_in_a_block_warm_allocates_nothing`).

> applied by db-solver (2026-10-29 23:45): `for_each_relevant_impl`'s `seen` is `SmallVec<[AnyImplId; 8]>` and `for_each_blanket_impl`'s `SmallVec<[hir_def::ImplId; 8]>`. Budget test `resolving_goals_with_an_impl_in_a_block_warm_allocates_nothing` (`next_solver/budget_tests.rs`) not run.

## 11. lane mem (`crates/blaze-mem/src/scratch.rs`, through mem-db): `Shelf<B, N>` public

- **Why:**
  - Request 5's shelves keep a hir-ty type: `InferTables<'static>`, `SearchStorage<..>`,
    `Pending<'static>`.
  - `scratch::Shelved` is public, but its implementation needs a thread-local `Shelf`, and
    `Shelf` is private (`scratch.rs:26`).
  - `DEPTH` is 4. Canonical goals nest deeper: each nested goal's `InferCtxt` is alive while it
    solves, commonly 3–10 deep, up to the recursion limit. With depth 4, the fifth-deepest
    context and below would allocate every time.
- **What:**
  - `pub struct Shelf<B: Shelved, const N: usize = DEPTH>`.
  - `pub const fn new() -> Self`, with every slot `B::default()`. That needs `B: Default` to be
    const-callable. Otherwise use `[const { None }; N]` with `Option<B>` slots, which is
    simpler.
  - `pub fn take(&mut self) -> Option<B>` and `pub fn give(&mut self, buffer: B)`, the existing
    bodies with `DEPTH` replaced by `N`.
  - The existing shelves use `Shelf<B>` (the default `N`), so nothing else changes.
- **Test:** the existing `scratch` tests, plus `a_shelf_keeps_its_n_largest` at `N = 8`.

> to mem-db by tmux: lane mem's, as are its other blaze-mem requests.
> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-mem-infer-a-solver (21:52): filed with lane mem as `v2/requests/mem-db.md` section 12, forwarded to mem-next1; not landed yet (`Shelf` is still private in `scratch.rs`). `recycle.rs` uses `Shelf<B, N>` with `const fn new`, `take`, `give` as written here.

> done by mem (2026-10-29): `scratch::Shelf<B, N>` is public, as mem-db.md section 12 says.

