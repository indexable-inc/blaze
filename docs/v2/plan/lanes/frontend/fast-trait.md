---
status: plan
summary: frontend-fast-trait — trait goals answered before canonicalization (a per-revision global-goal table keyed by packed IDs), a shape table for Copy, Clone and auto traits over a per-ADT verdict, and the persistent goal cache by content; the bound, today's cost, the design, and why each keeps rustc's answers.
date: 2026-10-29
---
# frontend-fast-trait: trait goals

Section 2 of [fast.md](fast.md). Code: `crates/hir-ty/src/next_solver/global_goals.rs` and
`global_goals/` (the global-goal table, the shape table), `crates/hir-ty/src/next_solver/goal_store.rs`
and `goal_store/` (the persistent cache, child `frontend-fast-trait-store`), cases in
`harness/v2/frontend/cases/trait/` (child `frontend-fast-trait-cases`). The hooks in files of lane
db (`solver.rs`, `fulfill.rs`, `interner.rs`, `interner/tls_cache.rs`, `next_solver.rs`,
`infer/mod.rs`) and in the vendored solver (`rustc_type_ir`, `rustc_next_trait_solver`, lane db's
solver-sync series) are requests in `v2/requests/db-ty.md` under `## frontend-fast-trait: ...`.

Nothing here was run. Costs are counted from the code with the machine of
[../db/speed-of-light.md](../db/speed-of-light.md): about 3.5 GHz, L1 in 4 to 5 cycles, L2 in 14,
L3 in 50, branchy code at 2 instructions a cycle.

## The work and its bound

A body's inference asks a few hundred goals. Most are `Sized`, `Copy`, `Clone`, auto traits and
impl lookups whose self type has no inference variables, and the same goals recur across bodies
(`T: Sized`, `String: Display`, `Vec<u8>: Clone`).

- **A goal answered already in this revision** must still be found and its reads recorded on the
  asking query. Finding it: the goal's identity is already two hash-consed rows (the predicate,
  the param-env's clause list) and the typing mode's tag, so the key is a register op: two row
  flag loads (whether the goal is global, both rows hot in L1/L2), a shift and an or. One probe
  of an open-addressed table: a multiply and shift to the slot (4 cycles), one line holding the
  key and the answer (4 to 14 cycles). **Bound: about 20 cycles, 6 ns**, plus recording the reads.
  Recording the reads is salsa's `replay_reads`: a capture's edges are appended to the asking
  query, about 5 cycles an edge. A goal reads tens of edges, so replay is the larger part of a
  hit; its own bound (one anonymous node, as rustc's `read_index` of a dep node) is salsa's, not
  this file's (lane db, [../db/speed-salsa.md](../db/speed-salsa.md)).
- **A `Copy`/`Clone`/auto-trait goal on a type whose structure decides it** costs a `match` on
  the type's kind tag and, for an ADT, one memoized verdict: a salsa memo hit, about 19 cycles
  ([../db/speed-of-light.md](../db/speed-of-light.md), "Memo hit"). **Bound: about 25 cycles,
  7 ns.**
- **A goal answered in an earlier run or another crate's build** costs one store probe and the
  check that every fact it read still has its hash: a few fact lookups (each a memo hit) and one
  read of a mapped record. **Bound: about 1 µs**, against the microseconds to milliseconds of a
  solve.

## What the code does today (2026-10-29)

Counted from `fulfill.rs`, `solver.rs`, `interner/tls_cache.rs` and the vendored solver.

- `FulfillmentCtxt::register_predicate_obligation` tries `compute_goal_fast_path`
  (`solver.rs`: trivially sized types, trivially pure `Copy`/`Clone`, a few region and WF
  shapes) and otherwise pushes the obligation.
- Each round, `try_evaluate_obligations` calls `evaluate_root_goal` for every pending
  obligation not stalled. That builds a root `EvalCtxt` and a `SearchGraph`
  (`EvalCtxt::enter_root`), canonicalizes the goal (a fold over the predicate and the param
  env with the canonicalizer's state reused, then interning the canonical input: about 300 to
  600 cycles for a goal of five to ten type nodes), probes the search graph's `GlobalCache`
  through `tls_cache::shared` (the scope lookup, about 45 cycles, then FxHash of the canonical
  input and a probe, about 30), replays the reads, and instantiates the canonical response back
  into the inference context (about 200 cycles). **A hit costs about 600 to 1,000 cycles,
  30 to 50 times the bound.**
- Within a goal, each nested goal goes the same way: `add_goal` tries the delegate's fast path,
  then canonicalizes and probes the search graph.
- `Send`, `Sync`, `Unpin`, `Freeze` and `Copy`/`Clone` of an ADT have no fast path: each is a
  full evaluation of the auto-trait candidate (constituent types, one nested goal each) or of
  the impl candidate.
- Nothing outlives the revision: the next run, or the next crate in a fresh process, solves
  `String: Display` again.

## The design

### 1. The global-goal table, before canonicalization (`global_goals.rs`, `global_goals/table.rs`)

**Which goals.** A goal is *global* when its predicate and param env have no inference
variables (`HAS_INFER`), no placeholders (`HAS_PLACEHOLDER`), no bound variables escaping, no
free local regions (`HAS_FREE_LOCAL_REGIONS`: region parameters, region inference variables
and placeholders; `'static` and erased regions are allowed) and no error (`HAS_ERROR`).
Parameters are allowed: `T: Sized` under a param env is a goal like any other, and its
canonical form keeps `T`. The test is on the goal resolved through the unification table
(`deeply_resolve_via_unification_table`, a fold only when the goal has inference variables),
the resolution the solver's `evaluate_goal_raw` makes first and whose result is the
evaluation's goal: a goal whose variables are all unified with global types, as a method
probe's impl where clauses are once its receiver is (frontend-fast-method's section 7), is
the global goal it stands for, and a hit returns the resolved goal as the solver would. Then
two flag tests.

