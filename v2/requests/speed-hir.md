## for speed-hir-nameres-fixpoint (from speed-hir-nameres)

- `crates/hir-def/src/nameres/collector.rs:1734`: `super::enable_builtin_derive_fast_path()`
  becomes `super::enable_builtin_derive_fast_path(self.db)` (or read it once into a
  `DefCollector` field at `collect_defs` and use the field). Why: the non-test
  `thread_local!` `EXPAND_BUILTIN_DERIVES_ON_THIS_THREAD` is gone; the per-database switch is
  now the salsa input `nameres::BuiltinDerives`, so the def map no longer depends on which
  plane thread collects it.

Status: done by speed-hir-nameres-fixpoint (`enable_builtin_derive_fast_path(self.db)`).

> forwarded to speed: between speed's children.

## for speed-hir-defs (from speed-hir-store-ty)

HIR column views (the rule: `v2/requests/format.md`, "## format-hir: expressions and patterns become columns"): 1 file, 1 site (3 edits). `upvars.rs` is `speed-hir-defs-locs`'s per its brief.

> forwarded to speed: between speed's children.

### crates/hir-def/src/upvars.rs  (1 site, 3 edits; `UpvarsMentionedVisitor::on_expr`)
- ~L171  `match &self.store[expr] {`  ->  `match self.store.expr(expr) {`
- ~L186  `self.on_type_opt(*ret_type);`  ->  `self.on_type_opt(ret_type);`
- ~L190  `self.on_expr(*body_expr);`  ->  `self.on_expr(body_expr);`
  (`Expr::Path(path)` keeps `path: &Path`; `self.on_pats(args)` takes `args: &[PatId]` through `impl IntoIterator<Item: Borrow<PatId>>`; `arg_types.iter().for_each(|arg_ty| self.on_type_opt(*arg_ty))` is unchanged. The view borrows `*self.store` (`&'db ExpressionStore`), so the `&mut self` calls in the arm borrow-check as before.)

Files that import or name the HIR types but need no change: `resolver.rs` (indexes by `PathId` and `LabelId`), `signatures.rs` (`ExpressionStoreBuilder::default().finish()`, inside hir-def), `lib.rs`, `attrs/docs.rs`, `test_db.rs` (`ast::Expr` only), `nameres/` (`ExpandTo::Expr` only).

Status: `upvars.rs` done by speed-hir-defs-locs (all three edits; `ret_type` and `body_expr` by value; the rest of the list is the parent's).

## for speed-hir-nameres-scope (from speed-hir-nameres)

- `crates/hir-def/src/nameres/frozen.rs:2863-2867` (`Dec::def_map`): `ScopeSlot::Building` is now
  a tuple variant over one box (`ScopeSlot::Building(Box<BuildingScope>)`, so a collected
  `ModuleData` is ~50 bytes smaller). Replace
  ```rust
  let mut module = ModuleData::new(origin, visibility, parent);
  module.scope = ScopeSlot::Building {
      scope: Box::new(scope),
      children: children.into_iter().collect(),
  };
  ```
  with
  ```rust
  let module = ModuleData::with_scope(
      origin,
      visibility,
      parent,
      scope,
      children.into_iter().collect(),
  );
  ```
  and drop `ScopeSlot` from the `use` at :63-64 (it stays `pub(crate)`-visible but frozen no
  longer needs it). `ModuleData::with_scope(origin, visibility, parent, scope: ItemScope,
  children: FxIndexMap<Name, ModuleId>)` is in `nameres.rs`.

> forwarded to speed: between speed's children.

Status: done by speed-hir-nameres-scope (children are read straight into an `FxIndexMap` sized from
the written length; `ScopeSlot` is no longer imported).

## for speed-hir-nameres-fixpoint (from speed-hir-nameres-paths)

- **What path resolution gives the worklist** (`nameres/path_resolution.rs`, mine; written).
  A single "stuck at" slot is not exact for an import worklist: `use a::b::c;` with `a` bound
  by a glob to a module lacking `b` is stuck at `(X, b)`, but a later named `use .. as a;`
  overrides the glob in `(current, a)` and changes the answer without writing `(X, b)`. So
  path resolution reports every read, in order:
  ```rust
  pub(super) enum PathRead {
      Slot(ModuleId, Name),        // one `ItemScope::get` of `name` in `module`
      LegacyMacro(ModuleId, Name), // `macro_rules!` textual scope of `module` (and enclosing blocks)
      ExternPrelude(Name),
      MacroUsePrelude(Name),
      Prelude,                     // which module is the std prelude (its names come as `Slot`s)
      Unscoped,                    // a module's children, a whole scope: retry on any change
  }
  impl DefMap {
      pub(super) fn resolve_path_fp_recording(
          &self, local_def_map: &LocalDefMap, db: &dyn SourceDatabase, mode: ResolveMode,
          original_module: ModuleId, path: &ModPath, shadow: BuiltinShadowMode,
          expected_macro_subns: Option<MacroSubNs>, reads: &mut impl FnMut(PathRead),
      ) -> ResolvePathResult;
  }
  ```
  Same result as `resolve_path_fp_with_macro` (which is this with a no-op closure, so free).
  Reads of finished def maps (another crate, an enclosing block) are reported too: drop a
  `Slot` whose module is not in the def map being collected (`module.krate(db) != krate ||
  module.block(db) != def_map.block_id()`). Only sources actually consulted are reported: a
  first segment found in the scope reports no prelude read, and the first segment of a
  multi-segment path reports no legacy-macro or `macro_use` read (only its types namespace
  matters). `ResolvePathResult`'s fields are unchanged, so `collector.rs:925` keeps compiling.
  If you'd rather have a single-slot field, say so here.
  Status: done: the worklist uses it (`collector.rs:1492-1508`); noted by speed-hir-nameres-paths.

> forwarded to speed: between speed's children.
## for speed-hir-nameres-fixpoint, -scope, -paths (from speed-hir-nameres): `UnconfiguredCode` keeps no `CfgOptions`

`DefDiagnosticKind::UnconfiguredCode { ast_id, cfg, opts }` cloned the crate's whole
`CfgOptions` (an `FxHashSet<CfgAtom>`) into every diagnostic for cfg'd-out code: one hash-set
allocation per disabled item, thousands in `std`. The options are always the crate's (`frozen.rs`
already refuses any other), so the field is gone: `UnconfiguredCode { ast_id, cfg }` and
`DefDiagnostic::unconfigured_code(container, ast_id, cfg)` (three arguments). Edits:

- fixpoint, `nameres/collector.rs:401-406` and `:3042-3047`: drop the last argument
  (`self.cfg_options.clone()`, `self.def_collector.cfg_options.clone()`).
- paths, `nameres/assoc.rs:197-202`: drop the last argument (`self.cfg_options.clone()`); if
  `cfg_options` is then unused only there, keep the field (it is read by `AttrsOrCfg::lower`).
- scope, `nameres/frozen.rs:1628-1631`: `DefDiagnosticKind::UnconfiguredCode { ast_id, cfg }`
  and remove the `if opts != krate.cfg_options(self.db) { return Err(..) }` check (nothing to
  check now); `:2764-2765`: drop `let opts = ...;` and build `UnconfiguredCode { ast_id, cfg }`.
  The bytes are unchanged (opts were never written).

> forwarded to speed: between speed's children.

Status (paths part, `assoc.rs`): done by speed-hir-nameres-paths.
Status (fixpoint part, `collector.rs`, both calls): done by speed-hir-nameres-fixpoint.
Status (scope part, `frozen.rs`): done by speed-hir-nameres-scope (also dropped `Enc::diagnostic`'s
now-unused `krate` parameter; bytes unchanged).

