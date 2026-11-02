<!-- From mem-db-def-collect (child of mem-db-def). Notes: docs/v2/plan/lanes/mem/db-def-collect.md.
     Report: /tmp/blaze-lanes/v2-mem-db-def-collect.md. Line numbers are as read on 2026-10-29;
     the files move under other lanes, so the function named with each line is what to match. -->

# mem-db-def-collect: def-map collection on the query frame

Contract: `docs/v2/plan/lanes/mem/db.md` (points 1 to 7). Every collecting query opens its own
task frame (`blaze_mem::frame::task(Tag::Nameres, |frame| ..)` around its body; salsa's
`execute` opens none), threads `&'f Frame` into its collector, and keeps every per-run temporary
there. What it
returns is owned, on the global allocator, at exact size, made once from the frame vectors.
Manifests and `#![feature(allocator_api)]` are mem-db-def's H1 to H3 (not refiled here).

Owners: `nameres.rs`, `nameres/collector.rs`, `assoc.rs`, `diagnostics.rs`, `demand.rs`,
`frozen.rs`, `uses.rs`, `mod_resolution.rs`, `proc_macro.rs`, `attr_resolution.rs` and the new
test: **db-expand-names** (window `db-expand-names-next1`). `nameres/facts.rs` and `nameres/facts/`:
**frontend-nameres** (window `frontend-nameres-next1`, children
`frontend-nameres-core-{collect,unit,macros,driver}`). Part E is for mem-db-def-resolve and
mem-db-def-expand.

Rules every change below keeps:

- `frame::task` only at the top of a query function's body, and the frame is passed down from
  there. `frame::current` is not used in nameres: it stops outside a task, and inside a nested
  query it would hand out the outer query's frame. Code reached outside query runs (IDE,
  `Resolver`, `resolve_path_fp*`, `demand::walk_bodies` from drivers) keeps its signature and
  uses `blaze_mem::scratch` (Part D3).
- A closure given to `Db::run_all` (`F: Fn(&Db, usize) + Sync`) captures `&[T]`, never a
  `&Vec<T, &Frame>`: `Frame` is not `Sync`.
- Frame vectors drop their elements when dropped (a `Name` holds a `Symbol`); every frame
  collection here lives inside the `task` closure, so it is dropped before the task ends.
- `Default` is not implemented for `Vec<T, &Frame>`: every `mem::take` of a frame vector
  becomes `mem::replace(&mut v, Vec::new_in(frame))` (allocates nothing).
- Out of frame room is blaze-mem's `MemError` (the frame's own reserve-or-stop); nothing here
  falls back to the global heap.

## Tags

| Query (file) | Tag |
|---|---|
| `crate_local_def_map`, `block_def_map` (`nameres.rs`) | `Tag::Nameres` |
| `Modules::module`, `ModuleScope::{name, legacy_macro, traits}` (`nameres.rs`) | `Tag::Nameres` |
| `TraitItems::query_with_diagnostics`, `ImplItems::of` (`assoc.rs`) | `Tag::Nameres` |
| `used_imports` (`uses.rs`) | `Tag::Nameres` |
| `macros_declaring_roots`, `macros_writing_diagnostic_items` (`demand.rs`) | `Tag::Nameres` |
| every `#[salsa::tracked]` fn in `facts/queries.rs`, `facts/driver.rs`, `facts/macros.rs`, `facts/impls.rs` (`module_decls`, `legacy_exports`, `subtree_macro_exports`, `round_modules`, `module_round`, `import_shape`, `partition`, `round_env`, `scc_state`, `fact_handle`, `unresolved_named`, `macro_step`, `round_progress`, `round_kind`, `round_row`, `module_impls`, ...) | `Tag::Nameres` |

Each query passes its tag to `frame::task(tag, ..)`, which sets the frame's tag.

Shared aliases, added at the top of `nameres.rs` (used by `collector.rs`, `facts/`, `assoc.rs`,
`uses.rs`, `demand.rs`, `frozen.rs`, and requested of `item_scope.rs` in Part E):

```rust
use blaze_mem::Frame;
use rustc_hash::FxBuildHasher;

/// A per-run vector, in the query's frame.
pub(crate) type FrameVec<'f, T> = Vec<T, &'f Frame>;
/// A per-run map, in the query's frame.
pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;
/// A per-run set, in the query's frame.
pub(crate) type FrameSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;

pub(crate) fn frame_map<'f, K, V>(frame: &'f Frame) -> FrameMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}
pub(crate) fn frame_set<'f, T>(frame: &'f Frame) -> FrameSet<'f, T> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}
```

`hashbrown`'s `Entry` has the same `Occupied`/`Vacant`/`or_insert`/`or_insert_with` as std's;
`.entry(k).or_default()` on a map whose values are frame vectors becomes
`.entry(k).or_insert_with(|| Vec::new_in(frame))`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:25): the facts side uses `Tag::Nameres` and these aliases, copied as written, when it adopts C1-C4. That waits on H2, where the queries open the task (see C5). Nothing applies before then.

## Part A. `nameres.rs`

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:25): not ours: `nameres.rs` is db's. Lane frontend-nameres owns only `nameres/facts.rs` and `nameres/facts/`. A1-A8 (and H1/H2, which supersede A1/A2) are db-expand-names', deferred with R10 at ~21:20 (Andrew, 21:05). Since db applied frontend-nameres-core's hook, A1/A2's bodies call `facts::collect_crate`/`facts::collect_block`, not `collector::collect_defs`. H2 has to call those, with `frame` as H6 says.

### A1. `crate_local_def_map` (line 584): the query opens its task

> rewritten 2026-10-29 ~21:30 (mem-db-def-collect). The first text used `frame::current`, on the
> premise that salsa's `execute` opens a task per query run. It does not (`vendor/salsa` uses
> blaze-mem only for memo tables), and `frame::current` outside a task stops (`frame.rs` 497).
> A1 and A2 don't depend on R10, so they can land now. They are what frontend-nameres' C1 to C4
> wait on.

Before (584 to 591, after the tracing span):

```rust
    if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {
        return DefMapPair::new(db, def_map, local_def_map);
    }
    let (def_map, local_def_map) = facts::collect_crate(db, crate_id);
    frozen::check(db, crate_id, &def_map, &local_def_map);
    DefMapPair::new(db, def_map, local_def_map)
```

After (the span stays where it is, before the task):

```rust
    blaze_mem::frame::task(Tag::Nameres, |frame| {
        if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {
            return DefMapPair::new(db, def_map, local_def_map);
        }
        let (def_map, local_def_map) = facts::collect_crate(db, frame, crate_id);
        frozen::check(db, crate_id, &def_map, &local_def_map);
        DefMapPair::new(db, def_map, local_def_map)
    })
```

`use blaze_mem::Tag;` is added. `frozen::served` becomes `frozen::served_in(db, crate_id, frame)`
when D5 lands, and the body becomes H2's when R10 lands. The facts queries the driver calls open
their own tasks (C1), so `frame` here holds only what `collect_crate` itself makes.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).
> applied by db-expand-names (2026-10-29 ~21:40): as written, with `use blaze_mem::Tag;`. `facts.rs`'s `collect_crate` does not take `frame` yet. The facade edit is requested in `v2/requests/db-expand.md` (`## db-expand-names: facts' collect_crate/collect_block take the query's frame`), and frontend-nameres-next1 was told.

### A2. `block_def_map` (line 595): the query opens its task

Everything from `let krate = module.krate(db);` (610) to the end goes inside
`blaze_mem::frame::task(Tag::Nameres, |frame| { ... })`, and the call (620) becomes
`facts::collect_block(db, frame, def_map, TreeId::new(ast_id.file_id, Some(block_id)), local_def_map.local(db))`.
The two `to_static()` calls (safe fns now, speed-unsafe-front-def, 2026-10-30: no `unsafe`, no
SAFETY), `visibility` and `module_data` stay before the task. Under R10 the body becomes H2's, and `frame` travels inside the
`BuildingDefMap` instead of as an argument.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).
> applied by db-expand-names (~21:40): as written. The SAFETY blocks, `visibility` and `module_data` stay before the task. The same `facts.rs` request covers `collect_block`'s `frame`.

### A3. `DefMap.diagnostics` (line 244) and `derive_helpers_in_scope` (line 234)

| Field | Before | After |
|---|---|---|
| `diagnostics` | `Vec<DefDiagnostic>`, pushed by the collector (13 sites), then `shrink_to_fit` | `Box<[DefDiagnostic]>`, made once in `collector::finish` from the collector's frame vector (B4) |
| `derive_helpers_in_scope` | `FxHashMap<AstId<ast::Item>, Vec<DeriveHelper>>`, grown during collection, `shrink_to_fit` | `FxHashMap<AstId<ast::Item>, Box<[DeriveHelper]>>`, built once at exact capacity from the collector's frame map (B4) |

with `type DeriveHelper = (Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>);` named in
`nameres.rs`. Only the collector reads `derive_helpers_in_scope` while a map is collected
(`collector.rs` 2265, 2312, 2498, 2716, 2721, 2983); `path_resolution.rs` does not. So it is staged in
`DefCollector` (B1) and published at `finish`. `DefMap::derive_helpers_in_scope()` (line 787) keeps
its signature (`Box<[T]>` derefs to `[T]` as `Vec` did). `DefMap::empty` sets
`diagnostics: Box::default()` and `derive_helpers_in_scope: FxHashMap::default()` (neither
allocates).

`DefMap` gains one crate-visible setter used by `finish`, the facts driver and `frozen`:

```rust
    /// The def map's diagnostics and derive helpers, once, from the collection's frame.
    pub(super) fn publish<'f>(
        &mut self,
        diagnostics: FrameVec<'f, DefDiagnostic>,
        helpers: FrameMap<'f, AstId<ast::Item>, FrameVec<'f, DeriveHelper>>,
    ) {
        always!(self.diagnostics.is_empty() && self.derive_helpers_in_scope.is_empty());
        // One allocation: `Drain` is `TrustedLen`, so the vector is made at its length.
        self.diagnostics = diagnostics.into_iter().collect();
        let mut map = FxHashMap::with_capacity_and_hasher(helpers.len(), Default::default());
        for (item, list) in helpers {
            map.insert(item, Box::from(&list[..]));
        }
        self.derive_helpers_in_scope = map;
    }
```

(`DeriveHelper` is `Copy`: `Name` and the ids are. `vec::IntoIter<T, &Frame>` is `TrustedLen`, so
`collect::<Box<[T]>>()` allocates exactly once.)

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A4. `DefMap::shrink_to_fit` (line 672) becomes `DefMap::settle`

Before: `macro_use_prelude.shrink_to_fit()`, `diagnostics.shrink_to_fit()`,
`derive_helpers_in_scope.shrink_to_fit()`, `modules.shrink_to_fit()`, and per module
`children_mut().shrink_to_fit()`, `settle_glob_ambiguities()`, `scope_mut().shrink_to_fit()`.

After: `diagnostics` and `derive_helpers_in_scope` are exact already (A3), so they leave the list.
What path resolution reads *while* the map is collected stays in the map and grows there:
`macro_use_prelude`, the `ModulesMap`, each module's `ItemScope` and `children`. Those are
rebuilt at exact size once, in place of the realloc `shrink_to_fit` made:

```rust
    /// Once collection is done: every table read while collecting, made again at its size.
    fn settle(&mut self) {
        let Self { macro_use_prelude, modules, .. } = self;
        *macro_use_prelude = exact_map(mem::take(macro_use_prelude));
        if let ModulesSlot::Building(modules) = modules {
            modules.settle();
            for (_, module) in modules.iter_mut() {
                let children = module.children_mut();
                *children = mem::take(children).into_iter().collect_exact();
                module.scope_mut().settle_glob_ambiguities();
                module.scope_mut().settle();
            }
        }
    }
```

with, in `nameres.rs`:

```rust
/// `map` again at exactly its length: one allocation, the same count `shrink_to_fit` made, and
/// no slack kept.
fn exact_map<K: Hash + Eq, V>(map: FxHashMap<K, V>) -> FxHashMap<K, V> {
    let mut out = FxHashMap::with_capacity_and_hasher(map.len(), Default::default());
    out.extend(map);
    out
}
```

`ModulesMap::settle` and `collect_exact` for `FxIndexMap` do the same (`with_capacity_and_hasher(len)`
then `extend`). `ItemScope::settle` is Part E1. The name is changed so no caller keeps reading it
as a shrink. `collect_defs` (line 136) and `facts/driver.rs` (557, 766) call `settle`.

The growth of these four tables during the miss stays on the global heap: they are part of the
`DefMap` that `resolve_path_fp` reads, and the result cannot borrow the frame. The notes give the
long-term move (staging them in the frame behind the resolve-side view). Under the facts engine the
crate `DefMap` is assembled from final rows (`driver.rs` 445: `ModulesMap` reserved to
`modules.order.len()`), so only blocks and the old collector still grow them.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A5. `LocalDefMap::shrink_to_fit` and `DefMapCrateData::shrink_to_fit` (line 327)

`LocalDefMap.extern_prelude: FxIndexMap` is filled by the collector one dependency at a time.
It becomes built once from the collector's frame list in `finish` (B4):
`FxIndexMap::with_capacity_and_hasher(n, ..)` plus `extend`. `LocalDefMap::shrink_to_fit` goes.
`DefMapCrateData::shrink_to_fit` becomes `settle`, with `exact_map` for the three maps.
`registered_tools: Vec<Symbol>` becomes `Box<[Symbol]>`, made once after the crate attributes are
read: the collector gathers `#![register_tool]` names in a frame vector, then one
`Vec::with_capacity(PREDEFINED_TOOLS.len() + extra)` is filled and boxed (exact, no realloc).
`unstable_features.shrink_to_fit()` is `UnstableFeatures`' own (hir-def items slice); it stays
until that type is built at size.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres-core (2026-10-29 22:45): `nameres.rs` is db's (db-expand-names), not core's.
> Whoever applies A5 there also changes these core call sites in the same change, or tells core
> to: `facts/collect.rs` `crate_data`, which pushes `#![register_tool]` names onto
> `registered_tools` and calls `crate_data.shrink_to_fit()`; and `facts/driver.rs`
> `collect_crate`, which calls `Arc::make_mut(&mut crate_ops.data).shrink_to_fit()` and
> `local.shrink_to_fit()`. With a `Box<[Symbol]>`, gather the extra tools first and box them
> once, starting from the predefined ones. `CrateEnv::local_def_map` already reserves the
> extern prelude exactly, so the `local` call can simply go.