**The key** is one `u64`:

| bits | what |
|---|---|
| 0..30 | the predicate's row (`Predicate::row`, at most `table::LAST_ROW` = 2^30 - 1) |
| 30..60 | the param env's clause list (`Clauses::index`; a list past 2^30 has no key) |
| 60..63 | the typing mode's tag (0 to 6, below) |
| 63 | 0 in a key; set in a slot being written |

**Which typing modes.** The key stands for the canonical input `(goal, param env, typing mode,
predefined opaques)` only when that input is a function of the key. So:

- `Coherence`, `Reflection`, `PostAnalysis`, `Codegen`: no data, keyed by tag.
- `Typeck`, `PostTypeckThroughBorrowck`, `PostBorrowck`: keyed only when their list of opaque
  types (and coroutines) is empty. A non-empty list is the defining scope of a body, and a goal
  may reach an opaque in it through a field type or a projection even when the goal names none,
  so the answer depends on the list; those goals keep the canonical path.
- `ErasedNotCoherence`: never keyed. Its answers may be refused and rerun in the non-erased
  mode (`RerunNonErased`), so the answer is not a function of the goal alone.
- And in every mode, only when the inference context's opaque-type storage is empty
  (`opaque_types_storage_num_entries` is zero): otherwise the canonical input carries the
  predefined opaques, which the key does not.

So in every keyed case, key equal means canonical input equal (regions: only `'static` and
erased, which the canonicalizer maps the same way every time), and the vendored solver's answer
to a canonical input in a revision is the answer the table holds. rustc's old solver kept global
evaluation and selection caches keyed by non-canonical global predicates on the same argument.

**Which answers.** Only root goals are inserted, and only when:

1. `salsa::capture_reads` around the root evaluation gave `Some`: the evaluation read no
   provisional value of a salsa cycle, made no tracked struct, read no query-local cache
   (`unshareable_reads`) and ran under a query. These are exactly the shared search-graph
   cache's conditions (`interner.rs`, `cache_scope`).
2. The answer is `Ok` with `Certainty::Yes` and `HasChanged::No`, or `Err(NoSolution)`.
   Ambiguity is never inserted: its `MaybeInfo` and stall data are what the caller acts on.
