# Requests of lane `lints` (crates/blaze-lints)

Design: docs/v2/plan/lanes/lints.md, docs/v2/plan/lanes/lints/cx.md.

## db: `hir::BodyAnalysis`, one body's types and resolutions by pointer into its relative tree

- What: a public type in `crates/hir` (new file `crates/hir/src/body_analysis.rs`, re-exported
  `hir::BodyAnalysis`), built on `SourceAnalyzer`'s internals over
  `hir_def::expr_store::Body::relative`'s source map rather than `body_with_source_map`'s:

  ```rust
  pub struct BodyAnalysis<'db> { /* db, def, file, body, source map; inference in a OnceCell */ }
  impl<'db> BodyAnalysis<'db> {
      /// Reads `body_syntax` and `Body::relative` (for a body a macro writes, the expansion's
      /// tree and `Body::with_source_map`); never `infer`.
      pub fn new(db: &'db Db, def: DefWithBody) -> BodyAnalysis<'db>;
      /// The tree every pointer below is into, and its file: `BodySyntax::root` and its
      /// `file_id`, or the expansion's root and its macro file.
      pub fn tree(&self) -> (SyntaxNode, HirFileId);
      pub fn def(&self) -> DefWithBody;
      pub fn body(&self) -> &'db hir_def::expr_store::Body;
      pub fn source_map(&self) -> &'db hir_def::expr_store::BodySourceMap;
      /// Infers the body the first time it is called.
      pub fn infer(&self) -> &'db hir_ty::InferenceResult<'db>;
      // Each of these infers the body the first time one is asked:
      pub fn type_of_expr(&self, expr: InFile<AstPtr<ast::Expr>>) -> Option<TypeInfo<'db>>;
      pub fn type_of_pat(&self, pat: InFile<AstPtr<ast::Pat>>) -> Option<TypeInfo<'db>>;
      pub fn expr_adjustments(&self, expr: InFile<AstPtr<ast::Expr>>) -> Option<Vec<Adjustment<'db>>>;
      pub fn resolve_method_call(&self, call: InFile<AstPtr<ast::MethodCallExpr>>) -> Option<Function>;
      pub fn resolve_path(&self, path: InFile<AstPtr<ast::Path>>) -> Option<PathResolution<'db>>;
      pub fn binding(&self, pat: InFile<AstPtr<ast::IdentPat>>) -> Option<Local<'db>>;
      /// The callee of a call or method call: the impl's function when the arguments say
      /// which (`lookup_impl_method`), else the trait's.
      pub fn callee(&self, expr: hir_def::hir::ExprId) -> Option<(hir_def::FunctionId, hir_ty::next_solver::GenericArgs<'db>)>;
      /// `ty` with the body's environment, for `Type`'s methods (`is_copy`, `impls_trait`).
      pub fn ty(&self, ty: hir_ty::next_solver::Ty<'db>) -> Type<'db>;
      // No inference:
      pub fn expansion(&self, call: InFile<AstPtr<ast::MacroCall>>) -> Option<MacroCallId>;
      /// Where `expr` is written in `tree()`: its own range, or the outermost macro call of
      /// the tree that wrote it.
      pub fn expr_range(&self, expr: hir_def::hir::ExprId) -> Option<TextRange>;
      // Added by lints-driver (2026-10-29), no inference either: what `Semantics` answers of
      // a node, in the scope of the block the node is in, for nodes of `tree()` (the body's
      // expressions, patterns and statements, and items nested in it: their fields and
      // signatures), so that a body's lint query never reads the file's whole tree:
      /// A written type lowered in its scope, with the body's environment
      /// (`Semantics::resolve_type`).
      pub fn resolve_type(&self, ty: InFile<AstPtr<ast::Type>>) -> Option<Type<'db>>;
      /// The macro a call calls (`Semantics::resolve_macro_call`).
      pub fn resolve_macro_call(&self, call: InFile<AstPtr<ast::MacroCall>>) -> Option<Macro>;
      /// `name` as a path of one segment in the scope at `at`
      /// (`SemanticsScope::speculative_resolve`).
      pub fn resolve_name(&self, at: InFile<SyntaxNodePtr>, name: &Name) -> Option<PathResolution<'db>>;
  }
  ```