> db-expand-names (2026-10-29 22:42): mine (`nameres.rs`), deferred with Part H, which replaces it (Andrew's decision at 21:05; see the Part H note below). It also needs others' edits: the `facts/driver.rs` calls of `local.shrink_to_fit()` and `Arc::make_mut(data).shrink_to_fit()` (frontend-nameres renames them in the same pass), and B4/H4 build `extern_prelude`, `registered_tools` and `unstable_features` at their size. Without those, dropping the shrink would leave those columns oversized. db-audit-dbsub-expand asked to apply it; I asked it to wait. If Andrew lifts the deferral for A5 alone, I apply it and tell frontend-nameres-next1.

### A6. `DiagnosticsByModule::of` (line 262)

`order` is exact (`(0..len).collect()` from a `Range`). `modules` grows by `push` and is shrunk by
`.into()`. After: count the runs first, then fill at exact size, with no frame needed:

```rust
    fn of(diagnostics: &[DefDiagnostic]) -> DiagnosticsByModule {
        let len = u32::try_from(diagnostics.len()).expect("a def map has 2^32 diagnostics");
        let mut order: Vec<u32> = (0..len).collect();
        order.sort_by_key(|&at| diagnostics[at as usize].in_module);
        let module_of = |at: &u32| diagnostics[*at as usize].in_module;
        let runs = order.chunk_by(|a, b| module_of(a) == module_of(b)).count();
        let mut modules = Vec::with_capacity(runs);
        let mut start = 0u32;
        for run in order.chunk_by(|a, b| module_of(a) == module_of(b)) {
            modules.push((module_of(&run[0]), start));
            start += run.len() as u32;
        }
        DiagnosticsByModule { order: order.into_boxed_slice(), modules: modules.into_boxed_slice() }
    }
```

`sort_by_key` is a stable sort and allocates scratch for long slices. It becomes
`order.sort_unstable_by_key(|&at| (diagnostics[at as usize].in_module, at))`: same order, since the
index breaks ties, and no allocation.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A7. `ModuleScope::traits` (line 514)

Before: `self.scope(db).traits().collect()`. `traits()` is a `filter`, so the box grows and is
shrunk. After:

```rust
    #[salsa::tracked(returns(ref))]
    fn traits(self, db: &'db Db) -> Box<[crate::TraitId]> {
        let scope = self.scope(db);
        let mut out = Vec::with_capacity(scope.traits().count());
        out.extend(scope.traits());
        out.into_boxed_slice()
    }
```

`legacy_macro` (`Box::from(&[MacroId])`) and `name` (`PerNs`, no box) are exact already.
`Modules::module` clones a `ModuleData` (no box: `ScopeSlot::Collected` is an id).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A8. `ModuleData::new` (line 1113) and `DefMap::empty` (line 639)

`ScopeSlot::Building(Box::default())` is one global box per module, dropped at `hand_over_scopes`.
That stays: the `Box<BuildingScope>` is how a collected `ModuleData` keeps 16 bytes. The note in
A4 covers it as part of the building `DefMap`. `DefMap::empty`'s `ModulesMap::new()` stays
(no allocation until the root is inserted).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part B. `nameres/collector.rs`

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:25): not ours, and now test-only. `collector.rs` is db's, and since frontend-nameres-core's hook it is `#[cfg(test)] mod collector;`, the differential reference. B1-B5 no longer touch the build's path, and its allocations matter only in tests. db decides whether to still apply them. H3 is the same.

### B1. `collect_defs` (line 72) and `DefCollector` (line 745)

Signature: `pub(super) fn collect_defs<'f>(db: &Db, frame: &'f Frame, def_map: DefMap, tree_id: TreeId, crate_local_def_map: Option<&LocalDefMap>) -> (DefMap, LocalDefMap)`.
`DefCollector<'db>` becomes `DefCollector<'db, 'f>` with a new field `frame: &'f Frame`;
`ModCollector<'a, 'db>` (line 2928) becomes `ModCollector<'a, 'db, 'f>` holding
`def_collector: &'a mut DefCollector<'db, 'f>`; every `impl` and every closure typed
`&mut DefCollector<'_>` (e.g. `update_def`) gains the `'_`.

| Line | Field / local | Before | After |
|---|---|---|---|
| 82, 752 | `deps` | `FxIndexMap<Name, BuiltDependency>` (`dep.clone()` each) | `FrameVec<'f, (Name, &'db BuiltDependency)>`: insert replaces in place when the name is there (`iter_mut().find`), else pushes (the `FxIndexMap::insert` order); `get(name)` is `iter().find` (tens of entries) |
| 90 | `proc_macros` | `Box<[(Name, CustomProcMacroExpander, bool)]>` from `ProcMacros::list` | unchanged here; Part E3 asks hir-expand for `list_in(ctx, frame)` |
| 106, 753 | `globs` | `GlobEdges` = `ImporterLists<ModuleId, (Visibility, GlobId)>` | `GlobEdges<'f>`, B3 |
| 107, 754 | `imports` | `Imports<'db>` | `Imports<'db, 'f>`, B2 |
| 108, 757 | `held` | `Vec<(Option<Name>, PerNs)>` | `FrameVec<'f, (Option<Name>, PerNs)>` |
| 109, 759 | `path_buf` | `String` | removed: B5 |
| 110, 761 | `round_resolved` | `Vec<ResolvedCall>` | `FrameVec<'f, ResolvedCall>` |
| 111, 762 | `round_calls` | `Vec<MacroCallId>` | `FrameVec<'f, MacroCallId>` |
| 112, 763 | `macros_spare` | `Vec<MacroDirective<'db>>` | `FrameVec<'f, MacroDirective<'db>>` |
| 113, 764 | `unresolved_macros` | `Vec<MacroDirective<'db>>` | `FrameVec<'f, MacroDirective<'db>>` |
| 116, 775 | `mod_dirs` | `FxHashMap<ModuleId, ModDir>` | `FrameMap<'f, ModuleId, ModDir>` |
| 119, 783 | `from_glob_import` | `PerNsGlobImports` (three `FxHashSet<(ModuleId, Name)>`) | `PerNsGlobImports<&'f Frame>`, Part E1 |
| 120, 791 | `skip_attrs` | `FxHashMap<InFile<ModItemId>, AttrId>` | `FrameMap<'f, InFile<ModItemId>, AttrId>` |
| 121, 795 | `prev_active_attrs` | `FxHashMap<AstId<ast::Item>, SmallVec<[AttrId; 1]>>` (spills to the heap past one) | `FrameMap<'f, AstId<ast::Item>, FrameVec<'f, AttrId>>`; line 2393 `entry(ast_id).or_insert_with(|| Vec::new_in(frame))` |
| 122, 774 | `unresolved_extern_crates` | `FxHashSet<Name>` | `FrameSet<'f, Name>` |
| 124, 800 | `deferred_builtin_derives` | `FxHashMap<AstId<ast::Item>, Vec<DeferredBuiltinDerive>>` | `FrameMap<'f, AstId<ast::Item>, FrameVec<'f, DeferredBuiltinDerive>>`; the `consider_deferred_derives` closure parameter type follows |
| 125 | `resolved_macro_paths` | `Vec<ResolvedMacroPath>` | `FrameVec<'f, ResolvedMacroPath>` |
| 126, 803 | `derive_helper_uses` | `Vec<DeriveHelperUse>` | `FrameVec<'f, DeriveHelperUse>` |
| 127, 805 | `not_introducing` | `Vec<ExternCrateId>` | `FrameVec<'f, ExternCrateId>` |
| new | `diagnostics` | (pushed into `self.def_map.diagnostics`) | `FrameVec<'f, DefDiagnostic>`; every `self.def_map.diagnostics.push(..)` (913, 1062, 2424, 2562, 2575, 2777, 2795, 2830, 3128, 3464, 3972 and the rest `rg 'def_map.diagnostics.push'` finds) becomes `self.diagnostics.push(..)` / `self.def_collector.diagnostics.push(..)` |
| new | `derive_helpers` | (in `self.def_map.derive_helpers_in_scope`) | `FrameMap<'f, AstId<ast::Item>, FrameVec<'f, DeriveHelper>>`; the six reads/writes (2265, 2312, 2498, 2716, 2721, 2983) go to it, `or_default()` becoming `or_insert_with(|| Vec::new_in(frame))` |
| new | `extern_prelude` | (inserted into `self.local_def_map.extern_prelude`) | `FrameVec<'f, (Name, (CrateRootModuleId, Option<ExternCrateId>))>`, with the map's replace-in-place rule, published in `finish` (A5) |

`LocalDefMap` and the `DefMap` themselves stay owned (they are the result).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B2. `Imports<'db>` (line 299) becomes `Imports<'db, 'f>`

`fn new() -> Self` (line 339) becomes `fn new_in(frame: &'f Frame) -> Self`, and each field
changes as follows:

| Field | Before | After |
|---|---|---|
| `directives` | `Vec<ImportDirective<'db>>` | `FrameVec<'f, ImportDirective<'db>>` |
| `list` | `Vec<ImportList>` | `FrameVec<'f, ImportList>` |
| `generation` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `queued` | `Vec<u8>` | `FrameVec<'f, u8>` |
| `seq` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `indeterminate` | `Vec<(u32, PerNs)>` | `FrameVec<'f, (u32, PerNs)>` |
| `named` | `FxHashMap<(ModuleId, Name), (u32, u32)>` | `FrameMap<'f, (ModuleId, Name), (u32, u32)>` |
| `next_named` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `pending_unresolved`, `pending_indeterminate` | `BinaryHeap<Reverse<u32>>` | `BinaryHeap<Reverse<u32>, &'f Frame>` (`BinaryHeap::new_in(frame)`) |
| `deferred_unresolved`, `deferred_indeterminate` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `waits` | `FxHashMap<WaitOn, u32>` | `FrameMap<'f, WaitOn, u32>` |
| `edges` | `Vec<WaitEdge>` | `FrameVec<'f, WaitEdge>` |
| `any_waiters` | `Vec<(u32, u32)>` | `FrameVec<'f, (u32, u32)>` |
| `keys` | `Vec<(WaitOn, u8)>` | `FrameVec<'f, (WaitOn, u8)>` |
| `woken` | `Vec<u32>` | `FrameVec<'f, u32>` |

`#[cfg(test)] rounds` stays (test-only). `wake_any` (line 578) uses `mem::take` on a frame
vector. It becomes an index walk (`wake` never adds to `any_waiters`):

```rust
    fn wake_any(&mut self) {
        for at in 0..self.any_waiters.len() {
            let (import, generation) = self.any_waiters[at];
            if self.generation[import as usize] == generation {
                self.wake(import);
            }
        }
        self.any_waiters.clear();
    }
```

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B3. `ImporterLists<K, T>` (line 630) becomes `ImporterLists<'f, K, T>`

`edges: FrameVec<'f, ImporterEdge<K, T>>`, `heads: FrameMap<'f, K, (u32, u32)>`,
`by_pair: FrameMap<'f, (K, K), u32>`; `new()` (639) becomes `new_in(frame)`;
`type GlobEdges<'f> = ImporterLists<'f, ModuleId, (Visibility, GlobId)>;` (682).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B4. `finish` (line 2745) and `name_records` (line 2850)

`finish` publishes what the frame staged. It replaces the `def_map.shrink_to_fit()` and
`local_def_map.shrink_to_fit()` in `collect_defs` (136, 137):

```rust
        self.def_map.names = self.name_records();
        let diagnostics = mem::replace(&mut self.diagnostics, Vec::new_in(self.frame));
        let helpers = mem::replace(&mut self.derive_helpers, frame_map(self.frame));
        self.def_map.publish(diagnostics, helpers);
        self.local_def_map.extern_prelude = {
            let mut map = FxIndexMap::with_capacity_and_hasher(self.extern_prelude.len(), Default::default());
            map.extend(self.extern_prelude.drain(..));
            map
        };
        (self.def_map, self.local_def_map)
```

and `collect_defs` ends `let (mut def_map, local_def_map) = collector.finish(); def_map.settle(); (def_map, local_def_map)`.

`name_records`: `uses`, `found`, `unresolved`, `glob_targets` and `macro_calls` (2855 to 2888)
become `Vec::new_in(self.frame)`, and `seen` (2914) becomes `frame_set(self.frame)`. Each box is
made at its length: `uses: Box::from(&uses[..])` (`Use` is `Copy`, as the `retain(|&it| ..)` reads
it), `unresolved: Box::from(&unresolved[..])`, `glob_targets: Box::from(&glob_targets[..])`,
`macro_calls: Box::from(&macro_calls[..])`, `derive_helpers: Box::from(&self.derive_helper_uses[..])`
and `not_introducing: Box::from(&self.not_introducing[..])`, replacing the
`into_boxed_slice` (a realloc whenever a `Vec` has slack). `path_uses` and
`path_uses_outside_imports` take `out: &mut Vec<Use>`. They become generic, `out: &mut Vec<Use, A>`
with `A: Allocator`, in `uses.rs` (D2).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B5. Per-call temporaries

