---
status: plan
summary: Push-based incremental name resolution, impl tables and trait candidate sets (lane dbsp). The relations are views maintained on the write side by deltas, DBSP-style, and published as salsa inputs, one per slice, so an edit costs work proportional to the facts it changes and a reader sees a change only when its slice changed.
date: 2026-10-30
---
# dbsp: resolution, impls and trait candidates maintained by deltas

Lane dbsp (Andrew approved, 2026-10-30). Notes: `docs/v2/plan/lanes/dbsp/` (speed of light,
children's notes). Report: `/tmp/blaze-lanes/v2-dbsp.md`.

## 1. Why

Name resolution is already per module (`crates/hir-def/src/nameres/facts/`,
[lanes/frontend/nameres.md](../lanes/frontend/nameres.md)). Each module's round-0 collection is
a query, imports resolve per SCC, and each name is a backdating memo. It is still pull-based,
so every edit pays crate-wide terms even when one fact changed:

- **the round driver**: `round_modules`, `import_shape`, `partition` and `round_active` are
  each `O(m)` per round, and `crate_local_def_map` reassembles all `m` modules;
- **every reader of the whole `DefMap`**: `TraitImpls::modules_by_trait`,
  `InherentImpls::modules_by_self_ty` and `blaze-items`' `traits_in` loop over every module.
  After an edit, salsa re-verifies each of them and every query that read them;
- **verification**: each memo of the `O(k·m)` round queries is checked, about 50 ns each.

Adding one `impl` should cost the impl's own facts: one row inserted into one trait's impl
set, and one input set for that slice.

## 2. The model

DBSP (Budiu, McSherry, Ryzhyk, Tannen, "DBSP: Automatic Incremental View Maintenance", VLDB
2023) treats a view as a circuit over streams of Z-sets (multisets with integer weights). Its
incremental form takes the input delta and produces the output delta. Linear operators (map,
filter, join with an arrangement) are their own incremental form. A non-linear operator
(aggregate, distinct) is recomputed only for the groups the delta touches. Recursion becomes a
nested fixpoint over deltas. We use this model with three choices:

1. **Write-side maintenance.** The views are maintained when an input is written, not when a
   query reads them. Every `Db` write (`set_file_text*`, `set_view_root`, the crate graph, the
   source roots) ends by running the *maintainers* (§3). A maintainer reads the pure queries it
   needs (item trees, round-0 logs, expansions), updates its state by the delta, and sets the
   salsa inputs whose slice changed. Queries never read maintainer state, only the inputs. So
   every query stays a pure function of salsa inputs, and a reader is invalidated exactly when
   its slice is.
2. **One input per slice.** Each relation is published as one salsa input per slice: a module's
   scope, a trait's impl set in a crate, a self-type head's inherent impls, the traits with an
   associated function of a name. Each slice holds its content and its 128-bit hash, and is set
   only when the content changed. This is the "one input per relation version" of the brief.
3. **The collector's rules, maintained.** Name resolution keeps the old collector's rules,
   which `facts/` also keeps: `collect.rs`, the unit's `update` and glob propagation, and
   `macros.rs`' `resolve_round`. Only the schedule changes again. Instead of rounds, a
   persistent worklist runs over a persistent state, with inserts propagated forward and
   deletes handled by DRed (§6).

Prior art, and where each lands:
- DBSP: integration and differentiation of the circuit, nested fixpoints. The glob and macro
  fixpoints are nested circuits, and the per-module scope rebuild is an aggregate.
- Differential dataflow (McSherry et al., CIDR 2013): arrangements shared by operators. The read
  index and the impl arrangements are arrangements.
- DRed (Gupta, Mumick, Subrahmanian, SIGMOD 1993): over-delete, then rederive, for recursive
  views under deletion. Counting (the same paper) is not enough here: glob cycles are
  self-supporting. Backward/Forward (Motik et al., AAAI 2015) is the refinement if over-delete
  regions turn out large.
- Datalog name resolution: "Scopes as types" (van Antwerpen et al., OOPSLA 2018), Hack's and
  Flow's incremental servers (per-file facts, a reverse dependency index, a re-check of the
  readers of changed names), and rustc's resolver and coherence as the semantics to match.
  Polonius and chalk are the precedent for Datalog inside the Rust front end.

