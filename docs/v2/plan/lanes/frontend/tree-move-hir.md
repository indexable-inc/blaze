---
status: plan
summary: frontend-tree-move-hir, hir and hir-ty* onto syntax views; the view twins hir hands to blaze-diagnostics, blaze-engine and the tools (hir::view::Semantics, HasSourceView, the item source twins).
date: 2026-10-29
---
# frontend-tree-move-hir: `hir` and `hir-ty*` onto views

Part of [tree.md](tree.md) (frontend.md item 16). `hir` is how the build path reads syntax:
`blaze-diagnostics`, `blaze-engine`, `blaze-lints`, `blaze-doc`, `blaze-wasm`, `blaze-intent`,
`hir-ty-mir` and `hir-ty-codegen` call `Semantics` (about 60 methods, 500 call sites),
`HasSource::source` (220 sites), `Module::{definition,declaration}_source` and the item
source methods. Every one of them gives or takes a counted handle today.

## The twins (declared; program against these)

All of it is additive: the handle API stays until its last caller moves, then the same request
deletes it. Every twin has the handle version's meaning exactly (same nodes, ranges, order and
`None`s), and a test beside it compares the two on `hir`'s fixtures element by element.

### `hir::view::Semantics<'db>`

A second `Semantics` type, in `crates/hir/src/semantics/view.rs` (module `crate::semantics::view`,
re-exported as `hir::view`). Callers write `use hir::view::Semantics;` (or
`hir::view::Semantics::new(db)`) and keep their bodies: every method has the handle method's
name and meaning, takes views by value (`ast::Fn<'db>`, `Node<'db>`, `Token<'db>`; all `Copy`)
and gives views borrowed from the database, `'db` from `db: &'db Db`. There is no `Deref` to
the handle `SemanticsImpl`: a caller that still passes a handle gets a type error, not a quiet
handle path.

- `pub db: &'db Db`, `new(db)`, `new_within(db, crates)`.
- **Roots it hands out, and remembers.** `parse(EditionedFileId) -> ast::SourceFile<'db>`,
  `parse_guess_edition(FileId) -> ast::SourceFile<'db>`, `parse_or_expand(HirFileId) -> Node<'db>`,
  `expand(MacroCallId) -> ExpandResult<Node<'db>>`, `source<D: HasSourceView>(d) ->
  Option<InFile<D::Ast<'db>>>`, `find_parent_file(HirFileId) -> Option<InFile<Node<'db>>>`,
  `module_definition_node(Module) -> InFile<Node<'db>>`, `expand_macro_call(ast::MacroCall<'db>)
  -> Option<InFile<Node<'db>>>`, `expand_attr_macro(ast::Item<'db>) ->
  Option<ExpandResult<InFile<Node<'db>>>>`, `expand_derive_macro(ast::Attr<'db>) ->
  Option<Vec<Option<ExpandResult<Node<'db>>>>>`. Each records its tree's address
  (`node.file()` as `*const FlatTree`, the file tree, never a part) with the `HirFileId`.
  A view's file is found again from that address: `hir_file_for(Node<'db>) -> HirFileId`. A
  view of a tree this `Semantics` never handed out panics naming the node, as the handle
  version does: no quiet lookup by another route.
- **To defs.** `to_def<T: ToDef<'db>>(T) -> Option<T::Def>`, the `to_*_def` family
  (`to_fn_def(ast::Fn<'db>)`, ...), `file_to_module_def(s)`, `hir_file_to_module_def(s)`.
  `hir::view::ToDef<'db>: view::AstNode<'db> + Copy` with the handle trait's impls, view types.
  The dyn maps, `ChildContainer` and the file-to-module tables are the handle `Semantics`'s
  own (keyed by `AstPtr` kind tags and ids, no handles), shared, not copied.
- **Walking up through macros.** `ancestors_with_macros(Node<'db>) -> impl Iterator<Item =
  Node<'db>>`, `ancestors_with_macros_file(..) -> impl Iterator<Item = InFile<Node<'db>>>`,
  `token_ancestors_with_macros(Token<'db>)`.
- **Down into macros.** `descend_into_macros_exact(Token<'db>) -> SmallVec<[Token<'db>; 1]>`.
- **Ranges and attributes.** `original_range(_opt)`, `original_ast_node`,
  `original_syntax_node_rooted`, `diagnostics_display_range(_for_range)`, `lint_attrs`,
  `is_attr_macro_call`, `is_derive_annotated`, `resolve_derive_macro`,
  `expand_derive_as_pseudo_attr_macro`, `expand_allowed_builtins`, `store_owner_for`.