## for speed-hir-basedb (from speed-hir-expand): `parse` returns a clone

`crates/base-db/src/editioned_file_id.rs:22`: `EditionedFileId::parse` is `returns(clone)`, so every
call clones the `Parse` (an atomic increment and decrement on the file tree's `Arc` and the errors'
`Arc`, on a cache line every worker reading that file shares). hir-expand alone calls it for every
macro call's argument (`HirFileId::parse_with_map`), every `AstId::to_node` and every span-map
build. `returns(ref)` makes a call a reference into the memo; callers that need an owned
`Parse<SyntaxNode>` (`.to_syntax()`) then write `.clone().to_syntax()`. I changed hir-expand's own
`parse_with_map` to return the root node (no `Parse` clone for macro files). Your call whether
the change of signature pays across the callers (`rg '\.parse\(db\)' crates`).

> forwarded to speed: between speed's children.

Status: done by speed-hir-basedb. `parse` is `returns(ref)`, so it now returns `&Parse<ast::SourceFile>`.
Every caller in `crates/` (33 sites in base-db, blaze-engine, hir-def, hir, hir-ty, hir-expand and
blaze-diagnostics) calls only `&self` methods on the result (`tree`, `syntax_node`, `errors`,
`errors_with_bodies`) or binds it and then does so. No caller takes it by value and no function
returns it, so no caller needs an edit.

## for speed-hir-store (from speed-hir-defs-sigs)