## 3. Where it runs: maintainers in `base-db`

`crates/base-db/src/maintain.rs` (child `dbsp-engine`):

```rust
/// What one write changed: what a maintainer reads to find its delta.
pub struct Written<'a> {
    pub set: &'a [FileId],           // text changed
    pub added: &'a [FileId],
    pub removed: &'a [FileId],
    pub roots: &'a [SourceRootId],   // file sets changed (a `mod` probe may now hit)
    pub crates: bool,                // the crate graph, cfg, env or proc macros changed
}
/// A write-side view maintainer, registered with `inventory::submit!` by the crate with the code
/// (as `hir-ty/src/providers.rs` registers MIR): nothing installs anything.
pub struct Maintainer { pub order: u16, pub name: &'static str, pub run: fn(&mut Db, &Written<'_>) }
inventory::collect!(Maintainer);
impl Db {
    /// Runs `f` with this database and its write-side state `T` (made by `Default`); the state is
    /// taken out for the call, so `f` may read queries with `&*db` and set inputs with `db`.
    /// No query can reach it.
    pub fn with_state<T: Default + Send + Sync + 'static, R>(&mut self, f: impl FnOnce(&mut Db, &mut T) -> R) -> R;
    /// Runs `f` with maintenance held back, then runs each maintainer once over everything `f`
    /// wrote. Every write method outside a batch is its own batch.
    pub fn batch<R>(&mut self, f: impl FnOnce(&mut Db) -> R) -> R;
}
```

- Maintainers run in `order`: hir-def name resolution (`100`), then hir-ty impls and candidates
  (`200`). A later one reads the earlier one's delta from its state (`Db::with_state` of the
  earlier one's public `LastWrite` type).
- Within a maintainer, crates go in dependency order (`CrateGraph` topological). A crate reads
  its dependencies' published inputs, set just before.
- **No query reads a maintainer's own crate's published inputs during that crate's
  maintenance.** This is the old collector's rule (collection never reads its own def map). The
  published inputs are set only after the crate's delta has been fully applied.
- A crate served by the frozen image (`nameres/frozen.rs`, `method_resolution/frozen.rs`) is
  never maintained: its inputs are set once, from the image.
- A crate first seen is maintained from the empty state. Its delta is "insert everything", and
  it runs the same code as an edit. This is the cold run, and the differential tests compare
  against it.
- A panic in a maintainer drops that crate's state (it is out of the map during the call), and
  the panic goes on out of the write. The next write maintains the crate from empty. This is
  the cold run again, not another path.

## 4. The relations

All keys are interned `u32`s (`ModuleId`, `Name`, `ImplId`, `TraitId`, `FileId`). The table says
which are base (EDB, read from pure queries) and which are derived (IDB, maintained).