- **Scopes and resolution** (same signatures, views for handles): `scope(Node<'db>) ->
  Option<SemanticsScope<'db>>` (the shared `SemanticsScope`; its
  `speculative_resolve_view(ast::Path<'_>)` is the view twin; `speculative_resolve_mod_path(&ModPath)`,
  which blaze-lints calls, is frontend-tree-move-tools' request in `v2/requests/db.md`),
  `resolve_path(_with_subst)`,
  `resolve_path_per_ns`, `resolve_mod_path`, `resolve_method_call`, `resolve_method_call_as_callable`,
  `resolve_expr_as_callable`, `resolve_field`, `resolve_record_field(_with_substitution)`,
  `resolve_record_pat_field(_with_subst)`, `resolve_variant`, `resolve_bind_pat_to_const`,
  `resolve_macro_call`, `resolve_macro_call2`, `is_proc_macro_call`, `resolve_macro_call_arm`,
  `resolve_attr_macro_call`, `resolve_type`, `resolve_lifetime_param`, `resolve_label`,
  `resolve_offset_of_field`, `resolve_tuple_struct_pat_fields`, `type_of_expr`, `type_of_pat`,
  `type_of_binding_in_pat`.
- **No syntax in it**: `get_unsafe_ops`, `is_nightly`, `first_crate`, `attach_first_edition(_opt)`,
  `adjust_edition`, `impl_generated_from_derive` take and give ids, so the view type has them
  under the same names, answered by the shared code.
- **Not on the view type** (no build-path caller; a caller that needs one asks this lane):
  `expr_adjustments`, `is_inside_unsafe`, the offset lookups (`find_node_at_offset_*`,
  `scope_at_offset`), `descend_into_macros` other than `_exact`, and the IDE-only list below.

Files: `semantics/view.rs` (the type, roots, ids), `view/source_to_def.rs` (to-def, walking
up, ranges, expansions), `view/analyze.rs` (scopes, resolution, descending),
`view/has_source.rs` (`HasSourceView` and the item twins), `view/tests.rs`, and
`source_analyzer/view.rs`: `SourceAnalyzer`'s `_view` twins, a child module so it calls the
handle analyzer's private resolve helpers rather than copying them.

### `hir::view::HasSourceView`

```rust
pub trait HasSourceView: Sized {
    type Ast<'db>: syntax::view::AstNode<'db>;
    fn source_view<'db>(self, db: &'db Db) -> Option<InFile<Self::Ast<'db>>>;
}
```

Implemented for every type with `hir::HasSource`, `Ast<'db>` the view of its `Ast`
(`Function` gives `ast::Fn<'db>`, `Macro` `Either<ast::Macro<'db>, ast::Fn<'db>>`, `Param`
`Either<ast::SelfParam<'db>, ast::Param<'db>>`, `Local` `Either<ast::IdentPat<'db>,
ast::SelfParam<'db>>`, `Field` `FieldSourceView<'db>`). It finds the node from the def's
`AstPtr` (`hir_def::src::HasSource::ast_ptr`) under `HirFileId::root(db)`: no handle and no
reparse. `source_with_range_view` likewise.

### Item source twins (inherent, beside the handle ones)

- `Module::definition_source_view(db) -> InFile<ModuleSourceView<'db>>`,
  `Module::declaration_source_view(db) -> Option<InFile<ast::Module<'db>>>`
  (`ModuleSourceView` is hir-def's twin of `ModuleSource`, re-exported).
- `FieldSourceView<'db> { Named(ast::RecordField<'db>), Pos(ast::TupleField<'db>) }`, a
  `view::AstNode<'db>`.
- `Local::sources_view(db) -> Vec<LocalSourceView<'db>>`, `primary_source_view(db) ->
  LocalSourceView<'db>`; `LocalSourceView<'db>` has `LocalSource`'s methods (`as_ident_pat`,
  `into_ident_pat`, `name`, `syntax`, `syntax_ptr`, `is_param`, ...) over views.
- `Const::value_view`, `Static::value_view` (`Option<ast::Expr<'db>>`), `Param::pattern_source_view`,
  `Use::source_view`, `ConstParam`/`GenericArg` const-arg twins.

### Kept as ids (no twin)

`hir::diagnostics`' `InFile<AstPtr<ast::X>>` and `InFile<SyntaxNodePtr>` fields,
`match_lints`' pointers, `ClosureCapture`'s `AstPtr<Either<ast::Expr, ast::Pat>>`: kind tags
by the tree.md convention. A reader turns one into a view with `ptr.value.to_view(sema.parse_or_expand(ptr.file_id))`
(or `AstPtr::to_view(root)` on `HirFileId::root(db)` when it doesn't need `Semantics`).

## What `hir` needs from below

- hir-expand (`frontend-tree-move-expand`, in `v2/requests/db-expand.md`): `MacroCallLoc::to_node_view`,
  `to_node_item_view`, `MacroCallKind::arg_view`, `MacroCallId::call_node_view`,
  `ExpansionInfo::{expanded_view, arg_view, map_range_down_view}`,
  `InFile<Node>::{ancestors_with_macros_view, original_*_view}`, `expand_cfg_attr_view`,
  `find_attr_range_with_source_view`, `AstPathExt for view::Path`, `AsName` for the view names.
- hir-def (`frontend-tree-move-def`): `ExpressionStoreSourceMap::{node_expr, node_pat,
  node_type, node_label, node_macro_file, macro_expansion_expr, expansion,
  implicit_format_args}_view(InFile<ast::X<'_>>)`, `ExprCollector::lower_path_view(ast::Path<'_>,
  &mut ImplTraitLowering<'_>)` (frontend-tree-move-def-lower, in the tree),
  `ModuleSourceView<'db>` with `ModuleData::definition_source_view` and
  `declaration_source_view`, `ModuleOrigin::definition_source_view`,
  `parse_extra_crate_attrs_view`, `AttrId::find_attr_range_view`.

## IDE-only modules (not moved)

No build-path crate calls these; they go with `crates/ide*`: `hir/src/term_search.rs`,
`term_search/`, `symbols.rs`, and the handle `Semantics` methods only IDE crates call
(`find_node_at_offset_*`, `find_namelike_at_offset_with_descend`, `speculative_expand_*`,
`rename_conflicts`, `as_format_args_parts`, `check_for_format_args_template*`,
`descend_into_macros{,_cb,_no_opaque,_breakable}`, `debug_hir_at`,
`get_failed_obligations`, `locals_used`, `fn_return_points`).

## hir-ty and the checkers

- `hir-ty/src/effects.rs` (`closure_expr_is_const`): `AstPtr::to_view` under
  `HirFileId::root(db)`, requested in `v2/requests/db.md`.
- `method_resolution.rs` (items', DONE; edited): `loaded_crates` walks `Parse::root()`'s
  `Node::flat()`, the same `(tree, at)` the handle `flat()` gave.
- `opaques.rs` names `ast::TypeAlias` as a kind tag only; `next_solver/format_proof_tree.rs` has
  no syntax.
- `hir-ty-borrowck/src/names.rs` (`from_external_macro`) and `check/tests.rs`'s `source`:
  `AstPtr::to_view` / `view::to_node`, requested in `v2/requests/db.md`.
- `lang_items.rs`, `infer/op.rs`, `expr_use_visitor.rs` use the operator enums, which the views
  share (`syntax::view::{BinaryOp, ...}` are `syntax::ast`'s own): nothing to move.
- `hir-ty-wfcheck`, `hir-ty-matchck`, `hir-ty-unsafeck`: no handle use (unsafeck's tests name
  `syntax::TextRange` only).

## Proof

`crates/hir/src/semantics/view/tests.rs`, on one fixture with fn-like and item macros, modules,
every item kind, generics, closures, labels, record and tuple patterns and a match. Every node
of the file and of every expansion is paired with its handle twin (same file, kind and range):

- `to_def_is_the_handles_for_every_node`: `to_def` for every `ToDef` kind.
- `ancestors_ranges_and_scopes_are_the_handles_for_every_node`: `hir_file_for`,
  `ancestors_with_macros` (kinds and ranges, in order), `original_range(_opt)`, `scope`'s file,
  module and crate.
- `types_and_resolutions_are_the_handles_for_every_node`: `type_of_*`, the callables,
  `resolve_path`, `resolve_path_per_ns`, `speculative_resolve_view`, method, field, record and
  variant resolution, macro calls and arms, attribute macros, lifetimes and labels.
- `descending_a_token_is_the_handles_for_every_token`: `descend_into_macros_exact`, tokens in
  order.
- `source_view_is_the_handle_source_for_every_def`: `HasSourceView` against `HasSource` for
  every module (definition and declaration), declaration, variant, field and impl.
- `a_warm_lookup_walk_allocates_nothing`: a repeated `to_def`/`hir_file_for`/
  `ancestors_with_macros` walk allocates nothing once the dyn maps are built
  (`blaze_alloc_count::assert_at_most(0, ..)`).

## State of the art

rustc's `TyCtxt::hir_node(HirId)` answers "what is at this id" by indexing an owner's node
array; nothing is counted. Here a view is a pointer and an index, finding its file is one hash
of the tree's address, and the def lookup is the shared dyn map: rustc's shape, with no handle
in it.
