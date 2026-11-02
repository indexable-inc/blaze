---
status: plan
summary: frontend-tree-move-def-lower, body and signature lowering (ExprCollector, expr_store/lower*, expander.rs) walking borrowed syntax views instead of handles; the signatures body.rs, signatures.rs and hir call, and what each test proves.
date: 2026-10-29
---
# frontend-tree-move-def-lower: lowering on views

Part of [tree-move-def.md](tree-move-def.md) ([tree.md](tree.md), frontend.md item 16). Paths, in
`crates/hir-def/src/`: `expr_store/lower.rs`, `expr_store/lower/**`, `expr_store/expander.rs`,
`expr_store/tests/**`. Owner lane format is done, so these are edited in place.

## Shape

`ExprCollector<'db>` stores no syntax: every lowering method takes the view it lowers by value,
with a lifetime of its own (`ast::Expr<'_>`), so one collector lowers the body's tree
(`BodySyntax::tree`, `'t`), the file's tree (`'db`) and macro expansions (`'db`) alike. Files say
`use syntax::view as ast;`; kind tags (`AstPtr<N>`, `ExprPtr`, `TypePtr`, `FileAstId<N>`) name the
handle types as `syntax::ast::X` (alias `tag`). A pointer is `AstPtr::<tag::X>::of(view)` (or the
`ExprPtr::of`-style alias), never a handle.

- **Expansions are borrowed, not cloned.** `MacroCallId::parse_macro_expansion` is
  `returns(ref)`, so `Expander::within_limit` views `res.value.0.root()` as the `'db` view
  `T::cast(root)` where the handle code cloned the expansion's `Arc` and `cast::<T>()` it.
  `collect_macro_call` is generic over the view type (`T: ast::AstNode<'db>`), so a callback's
  use still infers it, as with handles. A helper whose result is of the input's tree or of an
  expansion's (`expand_macros_to_string`, `collect_path`) returns a fresh lifetime `'v` that
  both outlive (`'t: 'v, 'db: 'v`); the caller's `'v` is local, so neither bound spreads.
- **Handle callers outside the crate** (`lower_path`, `lower_generic_params`) find their node
  as a view under their root's flat tree (`top_of`, `view_of`: one `Arc` count per call) and
  lower that, so twin and handle share one implementation.