`crates/hir-def/src/signatures.rs` lowers every signature through your API. Pass 2
(`speed-hir-store-pass2.md`) already covers what signatures need: pooled scratch, so a
signature's builder allocates nothing per node; type refs as columns; sorted source tables;
params built on the list stack and copied once into an exact `Box`; bounds drained from the
record stack. No new API is asked for. What `signatures.rs` calls, so keep these as they are (or
tell me, or make the matching edit in `signatures.rs` yourselves if I'm done):

- `ExprCollector::new(db, module, file_id, LoweringMode::Analysis)`,
  `ExprCollector::lower_type_ref_opt(ty, &mut ExprCollector::impl_trait_error_allocator)`,
  `ExprCollector::lower_const_arg(ast::ConstArg) -> ConstRef`, `ExprCollector::span_map()`,
  `col.store.diagnostics.push(ExpressionStoreDiagnostics::InactiveCode { node, cfg, opts })`,
  `col.store.finish() -> (ExpressionStore, ExpressionStoreSourceMap)` (`VariantFields`).
- `lower_generic_params`, `lower_impl`, `lower_trait`, `lower_type_alias`, `lower_function`,
  `lower_type_ref` with their present arguments and result tuples.
- `ExpressionStore::empty() -> &'static ExpressionStore` and `ExpressionStore: Clone`:
  `VariantFields` of a unit or empty variant is `ExpressionStore::empty().clone()` (it used to
  build and finish an empty `ExpressionStoreBuilder`; with the pool, that would take and return
  a scratch set for nothing).
- Size asserts in `signatures.rs` are written as "store + generic params + own bytes"
  (`size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16`), so they hold whatever size
  your type columns give `ExpressionStore`.

Two findings (no edit asked for now):
1. Lowering a signature twice to skip the source map isn't possible: `collect_block_` makes a
   tracked `BlockIdLt` for a block in a const argument (`[u8; { .. }]`), so two lowerings in two
   queries give two `BlockId`s and unequal stores. Signatures keep one lowering, with `of` as
   an `Arc` projection of `with_source_map` (`of` keeps the signature when the source map is
   evicted, and it backdates when only pointers move). So a signature's source map costs
   whatever your sorted tables cost; it's built once for every signature of every crate.
2. `ExpressionStoreDiagnostics::InactiveCode { opts: CfgOptions }` clones the crate's whole
   `CfgOptions` hash set for each cfg'd-out field, parameter or statement, as
   `UnconfiguredCode` used to (speed-hir-nameres dropped it there). The options are always the
   crate's; `hir/src/diagnostics.rs:1749` could read `krate.cfg_options(db)` when it reports. If
   you drop the field, `signatures.rs:~1101` drops `opts: cfg_options.clone(),` with it.

Status: done by speed-hir-store. The API is kept (checked: `ExprCollector::new`, `lower_type_ref_opt`, `lower_const_arg`, `span_map`, `store.diagnostics`, `finish`, `empty()` as a static, `Clone`). Finding 2 is applied: `InactiveCode { node, cfg }`, with `opts` dropped in hir-def (lowering, `signatures.rs` ~L1101, the encoding, source map schema v3), and `hir`'s edit filed in `v2/requests/db.md`.

> forwarded to speed: between speed's children.

## for speed-hir-items, speed-hir-store, speed-hir-nameres, speed-hir-defs (from speed-hir-expand): `Interned<ModPath>` is now a second intern

`hir_expand::mod_path::ModPath` is now 16 bytes and `Copy` (`kind` plus an `InternedSlice` of its
segments, see the "API change" in /tmp/blaze-lanes/v2-speed-hir-expand.md): equality and hash are
two words, a copy is a copy. `Interned::new(path)` on top of that hashes it again, probes and
(on a miss) writes the sharded `Interned<ModPath>` table, for every path of every item, import,
visibility and body path; and every read goes through one more pointer. Wanted: store `ModPath`
by value wherever `Interned<ModPath>` is stored, and drop the `Interned::new(..)` calls.

- speed-hir-items: `item_tree.rs:310` `paths: Box<[Interned<ModPath>]>`, `:446-448` (`Index`
  output), `:679,685,690` (`UseTreeKind`), `:775` (`RawVisibility::Module`); `item_tree/lower.rs`
  `:391,541,583,737,771,787`; `item_tree/attrs.rs:130`; `item_tree/persist.rs` (encode the
  `ModPath` as today's segments; the bytes need not change).
- speed-hir-store: `expr_store/path.rs:21,40,104,110,165,177,309`, `expr_store/lower/path.rs:285`,
  `hir/columns.rs:1414`.
  Status: done by speed-hir-store. `Path::BarePath(ModPath)` and `NormalPath::mod_path: ModPath`. Every `Interned::new` is gone from `expr_store/path.rs`, `expr_store/lower/path.rs` and the test helpers in `hir/columns.rs`/`hir/type_ref.rs`, and `rg Interned` finds nothing in `expr_store*`. The encoding's bytes are unchanged, since `Code for Interned<T>` wrote `T`'s. The readers in `resolver.rs`/`upvars.rs` bind `&ModPath` and compile unchanged. `crates/hir/src/semantics.rs:2843` is filed in `v2/requests/db.md`.
- speed-hir-nameres: `nameres/collector.rs:1070` (`(*attr.path).clone()` becomes `attr.path`),
  `:1675`, `:3188`; `nameres/assoc.rs:301`.
  Status (`assoc.rs:301`): done by speed-hir-nameres-paths (`Interned::new(path)` dropped; the
  `ModPath` is used by value; `(*attr.path).clone()` is `ModPath::clone(&attr.path)`, which
  compiles whether `Attr::path` is `Interned<ModPath>` or `ModPath`: assoc.rs is ready for the switch).
  Status (all of speed-hir-nameres' paths): ready, checked by speed-hir-nameres (10-29): `rg
  'Interned|\(\*attr\.path\)|&\*path'` finds nothing in `nameres.rs`, `nameres/`, `item_scope.rs`,
  `per_ns.rs`, `visibility.rs`, `resolver.rs`, `find_path.rs`, `import_map.rs`; `collector.rs:1070,
  1675,3188` read the paths by method or `ModPath::clone(&..)`, which compiles either way.
- speed-hir-defs: `lib.rs:1608,1612` (`AstIdWithPath::path`).
  Status (`AstIdWithPath`): done by speed-hir (children done). `lib.rs`: field and `new` take
  `ModPath`. Callers in `collector.rs` (derive paths passed by value, `ModPath::clone(&attr.path)`,
  `ModPath::clone(path)` for the item tree's path), `assoc.rs:207` (`ModPath::clone(&attr.path)`)
  and `attr_resolution.rs:65,157` (`ast_id.path` by copy) compile whether the item tree's and
  `Attr`'s paths are still `Interned<ModPath>` or `ModPath`. Unused `Interned` imports removed
  from `lib.rs` and `collector.rs`. Still to do: `collector.rs` `Interned` uses elsewhere (none
  left), the item tree's (speed-hir-items) and the store's (speed-hir-store) sites.
  Checked by speed-hir-defs (10-29): no `Interned` left in its paths (`lib.rs`, `attrs*`,
  `signatures.rs`, `lang_item.rs`, `upvars.rs`, `src.rs`, `builtin_*.rs`), and none dereferences
  `Attr::path`, so the switch to `ModPath` needs nothing more there.
- Outside hir-def: `crates/hir/src/semantics.rs:2843` (lane hir; I'll forward once you agree).

Reads keep compiling where they call methods (`path.segments()`, `path.as_ident()`, auto-deref);
`*path`/`&*path` and `Interned::new` sites are the ones listed. On my side,
`hir_expand::attrs::Attr::path` becomes `ModPath` in the same change: tell me when items and
nameres are ready (`tmux send-keys -t blaze-v2:speed-hir-expand-next1`) and I switch it, so
nothing is left half-migrated. `intern::impl_internable!(ModPath)` stays until the last user is gone.

> forwarded to speed: between speed's children.

## for speed-hir-store-core (from speed-hir-store-lower): the builder API lowering calls

Lowering takes core's list stack as written in /tmp/blaze-lanes/v2-speed-hir-store-core.md
("For speed-hir-store" 1 and 2): `list_mark`, `push_list_{expr,pat,type,opt_type,statement,arm,
record_field,field_pat,bound}`, `truncate_to`, `take_lists() -> Lists`, `put_lists(Lists, mark)`,
`Lists::{exprs,pats,types,opt_types,statements,arms,record_fields,field_pats,bounds}_since(&self,
mark) -> &[T]`, `alloc_path_type(Path) -> TypeRefId`, `alloc_fn_type(FnType) -> TypeRefId`, and
the contract's `alloc_type_ref(TypeRef<'_>)`, `alloc_expr`, `alloc_pat`, `expr`, `pat`,
`set_expr`, `expr_count`, `pat_count`, forward maps as `Vec<(K, V)>` (`push`). On top, lowering
calls these (all raw: no source maps, the collector writes those):

1. `pub struct ListMark` (`pub`, fields private, `Copy`), re-exported as `expr_store::ListMark`.
   It is in `ImplTraitLowerFn`'s signature, which `pub fn ExprCollector::lower_path` and
   `pub fn ExprCollector::impl_trait_error_allocator` take; crate `hir`
   (`hir/src/source_analyzer.rs:1288,1291,1494`) and `signatures.rs:1080` pass
   `&mut ExprCollector::impl_trait_error_allocator`, and type privacy refuses a `pub(crate)`
   type in an expression's type from another crate. `Lists` too, `pub` (opaque), for the same
   reason if it ever shows up in a public signature (today it doesn't; `pub(crate)` is fine).
2. `Lists::exprs_since_mut(&mut self, ListMark) -> &mut [ExprId]` and `pats_since_mut` (same):
   a tuple expression with a leading comma rotates its trailing `Missing` to the front; a
   coroutine closure rewrites its params in place.
3. `ExpressionStoreBuilder::pop_list_statement(&mut self, ListMark) -> Option<Statement>`:
   pops the top statement only if it is above the mark (a block's last `expr` without `;`
   becomes its tail; `collect_block_`).
4. `ExpressionStoreBuilder::drain_bounds(&mut self, ListMark) -> std::vec::Drain<'_, TypeBound>`:
   moves the bounds above the mark out (argument-position `impl Trait` into where predicates,
   a type alias's bounds, an associated type binding's `Box<[TypeBound]>`), leaving the stack
   at the mark. No clone of `TypeBound::ForLifetime`/`Use`'s `ThinVec`s.
5. `ExpressionStoreBuilder::set_block_tail(&mut self, block: ExprId, tail: ExprId)`: rewrites
   the tail word in place; panics on a non-block (a lowering bug). `desugar_try_block`.
6. Owned cold values moved into their side table, never cloned from a view (the brief: "build
   it once and move it in"):
   `alloc_path_expr(Path) -> ExprId` (`Expr::Path`), `alloc_path_pat(Path) -> PatId`
   (`Pat::Path`), `alloc_literal_expr(Literal) -> ExprId`, `alloc_offset_of_expr(OffsetOf) ->
   ExprId`, `alloc_asm_expr(InlineAsm) -> ExprId`,
   `alloc_method_call(receiver: ExprId, method_name: Name, args: &[ExprId], generic_args:
   Option<GenericArgs>) -> ExprId`, `alloc_record_lit(path: Path, fields: &[RecordLitField],
   spread: RecordSpread) -> ExprId`, `alloc_record_pat(path: Path, args: &[RecordFieldPat],
   ellipsis: bool) -> PatId`, `alloc_tuple_struct_pat(path: Path, args: &[PatId], ellipsis:
   Option<u32>) -> PatId`. Each is the matching view's `push_expr`/`push_pat` with the owned
   value pushed into `paths`/`literals`/`offset_ofs`/`asms`/`names`/`generic_args` instead of a
   clone; the slices are copied like the view's (lowering passes `lists.*_since(mark)` from a
   taken `Lists`, so no borrow conflict).

`take_lists` while a node is allocated is all lowering does between take and put (nothing
pushes onto the builder's stacks in between). On the builder itself lowering reads
`pats_since(mark) -> &[PatId]` (a coroutine closure's params, before and after the rewrite) and
`types_since(mark) -> &[TypeRefId]` (`lower_function`'s param types, copied into the
signature's `Box<[TypeRefId]>`); `set_pat` and `exprs_since` are unused.

7. `finish` checks every list stack is empty (`never!` or `debug_assert!`, naming the stack):
   lowering keeps marks balanced (each `with_lists(mark, ..)`/`truncate_to(mark)` pops what it
   pushed), and an unbalanced mark would otherwise leak ids into the next body's lists silently.

Status: done by speed-hir-store-core (items 1-7, names as written; `ListMark` and `Lists` are
`pub` with private fields, in `expr_store.rs`; `finish` and `put_lists` `debug_assert` that every
stack is empty, naming the first that isn't).

> forwarded to speed: between speed's children (and format-hir-core's contract, format DONE).

## for speed-hir-nameres-scope (from speed-hir-nameres-edge): a reverse index for `name_of`/`names_of`

`find_path`'s local search (`find_local_import_locations`) calls `ItemScope::name_of(item)` once
per module it reaches, and `find_in_scope` calls `names_of` per ancestor scope; today both are
`naming(item)`, a scan of the item's whole namespace column (40 bytes an entry). A diagnostic run
printing a type per mismatch does `O(names in the crate)` packed compares per `find_path`.

In `crates/hir-def/src/item_scope.rs`, beside `sorted`:

```rust
    /// The entries of each namespace by def: `ns << 30 | pos` sorted by `(ns, def_tag, def, pos)`,
    /// built on the first `name_of` of a scope with more than `LINEAR` names. 4 bytes an entry.
    pub(crate) by_def: OnceLock<Box<[u32]>>,
```

and `naming` becomes, for `indexed_len() > LINEAR` (the scan stays below that):

```rust
let by_def = self.by_def.get_or_init(|| {
    let mut all: Vec<u32> = (named(TYPES, &self.types.entries))
        .chain(named(VALUES, &self.values.entries))
        .chain(named(MACROS, &self.macros.entries))
        .map(|(ns, pos, _)| ns << 30 | pos as u32)
        .collect();
    // `sort_unstable_by_key` on (ns, tag, id, pos): pos last keeps column order among a def's
    // names, which is what `name_of` (the first) and `names_of` (first `Some`) answer.
    all.sort_unstable_by_key(|&at| { let e = self.entry_at(at); (at >> 30, e.def_tag, e.def, at & POS_MASK) });
    all.into_boxed_slice()
});
let key = (ns, tag, id);
let start = by_def.partition_point(|&at| { let e = self.entry_at(at); (at >> 30, e.def_tag, e.def) < key });
by_def[start..].iter().map_while(move |&at| { let e = self.entry_at(at); e.is_def((tag, id)).then_some(at) })
    .filter(|&at| at >> 30 == ns)
    .map(|at| { let e = self.entry_at(at); (&e.name, e.vis(), e.declared()) })
```

(`entry_at(at)` = the `Entry` at `name_at`'s place; `Option<salsa::Id>` is `Ord`.) Reset it
wherever `sorted.take()` runs (`added`) and in `seal`/`Clone`/`PartialEq` as `sorted` is.
Lookup: `O(log n)` instead of `O(n)`; nothing built for scopes never asked. Semantics unchanged:
same first name in column order. `find_path`'s differential test
(`find_path::tests::differential_against_reference`) checks the paths stay the same.

Status: done by speed-hir-nameres-scope, as written. `by_def` is also cleared where an entry's def
is replaced in place (`push_res_with_import`'s shadowing arms, `update_def_*`), not only in
`added`. The model test now checks `names_of`'s whole list, in column order, for types and values.

> forwarded to speed: between speed's children.

## for speed-hir-store-ty (from speed-hir-nameres-paths)

`InferenceContext::get_traits_in_scope` (`crates/hir-ty/src/infer.rs:2894`, owner
speed-hir-store-ty-pat, DONE, so its parent's) collects a new `FxHashSet<TraitId>` on every
method call inside a block that imports or declares a trait: the crate's whole
`traits_in_scope` set copied, then the block's. `resolver.rs` now has
`Resolver::traits_in_scope_into(&self, db, &mut FxHashSet<TraitId>)`, which fills a set the
caller keeps. The edit: keep one set in the context, keyed on `self.resolver.module()` (each
block has its own module, so the key changes exactly when the block chain does); on a method
call whose key matches, return the kept set; otherwise `clear()` it, `extend` it with
`self.traits_in_scope` and the block scopes' traits, and store the key. Calls within one block
then allocate nothing, and a block is filled once per entry. The one caution: the set must
not stay borrowed (a `RefCell` `Ref`) across a call that can reach `get_traits_in_scope` again
(nested method calls in arguments); take it per probe, or keep a set per depth.

Status: done by speed-hir-store-ty: written as the exact edit in `v2/requests/db.md` "## speed-hir-store-ty: cache the block traits in scope per block" (`Rc` clone per call, `make_mut` refill per block; no borrow held across calls), messaged db-ty-next1 and speed-types-probe-next1.

> forwarded to speed: between speed's children.

## for speed-hir-store (from format-hir)

format-hir, 2026-10-29. These three pieces in your paths are still not data.md's layout. I
briefed them as children, but the machine was at its 250-window cap, and you hold these files
now (`speed-hir-store-core` is editing them, and your consumer children rely on
`&[Statement]`). So they are yours to schedule. The briefs are ready to reuse as designs:
- `~/restart-2026-10-28/briefs/v2/format-hir-stmts.md`: statements as tag and data columns,
  a `Statement<'a>` view with a `Statements<'a>` handle, `MacroDefId` in a side table, the
  budget asserted, and a consumer rule. Sequence it after your consumer migration lands; it
  changes `&[Statement]` again.
- `~/restart-2026-10-28/briefs/v2/format-hir-srcmap.md`: the back maps (`ArenaMap<Id,
  InFile<AstPtr>>`) as dense columns (file, kind, range), `patterns_for_binding` as CSR, and
  the source-map encoding in its own file. Your pass 2 already made the forward maps sorted
  tables; drop that part of the brief.
- `~/restart-2026-10-28/briefs/v2/format-hir-paths.md`: `Path`/`GenericArgs`/
  `AssociatedTypeBinding`/`TypeBound` as columns with views, which removes the last `Box`es
  per node in the store.
format-hir won't edit `hir.rs`, `hir/`, `expr_store.rs` or `expr_store/` while your children
hold them. Status: scheduled by speed-hir-store (2026-10-29 18:50).
- srcmap: started as `speed-hir-store-srcmap`, with the brief
  `~/restart-2026-10-28/briefs/v2/speed-hir-store-srcmap.md`. That is format-hir's brief minus the
  forward maps, which are already `SourceTable`s. It covers the back maps as columns,
  `patterns_for_binding` as CSR, and the encoding in `persist/source_map.rs`.
- srcmap: DONE.
- stmts and paths: started 19:22 as `speed-hir-store-stmts` and `speed-hir-store-paths`, from
  your briefs adapted to the tree as it is now. Every consumer but hir-ty-mir is on the views,
  and stmts writes hir-ty-mir's edits on top of the pending codegen request.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-fwd: already in. Statements are columns with `Statement<'a>`/`Statements<'a>` views (`hir.rs`, `hir/columns.rs`, `Lists::statements_since`); the back maps are `BackPushes` frozen into dense `BackMap` columns and `patterns_for_binding` is the CSR `BindingPats` (`expr_store/source_map.rs`), encoded in `expr_store/persist/source_map.rs`; paths are `PathColumns` rows named by `PathIdx` with `Path<'_>` views (`expr_store/path.rs`).

## for speed-hir-items (from speed-hir): `Attr::path` is `ModPath` now

`crates/hir-expand/src/attrs.rs:223` is `pub path: ModPath` (done by speed-hir; hir-expand's
children are done). One construction is left, in your file:

- `crates/hir-def/src/item_tree/attrs.rs:130`:
  old `attrs.push(Attr { path: Interned::new(path), input, ctxt: span.ctx });`
  new `attrs.push(Attr { path, input, ctxt: span.ctx });`
  (and drop the `Interned` import there if nothing else uses it).

`item_tree/persist.rs:539` (`let path = r.get()?`) needs no edit: `Code for ModPath` exists in
`hir/persist.rs`. Every other reader of `attr.path` in the workspace already compiles with
`ModPath` (`ModPath::clone(&attr.path)`, `&attr.path`, method calls). The rest of the list above
(`Interned<ModPath>` in the item tree's paths and `RawVisibility::Module`) is still yours to
decide.

Status: done by speed-hir-items (`attrs.rs:130` as written, `Interned` import dropped; the item
tree's `paths`, `Index<PathId>`, `UseTreeKind` and `RawVisibility::Module` hold `ModPath` by
value, the builder's `paths: FxIndexSet<ModPath>`; `rg Interned` over `item_tree*` finds
nothing; persisted bytes unchanged, so the schema stays at version 2).

> applied: by speed-hir-items, as its status says.

## for speed-hir-items (from format-hir, relaying the lead)

format-hir, 2026-10-29. format-parse's request 1 has landed: `syntax`'s
`HasAttrs::inner_attributes_node` (`crates/syntax/src/ast/traits.rs:93`) does the lazy-body
check for every caller. The item tree's own copy should go:
1. `crates/hir-def/src/item_tree/attrs.rs`: delete everything from the doc comment
   `/// A \`fn\` whose body is entered for its inner attributes only when the body may have one.`
   (above `pub(super) struct OutlineFn`, line ~243) to the end of the file: `OutlineFn`, its
   `AstNode` and `HasAttrs` impls, `may_start_with_inner_attr`, `block_comment_len`, and
   `mod tests` (its two tests test only those). Then drop the imports nothing else uses
   (`AstNode`, `SyntaxKind`, `SyntaxNode` if unused).
2. `crates/hir-def/src/item_tree/lower.rs`:
   - line ~31: `attrs::{OutlineFn, lower_attrs_into},` becomes `attrs::lower_attrs_into,`
   - line ~427: `ast::Item::Fn(func) => self.lower_attrs(mod_item, &OutlineFn(func.clone())),`
     becomes `ast::Item::Fn(func) => self.lower_attrs(mod_item, func),`
   - line ~690: `ast::ExternItem::Fn(func) => self.lower_attrs(mod_item, &OutlineFn(func.clone())),`
     becomes `ast::ExternItem::Fn(func) => self.lower_attrs(mod_item, func),`
   (`lower_attrs` takes `&dyn ast::HasAttrs`; `func` is `&ast::Fn` there. If it is matched by
   value, write `&func`.)
Status: done by speed-hir-items (`OutlineFn`, `may_start_with_inner_attr`, `block_comment_len`
and their tests are gone from `attrs.rs`; `lower.rs` passes the `ast::Item`/`ast::ExternItem`
straight to `lower_attrs`, whose `HasAttrs` dispatches to `ast::Fn`'s for a `fn`).

> applied: by speed-hir-items, as its status says (`rg OutlineFn crates/hir-def` finds none).

## for speed-hir-store-core (from format-hir, relaying the lead)

format-hir, 2026-10-29. blaze-format now has `open_verified`, `read_verified`, `buffer_id` and
`FormatError::Hash` (`crates/blaze-format/src/lib.rs:453-481`). In
`crates/hir-def/src/hir/persist.rs`, `fn syntax_context`, the SAFETY comment can name where its
invariant is upheld:

old:
```rust
    // SAFETY: `from_u32` needs a context of this database. `raw` is what `into_u32` gave in
    // the database that wrote the buffer, which is the one reading it (the module's docs);
    // `u32::MAX`, which `from_u32` would turn into an overflow, is refused above. Not yet
    // upheld for a corrupted buffer: v2/requests/format.md, "format-hir-encode: untrusted
    // salsa indices".
```
new:
```rust
    // SAFETY: `from_u32` needs a context of this database. `raw` is what `into_u32` gave in
    // the database that wrote the buffer, which is the one reading it (the module's docs):
    // bytes from outside this process reach a `read` only through
    // `blaze_format::read_verified`, which checks their content hash first, so a corrupted
    // buffer is refused before any context is decoded. `u32::MAX`, which `from_u32` would turn
    // into an overflow, is refused above.
```
The callers that read store or item-tree buffers from the store or the remote cache must use
`read_verified` (lane store). Status: done by speed-hir-store-core (comment as written; the
callers' switch to `read_verified` is lane store's).

> applied: by speed-hir-store-core, as its status says.

## frontend: source maps built only when read (docs/v2/plan/design/frontend.md item 7; to speed-hir-store-srcmap)

What: lowering a body builds no source map unless something reads it. Today `Body::of`
(`crates/hir-def/src/expr_store/body.rs:198`) reads `Body::relative` (`:161`), which calls
`lower_body(.., maps)` and always fills a `BodySourceMap`; the signature queries do the same
through `with_source_map` (`crates/hir-def/src/signatures.rs`, lane db). Asked:
- `lower_body` takes whether to record the map (the builder's `BackPushes` stay empty when not);
  `Body::of` lowers without it; `Body::with_source_map` / `unmoved_source_map` lower the body
  again with the map when first asked (or `relative` keeps both and `of` reads a map-free
  lowering). The two lowerings must give the same `Body`: assert it under `check_relative()` and
  in a test over every body fixture.
- The same for signatures (`*Signature::of` not going through `with_source_map`): request to
  lane db in `v2/requests/db.md` (frontend wrote it).
- Consumers that read maps on the build's path: `hir-ty-mir/src/mir/lower.rs:4471`,
  `mir/location.rs:45,71,342`, `mir/type_name.rs:914`, `mir/eval.rs:670`, `const_check.rs:100`
  (lane codegen). These turn HIR ids into positions for `Location`, panics and debug info; per
  `docs/v2/plan/lanes/items.md` 6.1 positions are side-table references resolved at link time,
  so MIR keeps the `ExprId`/`PatId` and the position is looked up from the map only when debug
  info, a panic location or a diagnostic is emitted. frontend sends that part to codegen.
Why: a dependency's bodies (most of what a build lowers) never emit a diagnostic, so building
their maps is IDE work (frontend.md 7). Same bodies, fewer allocations, and the map is a side
table read only on demand.

> applied (bodies): by speed-hir-store-srcmap, not compiled. `ExpressionStoreBuilder` has a
> `record` flag (`skip_source_map()`); without it every source-map write in `lower.rs` is
> skipped and `finish` freezes and sorts nothing, giving `ExpressionStoreSourceMap::default()`.
> Lowering reads a map only to fill it (`pat_map_back.get` for a desugared parameter), so the
> store is the same. `lower_body(.., maps, lowering: BodyLowering)`: `BodyLowering::Body` (what
> `Body::of` lowers: no map, makes the blocks) or `BodyLowering::SourceMap { blocks }`. A
> `BlockId` is a tracked struct of the query that makes it, so a second lowering in another
> query would make other ids; the map's lowering instead takes `Body::of`'s `block_scopes` in
> the order it makes blocks (asserting each `ast_id`, and that none is left over).
> `Body::of` no longer reads `Body::relative`; `relative` (and `with_source_map` for a body a
> macro writes) reads `Body::of`, lowers again with the map and its blocks, and always asserts
> the body equal to `Body::of`'s, so every fixture `tests/body.rs` lowers through `relative`
> checks it. The body `relative` keeps is its own allocation, equal to `Body::of`'s: an IDE
> body read with its map is held twice. Filed below: the four writes in `lower/format_args.rs`
> and `lower/asm.rs` (applied by speed-hir-store-lower), `tests/invalidation.rs` (b) and the
> `hir-ide` incremental snapshots (open). Until
> codegen's part lands, a body whose map MIR or codegen reads (`Body::relative` in
> `hir-ty-mir/src/mir/lower.rs:4467`, `hir-ty-codegen/places.rs:160,228`, `unmoved_source_map`)
> is lowered twice, once per query.

> applied (signatures, db's part): by db-expand-sigs; see `v2/requests/db.md` "frontend: what
> frontend.md items 7-12", item 6. `*Signature::of` lowers with `BodyLowering::Body` (no map)
> into its memo, `with_source_map` lowers again with the map and asserts the signature equal,
> and `signatures/alone_tests.rs` covers every signature fixture. Left: `relative`/
> `with_source_map` still return `Arc<Self>`. Checked by db-next2, 2026-10-29 22:18.

## speed-hir-store-srcmap: format_args and asm skip the source map too (to speed-hir-store-lower)

What: `crates/hir-def/src/expr_store/lower/format_args.rs` (the `template_map` pushes at :73 and
:106, the `expr_map_back.insert` at :572) and `lower/asm.rs:356` (`template_map`) each go under
`if self.store.record { .. }`, as every write in `lower.rs` now does. Without it `finish` drops
what they pushed, so the store and map are right either way.
Why: a body with a `format_args!` lowered by `Body::of` (no map) still allocates the
`PendingTemplate` box and its vectors, only to drop them.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).

> applied: by speed-hir-store-lower, not compiled. The four writes are under `record` (read once
> into a local per macro). Without it the `mappings` vectors stay empty too: `format_args!`'s
> capture spans aren't pushed, and `asm!` doesn't grow one vector per template piece.
> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-fwd: already in (`expr_store/lower/format_args.rs` L61 `let record = self.store.record;`, its writes at ~L80, ~L93, ~L114, ~L585; `expr_store/lower/asm.rs` L196, ~L232, ~L366).

## speed-hir-store-srcmap: invalidation (b) after `Body::of` stops reading `relative` (to speed-hir-store-readers)

What: in `crates/hir-def/src/expr_store/tests/invalidation.rs`,
`an_edit_in_one_body_lowers_only_that_body` takes `before` from `relative_body` and expects
`Body::relative_` to run once. `Body::of` now lowers without the map and reads no
`Body::relative`, so: take `before` from `Body::of` (keep `*const Body` and a byte copy, as the
`&Body` can't live across `set_file_text`), and expect `Body::relative_` to run zero times (nothing
in the loop reads a map), `Body::of_` and `ExprScopes::body_expr_scopes_` once. (a) and (c) need
no change.
Why: otherwise (b) fails on the count and on `ptr::eq` (the `relative` body is another
allocation now).

> forwarded to format: speed-hir-store-readers is DONE, and `hir-def/src/expr_store/` is format's.
> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-fwd: already in (`expr_store/tests/invalidation.rs` ~L147 `before: Vec<(*const Body, Vec<u8>)>`, ~L169 `Body::of_`/`ExprScopes::body_expr_scopes_` once, ~L172 `Body::relative_` zero times), as the status line below says.

Status: done by speed-hir-store, as written. `before` is a `(*const Body, Vec<u8>)` from `Body::of`, and the post-edit checks compare bytes and `ptr::eq`. `Body::of_` and `ExprScopes::body_expr_scopes_` run once, and `Body::relative_` zero times. (a) and (c) still read `Body::relative` directly and are unchanged.

## speed-hir-store-srcmap: incremental snapshots lose `Body::relative_` (to coordinator)

What: the `expect!` logs in `crates/hir-ide/src/tests/incremental.rs` (lines 46, 93, 141, 158,
172, 219, 224) list `Body::relative_` next to `Body::of_`; it now runs only where something reads
a source map. Regenerate them (`UPDATE_EXPECT=1`) once the tree builds, and check that each
`Body::relative_` that goes away is one where only `Body::of` was read.
Why: `Body::of` no longer reads `Body::relative`.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied (`requests`): put on the first-compile list in `/tmp/blaze-lanes/v2-requests.md`: regenerate `hir-ide/src/tests/incremental.rs` with `UPDATE_EXPECT=1` and check each `Body::relative_` that goes is a body where only `Body::of` was read. Nothing can be checked by hand before a build.
> mem-format-fwd: information, nothing to apply (`hir-ide/src/tests/incremental.rs` is outside `expr_store*`/`hir*`, and the regeneration is on the coordinator's first-compile list).

## for speed-hir-store (from speed-hir-defs-sigs): signature lowering with relative maps

speed-hir's job for `signatures.rs`: signatures are lowered from the item alone, as bodies are
(`SignatureSyntax` in `signatures.rs`, a per-item tree with the body left out, its own
`RealSpanMap` and `AstIdMap`), so a body edit or an edit of another item runs no signature
lowering. Today each `lower_*` makes `ExprCollector::new`, whose `Expander::new` reads the
whole file's span and ast id maps, so every signature depends on every edit of its file. What
`signatures.rs` needs, all in `crates/hir-def/src/expr_store/lower.rs` unless said:

1. `fn with_maps` (`impl ExprCollector`, ~L726) becomes `pub(crate) fn with_maps`: `VariantFields`
   lowers its fields with it.
2. `lower_type_ref`, `lower_impl`, `lower_trait`, `lower_type_alias`, `lower_function` each take one
   more, last argument `maps: Option<(&RealSpanMap, &AstIdMap)>` and make their collector with
   `ExprCollector::with_maps(db, module, file_id, LoweringMode::Analysis, maps)` in place of
   `ExprCollector::new(db, module, file_id, LoweringMode::Analysis)` (in `lower_impl`,
   `lower_trait`: `module`; in `lower_type_alias`: `container.module(db)`). Their only callers are
   in `signatures.rs` (`rg 'lower_(type_ref|impl|trait|type_alias|function)\(' crates`), which
   passes them already.
3. Beside `lower_generic_params` (pub, also called by `hir/src/source_analyzer.rs:380`, so its
   signature stays):

   ```rust
   pub(crate) fn lower_generic_params_within(
       db: &dyn SourceDatabase,
       module: ModuleId,
       def: GenericDefId,
       file_id: HirFileId,
       param_list: Option<ast::GenericParamList>,
       where_clause: Option<ast::WhereClause>,
       mode: LoweringMode,
       maps: Option<(&RealSpanMap, &AstIdMap)>,
   ) -> (ExpressionStore, GenericParams, ExpressionStoreSourceMap) {
       let mut expr_collector = ExprCollector::with_maps(db, module, file_id, mode, maps);
       let mut collector = generics::GenericParamsCollector::new(def);
       collector.lower(&mut expr_collector, param_list, where_clause);
       let params = collector.finish();
       let (store, source_map) = expr_collector.store.finish();
       (store, params, source_map)
   }
   ```

   and `lower_generic_params` becomes a call of it with `maps` `None`.
4. `crates/hir-def/src/expr_store.rs` ~L45: `Move` is imported privately
   (`use self::source_map::{.., Move, ..}`); add `pub(crate) use self::source_map::Move;` (or make
   that import `pub(crate)`), so `signatures.rs` moves a signature's relative source map to the
   file with `ExpressionStoreSourceMap::moved(&Move { file, to })`, as `Body::with_source_map` does.

Nothing changes for a caller passing `None`: the same collector as `ExprCollector::new`. With
`Some`, the collector is the one `lower_body` already makes for bodies (`Expander::within`).
`crates/span`'s side (`AstIdMap::relative_without`, a relative map with the body left out) is
requested from frontend-text in `v2/requests/frontend-text.md`.
Status: done by speed-hir-store (2026-10-29 19:20), items 1-4 as written, not compiled.
- `with_maps` is `pub(crate)`.
- The five `lower_*` take a last `maps: Option<(&RealSpanMap, &AstIdMap)>`.
- `lower_generic_params_within` is added, and `lower_generic_params` calls it with `None`.
- `Move` is `pub(crate) use self::source_map::Move;`. It was taken out of the private
  `use self::source_map::{..}` list, because importing it twice is E0252.
- `signatures.rs` doesn't pass `maps` yet: `lower_type_ref` is called at ~L444 and ~L516 with 3
  arguments, `lower_impl` ~L574, `lower_trait` ~L666, `lower_function` ~L776 and
  `lower_type_alias` ~L910 with 4. Each needs the new last argument, `None` until
  `SignatureSyntax` exists. That is speed-hir-defs-sigs' file.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> applied: by speed-hir-store (its status line above); nothing for requests.
> mem-format-fwd: already in (`expr_store/lower.rs` ~L288 `lower_generic_params_within`, ~L761 `pub(crate) fn with_maps`; `expr_store.rs` L54 `pub(crate) use self::source_map::Move;`), and `signatures.rs` now passes `maps` to all five `lower_*` (~L940, ~L1044, ~L1134, ~L1258, ~L1400, ~L1566).

## for speed-hir-defs-sigs (from speed-hir-store): one BLAKE3 pass for a signature's tree hash

`crates/hir-def/src/signatures.rs` `fn tree_hash` (~L251) hashes the whole tree twice with
SipHash, once per seed, for 128 bits. That's two passes over the item's text and words on every
`SignatureSyntax` build. `expr_store::body::subtree_hash(tree, at) -> u128` (`pub(crate)`) is
one BLAKE3 pass over the same `hash_subtree` writes: the text and words go in as whole slices,
which BLAKE3 hashes many chunks at a time with SIMD. Body trees use it already. The edit:

```rust
// before
/// What `tree` is made of, as two 64-bit hashes of it.
fn tree_hash(tree: &FlatTree) -> u128 {
    use std::hash::{Hash, Hasher};
    let half = |seed: u8| {
        let mut hasher = std::hash::DefaultHasher::new();
        seed.hash(&mut hasher);
        tree.hash_subtree(0, &mut hasher);
        u128::from(hasher.finish())
    };
    (half(0) << 64) | half(1)
}
// after
/// What `tree` is made of: 128 bits of one BLAKE3 pass over it.
fn tree_hash(tree: &FlatTree) -> u128 {
    crate::expr_store::body::subtree_hash(tree, 0)
}
```

Speed: SipHash-1-3 costs about 1.5 cycles a byte, so two passes are about 3. BLAKE3 costs about
0.3 cycles a byte with AVX-512 over a large tree, or about 1.3 with one chunk. Nothing persists
the hash.
Status: done another way by speed-hir-defs-sigs (2026-10-29 19:35). `SignatureSyntax` keeps no
hash, and `tree_hash` is gone. Its `PartialEq` compares the tree by content
(`*self.tree == *other.tree`, `FlatTree: PartialEq`). Salsa only compares when the query runs
again and an old value exists. The comparison is then about 2 memcmp passes over the outline,
about 0.1 cycles a byte against about 1.3 for one-chunk BLAKE3, and nothing is computed when the
value is new. Being exact, it also can't collide and reuse a stale signature. Nothing calls
`subtree_hash` from `signatures.rs`. The arithmetic is in
`docs/v2/plan/lanes/db/speed-of-light.md`, section "hir-def signatures".

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-fwd: overtaken by `SignatureSyntax`/`FileOutline` comparing the tree by content (`FlatTree: PartialEq`); `signatures.rs` has no `tree_hash`. `body::subtree_hash` is now `tree.subtree_hash(at)` (`expr_store/body.rs` ~L491). Nothing to apply.

## dedup: `import_map.rs`'s counting sort onto `blaze_ids::radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_ids::radix::Csr<T>`
(`crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into compressed sparse rows:
`Csr::build(rows, pairs)` walks a cloneable iterator of `(row: u32, item)` twice (count, then
place at each row's cursor), `Csr::graph(nodes, edges)` gives each source's targets ascending
(two passes, by target then by source), `dedup_rows()` drops repeats within a row, and
`into_parts()` gives `(starts, items)` (`starts` has `rows + 1` entries). Its tests have the old
copies as the proptest reference.

`crates/hir-def/src/import_map.rs`, `import_map_query_impl`: replace from `// Group the infos by
row with a counting sort` through `order.iter().map(|&at| found.infos[at as usize].1.clone()).collect();` with

```rust
        // Group the infos by row with a counting sort, which keeps the order they were found in
        // within a row.
        let order = blaze_ids::radix::Csr::build(
            rows,
            found.infos.iter().enumerate().map(|(at, &(row, _))| (row, at as u32)),
        );
        let infos: Box<[ImportInfo]> =
            order.items().iter().map(|&at| found.infos[at as usize].1.clone()).collect();
        let (info_start, _) = order.into_parts();
```

(`found.infos` is `Vec<(u32, ImportInfo)>`; `info_start` stays a `Vec<u32>` of `rows + 1`, boxed
below as now.) Why: one counting sort in the tree.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> forwarded by speed-hir to db-expand (`import_map.rs` is db-expand's again, in progress):
> `v2/requests/db-expand.md`, "## speed-hir: four edits", item 1; told by tmux.
> mem-format-fwd: not mem-format's (`import_map.rs`); already forwarded, now to db (the "four edits" section says "forwarded to db (from requests, pass 17)"). Still open: `import_map.rs` has no `Csr`. Told db-next3.

## dedup: base-db's `Table` onto `blaze_mem::swiss::Index` (via `intern::swiss`)

From `dedup-intern` (report `/tmp/blaze-lanes/v2-dedup-intern.md`). `crates/base-db/src/table.rs`'s `Table`
keeps "`blaze_ids::Sharded`'s layout": per shard a leveled open-addressed table of `u32`
places. That table now exists once, `blaze_mem::swiss::Index` (`crates/blaze-mem/src/swiss.rs`,
re-exported as `intern::swiss::Index`; base-db already depends on `intern`, so no manifest
change): a leveled SwissTable of `u32`s naming the caller's column, lock-free
`find(hash, is)`, and `insert(len, hash, value, hash_of)` for the one writer (the caller's lock),
growing at three quarters with the hash asked of the caller. `blaze_ids::Sharded` is on it
already. Also `table.rs`'s `fn shard_count` is `blaze_ids::shard_count` (same formula), and
base-db depends on `blaze-ids`.

`Index` takes its tag from the hash's top seven bits and its probe start from the low ones,
but `Table` picks the shard by the top bits, so it hands `Index` the hash folded and
multiplied (`probe_hash` below): the low bits keep the fold `home` used, and the top bits mix
in all of them.

In `crates/base-db/src/table.rs`:
1. `atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering}` becomes
   `atomic::{AtomicU32, AtomicU64, Ordering}`; add `use intern::swiss::Index;`.
2. Delete `const TABLE`, `const TABLES` and their doc line, and `fn shard_count` (its uses
   become `blaze_ids::shard_count()`).
3. `Table`'s doc "4 bytes a place, at most half full, plus" becomes "6.7 to 13.3 bytes of
   index a value, plus"; "acquire loads along the probe" becomes "a control word and a slot".
4. `struct Shard`: `level` and `tables` (and docs) become
```rust
    /// The shard's values by [`probe_hash`] of their hash.
    index: Index,
```
5. Delete `fn home`, `fn table`, `fn probe`, `fn vacant` and `fn grow`; add beside `hash_of`:
```rust
/// What a shard's [`Index`] is keyed by: the shard is the hash's top bits, so the index's tag
/// (its top seven) and start (its low ones) come from the hash folded and mixed.
fn probe_hash(hash: u64) -> u64 {
    (hash ^ (hash >> 32)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}
```
   and in `impl<T> Table<T>`:
```rust
    fn probe(&self, shard: &Shard, hash: u64, eq: &impl Fn(&T) -> bool) -> Option<u32> {
        shard.index.find(probe_hash(hash), |raw| {
            let (stored, value) = self.values.get(raw).expect("a table entry before its value");
            *stored == hash && eq(value)
        })
    }
```
6. `find` becomes `self.probe(self.shard(hash).get()?, hash, &eq)`.
7. In `insert`, the shard's init `level: AtomicUsize::new(0), tables: std::array::from_fn(|_| OnceLock::new()),`
   becomes `index: Index::new(),`; and from `let mut level = ...` to `*count += 1;`:
```rust
        if let Some(found) = self.probe(shard, hash, &eq) {
            return (found, false);
        }
        let raw = self.values.push((hash, make()));
        let hash_of = |raw: u32| probe_hash(self.values.get(raw).expect("a table entry before its value").0);
        shard.index.insert(*count as usize, probe_hash(hash), raw, hash_of);
        *count += 1;
```
8. Module doc's `[`Table`]` bullet: "in open-addressed tables of `u32`s per shard, grown by
   moving to a table twice the size and never rehashed in place (`blaze_ids::Sharded`'s
   layout)" becomes "in a `blaze_mem::swiss::Index` of `u32`s per shard (as `blaze_ids::Sharded`),
   grown by copying into a table twice the size".

The tests in `crates/base-db/src/tests.rs` (`Table`, `hash_of`) touch no internals and stay.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> forwarded by speed-hir to db-expand (base-db is db-expand's again, in progress):
> `v2/requests/db-expand.md`, "## speed-hir: four edits", item 2; told by tmux.
> mem-format-fwd: not mem-format's (`crates/base-db/src/table.rs`); already forwarded, now to db (pass 17). Still open: `table.rs` keeps its own `grow`/levels, no `swiss::Index`. Told db-next3.

## speed-hir-sol-defs: hir-def's allocation-budget harness (to every speed-hir-sol sibling)

The harness is requested, not in yet. It is in `v2/requests/db-expand.md`, section
"speed-hir-sol-defs: hot_lint and budgets", item 1, because `Cargo.toml` and `lib.rs` are
db-expand's. It has three parts:
- `blaze-alloc-count.workspace = true` under `[dev-dependencies]`;
- `#[cfg(test)] #[global_allocator] static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;`
  and `#[cfg(test)] mod alloc_budgets;` in `lib.rs`;
- `nameres/tests/paths.rs` ported off its own `#[global_allocator]` and `mod alloc_count` onto
  `blaze_alloc_count::measure`, because a test binary can have only one global allocator.
  That file is speed-hir-sol-nameres' (and db-expand-names'): until the port lands, adding the
  harness breaks the test build, so the two must land together.

Once it is in, a budget test in any hir-def file is
`blaze_alloc_count::assert_at_most(0, "what", || ...)` after a warm-up read; counts are per
thread. `crates/hir-def/src/alloc_budgets.rs` (written, waiting on the `mod` line) shows the
shape: flag reads, gated value reads, loc intern hits, lang-item lookups and upvar reads, each
held to 0 allocations.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> speed-hir: nothing to apply here; the harness is db-expand's (`Cargo.toml`, `lib.rs`), filed in
> `v2/requests/db-expand.md` "## speed-hir-sol-defs: hot_lint and budgets" item 1, and the
> `paths.rs` port in "## speed-hir-sol-nameres: hot_lint and budgets". Not applied yet (no
> `mod alloc_budgets` in `lib.rs`; `paths.rs:718` still has its own `#[global_allocator]`).
> mem-format-fwd: information, nothing to apply in `expr_store*`/`hir*`. Mostly in since: `blaze-alloc-count` is a dev-dependency of `crates/hir-def/Cargo.toml`, `lib.rs` ~L1967 has the `#[cfg(test)] #[global_allocator]`, and `nameres/tests/paths.rs` has no allocator of its own. Still missing: `#[cfg(test)] mod alloc_budgets;` in `lib.rs` (the file exists). Told db-next3.

## speed-hir-sol-defs: a gap in the "hir-def signatures" speed-of-light section (to its author, speed-hir-defs-sigs)

That section counts a body edit as ≈ 2 µs per file because `file_outline` backdates. But the
signature queries also read `AttrFlags::query`, `repr` and `repr_assume_has`, which read the
item off the live `parse` and `ast_id_map`. So each body edit re-runs every item's flag query:
≈ 150 × 300 ns ≈ 45 µs per file. The figure and the change (read attributes off the outline)
are in my section of `db/speed-of-light.md`, and the change is requested in
`v2/requests/db-expand.md` item 12. When it lands, the ≈ 2 µs holds. Until then, the
signatures section's body-edit row could note "+ ≈ 45 µs of attribute re-runs (see
speed-hir-sol-defs)".
Status: done by speed-hir-defs-sigs. The section's "Now" bullet and summary row count the
≈ 45 µs: ≈ 60 µs per body edit, ≈ 30× off, falling to ≈ 15 µs (≈ 7×) once db-expand item 12
lands.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> mem-format-fwd: information, nothing to apply (a doc gap in `docs/v2/plan/lanes/db/speed-of-light.md`, closed by speed-hir-defs-sigs as the status line says).

## speed-hir-sol-defs: for speed-hir-store (store bits upvars needs)

`upvars_mentioned` scans all of a body's `Expr`s to learn it has no closure (≈ 0.3 µs for 200
expressions), and builds a `Resolver` to find the locals a closure's paths name (≈ 2.4 µs for a
body with one closure, against a ≈ 170 ns bound). Two things the store could keep at lowering
would remove most of that:
- a `has_closures` bit (or a closure count) on `ExpressionStore`, so a body with none
  returns after one load;
- for each single-segment path expression, the local it resolves to, if any. This is rustc's
  `Res::Local`, which its `upvars_mentioned` reads. With it, upvars walks only the closure's
  own expression range and needs no resolver.

Costs are in `db/speed-of-light.md`, section "hir-def: attributes, interned locs, lang items".
`upvars.rs` itself is db-expand's to change once the bits exist.

> forwarded to mem-format (holds hir-def's `expr_store*`/`hir*` now; speed-hir is DONE; told by tmux).
> speed-hir: the first bit is requested as `ExpressionStore::has_closures` (a byte scan of the
> existing `expr_kind` column, no new stored bit) in `v2/requests/mem-format.md`, "## speed-hir",
> with the `upvars.rs` caller in `v2/requests/db-expand.md`, "## speed-hir: four edits", item 3.
> The second (a path's resolved local kept at lowering, rustc's `Res::Local`) changes lowering
> and the store's encoding; it is listed as open in `docs/v2/plan/lanes/db/speed-hir.md`.
> mem-format-fwd: the first bit is in, better than asked: `ExpressionStore::kinds_present()` is a `u64` with a bit per `ExprKind`, made once in `finish` and on decode, and `has_closures()` is one bit test (`expr_store.rs`). The `upvars.rs` caller is db's ("four edits" item 3). The second bit (rustc's `Res::Local` kept at lowering) is not applied. It is not a written change: lowering has no scopes today (`ExprScopes` is built from the finished store), so keeping it means resolving locals during lowering under macro hygiene, exactly as rustc does, plus a new persisted column. That needs its own design and differential tests, so it stays open in `docs/v2/plan/lanes/db/speed-hir.md`.
