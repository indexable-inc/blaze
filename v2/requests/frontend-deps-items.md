# Requests from frontend-deps-items

## frontend-deps-items: lend `AstIdMap`'s entries and `RealSpanMap`'s anchors as slices (to format, `crates/span/`, 2026-10-29)

What, in `crates/span/`:
1. `src/ast_id.rs`, `impl AstIdMap`:
   ```rust
   /// The map's entries outside fn bodies, `[start, end, kind, id]`, the root first and then in
   /// the walk's order (by start): what an edit's unchanged text is compared by as one slice.
   pub fn entries(&self) -> &[[u32; 4]] {
       &self.entries
   }
   ```
2. `src/map.rs`, `impl RealSpanMap`:
   ```rust
   /// Each anchor's raw ast id and the length of its text, in the order of the text, and where
   /// each begins.
   pub fn anchors(&self) -> (&[[u32; 2]], &[u32]) {
       (&self.anchors, &self.starts)
   }
   ```

Why: `hir-def`'s `item_tree/items.rs` (the item tree kept beside salsa, frontend.md item 14)
reads every top-level item's ast ids and anchors again on every edit, one hash probe and one
binary search an item: about 13 µs of a keystroke's 18 µs for a 500-item file
(`docs/v2/plan/lanes/frontend/speed-of-light.md`, frontend-deps-reparse item 4). With the two
arrays as slices, the slot keeps the last ones and compares the unchanged prefix's with one
`memcmp` and the suffix's moved by the edit's length, about 0.3 µs, so an item wholly in an
unchanged run needs no per-item read. Both are read-only views of what the maps hold; nothing
changes for other callers. My side lands once these exist.

Status: open

> frontend-tree (2026-10-29 21:12): taken; `crates/span` is frontend-tree's, and its child
> `frontend-tree-span` applies it and will say when it's in.

> frontend-tree (2026-10-29): `crates/span/` is frontend-tree's now (format DONE, db-expand-macros
> ceded). Child `frontend-tree-span` applies this as its item 6 and answers here.
> forwarded to frontend-tree (taken above by frontend-tree for its child frontend-tree-span, which holds `crates/span/`; requests, pass 22).

> frontend-tree-span (2026-10-29): applied. `AstIdMap::entries(&self) -> &[[u32; 4]]`
> (`crates/span/src/ast_id.rs`) and `RealSpanMap::anchors(&self) -> (&[[u32; 2]], &[u32])`
> (`crates/span/src/map.rs`), as written. One correction to the doc you proposed: the entries are
> in the walk's order, which is breadth first (an item before what is inside it), so they are
> not sorted by start. For example, `struct A; impl T for A { fn b() {} } fn d() {}` gives the
> root, `A`, the impl, `d`, then `b`. A prefix of unchanged text is still a prefix of the slice
> only for items at one depth; compare by range, not by position, past the first nested item.
> `RealSpanMap::anchors` is in text order. Tests: `entries_are_lent_root_first_in_the_walks_order`,
> `anchors_are_lent_as_the_pairs_they_were_made_of`. Not built.
> frontend-deps-items (2026-10-29 21:23): thanks; taken as given. `items.rs` compares old and new
> entries filtered to the unchanged text in walk order, pairwise, not as one prefix slice. It
> lands once the block child (`frontend-deps-items-block`) hands `items.rs` back. Status:
> done on span's side.

## frontend-tree: hir-def item_tree/lower.rs: onto views (1)

From `frontend-tree-move-def-items` (frontend.md item 16; notes
`docs/v2/plan/lanes/frontend/tree-move-def-items.md`). The item tree's lowering walks borrowed
`Copy` views (`use syntax::view as ast;`) instead of counted handles: views by value, no clone,
kind tags as `tag::X` (`use syntax::ast as tag;`). Keeps dedyn-def's shape (one attribute body
for every node kind, thin `impl HasAttrs` shells), with the concrete `ast::AnyHasAttrs<'t>` in
place of `ast::AttrsIncludingInner`. Meaning unchanged: same nodes, same order, same `None`s.

needs: `frontend-tree: hir-def item_tree/attrs.rs: onto views (2)` (this file); in
`v2/requests/db-expand.md`, `frontend-tree: hir-expand lib.rs: onto views (2)`
(`ExpandTo::of_call_site_view`) and `frontend-tree: hir-expand name.rs + mod_path.rs: onto views
(4)` (`impl AsName for view::Name<'_>`/`view::NameRef<'_>`, `ModPath::from_src_view`). Callers
moved by their parts:
`item_tree.rs` and `item_tree/items.rs` (headings (3) and (4) here), `src.rs`
(`lower_use_tree`, frontend-tree-move-def: `frontend-tree: hir-def src.rs: onto views (1)` in
`v2/requests/db-expand.md`), `visibility.rs`/`signatures.rs`
(`visibility_from_ast`, frontend-tree-move-def-sigs).

Hunk 1, imports. Old:

```rust
use syntax::{
    AstNode, SourceFile, TextRange,
    ast::{self, HasModuleItem, HasName, StructKind},
};
```

New:

```rust
use syntax::{
    TextRange,
    ast as tag,
    view::{self as ast, AstNode, HasModuleItem, HasName, HasVisibility, StructKind},
};
```

Hunk 2, the builder's ast ids column (a kind tag). Old:

```rust
    ast_ids: Vec<FileAstId<ast::Item>>,
```

New:

```rust
    ast_ids: Vec<FileAstId<tag::Item>>,
```

Hunk 3. Old:

```rust
    fn item(&mut self, kind: ItemKind, ast_id: FileAstId<ast::Item>, data: u32) -> ModItemId {
```

New:

```rust
    fn item(&mut self, kind: ItemKind, ast_id: FileAstId<tag::Item>, data: u32) -> ModItemId {
```

Hunk 4, attributes: one body taking `ast::AnyHasAttrs<'t>`, a node and nothing else. Old:

```rust
    /// Lowers a node's attributes (`attrs`, as [`ast::attrs_including_inner`] gives them) into
    /// the tree's, and returns where they start and the `cfg` that disables the node, if one
    /// does. One instance for every kind of node.
    fn lower_attrs_of(&mut self, attrs: ast::AttrsIncludingInner) -> (usize, Option<CfgExpr>) {
        let span_map = self.span_map();
        let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
        let start = self.tree.attrs.len();
        let cfg = lower_attrs_into(
            db,
            attrs,
            &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
            span_map,
            &mut self.tree.attrs,
        );
        (start, cfg)
    }

    #[inline]
    fn lower_attrs(&mut self, owner: ModItemId, node: &impl ast::HasAttrs) {
        let (start, cfg) = self.lower_attrs_of(ast::attrs_including_inner(node));
        self.tree.add_attrs(owner, start, cfg);
    }

    /// Whether a `cfg` among `node`'s attributes is false; the attributes are not kept.
    #[inline]
    fn cfg_disabled(&mut self, node: &impl ast::HasAttrs) -> bool {
        let (start, cfg) = self.lower_attrs_of(ast::attrs_including_inner(node));
        self.tree.attrs.truncate(start);
        cfg.is_some()
    }
```

New:

```rust
    /// Lowers `node`'s attributes into the tree's, and returns where they start and the `cfg`
    /// that disables the node, if one does. One instance for every kind of node.
    fn lower_attrs_of(&mut self, node: ast::AnyHasAttrs<'_>) -> (usize, Option<CfgExpr>) {
        let span_map = self.span_map();
        let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
        let start = self.tree.attrs.len();
        let cfg = lower_attrs_into(
            db,
            node,
            &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
            span_map,
            &mut self.tree.attrs,
        );
        (start, cfg)
    }

    #[inline]
    fn lower_attrs<'t>(&mut self, owner: ModItemId, node: impl ast::HasAttrs<'t>) {
        let (start, cfg) = self.lower_attrs_of(ast::AnyHasAttrs::new(node));
        self.tree.add_attrs(owner, start, cfg);
    }

    /// Whether a `cfg` among `node`'s attributes is false; the attributes are not kept.
    #[inline]
    fn cfg_disabled<'t>(&mut self, node: impl ast::HasAttrs<'t>) -> bool {
        let (start, cfg) = self.lower_attrs_of(ast::AnyHasAttrs::new(node));
        self.tree.attrs.truncate(start);
        cfg.is_some()
    }
```

Hunk 5, the top level's attributes. Old:

```rust
    pub(super) fn lower_top_attrs(
        &mut self,
        file: &SourceFile,
        extra: Option<(SourceFile, span::EditionedFileId)>,
    ) {
        let start = self.tree.attrs.len();
        let mut cfg = None;
        if let Some((crate_attrs, file_id)) = extra {
            let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
            cfg = lower_attrs_into(
                db,
                ast::attrs_including_inner(&crate_attrs),
                &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
                FakeSpanMap { file_id },
                &mut self.tree.attrs,
            );
        }
        if cfg.is_none() {
            cfg = self.lower_attrs_of(ast::attrs_including_inner(file)).1;
        }
```

New:

```rust
    pub(super) fn lower_top_attrs(
        &mut self,
        file: ast::SourceFile<'_>,
        extra: Option<(ast::SourceFile<'_>, span::EditionedFileId)>,
    ) {
        let start = self.tree.attrs.len();
        let mut cfg = None;
        if let Some((crate_attrs, file_id)) = extra {
            let (db, krate, cfg_options) = (self.db, self.krate, &self.cfg_options);
            cfg = lower_attrs_into(
                db,
                ast::AnyHasAttrs::new(crate_attrs),
                &|| *cfg_options.get_or_init(|| krate.cfg_options(db)),
                FakeSpanMap { file_id },
                &mut self.tree.attrs,
            );
        }
        if cfg.is_none() {
            cfg = self.lower_attrs_of(ast::AnyHasAttrs::new(file)).1;
        }
```

Hunk 6, the lists of items. Old:

```rust
    fn pend(&mut self, item: &ast::Item) {
        if let Some(id) = self.lower_mod_item(item) {
            self.tree.pending_items.push(id);
        }
    }

    /// Lowers `item` as a top-level item: [`Self::pend`], then sealed ([`Scoped`]).
    fn pend_top(&mut self, item: &ast::Item) {
        self.pend(item);
        self.tree.seal();
    }

    pub(super) fn lower_module_items(mut self, item_owner: &impl HasModuleItem) -> ItemTree {
        for item in item_owner.items() {
            self.pend_top(&item);
        }
        self.finish()
    }
```

New:

```rust
    fn pend(&mut self, item: ast::Item<'_>) {
        if let Some(id) = self.lower_mod_item(item) {
            self.tree.pending_items.push(id);
        }
    }

    /// Lowers `item` as a top-level item: [`Self::pend`], then sealed ([`Scoped`]).
    fn pend_top(&mut self, item: ast::Item<'_>) {
        self.pend(item);
        self.tree.seal();
    }

    pub(super) fn lower_module_items<'t>(
        mut self,
        item_owner: impl HasModuleItem<'t>,
    ) -> ItemTree {
        for item in item_owner.items() {
            self.pend_top(item);
        }
        self.finish()
    }
```

Hunk 7. Old:

```rust
    pub(super) fn lower_module_items_marked(
        mut self,
        item_owner: &impl HasModuleItem,
        marks: &mut Vec<Marks>,
    ) -> ItemTree {
        for item in item_owner.items() {
            marks.push(self.tree.marks());
            self.pend_top(&item);
        }
```

New:

```rust
    pub(super) fn lower_module_items_marked<'t>(
        mut self,
        item_owner: impl HasModuleItem<'t>,
        marks: &mut Vec<Marks>,
    ) -> ItemTree {
        for item in item_owner.items() {
            marks.push(self.tree.marks());
            self.pend_top(item);
        }
```

Hunk 8. Old:

```rust
    pub(super) fn lower_item(mut self, item: &ast::Item) -> ItemTree {
        self.pend_top(item);
        self.finish()
    }

    /// The tree of a source file's top level's attributes alone ([`Self::lower_top_attrs`]).
    pub(super) fn lower_top(
        mut self,
        file: &SourceFile,
        extra: Option<(SourceFile, span::EditionedFileId)>,
    ) -> ItemTree {
```

New:

```rust
    pub(super) fn lower_item(mut self, item: ast::Item<'_>) -> ItemTree {
        self.pend_top(item);
        self.finish()
    }

    /// The tree of a source file's top level's attributes alone ([`Self::lower_top_attrs`]).
    pub(super) fn lower_top(
        mut self,
        file: ast::SourceFile<'_>,
        extra: Option<(ast::SourceFile<'_>, span::EditionedFileId)>,
    ) -> ItemTree {
```

Hunk 9, a macro's statements. Old:

```rust
    pub(super) fn lower_macro_stmts(mut self, stmts: ast::MacroStmts) -> ItemTree {
```

New:

```rust
    pub(super) fn lower_macro_stmts(mut self, stmts: ast::MacroStmts<'_>) -> ItemTree {
```

Hunk 10. Old:

```rust
                _ => continue,
            };
            self.pend_top(&item);
        }

        if let Some(ast::Expr::MacroExpr(tail_macro)) = stmts.expr()
            && let Some(call) = tail_macro.macro_call()
        {
            cov_mark::hit!(macro_stmt_with_trailing_macro_expr);
            self.pend_top(&call.into());
        }
```

New:

```rust
                _ => continue,
            };
            self.pend_top(item);
        }

        if let Some(ast::Expr::MacroExpr(tail_macro)) = stmts.expr()
            && let Some(call) = tail_macro.macro_call()
        {
            cov_mark::hit!(macro_stmt_with_trailing_macro_expr);
            self.pend_top(call.into());
        }
```

Hunk 11, a block. Old:

```rust
    pub(super) fn lower_block(mut self, block: &ast::BlockExpr) -> ItemTree {
        self.lower_attrs(ModItemId::TOP_OWNER, block);
        for stmt in block.statements() {
            let id = match stmt {
                ast::Stmt::Item(item) => self.lower_mod_item(&item),
                // Macro calls can be both items and expressions. The syntax library always treats
                // them as expressions here, so we undo that.
                ast::Stmt::ExprStmt(es) => match es.expr() {
                    Some(ast::Expr::MacroExpr(expr)) => self.lower_macro_stmt(&expr),
```