- **cfg checks.** `check_cfg<'t>(owner: impl ast::HasAttrs<'t>)` reads one child for the fast
  path (`may_have_attrs`) and otherwise goes through one non-generic instance,
  `check_cfg_of(ast::AnyHasAttrs<'t>)` (dedyn's one-instance split, kept, with no `dyn`).
  `cfg!()` / `#[cfg]` predicates go to `CfgExpr::parse_from_view`.
- **Text is lent.** Names are made once from `&'t str` (`AsName` on views), lifetimes from
  `Lifetime::text()`, asm operands from token `text()`. No `clone()` of a view (they are `Copy`):
  the eight `// clone: a syntax node's count bump` sites go.
- **Tokens.** `comma_follows_token` steps `Token::next_token` over trivia on the view.

## Signatures (in place inside hir-def)

`lower.rs`, called by `body.rs` (parent) and `signatures.rs` (sigs):

```rust
pub(super) fn lower_body(db: &Db, owner: DefWithBodyId, syntax_node: SyntaxNodePtr,
    current_file_id: HirFileId, module: ModuleId, parameters: Option<ast::ParamList<'_>>,
    body: Option<ast::Expr<'_>>, is_async_fn: bool, is_gen_fn: bool,
    maps: Option<(&RealSpanMap, &AstIdMap)>, lowering: BodyLowering<'_>) -> (Body, BodySourceMap);
pub(crate) fn lower_type_ref(db, module, type_ref: InFile<Option<ast::Type<'_>>>, maps) -> ..;
pub(crate) fn lower_generic_params_within(db, module, def, file_id,
    param_list: Option<ast::GenericParamList<'_>>, where_clause: Option<ast::WhereClause<'_>>,
    mode, maps) -> ..;
pub(crate) fn lower_impl(db, module, impl_syntax: InFile<ast::Impl<'_>>, impl_id, maps) -> ..;
pub(crate) fn lower_trait(db, module, trait_syntax: InFile<ast::Trait<'_>>, trait_id, maps) -> ..;
pub(crate) fn lower_type_alias(db, container, alias: InFile<ast::TypeAlias<'_>>, id, maps) -> ..;
pub(crate) fn lower_function(db, module, fn_: InFile<ast::Fn<'_>>, function_id, maps) -> ..;
impl ExprCollector<'_> {
    pub(crate) fn lower_type_ref_opt(&mut self, node: Option<ast::Type<'_>>, f: &mut ImplTraitLowering<'_>) -> TypeRefId;
    pub(crate) fn lower_type_ref_disallow_impl_trait(&mut self, node: ast::Type<'_>) -> TypeRefId;
    pub(crate) fn lower_type_ref_opt_disallow_impl_trait(&mut self, node: Option<ast::Type<'_>>) -> TypeRefId;
    pub fn lower_const_arg(&mut self, arg: ast::ConstArg<'_>) -> ConstRef;
}
pub(in crate::expr_store) fn may_have_attrs<'t>(owner: impl ast::HasAttrs<'t>) -> bool;
```

`BodyLowering`, `ExprCollector::new`/`with_maps` and the results are unchanged. `lower/generics.rs`
(`GenericParamsCollector::{lower, with_self_param}`) and `lower/path.rs` take views the same way;
their callers are all in `lower*`.

## Edge twins (callers outside hir-def)

The handle versions stay until their last caller (in `crates/hir`) moves, then are deleted:

- `lower.rs`: `pub fn lower_generic_params_view(db, module, def, file_id,
  param_list: Option<ast::GenericParamList<'_>>, where_clause: Option<ast::WhereClause<'_>>,
  mode) -> (ExpressionStore, GenericParams, ExpressionStoreSourceMap)` beside
  `lower_generic_params` (`hir/src/source_analyzer.rs`).
- `lower.rs`: `ExprCollector::lower_path_view(&mut self, path: ast::Path<'_>,
  f: &mut ImplTraitLowering<'_>) -> Option<PathIdx>` beside `lower_path` (`hir/src/source_analyzer.rs`).
  Every in-crate caller uses `lower_path_view`.
- `lower.rs`: `pub fn hir_assoc_type_binding_to_ast_view<'t>(segment_args: ast::GenericArgList<'t>,
  binding_idx: u32) -> Option<ast::AssocTypeArg<'t>>` and `pub fn hir_generic_arg_to_ast_view<'t>(args:
  ast::GenericArgList<'t>, arg_idx: u32, has_self_arg: bool) -> Option<ast::GenericArg<'t>>`.
- `lower/path.rs`: `pub fn hir_segment_to_ast_segment_view<'t>(path: ast::Path<'t>, segment_idx:
  u32) -> Option<ast::PathSegment<'t>>` (`hir/src/diagnostics.rs`).

The handle `lower_generic_params` and `lower_path` find the handle's node as a view under the
tree of the handle's root (`AstPtr::to_view`, one `Arc` count on the cold IDE edge) and call the
view version, so both give the same store. The three `hir_*_to_ast` handle functions keep their
bodies.

## What this part needs from others

- hir-expand (frontend-tree-move-expand): `impl AsName` for `view::{Name, NameRef,
  NameOrNameRef, FieldKind}<'_>`; `ExpandTo::of_call_site_view(call: view::MacroCall<'_>) ->
  Result<ExpandTo, SyntaxKind>`; `ModPath::from_src_view(db, path: view::Path<'_>,
  span_for_range) -> Option<ModPath>`.
- attrs (frontend-tree-move-def-attrs): `AttrFlags::is_cfg_enabled_for<'t>(owner: &impl
  view::HasAttrs<'t>, cfg_options) -> Result<(), CfgExpr>`.
- parent (hir/**, expr_store.rs): `impl From<view::LiteralKind<'_>> for Literal` (hir.rs, in place:
  every caller is in `lower*`); `format_args::parse(s: view::String<'_>, ..)` (hir/format_args.rs,
  in place); the twins re-exported from `expr_store.rs` beside the handle names.
- span: `AstIdMap::{ast_id_of_view, ast_id_for_block_view}` (in the tree).

## Speed

A walk step is arithmetic on the flat tree's arrays (tree.md): no count bump per accessor, no
`Arc` clone per macro call, no `String` to compare text. Bound and arithmetic:
[speed-of-light.md](speed-of-light.md) (frontend lane) covers lowering as a whole; this part
removes the per-node handle costs, which are the only syntax costs lowering had.

## Proof

- `expr_store/tests/*`, `lower/path/tests.rs`: every existing lowering test runs on views
  unchanged in its expectations: the same pretty-printed bodies, signatures, scopes and source
  maps as on handles.
- `lower/path/tests.rs`: `hir_segment_to_ast_segment_view` maps every lowered segment back to the
  segment lowering read (keyed by pointer), and agrees with `hir_segment_to_ast_segment` on the
  same path, segment by segment; `lower_path` (handle) and `lower_path_view` print the same path.
- `tests/body.rs`: `hir_generic_arg_to_ast_view`/`hir_assoc_type_binding_to_ast_view` agree with
  the handle versions on every generic argument list of the fixtures.
- `tests/cfg_attrs.rs`: `may_have_attrs` on views never skips a node that has an attribute.
- `tests/alloc.rs`: the syntax side of the walk (cfg fast path, pointers, names' text, the comma
  check) over every node of a body allocates nothing.