- What the lints rely on: `body()` is the same `Body` as `Body::of(db, def)`, its `ExprId`s
  the ones `infer()` keys; `def()` is the `hir::DefWithBody`; `ty(Ty)` is a `hir::Type` in the
  body's environment; `expr_range(ExprId)` is a range of `tree()`'s root (the expression's,
  or its outermost macro call's), `None` for what a macro made up with no call in that tree.
- Where: `crates/hir/src/body_analysis.rs` (new), `crates/hir/src/lib.rs` (`mod` and `pub use`).
- Why: blaze-lints runs each lint per body as a query over `body_syntax`'s tree (lints.md), so
  an edit lints only the bodies it changed. `Semantics` answers only for nodes of the file's
  whole tree, through the file-positioned source map, so a query asking it would depend on
  the whole file and run again on every edit above the body. Pointers (`AstPtr`) are the key
  because the lints walk `syntax::view`s and `hir` still takes handles: a pointer is the same
  for both. Lazy inference keeps "a lint that needs only syntax never forces inference".
  Until it exists, blaze-lints is written against this exact API.

> forwarded to db (`crates/hir`; told by tmux).
> applied by db-audit-other-bodyanalysis (2026-10-29 23:10): `crates/hir/src/body_analysis.rs`,
> re-exported `hir::BodyAnalysis`, this exact API; answers through `SourceAnalyzer` built over
> the relative maps of the store a node is in (the body's, the item's signature or fields, or an
> item nested in the body, found from `Body::blocks`' item scopes), scopes climbed out of
> expansions by the store's own calls; tested in `crates/hir/src/body_analysis/tests.rs`. Not
> built (no-build lane). Left: `resolve_path`'s fresh lowering of a path still makes
> `ExprCollector::new`, which reads the file's ast id and span maps (request in `db.md`).
> applied by db-audit-other (23:16): the gap is closed and the rest is checked. `SourceAnalyzer` has `maps` (`crates/hir/src/source_analyzer.rs:87`, `None` at every other constructor), and its four path re-lowerings go through `path_collector` (:301, `ExprCollector::with_maps`, which the lower.rs owner made `pub`). `BodyAnalysis` sets `maps` to the anchor's `BodySyntax` maps for a node of the body's own file (`body_analysis.rs:472`), so no answer reads the file's whole maps. Every method blaze-lints calls exists with its caller's signature (`pub use` at `crates/hir/src/lib.rs:167`). Not compiled.

## diagnostics: register blaze's new lints and the group `blaze::hot`

- What: in `crates/blaze-diagnostics/src/lints/registry/lints.rs`, next to
  `blaze::const_impure`, six lints, each `Allow` by default, and a group:
  - `blaze::hot_clone`: "detects a `.clone()` that allocates or bumps a reference count in hot code";
  - `blaze::hot_alloc`: "detects a call that reaches the global allocator in hot code";
  - `blaze::hot_dyn`: "detects a call through `dyn` in hot code";
  - `blaze::std_hashmap`: "detects std's `HashMap` and `HashSet` with their default hasher";
  - `blaze::parallelizable`: "detects a sequential loop or iterator chain whose iterations are
    independent and worth running in parallel";
  - `blaze::serial_dependence`: "detects a hot loop kept serial by one loop-carried dependence";
  - group `blaze::hot` = the four hot lints (`hot_clone`, `hot_alloc`, `hot_dyn`,
    `std_hashmap`); the two parallel lints are in no group.
  The command line accepts `--warn=blaze::hot_alloc` and the rest (from `[lints.blaze]`).
  (Updated 2026-10-29 for child `lints-parallel`; `blaze_lints::blaze::Lint::ALL` has the same six.)
- Why: they run in blaze-lints (`blaze clippy`), but rustc's level machinery in
  blaze-diagnostics sees `#[deny(blaze::hot)]` and `--deny=blaze::hot_alloc`: without them
  registered, it says `unknown_lints`. Allow by default, so nothing changes for a crate that
  doesn't name them (superset-of-rust).

> forwarded to diagnostics (told by tmux).

## codegen: `blaze-engine` hands blaze-lints `&base_db::Db`

- What: `crates/blaze-engine/src/check.rs` `Checker::lints`: call
  `blaze_lints::serve::answer(db, &scope.linted, &self.root, &path_of, &ask)` with `db: &Db`
  (the engine's database once it is `base_db::Db`), `scope.linted: &[base_db::Crate]`,
  `path_of: &dyn Fn(base_db::FileId) -> Option<PathBuf>`. The signature of `answer` is
  otherwise unchanged.
- Why: blaze-lints no longer takes `ide_db::RootDatabase` (deleted at integration).

> forwarded to codegen (told by tmux).

## cargo: `[lints.blaze]` read as `[lints.clippy]` is

- What: wherever blaze-cargo reads a package's `[lints]` for rustc's command line (the check
  and the build, not only `cargo clippy`), a `blaze` tool table becomes
  `--<level>=blaze::<name>` flags as `clippy`'s does, and no "unrecognized lint tool" warning
  is given for it. `blaze_lints::manifest::lints` and `flags` already read it for
  `blaze clippy` (`crates/blaze-cargo/src/cargo/own/clippy.rs` needs no change).
- Why: `[lints.blaze]` is how a crate opts in to blaze's own lints (lints.md), a blaze
  extension: a crate without the table sees nothing new.

> forwarded to cargo (told by tmux).

## coordinator: at integration

- Delete `tools/v2/hot_lint.py` once blaze-lints' `blaze::` lints build and their tests (its
  patterns as fixtures, `crates/blaze-lints/src/blaze/tests.rs`) pass, and add to the root
  `Cargo.toml` `[workspace.lints.blaze]` with `std_hashmap = "deny"`; in each crate of
  hot_lint.py's `HOT` list, `[lints.blaze] hot = "deny"` (its `[lints]` then stops being only
  `workspace = true`: cargo's `[lints]` can't extend the workspace's, so the hot crates spell
  out the workspace table and add `blaze`). Point common.md's hot_lint step at
  `blaze clippy`.
- `crates/blaze-lints/Cargo.toml` drops `ide-db` and gains `base-db`, `hir-def`, `hir-ty`,
  `salsa`, `triomphe` (all workspace dependencies already): no new workspace dependency.

> forwarded to the coordinator, for integration: deleting `tools/v2/hot_lint.py`, the hot crates' `[lints.blaze]`, common.md and blaze-lints' manifest are its. requests adds the root `[workspace.lints.blaze] std_hashmap = "deny"` only then: before blaze-lints' lints build, cargo would warn about the unknown tool.

## frontend-tree: answers on the views (2026-10-29)

1. **Untyped view.** `syntax::view::Node<'t>` is `syntax_tree::view::Node<'t, RustLanguage>`
   (`crates/syntax/src/view.rs` L37). Likewise `syntax::view::{Token, Element, NodeId}` and the
   iterator aliases (`Children`, `ChildrenWithTokens`, `Descendants`, `DescendantsWithTokens`,
   `Preorder`, `PreorderWithTokens`). `Element<'t>` is `NodeOrToken<Node<'t>, Token<'t>>`,
   `NodeOrToken` and `WalkEvent` as today. A view is 24 bytes, `Copy`, `Send + Sync`, and
   compares by `(file, part, element)`. `text()` is `&'t str`. A lazy body is entered as if it
   were in the tree.
2. **Extension layer.** Same names, ported by child `frontend-tree-ast` (running now), into
   `crates/syntax/src/view/{node_ext,expr_ext,token_ext,prec}.rs`.
   - Already there:
     - every generated accessor, for example `MacroCall::token_tree()`;
     - every trait, in `view/traits.rs`: `HasAttrs::attrs`, `attrs_with_doc`,
       `inner_attributes_node`, `HasName`, `HasVisibility`, `HasGenericParams` and the rest,
       plus `attrs_including_inner(&owner)`.
   - Being written: `simple_name`, `String::value`, `BinExpr::op_kind` and the rest of
     `node_ext`, `expr_ext` and `token_ext`.
   - Differences from the handle versions:
     - Methods take `self`.
     - Text that the handle returned owned only because it could not lend it is `&'t str`.
       Expect `simple_name() -> Option<&'t str>` where the handle gives a `SmolStr`.
     - `value()` keeps `Result<Cow<'t, str>, EscapeError>`.
     - `op_kind()` returns the same operator enums, `syntax::ast::{BinaryOp, ArithOp, ...}`,
       reused rather than copied.

   I'll tmux `lints` when the child reports DONE, with any name that differs.
3. **Source maps to a view.** Source maps keep handing out `InFile<AstPtr<N>>`, with `N` the
   handle type used only as a kind tag; no handle is ever made from it. To get a view, write
   `ptr.value.to_view(root)`, which gives `<N as Typed>::Of<'db>`, e.g. `syntax::view::Fn<'db>`.
   `root` comes from the database:
   - For a real file, `editioned_file_id.parse(db).root()` works today. `parse` is
     `returns(ref)`, so the view borrows `&'db Db` and lives as long as it.
   - For any `HirFileId`, use `file_id.root(db: &Db)`. It is requested of db in
     `v2/requests/db.md`, together with `AstId::to_view(db)`, so it is not there until db
     answers.

   `syntax::view::{ptr, to_node, try_to_node}` cover the untyped case (`SyntaxNodePtr`).
   `CfgExpr::parse_from_view(view::CfgPredicate<'_>)` exists in `cfg` beside `parse_from_ast`.

`syntax::view` becomes reachable once frontend-deps adds `pub mod view;` to `syntax/src/lib.rs`,
which is requested in `v2/requests/frontend-deps.md`.

> forwarded to lints (answers for it; told by tmux).

## frontend-db: the API to write blaze-lints against (answer; 2026-10-29)

1. `hir::Semantics` stays and becomes non-generic: `Semantics<'db>` with `pub db: &'db Db`
   (today `Semantics<'db, DB: ?Sized>` holding `&'db DB`, over a `SemanticsImpl` holding
   `&'db dyn HirDatabase`). With one database type, the parameter carries nothing.
   `Semantics::new(db: &'db Db)`. frontend-db's child `frontend-db-hir` makes the change.
2. Queries keep the form they have today; only the database type changes:
   - The free/associated query functions stay as they are, over `&Db`:
     `InferenceResult::of(db, def)`, `Body::of(db, def)`, `Body::with_source_map(db, def)`,
     `hir_def::lang_item::lang_items(db, krate)`, `AttrFlags::lang_item(db, owner)`.
   - What is a `HirDatabase` method today stays one (`db.ty(def)`, `db.mir_body(def)`,
     `db.const_eval(..)`, ...). It is a sealed extension trait with the one impl for `Db`, so
     you need `use hir::db::HirDatabase;` in scope, and the calls are static.
   - `SourceDatabase`'s methods (`db.file_text(..)`, `db.source_root(..)`, ...) are `Db`'s
     inherent methods, with no import.
   Through `hir`'s own API (`hir::Function`, `hir::Module`, ...), every method that took
   `db: &dyn HirDatabase` takes `db: &Db`.
3. `base_db::EditionedFileId` (the interned one, `crates/base-db/src/editioned_file_id.rs`) is
   what queries take; `span::EditionedFileId` is its raw value and `base_db::FileId` the VFS
   id (`vfs::FileId`, re-exported). Keep using them as `hir` re-exports them today.

Test databases: `type TestDB = base_db::Db;` made by `Db::for_tests()` (no `Default`), with
`use hir as _;` so the MIR providers are linked. `test-fixture`'s `WithFixture` is implemented
for `Db`.

> forwarded to lints: an answer for its asker, nothing to apply.

> applied by frontend-db-hir (item 1): `hir::Semantics<'db>` is non-generic, `pub db: &'db Db`, made by `Semantics::new(db: &'db Db)` or `new_within`; `new_dyn` is gone. `blaze-lints/src/check.rs`'s `Semantics<'db, RootDatabase>` became `Semantics<'db>`.

## dedyn: crates/blaze-lints: `dyn Fn` sweep (second sweep item 5, 2026-10-29): none is per item; `// dyn:` lines
> APPLIED by dedyn-misc 2026-10-29 21:50, re-decided: `check.rs`'s `NameOf` and the four
> `name_of: &dyn Fn` in `diag.rs` keep `dyn` with a `// dyn:` line (once per file and per span
> leaving its file; a generic would copy the ~500-line driver per caller). `serve.rs`'s
> `answer` takes `path_of: &impl Fn` (one production caller, blaze-engine), no comment needed.
> Being applied in the tree by dedyn-misc (dedyn, 2026-10-29 21:22, new edit rule): don't apply it again.


Read for where each is called: once per diagnostic span or per file, never per node a lint
walks. No code change; add the line above each:

- `src/diag.rs:56`, `:106`, `:175`, `:326` (`name_of`): `// dyn: the driver's file names, asked once per span of a diagnostic, and only for one that leaves its file.`
- `src/check.rs:62` (`NameOf`): `// dyn: as `diag.rs`'s `name_of`: once per diagnostic span.`
- `src/emit.rs:32` (`read`): `// dyn: the source a diagnostic quotes, read once per file shown.`
- `src/serve.rs:83` (`path_of`): `// dyn: one call per crate root and per file a finding names.`

(`src/blaze/tests.rs:512` and `src/lints/more_types_tests.rs:258` are lint test fixtures: text
about the Rust being checked, left alone.)

> lints, 2026-10-29 21:25: the line numbers predate the dedup with blaze-diagnostics (applied
> 21:12). `src/emit.rs` is deleted, so it has no `read` comment. `src/serve.rs`'s `written` takes
> `read: impl Fn`, so no line is needed there. In `src/diag.rs`, `name_of` is now a parameter of
> the free functions `place_diag`, `place_multi`, `place` and `place_expansion`, so the comment
> goes above each `name_of: &dyn Fn` there. `check.rs`'s `NameOf` and `serve.rs`'s `path_of`
> are as listed.
> forwarded to dedyn (dedyn-misc is applying it, per the note above; requests, pass 23).

## frontend-tree: blaze-lints check.rs: onto views (1) (2026-10-29)

From frontend-tree-move-expand (`docs/v2/plan/lanes/frontend/tree-move-expand.md`). Needs
`## frontend-tree: hir-expand lib.rs + files.rs: original_call_node_view, and the handle functions
nobody calls go (9)` in `v2/requests/db-expand.md` first (it adds
`HirFileId::original_call_node_view`). `lint_body` reads only the outer call's file and range, so
the view does the same with no handle made.

File `crates/blaze-lints/src/check.rs`. Old:

```rust
                let Some(outer) = file_id.original_call_node(db) else { return };
```

New:

```rust
                let Some(outer) = file_id.original_call_node_view(db) else { return };
```

Why: the outermost call in the real file, as a view (`InRealFile<view::Node<'_>>`); `outer.file_id`
and `outer.value.text_range()` read the same values. `crates/hir-expand/src/view_tests.rs`
compares `original_call_node_view` with `original_call_node` on every macro file of its fixture.

> forwarded to lints (lints-driver-next1, its live window; lane lints is DONE, and blaze-lints is otherwise the coordinator's); told by tmux (requests, pass 28).
> accepted, waiting (lints-driver, 2026-10-29 22:09): `HirFileId::original_call_node_view` is
> not in `crates/hir-expand/src/lib.rs` yet, so the line stays until db-expand's request 9 lands;
> then it is the one line as written. Logged as pending in /tmp/blaze-lanes/v2-lints-driver.md.
> applied (lints-driver, 2026-10-29 23:48): request 9 landed; check.rs line as written.
