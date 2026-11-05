---
status: plan
summary: frontend-fast — where the front end's time goes, area by area (name resolution, expansion, trait goals, method probing, inference, bodies, const eval, MIR building, borrowck): the work's lower bound, what v2's code does as of 2026-10-29, the faster design with its prior art, and which child builds it.
date: 2026-10-29
---
# frontend-fast: the front end's algorithms, redone where faster

The mandate (decisions.md, 10-29, "The front end's algorithms ... may be redone completely"):
any front-end algorithm may be replaced by a faster one, so long as every program the pinned
nightly accepts is accepted with the same meaning and every one it rejects is rejected with the
same error code and primary span. Porting rustc's or rust-analyzer's code is not the goal.

Costs below are counted from the code, not measured (nothing builds yet). The machine is the one
at the top of [../db/speed-of-light.md](../db/speed-of-light.md): about 3.5 GHz, L1 in 4 to 5
cycles, L2 in 14, L3 in 50, DRAM in 110 ns, branchy code at 2 instructions per cycle.

Each area's own notes, written by the child that builds it, are `fast-<area>.md` in this
folder. This file is the map: the bound, today's state, the design, and the owner.

## How the areas were chosen

Lane db has already brought each body-local component close to its bound
(the `../db/speed-of-light-*.md` docs):

- types are hash-consed `NonZeroU32` rows;
- inference's union-find is flat columns, with rollback by log truncation
  (`next_solver/infer/union_find.rs`);
- the solver's cache is lock-free and shared across queries within a revision
  (`tls_cache.rs` with salsa's `capture_reads`/`replay_reads`);
- the vendored solver already has rustc's goal fast path (`SolverDelegate::compute_goal_fast_path`
  in `next_solver/solver.rs`: trivially sized types, trivially `Copy`).

What is left is the algorithms themselves: work that is done again although its answer is
already known (in another body, another crate, an earlier run), and work done in full where most
of the inputs take one cheap shape. Those are the areas below.

## 1. Name resolution and the expansion fixpoint

- **Lower bound.** Per edit: rerun only the modules whose facts (names read, names found
  missing) changed, and the glob-import fixpoint only over the strongly connected set the change
  touches. A cold crate: one pass over its items plus one fixpoint iteration per glob cycle,
  about 50 ns per item.
- **Today.** Being redone already, by sibling `frontend-nameres` (per-module facts with
  negative facts, SCC-scoped fixpoint; [nameres.md](nameres.md), [nameres-core.md](nameres-core.md)),
  differentially tested old against new ([nameres-diff.md](nameres-diff.md)). Expansion rounds
  run in parallel as plane tasks (lane db, `../db.md`).
- **Design.** That design is the Datalog one the mandate names: semi-naive evaluation (only new
  facts drive the next round, as Soufflé does) over dense `u32` module and name IDs. No second
  redesign here; frontend-fast adds only the rustc differential (section 9) for name resolution
  errors (E0425, E0432, E0433, E0659 and the rest) with their primary spans.
- **Owner.** `frontend-nameres` (sibling).
- **Maintained form.** Lane dbsp maintains this resolution, the impl tables and the trait
  candidate sets by deltas on the write side, published one salsa input per slice
  ([../../design/dbsp.md](../../design/dbsp.md)). `facts/` becomes its differential reference.

## 2. Trait goals

- **Work.** A body asks a few hundred goals. Most are `Sized`, `Copy`, `Clone`, auto traits, and
  impl lookups whose self type has no inference variables, the same goals other bodies ask
  (`T: Sized`, `String: Display`, `Vec<u8>: Clone`).
- **Lower bound.** A goal already answered costs one probe: hash a key of two or three `u32`s,
  one line, about 20 cycles (6 ns). A goal never answered anywhere costs the solver's evaluation;
  the bound is to evaluate each distinct global goal once per toolchain and impl set, for
  everyone.
- **Today.**
  - Every goal is canonicalized before the cache probe (a fold over the goal, interning a
    canonical form), even when it has no inference variables and no free regions. That is the
    bulk of a hit's cost, since the probe itself is a few loads.
  - The cache lives for a revision. A new run, or a dependent crate in a fresh process, solves
    `String: Display` again.