| relation | kind | key → value | from | published as |
|---|---|---|---|---|
| `Log(m, pos) → op` | EDB | the round-0 op log of module `m` (`DeclOp`), position-keyed | `module_decls(DeclKey)` | — |
| `Expansion(d, pos) → op` | EDB | the ops of the expansion of directive `d` | `collect::collect_expansion` of the resolved call | — |
| `Directive(m, d)` | EDB/IDB | a macro directive pending in `m` (`MacroOp`), from a log or an expansion | logs | — |
| `Resolved(d) → MacroId` | IDB | the macro a directive's path resolves to | `resolve_round`'s rules over `Binding` and textual scopes | — |
| `Import(m, i)` | EDB/IDB | a use tree of `m` (`ImportOp`) | logs | — |
| `ImportResult(i) → PerNs` | IDB | what import `i` resolves to, with its read set | path resolution over `Binding` | `NameRecords` |
| `GlobEdge(src, dst, vis, i)` | IDB | glob import `i` of `dst` reads every visible name of `src` | `ImportResult` of glob imports | — |
| `Candidate(m, ns, n) → (def, vis, import, kind)` | IDB (Z-set) | every source that may bind `n` in `m`: declared item, plain import, glob edge, `#[macro_export]`, expansion | the above | — |
| `Binding(m, ns, n) → PerNs` | IDB (aggregate) | the collector's `update` rules over the candidates: explicit over glob, `max` visibility, glob ambiguity | aggregate per module | `ScopeInput(m)` |
| `Legacy(m, n) → [MacroId]` | IDB | textual `macro_rules!` scope | logs, `#[macro_use]` | `ScopeInput(m)` |
| `Child(m, n) → m'`, `Module(m) → row` | IDB | the module tree | logs, expansions | `CrateResolution(krate)` |
| `ImplIn(m, impl)` | IDB | impls (trait, inherent, reservation, builtin derive) per module | `DefineImpl` ops | — |
| `ImplKey(impl) → (trait?, head)` | IDB | the impl's trait and simplified self type, with its read set | `impl_trait`, `impl_self_ty`, `simplify_type` | — |
| `ImplsOf(krate, trait) → rows by head` | IDB (arrangement) | trait impls by self-type head, blanket separate | `ImplIn ⋈ ImplKey` | `TraitImplsSlice(krate, trait)` |
| `InherentOf(krate, head) → impls` | IDB (arrangement) | inherent impls by head | `ImplIn ⋈ ImplKey` | `InherentSlice(krate, head)` |
| `AssocFn(trait, n)` | IDB | the trait has an associated function (or const or type) named `n` | `TraitItems` | — |
| `TraitsWith(krate, n) → [trait]` | IDB (arrangement) | trait candidates for method name `n` | `AssocFn` | `CandidateSlice(krate, n)` |
| `ImplSetHash(krate, trait)` | IDB | the 128-bit hash of `ImplsOf(krate, trait)` (for lane item-hash) | the slice | the slice's `hash` field |

Order is data. Each op keeps its *position*: `(m, 0, index)` for round 0, or
`(m, directive position, index)` for an expansion. Positions compare lexicographically, which
is the textual order of the source after expansion. A module's scope columns are built in
position order. That makes the order a function of the program, never of the history of edits.

## 5. A file edit as a delta

1. **Affected modules.** The state keeps an index from each file to the modules whose
   definition is in it, and to the directives that read it (`include!`). A `set` file maps to
   those modules and directives. An `added`/`removed` file or a changed root re-checks the
   modules of that source root that declare an out-of-line `mod`, since its path probes may now
   hit or miss. That is `O(modules with out-of-line mods in the root)`, and only on a file add
   or remove. `crates` re-checks every module of the crate.
