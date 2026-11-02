---
status: plan
summary: frontend-nameres-core — the per-module resolver (nameres.md §2-§6, §8): round-0 declaration queries, per-SCC units, per-module macro steps, the round driver and assembly; the contract between its files and what each test proves.
date: 2026-10-29
---
# frontend-nameres-core: the per-module resolver

The design is [nameres.md](nameres.md); this is how core builds it, and the contract between
its files (`crates/hir-def/src/nameres/facts.rs` and `facts/{ops,env,kinds,queries,collect,unit,macros,driver}.rs`).
Every rule is the old collector's (`nameres/collector.rs`), moved; only the schedule changes.

## 1. Three collections, one collector

The old `ModCollector` is ported once (`collect.rs`), generic over a [`Sink`](#4-the-sink) that
receives every mutation as a `DeclOp` (`ops.rs`):

| sink | file | holds | used by |
|---|---|---|---|
| `DeclSink` | `collect.rs` | one module's row (round 0), its log, its env | `module_decls` |
| `StepSink` | `macros.rs` | one module at one round (its scope cloned from `fact_handle`) and the modules its expansions declare | `macro_step` |
| `Unit` | `unit.rs` | a block's whole map | `collect_block` |

A crate is collected as rounds (§4 of nameres.md). Round 0: `module_decls` per module (DFS).
Each round `r`: per component of `partition(r)` that the round can change (§9), `scc_state`
replays its members' ops of `r` and runs the import worklist; then per module with a directive
`macro_step(r)` resolves its pending macro directives against the round's facts and collects
their expansions as ops of round `r + 1`.

## 2. Query graph (acyclic by construction)

```text
module_decls(key)        -> its children's keys, legacy_exports / subtree_macro_exports / decl_handle of earlier keys
round_modules(r)         -> round_modules(r-1), macro_step(r-1, stepping), round_kind(r),
                            round_active(r-1), fact_handle(r-1, ran) (r = 0: the DFS of module_decls)
module_round(r, M)       -> macro_step(r-1, M), macro_step(r-1, parent of M), (root: macro_step(r-1, stepping))
import_shape(r)          -> module_decls, module_round(1..=r, gaining)
partition(r)             -> import_shape(r)
round_env(r)             -> root's crate-level ops of rounds < r (r = 0: root_decl + root's round-0 log)
round_kind(r)            -> round_kind(r-1), round_progress(r-1)
round_active(r)          -> round_modules(r), partition(r), partition(ran), scc_state(ran, *).upstream,
                            round_env(r, r-1), the prelude holder's scc_state(r) when it runs
scc_state(r, anchor)     -> partition(r), module_round(r, gaining members), round_env(r), round_kind(r),
                            round_active(r), scc_state(ran, same members) (carried),
                            fact_handle(fact_key(r, upstream)) through path reads
fact_handle(r, M)        -> partition(r), scc_state(r, comp(M))          (only a round that runs comp(M))
unresolved_named(r, M)   -> scc_state(r, comp(M))                        (as fact_handle)
macro_step(r, M)         -> round_kind(r), fact_handle(fact_key(r, *)) through path reads,
                            unresolved_named(ran_by(r, M), M), macro_step(last step, M) (pending, side),
                            module_round(declared round, M) (macros it started with)
round_progress(r)        -> macro_step(r, stepping), the carried directives of the rest
```

No query of round `r` reads a `macro_step` of round `r`, so expansion effects cross modules only
one round later (nameres.md §8.3). `round_active(r)` reads no `scc_state` of round `r` but the
prelude holder's, whose component reads no other (§9).

## 3. Values and their invariants

- **Module order** of a round = `round_modules(r).order`; a module's index there is its index in
  `import_shape(r).modules` and in `partition(r)`. Round 0: root, then DFS preorder of
  `module_decls` children (the old map's insertion order). Round `r + 1`: round `r`'s order, then
  the modules each `macro_step(r, M)` declared (`MacroStep::new_modules`), steps in module order.
- **Which ops a unit applies.** From a module `M`'s round-0 log: the ops with
  `target() == Some(M)` and `is_resolution_op()`; ops with target the crate root in a log owned
  by another module are skipped there, and applied in the root's replay at the root log's
  `Splice { key, .. }`, as the ops of `subtree_macro_exports(key)`. From `module_round(r, M)`
  (`r >= 1`): every resolution op (all target `M` by construction).
- **`module_round(r, M)`**, `r >= 1`: the resolution ops of `macro_step(r-1, M).ops` with target
  `M`; for a module declared at `r - 1`, first the ops of the declaring step (its parent's) with
  target `M` (its `Init`, items, `Import`s, `Macro`s, `SetModDir`); for the root, then the root
  ops of every other `macro_step(r-1, *)`, steps in module order. `DeclOp::Macro` ops are kept
  (a module's pending directives of round `r` are those of its last step plus these).
- **Pending directives** of `macro_step(r, M)`: `r = 0`: the `DeclOp::Macro` ops of
  `module_decls(M)` in order (`M`'s round-0 log), or, for a module declared at `r - 1`, those
  of `module_round(r, M)`; `r >= 1`: `macro_step(r-1, M).pending` then the `Macro` ops of
  `module_round(r, M)` that did not come from `M`'s own step (a `StepSink` moves its own
  module's new directives straight into `pending` with `take_macros`, so they are never in
  its log).
- **Side tables** of `M`: `module_decls(M).side` at round 0, then each `macro_step` carries its
  module's on (`MacroStep::side`). A module declared by a step starts from empty tables plus its
  `SetModDir`.
- **Crate-level state** (`CrateEnv`): only the root's collection and the root's steps make
  `ExternPrelude`, `MacroUsePrelude`, `Prelude` ops. `round_env(r)` folds them with
  `CrateEnv::after` over `root_decl`'s env, the root's round-0 log and the root's steps of
  rounds `< r`. A `#[prelude_import]` is resolved by the unit holding it (`SccState::prelude`);
  every other unit of the round installs it from that state (the graph puts that component
  upstream of every other; the unit asserts `may_read`).
- **`DefMapCrateData`**: `crate_data(krate)` (the crate attributes) plus the `ExportedDerive` and
  `ProcMacroFn` ops of every log, in round then module order, applied by the driver at assembly;
  views use `crate_data` (nothing in a crate's own collection reads its own exported derives or
  proc-macro mapping).
- **`ModuleIdLt` creation** (hazard 2): a child module's id is created by the query that
  declares it (`module_decls` of its parent, `macro_step` of the module whose expansion declares
  it) with `ModuleIdLt::new(db, krate, block, Some(parent), name)`. Invariant, to be written
  at each `to_static`: the id goes only into values of this crate's resolver queries and the
  `DefMap` assembled from them; the creating query is keyed by content that includes the parent
  and the declaration, so every revision in which any of those values is live re-creates the id
  with the same fields in the same query, and salsa keeps it; a value naming it is recomputed
  whenever the creating query is.
- **Round 0 reads** (hazard 1), each in the key or through a backdating query:
  inherited textual scope (`Decl::inherited`); the crate env as of the declaration
  (`CollectEnv::krate`, from the root's position); ancestors' module bindings and children so
  far (`CollectEnv::ancestors`, what `pub(in path)` and case-1 paths read); earlier siblings'
  subtrees through `decl_handle` (`Rows::Decl` rows); the root's own scope after each child
  through `subtree_macro_exports` (Splice, root only); a `#[macro_use] mod`'s macros through
  `legacy_exports`; the module's own unresolved imports (`Sink::find_unresolved_named`); other
  crates' def maps (final). Blocks: one unit, the maps around are final.

- **`NameRecords`** (`DefMap::names`, `nameres/uses.rs`, lane db-expand-names): the old
  `finish` builds it from every import's final status, `resolved_macro_paths`,
  `derive_helper_uses` and `not_introducing`. Here: `DeclOp::ResolvedMacroPath` (pushed where
  the old code pushed `resolved_macro_paths`: `resolve_macros` in `macros.rs`, case 1 of
  `collect_macro_call` in `collect.rs`), `DeclOp::DeriveHelperUse` (`macros.rs`),
  `DeclOp::NotIntroducing` (`collect.rs`, extern crates), and `SccState::imports` at the
  `Final` round (`unit.rs`: every member import with `Some(status.namespaces())` or `None` when
  unresolved). The driver ports `name_records` over the assembled map: imports in round then
  module then import order, the record ops in log order.

## 4. The sink

`env.rs` holds `Sink`, `CollectCx`, `SideTables`, `CrateEnv`, `CollectEnv`, `Ancestor`,
`ChildRequest`, `ChildMode` (written; read them). A collection never mutates a `DefMap`
directly: every mutation is `sink.apply(op)`, after which `sink.def_map()` reflects it for
the modules the sink holds.

## 5. File APIs (frozen between core's children; change only by agreement, tell core)

### `collect.rs` (child `frontend-nameres-core-collect`)

```rust
/// The constants of a collection of `krate` (`collect_defs`' prologue): deps, cfg, the crate's
/// own proc macros (read with the root file's syntax context), the fast-path flag.
pub(crate) fn cx<'db, 'f>(db: &'db Db, frame: &'f Frame, krate: Crate, in_block: bool) -> CollectCx<'db, 'f>;
pub(crate) fn crate_data(db: &Db, krate: Crate) -> Arc<DefMapCrateData>;
pub(crate) fn root_decl(db: &Db, frame: &Frame, krate: Crate) -> DeclKey;
pub(crate) fn module_decls(db: &Db, frame: &Frame, key: DeclKey) -> ModuleDecls;
pub(crate) fn decl_row(db: &Db, key: DeclKey, module: ModuleId) -> Option<ModuleData>;
/// `ModCollector { .. }.collect(items, container)` through `sink`.
pub(crate) fn collect_items<'db, S: Sink<'db>, A: Allocator + Clone>(cx: &CollectCx<'db, '_>, sink: &mut S, side: &mut SideTables<A>,
    module: ModuleId, tree: TreeId, macro_depth: u32, mod_dir: ModDir, items: &[ModItemId], container: ItemContainerId);
/// `collect_macro_expansion`.
pub(crate) fn collect_expansion<'db, S: Sink<'db>, A: Allocator + Clone>(cx: &CollectCx<'db, '_>, sink: &mut S, side: &mut SideTables<A>,
    module: ModuleId, call: MacroCallId, depth: u32, container: ItemContainerId, attr_macro_item: Option<AstId<ast::Item>>);
/// `seed_with_inner` (a block).
pub(crate) fn seed_block<'db, S: Sink<'db>, A: Allocator + Clone>(cx: &CollectCx<'db, '_>, sink: &mut S, side: &mut SideTables<A>, tree: TreeId);
/// The batched `expand_all` of `expand_round`.
pub(crate) fn expand_all(db: &Db, krate: Crate, calls: &[MacroCallId]);
```

`module_decls` of the root runs `seed_with_top_level`'s item part, `inject_prelude`,
`inject_std_extern_crate` first (as ops). `DeclSink::child` returns `ChildMode::Separate(key)`,
pushes `Splice { child, key }` after the child's `PushChild`, and, the sink of the root only,
applies `subtree_macro_exports(key)` to its own scope; after a `#[macro_use] mod` it applies
`legacy_exports(key)` as `DefineLegacyMacro` ops.

### `unit.rs` (child `frontend-nameres-core-unit`)

```rust
#[derive(Clone)] pub(crate) struct UnitState<A: Allocator + Clone = Global> { /* members' rows, Imports, GlobEdges, from_glob_import, upstream reads */ }
pub(crate) struct Unit<'db, 'f, A: Allocator + Clone = Global> { /* the view DefMap, LocalDefMap, state, the run's ImportQueues<'f>, Option<ReadCheck> */ }
impl<'db, 'f> Unit<'db, 'f, &'f Frame> {
    /// A block's unit: `def_map` is `block_def_map`'s empty map (`ModulesSlot::Building`),
    /// every module it holds is a member, no partition check.
    pub(crate) fn block(cx: &CollectCx<'db, 'f>, def_map: DefMap, local: &'db LocalDefMap) -> Unit<'db, 'f, &'f Frame>;
    /// The import fixed point (`resolve_imports` until `ReachedFixedPoint::Yes`).
    pub(crate) fn resolve_imports(&mut self);
    /// `collect`'s end: `record_resolved_import` of every still-unresolved import.
    pub(crate) fn record_unresolved(&mut self);
    /// The still-unresolved imports, in import order.
    pub(crate) fn unresolved_imports(&self) -> impl Iterator<Item = &ImportOp> + '_;
    pub(crate) fn def_map_mut(&mut self) -> &mut DefMap;
    pub(crate) fn into_def_map(self) -> DefMap;
}
impl<'db, A: Allocator + Clone> Sink<'db> for Unit<'db, '_, A> { /* apply = the old DefCollector's mutation, Import -> worklist, Macro -> pending */ }
pub(crate) fn local_scope(db: &Db, frame: &Frame, module: ModuleId, inherited: &Inherited, ops: &OpLog) -> BuildingScope;
pub(crate) fn scc_state(db: &Db, frame: &Frame, key: SccKey) -> SccState;
pub(crate) fn fact_handle(db: &Db, key: FactKey) -> salsa::Id;
pub(crate) fn unresolved_named(db: &Db, key: FactKey) -> Box<[(Name, ModPath)]>;
```

`scc_state(r, anchor)`, only for a component `round_active(r)` runs (else it stops, §9):
members = `partition(r).members(comp)` mapped through `round_modules(r).order`. Start: if the
component ran before (at `l = RoundModules::ran`) and `scc_state(l, anchor)` has the same
members, its state (quick path first: no member gains an op at `r`, the env and prelude are the
same, and every `upstream` entry is still that module's `fact_handle(fact_key(r, module))`:
return that state's `round`,
`handles` and `Arc` unchanged, no clone); else clone its `UnitState` (`// clone:` once per round
a component changes) and replay `module_round(r, M)` per member, wake every unresolved and
indeterminate import, re-pull every upstream glob source; else (merged or new) replay rounds
`0..=r` of each member in module order. Then the worklist; at `RoundKind::Final`, also
`record_unresolved`, fill `imports`, and censor the root of a proc-macro crate. A read of a
non-member: `PathRead::Slot(module, _)` with `module` of this crate and round not a member is
checked with `partition.may_read(comp, index(module))` before the read, else
`panic!("nameres facts: import `{path}` of {importer:?} reads {module:?}, which the partition does not put upstream")`.
Each member's handle: `ModuleScope::new(db, module, scope, children)` (created by this query).
If no member scope changed since the carried state, return the carried handles and `round`.

### `macros.rs` (child `frontend-nameres-core-macros`)

```rust
pub(crate) fn macro_step(db: &Db, frame: &Frame, key: FactKey) -> MacroStep;
pub(crate) struct RoundResult { pub(crate) progress: bool, pub(crate) deferred_eager: bool }
/// One `resolve_macros` over `pending` through `sink`: resolves each directive (all three
/// kinds, eager calls, the builtin-derive fast path and its deferral), retains the rest,
/// expands the resolved ones in one `collect::expand_all` batch, collects each expansion.
pub(crate) fn resolve_round<'db, 'f, S: Sink<'db>, A: Allocator + Clone>(cx: &CollectCx<'db, 'f>, sink: &mut S, side: &mut SideTables<A>,
    pending: &mut FrameVec<'f, MacroOp>, eager_with_errors: bool) -> RoundResult;
/// The first `Attr` directive's position, for `RoundOutcome::first_attr`.
pub(crate) fn first_attr<'p>(pending: impl IntoIterator<Item = &'p MacroOp>) -> Option<u32>;
/// `reseed_with_unresolved_attribute` of `pending[index]`.
pub(crate) fn reseed_at<'db, 'f, S: Sink<'db>, A: Allocator + Clone>(cx: &CollectCx<'db, 'f>, sink: &mut S, side: &mut SideTables<A>,
    pending: &mut FrameVec<'f, MacroOp>, index: u32);
/// `finish`'s unresolved-macro diagnostics for `pending`, as `Diagnostic` ops through `sink`.
pub(crate) fn unresolved_macro_diagnostics<'db, 'p, S: Sink<'db>>(cx: &CollectCx<'db, '_>, sink: &mut S,
    pending: impl IntoIterator<Item = &'p MacroOp>);
/// What a module has as a round starts: its last step's directives and tables, or what it
/// started with (§9).
pub(crate) struct Carried<'a>;
impl<'a> Carried<'a> {
    pub(crate) fn at(db: &'a Db, krate: Crate, modules: &RoundModules, at: u32) -> Carried<'a>;
    pub(crate) fn directives(&self) -> impl Iterator<Item = &'a MacroOp> + 'a;
    pub(crate) fn tables(&self) -> Arc<SideTables>;
}
```

`macro_step(r, M)` by `round_kind(r)`: `Normal` / `EagerWithErrors`: `resolve_round`;
`Reseed { module, index }`: `reseed_at` when `module == M`, else nothing (`progress = true`
for the reseeded module); `Final`: nothing. `StepSink`: own = `M`'s row with its scope cloned
from `fact_handle(fact_key(r, M))` (`// clone:` one module per step that has a directive; a step with no
pending directive and no reseed returns at once, cloning nothing) and the modules it declares
(`ChildMode::Inline`, ids created here); every other module through `Rows::Round(r)`.

### `driver.rs` (child `frontend-nameres-core-driver`)

```rust
pub(crate) fn collect_crate(db: &Db, frame: &Frame, krate: Crate) -> (DefMap, LocalDefMap);
pub(crate) fn collect_block(db: &Db, frame: &Frame, def_map: DefMap, tree_id: TreeId, local: &LocalDefMap) -> DefMap;
pub(crate) fn round_modules(db: &Db, frame: &Frame, round: RoundKey) -> RoundModules;
pub(crate) fn module_round(db: &Db, frame: &Frame, key: FactKey) -> ModuleRound;
pub(crate) fn import_shape(db: &Db, frame: &Frame, round: RoundKey) -> ImportShape;
pub(crate) fn round_env(db: &Db, round: RoundKey) -> CrateEnvKey;
pub(crate) fn round_progress(db: &Db, round: RoundKey) -> RoundOutcome;
pub(crate) fn round_kind(db: &Db, round: RoundKey) -> RoundKind;
pub(crate) fn round_row(db: &Db, round: RoundKey, module: ModuleId) -> Option<ModuleData>;
```

`round_kind(0) = Normal`; for `r >= 1`, with `o = round_progress(r-1)`, `k = round_kind(r-1)`:
`o.progress` -> `Normal`; `k == Normal && o.deferred_eager` -> `EagerWithErrors`;
`k != Final && o.first_attr = Some((m, i))` -> `Reseed { module: m, index: i }`; else `Final`.
A `Reseed` round's outcome has `progress = true`. Past `FIXED_POINT_LIMIT` (8192) rounds,
`tracing::error!("name resolution is stuck")` and `Final`, as the old loop.
`collect_crate`: loop `r` until `round_kind(r) == Final`, then assemble: `Modules` made once
from `round_row`-like rows whose scope is `ScopeSlot::Collected(fact_handle(final, M))`,
diagnostics (every log's `Diagnostic` ops in round then module order, then the final steps'
unresolved-macro diagnostics, then unresolved-import diagnostics from the final `SccState`s'
`imports` filtered by the `UnresolvedExternCrate` names), `names` (`NameRecords`, §3),
`diagnostics_by_module`, `derive_helpers_in_scope`
from the last side tables, `session_macros` from any `SessionMacros` op, the
deferred-derives `always!`, `LocalDefMap` from the final `round_env`. `collect_block`: the old
`collect_defs` loop on one `Unit::block` (seed, imports, `resolve_round`, eager round,
reseed, `record_unresolved`, diagnostics, `hand_over_scopes`).

## 6. What differs from the old collector

Only nameres.md §8's three. Named cases: (1) column order of `ItemScope` follows component then
module order; (2) two globs bringing different items of one name in different components: the
upstream one is applied first; an import resolved in round `r` keeps its answer when a later
round's expansion changes its target (as before); the reseeded attribute is the first in module
order, then position; (3) a macro whose path goes through a name another module's expansion adds
in the same round resolves one round later; a `#[macro_export]` from a non-root module's
expansion reaches the root's scope at the next round.

Not differences, but where the old end-of-collection steps moved. `DefMap::shrink_to_fit`'s
`settle_glob_ambiguities` runs in `scc_state` at the `Final` round, after the proc-macro
censor, for each member with an ambiguity. A crate's scopes are handed over there, so they
never reach `shrink_to_fit`. A block still settles in `shrink_to_fit`. The partition check
covers `PathRead::Slot` reads only. Two `Unscoped` reads also read a scope fact, unchecked:
`declared_enum_visibility` (the enum's container's whole scope) and the test-harness check
(the parent's children). They hold by transitivity. The binding the path reached came through
a chain of imports from the container, each one an edge of the graph. The path reached the
harness module through its parent. So either module is a member or upstream. A miss shows up
as a salsa cycle, not a wrong answer; the differential tests hunt it. Only the first
`#[prelude_import]` holder is used.

## 7. Speed

Per edit, nameres.md §6 and its speed-of-light. Core's costs on top: one `DeclKey`/`FactKey`
intern per module and round (a hash probe each); `scc_state`'s quick path compares
`upstream.len()` handle ids per unchanged component (no clone, no allocation); a changed
component clones its `UnitState` once per round (`O(members' scope sizes)`, the term to cut
with copy-on-write rows if measured); a macro step clones one module's `ItemScope` only when
the module has a pending directive. State of the art: incremental Datalog stratification
(differential dataflow) with salsa's red-green edges as the fact table; no other Rust resolver
memoizes below the crate.

## 8. What proves it

| claim | test |
|---|---|
| replaying a round-0 log gives the scope collecting left | `collect.rs` tests: `local_scope(module_decls(k).ops)` equals the `DeclSink`'s own scope |
| a one-member unit equals the old collector on single-module fixtures | `unit.rs` tests, against `collector::collect_defs` |
| a read the partition forbids stops, naming the import | `unit.rs` test with a hand-built `Partition`-violating read (`#[should_panic(expected = "nameres facts: import")]`) |
| the round kinds follow the old loop | `driver.rs` tests: eager-with-errors and reseed fixtures |
| the final scopes' glob ambiguities are settled as the old end of collection settled them | `driver.rs` `glob_ambiguities_are_settled_as_the_old_collector_settles_them` (the projection compares `glob_ambiguities`, `shadowed_imports`, `underscore_imports`) |
| crate-wide equality, trace | `facts/tests/` (frontend-nameres-diff) |
| a round runs only the modules it can change, and the map stays the old collector's | `facts/tests/active.rs` (§9) |
| a component a round does not run keeps its facts, named by the round they changed at | `unit.rs` `an_unchanged_component_keeps_its_state` |
| asking the state of a component the round does not run stops | `unit.rs` `the_state_of_a_component_the_round_does_not_run_stops` (`#[should_panic]`) |

## 9. The active set

A round after round 0 runs only over the modules it can change (request §1 of
`v2/requests/frontend-nameres.md`). Same facts, same results: the assembled `DefMap` does not
change, only which queries run.

**What `round_modules(r)` knows before the fixed point.** Per module, `RoundModules` holds:

- `STEP`: its macro step runs at `r`. At a `Normal` or `EagerWithErrors` round, a module steps
  when it has directives. At `Reseed { module }`, only that module steps. At `Final`, none does.
- `GAINS`: `module_round(r, M)` may have ops. At round 0 every module gains. Later, a module
  gains when its own step of `r-1` made a resolution op in it or a step of `r-1` declared it.
  The root also gains when another step of `r-1` made an op in it.
- `PENDING`: it has directives as `r` starts.
- `active`: the modules with `STEP` or `GAINS`, ascending.
- `last_step`: the round of its last step.
- `ran`: the last round its component ran at.
- `since`: the last round its facts changed at, found by comparing `fact_handle(r-1)` with
  `fact_handle(since)` for each module whose component ran at `r-1`.

The columns are read off the steps of `r-1` that ran, so a module that no step touched costs a
copy of its columns and no query. A module gains no directive after it starts, because
`clone_resolution_op` and `clone_root_op` carry no `Macro`. So a module that does not step keeps
its directives, and `macros::Carried::at` finds them: from its last step, or from what it
started with (its round-0 log, or `module_round` of the round it was declared at).

**Which components run: `round_active(r)`** (kind `0x2218`). It is its own query, not a column
of `RoundModules`, because it needs `partition(r)`, which is built from `round_modules(r)`, and a
merge can make a component run that no op or directive names. Every component runs at round 0,
at `Final`, when `round_env` changes, and when the prelude holder or the prelude it resolves
changes. Otherwise components are visited upstream first, and one runs when:

- a member gains ops or is new;
- its members are not those it had when it last ran;
- a module in the `upstream` of its state from the last round it ran at is in a component that
  runs.

This is the cone of what can change, a superset taken from the reads actually made. The quick
path still catches a component whose reads came back the same.

The prelude holder's component is upstream of every other and reads none. So whether it runs
needs only `GAINS` and its members (`unit::component_runs`, without `round_active`), and
`round_active` may read its `scc_state(r)` to compare the prelude. A unit of that component that
read a module outside it would show as a salsa cycle, as other unchecked reads do (§6).

**Readers of an idle module.** A module whose component does not run keeps its facts. Every
reader names them by `queries::fact_key(r, at)`, which is `FactKey(r)` when the component runs
and `FactKey(since)` otherwise, so the read hits the memo that made them instead of a new key per
round. The readers are `round_row`'s scope slots, `scc_state`'s upstream checks and handles, and
the `StepSink`'s own scope. A step whose component does not run reads its unresolved imports at
`ran_by(r, at)`. `scc_state`, `fact_handle` and `unresolved_named` of a component the round does
not run stop with `"... a component the round does not run"`, so a missed reader shows at once
and never makes a new key per round.

**Only active modules run.** `macro_step` runs only for `STEP` modules. Every reader
(`round_modules`, `module_round`, `round_env`, `round_progress`, `collect_crate`'s crate ops)
iterates `RoundModules::stepping`. `module_round` runs only for `GAINS` modules: `import_shape`
copies the old shape of any other module, and `replay_round` and the quick path skip them.
`unresolved_named` is asked only by a step.

**Round 0's macro queries.** `OpLog` keeps two flags, read off its ops once by `OpLog::from_frame`. `has_children` means a
`Splice`. `writes_elsewhere` means a scope op of another module than the log's own, which in a
round-0 log is a `#[macro_export]` or proc macro into the root. `subtree_macro_exports` returns
at once for a log with neither flag. It reads a child without children of its own in place, and
asks the query only of a child that has children. The root still asks it of each child, because
that is the firewall between the root's collection and the subtrees. `legacy_exports` was
already asked only of `#[macro_use]` children. `child_key` is a look-up in `module_decls`'
children instead of a query. Its kind `0x2210` stays unused.

**What still runs per round and module.** A `row` execution for each module an active view
reads, and `RoundModules`' `O(m)` copy of its columns.

## 10. Frames

What a query makes and drops within its run goes in a `blaze_mem` frame. What it returns, and
what one query carries to the next, stays owned and exact (`v2/requests/mem-db-def-collect.md`,
Part C).

**Who opens a frame.** Each tracked fn in `queries.rs` that has temporaries opens its own
`frame::task(Tag::Nameres, |frame| ..)` and passes `frame` to its body. That is `root_decl`,
`module_decls`, `round_modules`, `module_round`, `import_shape`, `partition`, `scc_state`,
`macro_step`, `decl_handle` and `subtree_macro_exports`. `collect_crate` and `collect_block` take the
frame of `crate_local_def_map`/`block_def_map`. Nothing in `facts/` calls `frame::current`: salsa
opens no frame, and a query must not depend on its caller's. A query that another query calls
gets a frame of its own, nested, so the inner reset frees only the inner query's memory. Queries
with nothing to free open none (`round_env`, `legacy_exports`, `round_progress`, `round_kind`,
`row`, `fact_handle`, `unresolved_named`). `CollectCx<'db, 'f>` carries the frame to the
collection, and its `deps` are a frame list of `(Name, &'db BuiltDependency)`, looked up by
`cx.dep(name)`.

**The aliases.** `facts.rs` has `FrameVec<'f, T>`, `FrameMap<'f, K, V>` and `FrameSet<'f, K>`
(hashbrown in the frame, with the Fx hasher), and `map_in`/`set_in`, as in
`hir-ty-mir/src/mir/framed.rs`. `facts/` does not depend on hir-ty-mir.

**Owned results, at exact size.** A result is a frame list copied once, either
`frame_vec.into_iter().collect::<Box<[_]>>()` or `Box::from(&v[..])` for `Copy` items. A
filtered list is counted first and then filled to its capacity. `OpLog { ops: Box<[DeclOp]> }` is
made once by `OpLog::from_frame`, which reads `owner`, `has_children` and `writes_elsewhere` off
the ops. Its producers are `DeclSink::finish`, `StepSink::into_step` and `module_round`. A def map's
diagnostics, derive helpers and `NameRecords` are gathered in the frame and copied once at the
end of `collect_crate`/`collect_block`.

**Carried vs. per run.** `UnitState<A = Global>`, `SideTables<A = Global>`, `Imports<A>`,
`ImporterLists<K, T, A>` and `GlobEdges<A>` take their allocator as a type parameter. Where a value
carries them (`SccState.state`, `ModuleDecls.side`, `MacroStep.side`) they are `Global`. A block's
are in the frame (`SideTablesIn<'f>`, `Unit<'db, 'f, &'f Frame>`), and so is `local_scope`'s.
`ImportQueues<'f>` (`unit/imports.rs`) is one run's worklist over the imports: the pending and
deferred queues, the waits and their edges, the wake keys, and also `generation`, `queued`,
`phase` and `free_edge`. A `Unit` makes it from `Imports::len()` and drops it at its end. That gives
the same results as carrying it, because a run's fixed point ends with every queue empty, and the
next run attempts every import first: a fresh unit pushes each one, and a carried one calls
`wake_all` after its replay.

**The graph.** `ImportGraph<'f>` and `Solver<'a, 'f>` live in the frame. `from_edges` is
`Csr::new_in(frame)` with `rebuild_graph` (its scratch in the frame too), then `dedup_rows`, and
`components` groups the members with `Csr::rebuild` over the frame `stack` as cursors. The DFS stack
entry is `DfsFrame`. `partition`'s only global allocations are the three columns of `Partition`
(`PARTITION_ALLOCATIONS = 3` in `facts/tests/graph.rs`).

**Still global.** No temporary: the reused buffers (`eager_calls`, `name_records`' `found`) are in
the frame, as `macro_call_as_call_id` and `DefMap::path_uses*` take any allocator. The final
`def_map.shrink_to_fit()`, `local.shrink_to_fit()` and `Arc::make_mut(data).shrink_to_fit()` stay
until A3/A4/A5's `publish`/`settle` (deferred with R10). `DefMap::shrink_to_fit` also settles
glob ambiguities and scopes.