| Line | Function | Before | After |
|---|---|---|---|
| 864 | `seed_with_top_level` | `let mut prelude = Vec::new();`, read by a `run_all` closure | `Vec::new_in(self.frame)`; the closure captures `let prelude: &[_] = &prelude;` |
| 2094 | `resolve_macros` | `mem::replace(&mut self.unresolved_macros, mem::take(&mut self.macros_spare))` | `mem::replace(&mut self.unresolved_macros, mem::replace(&mut self.macros_spare, Vec::new_in(self.frame)))` |
| 2095 | `resolve_macros` | `mem::take(&mut self.round_resolved)` | `mem::replace(&mut self.round_resolved, Vec::new_in(self.frame))`; the `push_resolved` closure's parameter is `&mut FrameVec<'f, ResolvedCall>` |
| 2118 | `resolve_macros` | `let mut eager_callback_buffer = vec![];` | `Vec::new_in(self.frame)` |
| 2393 | `resolve_macros` (attr) | `prev_active_attrs.entry(ast_id).or_default()` | `.or_insert_with(|| Vec::new_in(frame))`; `AttrMacroAttrIds::from_many(&active_attrs[..])` unchanged |
| 2431 | `resolve_macros` (derive) | `SmallVec::new()` for `derive_call_ids` | unchanged: it goes into the owned `MacroCallKind`/derive record (result data) |
| 2662 | `expand_round` | `mem::take(&mut self.round_calls)` | `mem::replace(.., Vec::new_in(self.frame))`; `expand_all` already takes `&[MacroCallId]` |
| 3342 | `process_macro_use_extern_crate` | `let mut single_imports = Vec::new();` then `Option<Vec<Name>>` to `import_macros_from_extern_crate` | `Vec::new_in(frame)`; the parameter becomes `Option<&[Name]>` |
| 3391, 3420 | `collect_module` | `&mut self.def_collector.path_buf` | `blaze_mem::scratch::with_string(|buf| ...)` around each `descend_into_definition` / `resolve_declaration` call (`mod_resolution.rs` keeps `buf: &mut String`); `path_buf` leaves the struct |
| 3780 | `collect_macro_call` | `let mut eager_callback_buffer = vec![];` per macro call | `Vec::new_in(self.def_collector.frame)` |
| `MacroDirectiveKind::Attr` | `attr: Attr` cloned from the item tree (an `Attr` owns `Option<Box<AttrInput>>`, so 1 to 3 allocations per attribute directive) | `attr: &'db Attr` (`Attrs<'db>::iter` yields `&'db Attr`); `resolve_attributes(&mut self, attrs: Attrs<'db>, ..)` |

`expand_all` (line 144) captures `calls: &[MacroCallId]` already.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part C. `nameres/facts/`

The facts files are being written now (`driver.rs` and `macros.rs` changed while this was
read), so these are rules for each function. The line numbers are as read. The query graph has
no cycle (§G). Values carried between queries (`SccState.state: Arc<UnitState>`, `MacroStep`,
`ModuleDecls`, `SideTables`) are memo values: they stay owned. Everything a query makes
and drops in its run goes to the frame.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### C1. Frame plumbing

`CollectCx<'db>` (`env.rs`) becomes `CollectCx<'db, 'f>` with `frame: &'f Frame`.
`collect::cx(db, krate, in_block)` becomes `cx(db, frame, krate, in_block)`, called inside each
query's task. `CollectCx.deps: FxIndexMap<Name, BuiltDependency>` becomes
`FrameVec<'f, (Name, &'db BuiltDependency)>` (the rule of B1). Each query that makes a `cx` opens
`blaze_mem::frame::task(Tag::Nameres, |frame| ..)` around its body. That is `module_decls`,
`macro_step`, `round_env`, `scc_state`, `import_shape`, `partition`, `round_modules`,
`module_round` and `legacy_exports`. `collect_crate`/`collect_block` are plain functions and take
the frame from A1/A2.

> corrected 2026-10-29 ~21:30 (mem-db-def-collect), answering frontend-nameres' 21:20 note under
> C5: this said `frame::current`, which stops outside a task. With each query opening its own
> `frame::task`, no facts query depends on its caller being in a task. So C1 to C4 don't wait on
> H2 or R10: the queries can adopt them now. `collect_crate`/`collect_block` take `frame` once
> A1/A2 land, and A1/A2 don't depend on R10 either.
> frontend-nameres (2026-10-29 21:36): accepted, and C1 to C4 are no longer blocked. db-expand-names applied A1/A2, and `facts::collect_crate(db, frame, krate)` and `collect_block(db, frame, ..)` now take that frame. Their own temporaries (the component states, the pending carried steps, the import list, the block's unresolved imports) are `Vec<_, &Frame>` in it. The two test queries in `facts/tests.rs` open their own `frame::task`. Nothing is built. C1 to C4 are being built by the child `frontend-nameres-frames`, with brief `~/restart-2026-10-28/briefs/v2/frontend-nameres-frames.md` and report `/tmp/blaze-lanes/v2-frontend-nameres-frames.md`, which answers under each part when done. Where C2 names A3/A4/A5 (deferred with R10), it keeps `shrink_to_fit` for now.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).
> answered by frontend-nameres-frames (2026-10-29): C1 applied, unbuilt (no build allowed in the lane). `CollectCx<'db, 'f>` carries `frame` and `deps: FrameVec<'f, (Name, &'db BuiltDependency)>` (B1's rule, looked up by `cx.dep(name)`); `collect::cx(db, frame, krate, in_block)`. Each tracked fn in `queries.rs` that allocates temporaries opens `frame::task(Tag::Nameres, ..)` and passes `frame` down: `root_decl`, `module_decls`, `round_modules`, `module_round`, `import_shape`, `partition`, `scc_state`, `macro_step`, and `decl_handle` and `subtree_macro_exports` too (one replays a unit, the other gathers ops). `round_env` and `legacy_exports` open none, as neither has a temporary (`legacy_exports` counts and then fills exactly). Nothing in `facts/` calls `frame::current`. Report: `/tmp/blaze-lanes/v2-frontend-nameres-frames.md`.

### C2. Per query

| Query / function (file:line) | Temporary before | After | Owned result: how it is made |
|---|---|---|---|
| `round_modules` (driver.rs:56) | `order`, `rows` `Vec::new()` push, `index` `FxHashMap`, `stack` `vec![root]` | `order`/`rows` in `FrameVec`, `stack` `FrameVec`; `index` stays owned (it is in `RoundModules`) but is made `with_capacity_and_hasher(order.len())` after the walk, from the frame `order` | `order: Box::from(&order[..])`, `rows` moved by `collect::<Box<[_]>>()` from `order.drain(..)`'s twin (`TrustedLen`): one allocation each, no realloc |
| `round_modules` later rounds (driver.rs:90) | `previous.index.clone()` then inserts | build the new index once at `rows.len()` | as above |
| `module_round` (driver.rs:109, 151) | `ops` pushed, `ops.shrink_to_fit()` | frame vector of `DeclOp` | `OpLog` from it (C3) |
| `import_shape` (driver.rs:189, 200, 207) | a `collect()` (line 200) of unknown length, `out` `with_capacity(order.len())` | the line-200 list in a `FrameVec` | `modules: out.into_boxed_slice()` is exact (filled to its capacity) |
| `ShapeScratch` (driver.rs:267) | its vectors | `FrameVec`s, built with `cx.frame` | none |
| `collect_crate` (driver.rs:434) | `crate_ops.diagnostics: Vec::new()` push, `unresolved_extern_crates: FxHashSet`, `states: Vec<&SccState>`, `derive_helpers_in_scope` `FxHashMap<_, Vec>` with `or_insert_with(Vec::new)`, `pending: Vec<&MacroStep>`, `FinishSink`'s pushes into `def_map.diagnostics` | `FrameVec`s, `FrameSet`, the helpers as `FrameMap<_, FrameVec<DeriveHelper>>`; `FinishSink` gains `diagnostics: &mut FrameVec<'f, DefDiagnostic>` | `def_map.publish(diagnostics, helpers)` (A3) then `def_map.settle()` in place of `shrink_to_fit` at 557; `local` is exact already (`CrateEnv::local_def_map` reserves exactly); `Arc::make_mut(&mut crate_ops.data).shrink_to_fit()` (479) becomes `.settle()` (A5) |
| `collect_block` (driver.rs:693) | `unresolved_extern_crates`, `pending: Vec<MacroOp>`, `unresolved: Vec<DefDiagnostic>` `collect()` (753), `side: SideTables::default()` | `FrameVec`s, `FrameSet`; `side` is dropped in the run, so the block path's `SideTables` maps become a frame `SideTablesIn<'f>` (C4) | `publish` (A3) with `diagnostics.extend(unresolved)` into the frame vector first; `settle` at 766 |
| `macros::resolve_round` (macros.rs:75): `kept`, `added`, `resolved`, `eager` | `Vec::with_capacity`/`Vec::new()` | `Vec::new_in(cx.frame)` / `with_capacity_in`; `pending: &mut Vec<MacroOp>` becomes `&mut FrameVec<'f, MacroOp>` (and `Sink::take_macros(&mut self, into: &mut FrameVec<'f, MacroOp>)`) | none |
| `macros::expand_round` (macros.rs:545) | `calls` `with_capacity` | `with_capacity_in(n, cx.frame)`, passed as `&calls[..]` | none |
| `macros::resolve_one` derive (macros.rs:333) | `SmallVec::new()` for `derive_call_ids` | unchanged (result data, B5) | |
| `macro_step` (macros.rs:658) | `Carried::pending` (757) `with_capacity`, `StepSink` vectors `macros`, `imports`, `new_modules` (859 to 862), the view `DefMap` (840 to 846), `scope.scope(db).clone()` and `children(db).clone()` (826, 828) | `FrameVec`s for `macros`/`imports`/`pending`; `new_modules` in a `FrameVec`; the view map's tables stay owned (they are a `DefMap` the resolver reads) | `pending: Box::from_iter(pending.drain(..))`, `new_modules: Box::from_iter(..drain(..))` (one allocation each, in place of `into_boxed_slice`), `ops` per C3 (874: `shrink_to_fit` goes) |
| `partition` / `graph.rs` | `ImportGraph::from_edges`'s `cursor`, `by_target`, `offsets`, `targets`; `components`' `rindex`, `frames`, `stack`; `Solver`'s `paths`, `level_path`, `level_first`, `pair_ids`, `pairs`, `module_pairs`, `module_globs`, `links`, `seen`, `work`, `prelude_levels`, `edges` | each a `FrameVec`/`FrameMap`/`FrameSet` presized as today with `with_capacity_in`; `ImportGraph<'f>`, `Solver<'f>` | `Partition { of, starts, members }`: `of` is `rindex` once it is final, so `Box::from(&rindex[..])`; `starts`/`members` `Box::from(&v[..])` |
| `legacy_exports` (queries.rs) | `.filter_map().collect()` | count, then `with_capacity` + `extend` + `into_boxed_slice` (A7's form) | exact |
| `subtree_macro_exports`, `unresolved_named`, `round_progress`, `round_kind`, `latest`, `fact_handle`, `crate_root`, `crate_data`, `root_decl`, `child_key`, `decl_handle`, `row` | no temporaries, or a key | no task where nothing is allocated | ids and `Copy` values |
| `module_impls` (impls.rs:30) | none | none | exact already (`with_capacity` filled to it): one allocation per non-empty box. Budget test (b) holds it there |

`graph.rs`'s local `struct Frame` (the DFS stack entry) is renamed `DfsFrame` so it does not clash
with `blaze_mem::Frame`. The graph tests (`facts/tests/graph.rs`) call `components`,
`import_graph` and `partition` outside a query, so each test body is wrapped in
`blaze_mem::frame::task(Tag::Nameres, |frame| ..)` and passes `frame`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).
> answered by frontend-nameres-frames (2026-10-29): C2 applied row by row, unbuilt. The A3/A4/A5 rows keep today's `def_map.shrink_to_fit()`, `local.shrink_to_fit()` and `Arc::make_mut(data).shrink_to_fit()`: they are deferred with R10, and `DefMap::shrink_to_fit` also settles glob ambiguities and scopes. `collect_crate`/`collect_block` gather diagnostics and derive helpers in the frame and make the def map's list and table exactly at the end. `round_modules` checks repeats with a frame set and builds `index` once from the frame `order`. `graph.rs`: `ImportGraph::from_edges` sorts in the frame by itself (`blaze_radix::Csr` takes no allocator), `DfsFrame`, and `Partition` boxed from the frame columns. `PARTITION_ALLOCATIONS` is now 3: the value's columns, since a frame's memory comes from the system and not the global heap. The three reused buffers (`eager_calls` twice, `name_records`' `found`) are in the frame too, now that db-expand-names/db-expand made `DefMap::path_uses*` and `macro_call_as_call_id` take any allocator.

### C3. `OpLog` (`ops.rs`)

Before: `OpLog { ops: Vec<DeclOp> }`, grown by `push`, then `shrink_to_fit`. After:
`OpLog { ops: Box<[DeclOp]> }` with `OpLog::from_frame(ops: FrameVec<'_, DeclOp>) -> OpLog { OpLog { ops: ops.into_iter().collect() } }`.
That is one allocation, since `vec::IntoIter` is `TrustedLen`. Producers (`collect.rs`'s sink,
`StepSink`, `module_round`) push into a `FrameVec` and call it once. `Update.resolutions:
Box<[(Option<Name>, PerNs)]>` is made from the collector's `held` slice with `Box::from(&held[..])`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).
> answered by frontend-nameres-frames (2026-10-29): C3 applied, unbuilt. `OpLog { ops: Box<[DeclOp]>, owner, flags }` and `OpLog::from_frame(FrameVec<DeclOp>)`, which reads the owner and flags off the ops and then collects them in one exact allocation. `push`, `extend` and `shrink_to_fit` are gone. Its producers are `collect.rs`'s `DeclSink::finish`, `macros.rs`'s `StepSink::into_step` and `driver.rs`'s `module_round`.

### C4. What `UnitState` carries (`unit.rs`, not yet written; `nameres-core.md` §5)

`UnitState` is carried to the next round in `SccState.state: Arc<UnitState>`, so it is owned. Its
per-round queues are made again each round from what is carried, so they are split into a frame
struct the round makes:

```rust
/// One round's queues over a unit's imports: made from the carried `UnitState` when the round
/// starts, dropped when it ends.
struct ImportQueues<'f> {
    pending_unresolved: BinaryHeap<Reverse<u32>, &'f Frame>,
    deferred_unresolved: FrameVec<'f, u32>,
    pending_indeterminate: BinaryHeap<Reverse<u32>, &'f Frame>,
    deferred_indeterminate: FrameVec<'f, u32>,
    waits: FrameMap<'f, WaitOn, u32>,
    edges: FrameVec<'f, WaitEdge>,
    any_waiters: FrameVec<'f, (u32, u32)>,
    keys: FrameVec<'f, (WaitOn, u8)>,
    woken: FrameVec<'f, u32>,
}
```

The carried part keeps `directives`, `list`, `seq`, `indeterminate`, `named`/`next_named`, the
glob edges, `from_glob_import` and the rows. When a carried `Vec` is cloned into a new state, it is
cloned at its length (`Vec::clone` allocates `len`, not capacity), so the carried state is exact.
The block path, whose state is never carried, uses the frame forms of B2 and B3 throughout. The
same split applies to `SideTables` (`env.rs`): `SideTablesIn<'f>` with frame maps for the block
path, and the owned `SideTables` for what `MacroStep` carries.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).
> answered by frontend-nameres-frames (2026-10-29): C4 applied, unbuilt. `unit/imports.rs`: `Imports<A>` keeps `directives`, `list`, `seq`, `indeterminate`, `named` and `next_named`. `ImportQueues<'f>` is made per run from `Imports::len()`, held by `Unit` and dropped with it. Beyond the listed fields it also holds `generation`, `queued`, `phase`, `free_edge` and `keys_any`, which describe the queues and edges and so have to start fresh with them. The results are the same because a run's fixed point ends with every queue empty, and the next run attempts every import first (a fresh unit pushes each one, a carried one calls `wake_all` after its replay). `UnitState<A = Global>` stays owned in `SccState`; a block's and `local_scope`'s are in the frame. `SideTables<A = Global>` stays owned in `ModuleDecls`/`MacroStep`, and `SideTablesIn<'f>` is the block's.

### C5. `env.rs`

`CrateEnv::after` (`into_vec` + `push` + `into_boxed_slice`, which reallocates when the vector has
slack) becomes `let mut v = Vec::with_capacity(self.list.len() + 1); v.extend_from_slice(&self.list); v.push(new); v.into_boxed_slice()`.
`install` (`macro_use_prelude.clear()` + `reserve(n)`) is exact already.

> frontend-nameres-core (owner of `nameres/facts/` but `graph.rs`/`impls.rs`): C5 applied (`CrateEnv::after` builds the grown list at exact size, `env.rs` `set_or_push`). C1-C4 open, owner frontend-nameres-core: they need `blaze_mem::frame::current` (blaze-mem has `frame::task` only today) and hir-def's blaze-mem dependency (H1-H3); `facts/` adopts them in one pass once both land, with `collect_crate`/`collect_block` taking `frame` exactly as A1/A2 call them. Note: `ModuleDecls::side` and `MacroStep::side` are now `Arc<SideTables>`, and `SccState::unresolved` is `SccState::imports: Box<[(ImportOp, Option<PerNs>)]>`.
> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:20, owner of `nameres/facts/` now that core is DONE): C5 is
> applied. C1-C4 stay open, still blocked, now on one thing only: the frame being open where the
> resolver runs. Both earlier prerequisites have landed: hir-def depends on `blaze-mem`, and
> `blaze_mem::frame::current` exists (`frame.rs:495`). But A1/A2 (and their replacement H2,
> where `crate_local_def_map`/`block_def_map` open the task) were deferred with R10 at ~21:22 and
> are not in `nameres.rs`. `frame::current` stops outside a `frame::task` by design, so putting it
> at the top of `module_decls`, `scc_state`, `macro_step` and the rest now would make every
> def-map collection that isn't inside a task panic (direct `crate_def_map` calls, `TestDB`
> tests). `facts/` adopts C1-C4 in one pass when H2 lands: frame-taking `collect_crate`/
> `collect_block`, `CollectCx<'db, 'f>`, `OpLog { ops: Box<[DeclOp]> }` with `from_frame`,
> `ImportQueues<'f>` split from the carried unit state, the per-query table, the `DfsFrame`
> rename, and `frame::task(Tag::Nameres, ..)` around the graph tests. The `FrameVec`/`FrameMap`/
> `FrameSet` aliases follow `hir-ty-mir/src/mir/framed.rs`. Owner of the blocker: db
> (db-expand-names, R10/Part H).

