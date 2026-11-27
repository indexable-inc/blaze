---
status: current
summary: blaze's trait solver, crates/blaze-solve, called by blaze-typeck: Rust's semantics as the pinned nightly's next-generation solver implements them (vendor/rustc_next_trait_solver is the reference, ported rule by rule), on a data-oriented engine: goals, obligations and candidates as fixed-size rows in columns, an explicit stack in place of recursion, the reference's search graph for cycles, wake lists for stalled obligations, a ground fast path through canonical bytes and marker bits, a SIMD fingerprint prefilter over impl buckets, ground goals shared across a request's bodies, and what is not ported.
---
# The trait solver

crates/blaze-solve is blaze's trait solver, written from scratch. Semantics are not
open: they are the pinned nightly's next solver, `vendor/rustc_next_trait_solver` and
`vendor/rustc_type_ir`, and every rule's doc comment names the reference file and function it
ports. Only the representation and the engineering are new.

## The seams

- **Types** (`blaze_solve::Tys`, src/tys.rs): what blaze-typeck's `Terms` implements over
  blaze-ty's arena (crates/blaze-typeck/src/infer/terms.rs): one level of a term
  (`kind`), its `flags` and `markers` columns, interning (`mk`, `mk_list`), fresh variables,
  `shallow_resolve`, `unify` (with `Relate::Lazy` a non-rigid alias met on one side is handed
  back as a deferred pair: the solver emits `Projection { alias, term: fresh }` and relates
  `fresh` with the other side, as rustc_type_ir relate/solver_relating.rs does for
  `Alias(IsRigid::No, ..)`; a rigid alias relates by structure; with `Relate::Structural`
  everything does, as canonical/mod.rs `ResponseRelating` applies responses), snapshots
  with an undo log, and the binding log the wake lists read. Terms are one arena: types,
  regions and consts are `Ty(u32)` handles of one `TyKind` (src/term.rs), so generic argument
  lists are plain `List`s of handles. `Ty`, `List`, `TyKind`, `Flags` and `Markers` are defined
  here (src/term.rs) and again in blaze-ty; `Terms` bridges the two. The
  tests' arena (src/testing/tys.rs) is the second implementation and the model of the trait.