2. **Round-0 logs.** For each affected module, ask `module_decls(key)`, where `key` comes from
   the parent's new log (children first in DFS order). If the key or the log differs from the
   stored one, diff the logs. Equal prefix and suffix are skipped. The middle is a multiset diff
   over op hashes (FxHash of the op's fields), giving deletions and insertions with their
   positions. Ops of untouched items are equal because item IDs are AST-ID based (stable under
   insertion before them). A child whose key changed is diffed against its old log in the same
   way. A child no longer declared is deleted with its subtree. A new child is inserted with its
   subtree.
3. **Crate-level ops** (`ExternPrelude`, `MacroUsePrelude`, `Prelude`, `ProcMacroFn`,
   `ExportedDerive`): a change to these is a change of the crate env. Every directive and import
   that read the env (by name: the extern prelude and the macro-use prelude are keyed by name in
   the read index) is woken.

The delta is the Z-set `ΔLog`. Everything below is the incremental circuit applied to it.

## 6. Maintaining the fixpoint under insertion and deletion

The state of a crate (`crates/hir-def/src/nameres/delta/state.rs`):
- per module: its row, its ops by position, its `Candidate` Z-set per `(ns, name)`, its built
  `BuildingScope`, its side tables and pending directives;
- per import: its `ImportOp`, its status (`PartialResolvedImport`), and its read set;
- per directive: its op, its resolution, its expansion's positions, and its read set;
- the **read index** `(m, n) → readers` (imports, directives, glob edges, the env's names),
  where a negative read (the name is absent) is indexed like a positive one;
- `GlobEdge`s, both ways.

**Insert (semi-naive).** An inserted op goes through the collector's own mutation: `update` for
`Declare`/`Update`, which propagates to glob importers along `GlobEdge`; the define ops for
impls and the rest; the worklist for `Import` and `Macro`. Every `(m, ns, n)` whose `Binding`
changed wakes its readers in the read index. A woken import or directive re-resolves against the
current state, and its old result's candidates are replaced by the new ones (a delete and an
insert). This is semi-naive evaluation: only new facts drive the next step.

**Delete (DRed).** A deleted op, a retracted import result or a retracted expansion:
1. *Over-delete.* Remove its candidates. Then, transitively through the read index and
   `GlobEdge`, mark every derived candidate whose support read a removed binding. The marked set
   (the *region*) is closed under "read something in the region". Glob cycles are inside it
   whole, so self-support can't keep a deleted name alive.
2. *Rederive.* Rebuild each region module's `Binding`s from its surviving `Candidate`s, in
   position order (the aggregate). Then re-run the region's imports, glob edges and directives
   with the collector's worklist, reading modules outside the region as they are. A region
   module whose final `Binding(n)` differs from its value before the edit wakes the readers of
   `(m, n)` outside the region, which join it, and the step repeats until nothing outside
   changes. This is early cutoff at name granularity.

**Macros.** A directive's expansion is a sub-log whose positions sit under the directive's.
When a directive's resolution changes, or the directive is deleted, its expansion's ops are
deleted, recursively for the directives the expansion held, and the new expansion's ops are
inserted. Textual `macro_rules!` scopes are positional: a directive reads the legacy scope at
its position. The read index keys legacy reads by `(m, n)` as well, so a new `macro_rules! n`
before a directive wakes it. Expanding stays batched (`collect::expand_all`) per worklist step,
so independent expansions run in parallel (`Db::run_all`).

**Quiescence phases.** When the worklist is empty:
1. eager calls deferred for unresolved arguments are expanded with errors (the old
   `EagerWithErrors`);
2. every attribute directive still unresolved is reseeded as a plain item;
3. the worklist runs again.

These repeat until nothing changes. Then the final phase runs. It is a pure function of the
quiescent state: the still-unresolved imports' diagnostics, the proc-macro root censor,
`settle_glob_ambiguities` and `NameRecords`. Each phase's outputs are keyed by what they read
(the directive or import), so a later edit retracts them like any other derived fact.

**Why the result is history-independent.**
- *Accepted programs.* For a program rustc accepts, resolution has one fixpoint. rustc's
  determinacy rules and its E0659 time-travel and ambiguity errors exist to make that so. A
  maintained fixpoint where every import and directive is resolved against the final state is
  that fixpoint, whatever order the deltas came in.
- *Glob ties.* For the one order-dependent rule in `update`, two globs bringing different items
  of one name, the binding is the candidate first in position order, flagged ambiguous. It is
  not the first to arrive. So a fresh run and any edit sequence agree.
- *Rejected programs.* For programs rustc rejects, the first error (its code and primary span)
  is the same. What may differ is listed next.

**What differs from `facts/` (and so from the old collector):**
1. Column order is position order, not component order. The tests compare the canonical
   projection, as `facts/tests/differential.rs` does.
2. Two globs with different items of one name: the first in position order is chosen, not the
   upstream component's. The name is ambiguous either way, and rustc rejects any use of it
   (E0659).
3. An import that the old schedule resolved before a later expansion changed its target is
   resolved against the final state here, never kept stale. rustc *accepts* these programs and
   binds the expanded item. A glob binding read through a module path is undetermined while
   the glob's module has unexpanded invocations (`rustc_resolve` `ident.rs`
   `resolve_ident_in_module_globs_unadjusted`, `DeclKind::determined`), so the import waits.
   The shared unit gets rustc's rule (queue 00882): such an import is indeterminate, not
   resolved, until its module's directives are gone. This makes `facts/` agree too, so this is
   a fix there, not a difference.
4. Unresolved attributes are reseeded all at once at quiescence, not one per round. The first
   "cannot find attribute" error, in module then position order, is the same.