## Part D. The other files of the slice

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:25): not ours: `assoc.rs`, `uses.rs`, `demand.rs`, `diagnostics.rs`, `frozen.rs`, `mod_resolution.rs`, `proc_macro.rs` and `attr_resolution.rs` are db's (db-expand-names). `facts/` calls `DefDiagnostics`/`frozen` only through their public APIs, so D4's generic `new` and D5's `new_in` need no change in `facts/`.

### D1. `assoc.rs`

`TraitItems::query_with_diagnostics` (line 48) and `ImplItems::of` (115): the body goes inside
`blaze_mem::frame::task(Tag::Nameres, |frame| ..)`, and `AssocItemCollector::new` takes `frame`.

| Field (line) | Before | After |
|---|---|---|
| `items` (147) | `Vec<(Name, AssocItemId)>`, `into_boxed_slice()` at 188 (realloc with slack) | `FrameVec<'f, (Name, AssocItemId)>`; result `Box::from_iter(self.items.drain(..))` (one allocation) |
| `macro_calls` (148) | `ThinVec`, `shrink_to_fit()` at 187 | `FrameVec<'f, (AstId<ast::Item>, MacroCallId)>`; result `ThinVec::with_capacity(n)` + `extend` (exact; empty allocates nothing) |
| `diagnostics` (143) | `Vec<DefDiagnostic>` | `FrameVec<'f, DefDiagnostic>`; `DefDiagnostics::new` takes it (D4) |

`collect` returns `(Box<[(Name, AssocItemId)]>, ThinVec<..>, DefDiagnostics)`. Line 59's
`DefDiagnostics::new(vec![])` becomes `DefDiagnostics::default()` (no allocation either way).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D2. `uses.rs`

`used_imports` (`db: &Db`): body in `frame::task(Tag::Nameres, ..)`. `Walk { uses, unresolved, glob_targets }` become
`FrameVec`s and a `FrameMap`. `store()`'s `resolvers` becomes `FrameMap`, and `Within::of`'s
`pats`, `types` and `todo` become frame collections, with the frame passed down from the query.
`finish`'s `imports`, `macro_use` and `globs` (`FxIndexMap<_, Vec<Name>>`), `ambiguous` and `seen`
become frame collections (`indexmap::IndexMap` has no allocator parameter, so they become a
`FrameMap<K, u32>` index into a `FrameVec<(K, FrameVec<'f, Name>)>`, which keeps insertion order).
Each owned result box is made with `Box::from(&v[..])` or a `TrustedLen` collect. `path_uses` and
`path_uses_outside_imports` take `out: &mut Vec<Use, A>` with `A: Allocator` (callers in
`collector.rs` pass frame vectors; IDE callers keep `Vec<Use>`).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D3. `demand.rs`

`macros_declaring_roots` and `macros_writing_diagnostic_items`: body in `frame::task(Tag::Nameres, ..)`. `Look`'s
`known` (`FxHashSet`), `found`, `calling`, `seen` and `settle`'s `writing` become frame collections.
The result `names` is collected from a hash set (exact length), sorted, then `.into()`. It is
built from a frame vector with `Box::from(&v[..])` after `sort_unstable`. `macro_text` and `words`
(`Vec<Symbol>`, per call) become `FrameVec`. The public non-query walkers (`scope_body_owners`,
`module_body_owners`, `crate_body_owners`, `body_blocks`, `walk_bodies`, `every_body`,
`bodies_declaring_roots`, `walk_bodies_saying`) run from drivers outside a query. They keep their
signatures, and `walk_bodies`' `seen`/`queue` and `walk_bodies_saying`'s `local` move to
`thread_local!` `blaze_mem::scratch::Scratch<T>` buffers (`take()`, cleared on entry). A nested call
gets its own buffer through `scratch::lend_u32s` (depth 4), and depth 5 is blaze-mem's panic
naming the caller, not a heap fallback.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D4. `diagnostics.rs`

```rust
impl DefDiagnostics {
    pub fn new<A: Allocator>(diagnostics: Vec<DefDiagnostic, A>) -> Self {
        Self(
            diagnostics
                .is_empty()
                .not()
                .then(|| triomphe::ThinArc::from_header_and_iter((), diagnostics.into_iter())),
        )
    }
}
```

`vec::IntoIter<T, A>` is `ExactSizeIterator`, which is what `from_header_and_iter` needs: one
allocation at exact size, as today.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D5. `frozen.rs`