- **Facts** (`blaze_solve::Facts`, src/facts.rs): the world: trait definitions, the impls of a
  trait for a self shape (with their fingerprint columns), impl headers, ADT definitions, fn
  signatures, associated items, lang items, opaque types. Each method takes a `Seal` only this
  crate makes (blaze-typeck's pattern), so every read goes through the recorder. Answers carry
  their types in a portable table (`Portable`: nodes are `TyKind`s whose handles index the
  answer's own nodes and lists, generic parameters by index) and a content `digest`.
  blaze-typeck's `FactCache` implements it (crates/blaze-typeck/src/infer/facts.rs), filled
  only through typeck's `Reads`, so a body's read list holds what its goals read.
- **The driver's side**: `Solver` (src/eval.rs: `env`, `evaluate`, `why_no`, `why_maybe`) and
  `Fulfill` (src/fulfill.rs: `register`, `select`, `pending`), held by blaze-typeck's inference
  context (crates/blaze-typeck/src/infer/mod.rs). The caller owns the snapshot that wraps them;
  the solver owns everything inside a goal.

## Goals (`Pred`, src/term.rs)

The kinds of the pinned nightly's `PredicateKind` and `ClauseKind` (rustc_type_ir
predicate_kind.rs). It has no `AliasRelate`: an alias carries `IsRigid`, and relating a
non-rigid one emits a `Projection` goal, so this solver has none either (2026-11-15).

| kind | meaning | reference |
|---|---|---|
| `Trait { trait_, args, polarity }` | `args[0]: trait_<args[1..]>`, positive or negative | trait_goals.rs |
| `Projection { alias, term }` | `<..>::A == term` (`ClauseKind::Projection`); also what relating a non-rigid alias emits, with a fresh `term` | project_goals/mod.rs; rustc_type_ir relate/solver_relating.rs |
| `NormalizesTo { alias, term }` | the solver-internal step: the alias's value, term unconstrained (`PredicateKind::NormalizesTo`) | normalizes_to.rs |
| `TypeOutlives`, `RegionOutlives` | hold here; regions are erased, so the borrow checker takes outlives obligations from the fulfillment loop, not from responses (see Not ported) | solve/mod.rs `compute_type_outlives_goal` |
| `WellFormed(term)` | the term's WF obligations | solve/mod.rs `compute_well_formed_goal` |
| `ConstEvaluatable(ct)` | stalls while the const has variables, else holds | solve/mod.rs |
| `Subtype`, `Coerce` | stall while either side is an unresolved variable, else relate | solve/mod.rs `compute_subtype_goal` |
| `DynCompatible(trait_)` | the trait's flag, read from its definition | solve/mod.rs |
| `HostEffect { .. }` | `const` trait bounds | effect_goals.rs |
| `ConstArgHasType` | a const argument's type equals its parameter's | solve/mod.rs |

## Responses

A goal's answer is canonical: `certainty` (one byte: yes; maybe because of ambiguity; maybe
because of overflow, with or without the hint to raise the limit; `Certainty::and` and `or`
port `MaybeCause::and/or`), the **variable values** (for each canonical variable of the input,
its value as canonical words over the input's variables and new existential ones; one flag
when the response is the identity), **how a trait goal was proven** (`TraitGoalProvenVia`, which
normalization reads), and the **nested normalization goals** a `NormalizesTo` answer hands back
to its caller (eval_ctxt/mod.rs `evaluate_added_goals_and_make_canonical_response`). Region
constraints and opaque-type uses are not in it (see Not ported). No solution is its own result, not a certainty. Applying a
response in the caller instantiates the existential variables fresh and unifies each input
variable's original term with its value (eval_ctxt/mod.rs `instantiate_and_apply_query_response`).

## The search (src/search.rs, src/eval.rs)

One explicit stack of frames; a frame is a fixed-size row (input id, state byte, candidate and
nested-goal ranges into shared pools, snapshot, depth, step kind, cycle heads, provisional
result, iteration count). A rule never calls the evaluation of a nested goal: it records it, and
the loop pushes a frame for it and resumes the parent at its state when the child pops. Depth
is the stack's length; past the limit (the crate's `recursion_limit`, 128 by default) the goal
answers `Maybe(Overflow { suggest_increasing_limit })` as `stack_overflow_result` does: a
typed answer, never a crash. Frames, candidates and nested goals are popped in stack order, so
each pool is a vector truncated on pop: no allocation per goal after warm-up.

Cycles follow rustc_type_ir/src/search_graph/mod.rs: a goal met on the stack is a cycle; the
path's kind is the `PathKind::extend` (a max) of every step from the head to the use, each step's
kind from the parent goal as `step_kind_to_nested` gives it (a coinductive trait, `Sized` or an
auto trait: `Coinductive`; a projection: `Unknown`; anything else: `ForcedAmbiguity`). The first
iteration's provisional answer is `initial_provisional_result`: yes for coinductive, overflow
ambiguity for unknown and forced ambiguity; an inductive path (only the coherence mode can
make one) is unknown. Every frame above the head gets it as a cycle head. When a head pops, its
result is compared with the provisional one it handed out; if they differ and the head was used,
it is evaluated again with the new result as provisional, at most `FIXPOINT_STEP_LIMIT` (8)
times, then `fixpoint_overflow_result`. A result that depends on a head still on the stack goes
to the **provisional cache** (input, heads, path from the head, result), valid while those heads
stay and their provisional results do not change; on a hit the use is re-propagated with the
path re-extended (`lookup_provisional_cache`). Everything else goes to the task cache.