3. The evaluation left the inference context as it found it: the same `state_changes`, region
   variables, region constraints (both the legacy and the solver's data), registered region
   obligations, and overflow warnings (rustc's `recursion_depth_exceeding_limit` rerun, which
   pushes one). Then a hit, which changes nothing, is the evaluation exactly.

A root result is final: the root's search graph is empty when it returns, so no answer in the
table is a cycle head's provisional one. The table is probed only for root goals (fulfillment
rounds and registration), never for nested ones: a nested goal inside a cycle must see the
search graph's provisional answers, which only the search graph's own cache (with its
`NestedGoals` applicability check) knows how to keep. Nested goals keep the canonical path, with
the shape table in front of it.

**A hit** replays the stored `Reads` into the asking query (`salsa::replay_reads`), exactly as
`tls_cache.rs`'s shared cache does through `get_tracked`, then returns the answer. It allocates
nothing.

**Where it lives.** One table per scope and revision, like the shared search-graph cache: the
shared generation in `tls_cache.rs` holds both (request: `SharedCaches { search, goals }`), so it
is per database, crate and `regions` flag, replaced on the first use in a later revision, and
freed through the same limbo after its grace.

**The table** (`global_goals/table.rs`): open addressing with linear probing over 64-byte slots
(the key word, the answer, the 32-byte `Reads`), one slot to a cache line. It starts empty and
allocates its first level (2^12 slots, 256 KB, zeroed pages) on the first insert, so making a
generation allocates nothing for it. A full level (half its slots used) is never rehashed: a new
level four times larger is added (docs/v2/plan/design/concurrency.md: "A full table grows by
adding a level"), and inserts go to the newest. Up to five levels (about 700,000 goals at half load); past
that inserts stop, which costs solves, never an answer. Lookups probe levels oldest first, since
the goals asked first are asked most. A reader does plain acquire loads: no lock and no write on
the read path. A writer claims an empty slot with a compare-and-swap to the key with bit 63 set,
writes the answer and reads, then publishes the key with a release store; a reader that finds the
pending key stops with a miss. Two writers of one key: the second sees the first's key and
returns.

### 2. The shape table (`global_goals/shape.rs`)

In `compute_goal_fast_path`, where it is asked for answers that cost depth ("Depth" below), for
a positive trait goal whose self type has no inference
variables, placeholders, regions (not even `'static`), parameters, errors or escaping bound
variables, outside `Coherence` and `Reflection` (which assembles no built-in candidate):

- **`Sized`, `MetaSized`**: rustc's own fast path (`sizedness_fast_path`, `has_trivial_sizedness`
  with the per-ADT `sizedness_constraint`) already is the shape table for sizedness; kept as is.
- **`Copy`, `Clone`**: rustc's `is_trivially_pure_clone_copy` for scalars, shared references,
  raw pointers, `!`, and tuples and arrays of those, kept; extended to a non-generic ADT whose
  verdict holds: exactly one relevant impl of the trait for it, positive, not `default`, with no
  where clauses (a `#[derive(Copy, Clone)]` or a plain manual impl). Its fields are not looked
  at: an impl candidate's nested goals are its where clauses only.