`served(db, krate)` becomes `served_in(db, krate, frame)`, called from A1 inside the frame.
`Dec<'a>` becomes `Dec<'a, A: Allocator + Copy = Global>`, with a private
`Dec::new_in(db, krate, bytes, alloc: A)`. The public `Dec::new` (used by
`hir-ty/src/method_resolution/frozen.rs`, `deps/signatures/table.rs`, `demand_tests.rs`) is
`new_in(.., Global)`, unchanged for them. The decode temporaries become `Vec<_, A>` /
`hashbrown::HashMap<_, _, FxBuildHasher, A>`: `closure` (and `closure()`'s set), `paths`, `files`,
`calls`, `contexts`, `own`, `modules: HashMap<u32, Vec<ModuleId, A>, _, A>`, and `def_map`'s
`skeleton`. `many()` gets `many_in(alloc)`. The owned maps it fills (`macro_use_prelude`,
`derive_helpers_in_scope`, `exported`, `functions`, `extern_prelude`) are built
`with_capacity(n)` from the frame vector (`n` is known from the encoding) in place of
`into_iter().collect()`. `derive_helpers_in_scope` values are `Box::from(&v[..])` (A3).
`Dec::def_map`'s `super::ModulesMap::new()` becomes `ModulesMap::with_capacity(n)`, and
`Dec::scope` is exact already (`reserve_exact`). `check` (`Mode::Check`, testing) uses the same
frame.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D6. `mod_resolution.rs`, `proc_macro.rs`, `attr_resolution.rs`

- `ModDir::resolve_declaration(.., buf: &mut String)` stays. The caller's buffer comes from
  `scratch::with_string` (B5). The `Box<[String]>` candidates of an unresolved-module diagnostic
  are diagnostic data (owned).
- `parse_macro_name_and_helper_attrs` (`proc_macro.rs:85`): the `filter_map(..).collect::<Box<[_]>>()`
  is a filter (it grows, then shrinks). After:

```rust
        let ident = |tt: &TtElement<'_>| match tt {
            TtElement::Leaf(Leaf::Ident(helper)) => Some(helper.as_name()),
            _ => None,
        };
        let mut out = Vec::with_capacity(helpers.clone().filter(|tt| ident(tt).is_some()).count());
        out.extend(helpers.filter_map(|tt| ident(&tt)));
        let helpers = out.into_boxed_slice();
```

  (`TtIter` is `Clone`.)
- `attr_macro_as_call_id` clones the attribute's `tt` into the call's owned argument (result data
  of the interned call). No change.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part E. Requests to the other slices

E1, to **mem-db-def-resolve** (`item_scope.rs`):

- `PerNsGlobImports` becomes `PerNsGlobImports<A: Allocator + Clone = Global>` with its three sets
  `hashbrown::HashSet<(ModuleId, Name), FxBuildHasher, A>` and `PerNsGlobImports::new_in(alloc)`.
  `push_res_with_import(&mut self, glob_imports: &mut PerNsGlobImports<A>, ..)` is generic over
  `A`.
- `ItemScope::shrink_to_fit` becomes `ItemScope::settle`: each column made again at its length
  (`Vec::with_capacity(len)` + `extend(drain)`, maps by `exact_map`), not shrunk.
- Nothing on the hit path changes (`get`, `resolve_path_fp*` keep their signatures).

E2, to **mem-db-def-items**: none from this slice. `AstIdWithPath` and `ModPath` are interned or
`Copy` already, so directive clones allocate nothing.

E3, to **mem-db-def-expand** (`hir-expand/src/proc_macro.rs`): `ProcMacros::list(ctx)` returns a
new `Box<[(Name, CustomProcMacroExpander, bool)]>` on every collection of a proc-macro crate. It
should be `list_in(ctx, frame: &Frame) -> Option<&[(Name, CustomProcMacroExpander, bool)]>`
(frame-allocated), or an iterator. The collector's `proc_macros` field then becomes
`&'f [..]`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:25): none of E1-E3 is ours. E1 (`PerNsGlobImports<A>`) touches `facts/` only through `PerNsGlobImports::of_scope` (db's, applied), and `facts/` keeps the default `Global` until it adopts C4. E3 (`ProcMacros::list_in`) would make `CollectCx`'s proc-macro list a frame slice, adopted with C1.

## Part F. Budget tests (new file `nameres/tests/budget.rs`, `mod budget;` in `nameres/tests.rs`)

They need H3's `#[global_allocator] static ALLOC: blaze_alloc_count::Counting` (mem-db-def). Every
batch runs on the calling thread (`TestDB`'s `RUN_ORDER`), so per-thread counts see the whole
collection.

```rust
//! Allocation budgets of def-map collection (docs/v2/plan/lanes/mem/db.md, point 7): a
//! re-collection that changes nothing allocates nothing, and a miss allocates what its owned
//! result holds and no more.

use std::hint::black_box;

use blaze_alloc_count::{assert_at_most, measure};
use test_fixture::WithFixture;

use crate::{
    ModuleId, file_item_tree,
    nameres::{crate_def_map, facts::impls::module_impls},
    test_db::TestDB,
};

/// A crate whose last file holds only a function body: whitespace there changes no item tree.
const CRATE: &str = r#"
//- /lib.rs crate:main
mod a;
mod b;
pub use a::*;
//- /a.rs
macro_rules! make { ($name:ident) => { pub struct $name; } }
make!(FromMacro);
pub trait Tr {}
impl Tr for FromMacro {}
pub mod inner { pub use super::FromMacro as Again; }
//- /b.rs
pub fn body() {}
"#;

/// (a): collect cold, edit whitespace in `b.rs`'s body twice. The first re-collection warms what
/// a revalidation keeps (salsa's verification scratch, blaze-mem's scratch); the second one is
/// identical and allocates nothing. The item tree of the edited file is made again outside the
/// measure: it is `Tag::Parse`'s miss, not collection's, and it backdates.
#[test]
fn a_recollection_after_an_edit_that_changes_no_item_tree_allocates_nothing() {
    let (mut db, files) = TestDB::with_many_files(CRATE);
    let krate = db.fetch_test_crate();
    let b = files[2];
    black_box(crate_def_map(&db, krate));

    let edit = |db: &mut TestDB, text: &str| {
        let file = b.file_id(db);
        db.set_file_text(file, text);
        black_box(file_item_tree(db, b.into(), krate));
    };
    let recollect = |db: &TestDB| {
        measure(|| {
            let def_map = crate_def_map(db, krate);
            black_box(def_map.root);
        })
        .1
    };

    edit(&mut db, "pub fn body() { }\n");
    recollect(&db);
    edit(&mut db, "pub fn body() {  }\n");
    let executed = db.log_executed(|| {
        black_box(crate_def_map(&db, krate));
    });
    assert!(
        !executed.iter().any(|it| it.contains("crate_local_def_map")),
        "a whitespace edit in a body re-collected the crate: {executed:#?}"
    );

    edit(&mut db, "pub fn body() {   }\n");
    let used = recollect(&db);
    assert_eq!(
        used.allocations, 0,
        "re-collecting after an edit that changes no item tree allocated {} times ({} bytes)",
        used.allocations, used.bytes
    );
}

/// (a), the hit path: once collected, reading a module's scope through the def map allocates
/// nothing.
#[test]
fn reading_a_collected_scope_allocates_nothing() {
    let (db, files) = TestDB::with_many_files(CRATE);
    let krate = db.fetch_test_crate();
    let a = db.module_for_file(files[1].file_id(&db));
    let def_map = crate_def_map(&db, krate);
    let name = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("FromMacro"));
    let warm = def_map.module(&db, a).name_in_scope(&db, &name);
    assert!(warm.take_types().is_some(), "the fixture's `FromMacro` is not in `a`");
    black_box(def_map.module(&db, a).traits_in_scope(&db).count());
    assert_at_most(0, "a hit of a collected module's name", || {
        let def_map = crate_def_map(&db, krate);
        black_box(def_map.module(&db, a).name_in_scope(&db, &name));
        black_box(def_map.module(&db, a).traits_in_scope(&db).count());
    });
}

/// The boxes a `ModuleImpls` owns: an empty box allocates nothing.
fn owned_boxes(lens: &[usize]) -> u64 {
    lens.iter().filter(|&&len| len > 0).count() as u64
}

/// What one first run of `f` allocates, with everything it reads already computed.
fn miss(f: impl FnOnce()) -> u64 {
    measure(f).1.allocations
}

/// (b): a miss of `module_impls` allocates what its result holds and no more. Salsa's own cost of
/// a run (its memo, its dependency list) is read from the same query on a module whose result
/// owns nothing: both runs read the same inputs, so they record the same dependencies. A first
/// run on a third module (the root) takes the query's one-time setup outside both measures.
#[test]
fn a_miss_of_module_impls_allocates_its_result_alone() {
    let (db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:main
mod plain;
mod with_impls;
pub struct Root;
//- /plain.rs
pub struct P;
//- /with_impls.rs
pub struct S;
pub trait Tr {}
impl S { fn f() {} }
impl Tr for S {}
impl Tr for () {}
"#,
    );
    let krate = db.fetch_test_crate();
    let plain = db.module_for_file(files[1].file_id(&db));
    let with_impls = db.module_for_file(files[2].file_id(&db));
    let warm = |module: ModuleId| {
        let def_map = crate_def_map(&db, krate);
        black_box(def_map.module(&db, module));
    };
    let root = db.module_for_file(files[0].file_id(&db));
    warm(root);
    warm(plain);
    warm(with_impls);
    black_box(module_impls(&db, root));

    let salsa = miss(|| {
        black_box(module_impls(&db, plain));
    });
    let empty = module_impls(&db, plain);
    assert_eq!(
        owned_boxes(&[empty.trait_impls.len(), empty.inherent_impls.len(), empty.builtin_derive_impls.len()]),
        0,
        "the baseline module has impls"
    );

    let used = miss(|| {
        black_box(module_impls(&db, with_impls));
    });
    let impls = module_impls(&db, with_impls);
    let owned = owned_boxes(&[
        impls.trait_impls.len(),
        impls.inherent_impls.len(),
        impls.builtin_derive_impls.len(),
    ]);
    assert_eq!((impls.trait_impls.len(), impls.inherent_impls.len()), (2, 1), "fixture changed");
    assert_eq!(
        used,
        salsa + owned,
        "a miss of `module_impls` allocated {used}: salsa's run is {salsa}, its result owns {owned} boxes"
    );
}

/// (b): a miss of `ModuleScope::traits` allocates its box (none when the module has no trait),
/// on top of the same run's cost on a module without traits.
#[test]
fn a_miss_of_a_modules_traits_allocates_its_box_alone() {
    let (db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:main
mod none;
mod some;
//- /none.rs
pub struct N;
//- /some.rs
pub trait A {}
pub trait B {}
pub struct S;
"#,
    );
    let krate = db.fetch_test_crate();
    let none = db.module_for_file(files[1].file_id(&db));
    let some = db.module_for_file(files[2].file_id(&db));
    let def_map = crate_def_map(&db, krate);
    let (none_data, some_data) = (def_map.module(&db, none), def_map.module(&db, some));
    // The query's one-time setup, outside both measures: the root has no trait either.
    black_box(def_map.module(&db, def_map.root).traits_in_scope(&db).count());

    let salsa = miss(|| {
        black_box(none_data.traits_in_scope(&db).count());
    });
    assert_eq!(none_data.traits_in_scope(&db).count(), 0, "the baseline module has traits");

    let used = miss(|| {
        black_box(some_data.traits_in_scope(&db).count());
    });
    let traits = some_data.traits_in_scope(&db).count();
    assert_eq!(traits, 2, "fixture changed");
    let owned = u64::from(traits > 0);
    assert_eq!(used, salsa + owned, "salsa's run is {salsa}, the result owns {owned} box");
}
```

When mem-db-salsa makes a run's memo and dependency list allocation-free (its pooled memos), the
baseline `salsa` measured here goes to 0, and both (b) tests then hold a miss to exactly
`owned`. They need no edit for that. A crate-wide miss (`crate_local_def_map` cold) is not held to
an exact count until the A4 tables leave the global heap (notes, "Residual"). It is held by (a)
from the warm side.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:25): not ours: `nameres/tests/` is db's. `crate_def_map` runs the facts resolver now, so these budgets measure it. A miss's count includes the resolver's owned memo values (`ModuleDecls`, `SccState`, `MacroStep` per module and round), not only the result map's boxes. Size each budget from a run at the first build, not from the old collector's counts.

## Part G. Salsa cycles

No nameres query has `cycle_fn`, `cycle_initial` or `cycle_result` (`rg` over `nameres.rs` and
`nameres/`). The one in hir-def is `expr_store/lower.rs:4236`, outside this slice. The collector's
fixpoints (`resolution_loop`, `FIXED_POINT_LIMIT`) are loops inside one run, so one task. The facts
rounds are separate salsa keys (`RoundKey(krate, r)`). A round reads only round `r - 1`, so the
graph is acyclic by construction (`nameres-core.md` §3). What a round keeps for the next is the
owned memo `SccState { members, handles, prelude, upstream, unresolved: Box<[ImportOp]>, state: Arc<UnitState> }`
and `MacroStep { ops, pending, side, new_modules, .. }`. Both are owned and made at exact size per
C2 to C4, and nothing in them points into a frame.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

> frontend-nameres (2026-10-29 21:25): confirmed acyclic, with two updates since this was written. (1) With the active set (`frontend-nameres-active`), a round reads not only round `r - 1` but any earlier round: `fact_handle(since[M], M)` and `scc_state(ran)`. Every such key names an earlier round, or an upstream component of the same round, so the graph is still acyclic. The one same-round read across components is `round_active(r)` reading the `#[prelude_import]` holder's `scc_state(r)`. It is acyclic because the import graph puts the holder upstream of every module (`nameres-core.md` §9). (2) `SccState.unresolved` is now `imports: Box<[(ImportOp, Option<PerNs>)]>`, made at `Final` only at exact size, and `ModuleDecls::side`/`MacroStep::side` are `Arc<SideTables>`. All are owned, and none points into a frame.

## Part H. The building `DefMap` in the frame: the collector's side of R10 (2026-10-29, second pass)

> deferred by db-expand-names (2026-10-29 ~21:20) with R10 (Andrew's decision at 21:05; see R10 in
> `v2/requests/mem-db-def-resolve.md`). Not started. H6 sending the facts side to frontend-nameres
> answers R10's stale-premise note (the cold path is `nameres/facts/`). It still waits for the
> tree it rewrites to build and run once in an ix VM: H1-H3, R1-R5/T1-T3, the facts switch-over
> and frozen encoding 11 are all unbuilt. A1-A5, A8, B1 and B4, which Part H supersedes, are left
> as they are until then.

mem-db-def's ruling (`docs/v2/plan/lanes/mem/db-def.md`, "Ruling: the scopes a collection builds
live in its frame") puts the def map being collected in the collector's frame. mem-db-def-resolve
wrote the types and readers as **R10** of `v2/requests/mem-db-def-resolve.md`, and mem-db-def
accepted R10's shape over its ruling's `ItemScope<A>`/`DefMap<A>` names (21:17).

**R10 is the spec, and T5 is its test.** The view (`DefMapRead`, `ExternPreludeRead`,
`ResolvePath`, R10.1), `ItemScope<S: ScopeStore = Settled>` with `Building<'f>` (R10.2, R10.6),
`BuildingDefMap<'f>` and `BuildingModule<'f>` with their methods, `settle`, `DefMap::from_settled`
and `DefMapParts` (R10.3, R10.10), `owned_allocations` (R10.7), `Declared`/`reserve_declared`
(R10.8) and `Visibility::max`/`min` (R10.9) are R10's. Part H does not restate them. It writes
only what R10 leaves to the collector:

- the sites in `collector.rs`, `uses.rs`, `attr_resolution.rs`, the facts driver and the frozen
  read that make, read or hand over the building map (H2, H3, H6, H7);
- the item-tree capacity hints (H4);
- the helpers R10 assigns to this lane: `BuildingLocal<'f>` ("collect's A5 frame list"),
  `exact_map`/`exact_map_with`/`settled_vec` ("collect's"), and the two `BuildingDefMap` methods
  the collector needs beyond R10.3's list (H1);
- the exact-count test T6 beside T5 (H8).

**Where R10 and Parts A and B disagree, R10 wins:**

| Part A/B line | R10 line that wins | What changes in A/B |
|---|---|---|
| A3 `DefMap::publish(diagnostics, helpers)` | R10.3: `diagnostics` and `derive_helpers_in_scope` are `BuildingDefMap` fields, settled by `settle` | `publish` is not added. A3's field types (`Box<[DefDiagnostic]>`, `FxHashMap<_, Box<[DeriveHelper]>>`) stay: they are what R10.3's `settle` makes |
| A4 `DefMap::settle(&mut self)` in place | R10.3 `BuildingDefMap::settle(self, db) -> DefMap` | A4 is not applied. `DefMap::shrink_to_fit` and `hand_over_scopes` go with R10.3 |
| A5's `LocalDefMap` row, B1's `extern_prelude` row (a frame list published at `finish`) | R10.3: the in-progress `LocalDefMap` is `BuildingLocal<'f>`, read through `ExternPreludeRead` | read during collection, not hidden until `finish` (the correction below) |
| A8 `Box<BuildingScope>` kept in `ScopeSlot` | R10.3: `ScopeSlot`, `ModulesSlot`, `BuildingScope` go; `ModuleData.scope` is a `salsa::Id` | A8 is not applied |
| B1 `def_map: DefMap` and its `diagnostics`/`derive_helpers` staging rows | R10.3: `def_map: BuildingDefMap<'f>` | no staging fields; the collector writes the building map's fields |
| B4 `finish` publishing into `DefMap` | R10.3's `collect_defs` returning `(def_map.settle(db), local)` | B4's `name_records` part stands; its publish part is H3's |
| `DefMapRead for BuildingDefMap::child` with `name.clone()` (my H1, first pass) | R10.10: `(module, *name)`, `Name` is `Copy` | taken |

A1 and A2 stand, rewritten (they open the task, which salsa doesn't, and don't depend on R10).
H2 is their form once R10 lands.

Owners: `nameres.rs`, `collector.rs`, `uses.rs`, `attr_resolution.rs`, `frozen.rs`,
`tests/budget.rs`: **db-expand-names** (window `db-expand-names-next2`). `facts/driver.rs`,
`facts/env.rs`: **frontend-nameres** (window `frontend-nameres-next1`).

**Correction to A5/B1.** They staged the extern prelude in a frame list published only at
`finish`. Path resolution reads it while the crate is collected (`path_resolution.rs` 433, 466,
485, 760, 789, 809), so hiding it until `finish` would change what imports resolve to. R10.3 and
H1's `BuildingLocal<'f>` keep it readable from the first insert on.

Part F's closing paragraph about the cold `crate_local_def_map` gives way to T5 (R10) and T6 (H8).
Everything else in Parts A to G stands.

> forwarded to frontend-nameres (frontend-nameres-next1: the collector, facts, frozen read, T6) and db (db-expand-names-next2: H1's `nameres.rs` beside R10.3) (requests, pass 21; told by tmux).

> frontend-nameres (2026-10-29 21:29): Part H is deferred with R10 (above), and the forward gives this lane more than Part H does. Going by Part H's owners line, this lane has two pieces. One is H6, `facts/driver.rs` and `facts/env.rs` (answered under H6 at 21:25). The other is H2's paragraph on the two differential helpers in `facts/tests.rs` (81, 125): each wraps its body in `frame::task(Tag::Nameres, ..)`, builds `BuildingDefMap::new_in(frame, ..)` and calls `collect_defs`. Both are accepted and land in one change with R10.3's `facts/` part, H6 and T5, once R10's types are in the tree. The rest belongs to db-expand-names, the owner this part names: `collector.rs` (H3, H4), `frozen.rs` (H7), `nameres/tests/budget.rs` (T6/H8) and `nameres.rs` (H1, H2's two queries). This lane does not take them. For H5, the facts `collect_block` calls `settle` once, last, as H6 has it. H9 needs nothing from this lane.

### H1. `nameres.rs`: what this slice adds next to R10.3

> outcome (requests pass 21, db-expand-names, 2026-10-29 ~21:22): deferred with R10 and the rest
> of Part H, not applied. H1 is the bodies of R10.3's `BuildingDefMap`. It needs R10.2's
> `ItemScope::new_in(frame)`, `item_scope::{Building, ScopeStore}`, R10.1's
> `DefMapRead`/`ExternPreludeRead` and H1's `blaze-mem`/`hashbrown` dependencies, and none of these
> exist yet. It lands in the same change as R10 once hir-def's current tree has built
> (Andrew's decision, 21:05).

> trimmed 2026-10-29 ~21:30 (mem-db-def-collect, on mem-db-def's 21:17 instruction): the
> `BuildingDefMap` bodies are gone from H1, since R10.3 specifies that type. What is left is what R10
> assigns to this lane.

Imports added at the top of `nameres.rs` (with Part A's aliases):

```rust
use std::alloc::Allocator;

use blaze_mem::{Frame, Tag};
use rustc_hash::FxBuildHasher;

use crate::item_scope::{Building, ScopeStore};
use self::path_resolution::{DefMapRead, ExternPreludeRead};
```

**The root id**, moved out of `DefMap::empty` (639 to 654, which goes) so that
`BuildingDefMap::new_in` and the frozen read (H7) make it the same way:

```rust
/// The id of the root module of a def map of `krate` (a block's, with `block`).
fn root_module_of(db: &Db, krate: Crate, block: Option<BlockId>) -> ModuleId {
    ModuleIdLt::new(db, krate, block, None, Name::new_symbol_root(sym::__empty)).to_static()
}
```

**`BuildingDefMap` itself is R10.3's**: its fields, `new_in` (with R10.10's root
`(ModuleOrigin, Visibility)`), `add_module`, `add_child`, `module`, `module_mut`, `modules`, its
`DefMapRead` impl and `settle`. Part H does not restate them. It needs one property of them and
three more methods.

The property: the collector adds a module id that is already in the map when a module declares
the same `mod` name twice (`ModuleIdLt` is interned by parent and name). Today's `FxIndexMap::insert`
keeps the id's place, takes the new data and leaves the old children map behind. So
`add_module` of an id already present keeps its place in `modules`, makes it empty again, and
removes its old children from `child_at`. `add_child` of a name already present keeps the name's
place in the parent's `children`, takes the new id, and returns the id it replaced
(`Option<ModuleId>`, which `push_child_module` reads, H3).

The two methods the collector calls that R10.3 doesn't list:

```rust
impl<'f> BuildingDefMap<'f> {
    /// Room for `additional` more modules: a capacity hint from the item tree (H4).
    pub(crate) fn reserve_modules(&mut self, additional: usize) {
        self.modules.reserve(additional);
        self.module_at.reserve(additional);
        self.child_at.reserve(additional);
    }

    /// Two different modules at once, for a write to one from a read of the other; `None` when
    /// either is not here.
    pub(crate) fn module_pair_mut(
        &mut self,
        a: ModuleId,
        b: ModuleId,
    ) -> Option<[&mut BuildingModule<'f>; 2]> {
        assert_ne!(a, b, "a module paired with itself");
        let (a, b) = (*self.module_at.get(&a)? as usize, *self.module_at.get(&b)? as usize);
        let [a, b] = self.modules.get_disjoint_mut([a, b]).ok()?;
        Some([&mut a.1, &mut b.1])
    }
}
```

`module_pair_mut` keeps `FxIndexMap::get_disjoint_mut`'s panic on equal keys.

**What the collector reads of the building map besides `DefMapRead`.** `collector.rs` calls these
on its `DefMap` today. On `BuildingDefMap` they become:

| Call (count in `collector.rs`) | After |
|---|---|
| `def_map.root_module_id()` (3) | `def_map.root()` (`DefMapRead`) |
| `def_map.block_id()` (3), `def_map.crate_root(db)` (1) | unchanged: `DefMapRead`'s provided methods |
| `def_map.module(id)` (3) | R10.3's `module(id)`, the fields read directly (`.scope`, `.parent`) |
| `def_map.recursion_limit()` (6), `features()` (1), `is_no_std()` (1), `is_no_core()` (1), `is_builtin_or_registered_attr(..)` (1) | `def_map.data.recursion_limit()`, `.features()`, `.is_no_std()`, `.is_no_core()`, `.is_builtin_or_registered_attr(..)` |
| `resolve_path*`, `path_uses*`, `macro_via` | unchanged calls: `ResolvePath` (R10.1) and `RecordUses` (H3) |

For the fifth row, those five bodies move to `DefMapCrateData`, each reading its own fields:
`recursion_limit(&self) -> u32 { self.recursion_limit.unwrap_or(128) }`, `features` returns
`&self.unstable_features`, `is_no_std` is `self.no_std || self.no_core`, `is_no_core` is
`self.no_core`, and `is_builtin_or_registered_attr` (`attr_resolution.rs` 81) has
`self.data.registered_tools` become `self.registered_tools`. `DefMap`'s five methods call them on
`self.data`, so the readers outside nameres don't change. `resolve_attr_macro` (`attr_resolution.rs` 30, called by
`assoc.rs` 209 on collected maps only) stays on `DefMap`, its first line becoming
`self.data.is_builtin_or_registered_attr(&ast_id.path)`.

**The extern prelude while a crate is collected** (R10.3 calls it "collect's A5 frame list"):

```rust
/// The extern prelude of a crate being collected, in the collector's frame. Path resolution
/// reads it mid-collection, through [`ExternPreludeRead`], and [`Self::settle`] makes the
/// owned [`LocalDefMap`] from it.
pub(crate) struct BuildingLocal<'f> {
    /// In first-insert order, as `FxIndexMap::insert` keeps them.
    entries: Vec<(Name, (ModuleId, Option<ExternCrateId>)), &'f Frame>,
    at: hashbrown::HashMap<Name, u32, FxBuildHasher, &'f Frame>,
}

impl<'f> BuildingLocal<'f> {
    pub(crate) fn with_capacity_in(n: usize, frame: &'f Frame) -> Self {
        BuildingLocal {
            entries: Vec::with_capacity_in(n, frame),
            at: hashbrown::HashMap::with_capacity_and_hasher_in(n, FxBuildHasher, frame),
        }
    }