- **Design.**
  1. **Global-goal table before canonicalization.** A goal with no inference variables, no
     placeholders and only erased or `'static` regions is its own canonical form. Because
     types and predicates are hash-consed, its identity is (predicate ID, param-env ID, solver
     mode, typing mode): two `u32`s and two tags packed into a `u64`. It is looked up in an
     open-addressed table keyed by that `u64` before the solver builds anything; a hit returns
     the stored result without folding. This is rustc's old solver's global evaluation and
     selection caches, keyed by non-canonical global predicates, done with packed IDs.
  2. **Shape table for the common traits.** `Sized`, `Copy`, `Clone`, `Send`, `Sync`, `Unpin`,
     `Freeze` on a self type whose structure decides the answer (scalars, references, raw
     pointers, `!`, arrays and tuples of such, ADTs whose answer is a stored per-ADT bit set
     with no generic dependence): answered by a `match` on the type's kind tag and a bitset per
     ADT, extending rustc's fast path (which covers sizedness and trivially-`Copy` alone).
  3. **Persistent cache by content** (compile-time.md change 17). A global goal's result,
     in stable form (types by `ContentId`, never dense IDs), stored in `blaze-cas` with the list
     of facts it read (items' `Fact::key` with each fact's hash, from the `Reads` that
     `capture_reads` already records). Reuse needs every fact to hash the same in the current
     view: a constructive trace, as Shake and Bazel's action cache do. Cycle heads and
     goals with inference variables stay per query, as compile-time.md 17 requires.
- **Prior art.** rustc's old-solver global caches and the new solver's `GlobalCache`; chalk's
  lesson that canonicalization dominates small goals; Swift's requirement-machine caching of
  conformance lookups per (protocol, nominal type).
- **Owner.** `frontend-fast-trait` ([fast-trait.md](fast-trait.md)).

## 3. Method resolution

- **Work.** Each method call: autoderef the receiver, gather inherent candidates by simplified
  self type and trait candidates from the traits in scope, filter by applicability (solver
  calls), pick by rustc's order (by-value, autoref, autoref-mut, then the next deref step),
  report ambiguity and the unstable-candidate lints.
- **Lower bound.** Most receivers' types are known when the call is checked (no inference
  variables). For those the answer depends only on (receiver type ID, method name symbol, the
  scope's traits-in-scope set, param env, crate, edition): one probe, about 6 ns.
- **Today.** No cache at all (`method_resolution.rs`, `method_resolution/probe.rs`): every call
  runs the whole probe, with one or more solver calls per candidate. rustc caches only the
  autoderef steps (`method_autoderef_steps`), not the pick.
- **Design.** Intern each body's traits-in-scope set to an ID (the scopes repeat across bodies of
  a module, so this is a per-module table). A pick cache keyed by the IDs above, shared by every
  body of the crate within a revision through the same `capture_reads` rule the solver cache
  uses (a pick is reused only with its reads replayed), filled only for receivers without
  inference variables and probes that emitted no lint or diagnostic (those stay per body, since
  their spans differ). The autoderef steps for a known receiver are a cached CSR list.
- **Prior art.** Roslyn's per-binder lookup caches; Swift's member-lookup table per nominal type.
- **Owner.** `frontend-fast-method` ([fast-method.md](fast-method.md)).

## 4. Inference within a body

- **Lower bound.** About 40K cycles (11 µs) for the typical body without probing and solving
  ([../db/speed-of-light-infer.md](../db/speed-of-light-infer.md)). Today about 105K, 2.6× off.
- **What is left.**
  - **Coercions**, about 150 sites a body, 2.7× off, from a snapshot per attempt.
  - **Fulfillment rounds**, 2.4× off, from rechecking stalled obligations.
- **Design.**
  1. Coercion by head dispatch: when the source and target are the same ID, or their kind tags
     decide that only one coercion rule can apply (no reference, pointer, closure, `!`,
     unsizing or reborrow target), relate directly with no snapshot. Snapshots remain only
     where rustc's order tries one rule and may fall back to another.
  2. Watch lists for stalled obligations: each pending obligation is on the watch list of the
     inference variables it stalled on; unifying a variable moves its watchers to the ready
     worklist. A round then touches only obligations whose inputs changed (MiniSat's
     two-watched-literal propagation). Order of processing stays rustc's, so results and
     error order are identical.
- **Owner.** `frontend-fast-infer` ([fast-infer.md](fast-infer.md)), agreed with
  `solver-sync-infer-core` and `mem-db-infer` before touching their files.

## 5. Which bodies are checked, where and when

- **Lower bound.** A build checks every body of the crates it compiles (rustc rejects an error
  in a function nothing calls), in parallel on the plane's pool up to the core count, the bodies
  reachable from what it builds first (compile-time.md 1 and 16), as they feed codegen; after an
  edit only the bodies whose own text or whose read signatures changed. A dependency from
  crates.io is checked once per toolchain for everyone (compile-time.md 8).