- **Auto traits** (`trait_is_auto`: `Send`, `Sync`, `Unpin`, `Freeze`, `UnsafeUnpin` and user
  auto traits alike; `Send` is not a hir-def lang item, so no fixed list): a walk that is rustc's
  auto-trait rule on a closed fragment, answering only "holds":
  - at every node, the relevant impls (`for_each_relevant_impl`, the solver's own lookup) come
    first. Any impl disqualifies the structural candidate, as
    `disqualify_auto_trait_candidate_due_to_possible_impl` does for every rigid type. Exactly one
    positive, non-`default` impl whose self type is the node's head over distinct parameters
    (`&'a T`, `*const T`, `Unique<T>`), whose where clauses are each an auto trait on a parameter
    (`T: Sync`) or a sizedness bound the sizedness fast path proves, is followed into those
    clauses. A negative impl, two impls, or any other clause ends the walk with no answer;
  - with no impl: rustc's `instantiate_constituent_tys_for_auto_trait` for scalars, `!`, fn
    definitions and pointers (none), `str` (`[u8]`), pointers, references, arrays, slices,
    patterns (the element), tuples (the elements), `PhantomData<T>` (`T`) and other ADTs (every
    field type, instantiated). Unsafe traits on types with unsafe fields, closures, coroutines,
    `dyn`, parameters, aliases, foreign types and errors end the walk with no answer;
  - a (trait, type) pair already on the walk's stack holds: auto traits are coinductive
    (`TraitFlags::AUTO | COINDUCTIVE`, `CurrentGoalKind::CoinductiveTrait`), and the solver's
    coinductive cycles hold unless a leaf fails, which is what the walk computes;
  - the walk stops with no answer past 24 levels on its stack, past the depth the goal is
    evaluated with (below), or past 512 nodes.

  For an ADT the verdict is a query, `auto_trait_depth(adt, trait, crate)`, computed once per
  revision from its fields with identity arguments, with the asking crate's interner: a user
  auto trait's impls for a foreign type sit in the trait's crate, which the ADT's crate need not
  see, so only the asking crate sees what the solver sees. A verdict that holds with identity
  arguments holds for every instantiation: the walk reached no parameter whose answer it used,
  which is the brief's "no generic dependence". The verdict is the number of levels the walk
  used (0: no answer). An instance can close a cycle sooner than the identity walk, never later,
  so the instance needs at most that many.
  The fast path answers an ADT instance from the verdict alone and never walks it inline: that
  would instantiate and intern field types on every ask, and the canonical path's cache answers
  those instances once. Inside the verdict query the walk is inline (no query per field ADT, so
  no query cycle). `Copy`/`Clone` have their own query, `copy_clone_impl(adt, trait, crate)`.

**Allocation.** A hit allocates nothing: the per-ADT verdict is a `Copy` memo, the walk keeps its
stack in a fixed array of 24, and the impl lookup allocates only for impls inside bodies.

**What it must keep of rustc.** Every answer it gives is `Certainty::Yes`
with no constraints, which the solver gives on the same goal: same candidates (impl or auto),
same nested goals, coinduction for cycles. Param-env candidates are the one way the solver's
answer could differ: the new solver prefers a non-global where-bound, and one whose only local
names are regions (`Wrap<&'a u8>: Send`) unifies with a nested goal on `Wrap<&'static u8>` and
adds `'a == 'static`, an error rustc reports. A where-bound with a type or const parameter never
unifies with the walk's goals, which have none. So no auto-trait goal is answered under a param
env holding an auto-trait clause whose only local names are regions; `Copy`/`Clone` of a
non-generic ADT has no region for such a clause to meet.

**Depth.** Every goal this table answers is one rustc evaluates through the search graph, which
gives it a depth and charges it. rustc's own fast paths (sizedness, trivially `Copy`, stalls)
answer in `add_goal` and charge nothing, and rustc does the same. So the table charges what the
evaluation would:

- the walk counts levels: the goal is level 1, each goal it steps to one more, a cycle hit
  included (the solver evaluates that goal and finds it on its stack), and a sizedness clause
  the sizedness fast path proves none. A verdict adds its levels at the ADT's level;
- it answers only when the levels below the goal fit in the depth the goal is evaluated with:
  `allowed_depth_for_nested` (the parent's depth minus one, divided by 4 once the parent has
  overflowed, none at 0), or the crate's limit at the root. It returns them
  (`HoldsWithin { required_depth }`), and the solver charges the parent goal as for a global
  cache hit of that depth with no nested goals (`charge_answered_nested`). The parent's cache
  entry then records the depth rustc's does, and is reused at the same depths;
- that depth is known only when the goal is evaluated, not when it is added: a sibling that
  overflows in between divides it. So `add_goal` asks only for answers that cost no depth
  (`FastPathDepth::Added`). The nested goal loop asks for the depth-costing ones with the depth
  the search graph would give (`Evaluating`), and the root and stalled reruns ask for both
  (`Any`). With a proof tree being built, nothing depth-costing is answered;
- `Copy`/`Clone` through the one impl costs the goal's own level, so it is depth-costing too.

What the walk does not see, it overcounts, never undercounts: a cycle the solver closes through a
goal further up its own stack, and a repeat its provisional cache answers, cost the solver one
level where the walk counts the whole subtree. The walk then declines where the solver would
still fit, and the solver evaluates the goal as rustc does. A larger charge only makes a cache
entry apply at fewer depths, and recomputing gives rustc's result (the search graph's
cache-independence, which its validation mode checks). The vendored edits are in
`v2/requests/db-ty.md`, "the shape table charges search-graph depth as rustc does". Before them,
a goal nested within 24 levels of the limit held in blaze where rustc overflowed:
`harness/v2/frontend/cases/trait/recursion_send_nested_past_twice_small_limit_rejects.rs` is
that program, and `recursion_*` pins the limits around it.