    /// `FxIndexMap::insert`: a name already there keeps its place and takes `value`.
    pub(crate) fn insert(&mut self, name: Name, value: (ModuleId, Option<ExternCrateId>)) {
        match self.at.entry(name) {
            hashbrown::hash_map::Entry::Occupied(it) => self.entries[*it.get() as usize].1 = value,
            hashbrown::hash_map::Entry::Vacant(it) => {
                let at = u32::try_from(self.entries.len()).expect("an extern prelude of 2^32 crates");
                self.entries.push((*it.key(), value));
                it.insert(at);
            }
        }
    }

    pub(crate) fn get(&self, name: &Name) -> Option<(ModuleId, Option<ExternCrateId>)> {
        self.at.get(name).map(|&at| self.entries[at as usize].1)
    }

    /// The owned extern prelude, in insertion order: its entries and its index, one allocation
    /// each, none when empty.
    pub(crate) fn settle(self) -> LocalDefMap {
        // alloc: the memo's extern prelude, at its length
        let mut extern_prelude =
            FxIndexMap::with_capacity_and_hasher(self.entries.len(), Default::default());
        extern_prelude.extend(self.entries);
        LocalDefMap { extern_prelude }
    }
}

impl ExternPreludeRead for BuildingLocal<'_> {
    fn extern_crate(&self, name: &Name) -> Option<(ModuleId, Option<ExternCrateId>)> {
        self.get(name)
    }
}

/// The extern prelude a collection resolves in: the crate's collected one while a block is
/// collected, the collector's own while the crate is.
#[derive(Clone, Copy)]
pub(crate) enum LocalRead<'a, 'f> {
    Crate(&'a LocalDefMap),
    Building(&'a BuildingLocal<'f>),
}

impl<'a, 'f> LocalRead<'a, 'f> {
    /// Takes the two fields, not the collector, so a caller can hold `&mut` to its other fields.
    pub(crate) fn of(crate_local: Option<&'a LocalDefMap>, own: &'a BuildingLocal<'f>) -> Self {
        match crate_local {
            Some(it) => LocalRead::Crate(it),
            None => LocalRead::Building(own),
        }
    }
}

impl ExternPreludeRead for LocalRead<'_, '_> {
    fn extern_crate(&self, name: &Name) -> Option<(ModuleId, Option<ExternCrateId>)> {
        match self {
            LocalRead::Crate(it) => it.extern_crate(name),
            LocalRead::Building(it) => it.extern_crate(name),
        }
    }
}
```

`LocalDefMap::shrink_to_fit` (188) goes. `LocalDefMap`'s `Default` derive stays (`EMPTY`, tests).

**The settled helpers** R10.3's `settle` calls. They replace A4's `exact_map`, and take any
source allocator:

```rust
/// `map` made again on the global heap at exactly its length: one allocation, none when empty.
fn exact_map<K: Hash + Eq, V, A: Allocator>(
    map: hashbrown::HashMap<K, V, FxBuildHasher, A>,
) -> FxHashMap<K, V> {
    exact_map_with(map, |it| it)
}

/// [`exact_map`], each value made by `f`.
fn exact_map_with<K: Hash + Eq, V, W, A: Allocator>(
    map: hashbrown::HashMap<K, V, FxBuildHasher, A>,
    mut f: impl FnMut(V) -> W,
) -> FxHashMap<K, W> {
    // alloc: the memo's map, at its length
    let mut out = FxHashMap::with_capacity_and_hasher(map.len(), Default::default());
    out.extend(map.into_iter().map(|(k, v)| (k, f(v))));
    out
}

/// A memo's list from a frame one: one allocation of exactly its length, none when empty.
fn settled_vec<T, A: Allocator>(v: Vec<T, A>) -> Box<[T]> {
    // alloc: the memo's list, at its length
    let mut out = Vec::with_capacity(v.len());
    out.extend(v);
    out.into_boxed_slice()
}
```

(`into_boxed_slice` of a vector filled to its capacity does not reallocate.)

**`DefMap::from_table`.** R10.3's `from_settled` makes a `ModuleScope` per settled scope and then
the `Modules`. The facts driver (H6) already has a `ModuleScope` handle for every module, so the
second half is split out, and `from_settled` ends by calling it:

```rust
impl DefMap {
    /// A def map from a table whose every scope is handed over already: makes its `Modules` and
    /// groups the diagnostics.
    pub(crate) fn from_table(db: &Db, parts: DefMapParts, table: ModulesMap) -> DefMap {
        let DefMapParts {
            krate, block, root, prelude, data, macro_use_prelude, derive_helpers_in_scope,
            diagnostics, names, session_macros,
        } = parts;
        DefMap {
            diagnostics_by_module: DiagnosticsByModule::of(&diagnostics),
            modules: Modules::new(db, root, table).as_id(),
            krate, block, root, prelude, data, macro_use_prelude, derive_helpers_in_scope,
            diagnostics, names, session_macros,
        }
    }
}
```

and `from_settled`'s last statement is `DefMap::from_table(db, parts, table)`.

**`DefMap::set_diagnostics`**, for the facts driver, which adds diagnostics after the
`Modules` exists (H6):

```rust
    /// The def map's diagnostics, once, from a frame list, grouped by module.
    pub(super) fn set_diagnostics<A: Allocator>(&mut self, diagnostics: Vec<DefDiagnostic, A>) {
        always!(self.diagnostics.is_empty());
        self.diagnostics = settled_vec(diagnostics);
        self.diagnostics_by_module = DiagnosticsByModule::of(&self.diagnostics);
    }
```

**`DefMapCrateData`** (A5 refined): `registered_tools: Box<[Symbol]>`, and `new` sets
`registered_tools: Box::default()`. The collector fills it once (H4), and the frozen read sets it
from its list. `DefMapCrateData::shrink_to_fit` (327) goes: H4 makes every table of it at its
size or reserved to its bound before the first insert.

### H2. The two queries: A1 and A2 once R10 lands

```rust
#[salsa::tracked(returns(ref))]
pub(crate) fn crate_local_def_map(db: &Db, crate_id: Crate) -> DefMapPair<'_> {
    let _p = tracing::info_span!(
        "crate_def_map_query",
        name=?crate_id
            .extra_data(db)
            .display_name
            .as_ref()
            .map(|it| it.crate_name().to_smolstr())
            .unwrap_or_default()
    )
    .entered();

    blaze_mem::frame::task(Tag::Nameres, |frame| {
        if let Some((def_map, local_def_map)) = frozen::served_in(db, crate_id, frame) {
            return DefMapPair::new(db, def_map, local_def_map);
        }
        let (def_map, local_def_map) = facts::collect_crate(db, frame, crate_id);
        frozen::check(db, crate_id, &def_map, &local_def_map);
        DefMapPair::new(db, def_map, local_def_map)
    })
}

#[salsa::tracked(returns(ref))]
pub fn block_def_map<'db>(db: &'db Db, block_id: BlockIdLt<'db>) -> DefMap {
    let block_id = block_id.to_static();
    let ast_id = block_id.ast_id(db);
    let module = block_id.module(db).to_static();
    let visibility = Visibility::Module(module, VisibilityExplicitness::Implicit);

    blaze_mem::frame::task(Tag::Nameres, |frame| {
        let krate = module.krate(db);
        let local_def_map = crate_local_def_map(db, krate);
        let def_map = BuildingDefMap::new_in(
            frame,
            db,
            krate,
            local_def_map.def_map(db).data.clone(),
            (ModuleOrigin::BlockExpr { block: ast_id, id: block_id }, visibility),
            Some(BlockInfo { block: block_id, parent: module }),
        );
        facts::collect_block(
            db,
            def_map,
            TreeId::new(ast_id.file_id, Some(block_id)),
            local_def_map.local(db),
        )
    })
}
```

Each query opens its own task because salsa's
`execute` opens none (`vendor/salsa` uses blaze-mem only for memo tables). `frame::current`
outside a task stops (`frame.rs` 497), and a nested query run inside an outer task must not
allocate into the outer query's frame. A task costs two thread-local accesses once a thread has a
frame at that depth. So `crate_local_def_map(db, krate)`, called inside the block's task, runs its
own task one level deeper on a miss and leaves the block's frame alone. Neither query calls
`hand_over_scopes` (R10.3).

`collector::collect_defs` (H3) is the differential reference, called only by
`facts/tests.rs` (81, 125) outside any query. Each of those two helpers wraps its body in
`blaze_mem::frame::task(Tag::Nameres, |frame| ..)`, builds `BuildingDefMap::new_in(frame, ..)` with
the root origin it builds today, and calls `collect_defs` as H3 gives it.

### H3. `collector.rs`: the collector builds `BuildingDefMap<'f>`

Line numbers are as read on 2026-10-29. The function is named with each, since the file moves.

**`collect_defs` (73 to 145):**

```rust
pub(super) fn collect_defs<'f>(
    db: &Db,
    def_map: BuildingDefMap<'f>,
    tree_id: TreeId,
    crate_local_def_map: Option<&LocalDefMap>,
) -> (DefMap, LocalDefMap) {
    let frame = def_map.frame;
    let krate = &def_map.krate.data(db);
    let cfg_options = def_map.krate.cfg_options(db);
    // .. `deps` and `proc_macros` as B1 and E3 ..
    let item_tree = tree_id.item_tree(db, def_map.krate);
    let root_extern_crates = declared(item_tree, item_tree.top_level_items()).extern_crates;
    let mut collector = DefCollector {
        db,
        frame,
        def_map,
        local_def_map: BuildingLocal::with_capacity_in(
            krate.dependencies.len() + root_extern_crates as usize,
            frame,
        ),
        crate_local_def_map,
        // .. every other field as B1 to B3 give it ..
    };
    if tree_id.is_block() {
        collector.seed_with_inner(tree_id);
    } else {
        collector.seed_with_top_level();
    }
    collector.collect();
    let (def_map, local_def_map) = collector.finish();
    (def_map.settle(db), local_def_map.settle())
}
```

`def_map.shrink_to_fit()` and `local_def_map.shrink_to_fit()` (141, 142) go. The capacity hint
for the extern prelude counts the dependencies and the root's `extern crate`s. It is only a hint:
the list is in the frame, and `settle` makes the owned one at its length either way.

**`DefCollector` (750 to 812):** `def_map: DefMap` (753) becomes `def_map: BuildingDefMap<'f>`,
and `local_def_map: LocalDefMap` (754) becomes `local_def_map: BuildingLocal<'f>`. `frame:
&'f Frame` is B1's field.

**`finish` (2749 to 2850):** the signature becomes
`fn finish(mut self) -> (BuildingDefMap<'f>, BuildingLocal<'f>)`. Its body is unchanged up to
`self.def_map.names = self.name_records();` (2847), and it returns
`(self.def_map, self.local_def_map)`. It publishes nothing itself: the diagnostics and derive
helpers are already the building map's, and `settle` makes them exact. This replaces B4's
`finish` body. B4's `name_records` part (frame vectors, `Box::from(&v[..])`) stands.

**Every site that writes or reads the building map.** Each rule below applies to every line
listed with it. After the change, `rg 'modules\.building\(\)|scope_mut\(\)|children_mut\(\)|ModulesSlot|ScopeSlot|ModuleData::new' collector.rs`
finds nothing.