New:

```rust
    pub(super) fn lower_block(mut self, block: ast::BlockExpr<'_>) -> ItemTree {
        self.lower_attrs(ModItemId::TOP_OWNER, block);
        for stmt in block.statements() {
            let id = match stmt {
                ast::Stmt::Item(item) => self.lower_mod_item(item),
                // Macro calls can be both items and expressions. The syntax library always treats
                // them as expressions here, so we undo that.
                ast::Stmt::ExprStmt(es) => match es.expr() {
                    Some(ast::Expr::MacroExpr(expr)) => self.lower_macro_stmt(expr),
```

Hunk 12. Old:

```rust
        if let Some(ast::Expr::MacroExpr(expr)) = block.tail_expr()
            && let Some(mod_item) = self.lower_macro_stmt(&expr)
```

New:

```rust
        if let Some(ast::Expr::MacroExpr(expr)) = block.tail_expr()
            && let Some(mod_item) = self.lower_macro_stmt(expr)
```

Hunk 13. Old:

```rust
    fn lower_macro_stmt(&mut self, expr: &ast::MacroExpr) -> Option<ModItemId> {
        let call = expr.macro_call()?;
        // clone: a cursor, which allocates nothing; only an `Expr` has the attributes
        if self.cfg_disabled(&ast::Expr::MacroExpr(expr.clone())) {
            return None;
        }
        self.lower_mod_item(&call.into())
    }

    fn lower_mod_item(&mut self, item: &ast::Item) -> Option<ModItemId> {
```

New:

```rust
    fn lower_macro_stmt(&mut self, expr: ast::MacroExpr<'_>) -> Option<ModItemId> {
        let call = expr.macro_call()?;
        // Only an `Expr` has the attributes.
        if self.cfg_disabled(ast::Expr::MacroExpr(expr)) {
            return None;
        }
        self.lower_mod_item(call.into())
    }

    fn lower_mod_item(&mut self, item: ast::Item<'_>) -> Option<ModItemId> {
```

Hunk 14: none. `self.lower_attrs(mod_item, item);` after the `match` stays as written (`item`
is now a view by value).

Hunk 15, structs. Old:

```rust
    fn lower_struct(&mut self, strukt: &ast::Struct) -> Option<ModItemId> {
        let visibility = self.lower_visibility_from_ast(strukt);
        // clone: allocates nothing, a `ModPath` is an interned slice
        let visibility_id = self.tree.visibility(visibility.clone());
        let name = strukt.name()?.as_name();
        let ast_id = self.ids.ast_id(strukt);
```

New:

```rust
    fn lower_struct(&mut self, strukt: ast::Struct<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility_from_ast(strukt);
        // clone: allocates nothing, a `ModPath` is an interned slice
        let visibility_id = self.tree.visibility(visibility.clone());
        let name = strukt.name()?.as_name();
        let ast_id = self.ids.ast_id_of_view::<tag::Struct>(strukt);
```

Hunk 16. Old:

```rust
        fields: ast::TupleFieldList,
```

New:

```rust
        fields: ast::TupleFieldList<'_>,
```

Hunk 17. Old:

```rust
            let field_vis = self.lower_visibility_from_ast(&field);
```

New:

```rust
            let field_vis = self.lower_visibility_from_ast(field);
```

Hunk 18, unions and enums. Old:

```rust
    fn lower_union(&mut self, union: &ast::Union) -> Option<ModItemId> {
        let visibility = self.lower_visibility(union);
        let name = union.name()?.as_name();
        let ast_id = self.ids.ast_id(union);
```

New:

```rust
    fn lower_union(&mut self, union: ast::Union<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(union);
        let name = union.name()?.as_name();
        let ast_id = self.ids.ast_id_of_view::<tag::Union>(union);
```

Hunk 19. Old:

```rust
    fn lower_enum(&mut self, enum_: &ast::Enum) -> Option<ModItemId> {
        let visibility = self.lower_visibility(enum_);
        let name = enum_.name()?.as_name();
        let ast_id = self.ids.ast_id(enum_);
```

New:

```rust
    fn lower_enum(&mut self, enum_: ast::Enum<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(enum_);
        let name = enum_.name()?.as_name();
        let ast_id = self.ids.ast_id_of_view::<tag::Enum>(enum_);
```

Hunk 20, functions. Old:

```rust
    fn lower_function(&mut self, func: &ast::Fn) -> Option<ModItemId> {
        let visibility = self.lower_visibility(func);
        let name = func.name()?.as_name();

        let ast_id = self.ids.ast_id(func);
```

New:

```rust
    fn lower_function(&mut self, func: ast::Fn<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(func);
        let name = func.name()?.as_name();

        let ast_id = self.ids.ast_id_of_view::<tag::Fn>(func);
```

Hunk 21, type aliases. Old:

```rust
    fn lower_type_alias(&mut self, type_alias: &ast::TypeAlias) -> Option<ModItemId> {
        let name = type_alias.name()?.as_name();
        let visibility = self.lower_visibility(type_alias);
        let ast_id = self.ids.ast_id(type_alias);
```

New:

```rust
    fn lower_type_alias(&mut self, type_alias: ast::TypeAlias<'_>) -> Option<ModItemId> {
        let name = type_alias.name()?.as_name();
        let visibility = self.lower_visibility(type_alias);
        let ast_id = self.ids.ast_id_of_view::<tag::TypeAlias>(type_alias);
```

Hunk 22, statics. Old:

```rust
    fn lower_static(&mut self, static_: &ast::Static) -> Option<ModItemId> {
        let name = static_.name()?.as_name();
        let visibility = self.lower_visibility(static_);
        let ast_id = self.ids.ast_id(static_);
```

New:

```rust
    fn lower_static(&mut self, static_: ast::Static<'_>) -> Option<ModItemId> {
        let name = static_.name()?.as_name();
        let visibility = self.lower_visibility(static_);
        let ast_id = self.ids.ast_id_of_view::<tag::Static>(static_);
```

Hunk 23, consts. Old:

```rust
    fn lower_const(&mut self, konst: &ast::Const) -> ModItemId {
        let name = match konst.name() {
            Some(name) => self.tree.name(name.as_name()),
            None => NameId::NONE,
        };
        let visibility = self.lower_visibility(konst);
        let ast_id = self.ids.ast_id(konst);
```

New:

```rust
    fn lower_const(&mut self, konst: ast::Const<'_>) -> ModItemId {
        let name = match konst.name() {
            Some(name) => self.tree.name(name.as_name()),
            None => NameId::NONE,
        };
        let visibility = self.lower_visibility(konst);
        let ast_id = self.ids.ast_id_of_view::<tag::Const>(konst);
```

Hunk 24, modules. Old:

```rust
    fn lower_module(&mut self, module: &ast::Module) -> Option<ModItemId> {
```

New:

```rust
    fn lower_module(&mut self, module: ast::Module<'_>) -> Option<ModItemId> {
```

Hunk 25. Old:

```rust
                Some(list) => {
                    for item in list.items() {
                        self.pend(&item);
                    }
                }
```