Not ported (named): rebasing provisional entries when a nested head pops
(`rebase_provisional_cache_entries`): entries are dropped instead, which is less reuse and the
same answers; `candidate_usages` (ignoring head usages of discarded candidates): every usage
counts, which can rerun a fixpoint the reference skips; the doubled-limit overflow retry and its
future-compat lint (`maybe_evaluate_root_goal_for_proof_tree_with_higher_recursion_limit`).

## Candidates and how they merge (src/assembly.rs, src/trait_goals.rs)

A candidate is a row: its source, a range of equations (pairs of terms to unify), a range of
nested goals (each with its `GoalSource`), a certainty floor, and after its probe a response id.
Assembly writes rows; the engine probes each in a snapshot: unify the equations (deferred alias
pairs become `Projection` nested goals with a fresh term), then evaluate the nested goals to a fixpoint of rounds
as `try_evaluate_added_goals` does, then canonicalize the response, roll back.

Order, as `assemble_and_evaluate_candidates`: a self type that is an unresolved type variable
gives one forced-ambiguity candidate and nothing else; else alias bounds, then the param env,
then builtins, and only if no non-global where-bound or alias bound applied with no constraints,
impls and object bounds (a second phase of the frame). Merging is `merge_trait_candidates`:
a trivial builtin wins; for a marker trait (auto, `Sized`) a self-bound alias bound wins; any
non-global where-bound means only where-bounds are merged; then alias bounds; then specialized
impls are filtered (`filter_specialized_impls`, from the impl's `default` flags), the builtin dyn
candidate is preferred over impls when it has no constraints (`unsound_prefer_builtin_dyn_impl`);
then global where-bounds are dropped unless they are all there is; two impls (or an impl and a
builtin) never merge unless the trait is `#[marker]`: ambiguity. `try_merge_candidates`: an
always-applicable candidate (yes, identity, no constraints) wins; equal responses merge, an `or`
of region constraints keeps the one with none; otherwise `bail_with_ambiguity`.

Where each rule lives: param-env and impl candidates and merging in src/assembly.rs and
src/trait_goals.rs; builtins and structural auto traits in src/builtin.rs and src/structural.rs
(Sized and MetaSized, Copy/Clone for built-in shapes, the Fn family for closures, fn items and
pointers, Tuple, Pointee, DiscriminantKind, Destruct, auto traits with coinduction and
`disqualify_auto_trait_candidate_due_to_possible_impl`, `dyn Trait` with its elaborated
supertraits, unsizing); the `AsyncFn` family in src/async_fn.rs; aliases in src/alias.rs
(normalizes-to and projection goals, rigid and non-rigid aliases as the reference's `IsRigid`,
candidates from impls, param env and alias bounds, opaque types, associated consts); coherence
in src/coherence.rs (overlap of two impl headers with the intercrate mode and the orphan check).

Two typed seams carry what the reference keeps in side channels. A normalization asks how its
trait goal was proven through a `via_only` nested goal: its response is not applied, and the
frame records its `TraitGoalProvenVia`. A failed trait goal is `Via::Failed`, which fails the
normalization the way the reference's `NoSolution` does. A form not ported (inherent and free
aliases, anon consts, const evaluation) stops with `SolveError::Unsupported`, never a guessed
answer (rule 3). Fulfillment's two strategies (`fulfill::Wake`, `fulfill::Rescan`) are one
type parameter (rule 78) and are tested equal. A portable term is imported by a walk of the
nodes its root reaches, so a record's table may hold many terms.

## The engineering, each with its floor (rule 67)