## 7. Impl tables

Maintainer `200`, in `crates/hir-ty/src/method_resolution/delta.rs` and `delta/`.
- **Input deltas**, from hir-def's `LastWrite`:
  - `ImplIn` insertions and deletions;
  - the `(m, n)` bindings that changed;
  - the files written.
- **`ImplKey(impl)`**: `impl_trait(impl)`'s trait id and
  `simplify_type(impl_self_ty(impl), TreatParams::InstantiateWithInfer)`, which is what
  `TraitImpls`/`InherentImpls` index by today. Its *read set* is the `(m, n)` bindings and the
  type-alias and trait items that resolving the header's paths reaches, transitively through
  aliases. The maintainer indexes the read set (`mentions: (m, n) | item → impls`).
- **Which impls are recomputed:**
  - the impls of `ΔImplIn`;
  - every impl whose header is in a written file;
  - every impl whose `mentions` meet a changed binding;
  - every impl whose `mentions` meet an item of a written file.

  Each one's `ImplKey` is recomputed through the pure queries. A key that changed is a delete
  and an insert in the arrangements. Exactness rests on the read set covering every input of
  `ImplKey`. Tests check it: after each edit, every impl's key is recomputed from scratch and
  compared.
- **Arrangements:**
  - `ImplsOf(krate, trait)`: rows by head hash, as `TraitImpls`' columns, plus blanket impls and
    builtin-derive impls;
  - `InherentOf(krate, head)`.

  Each row keeps its impls in position order: module position, then the impl's op position,
  which is the order `for_crate` holds them in today.
- **Published slices:**
  - `TraitImplsSlice(krate, trait) { impls: TraitImpls (that trait alone), hash: u128 }`;
  - `InherentSlice(krate, head) { impls: Box<[ImplId]>, hash }`.

  `TraitImpls::for_crate_and_trait` and `InherentImpls::in_crate_for_self_ty` read the slice
  input and nothing else. `modules_by_trait` and `modules_by_self_ty` go. `for_crate` stays for
  the frozen image's writer and reads the slices.
- **Impls in body blocks.**
  - The maintainer handles them too. For every module whose file was written, it recomputes
    the module's body-block tables (`trait_bodies`, `inherent_bodies`, pure per module) and
    diffs the traits and heads they hold.
  - The slice of each trait (or head) keeps its "modules with body impls" rows. Readers merge
    them in module order, as `for_crate` does today.
  - So no reader loops over a crate's modules, and a body edit that adds no impl sets no slice.
- **Slices for absent keys.** Asking for a slice that has no input yet (no impl of that trait)
  must still depend on something, or it would miss the first impl added. So each `(krate,
  trait)` and `(krate, head)` slice is an input made on first set. A reader of an absent key
  reads the crate's `SliceDirectory(krate)` input, a map from key to slice that changes only
  when a key gains its first row or loses its last. A reader then depends on the directory and,
  once its slice exists, on the slice alone.