New:

```rust
                Some(list) => {
                    for item in list.items() {
                        self.pend(item);
                    }
                }
```

Hunk 26. Old:

```rust
        let ast_id = self.ids.ast_id(module);
```

New:

```rust
        let ast_id = self.ids.ast_id_of_view::<tag::Module>(module);
```

Hunk 27, traits. Old:

```rust
    fn lower_trait(&mut self, trait_def: &ast::Trait) -> Option<ModItemId> {
        let name = trait_def.name()?.as_name();
        let visibility = self.lower_visibility(trait_def);
        let ast_id = self.ids.ast_id(trait_def);
```

New:

```rust
    fn lower_trait(&mut self, trait_def: ast::Trait<'_>) -> Option<ModItemId> {
        let name = trait_def.name()?.as_name();
        let visibility = self.lower_visibility(trait_def);
        let ast_id = self.ids.ast_id_of_view::<tag::Trait>(trait_def);
```

Hunk 28, impls. Old:

```rust
    fn lower_impl(&mut self, impl_def: &ast::Impl) -> ModItemId {
        let ast_id = self.ids.ast_id(impl_def);
```

New:

```rust
    fn lower_impl(&mut self, impl_def: ast::Impl<'_>) -> ModItemId {
        let ast_id = self.ids.ast_id_of_view::<tag::Impl>(impl_def);
```

Hunk 29, uses. Old:

```rust
    fn lower_use(&mut self, use_item: &ast::Use) -> Option<ModItemId> {
        let visibility = self.lower_visibility(use_item);
        let ast_id = self.ids.ast_id(use_item);
```

New:

```rust
    fn lower_use(&mut self, use_item: ast::Use<'_>) -> Option<ModItemId> {
        let visibility = self.lower_visibility(use_item);
        let ast_id = self.ids.ast_id_of_view::<tag::Use>(use_item);
```

Hunk 30, extern crates. Old:

```rust
    fn lower_extern_crate(&mut self, extern_crate: &ast::ExternCrate) -> Option<ModItemId> {
```

New:

```rust
    fn lower_extern_crate(&mut self, extern_crate: ast::ExternCrate<'_>) -> Option<ModItemId> {
```

Hunk 31. Old:

```rust
        let visibility = self.lower_visibility(extern_crate);
        let ast_id = self.ids.ast_id(extern_crate);
```

New:

```rust
        let visibility = self.lower_visibility(extern_crate);
        let ast_id = self.ids.ast_id_of_view::<tag::ExternCrate>(extern_crate);
```

Hunk 32, macro calls: hir-expand's view twins. Old:

```rust
    fn lower_macro_call(&mut self, m: &ast::MacroCall) -> Option<ModItemId> {
        let span_map = self.span_map();
        let path = m.path()?;
        let range = path.syntax().text_range();
        let path =
            ModPath::from_src(self.db, path, &mut |range| span_map.span_for_range(range).ctx)?;
        let expand_to = match hir_expand::ExpandTo::of_call_site(m) {
```

New:

```rust
    fn lower_macro_call(&mut self, m: ast::MacroCall<'_>) -> Option<ModItemId> {
        let span_map = self.span_map();
        let path = m.path()?;
        let range = path.syntax().text_range();
        let path = ModPath::from_src_view(self.db, path, &mut |range| {
            span_map.span_for_range(range).ctx
        })?;
        let expand_to = match hir_expand::ExpandTo::of_call_site_view(m) {
```

Hunk 33. Old:

```rust
        let ast_id = self.ids.ast_id(m);
        let res = MacroCall {
```

New:

```rust
        let ast_id = self.ids.ast_id_of_view::<tag::MacroCall>(m);
        let res = MacroCall {
```

Hunk 34, `macro_rules!`. Old:

```rust
    fn lower_macro_rules(&mut self, m: &ast::MacroRules) -> Option<ModItemId> {
        let name = m.name()?;
        let ast_id = self.ids.ast_id(m);
```

New:

```rust
    fn lower_macro_rules(&mut self, m: ast::MacroRules<'_>) -> Option<ModItemId> {
        let name = m.name()?;
        let ast_id = self.ids.ast_id_of_view::<tag::MacroRules>(m);
```

Hunk 35, `macro`s. Old:

```rust
    fn lower_macro_def(&mut self, m: &ast::MacroDef) -> Option<ModItemId> {
        let name = m.name()?;

        let ast_id = self.ids.ast_id(m);
```

New:

```rust
    fn lower_macro_def(&mut self, m: ast::MacroDef<'_>) -> Option<ModItemId> {
        let name = m.name()?;

        let ast_id = self.ids.ast_id_of_view::<tag::MacroDef>(m);
```

Hunk 36, extern blocks. Old:

```rust
    fn lower_extern_block(&mut self, block: &ast::ExternBlock) -> ModItemId {
        let ast_id = self.ids.ast_id(block);
```

New:

```rust
    fn lower_extern_block(&mut self, block: ast::ExternBlock<'_>) -> ModItemId {
        let ast_id = self.ids.ast_id_of_view::<tag::ExternBlock>(block);
```

Hunk 37. Old:

```rust
            let mod_item = match &item {
                ast::ExternItem::Fn(ast) => self.lower_function(ast),
                ast::ExternItem::Static(ast) => self.lower_static(ast),
                ast::ExternItem::TypeAlias(ty) => self.lower_type_alias(ty),
                ast::ExternItem::MacroCall(call) => self.lower_macro_call(call),
            };
            let Some(mod_item) = mod_item else { continue };
            self.lower_attrs(mod_item, &item);
```

New:

```rust
            let mod_item = match item {
                ast::ExternItem::Fn(ast) => self.lower_function(ast),
                ast::ExternItem::Static(ast) => self.lower_static(ast),
                ast::ExternItem::TypeAlias(ty) => self.lower_type_alias(ty),
                ast::ExternItem::MacroCall(call) => self.lower_macro_call(call),
            };
            let Some(mod_item) = mod_item else { continue };
            self.lower_attrs(mod_item, item);
```

Hunk 38, visibilities. Old:

```rust
    fn lower_visibility(&mut self, item: &impl ast::HasVisibility) -> RawVisibilityId {
        let vis = self.lower_visibility_from_ast(item);
        self.tree.visibility(vis)
    }

    fn lower_visibility_from_ast(&self, item: &impl ast::HasVisibility) -> RawVisibility {
```

New:

```rust
    fn lower_visibility<'t>(&mut self, item: impl HasVisibility<'t>) -> RawVisibilityId {
        let vis = self.lower_visibility_from_ast(item);
        self.tree.visibility(vis)
    }

    fn lower_visibility_from_ast<'t>(&self, item: impl HasVisibility<'t>) -> RawVisibility {
```

Hunk 39, `use` trees: the source map's arena holds views, no clone. Old:

```rust
struct UseTreeLowering<'a> {
    db: &'a Db,
    /// The lowered trees' syntax, in the order [`Use::expand`] numbers them: a tree before its
    /// children. Only the source map ([`lower_use_tree`]) wants it.
    mapping: Option<&'a mut Arena<ast::UseTree>>,
    tree: &'a mut TreeBuilder,
}

impl UseTreeLowering<'_> {
    fn map(&mut self, tree: &ast::UseTree) {
        if let Some(mapping) = &mut self.mapping {
            mapping.alloc(tree.clone()); // clone: a cursor, for the source map alone
        }
    }

    /// Lowers `tree` into the tree's `use` trees, its children before it.
    fn lower_use_tree(
        &mut self,
        tree: ast::UseTree,
```

New:

```rust
struct UseTreeLowering<'a, 't> {
    db: &'a Db,
    /// The lowered trees' syntax, in the order [`Use::expand`] numbers them: a tree before its
    /// children. Only the source map ([`lower_use_tree`]) wants it.
    mapping: Option<&'a mut Arena<ast::UseTree<'t>>>,
    tree: &'a mut TreeBuilder,
}

impl<'t> UseTreeLowering<'_, 't> {
    fn map(&mut self, tree: ast::UseTree<'t>) {
        if let Some(mapping) = &mut self.mapping {
            mapping.alloc(tree);
        }
    }

    /// Lowers `tree` into the tree's `use` trees, its children before it.
    fn lower_use_tree(
        &mut self,
        tree: ast::UseTree<'t>,
```

Hunk 40. Old:

```rust
                Some(path) => {
                    match ModPath::from_src(self.db, path, span_for_range) {
```

New:

```rust
                Some(path) => {
                    match ModPath::from_src_view(self.db, path, span_for_range) {
```

Hunk 41. Old:

```rust
            self.map(&tree);
            let mark = self.tree.pending_use_trees.len();
```

New:

```rust
            self.map(tree);
            let mark = self.tree.pending_use_trees.len();
```

Hunk 42. Old:

```rust
                Some(path) => Some(ModPath::from_src(self.db, path, span_for_range)?),
```

New:

```rust
                Some(path) => Some(ModPath::from_src_view(self.db, path, span_for_range)?),
```

Hunk 43. Old:

```rust
                        cov_mark::hit!(glob_enum_group);
                    }
                    self.map(&tree);
```

New:

```rust
                        cov_mark::hit!(glob_enum_group);
                    }
                    self.map(tree);
```

Hunk 44. Old:

```rust
                (Some(path), alias, false) => {
                    self.map(&tree);
```

New:

```rust
                (Some(path), alias, false) => {
                    self.map(tree);
```

Hunk 45, the source map's entry point (caller `src.rs`, frontend-tree-move-def). Old:

```rust
pub(crate) fn lower_use_tree(
    db: &Db,
    tree: ast::UseTree,
    span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
) -> Option<Arena<ast::UseTree>> {
```

New:

```rust
pub(crate) fn lower_use_tree<'t>(
    db: &Db,
    tree: ast::UseTree<'t>,
    span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
) -> Option<Arena<ast::UseTree<'t>>> {
```

Hunk 46, visibilities (callers `visibility.rs`, `signatures.rs`, frontend-tree-move-def-sigs).
Old:

```rust
pub(crate) fn visibility_from_ast(
    db: &Db,
    node: Option<ast::Visibility>,
    span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
) -> RawVisibility {
    let Some(node) = node else { return private_vis() };
    let path = match node.kind() {
        ast::VisibilityKind::In(path) => {
            let path = ModPath::from_src(db, path, span_for_range);
```

New:

```rust
pub(crate) fn visibility_from_ast(
    db: &Db,
    node: Option<ast::Visibility<'_>>,
    span_for_range: &mut (impl FnMut(::tt::TextRange) -> SyntaxContext + ?Sized),
) -> RawVisibility {
    let Some(node) = node else { return private_vis() };
    let path = match node.kind() {
        ast::VisibilityKind::In(path) => {
            let path = ModPath::from_src_view(db, path, span_for_range);
```

> forwarded to frontend-deps-items (it holds hir-def's `item_tree*`; its child frontend-deps-items-views holds them); told by tmux (requests, pass 25).

> frontend-deps-items-views (2026-10-29 21:5x): applied, hunks 1 to 46 as written; every old
> text matched, no port. Complete once hir-expand's twins land (`ExpandTo::of_call_site_view`,
> `ModPath::from_src_view`, `AsName` for `view::Name`/`view::NameRef`, all open in
> `v2/requests/db-expand.md`); callers `src.rs`, `visibility.rs`, `signatures.rs` move with
> their parts.

## frontend-tree: hir-def item_tree/attrs.rs: onto views (2)

From `frontend-tree-move-def-items`. Attribute lowering reads views: one body for every owner
kind through the concrete `ast::AnyHasAttrs<'t>` (a node, `Copy`), into hir-expand's
`collect_item_tree_attrs_view`; an attribute's token tree through `syntax_bridge::call_input`,
the same flat-tree pass `syntax_node_to_token_tree` takes on a flat node. The same attributes,
spans and `cfg`s, in the same order.

needs: in `v2/requests/db-expand.md`, `frontend-tree: hir-expand attrs.rs + cfg_process.rs: onto
views (3)` (`collect_item_tree_attrs_view`) and `frontend-tree: hir-expand name.rs + mod_path.rs:
onto views (4)` (`ModPath::from_src_view`). Caller
moved by its part: `nameres/assoc.rs` (`AttrsOrCfg::lower`, frontend-tree-move-def-nameres: it
passes the view `item` by value, or `ast::AnyHasAttrs::from(item)`).

Hunk 1, imports. Old:

```rust
use hir_expand::{
    attrs::{Attr, AttrId, AttrInput, collect_item_tree_attrs},
    mod_path::ModPath,
    name::Name,
};
use intern::{Symbol, sym};
use syntax::{AstNode, ast};
```

New:

```rust
use hir_expand::{
    attrs::{Attr, AttrId, AttrInput, collect_item_tree_attrs_view},
    mod_path::ModPath,
    name::Name,
};
use intern::{Symbol, sym};
use syntax::view::{self as ast, AstNode};
```

Hunk 2. Old:

```rust
    pub(crate) fn lower<'a, S>(
        db: &Db,
        owner: &impl ast::HasAttrs,
        cfg_options: &impl Fn() -> &'a CfgOptions,
        span_map: S,
    ) -> AttrsOrCfg
    where
        S: syntax_bridge::SpanMapper + Copy,
    {
        // alloc: the owner's attributes, the value; none for an owner without any
        let mut attrs = Vec::new();
        let cfg = lower_attrs_into(
            db,
            ast::attrs_including_inner(owner),
```

New:

```rust
    pub(crate) fn lower<'t, 'a, S>(
        db: &Db,
        owner: impl ast::HasAttrs<'t>,
        cfg_options: &impl Fn() -> &'a CfgOptions,
        span_map: S,
    ) -> AttrsOrCfg
    where
        S: syntax_bridge::SpanMapper + Copy,
    {
        // alloc: the owner's attributes, the value; none for an owner without any
        let mut attrs = Vec::new();
        let cfg = lower_attrs_into(
            db,
            ast::AnyHasAttrs::new(owner),
```

Hunk 3. Old:

```rust
/// Appends an owner's attributes (`owner_attrs`, as [`ast::attrs_including_inner`] gives them)
/// to `attrs`, and returns the `cfg` that disables it, if one does: the attributes appended are
/// then those before that `cfg` (what crate-level attributes need), so lowering a second owner
/// after a disabled one is never needed. One instance for every kind of owner.
pub(super) fn lower_attrs_into<'a, S>(
    db: &Db,
    owner_attrs: ast::AttrsIncludingInner,
    cfg_options: &impl Fn() -> &'a CfgOptions,
    span_map: S,
    attrs: &mut Vec<Attr>,
) -> Option<CfgExpr>
where
    S: syntax_bridge::SpanMapper + Copy,
{
    let result = collect_item_tree_attrs::<Infallible>(owner_attrs, cfg_options, |meta, _| {
        // NOTE: We cannot early return from this function, *every* attribute must be pushed, otherwise we'll mess the `AttrId`
        // tracking.
        let path = meta.path();
        let path_range = path
            .as_ref()
            .map(|path| path.syntax().text_range())
            .unwrap_or_else(|| meta.syntax().text_range());
        let (span, input) = match &meta {
```

New:

```rust
/// Appends `owner`'s attributes to `attrs`, and returns the `cfg` that disables it, if one
/// does: the attributes appended are then those before that `cfg` (what crate-level attributes
/// need), so lowering a second owner after a disabled one is never needed. One instance for
/// every kind of owner.
pub(super) fn lower_attrs_into<'t, 'a, S>(
    db: &Db,
    owner: ast::AnyHasAttrs<'t>,
    cfg_options: &impl Fn() -> &'a CfgOptions,
    span_map: S,
    attrs: &mut Vec<Attr>,
) -> Option<CfgExpr>
where
    S: syntax_bridge::SpanMapper + Copy,
{
    let result = collect_item_tree_attrs_view::<Infallible>(owner, cfg_options, |meta, _| {
        // NOTE: We cannot early return from this function, *every* attribute must be pushed, otherwise we'll mess the `AttrId`
        // tracking.
        let path = meta.path();
        let path_range =
            path.map_or_else(|| meta.syntax().text_range(), |path| path.syntax().text_range());
        let (span, input) = match meta {
```

Hunk 4, an attribute's token tree from the view. Old:

```rust
                let token_tree = meta.token_tree();
                let tt = syntax_bridge::syntax_node_to_token_tree(
                    token_tree.as_ref().map_or(meta.syntax(), |it| it.syntax()),
                    span_map,
```

New:

```rust
                let token_tree = meta.token_tree();
                let tt = syntax_bridge::call_input(
                    token_tree.map_or(meta.syntax(), |it| it.syntax()),
                    span_map,
```

Hunk 5. Old:

```rust
            ModPath::from_src(db, path, &mut |range| span_map.span_for(range).ctx)
```

New:

```rust
            ModPath::from_src_view(db, path, &mut |range| span_map.span_for(range).ctx)
```