**(a) Records in columns, no recursion, no allocation per goal.** Obligations, frames,
candidates, equations, nested goals and responses are `Copy` rows in per-field `Vec`s of the
task's allocator (`alloc: Alloc<'a>`); a term is 4 bytes; a predicate is a 32-byte enum row. The
stack, the pools and the canonical buffers are reused across goals. Floor: zero allocations per
goal once the pools have grown to the task's high-water mark; native stack depth constant in the
goal's depth.

**(b) Wake lists.** A stalled obligation (maybe, with unresolved variables in its goal after
applying the response) is filed under each of those variables: `wait_head[var]` is the first
entry of a list in two columns (`wait_ob`, `wait_next`). `select_where_possible` reads the
binding log of the `Tys` from its cursor, moves every obligation filed under a newly bound
variable to the ready queue (a generation stamp stops a double push), and evaluates only those;
an obligation with no variables to wait on (stalled on overflow, say) is retried only at
`select_all`. Floor: work proportional to the bindings made plus the obligations they wake; the
rescan the reference does (`rerunning_stalled_goal_may_make_progress` per goal per round) costs
rounds times pending. `examples/cost.rs` measures both on 10,000 obligations.

**(c) The ground fast path.** A goal whose terms have no inference variable (one load of the
flags per argument) skips the canonicalizer's variable map: its canonical words are its
encoding, hashed and looked up in the task cache, then the request's goal table (e), before anything
else. Marker answers live in the type's `markers` byte (known and holds, for `Sized`, `Copy`,
`Send` and `Sync`), set from a solved ground goal in an env without global where-clauses; a
goal on a known type is then one bit test. Inside a frame a hit also appends the reads of the
record that set it (an indexed load of its entry id), so per-goal read lists stay complete.
Floor: bit test 1 to 2 ns; cache hit one hash of the words plus one probe and one compare.

**(d) The fingerprint prefilter (src/prefilter.rs).** Impl candidates come from blaze-impls'
buckets (the trait for the self shape, then the blanket bucket), and each bucket carries
**fingerprint columns**: 16 one-byte positions per impl header: for each of the first four
trait arguments (self first), the head of the argument and the heads of its first three
children (depth 2). A byte is 0 for a wildcard (a parameter of the impl, an alias, an error, a
region) or `1 + mix(tag, def, payload) % 255`. The columns are position-major (byte `p` of
impl `i` at `p * stride + i`), so a goal, which makes its own 16 bytes (wildcards where it has
an inference variable or an alias; a parameter or placeholder of the goal is rigid and has a
byte), visits only its non-wildcard positions, and per position compares 16 impls (NEON, SSE2)
or 32 (AVX2, when the target enables it at compile time) in one vector op: an impl survives
when every visited position's byte equals the goal's or is 0. A scalar path gives the same bits
and a property test holds them equal. Only survivors are instantiated and unified. A collision
of two heads in one byte only lets a candidate through to unification: never a wrong rejection.
Floor: (non-wildcard positions) x ceil(n / 16) vector compares; at n = 1,024 and two positions,
128 compares where a scan touches 1,024 headers. blaze-impls' bucket records do not hold the
columns; the `Facts` implementation builds them per bucket through `prefilter::fingerprint`.

**(e) Ground goals shared across a request (src/share.rs).** A goal finished with no cycle
head below it and no canonical variable, placeholder, closure or coroutine in its input is
solved once a request: its key (`GoalKey`) is the digest of the solver's code id, the recursion
limit, the typing mode, the canonical words of its parameter environment and its canonical
predicate, with the words kept beside the digest so two goals under one digest are refused
(`ShareRefused::Collision`). The table is the caller's (`GoalShare`), handed in through the
fact source; this crate stays sans-IO. Results that met an overflow are never shared.

**(f) Pure, re-entered, never blocking.** Every outside fact is a `Facts` call that answers or
says `Missing`; `Missing` unwinds the stack (rolling back every snapshot) and the call returns
`Stop::Need(question)`, after which the driver fetches it and calls again (bottom.md section 2).
Completed, cycle-free subgoals stay in the task cache, so re-entry resumes in effect where it
stopped. Every read goes through the recorder.

**(g) Nothing else shared on the solving path.** Each task has its own arena, pools and task
cache; the request's goal table (e) is the one shared structure. Floor: no atomic
and no lock per goal.

## Test language (src/testing/)

Cases are text in `src/tests/cases/*.solve` (builtin, auto, alias, coherence; `prelude.solve`
is read under every file), each naming the reference rule it tests:

```
case vec_clone = trait_goals.rs::consider_impl_candidate
goal Vec<u32>: Clone => yes
goal Vec<?0>: Clone => maybe
case tuple_sized_tail = structural_traits.rs::instantiate_constituent_tys_for_sized_trait
goal (u8, [u8]): Sized => no
```

One line per item: `[extern lang(x) auto marker coinductive fundamental unsafe const]` on
`trait`, `struct`, `enum`, `impl[@label]`, `fn`, `opaque Name: Bounds [= hidden]` and
`foreign`; params get an implicit `Sized` unless `?Sized`. Case lines: `params`, `env`,
`mode coherence`, `goal Pred => yes [?N = Ty, ..] | no | maybe | overflow`, `overlap @a @b =>
overlap | disjoint | may_overlap`, `orphan @a => ok | not_local | uncovered(i)`. Types include
`closure#N<kind, sig, upvars>`, `fn#name<..>`, `opaque#Name<..>`, `dyn ..`, `<T as Tr>::A`,
`rigid <..>` (an alias marked rigid), `?N`/`?iN`/`?fN`, `!N`, `{3}` and `_`.
`src/testing/parse.rs` parses, `world.rs` lowers each case's items into one portable table and
answers `Facts` (impl buckets per shape with their fingerprint columns), `tys.rs` is the arena,
`run.rs` checks each goal from one starting state. Controls (testing skill, `blaze_sim::control`):
a coinductive cycle taken as inductive must fail the cycle cases; a prefilter that drops a
wildcard must fail the prefilter's property against the truth, at every alignment. Wake lists
and rescan are tested equal on the staggered load (`testing/stagger.rs`), which
`examples/cost.rs` also times.

Measured 2026-11-15 (Mac, release, `examples/cost.rs`): cold 91k ground and 98k
generic goals/s, warm (canonical cache) 7.5M and 7.9M; fulfillment of 10,000 obligations in
100 waves, wake lists 2.1 ms and 20,199 evaluations against rescan's 40.5 ms and 510,180
(25x); prefilter scalar/simd 7.3/8.0 ns at 8 impls, 40/16 ns at 64, 678/297 ns at 1,024.

## Ported from memory, not with the reference open

The search graph's provisional-cache validity rule and fixpoint comparison (read only in part:
`search_graph/mod.rs` lines 140-250 and 860-1000); `filter_specialized_impls`; the certainty
lattice's overflow `suggest_increasing_limit` merge; the components rule for type outlives
(not ported: an outlives goal is recorded whole). Each such function says so in its doc.

## Not ported (by reference file)

- `canonical/canonicalizer.rs`: universe compression; placeholders are canonicalized literally.
- `placeholder.rs`, leak check: higher-ranked bounds over regions instantiate their binder with
  fresh region variables and regions are not related in `unify`, so `for<'a>` bounds hold
  where rustc's leak check would refuse a few.
- `solve/eval_ctxt/solver_region_constraints.rs`: region equality from unification is not
  recorded; only outlives goals are.
- `solve/inspect/`: proof trees.
- `solve/project_goals/opaque_types.rs` defining uses beyond recording hidden types.
- `solve/reflect.rs`, `solve/error_union.rs` (blaze's extensions in the fork), coroutines and
  coroutine closures (`Future`, `Iterator` for gen blocks, `Coroutine`), `TransmuteFrom`,
  `BikeshedGuaranteedNoDrop` (src/builtin.rs's module doc keeps the list).
- `normalize.rs` (deep normalization for writeback): the caller walks and calls `normalize`.