- **Impl-set hash** (for lane item-hash): `hash` is `ContentId` (BLAKE3, domain
  `"blaze v2 impl set 1"`) over the slice's rows in order. Each row is the impl's `DefPathHash`
  (item-hash's stable item identity), its head and its trait. A coherence or trait-solving
  result keys on the hashes of the slices it read, never on the crate.

## 8. Trait candidate sets

- `AssocFn(trait, n)` comes from `TraitItems::of(trait)`. It is recomputed for traits defined
  in a written file and for traits in `ΔBinding`'s defs. The trait's item list changes only by
  its own source, or by a macro in it, which is in its file's expansion.
- `TraitsWith(krate, n)` is published as `CandidateSlice(krate, n) { traits: Box<[TraitId]>,
  hash }`, sorted by `TraitId`, with a `SliceDirectory` like the impls'.
- Method probing (`method_resolution/scope_sets.rs`, lane frontend-fast-method) intersects the
  traits in scope at a module, which `ModuleScope::traits` already gives per module, with
  `CandidateSlice(krate, n)` for each crate in scope. So adding a trait with method `n` reruns
  only the probes for `n` in modules that can see it.
- `blaze-items`' `traits_in` (by `NameHash`) reads `CandidateSlice` instead of looping over
  modules.

## 8b. Item hash columns (queue 00868, lane items builds it)

Today `blaze_items::crate_hashes(krate)` (`blaze-items/src/table.rs`) is one query over every
item. Any changed hash reruns it: 4 hash memos read per item and every column rewritten. For
50,000 items that is about 200,000 memo reads and 8 MB per body edit, and `changed_since`
merges two builds. Maintained form, as maintainer `300` in `blaze-items`:

- **State**: the crate's `HashColumns` in key order, stored as **chunked copy-on-write
  columns**. Each column is a vector of `Arc`'d chunks of 4096 rows. A write copies only the
  chunks it changes, and publishing is `O(rows / 4096)` pointer copies. The state also holds
  two reverse indexes:
  - `uses: item → items whose interface or deep hash read its interface hash` (what
    `deep_of` folds in);
  - `names: (module, name) → items whose name-resolved tokens read that binding`.

  Both are recorded when an item is hashed, from the paths its tokens resolve.
- **Candidates** for one write:
  - hir-def's `CrateDelta::touched` (items of re-read modules);
  - the items `names` gives for `CrateDelta::bindings`;
  - the impls and traits in hir-ty's `method_resolution::delta::LastWrite` (impls whose
    `ImplKey` changed, traits whose associated names changed);
  - items added or gone.

  `Written::crates` is the delta from empty.
- **Apply**: recompute each candidate's `ItemHashes` through the pure queries (unchanged
  inputs make each a memo hit) and compare with its row. An item whose interface or deep hash
  changed pushes its `uses` readers onto a worklist. Deep hashes propagate breadth-first with
  early cutoff where a recomputed hash is equal. Rows change in place in their chunk. Rows for
  added or gone items are spliced into their chunk, which splits at 8192 rows and merges below
  1024.
- **Publish**:
  - `HashColumnsInput(krate) { columns, changed: Box<[ItemKey]> }`, where `changed` is this
    write's delta, so `changed_since` is the delta itself and needs no merge;
  - a reader that is behind by more than one write asks `changed_between(r0, r1)`, the union of
    the kept per-write deltas (a ring of the last 64 writes);
  - a reader further behind than the ring re-reads the columns whole. That is a reader catching
    up, not a fallback.
- **Proof**: after each write of the random edit scripts (§12), the maintained columns equal
  `crate_hashes` computed fresh, byte for byte. Adding a body edit costs 1 row and 1 chunk
  copy, plus deep-hash rows for the callers whose deep hash changes.

## 9. Publication into the `DefMap`

- `ScopeInput(m) { scope: ItemScope, children: FxIndexMap<Name, ModuleId>, diagnostics }`, a
  salsa input per module, set when the module's built scope, children or diagnostics changed.
  `ScopeSlot` gains `Input(salsa::Id)`. `ModuleData::scope(db)` reads it. A per-name tracked fn
  `scope_name(input, n)` backdates, as `ModuleScope::name` does.
- `CrateResolution(krate) { modules: ModulesMap (rows with ScopeSlot::Input), local:
  LocalDefMap, data: DefMapCrateData, names: NameRecords, crate_diagnostics }`. It is set only
  when the module tree, the crate env, the name records or crate-level diagnostics change, so
  not for an item added to a module.
- `crate_local_def_map(krate)` reads `CrateResolution(krate)`. It finds that input through the
  singleton `Resolutions { crates: FxHashMap<Crate, CrateResolution> }`, which changes only when
  a crate gains or loses its state. Then it builds the `DefMap` around the published `Modules`.
  A crate with no published resolution and no frozen image stops with an error naming the
  crate (no fallback).
- `block_def_map` is unchanged: it is per body, pull-based and local (`facts::collect_block`),
  and it reads the crate's published scopes.
- The facts resolver (`facts::collect_crate`) stays only as the differential reference, under
  `#[cfg(test)]`, as `collector` became for it.

## 10. Cost and speed of light