| Lines (function) | Before | After |
|---|---|---|
| 1046 to 1047 (`collect`'s proc-macro censor), 1281, 1300, 1324, 1339 (macro definitions), 1629, 1872, 1993, 2008, 2029, 2037, 2058, 2064, 2076 (`update_recursive`, `update`), 2191, 2250, 2549, 2609, 2628 (`resolve_macros`, `expand_round`), 3004, 3047, 3067, 3150, 3169, 3279 (`ModCollector::collect`), 3816, 3839 (`collect_macro_call`) | `self.def_map.modules.building()[id].scope_mut().f(..)` (and the `def_collector.`/`def_map.` spellings) | `self.def_map.module_mut(id).scope.f(..)` |
| 1851 to 1853 (underscore import), 1977 (`update`) | `self.def_map.modules.building()[id].scope(self.db).f(..)` | `self.def_map.module(id).scope.f(..)` |
| 1892 (`update_held`) | `self.def_map.module(self.db, module_id).scope(self.db)` | `&self.def_map.module(module_id).scope` |
| 1681 to 1706 (`record_resolved_import`, glob of the same crate) | `let scope = if .. { self.def_map.module(self.db, m).scope(self.db) } else { m.def_map(self.db).module(self.db, m).scope(self.db) };` then `self.held.extend(scope.resolutions()..)` | the two scopes are different types now (`ItemScope<Building<'f>>`, `ItemScope`), so each branch calls one generic helper (below) |
| 1857, 1716 (`old_vis.max(.., &self.def_map)`, `old_vis.max(db, vis, def_map)`) | `&DefMap` | unchanged text; `Visibility::max`/`min` take `&impl DefMapRead` (H9, ask 3) |
| 1364 to 1378 (`import_macros_from_extern_crate`) | `self.def_map.macro_use_prelude.insert(name, ..)` | unchanged text: the field is the building map's frame map |
| 2262 to 2271, 2311, 2497 to 2505, 2720 to 2728, 2992 (derive helpers) | `self.def_map.derive_helpers_in_scope.entry(k).or_default()` | `.entry(k).or_insert_with(|| Vec::new_in(frame))` with `let frame = self.frame;` taken before the borrow; `.get`/`.get_mut` unchanged |
| 2722 (`collect_macro_expansion`) | `let derive_helpers = derive_helpers.clone();` | `let derive_helpers = derive_helpers.clone(); // clone: into the frame, to write the map it was read from` (a `Vec<_, &'f Frame>` clones into its own allocator) |
| 913, 1062, 2424, 2565, 2578, 2781, 2799, 2834, 3128, 3473, 3972 and every `def_map.diagnostics.push` | `self.def_map.diagnostics.push(..)` | unchanged text: the field is the building map's frame vector |
| 2698 | `self.def_map.session_macros = true;` | unchanged |
| 1648, 1650, 919 to 962 | `self.def_map.prelude = ..`, `self.def_map.data`, `Arc::get_mut(&mut self.def_map.data)` | unchanged (R10.3 has `prelude` and `data`) |
| 952 (`seed_with_top_level`) | `Arc::get_mut(&mut self.def_map.data).unwrap().shrink_to_fit();` | removed: H4 makes the crate data's tables at their size |
| 2961 to 3000 (`consider_deferred_derives`) | closure parameter `def_map: &mut DefMap`, `let module = &mut def_map.modules.building()[module_id];` then `module.scope_mut().define_builtin_derive_impl(..)`, `module.scope_mut().set_derive_macro_invoc(..)`, then `def_map.derive_helpers_in_scope.get_mut(..)` | parameter `def_map: &mut BuildingDefMap<'f>`, the `let module` line removed, the two calls `def_map.module_mut(module_id).scope.define_builtin_derive_impl(..)` and `def_map.module_mut(module_id).scope.set_derive_macro_invoc(..)` (each re-borrows, so the later `def_map.derive_helpers_in_scope.get_mut` borrows the map alone); the `deferred_derives` parameter is B1's frame map |
| 2126 (`resolve_macros`' `resolver`), 3013 (`resolve_vis`) | closure parameters `def_map: &DefMap`, `local_def_map: &LocalDefMap` | `def_map: &BuildingDefMap<'f>`, `local_def_map: LocalRead<'_, 'f>`, passed on as `&local_def_map` |
| 1147, 1318, 1531, 1575, 2128, 2768, 2857, 3039 to 3040, 3493, 3875 to 3878, 3916 to 3919 | `self.crate_local_def_map.unwrap_or(&self.local_def_map)` (and `self.def_collector.` spellings) | `LocalRead::of(self.crate_local_def_map, &self.local_def_map)` (field borrows, so `&mut self.imports` and the rest stay usable); where it is passed as `&LocalDefMap`, it is passed as `&LocalRead` |
| 1197 to 1199 (`fn local_def_map`) | `fn local_def_map(&mut self) -> &LocalDefMap` | `fn local_def_map(&self) -> LocalRead<'_, 'f> { LocalRead::of(self.crate_local_def_map, &self.local_def_map) }` |
| 1122 (`inject_std_extern_crate`) | `self.local_def_map().extern_prelude().any(|(name, _)| *name == sym::std)` | `self.local_def_map().extern_crate(&Name::new_symbol_root(sym::std)).is_some()` (the same test: the prelude's names are unique) |
| 1189 (`resolve_extern_crate`) | `let Some(&(krate_root, _)) = self.local_def_map().extern_prelude.get(&name) else` | `let Some((krate_root, _)) = self.local_def_map().extern_crate(&name) else` |
| 908 to 911 (`seed_with_top_level`) | `self.local_def_map.extern_prelude.insert(name, (..))` | `self.local_def_map.insert(name, (..))` |
| 3097 to 3107 (`ModCollector::collect`, root `extern crate`) | `self.def_collector.local_def_map.extern_prelude.get(name)` / `.extern_prelude.insert(name.clone(), ..)` | `self.def_collector.local_def_map.get(name)` / `.insert(name.clone(), ..)` |
| 3573 (`resolve_attributes`) | `self.def_collector.def_map.is_builtin_or_registered_attr(&attr.path)` | `self.def_collector.def_map.data.is_builtin_or_registered_attr(&attr.path)` (H1) |
| 3945 to 3954 (`collect_macro_call`'s textual lookup) | `def_map.module(db, self.module_id).scope(self.def_collector.db).get(name).take_macros()` | `def_map.name_in_module(db, self.module_id, name).take_macros()` (`DefMapRead`); `def_map.legacy_macro_in_textual_scope(..)` is R10.1's `ResolvePath` method; `def_map.macro_use_prelude.get(name)` unchanged |
| 3962 to 3979 (`import_all_legacy_macros`) | `let [Some(source), Some(target)] = self.def_collector.def_map.modules.building().get_disjoint_mut([&module_id, &self.module_id]) else { return };` then `source.scope(db).legacy_macros()`, `target.scope_mut().define_legacy_macro(..)` | `let Some([source, target]) = self.def_collector.def_map.module_pair_mut(module_id, self.module_id) else { return };` then `source.scope.legacy_macros()`, `target.scope.define_legacy_macro(..)` |
| 3525 to 3550 (`push_child_module`) | below | below |
| `use` list (55) | `DefMap, LocalDefMap, .., ModuleData, ..` | `BuildingDefMap, BuildingLocal, LocalDefMap, LocalRead, ..`, without `ModuleData`; plus `path_resolution::{DefMapRead, ResolvePath}` and `item_scope::{ItemScope, ScopeStore}` |

The glob helper for 1681 to 1706, a free function in `collector.rs`:

```rust
/// Appends to `held` what `scope` binds that `module_id` sees, for a glob of the same crate.
fn hold_visible<S: ScopeStore>(
    held: &mut FrameVec<'_, (Option<Name>, PerNs)>,
    scope: &ItemScope<S>,
    db: &Db,
    def_map: &BuildingDefMap<'_>,
    module_id: ModuleId,
) {
    held.extend(
        scope
            .resolutions()
            .map(|(n, res)| {
                (n, res.filter_visibility(|v| v.is_visible_from_def_map(db, def_map, module_id)))
            })
            .filter(|(_, res)| !res.is_none()),
    );
}
```

and the branch becomes:

```rust
                            let start = self.held.len();
                            if m.block(self.db) == self.def_map.block_id() {
                                let scope = &self.def_map.module(m).scope;
                                hold_visible(&mut self.held, scope, self.db, &self.def_map, module_id);
                            } else {
                                let scope = m.def_map(self.db).module(self.db, m).scope(self.db);
                                hold_visible(&mut self.held, scope, self.db, &self.def_map, module_id);
                            }
                            self.update_held(module_id, start, vis, Some(ImportOrExternCrate::Glob(glob)));
```

`push_child_module`, from `let def_map = &mut self.def_collector.def_map;` (3525) to the
`declare` (3543):

```rust
        let def_map = &mut self.def_collector.def_map;
        def_map.add_module(module, origin, vis, Some(self.module_id));
        let [parent, child] = def_map
            .module_pair_mut(self.module_id, module)
            .expect("a module and the module it declares are both in the def map");
        for (name, macs) in parent.scope.legacy_macros() {
            for &mac in macs {
                child.scope.define_legacy_macro(*name, mac);
            }
        }
        let replaced = def_map.add_child(self.module_id, name, module);
        if replaced.is_some() {
            // A path can read whether a module is its parent's `__blaze_test_harness`.
            self.def_collector.imports.wake_all();
        }

        let def = ModuleDefId::from(module);

        self.def_collector.def_map.module_mut(self.module_id).scope.declare(def);
```

This is the same state as before: the old code filled a new `ModuleData`'s legacy macros from the
parent and then inserted it, replacing a module of the same id in place. `add_module` replaces it
in place, empty, and the loop fills it.

**`uses.rs`** (D2 refined). `impl DefMap` (150 to 357: `path_uses`, `path_uses_outside_imports`,
`used_at`, `textual_macro`, `macro_via`, `ident_macro_via`) is called on the building map by
`name_records`, so it becomes the provided methods of a trait over R10's read trait, as R10.1
does for path resolution:

```rust
pub(crate) trait RecordUses: ResolvePath {
    fn path_uses<L: ExternPreludeRead, A: Allocator>(
        &self,
        q: (&L, &Db, ResolveMode),
        // .. the other parameters unchanged ..
        out: &mut Vec<Use, A>,
    ) { .. }
    fn path_uses_outside_imports<A: Allocator>(
        &self,
        local_def_map: &impl ExternPreludeRead,
        // .. unchanged ..
        out: &mut Vec<Use, A>,
    ) { .. }
    fn macro_via(&self, local_def_map: &impl ExternPreludeRead, /* unchanged */) -> Option<(MacroId, MacroVia)> { .. }
    // `used_at`, `textual_macro`, `ident_macro_via`: unchanged parameters
}

impl<M: DefMapRead> RecordUses for M {}
```

Inside the bodies: `self.krate` (176) → `self.krate()`, `self.data.edition` (243) →
`self.edition()`, `self.block` (247, 278, 342) → `self.block()`, `self.root` (248, 342) →
`self.root()`, `self.module(db, self.root).name_in_scope(db, name)` (248) →
`self.name_in_module(db, self.root(), name)`, `self.module(db, module).legacy_macro_in_scope(db, name)`
(273) → `self.legacy_macros(db, module, name)`, `self.module(db, module).name_in_scope(db, name)`
(339) → `self.name_in_module(db, module, name)`, `self.macro_use_prelude.get(name)` (352) →
`self.macro_in_use_prelude(name)`. The reads of `parent.def_map(db)` stay (always collected). The
IDE callers keep their calls and add `use crate::nameres::uses::RecordUses;`.

### H4. Capacity hints from the item tree

A module's columns are reserved in the frame from the items it declares, counted once when its
items are collected. They are hints only: the lists are in the frame, and `settle` makes the
owned ones at their length either way. The hint saves the frame's growth copies.

In `collector.rs`, next to `ModCollector`:

```rust
/// How many of each kind of item a list declares: the capacity hint for the scope they go to.
fn declared(item_tree: &ItemTree, items: &[ModItemId]) -> Declared {
    let mut d = Declared::default();
    for &item in items {
        match item_tree.index(item) {
            ModItemKind::Const(_, it) => match item_tree.get_name(it.name) {
                Some(_) => d.named += 1,
                None => d.unnamed_consts += 1,
            },
            ModItemKind::Enum(..)
            | ModItemKind::Function(..)
            | ModItemKind::Macro2(..)
            | ModItemKind::Static(..)
            | ModItemKind::Struct(..)
            | ModItemKind::Trait(..)
            | ModItemKind::TypeAlias(..)
            | ModItemKind::Union(..) => d.named += 1,
            ModItemKind::Mod(..) => {
                d.named += 1;
                d.mods += 1;
            }
            ModItemKind::ExternBlock(..) => d.extern_blocks += 1,
            ModItemKind::ExternCrate(..) => d.extern_crates += 1,
            ModItemKind::Impl(..) => d.impls += 1,
            ModItemKind::MacroCall(..) => d.macro_calls += 1,
            ModItemKind::MacroRules(..) => d.macro_rules += 1,
            ModItemKind::Use(..) => d.uses += 1,
        }
    }
    d
}
```

`Declared` (with `mods`, which the scope ignores and the module table reads) and
`reserve_declared` are R10.8's.
`ModCollector::collect` (2951) begins:

```rust
        let declared = declared(self.item_tree, items);
        let def_map = &mut self.def_collector.def_map;
        def_map.reserve_modules(declared.mods as usize);
        let module = def_map.module_mut(self.module_id);
        module.scope.reserve_declared(&declared);
        module.children.reserve(declared.mods as usize);
```

`collect` runs once per item list that goes into a module: the module's own items, and each macro
expansion's. So the hint adds up as expansions add items.

**The crate data's tables, at their size** (seed, `seed_with_top_level` 820 to 863). These tables
are owned from the start: `Arc<DefMapCrateData>` is shared with the block maps, so it is not a
frame value.

- `#![feature]` and `#![register_tool]` (849 to 862): the loop pushes the feature symbols and the
  tool names into two frame vectors, `features: FrameVec<'f, Symbol>` and
  `tools: FrameVec<'f, Symbol>`, in place of `enable` and `push`. After the loop, at 863:

```rust
        crate_data.unstable_features.reserve(features.len());
        for feature in features {
            crate_data.unstable_features.enable(feature);
        }
        // alloc: the crate's tools, at their number
        let mut registered = Vec::with_capacity(PREDEFINED_TOOLS.len() + tools.len());
        registered.extend(PREDEFINED_TOOLS.iter().map(|it| Symbol::intern(it)));
        registered.extend(tools);
        crate_data.registered_tools = registered.into_boxed_slice();
        crate_data.proc_macro = self.is_proc_macro;
        if self.is_proc_macro {
            let n = self.proc_macros.len();
            crate_data.exported_derives.reserve(n);
            crate_data.fn_proc_macro_mapping.reserve(n);
            crate_data.fn_proc_macro_mapping_back.reserve(n);
        }
```

  The order of `enable` is the attributes' order, as before. `registered_tools` reads the same
  (the predefined tools, then the crate's in order). `UnstableFeatures::reserve` is mem-db-def-items' I11.1.
- `export_proc_macro` (1234 to 1239) inserts at most one entry per proc macro of the crate into
  each map. `self.proc_macros` is the build system's list, so the reservation holds them unless a
  `#[proc_macro]` function is missing from it (`missing_expander`, an error state). Then the map
  grows past the hint, and the budget tests count it by capacity (H8).

### H5. `settle` is R10.3's; what the collector owes it

The copy from the frame to the memo is R10.3's `BuildingDefMap::settle` and `DefMap::from_settled`,
which replace A4. The collector's part:

- It calls `settle` once, last: `collect_defs` (H3) and the facts `collect_block` (H6). It settles
  its `BuildingLocal` beside it (H1's `BuildingLocal::settle`, first-insert order kept).
- The orders R10.3's `settle` keeps are the orders the collector writes in, so the collector
  keeps today's write order. That means modules added as declared, a re-declared module or child
  keeping its first place (H1's property), each scope column pushed as today, derive helpers and
  diagnostics pushed as today. `macro_use_prelude` stays a hash map, and its readers sort
  (`resolver.rs` 834, `frozen.rs` 1893).
- Nothing the run makes outlives it. The building map and the `BuildingLocal` borrow the frame
  for `'f`, which ends when H2's `frame::task` closure returns, and `settle` consumes them and
  returns owned values. So a memo that keeps a frame reference does not compile.

### H6. The facts driver (`facts/driver.rs`, `facts/env.rs`; frontend-nameres)

`collect_crate` (434) assembles the crate's map from the final round's `ModuleScope` handles, so
it never holds a building map. Its table (446 to 459) is exact (`reserve(order.len())`), and each
`ModuleData` becomes R10's `ModuleData { origin, visibility, parent, scope: handle }`. The
`DefMap { .. }` literal (517 to 530) becomes:

```rust
    // `diagnostics` is `CrateOps`' (destructured at 515), C2's `FrameVec<'f, DefDiagnostic>`.
    let mut def_map = DefMap::from_table(
        db,
        DefMapParts {
            krate,
            block: None,
            root,
            prelude: None,
            data,
            macro_use_prelude: FxHashMap::default(),
            derive_helpers_in_scope: exact_map_with(derive_helpers_in_scope, settled_vec),
            diagnostics: Box::default(),
            names: NameRecords::default(),
            session_macros,
        },
        table,
    );
```

Here `diagnostics` and `derive_helpers_in_scope` (493) are C2's frame vector and frame map, and `frame` comes from `facts::collect_crate(db, frame, krate)` (H2). `env.install` (env.rs
58 to 61) fills `macro_use_prelude` on the owned map after `clear` and `reserve(n)`, so it is exact
already. `FinishSink` (773) gains `diagnostics: &'a mut FrameVec<'f, DefDiagnostic>`, and its
`self.def_map.diagnostics.push(it)` (795) becomes `self.diagnostics.push(it)`. The unresolved
imports (561) push into `diagnostics`. The end (575 to 579) becomes
`def_map.set_diagnostics(diagnostics); (def_map, local)`. `hand_over_scopes`, `shrink_to_fit` and
`local.shrink_to_fit()` go: `local` is `CrateEnv::local_def_map`'s, exact already.
`Arc::make_mut(&mut crate_ops.data).shrink_to_fit()` (479) goes with `DefMapCrateData::shrink_to_fit`.
`CrateOps::take`'s inserts into `exported_derives` happen only for proc-macro crates and are
reserved as H4 does, with `n` the number of `DeclOp::ExportedDerive` ops counted in the same walk
before the inserts.

`collect_block` (834) takes `def_map: BuildingDefMap<'f>` (what `block_def_map` now makes) and
returns `DefMap`. Its unit runs over that map (R10.3's last paragraph: a facts query that only
reads a carried scope keeps it `Settled`, and one that writes extends a `to_building` copy). The
end (890 to 908) becomes:

```rust
    let mut unresolved: FrameVec<'f, DefDiagnostic> = Vec::new_in(frame);
    unresolved.extend(
        unit.unresolved_imports()
            .filter(|it| !in_unresolved_extern_crate(&it.path, unit.unresolved_extern_crates()))
            .map(|it| DefDiagnostic::unresolved_import(it.module, it.source.id.lookup(db).id, it.source.use_tree)),
    );
    let names = name_records(db, unit.def_map(), local, unit.import_outcomes(), records);
    let mut def_map = unit.into_def_map();
    def_map.diagnostics.extend(unresolved);
    def_map.names = names;
    always!(side.deferred_derives.is_empty(), /* unchanged */);
    for (item, helpers) in side.derive_helpers.drain() {
        def_map.derive_helpers_in_scope.insert(item, helpers);
    }
    def_map.settle(db)
```

The unresolved imports are read before `into_def_map`, as now. They go into a
`FrameVec<'f, DefDiagnostic>` there (`Vec::new_in(frame)` plus `extend`), and that vector is
`extend`ed into the map. `side` is C4's frame `SideTablesIn<'f>` on the block path, so its helpers
are frame vectors already. `name_records` (facts) takes `&impl DefMapRead`. The unit's own types
(`unit.rs`) adapt to R10 as R10.3 says. That is frontend-nameres', and not restated here.

> frontend-nameres (2026-10-29 21:25): accepted, and blocked with R10. H6 is ours, but it is written against `DefMap::from_table`, `DefMapParts`, `BuildingDefMap<'f>`, `DefMapRead`, `settle` and `set_diagnostics`. None of them exists in `nameres.rs` yet, and R10 was deferred until hir-def's current tree has built (Andrew, 21:05). `facts/driver.rs`/`env.rs` adopt H6 together with C1-C4 in the change that lands R10 and H2, against the R10 types as they land. Owner of the blocker: db (db-expand-names).

### H7. The frozen read (`frozen.rs`)

`decode` (2100 to 2108) and `Dec::def_map` (3064 to 3176) build the map in place today
(`DefMap::empty`, then `def_map.modules = ModulesSlot::Building(table)`, then
`hand_over_scopes`). After R10 they make the parts and call `from_settled`:

- 3090 to 3095: the placeholder `ModuleData` and `DefMap::empty` go. The root id is
  `root_module_of(db, krate, None)` (H1), pushed to `self.own` first as now.
- 3110 to 3126: the table becomes a list of what `from_settled` takes, in the encoding's order
  (which is the collector's insertion order):
  `let mut modules = Vec::with_capacity(skeleton.len());` (in `Dec`'s allocator per D5), then per
  module `modules.push((self.own[index], origin, visibility, parent, scope, children))` with
  `scope: ItemScope` settled by `push_unindexed` then `seal` (R10.2). `children` stays the
  `FxIndexMap` it is (exact: `room` is the encoded length).
- 3128 to 3175: `prelude`, `macro_use_prelude`, `derive_helpers_in_scope` (values
  `into_boxed_slice()` of `many`'s exact vector), the crate data (`registered_tools:
  self.many(Self::symbol)?.into_boxed_slice()`) and `names` are read into locals as now. The
  diagnostics become `self.many(Self::diagnostic)?.into_boxed_slice()`. The function ends:

```rust
        Ok(DefMap::from_settled(
            db,
            DefMapParts {
                krate,
                block: None,
                root: self.own[0],
                prelude,
                data: TArc::new(data),
                macro_use_prelude,
                derive_helpers_in_scope,
                diagnostics,
                names,
                session_macros: false,
            },
            modules.into_iter(),
        ))
```

  `data` is built as a local (`let mut data = DefMapCrateData::new(edition);` at 3068, plus
  `exported_derives`, `fn_proc_macro_mapping`, `fn_proc_macro_mapping_back` set from their exact
  lists) and wrapped once at the end. The `TArc::get_mut(..).ok_or("the def map's crate data is
  shared")` at 3170 goes, since nothing has shared it yet. `session_macros` is `false` because
  the encoder refuses a map with session macros (1805).
- `decode`'s `def_map.hand_over_scopes(db)` (2107) goes.

### H8. T6: a crate collection is held to its result's boxes (`nameres/tests/budget.rs`)

This replaces "a cold `crate_local_def_map` has no exact-count test". mem-db-def-resolve's T5
(same file) holds a first-ever collection to a count that does not depend on how many names a
scope holds. T6 holds a collection to an exact count: the allocations of the memo's own boxes,
computed from the result, plus salsa's measured cost of the run.

**Which run is measured.** A miss of `crate_local_def_map` after an edit that changes an item
tree. The query's code runs from an empty frame and builds every module, scope, children list
and table again, then `settle` copies them out. It is the run the warm loop pays on every
signature edit. The edit (a function's visibility) keeps every salsa identity: the interned ids
(`ModuleIdLt` by parent and name, `FunctionLoc` and the rest by container and `AstId`, the macro
call by its `AstId`) are found, not made, and the `ModuleScope`s and the `Modules` reuse their
slots. A first-ever run differs only by salsa interning those ids: the growth of the interned
hash tables, which M5 of `docs/v2/plan/lanes/mem/db-salsa.md` keeps on the global allocator. That
cost is salsa's, depends on how full each shard's table already is, and is not a property of
collection. T5 covers the first-ever run.

**Salsa's cost of the run** is measured on `twin`, collected in the same way. It has the same
files, the same modules, the same `macro_rules!` and the same call, so its run makes the same
salsa reads and the same kinds of ids. It has none of `main`'s imports, glob, trait or impl. So
`twin`'s allocations, less its own result's boxes, are salsa's cost of a run of this shape. A
collection temporary that went to the global heap in both crates would hide in that baseline.
The twin leaves out the import, glob and impl paths, where the collection's temporaries are, so
those paths cannot hide there, and `main` must own more boxes than `twin` (asserted).

The counts come from the result, through these `#[cfg(test)]` functions, each beside the type it
counts:

`nameres.rs`:

```rust
/// One allocation for a list, set, map or box of `len` elements, none when it is empty.
#[cfg(test)]
fn nonempty(len: usize) -> u64 {
    u64::from(len > 0)
}

#[cfg(test)]
impl DefMap {
    /// The global allocations this def map's own boxes took when `settle` (or `from_settled`)
    /// made them: one per non-empty list, box, set and hash map, two per non-empty index map (its
    /// entries and its index), and its scopes' own. Salsa's storage of the `ModuleScope`s, the
    /// `Modules` and the `DefMapPair` is salsa's, not counted. Counts no diagnostic's own boxes:
    /// asserts there are none.
    pub(crate) fn owned_allocations(&self, db: &Db) -> u64 {
        let DefMap {
            krate: _,
            block,
            root: _,
            modules: _,
            prelude: _,
            macro_use_prelude,
            derive_helpers_in_scope,
            session_macros: _,
            names,
            diagnostics,
            diagnostics_by_module,
            data,
        } = self;
        assert!(diagnostics.is_empty(), "owned_allocations counts no diagnostic's boxes: {diagnostics:?}");
        let table = self.table(db);
        let mut n = 2 * nonempty(table.len());
        for (_, module) in table.iter() {
            n += module.scope(db).owned_allocations();
            n += 2 * nonempty(module.children(db).len());
        }
        n += nonempty(macro_use_prelude.len());
        n += nonempty(derive_helpers_in_scope.len());
        n += derive_helpers_in_scope.values().map(|it| nonempty(it.len())).sum::<u64>();
        n += diagnostics_by_module.owned_allocations();
        n += names.owned_allocations();
        // A block map shares its crate's data.
        if block.is_none() {
            n += data.owned_allocations();
        }
        n
    }
}

#[cfg(test)]
impl DiagnosticsByModule {
    fn owned_allocations(&self) -> u64 {
        nonempty(self.order.len()) + nonempty(self.modules.len())
    }
}

#[cfg(test)]
impl DefMapCrateData {
    /// The `Arc` and the tables: those reserved before their first insert count by capacity.
    fn owned_allocations(&self) -> u64 {
        let DefMapCrateData {
            exported_derives,
            fn_proc_macro_mapping,
            fn_proc_macro_mapping_back,
            registered_tools,
            unstable_features,
            rustc_coherence_is_core: _,
            no_core: _,
            no_std: _,
            proc_macro: _,
            edition: _,
            recursion_limit: _,
        } = self;
        1 + nonempty(exported_derives.capacity())
            + exported_derives.values().map(|it| nonempty(it.len())).sum::<u64>()
            + nonempty(fn_proc_macro_mapping.capacity())
            + nonempty(fn_proc_macro_mapping_back.capacity())
            + nonempty(registered_tools.len())
            + unstable_features.owned_allocations()
    }
}

#[cfg(test)]
impl LocalDefMap {
    pub(crate) fn owned_allocations(&self) -> u64 {
        2 * nonempty(self.extern_prelude.len())
    }
}
```

`uses.rs`:

```rust
#[cfg(test)]
impl NameRecords {
    /// One allocation per non-empty box, as `name_records` makes each once at its length.
    pub(crate) fn owned_allocations(&self) -> u64 {
        let NameRecords { uses, unresolved, glob_targets, macro_calls, derive_helpers, not_introducing } = self;
        [uses.len(), unresolved.len(), glob_targets.len(), macro_calls.len(), derive_helpers.len(), not_introducing.len()]
            .into_iter()
            .map(|len| u64::from(len > 0))
            .sum()
    }
}
```

`ItemScope::owned_allocations` is R10.7's and `UnstableFeatures::owned_allocations` is I11.1's.

The test, and the file's import block (Part F's, corrected: `module_for_file` and
`fetch_test_crate` are `TestDBExt`'s):

```rust
use std::hint::black_box;

use base_db::{Crate, EditionedFileId, relevant_crates};
use blaze_alloc_count::{assert_at_most, measure};
use test_fixture::WithFixture;

use crate::{
    ModuleId, file_item_tree,
    nameres::{
        DefMapPair, crate_def_map, crate_local_def_map,
        facts::impls::module_impls,
        tests::{TestDB, TestDBExt},
    },
};

/// T6's crates. `main`: two file modules, an inline one in the first, a `use`, a glob re-export,
/// a `macro_rules!` and its call, a trait and an impl. `twin`: the same files, modules, macro and
/// call, and nothing else. Each `b.rs` holds one function whose visibility the test edits.
const PAIR: &str = r#"
//- /main/lib.rs crate:main
mod a;
mod b;
pub use a::*;
use a::inner::Again;
//- /main/a.rs
macro_rules! make { ($name:ident) => { pub struct $name; } }
make!(FromMacro);
pub trait Tr {}
impl Tr for FromMacro {}
pub mod inner { pub use super::FromMacro as Again; }
//- /main/b.rs
pub fn body() {}
//- /twin/lib.rs crate:twin
mod a;
mod b;
//- /twin/a.rs
macro_rules! make { ($name:ident) => { pub struct $name; } }
make!(FromMacro);
pub mod inner {}
//- /twin/b.rs
pub fn body() {}
"#;

/// The global allocations of `pair`'s own boxes.
fn owned(db: &TestDB, pair: &DefMapPair<'_>) -> u64 {
    pair.def_map(db).owned_allocations(db) + pair.local(db).owned_allocations()
}

/// T6: a miss of `crate_local_def_map` that collects the crate again allocates its result's
/// boxes and salsa's cost of the run, and nothing else: every module, scope, children list,
/// prelude and worklist of the collection is in the query's frame, and `settle` copies each
/// table out once at its length.
#[test]
fn a_crate_collection_allocates_its_result_alone() {
    let (mut db, files) = TestDB::with_many_files(PAIR);
    let main = relevant_crates(&db, files[0].file_id(&db))[0];
    let twin = relevant_crates(&db, files[3].file_id(&db))[0];
    assert_ne!(main, twin);
    let (main_b, twin_b) = (files[2], files[5]);
    black_box(crate_local_def_map(&db, main));
    black_box(crate_local_def_map(&db, twin));

    // The item tree of the edited file is made again outside the measure: it is `Tag::Parse`'s
    // miss, not the collection's.
    let edit = |db: &mut TestDB, file: EditionedFileId, krate: Crate, text: &str| {
        let file_id = file.file_id(db);
        db.set_file_text(file_id, text);
        black_box(file_item_tree(db, file.into(), krate));
    };
    const PRIVATE: &str = "pub(crate) fn body() {}\n";
    const PUBLIC: &str = "pub fn body() {}\n";

    // A first re-collection of each, unmeasured: it grows what a thread keeps between runs (its
    // frame, blaze-mem's scratch shelves, salsa's edge buffer) and shows the edit re-collects.
    edit(&mut db, main_b, main, PRIVATE);
    edit(&mut db, twin_b, twin, PRIVATE);
    let executed = db.log_executed(|| {
        black_box(crate_local_def_map(&db, main));
        black_box(crate_local_def_map(&db, twin));
    });
    let collected = executed.iter().filter(|it| it.contains("crate_local_def_map")).count();
    assert_eq!(collected, 2, "a visibility edit did not re-collect both crates: {executed:#?}");

    edit(&mut db, main_b, main, PUBLIC);
    edit(&mut db, twin_b, twin, PUBLIC);
    let (twin_pair, twin_used) = measure(|| crate_local_def_map(&db, twin));
    let (main_pair, main_used) = measure(|| crate_local_def_map(&db, main));

    let def_map = main_pair.def_map(&db);
    assert_eq!(def_map.modules(&db).count(), 4, "fixture changed: main's modules");
    let a = db.module_for_file(files[1].file_id(&db));
    let tr = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("Tr"));
    assert!(
        def_map.module(&db, def_map.root).name_in_scope(&db, &tr).take_types().is_some(),
        "fixture changed: the root's glob does not bring `Tr` in"
    );
    assert!(
        def_map.module(&db, a).traits_in_scope(&db).next().is_some(),
        "fixture changed: `a` has no trait"
    );

    let (main_owned, twin_owned) = (owned(&db, main_pair), owned(&db, twin_pair));
    assert!(
        main_owned > twin_owned,
        "main's result owns {main_owned} boxes, no more than the twin's {twin_owned}: T6 measures nothing"
    );
    let salsa = twin_used.allocations.checked_sub(twin_owned).unwrap_or_else(|| {
        panic!(
            "the twin's collection allocated {} times, fewer than the {twin_owned} boxes its result owns",
            twin_used.allocations
        )
    });
    assert_eq!(
        main_used.allocations,
        salsa + main_owned,
        "a crate collection allocated {} times ({} bytes): salsa's run is {salsa} (the twin's {} \
         less its {twin_owned} boxes), its result owns {main_owned} boxes",
        main_used.allocations,
        main_used.bytes,
        twin_used.allocations,
    );
}
```

When mem-db-salsa's memo pooling (M3 to M6) makes salsa's run free, `salsa` measured here is 0,
and T6 then holds the miss to exactly `main_owned`. No edit is needed for that. Before R10 lands,
T6 fails: the building scopes grow by doubling on the global heap, which the twin's smaller
scopes do not match.

### H9. Asks of the other slices (told by tmux)

All four are answered:

| Ask | Answered by |
|---|---|
| 1. `ItemScope::owned_allocations` | R10.7 |
| 2. `Declared` and `ItemScope<Building<'f>>::reserve_declared` | R10.8, with `Declared` as given below |
| 3. `Visibility::max`/`min` taking `&impl DefMapRead` | R10.9 |
| 4. `UnstableFeatures::reserve`, its `shrink_to_fit` removed, `owned_allocations` | mem-db-def-items' I11.1 (`v2/requests/mem-db-def-items.md`) |

I11.1 lands with or after the removal of `DefMapCrateData::shrink_to_fit` (A5, and H1's
`DefMapCrateData` paragraph), since that function calls `UnstableFeatures::shrink_to_fit`
(`nameres.rs` 349).

`Declared`, which R10.8 puts in `item_scope.rs`:

```rust
/// How many of each kind of item a list declares into a scope (the collector's `declared`).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Declared {
    /// Items with a name: each goes into `declarations` and one namespace column.
    pub(crate) named: u32,
    pub(crate) impls: u32,
    pub(crate) uses: u32,
    pub(crate) extern_crates: u32,
    pub(crate) macro_calls: u32,
    pub(crate) macro_rules: u32,
    pub(crate) unnamed_consts: u32,
    pub(crate) extern_blocks: u32,
    /// Modules, counted in `named` too; the module table and children read this.
    pub(crate) mods: u32,
}
```