### 3. The persistent cache by content (`goal_store.rs`, child `frontend-fast-trait-store`)

compile-time.md change 17. A root goal inserted into the global-goal table is offered to the goal
store; a root goal the table misses asks the store before solving.

- **Record**: the goal in stable form (types by `ContentId`, never dense IDs), the answer, and
  the facts its evaluation read (each `Fact::key` with the fact's hash), under domain
  `"blaze v2 goal <n>"`, plus the solver version (`harness/toolchain`'s nightly and the hash of
  the vendored solver's patch series).
- **Reuse** only when the solver version matches and every fact hashes the same in the current
  view: a constructive trace, as Shake's and Bazel's action caches. A reused answer is inserted
  into the global-goal table with the reads of the fact checks, so its query reruns when a fact
  changes.
- **Never stored**: cycle results (not root, or `capture_reads` gave `None`), goals with
  inference variables (not global), and every goal whose read set cannot be expressed as facts.

**What stands in the way today.** `blaze-items` depends on `hir-ty`, so `hir-ty` cannot name
`Fact`, the items' stable type hashes or the fact queries; and salsa's `Reads::edges` is
`pub(crate)`, so a capture's reads cannot be listed at all. The solver's reads today are salsa
queries over hir-def and hir-ty (the crate's trait impls, impl headers and predicates, field
types, signatures, lang items) that have no fact. The child's notes list each read and the fact
requested of lane items, and the store holds no goal whose reads it cannot express.

## What proves it

- `global_goals/tests.rs`:
  - the key's packing round-trips in all seven keyed modes, and the non-keyed shapes (an
    inference variable, an env with a region parameter, the erased mode, a defining scope) have
    no key;
  - the table against a `Mutex<HashMap>` reference under eight threads of inserts and lookups,
    level growth included;
  - a table hit and a shape hit allocate nothing (`blaze-alloc-count`); a table hit's
    `replay_reads` appends to the asking query's edges as any read does;
  - the differential test: eight generated crates of 24 ADTs each (derived and written
    `Clone`/`Copy`, negative and explicit auto-trait impls, a `!Sync` and a `!Unpin` stand-in for
    `UnsafeCell` and `PhantomPinned`, `PhantomData`, generic wrappers with and without bounds,
    recursion through references) and every (trait, type) goal of `Send`, `Sync`, `Unpin`,
    `Copy`, `Clone` and `Sized` over each ADT, the built-in types around it and the wrappers,
    each asked twice in two typing modes, once with the tables and once on a fresh database with
    them switched off; the answers must be equal, and every goal the shape table answers must be
    `EvaluatedToOk` in the plain run;
  - a region-only auto-trait where-bound in the env turns the shape table off for the goal;
  - near the limit: under `#![recursion_limit = "8"]`, each link of a 20-link `Send` chain is
    answered exactly when its depth fits (0, 5, 8, 16 and 128 levels available). The chain's
    goals at the root and nested under 0 to 16 `W<..>: Deep` wrappers give the plain solver's
    answers and overflow warnings with the tables on. The plain run has a quiet hold, a warned
    hold and a failure among them.
- `goal_store`'s tests: the record read back from the store equals the computed one, and a
  changed fact refuses it.
- `harness/v2/frontend/cases/trait/`: programs through rustc and blaze, every shape the tables
  take and each edge where they hand over.

## Prior art

- rustc's old solver's global evaluation and selection caches, keyed by global predicates
  without canonicalizing; the new solver's `GlobalCache` behind one lock.
- chalk's finding that canonicalization dominates small goals.
- Swift's requirement machine caching conformance lookups per (protocol, nominal type), the
  model for the per-ADT verdict.
- Shake's and Bazel's constructive traces for the persistent cache.