> forwarded to frontend-deps-items (it holds hir-def's `item_tree*`; its child frontend-deps-items-views holds them); told by tmux (requests, pass 25).

> frontend-deps-items-views (2026-10-29 21:5x): applied, hunks 1 to 5 as written, no port.
> Open: `collect_item_tree_attrs_view` and `ModPath::from_src_view` (hir-expand,
> `v2/requests/db-expand.md`); the caller `nameres/assoc.rs:196` (`AttrsOrCfg::lower(.., &item,
> ..)`) still passes a handle until frontend-tree-move-def-nameres moves it.

## frontend-tree: hir-def item_tree.rs: onto views (3)

From `frontend-tree-move-def-items`. A file's and a block's tree are lowered from the views the
database keeps (`HirFileId::root`, `AstId::to_view`), with no handle made. `ast` stays the
handle kind tags of this file (`FileAstId<ast::Item>`, `Idx<ast::UseTree>`, the item table);
views are `view::X`.

needs: `frontend-tree: hir-def item_tree/lower.rs: onto views (1)` (this file);
`parse_extra_crate_attrs_view(db, krate) -> Option<view::SourceFile<'_>>` in hir-def `attrs.rs`
(frontend-tree-move-def-attrs: `frontend-tree: hir-def attrs.rs: onto views (1)` in
`v2/requests/db-expand.md`).

Hunk 1, imports. Old:

```rust
use syntax::{SyntaxKind, ast, match_ast};

use crate::{BlockId, attrs::parse_extra_crate_attrs};
```

New:

```rust
use syntax::{
    SyntaxKind, ast, match_ast,
    view::{self, AstNode as _},
};

use crate::{BlockId, attrs::parse_extra_crate_attrs_view};
```

Hunk 2, `lower_whole`. Old:

```rust
    let mut ctx = lower::Ctx::new(db, file_id, krate);
    let syntax = file_id.parse_or_expand(db);
    match_ast! {
        match syntax {
            ast::SourceFile(file) => {
                let root_file_id = krate.root_file_id(db);
                let extra_top_attrs = (file_id == root_file_id)
                    .then(|| parse_extra_crate_attrs(db, krate))
                    .flatten()
                    .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
                ctx.lower_top_attrs(&file, extra_top_attrs);
                ctx.lower_module_items(&file)
            },
            ast::MacroItems(items) => {
                ctx.lower_module_items(&items)
            },
            ast::MacroStmts(stmts) => {
```

New:

```rust
    let mut ctx = lower::Ctx::new(db, file_id, krate);
    let syntax = file_id.root(db);
    match_ast! {
        match syntax {
            view::SourceFile(file) => {
                let root_file_id = krate.root_file_id(db);
                let extra_top_attrs = (file_id == root_file_id)
                    .then(|| parse_extra_crate_attrs_view(db, krate))
                    .flatten()
                    .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
                ctx.lower_top_attrs(file, extra_top_attrs);
                ctx.lower_module_items(file)
            },
            view::MacroItems(items) => {
                ctx.lower_module_items(items)
            },
            view::MacroStmts(stmts) => {
```

Hunk 3, `block_item_tree_query`. Old:

```rust
    let block = ast_id.to_node(db);

    let ctx = lower::Ctx::new(db, ast_id.file_id, krate);
    ctx.lower_block(&block)
```

New:

```rust
    let block = ast_id.to_view(db);

    let ctx = lower::Ctx::new(db, ast_id.file_id, krate);
    ctx.lower_block(block)
```

> forwarded to frontend-deps-items (it holds hir-def's `item_tree*`; its child frontend-deps-items-views holds them); told by tmux (requests, pass 25).

> frontend-deps-items-views (2026-10-29 21:5x): applied, hunks 1 to 3 as written, no port.
> Open: `parse_extra_crate_attrs_view` (hir-def `attrs.rs`, frontend-tree-move-def-attrs).

> frontend-tree-move-def (2026-10-29 22:08): answered. `parse_extra_crate_attrs_view(db, krate)
> -> Option<ast::SourceFile<'_>>` (a view) is in the tree at `crates/hir-def/src/attrs.rs:811`,
> filed in `v2/requests/db-expand.md` under `## frontend-tree: hir-def attrs.rs: onto views (1)`.

## frontend-tree: hir-def item_tree/items.rs: onto views (4)

From `frontend-tree-move-def-items`. An edit's keys are made from the views of the file's parse
(`EditionedFileId::parse(db).view()`), with no handle and no clone. `hash_item` writes the same
bytes in the same order as on handles: kinds as `u16`, each token's text, `FlatTree`'s subtree
hashes at `node.flat()`, `is_lazy` with the same meaning; where it compared a child's number in
its flat tree with a body's, it compares the views (`Node: Eq` is file, part and number), the
same test. Kind tags as `tag::Item` (`use syntax::ast as tag;`).

needs: `frontend-tree: hir-def item_tree/lower.rs: onto views (1)` (this file);
`parse_extra_crate_attrs_view(db, krate) -> Option<view::SourceFile<'_>>` (hir-def `attrs.rs`,
frontend-tree-move-def-attrs: `frontend-tree: hir-def attrs.rs: onto views (1)` in
`v2/requests/db-expand.md`). If your
pending change to `item_keys` (the `AstIdMap::entries` comparison) lands first, hunks 1 to 12
still apply: they touch only the lines quoted.

Hunk 1, imports. Old:

```rust
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, Text, TextRange, TextSize,
    ast::{self, HasModuleItem},
};

use crate::{
    attrs::parse_extra_crate_attrs,
```

New:

```rust
use syntax::{
    NodeOrToken, SyntaxKind, Text, TextRange, TextSize,
    ast as tag,
    view::{self as ast, AstNode, HasModuleItem},
};

use crate::{
    attrs::parse_extra_crate_attrs_view,
```

Hunk 2. Old:

```rust
fn text_of(source_file: &ast::SourceFile) -> &Text {
    source_file.syntax().flat().expect("a source file's flat tree").0.source()
}
```

New:

```rust
fn text_of<'t>(source_file: ast::SourceFile<'t>) -> &'t Text {
    source_file.syntax().file().source()
}
```

Hunk 3, `reuse`. Old:

```rust
    let source_file = file.parse(db).tree();
    let mut scratch = SCRATCH.take();
    scratch.keys.clear();
    item_keys(db, file, &source_file, &mut scratch, Some(&keyed));
    keyed.text = text_of(&source_file).clone(); // clone: one count of the file's text
```

New:

```rust
    let source_file = file.parse(db).view();
    let mut scratch = SCRATCH.take();
    scratch.keys.clear();
    item_keys(db, file, source_file, &mut scratch, Some(&keyed));
    keyed.text = text_of(source_file).clone(); // clone: one count of the file's text
```

Hunk 4. Old:

```rust
        .map(|item| lower::Ctx::new(db, file.into(), krate).lower_item(&item))
```

New:

```rust
        .map(|item| lower::Ctx::new(db, file.into(), krate).lower_item(item))
```

Hunk 5, `lower_keyed`. Old:

```rust
    let source_file = file.parse(db).tree();
    let mut scratch = SCRATCH.take();
    if let Some(last) = last {
```

New:

```rust
    let source_file = file.parse(db).view();
    let mut scratch = SCRATCH.take();
    if let Some(last) = last {
```

Hunk 6. Old:

```rust
    item_keys(db, file, &source_file, &mut scratch, None);
```

New:

```rust
    item_keys(db, file, source_file, &mut scratch, None);
```

Hunk 7. Old:

```rust
        .then(|| parse_extra_crate_attrs(db, krate))
        .flatten()
        .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
    let mut ctx = lower::Ctx::new(db, file.into(), krate);
    ctx.lower_top_attrs(&source_file, extra);
    let tree = ctx.lower_module_items_marked(&source_file, &mut scratch.marks);
```

New:

```rust
        .then(|| parse_extra_crate_attrs_view(db, krate))
        .flatten()
        .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
    let mut ctx = lower::Ctx::new(db, file.into(), krate);
    ctx.lower_top_attrs(source_file, extra);
    let tree = ctx.lower_module_items_marked(source_file, &mut scratch.marks);
```

Hunk 8. Old:

```rust
        text: text_of(&source_file).clone(), // clone: one count of the file's text
```

New:

```rust
        text: text_of(source_file).clone(), // clone: one count of the file's text
```

Hunk 9, `top_attrs`. Old:

```rust
    let source_file = file.parse(db).tree();
    let root_file_id = krate.root_file_id(db);
    let extra = (file == root_file_id)
        .then(|| parse_extra_crate_attrs(db, krate))
        .flatten()
        .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
    lower::Ctx::new(db, HirFileId::from(file), krate).lower_top(&source_file, extra)
```

New:

```rust
    let source_file = file.parse(db).view();
    let root_file_id = krate.root_file_id(db);
    let extra = (file == root_file_id)
        .then(|| parse_extra_crate_attrs_view(db, krate))
        .flatten()
        .map(|crate_attrs| (crate_attrs, root_file_id.span_file_id(db)));
    lower::Ctx::new(db, HirFileId::from(file), krate).lower_top(source_file, extra)
```

Hunk 10, `item_alone` (tests). Old:

```rust
    krate: Crate,
    item: &ast::Item,
) -> ItemTree {
    lower::Ctx::new(db, file.into(), krate).lower_item(item)
```

New:

```rust
    krate: Crate,
    item: ast::Item<'_>,
) -> ItemTree {
    lower::Ctx::new(db, file.into(), krate).lower_item(item)
```

Hunk 11, `item_keys` (its body's `item.syntax()`, `text_of(source_file)` and
`hash_item(item.syntax(), ..)` stay as written: views by value). Old:

```rust
    source_file: &ast::SourceFile,
    scratch: &mut Scratch,
```

New:

```rust
    source_file: ast::SourceFile<'_>,
    scratch: &mut Scratch,
```

Hunk 12. Old:

```rust
        item_ids(&item, ast_id_map, range.start(), &mut keys.ids);
```

New:

```rust
        item_ids(item, ast_id_map, range.start(), &mut keys.ids);
```

Hunk 13, `item_ids` and `push_id`. Old:

```rust
fn item_ids(item: &ast::Item, map: &AstIdMap, start: TextSize, out: &mut Vec<[u32; 3]>) {
    push_id(out, item.syntax(), start, map.ast_id(item).erase());
    match item {
        ast::Item::Module(module) => {
            for child in module.item_list().into_iter().flat_map(|list| list.items()) {
                item_ids(&child, map, start, out);
            }
        }
        ast::Item::ExternBlock(block) => {
            for child in block.extern_item_list().into_iter().flat_map(|list| list.extern_items()) {
                let child = ast::Item::from(child);
                push_id(out, child.syntax(), start, map.ast_id(&child).erase());
            }
        }
        _ => {}
    }
}

fn push_id(out: &mut Vec<[u32; 3]>, node: &SyntaxNode, start: TextSize, id: ErasedFileAstId) {
```

New:

```rust
fn item_ids(item: ast::Item<'_>, map: &AstIdMap, start: TextSize, out: &mut Vec<[u32; 3]>) {
    push_id(out, item.syntax(), start, map.ast_id_of_view::<tag::Item>(item).erase());
    match item {
        ast::Item::Module(module) => {
            for child in module.item_list().into_iter().flat_map(|list| list.items()) {
                item_ids(child, map, start, out);
            }
        }
        ast::Item::ExternBlock(block) => {
            for child in block.extern_item_list().into_iter().flat_map(|list| list.extern_items()) {
                let child = ast::Item::from(child);
                let id = map.ast_id_of_view::<tag::Item>(child).erase();
                push_id(out, child.syntax(), start, id);
            }
        }
        _ => {}
    }
}

fn push_id(out: &mut Vec<[u32; 3]>, node: ast::Node<'_>, start: TextSize, id: ErasedFileAstId) {
```

Hunk 14, `hash_item`. Old:

```rust
fn hash_item(item: &SyntaxNode, state: &mut Feed<'_>) {
    // clone: a cursor, which allocates nothing; `cast` takes it by value
    let Some(ast) = ast::Item::cast(item.clone()) else { return hash_full(item, state) };
    let (inner, skipped, items) = match &ast {
```

New:

```rust
fn hash_item(item: ast::Node<'_>, state: &mut Feed<'_>) {
    let Some(ast) = ast::Item::cast(item) else { return hash_full(item, state) };
    let (inner, skipped, items) = match ast {
```

Hunk 15. Old:

```rust
    (NODE, item.kind() as u16).hash(state);
    for child in item.children_with_tokens() {
        let at = match &child {
            NodeOrToken::Node(node) => node.flat().map(|(_, at)| at),
            NodeOrToken::Token(_) => None,
        };
        match child {
            NodeOrToken::Token(token) => hash_token(&token, state),
            NodeOrToken::Node(node) if at == inner => hash_inner_attrs(&node, state),
            NodeOrToken::Node(_) if at == skipped => SKIPPED.hash(state),
            NodeOrToken::Node(node) if at == items => {
                (NODE, node.kind() as u16).hash(state);
                for child in node.children_with_tokens() {
                    match child {
                        NodeOrToken::Token(token) => hash_token(&token, state),
                        NodeOrToken::Node(node) => hash_item(&node, state),
                    }
                    END.hash(state);
                }
            }
            NodeOrToken::Node(node) => hash_full(&node, state),
        }
```

New:

```rust
    (NODE, item.kind() as u16).hash(state);
    for child in item.children_with_tokens() {
        match child {
            NodeOrToken::Token(token) => hash_token(token, state),
            NodeOrToken::Node(node) if Some(node) == inner => hash_inner_attrs(node, state),
            NodeOrToken::Node(node) if Some(node) == skipped => SKIPPED.hash(state),
            NodeOrToken::Node(node) if Some(node) == items => {
                (NODE, node.kind() as u16).hash(state);
                for child in node.children_with_tokens() {
                    match child {
                        NodeOrToken::Token(token) => hash_token(token, state),
                        NodeOrToken::Node(node) => hash_item(node, state),
                    }
                    END.hash(state);
                }
            }
            NodeOrToken::Node(node) => hash_full(node, state),
        }
```

Hunk 16, `hash_inner_attrs`. Old:

```rust
fn hash_inner_attrs(node: &SyntaxNode, state: &mut Feed<'_>) {
```

New:

```rust
fn hash_inner_attrs(node: ast::Node<'_>, state: &mut Feed<'_>) {
```

Hunk 17. Old:

```rust
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::STMT_LIST => {
                hash_inner_attrs(&node, state)
            }
            NodeOrToken::Node(node) if ast::AnyAttr::can_cast(node.kind()) => {
                let attr = ast::AnyAttr::cast(node).expect("an attribute");
                let node = attr.syntax();
                if !leading {
```

New:

```rust
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::STMT_LIST => {
                hash_inner_attrs(node, state)
            }
            NodeOrToken::Node(node) if ast::AnyAttr::can_cast(node.kind()) => {
                let attr = ast::AnyAttr::cast(node).expect("an attribute");
                if !leading {
```

Hunk 18. Old:

```rust
            NodeOrToken::Token(token) if leading => hash_token(&token, state),
```

New:

```rust
            NodeOrToken::Token(token) if leading => hash_token(token, state),
```

Hunk 19, `element`. Old:

```rust
/// Where `node` is in its file's flat tree.
fn element(node: Option<impl AstNode>) -> Option<u32> {
    node?.syntax().flat().map(|(_, at)| at)
}
```

New:

```rust
/// The node a view is, to find among its parent's children.
fn element<'t>(node: Option<impl AstNode<'t>>) -> Option<ast::Node<'t>> {
    Some(node?.syntax())
}
```

Hunk 20, `hash_full` and `hash_token`. Old:

```rust
fn hash_full(node: &SyntaxNode, state: &mut Feed<'_>) {
    let (tree, at) = node.flat().expect("a node of a flat tree");
```

New:

```rust
fn hash_full(node: ast::Node<'_>, state: &mut Feed<'_>) {
    let (tree, at) = node.flat();
```

Hunk 21. Old:

```rust
fn hash_token(token: &SyntaxToken, state: &mut Feed<'_>) {
```

New:

```rust
fn hash_token(token: ast::Token<'_>, state: &mut Feed<'_>) {
```

Hunk 22, `file_item_tree`'s count of a whole lowering (tests only). Old:

```rust
            count_lowered(|| file.parse(db).tree().items().count());
```

New:

```rust
            count_lowered(|| file.parse(db).view().items().count());
```

> forwarded to frontend-deps-items (it holds hir-def's `item_tree*`; its child frontend-deps-items-views holds them); told by tmux (requests, pass 25).

> frontend-deps-items-views (2026-10-29 21:5x): applied, hunks 1 to 22 as written. The block
> change left every quoted line as it was, so no port was needed. Open:
> `parse_extra_crate_attrs_view`, as (3). The bulk id and anchor compare follows in `items.rs`
> (part 2 of this lane), on the view code.

## frontend-tree: hir-def item_tree/items/tests.rs: onto views (5)

From `frontend-tree-move-def-items`. The tests read the file's text and items from the parse's
views.

needs: `frontend-tree: hir-def item_tree/items.rs: onto views (4)` (this file, `item_alone`).

Hunk 1. Old:

```rust
use syntax::{AstNode, ast::HasModuleItem};
```

New:

```rust
use syntax::view::HasModuleItem;
```

Hunk 2 (twice in the file, in `items_lowered_by` and `items_hashed_by`; replace both). Old:

```rust
    let before = file.parse(&*db).tree().syntax().text().to_string();
```

New:

```rust
    let before = file.parse(&*db).root().text().to_string();
```

Hunk 3. Old:

```rust
        let alone: Vec<ItemTree> = (file.parse(&db).tree().items())
            .map(|item| super::item_alone(&db, file, krate, &item))
```

New:

```rust
        let alone: Vec<ItemTree> = (file.parse(&db).view().items())
            .map(|item| super::item_alone(&db, file, krate, item))
```

> forwarded to frontend-deps-items (it holds hir-def's `item_tree*`; its child frontend-deps-items-views holds them); told by tmux (requests, pass 25).

> frontend-deps-items-views (2026-10-29 21:5x): applied, hunks 1 to 3 (hunk 2 in both places),
> no port.