The arithmetic is in [../lanes/dbsp/speed-of-light.md](../lanes/dbsp/speed-of-light.md).
In short, per edit:
- the edited module's log is diffed: `O(ops of m)` hash compares at about 2 ns each;
- the delta's facts are applied: `O(1)` per op, plus `O(readers)` per changed binding;
- each set input costs one salsa set (about 100 ns).

Adding one impl:
- one `DefineImpl` op is inserted;
- one `ImplKey` is computed, which is the impl header's lowering, already its own work;
- one arrangement row is inserted;
- one `TraitImplsSlice` (and its hash) is set.

That costs no round query, no `DefMap` assembly and no module loop. The test
`adding_an_impl_costs_its_facts` asserts the counters: ops applied, imports and directives
woken, modules rebuilt, slices set, and that a 1000-module crate gives the same counts as a
10-module one.

## 11. Coordination

- **frontend-fast** (listed Datalog name resolution, [fast.md](../lanes/frontend/fast.md) §1):
  this design is that item's maintained form. fast.md §1 points here.
- **item-hash** (window `item-hash`): consumes `ImplSetHash` (§7) and `CandidateSlice::hash`
  (§8) as the global-table inputs of a coherence or trait key. The identity of an impl inside
  the hash is item-hash's `DefPathHash`-style stable id. Until item-hash publishes it, the hash
  uses the impl's `ImplLoc` content (container `DefPath` and `FileAstId`), and item-hash takes
  over through one function, `impl_identity(db, ImplId) -> ContentId`.
- **frontend-nameres** (`facts/`, DONE): its rules are reused through their `Sink` API
  (`env.rs`), and its resolver becomes the test reference.
- **db**: `base-db/src/maintain.rs`, write-method edits in `base-db`, `ScopeSlot::Input` and
  `crate_local_def_map` in `nameres.rs`. These are targeted edits, each told to the live db
  window.
- **items**: `method_resolution.rs` reads the slices. This is a targeted edit, told to the
  items window.

## 12. What proves it

| claim | test |
|---|---|
| maintained = fresh, after random edit sequences | `nameres/delta/tests/random.rs`: proptest edit scripts (add, remove or rename items, uses, globs, `mod`s, `macro_rules!`, derives, impls) over the nameres fixtures and generated crates. After each edit, the maintained `DefMap` (canonical projection) equals a fresh maintainer's and `facts::collect_crate`'s, modulo §6's list |
| impl sets and candidates = fresh | `method_resolution/delta/tests.rs`: the same scripts; every slice equals a from-scratch arrangement, every hash equal |
| adding one impl costs its facts | `adding_an_impl_costs_its_facts` (§10), counters at 10 and 1000 modules |
| a reader sees only its slice | trace tests: `Db::log_executed` after an impl edit shows no `TraitImpls` query of another trait, and no resolver query but those of the edited module |
| the same as from nothing | `harness/v2/dbsp/`: edit sequences over the harness projects. Each step is built by blaze incrementally and by blaze from nothing, and the outcome, error keys and program run are compared (no rustc or cargo since 2026-10-30) |
| no allocation on a read | `blaze-alloc-count` budgets: a slice read and a `scope_name` hit allocate nothing |

## 13. Who builds what

| part | child | paths |
|---|---|---|
| the generic engine (Z-sets, arrangements, read index, DRed region, transactions, slice hashing) and the `base-db` maintainer hook | `dbsp-engine` | `crates/blaze-dbsp/` (new), `crates/base-db/src/maintain.rs` + targeted write-method edits |
| maintained name resolution and its publication | `dbsp-nameres` | `crates/hir-def/src/nameres/delta.rs`, `nameres/delta/`, targeted edits in `nameres.rs` |
| impl tables and trait candidate sets | `dbsp-impls` | `crates/hir-ty/src/method_resolution/delta.rs`, `delta/`, targeted edits in `method_resolution.rs`, `scope_sets.rs`, `blaze-items/src/index.rs` |
| the rustc differential over edit sequences, cost budgets | `dbsp-proof` | `harness/v2/dbsp/`, `crates/blaze-v2-proof/src/dbsp.rs` |