- **Design.**
  1. A body's inference reads its own HIR and the signatures it names; nothing crate-wide. Then
     its memo's edges are exactly those, and early cutoff on signature fingerprints (lane db)
     skips it when they are unchanged. The child audits every crate-wide read in `infer`,
     `method_resolution` and the solver glue, and replaces each with a per-item or per-bucket
     read (impls by (trait, simplified self type), items' facts).
  2. Bodies are checked as one plane task each, the reachable set first, not from a crate-wide
     loop; the unit's outcome waits for every body.
  3. The check results of a dependency (inference, MIR, borrowck outcome) are stored in the
     global cache keyed by the crate's content and the toolchain, beside frontend-deps'
     signatures.
- **Owner.** `frontend-fast-bodies` ([fast-bodies.md](fast-bodies.md)).

## 6. Const evaluation

- **Work.** Most constants in real code are literals, paths to other constants, arithmetic on
  them, `size_of`/`align_of`, and array lengths.
- **Lower bound.** A literal or an arithmetic tree of them costs its node count (a few ns). A
  constant already evaluated anywhere for the same inputs costs one store probe.
- **Today.** Every constant lowers to MIR and runs in the MIR evaluator (`consteval.rs`,
  `hir-ty-mir`'s `eval`); results memoized per revision.
- **Design.**
  1. An evaluator over HIR for the closed shape (literals, unary and binary arithmetic with
     rustc's overflow and division checks, casts between integers, paths to constants,
     `size_of`/`align_of` of a laid-out type), producing the same allocation bytes the MIR
     evaluator would. Anything else goes to MIR evaluation; that is the one path for those
     constants, so it is a split by shape, not a fallback.
  2. Results of constants in dependencies cached by content in the global cache.
- **Prior art.** Zig's comptime evaluating directly on its AIR; Clang's constant interpreter
  with its fast path for integer constant expressions; rustc's `Const::try_from_lit`.
- **Owner.** `frontend-fast-const` ([fast-const.md](fast-const.md)).

## 7. MIR building

- **Lower bound.** One pass over a body's HIR writing statements and terminators into flat
  arrays, about 30 cycles a node.
- **Design.** The child counts what `hir-ty-mir`'s `mir/lower.rs` and `match_tree.rs` do per
  node (place building, temporaries, drop scheduling, the match decision tree) against that
  bound, and replaces the parts more than 10% off, agreed with lane codegen, whose crate it is.
- **Owner.** `frontend-fast-mir` ([fast-mir.md](fast-mir.md)).

## 8. Borrow checking

- **Bound and gaps.** In [../db/speed-of-light-borrowck.md](../db/speed-of-light-borrowck.md):
  a typical body is within 10% of its dataflow bound; a large body is about 15× off, from dense
  rows, nine passes over MIR and recomputed place types.
- **Design.** That doc's six items, which nobody has applied yet: rows by size (dense up to two
  lines, line-summarized above, as rustc's `MixedBitSet` with cache-line chunks), difference
  propagation in the fixpoint, one decoding pass into fact columns, a place-type column, cache
  before folding in `is_copy`/`drop_glue`, a per-body frame arena. Polonius-style facts over
  dense IDs are what the decoding pass produces; the solver stays NLL over those columns, which
  computes the same answers at less cost than a Datalog join. Unchanged bodies are already
  skipped by their memo.
- **Owner.** `frontend-fast-borrowck` ([fast-borrowck.md](fast-borrowck.md)).

## 9. Proof: differential against rustc, and fuzzing

Dropped 2026-10-30 (Andrew: no harness compares blaze with rustc or cargo):
`rejections.py` and `frontend/fuzz.py` are deleted, and `frontend/cases.py` holds each case to
its own header ([the harnesses](../../../../harness.md#the-v2-proof)). The plan below is as of
2026-10-29.

- `harness/v2/rejections.py` (proof, done) compares rustc's error codes with blaze's. The bar
  also needs the **primary span** and, for accepted programs, the **same meaning**.
- `frontend-fast-diff` adds:
  - span comparison;
  - a case directory per area, `harness/v2/frontend/<area>/`, which each child fills with the
    programs its redesign must treat as rustc does;
  - a program generator that fuzzes the redesigned paths: trait goals, method picks, coercions,
    constants and borrow patterns. Each generated program goes to rustc and to blaze, and the
    outcome, codes, primary spans and the run's output must be the same.
- **Owner.** `frontend-fast-diff` ([fast-diff.md](fast-diff.md)).

## What proves each change

Every redesign keeps the interface its callers use and comes with:

- a differential test in its crate: the old algorithm (kept in the test module only) against
  the new one on the same inputs;
- its cases in `harness/v2/frontend/<area>/`, run against rustc by section 9's harness once
  compiling is allowed;
- an allocation-budget test with `blaze-alloc-count` wherever it claims none (every cache hit).
