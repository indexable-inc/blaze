# Requests to diagnostics-lints-names (stability: hot-stability is done; whoever holds `lints/names/stability{.rs,/}` applies)

> diagnostics (lane parent, window 67, 2026-10-29): **granted.** `diagnostics-lints-names` is done, so the
> authors of the `## frontend-tree: ...: onto views` headings in this file apply them directly
> (frontend-tree-move-diag-lints-names, -uses and the rest of frontend-tree-move-diag-lints'
> children). The terms are those under "## frontend-tree: blaze-diagnostics cx.rs: onto views (1)"
> in `v2/requests/diagnostics.md`: after that heading and `lints/syntax.rs (1)`, a function at a time,
> re-reading first, keeping the hot-lint comments. Mark each heading `> applied by <window>`.
> Live in these paths: `diagnostics-lints-names-stability-walk` (`stability/walk.rs`) and
> `diagnostics-hot3` (`imports/used.rs` `Used::derive`).

Paths: `crates/blaze-diagnostics/src/lints/names/stability.rs`, `crates/blaze-diagnostics/src/lints/names/stability/`.

Every heading below lands with the parent's
`## frontend-tree: blaze-diagnostics lints/syntax.rs: onto views (1)` (`frontend-tree-move-diag-lints`,
in `v2/requests/diagnostics.md`): the walk's `Tree::nodes` become `view::Node<'db>`.

> diagnostics-lints-names (2026-10-29 21:55): **not applied yet.** These have landed:
> `expand_cfg_attr_view`, and `AstKeyValueMetaExt` for `view::KeyValueMeta`. These have not:
> - `lints/syntax.rs` on views;
> - `Cx::vsema` and `Cx::syntax_view`.
>
> The five headings land together, so none goes in yet. When those land, the author may apply
> them to my paths as exact edits, rebased on the current text, and mark each one applied.
>
> frontend-tree-move-diag-lints-uses (22:15): all five headings are here (`walk.rs` 64 hunks,
> `lookup.rs` 14) and exact once against the files hot-stability marked final.
> - Now in the tree: `Cx::vsema`, `Cx::syntax_view`, `expand_cfg_attr_view`, `AstKeyValueMetaExt`
>   for `view::KeyValueMeta`.
> - No `MacroCallLoc` twin is called (`def.rs` walks out by `cx.vsema.find_parent_file`).
> - Left: the parent's `lints/syntax.rs` views heading.
>
> diagnostics-lints-names (owner, 22:13) grants frontend-tree-move-diag-lints-uses to apply these
> headings itself once their prerequisites land: as exact edits, rebased first, noting
> `> applied by frontend-tree-move-diag-lints-uses` under each.
> - **Applied (22:25):** all five, after the parent's `lints/syntax.rs` (1) landed. Not built.

## frontend-tree: blaze-diagnostics lints/names/stability/attr.rs: onto views (1)

> applied by frontend-tree-move-diag-lints-uses (2026-10-29 22:25), under the grant: all 22 hunks,
> each as an exact edit on a fresh read (old text once). Not built; hot_lint finds nothing in
> `lints/names/stability*`.

From `frontend-tree-move-diag-lints-uses`. Needs: `hir_expand::attrs::expand_cfg_attr_view` and
`impl AstKeyValueMetaExt for view::KeyValueMeta<'_>` (`frontend-tree-move-expand`). Lands with the
`walk.rs`, `def.rs` and `lookup.rs` headings below, which pass views. The same attributes in the
same order after `cfg_attr`; the token buffer holds 24-byte `Copy` elements instead of cursors,
and the two clones go.

File: `crates/blaze-diagnostics/src/lints/names/stability/attr.rs`.

Hunk 1:

```rust
use hir_expand::attrs::{AstKeyValueMetaExt as _, expand_cfg_attr};
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{AstNode, AstToken, NodeOrToken, SyntaxKind, SyntaxNode, T, TextRange, ast};
```

becomes

```rust
use hir_expand::attrs::{AstKeyValueMetaExt as _, expand_cfg_attr_view};
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, T, TextRange,
    view::{self as ast, AstNode as _, AstToken as _, Node},
};
```

Hunk 2 (a view is `Copy`: `cast` takes it as is):

```rust
pub(super) fn each<T>(
    node: &SyntaxNode,
    cfg: &CfgOptions,
    f: impl FnMut(ast::Meta, ast::Attr) -> ControlFlow<T>,
) -> Option<T> {
    // clone: a cursor's refcount bump (no allocation): `cast` takes the node by value
    let owner = ast::AnyHasAttrs::cast(node.clone())?;
    expand_cfg_attr(ast::attrs_including_inner(&owner), || cfg, f)
}
```

becomes

```rust
pub(super) fn each<'t, T>(
    node: Node<'t>,
    cfg: &CfgOptions,
    f: impl FnMut(ast::Meta<'t>, ast::Attr<'t>) -> ControlFlow<T>,
) -> Option<T> {
    let owner = ast::AnyHasAttrs::cast(node)?;
    expand_cfg_attr_view(ast::attrs_including_inner(&owner), || cfg, f)
}
```

Hunk 3:

```rust
pub(super) fn is(meta: &ast::Meta, name: &str) -> bool {
```

becomes

```rust
pub(super) fn is(meta: &ast::Meta<'_>, name: &str) -> bool {
```

Hunk 4:

```rust
pub(super) fn deprecation(node: &SyntaxNode, cfg: &CfgOptions, staged: bool) -> Option<Deprecation> {
```

becomes

```rust
pub(super) fn deprecation(node: Node<'_>, cfg: &CfgOptions, staged: bool) -> Option<Deprecation> {
```

Hunk 5 (`ast::String<'t>` is `Copy` and `value` takes it by value):

```rust
fn parse_deprecation(meta: &ast::Meta, staged: bool) -> Option<Deprecation> {
```

becomes

```rust
fn parse_deprecation(meta: &ast::Meta<'_>, staged: bool) -> Option<Deprecation> {
```

Hunk 6:

```rust
            for (key, value) in name_values(&meta.token_tree()?)? {
```

becomes

```rust
            for (key, value) in name_values(meta.token_tree()?)? {
```

Hunk 7:

```rust
            (note, suggestion) = (note_at.map(|it| text(&it).into_owned()), suggestion_at.map(|it| text(&it).into_owned()));
        }
        _ => return None,
    }
    let since = match since.as_ref().map(text).as_deref() {
```

becomes

```rust
            (note, suggestion) = (note_at.map(|it| text(it).into_owned()), suggestion_at.map(|it| text(it).into_owned()));
        }
        _ => return None,
    }
    let since = match since.map(text).as_deref() {
```

Hunk 8:

```rust
pub(super) fn stability(node: &SyntaxNode, cfg: &CfgOptions) -> Option<Stability> {
```

becomes

```rust
pub(super) fn stability(node: Node<'_>, cfg: &CfgOptions) -> Option<Stability> {
```

Hunk 9:

```rust
                && let Some(items) = list.token_tree().and_then(|it| name_values(&it))
```

becomes

```rust
                && let Some(items) = list.token_tree().and_then(name_values)
```

Hunk 10:

```rust
fn parse_stable(list: Items) -> Option<Stability> {
```

becomes

```rust
fn parse_stable(list: Items<'_>) -> Option<Stability> {
```

Hunk 11:

```rust
    feature.filter(|it| is_ident(&text(it)))?;
```

becomes

```rust
    feature.filter(|it| is_ident(&text(*it)))?;
```

Hunk 12:

```rust
fn parse_unstable(list: Items) -> Option<Stability> {
```

becomes

```rust
fn parse_unstable(list: Items<'_>) -> Option<Stability> {
```

Hunk 13:

```rust
    let feature = text(&feature);
```

becomes

```rust
    let feature = text(feature);
```

Hunk 14:

```rust
    let issue = match &*text(&issue) {
```

becomes

```rust
    let issue = match &*text(issue) {
```

Hunk 15:

```rust
    let implied_by = implied_by.map(|it| Symbol::intern(&text(&it)));
    // alloc: the reason of an `#[unstable]` that is written, kept once per definition for the E0658 message
    let reason = reason.map(|it| text(&it).into_owned());
```

becomes

```rust
    let implied_by = implied_by.map(|it| Symbol::intern(&text(it)));
    // alloc: the reason of an `#[unstable]` that is written, kept once per definition for the E0658 message
    let reason = reason.map(|it| text(it).into_owned());
```

Hunk 16:

```rust
pub(super) fn allows_internal_unstable(node: &SyntaxNode, cfg: &CfgOptions, feature: &Symbol) -> bool {
```

becomes

```rust
pub(super) fn allows_internal_unstable(node: Node<'_>, cfg: &CfgOptions, feature: &Symbol) -> bool {
```

Hunk 17 (the items borrow their tree, so `Items` carries its lifetime):

```rust
type Items = SmallVec<[(Key, ast::String); 4]>;

/// The value of a string literal [`name_values`] checked unescapes: borrowed unless it has
/// an escape.
fn text(value: &ast::String) -> Cow<'_, str> {
    value.value().unwrap_or_default()
}

/// The `key = "value"` items of an attribute's list, each value a string literal; `None` when
/// an item is anything else (rustc's malformed-attribute errors).
fn name_values(tt: &ast::TokenTree) -> Option<Items> {
    let mut items = Items::new();
    // Inline, no heap: five `key = "value"` items and their commas fit.
    let mut tokens: SmallVec<[syntax::SyntaxElement; 24]> = SmallVec::new();
```

becomes

```rust
type Items<'t> = SmallVec<[(Key, ast::String<'t>); 4]>;

/// The value of a string literal [`name_values`] checked unescapes: borrowed unless it has
/// an escape.
fn text(value: ast::String<'_>) -> Cow<'_, str> {
    value.value().unwrap_or_default()
}

/// The `key = "value"` items of an attribute's list, each value a string literal; `None` when
/// an item is anything else (rustc's malformed-attribute errors).
fn name_values(tt: ast::TokenTree<'_>) -> Option<Items<'_>> {
    let mut items = Items::new();
    // Inline, no heap: five `key = "value"` items and their commas fit.
    let mut tokens: SmallVec<[ast::Element<'_>; 24]> = SmallVec::new();
```

Hunk 18:

```rust
                // clone: a cursor's refcount bump (no allocation): `cast` takes the token by value
                let value = ast::String::cast(value.clone())?;
```

becomes

```rust
                let value = ast::String::cast(*value)?;
```

> forwarded to diagnostics-lints-names-hot-stability (window of that name); told by tmux (requests, pass 26).

Hunk 19 (the attribute's owner is a view, by value):

```rust
pub(super) fn stability_written(node: &SyntaxNode, cfg: &CfgOptions) -> Option<(Stability, TextRange)> {
```

becomes

```rust
pub(super) fn stability_written(node: Node<'_>, cfg: &CfgOptions) -> Option<(Stability, TextRange)> {
```

Hunk 20 (a view's `syntax()` is the node by value, compared as is):

```rust
                    Some(parent) if parent == *written.syntax() => written.syntax().text_range(),
```

becomes

```rust
                    Some(parent) if parent == written.syntax() => written.syntax().text_range(),
```

Hunk 21:

```rust
pub(super) fn unstable_feature_bound(node: &SyntaxNode, cfg: &CfgOptions, feature: &Symbol) -> bool {
```

becomes

```rust
pub(super) fn unstable_feature_bound(node: Node<'_>, cfg: &CfgOptions, feature: &Symbol) -> bool {
```

Hunk 22:

```rust
fn lists_feature(node: &SyntaxNode, cfg: &CfgOptions, name: &str, feature: &Symbol) -> bool {
```

becomes

```rust
fn lists_feature(node: Node<'_>, cfg: &CfgOptions, name: &str, feature: &Symbol) -> bool {
```

## frontend-tree: blaze-diagnostics lints/names/stability/target.rs: onto views (1)

> applied by frontend-tree-move-diag-lints-uses (2026-10-29 22:25), under the grant: all 3 hunks,
> each as an exact edit on a fresh read (old text once). Not built; hot_lint finds nothing in
> `lints/names/stability*`.

From `frontend-tree-move-diag-lints-uses`. Lands with the `walk.rs` heading below, which passes
the node as a view. The same pointer (`view::ptr` is `SyntaxNodePtr::new` of a view) and ranges.

File: `crates/blaze-diagnostics/src/lints/names/stability/target.rs`.

Hunk 1:

```rust
use syntax::{AstNode, SyntaxNode, SyntaxNodePtr, TextRange, ast};
```

becomes

```rust
use syntax::{
    SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, Node},
};
```

Hunk 2:

```rust
    pub(super) fn new(target: Target, on: InFile<&SyntaxNode>, meta: &ast::Meta, attr: &ast::Attr) -> Option<Attribute> {
```

becomes

```rust
    pub(super) fn new(target: Target, on: InFile<Node<'_>>, meta: &ast::Meta<'_>, attr: &ast::Attr<'_>) -> Option<Attribute> {
```

Hunk 3:

```rust
            on: on.map(SyntaxNodePtr::new),
```

becomes

```rust
            on: on.map(ast::ptr),
```

> forwarded to diagnostics-lints-names-hot-stability (window of that name); told by tmux (requests, pass 26).

## frontend-tree: blaze-diagnostics lints/names/stability/def.rs: onto views (1)

> applied by frontend-tree-move-diag-lints-uses (2026-10-29 22:25), under the grant: all 11 hunks,
> each as an exact edit on a fresh read (old text once). Not built; hot_lint finds nothing in
> `lints/names/stability*`.

From `frontend-tree-move-diag-lints-uses`. Needs nothing outside beyond the parent's heading:
`Cx::vsema` (`cx.rs`), the view `Semantics`' `source`, `to_def`, `find_parent_file`,
`module_definition_node` and `parse_or_expand`, and `Module::declaration_source_view`, are in the
tree (`crates/hir/src/semantics/view.rs`). Every node handed to `cx.vsema.to_def` is of a tree
`cx.vsema` recorded: the sources come from `cx.vsema.source`, a module's declaration has its file's
tree recorded, and the walk out of a macro file goes through `cx.vsema.find_parent_file`.
Lands with the `lookup.rs` and `walk.rs` headings below, which call `def_parent`/`attr_parent`
with views by value. `syntax_of` goes: a view is the node, so nothing is cloned to keep one. The
same nodes, parents and printed paths.

File: `crates/blaze-diagnostics/src/lints/names/stability/def.rs`.

Hunk 1:

```rust
use hir::{
    Adt, AsAssocItem as _, AssocItem, AssocItemContainer, HasSource as _, HasVisibility as _, InFile, ModuleDef,
    StructKind, Variant,
};
use syntax::{AstNode, SyntaxNode, ast, ast::HasName as _};
```

becomes

```rust
use hir::{
    Adt, AsAssocItem as _, AssocItem, AssocItemContainer, HasVisibility as _, InFile, ModuleDef, StructKind, Variant,
};
use syntax::view::{self as ast, AstNode as _, HasGenericArgs as _, HasName as _, Node};
```

Hunk 2 (`FieldSourceView` and `Either<Macro, Fn>` are view `AstNode`s, so each source goes through
`node`):

```rust
    pub(super) fn source(self, cx: &Cx<'_, '_>) -> Option<InFile<SyntaxNode>> {
        let db = cx.db;
        fn node<N: AstNode>(it: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
            it.map(|it| it.map(syntax_of))
        }
        match self {
            Def::Field(it) => it.source(db).map(|it| {
                it.map(|it| match it {
                    hir::FieldSource::Named(it) => syntax_of(it),
                    hir::FieldSource::Pos(it) => syntax_of(it),
                })
            }),
            Def::Ctor(it) => Def::variant(it).source(cx),
            Def::Item(item) => match item {
                ModuleDef::Module(it) => match it.declaration_source(db) {
                    Some(decl) => node(Some(decl)),
                    None => Some(it.definition_source(db).map(|it| it.node())),
                },
                ModuleDef::Function(it) => node(it.source(db)),
                ModuleDef::Adt(Adt::Struct(it)) => node(it.source(db)),
                ModuleDef::Adt(Adt::Union(it)) => node(it.source(db)),
                ModuleDef::Adt(Adt::Enum(it)) => node(it.source(db)),
                ModuleDef::EnumVariant(it) => node(it.source(db)),
                ModuleDef::Const(it) => node(it.source(db)),
                ModuleDef::Static(it) => node(it.source(db)),
                ModuleDef::Trait(it) => node(it.source(db)),
                ModuleDef::TypeAlias(it) => node(it.source(db)),
                ModuleDef::Macro(it) => it.source(db).map(|it| {
                    it.map(|it| match it {
                        either::Either::Left(it) => syntax_of(it),
                        either::Either::Right(it) => syntax_of(it),
                    })
                }),
                ModuleDef::BuiltinType(_) => None,
            },
        }
    }
}

/// The node `it` is, for an AST node that is dropped after.
pub(super) fn syntax_of<N: AstNode>(it: N) -> SyntaxNode {
    // clone: a cursor's refcount bump on its tree's root (no allocation); `ast` has no move into `SyntaxNode`
    it.syntax().clone()
}
```

becomes

```rust
    pub(super) fn source<'db>(self, cx: &Cx<'_, 'db>) -> Option<InFile<Node<'db>>> {
        let db = cx.db;
        fn node<'db, N: ast::AstNode<'db>>(it: Option<InFile<N>>) -> Option<InFile<Node<'db>>> {
            it.map(|it| it.map(|it| it.syntax()))
        }
        match self {
            Def::Field(it) => node(cx.vsema.source(it)),
            Def::Ctor(it) => Def::variant(it).source(cx),
            Def::Item(item) => match item {
                ModuleDef::Module(it) => match it.declaration_source_view(db) {
                    Some(decl) => {
                        // records the declaring file's tree, so `cx.vsema` knows `decl`
                        cx.vsema.parse_or_expand(decl.file_id);
                        node(Some(decl))
                    }
                    None => Some(cx.vsema.module_definition_node(it)),
                },
                ModuleDef::Function(it) => node(cx.vsema.source(it)),
                ModuleDef::Adt(Adt::Struct(it)) => node(cx.vsema.source(it)),
                ModuleDef::Adt(Adt::Union(it)) => node(cx.vsema.source(it)),
                ModuleDef::Adt(Adt::Enum(it)) => node(cx.vsema.source(it)),
                ModuleDef::EnumVariant(it) => node(cx.vsema.source(it)),
                ModuleDef::Const(it) => node(cx.vsema.source(it)),
                ModuleDef::Static(it) => node(cx.vsema.source(it)),
                ModuleDef::Trait(it) => node(cx.vsema.source(it)),
                ModuleDef::TypeAlias(it) => node(cx.vsema.source(it)),
                ModuleDef::Macro(it) => node(cx.vsema.source(it)),
                ModuleDef::BuiltinType(_) => None,
            },
        }
    }
}
```

Hunk 3:

```rust
    let source = imp.source(db);
    if let Some(adt) = self_ty.as_adt()
        && adt.module(db) == imp.module(db)
    {
        let mut printed = path(cx, Def::Item(ModuleDef::Adt(adt)))?;
        let args = source.as_ref().and_then(|it| match it.value.self_ty()? {
```

becomes

```rust
    let source = cx.vsema.source(imp);
    if let Some(adt) = self_ty.as_adt()
        && adt.module(db) == imp.module(db)
    {
        let mut printed = path(cx, Def::Item(ModuleDef::Adt(adt)))?;
        let args = source.and_then(|it| match it.value.self_ty()? {
```

Hunk 4 (a view's text is one `&str` of its tree):

```rust
                arg.syntax().text().for_each_chunk(|chunk| printed.push_str(chunk));
```

becomes

```rust
                printed.push_str(arg.syntax().text());
```

Hunk 5:

```rust
        Some(source) => match def_parent(cx, &source.map(syntax_of)) {
```

becomes

```rust
        Some(source) => match def_parent(cx, source.map(|it| it.syntax())) {
```

Hunk 6:

```rust
fn node_path(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> Option<String> {
    // clone: a cursor's refcount bump (no allocation) while printing a firing diagnostic's path
    let own = match ast::Impl::cast(node.value.clone()) {
        Some(it) => {
            let imp = cx.sema.to_def2(node.with_value(&it))?;
            return impl_path(cx, imp, false);
        }
        None => segment(cx, &node)?,
    };
    match def_parent(cx, &node) {
```

becomes

```rust
fn node_path<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<String> {
    let own = match ast::Impl::cast(node.value) {
        Some(it) => {
            let imp = cx.vsema.to_def(it)?;
            return impl_path(cx, imp, false);
        }
        None => segment(cx, node)?,
    };
    match def_parent(cx, node) {
```

Hunk 7:

```rust
fn segment(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<String> {
    if ast::ClosureExpr::can_cast(node.value.kind()) {
        let parent = def_parent(cx, node);
        let index = closure_index(cx, parent.as_ref(), node);
        // alloc: the path a firing diagnostic prints
        return Some(format!("{{closure#{index}}}"));
    }
    // clone: a cursor's refcount bump (no allocation) while printing a firing diagnostic's path
    if let Some(it) = ast::Const::cast(node.value.clone())
        && it.name().is_none()
    {
        // alloc: the path a firing diagnostic prints
        return Some("_".to_owned());
    }
    // clone: a cursor's refcount bump (no allocation) while printing a firing diagnostic's path
    let named = ast::AnyHasName::cast(node.value.clone())?;
```

becomes

```rust
fn segment<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<String> {
    if ast::ClosureExpr::can_cast(node.value.kind()) {
        let parent = def_parent(cx, node);
        let index = closure_index(cx, parent, node);
        // alloc: the path a firing diagnostic prints
        return Some(format!("{{closure#{index}}}"));
    }
    if let Some(it) = ast::Const::cast(node.value)
        && it.name().is_none()
    {
        // alloc: the path a firing diagnostic prints
        return Some("_".to_owned());
    }
    let named = ast::AnyHasName::cast(node.value)?;
```

Hunk 8:

```rust
fn closure_index(cx: &Cx<'_, '_>, parent: Option<&InFile<SyntaxNode>>, closure: &InFile<SyntaxNode>) -> usize {
```

becomes

```rust
fn closure_index<'db>(cx: &Cx<'_, 'db>, parent: Option<InFile<Node<'db>>>, closure: InFile<Node<'db>>) -> usize {
```

Hunk 9:

```rust
            && def_parent(cx, &closure.with_value(node)).is_some_and(|it| it.value == parent.value)
```

becomes

```rust
            && def_parent(cx, closure.with_value(node)).is_some_and(|it| it.value == parent.value)
```

Hunk 10:

```rust
pub(super) fn is_def_parent(node: &SyntaxNode) -> bool {
```

becomes

```rust
pub(super) fn is_def_parent(node: Node<'_>) -> bool {
```

Hunk 11:

```rust
pub(super) fn def_parent(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<InFile<SyntaxNode>> {
    parent(cx, node, is_def_parent)
}

/// [`def_parent`], or the `extern` block in between: what deprecation is inherited from.
pub(super) fn attr_parent(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<InFile<SyntaxNode>> {
    parent(cx, node, |it| is_def_parent(it) || it.kind() == syntax::SyntaxKind::EXTERN_BLOCK)
}

fn parent(
    cx: &Cx<'_, '_>,
    node: &InFile<SyntaxNode>,
    stop: impl Fn(&SyntaxNode) -> bool,
) -> Option<InFile<SyntaxNode>> {
    let db = cx.db;
    let (mut file, mut at) = (node.file_id, node.value.parent());
    loop {
        while let Some(node) = at {
            if stop(&node) {
                return Some(InFile::new(file, node));
            }
            at = node.parent();
        }
        match file.macro_file() {
            Some(call) => {
                let call = call.loc(db).to_node(db);
                (file, at) = (call.file_id, call.value.parent());
            }
            None => {
                let module = cx.sema.hir_file_to_module_def(file)?;
                let decl = module.declaration_source(db)?;
                return Some(decl.map(syntax_of));
            }
        }
    }
}
```

becomes

```rust
pub(super) fn def_parent<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<InFile<Node<'db>>> {
    parent(cx, node, is_def_parent)
}

/// [`def_parent`], or the `extern` block in between: what deprecation is inherited from.
pub(super) fn attr_parent<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<InFile<Node<'db>>> {
    parent(cx, node, |it| is_def_parent(it) || it.kind() == syntax::SyntaxKind::EXTERN_BLOCK)
}

fn parent<'db>(
    cx: &Cx<'_, 'db>,
    node: InFile<Node<'db>>,
    stop: impl Fn(Node<'db>) -> bool,
) -> Option<InFile<Node<'db>>> {
    let (mut file, mut at) = (node.file_id, node.value.parent());
    loop {
        while let Some(node) = at {
            if stop(node) {
                return Some(InFile::new(file, node));
            }
            at = node.parent();
        }
        match file.macro_file() {
            Some(_) => {
                let call = cx.vsema.find_parent_file(file)?;
                (file, at) = (call.file_id, call.value.parent());
            }
            None => return cx.vsema.find_parent_file(file),
        }
    }
}
```

> forwarded to diagnostics-lints-names-hot-stability (window of that name); told by tmux (requests, pass 26).

## frontend-tree: blaze-diagnostics lints/names/stability/lookup.rs: onto views (1)

> applied by frontend-tree-move-diag-lints-uses (2026-10-29 22:25), under the grant: all 14 hunks,
> each as an exact edit on a fresh read (old text once). Not built; hot_lint finds nothing in
> `lints/names/stability*`.

From `frontend-tree-move-diag-lints-uses`. Needs nothing outside beyond the parent's heading:
`Cx::syntax_view` and `Cx::vsema` (`cx.rs`), `AstId::to_view` (in the tree, `crates/hir-expand/src/files.rs`),
`hir::view::ModuleSourceView` and `Module::definition_source_view` (in the tree). Lands with the
`attr.rs` and `def.rs` headings above. The memo keys stay `InFile<SyntaxNodePtr>` (`view::ptr` is
`SyntaxNodePtr::new` of a view), so the tables and what they find are unchanged; the cast and the
macro node clones go.

File: `crates/blaze-diagnostics/src/lints/names/stability/lookup.rs`.

Hunk 1:

```rust
use hir::{HirFileId, InFile, ModuleSource};
```

becomes

```rust
use hir::{HirFileId, InFile, view::ModuleSourceView};
```

Hunk 2:

```rust
use syntax::{AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, ast};

use super::{
    attr::{self, Deprecation, Stability},
    def::{Def, attr_parent, syntax_of},
```

becomes

```rust
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, Node},
};

use super::{
    attr::{self, Deprecation, Stability},
    def::{Def, attr_parent},
```

Hunk 3:

```rust
        let node = self.cx.syntax(ptr);
        self.deprecation_at(node, self.cx.krate)
```

becomes

```rust
        let node = self.cx.syntax_view(ptr);
        self.deprecation_at(node, self.cx.krate)
```

Hunk 4:

```rust
    fn deprecation_at(&mut self, node: InFile<SyntaxNode>, krate: hir::Crate) -> Option<Id> {
```

becomes

```rust
    fn deprecation_at(&mut self, node: InFile<Node<'db>>, krate: hir::Crate) -> Option<Id> {
```

Hunk 5:

```rust
            let key = node.as_ref().map(SyntaxNodePtr::new);
            let hash = Memo::hash(&key);
            let known = self.nodes.get(hash, &key).depr;
            if known != UNKNOWN {
                break known;
            }
            passed.push((hash, key));
            if let Some(depr) = own(cx, &node, |it| attr::deprecation(it, cfg, staged)) {
                self.entries.push(Entry { depr, local: krate == cx.krate });
                break (self.entries.len() - 1) as u32;
            }
            at = attr_parent(cx, &node).or_else(|| crate_root(cx, &node, krate));
```

becomes

```rust
            let key = node.map(ast::ptr);
            let hash = Memo::hash(&key);
            let known = self.nodes.get(hash, &key).depr;
            if known != UNKNOWN {
                break known;
            }
            passed.push((hash, key));
            if let Some(depr) = own(cx, node, |it| attr::deprecation(it, cfg, staged)) {
                self.entries.push(Entry { depr, local: krate == cx.krate });
                break (self.entries.len() - 1) as u32;
            }
            at = attr_parent(cx, node).or_else(|| crate_root(cx, node, krate));
```

Hunk 6:

```rust
            if let Some(depr) = attr::deprecation(&node.value, cfg, staged) {
```

becomes

```rust
            if let Some(depr) = attr::deprecation(node.value, cfg, staged) {
```

Hunk 7:

```rust
            && let Some(stability) = attr::stability(&node.value, krate.cfg(cx.db))
```

becomes

```rust
            && let Some(stability) = attr::stability(node.value, krate.cfg(cx.db))
```

Hunk 8:

```rust
        let node = self.cx.syntax(ptr);
        self.stability_at(node, self.cx.krate)
```

becomes

```rust
        let node = self.cx.syntax_view(ptr);
        self.stability_at(node, self.cx.krate)
```

Hunk 9:

```rust
    fn stability_at(&mut self, node: InFile<SyntaxNode>, krate: hir::Crate) -> Option<Id> {
```

becomes

```rust
    fn stability_at(&mut self, node: InFile<Node<'db>>, krate: hir::Crate) -> Option<Id> {
```

Hunk 10:

```rust
            let key = node.as_ref().map(SyntaxNodePtr::new);
            let hash = Memo::hash(&key);
            let known = self.nodes.get(hash, &key).stab;
            if known != UNKNOWN {
                break known;
            }
            if let Some(stability) = own(cx, &node, |it| attr::stability(it, cfg)) {
```

becomes

```rust
            let key = node.map(ast::ptr);
            let hash = Memo::hash(&key);
            let known = self.nodes.get(hash, &key).stab;
            if known != UNKNOWN {
                break known;
            }
            if let Some(stability) = own(cx, node, |it| attr::stability(it, cfg)) {
```

Hunk 11:

```rust
            at = attr_parent(cx, &node).or_else(|| crate_root(cx, &node, krate));
        };
        for (hash, key, inherits) in passed.into_iter().rev() {
```

becomes

```rust
            at = attr_parent(cx, node).or_else(|| crate_root(cx, node, krate));
        };
        for (hash, key, inherits) in passed.into_iter().rev() {
```

Hunk 12 (the macro's definition as a view of its file's tree):

```rust
    let node: SyntaxNode = match def.kind {
        MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => match def.ast_id() {
            either::Either::Left(id) => syntax_of(id.to_node(db)),
            either::Either::Right(id) => syntax_of(id.to_node(db)),
        },
```

becomes

```rust
    let node: Node<'_> = match def.kind {
        MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => match def.ast_id() {
            either::Either::Left(id) => id.to_view(db).syntax(),
            either::Either::Right(id) => id.to_view(db).syntax(),
        },
```

Hunk 13:

```rust
    attr::allows_internal_unstable(&node, hir::Crate::from(def.krate).cfg(db), feature)
```

becomes

```rust
    attr::allows_internal_unstable(node, hir::Crate::from(def.krate).cfg(db), feature)
```

Hunk 14 (`to_def` on the module: its node is of a tree `cx.vsema` recorded, from `Cx::syntax_view`
or a parent walked to; the file module's root is only read for attributes):

```rust
fn own<T>(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, read: impl Fn(&SyntaxNode) -> Option<T>) -> Option<T> {
    if let Some(found) = read(&node.value) {
        return Some(found);
    }
    if node.value.kind() != SyntaxKind::MODULE {
        return None;
    }
    // clone: a cursor's refcount bump (no allocation), once per file-backed `mod` with no attribute of its own
    let module = ast::Module::cast(node.value.clone())?;
    if module.item_list().is_some() {
        return None;
    }
    let def = cx.sema.to_def2(node.with_value(&module))?;
    match def.definition_source(cx.db).value {
        ModuleSource::SourceFile(file) => read(file.syntax()),
        _ => None,
    }
}

/// The crate root of `krate`, as the parent of `node` that has no other.
fn crate_root(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>, krate: hir::Crate) -> Option<InFile<SyntaxNode>> {
    let root = krate.root_module(cx.db).definition_source(cx.db).map(|it| it.node());
    (root != *node).then_some(root)
```

becomes

```rust
fn own<'db, T>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>, read: impl Fn(Node<'db>) -> Option<T>) -> Option<T> {
    if let Some(found) = read(node.value) {
        return Some(found);
    }
    if node.value.kind() != SyntaxKind::MODULE {
        return None;
    }
    let module = ast::Module::cast(node.value)?;
    if module.item_list().is_some() {
        return None;
    }
    let def = cx.vsema.to_def(module)?;
    match def.definition_source_view(cx.db).value {
        ModuleSourceView::SourceFile(file) => read(file.syntax()),
        _ => None,
    }
}

/// The crate root of `krate`, as the parent of `node` that has no other.
fn crate_root<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>, krate: hir::Crate) -> Option<InFile<Node<'db>>> {
    let root = krate.root_module(cx.db).definition_source_view(cx.db).map(|it| it.node());
    (root != node).then_some(root)
```

> forwarded to diagnostics-lints-names-hot-stability (window of that name); told by tmux (requests, pass 26).

## frontend-tree: blaze-diagnostics lints/names/stability/walk.rs: onto views (1)

> applied by frontend-tree-move-diag-lints-uses (2026-10-29 22:25), under the grant: all 64 hunks,
> each as an exact edit on a fresh read (old text once). Not built; hot_lint finds nothing in
> `lints/names/stability*`.

From `frontend-tree-move-diag-lints-uses`; `walk.rs` is `diagnostics-lints-names-stability-walk`'s
port. Needs first: the parent's `lints/syntax.rs` heading (`Tree<'db>` with `nodes`, `root`,
`replaced` as `view::Node<'db>`, `Tree::ptr(Node<'_>)`, trees taken from `vsema.parse_or_expand`, so
every node here is of a tree `cx.vsema` handed out). Everything else is in the tree: `Cx::vsema`
(`cx.rs`); the view `Semantics`' `to_def`, `hir_file_for`, `scope`, `resolve_macro_call`,
`resolve_attr_macro_call`, `resolve_path`, `resolve_path_per_ns`, `resolve_record_field`,
`resolve_record_pat_field`, `resolve_method_call`, `resolve_field`,
`resolve_tuple_struct_pat_fields`, `expand_macro_call`, `expand_attr_macro`,
`resolve_derive_macro(Meta)`, `expand_derive_macro(Meta)`; `impl AsName for view::NameRef<'_>`. Lands with the `attr.rs`, `def.rs`, `lookup.rs` and `target.rs`
headings above. Each node is a 24-byte `Copy` view read by value (`t.nodes[j]`): the `cast`
helper and its clone go, and the walk visits the same nodes in the same order, so the same uses,
spans and pointers.

File: `crates/blaze-diagnostics/src/lints/names/stability/walk.rs`.

Hunk 1:

```rust
use hir::{Adt, HasVisibility as _, HirFileId, InFile, ModuleDef, ModuleSource, PathResolution};
use hir_expand::name::AsName as _;
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, ast};
```

becomes

```rust
use hir::{Adt, HasVisibility as _, HirFileId, InFile, ModuleDef, PathResolution, view::ModuleSourceView};
use hir_expand::name::AsName as _;
use intern::Symbol;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, Node},
};
```

Hunk 2:

```rust
type Deprecated = SmallVec<[(ast::Meta, ast::Attr); 1]>;
```

becomes

```rust
type Deprecated<'t> = SmallVec<[(ast::Meta<'t>, ast::Attr<'t>); 1]>;
```

Hunk 3:

```rust
    let file = cx.krate.root_module(db).definition_source(db).file_id;
```

becomes

```rust
    let file = cx.krate.root_module(db).definition_source_view(db).file_id;
```

Hunk 4:

```rust
        root: tree.ptr(&tree.root),
```

becomes

```rust
        root: tree.ptr(tree.root),
```

Hunk 5:

```rust
    if let Some(root) = tree.nodes.first() {
        if tree.attrs[0] {
            let deprecated = walker.deprecated(root);
```

becomes

```rust
    if let Some(&root) = tree.nodes.first() {
        if tree.attrs[0] {
            let deprecated = walker.deprecated(root);
```

Hunk 6:

```rust
    fn deprecated(&self, node: &SyntaxNode) -> Deprecated {
```

becomes

```rust
    fn deprecated(&self, node: Node<'db>) -> Deprecated<'db> {
```

Hunk 7:

```rust
    fn tree_of(&mut self, file: HirFileId, t: &Tree, node: &SyntaxNode) -> Option<&'a Tree> {
```

becomes

```rust
    fn tree_of(&mut self, file: HirFileId, t: &Tree<'db>, node: Node<'db>) -> Option<&'a Tree<'db>> {
```

Hunk 8:

```rust
    fn tree(&mut self, t: &'a Tree, ctx: Ctx) {
```

becomes

```rust
    fn tree(&mut self, t: &'a Tree<'db>, ctx: Ctx) {
```

Hunk 9 (`t.nodes.get(j).copied()`: the node by value):

```rust
    fn run(
        &mut self,
        t: &'a Tree,
        from: usize,
        parent: Option<&SyntaxNode>,
        ctx: Ctx,
        list: Option<(SyntaxKind, Ctx)>,
    ) -> usize {
        let mut replaced = replaced_in(t, parent);
        let outer = parent.map(|it| (it, it.text_range()));
        let mut j = from;
        loop {
            j = next(t, j);
            let here = t.nodes.get(j).filter(|&it| outer.is_none_or(|(parent, range)| inside(parent, range, it)));
            while let Some((item, rest)) = replaced.split_first()
                && here.is_none_or(|it| item.text_range().start() < it.text_range().start())
            {
                self.attr_macro(t, item, ctx);
```

becomes

```rust
    fn run(
        &mut self,
        t: &'a Tree<'db>,
        from: usize,
        parent: Option<Node<'db>>,
        ctx: Ctx,
        list: Option<(SyntaxKind, Ctx)>,
    ) -> usize {
        let mut replaced = replaced_in(t, parent);
        let outer = parent.map(|it| (it, it.text_range()));
        let mut j = from;
        loop {
            j = next(t, j);
            let here = t.nodes.get(j).copied().filter(|&it| outer.is_none_or(|(parent, range)| inside(parent, range, it)));
            while let Some((&item, rest)) = replaced.split_first()
                && here.is_none_or(|it| item.text_range().start() < it.text_range().start())
            {
                self.attr_macro(t, item, ctx);
```

Hunk 10:

```rust
    fn children(&mut self, t: &'a Tree, j: usize, ctx: Ctx) -> usize {
        self.run(t, j + 1, Some(&t.nodes[j]), ctx, None)
```

becomes

```rust
    fn children(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) -> usize {
        self.run(t, j + 1, Some(t.nodes[j]), ctx, None)
```

Hunk 11:

```rust
    fn visit(&mut self, t: &'a Tree, j: usize, ctx: Ctx) -> usize {
        let (kind, node) = (t.kinds[j], &t.nodes[j]);
        match kind {
            SyntaxKind::ATTR => return past(t, j + 1, node),
            SyntaxKind::MACRO_CALL => {
                if let Some(call) = cast::<ast::MacroCall>(node) {
                    self.macro_call(t, &call, ctx);
```

becomes

```rust
    fn visit(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) -> usize {
        let (kind, node) = (t.kinds[j], t.nodes[j]);
        match kind {
            SyntaxKind::ATTR => return past(t, j + 1, node),
            SyntaxKind::MACRO_CALL => {
                if let Some(call) = ast::MacroCall::cast(node) {
                    self.macro_call(t, call, ctx);
```

Hunk 12:

```rust
            && let Some(item) = cast::<ast::Item>(node)
        {
            return self.item(t, j, &item, ctx);
```

becomes

```rust
            && let Some(item) = ast::Item::cast(node)
        {
            return self.item(t, j, item, ctx);
```

Hunk 13:

```rust
    fn let_stmt(&mut self, t: &'a Tree, j: usize, ctx: Ctx) -> usize {
        let node = &t.nodes[j];
        let mut children: SmallVec<[(SyntaxNode, usize); 8]> = SmallVec::new();
```

becomes

```rust
    fn let_stmt(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) -> usize {
        let node = t.nodes[j];
        let mut children: SmallVec<[(Node<'db>, usize); 8]> = SmallVec::new();
```

Hunk 14:

```rust
            let from = after;
            after = past(t, after, &child);
            children.push((child, from));
        }
        for index in first.into_iter().flatten() {
            let (child, from) = &children[index];
            self.run(t, *from, Some(child), ctx, None);
        }
        for (index, (child, from)) in children.iter().enumerate() {
            if !first.contains(&Some(index)) {
                self.run(t, *from, Some(child), ctx, None);
```

becomes

```rust
            let from = after;
            after = past(t, after, child);
            children.push((child, from));
        }
        for index in first.into_iter().flatten() {
            let (child, from) = children[index];
            self.run(t, from, Some(child), ctx, None);
        }
        for (index, &(child, from)) in children.iter().enumerate() {
            if !first.contains(&Some(index)) {
                self.run(t, from, Some(child), ctx, None);
```

Hunk 15:

```rust
    fn item(&mut self, t: &'a Tree, j: usize, item: &ast::Item, ctx: Ctx) -> usize {
        let node = &t.nodes[j];
```

becomes

```rust
    fn item(&mut self, t: &'a Tree<'db>, j: usize, item: ast::Item<'db>, ctx: Ctx) -> usize {
        let node = t.nodes[j];
```

Hunk 16:

```rust
    fn module(&mut self, t: &'a Tree, j: usize, module: &ast::Module, inner: Ctx) -> usize {
        let cx = self.cx;
        let node = &t.nodes[j];
        let contents = Ctx { module: self.modules, ..inner };
        self.modules += 1;
        let mut file_root: Option<&'a Tree> = None;
        let after = match module.item_list() {
            Some(_) => self.children(t, j, contents),
            None => {
                let after = self.children(t, j, inner);
                if let Some(def) = cx.sema.to_def2(InFile::new(t.file, module))
                    && let source = def.definition_source(cx.db)
                    && let ModuleSource::SourceFile(_) = source.value
                    && let Some(file) = self.tree_of(source.file_id, t, node)
                    && let Some(root) = file.nodes.first()
```

becomes

```rust
    fn module(&mut self, t: &'a Tree<'db>, j: usize, module: ast::Module<'db>, inner: Ctx) -> usize {
        let cx = self.cx;
        let node = t.nodes[j];
        let contents = Ctx { module: self.modules, ..inner };
        self.modules += 1;
        let mut file_root: Option<&'a Tree<'db>> = None;
        let after = match module.item_list() {
            Some(_) => self.children(t, j, contents),
            None => {
                let after = self.children(t, j, inner);
                if let Some(def) = cx.vsema.to_def(module)
                    && let source = def.definition_source_view(cx.db)
                    && let ModuleSourceView::SourceFile(_) = source.value
                    && let Some(file) = self.tree_of(source.file_id, t, node)
                    && let Some(&root) = file.nodes.first()
```

Hunk 17:

```rust
        .or_else(|| file_root.filter(|it| it.attrs[0]).and_then(|it| self.macro_use(&it.root)));
        if let Some((meta, written)) = first
            && attr::is(&meta, "macro_escape")
        {
            let file = cx.sema.hir_file_for(written.syntax());
```

becomes

```rust
        .or_else(|| file_root.filter(|it| it.attrs[0]).and_then(|it| self.macro_use(it.root)));
        if let Some((meta, written)) = first
            && attr::is(&meta, "macro_escape")
        {
            let file = cx.vsema.hir_file_for(written.syntax());
```

Hunk 18:

```rust
    fn macro_use(&self, node: &SyntaxNode) -> Option<(ast::Meta, ast::Attr)> {
```

becomes

```rust
    fn macro_use(&self, node: Node<'db>) -> Option<(ast::Meta<'db>, ast::Attr<'db>)> {
```

Hunk 19:

```rust
        on: InFile<&SyntaxNode>,
        meta: &ast::Meta,
        written: &ast::Attr,
```

becomes

```rust
        on: InFile<Node<'_>>,
        meta: &ast::Meta<'_>,
        written: &ast::Attr<'_>,
```

Hunk 20:

```rust
    fn macro_call(&mut self, t: &'a Tree, call: &ast::MacroCall, ctx: Ctx) {
        let cx = self.cx;
        let node = call.syntax();
        if let Some(path) = call.path()
            && let Some(mac) = cx.sema.resolve_macro_call(call)
        {
            let span = InFile::new(t.file, path.syntax().text_range());
            self.check_macro(mac, span, t.ptr(node), ctx.derive, || written(path.syntax()));
        }
        if let Some(expanded) = cx.sema.expand_macro_call(call)
```

becomes

```rust
    fn macro_call(&mut self, t: &'a Tree<'db>, call: ast::MacroCall<'db>, ctx: Ctx) {
        let cx = self.cx;
        let node = call.syntax();
        if let Some(path) = call.path()
            && let Some(mac) = cx.vsema.resolve_macro_call(call)
        {
            let span = InFile::new(t.file, path.syntax().text_range());
            self.check_macro(mac, span, t.ptr(node), ctx.derive, || written(path.syntax()));
        }
        if let Some(expanded) = cx.vsema.expand_macro_call(call)
```

Hunk 21:

```rust
    fn attr_macro(&mut self, t: &'a Tree, node: &SyntaxNode, ctx: Ctx) {
        let cx = self.cx;
        let Some(item) = cast::<ast::Item>(node) else { return };
        if let Some(mac) = cx.sema.resolve_attr_macro_call(&item) {
```

becomes

```rust
    fn attr_macro(&mut self, t: &'a Tree<'db>, node: Node<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(item) = ast::Item::cast(node) else { return };
        if let Some(mac) = cx.vsema.resolve_attr_macro_call(item) {
```

Hunk 22:

```rust
        if let Some(expanded) = cx.sema.expand_attr_macro(&item)
```

becomes

```rust
        if let Some(expanded) = cx.vsema.expand_attr_macro(item)
```

Hunk 23:

```rust
    fn derives(&mut self, t: &'a Tree, j: usize, ctx: Ctx) {
        if !t.attrs[j] {
            return;
        }
        let node = &t.nodes[j];
        let cfg = self.cfg;
        attr::each::<()>(node, cfg, |meta, written| {
            if attr::is(&meta, "derive") {
                self.derive(t, node, &meta, &written, ctx);
```

becomes

```rust
    fn derives(&mut self, t: &'a Tree<'db>, j: usize, ctx: Ctx) {
        if !t.attrs[j] {
            return;
        }
        let node = t.nodes[j];
        let cfg = self.cfg;
        attr::each::<()>(node, cfg, |meta, written| {
            if attr::is(&meta, "derive") {
                self.derive(t, node, meta, written, ctx);
```

Hunk 24:

```rust
    fn derive(&mut self, t: &'a Tree, node: &SyntaxNode, meta: &ast::Meta, written: &ast::Attr, ctx: Ctx) {
        let cx = self.cx;
        let macros = cx.sema.resolve_derive_macro(meta).unwrap_or_default();
```

becomes

```rust
    fn derive(&mut self, t: &'a Tree<'db>, node: Node<'db>, meta: ast::Meta<'db>, written: ast::Attr<'db>, ctx: Ctx) {
        let cx = self.cx;
        let macros = cx.vsema.resolve_derive_macro(meta).unwrap_or_default();
```

Hunk 25:

```rust
        let Some(derived) = cx.sema.expand_derive_macro(meta) else { return };
        for root in derived.into_iter().flatten() {
            let file = cx.sema.hir_file_for(&root.value);
```

becomes

```rust
        let Some(derived) = cx.vsema.expand_derive_macro(meta) else { return };
        for root in derived.into_iter().flatten() {
            let file = cx.vsema.hir_file_for(root.value);
```

Hunk 26:

```rust
    fn uses_at(&mut self, file: HirFileId, value: &SyntaxNode, ctx: Ctx) {
        let cx = self.cx;
        let db = cx.db;
        match value.kind() {
            SyntaxKind::PATH => {
                if value.parent().is_some_and(|it| matches!(it.kind(), SyntaxKind::PATH | SyntaxKind::USE_TREE)) {
                    return;
                }
                if let Some(path) = cast::<ast::Path>(value) {
                    let anchor_trait = is_anchor_trait(&path);
                    self.path(file, &path, ctx, anchor_trait, None);
                }
            }
            SyntaxKind::USE_TREE => {
                if let Some(tree) = cast::<ast::UseTree>(value) {
                    self.use_tree(file, &tree, ctx);
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(call) = cast::<ast::MethodCallExpr>(value) {
                    self.method_call(file, &call, ctx);
                }
            }
            SyntaxKind::FIELD_EXPR => {
                if let Some(field) = cast::<ast::FieldExpr>(value) {
                    self.field(file, &field, ctx);
                }
            }
            SyntaxKind::RECORD_EXPR_FIELD => {
                let Some(field) = cast::<ast::RecordExprField>(value) else { return };
                let Some((def, _, _)) = cx.sema.resolve_record_field(&field) else { return };
                if matches!(def.parent_def(db), hir::Variant::EnumVariant(_)) {
                    return;
                }
                self.field_use(file, value, Def::Field(def), ctx);
            }
            SyntaxKind::RECORD_PAT_FIELD => {
                let Some(field) = cast::<ast::RecordPatField>(value) else { return };
                let Some((def, _)) = cx.sema.resolve_record_pat_field(&field) else { return };
                self.field_use(file, value, Def::Field(def), ctx);
            }
            SyntaxKind::TUPLE_STRUCT_PAT => {
                if let Some(pat) = cast::<ast::TupleStructPat>(value) {
                    self.tuple_struct_pat(file, &pat, ctx);
                }
            }
            SyntaxKind::ASSOC_TYPE_ARG => {
                if let Some(arg) = cast::<ast::AssocTypeArg>(value) {
                    self.constraint(file, &arg, ctx);
                }
```

becomes

```rust
    fn uses_at(&mut self, file: HirFileId, value: Node<'db>, ctx: Ctx) {
        let cx = self.cx;
        let db = cx.db;
        match value.kind() {
            SyntaxKind::PATH => {
                if value.parent().is_some_and(|it| matches!(it.kind(), SyntaxKind::PATH | SyntaxKind::USE_TREE)) {
                    return;
                }
                if let Some(path) = ast::Path::cast(value) {
                    let anchor_trait = is_anchor_trait(path);
                    self.path(file, path, ctx, anchor_trait, None);
                }
            }
            SyntaxKind::USE_TREE => {
                if let Some(tree) = ast::UseTree::cast(value) {
                    self.use_tree(file, tree, ctx);
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(call) = ast::MethodCallExpr::cast(value) {
                    self.method_call(file, call, ctx);
                }
            }
            SyntaxKind::FIELD_EXPR => {
                if let Some(field) = ast::FieldExpr::cast(value) {
                    self.field(file, field, ctx);
                }
            }
            SyntaxKind::RECORD_EXPR_FIELD => {
                let Some(field) = ast::RecordExprField::cast(value) else { return };
                let Some((def, _, _)) = cx.vsema.resolve_record_field(field) else { return };
                if matches!(def.parent_def(db), hir::Variant::EnumVariant(_)) {
                    return;
                }
                self.field_use(file, value, Def::Field(def), ctx);
            }
            SyntaxKind::RECORD_PAT_FIELD => {
                let Some(field) = ast::RecordPatField::cast(value) else { return };
                let Some((def, _)) = cx.vsema.resolve_record_pat_field(field) else { return };
                self.field_use(file, value, Def::Field(def), ctx);
            }
            SyntaxKind::TUPLE_STRUCT_PAT => {
                if let Some(pat) = ast::TupleStructPat::cast(value) {
                    self.tuple_struct_pat(file, pat, ctx);
                }
            }
            SyntaxKind::ASSOC_TYPE_ARG => {
                if let Some(arg) = ast::AssocTypeArg::cast(value) {
                    self.constraint(file, arg, ctx);
                }
```

Hunk 27:

```rust
    fn path(
        &mut self,
        file: HirFileId,
        path: &ast::Path,
        ctx: Ctx,
```

becomes

```rust
    fn path(
        &mut self,
        file: HirFileId,
        path: ast::Path<'db>,
        ctx: Ctx,
```

Hunk 28 (`Option<Node<'_>>` is `Copy`):

```rust
        let Some(res) = res.or_else(|| cx.sema.resolve_path(path)) else { return };
        let (relative, qualifier_res) = match &qualifier {
            Some(qualifier) => type_relative(cx, qualifier, &res),
            None => (false, None),
        };
        let parent = path.syntax().parent();
        let parent_kind = parent.as_ref().map(SyntaxNode::kind);
```

becomes

```rust
        let Some(res) = res.or_else(|| cx.vsema.resolve_path(path)) else { return };
        let (relative, qualifier_res) = match qualifier {
            Some(qualifier) => type_relative(cx, qualifier, &res),
            None => (false, None),
        };
        let parent = path.syntax().parent();
        let parent_kind = parent.map(|it| it.kind());
```

Hunk 29:

```rust
                let at = parent.as_ref().unwrap_or(path.syntax());
                let used = Use {
                    def,
                    owner: ctx.owner,
                    at: InFile::new(file, SyntaxNodePtr::new(at)),
```

becomes

```rust
                let at = parent.unwrap_or(path.syntax());
                let used = Use {
                    def,
                    owner: ctx.owner,
                    at: InFile::new(file, ast::ptr(at)),
```

Hunk 30:

```rust
                self.path(file, &qualifier, ctx, stability_only, qualifier_res.flatten());
```

becomes

```rust
                self.path(file, qualifier, ctx, stability_only, qualifier_res.flatten());
```

Hunk 31:

```rust
            let res = resolved.take().unwrap_or_else(|| cx.sema.resolve_path(&qualifier));
            self.segment(&qualifier, res, &main, through, ctx);
```

becomes

```rust
            let res = resolved.take().unwrap_or_else(|| cx.vsema.resolve_path(qualifier));
            self.segment(qualifier, res, &main, through, ctx);
```

Hunk 32:

```rust
    fn segment(
        &mut self,
        qualifier: &ast::Path,
```

becomes

```rust
    fn segment(
        &mut self,
        qualifier: ast::Path<'db>,
```

Hunk 33:

```rust
    fn use_tree(&mut self, file: HirFileId, tree: &ast::UseTree, ctx: Ctx) {
```

becomes

```rust
    fn use_tree(&mut self, file: HirFileId, tree: ast::UseTree<'db>, ctx: Ctx) {
```

Hunk 34:

```rust
                self.use_path(file, tree, &path, None, ctx);
```

becomes

```rust
                self.use_path(file, tree, path, None, ctx);
```

Hunk 35:

```rust
            if let Some(PathResolution::Def(def)) = cx.sema.resolve_path(&path) {
                self.use_path(file, tree, &path, Some(Def::Item(def)), ctx);
            }
            return;
        }
        let Some(per_ns) = cx.sema.resolve_path_per_ns(&path) else { return };
```

becomes

```rust
            if let Some(PathResolution::Def(def)) = cx.vsema.resolve_path(path) {
                self.use_path(file, tree, path, Some(Def::Item(def)), ctx);
            }
            return;
        }
        let Some(per_ns) = cx.vsema.resolve_path_per_ns(path) else { return };
```

Hunk 36:

```rust
            self.use_path(file, tree, &path, Some(def), ctx);
```

becomes

```rust
            self.use_path(file, tree, path, Some(def), ctx);
```

Hunk 37:

```rust
    fn use_path(&mut self, file: HirFileId, tree: &ast::UseTree, path: &ast::Path, def: Option<Def>, ctx: Ctx) {
        let cx = self.cx;
        let def = match def {
            Some(def) => def,
            None => match cx.sema.resolve_path(path) {
```

becomes

```rust
    fn use_path(&mut self, file: HirFileId, tree: ast::UseTree<'db>, path: ast::Path<'db>, def: Option<Def>, ctx: Ctx) {
        let cx = self.cx;
        let def = match def {
            Some(def) => def,
            None => match cx.vsema.resolve_path(path) {
```

Hunk 38:

```rust
            at: InFile::new(file, SyntaxNodePtr::new(tree.syntax())),
```

becomes

```rust
            at: InFile::new(file, ast::ptr(tree.syntax())),
```

Hunk 39:

```rust
                let res = cx.sema.resolve_path(&qualifier);
                self.segment(&qualifier, res, &main, through, ctx);
```

becomes

```rust
                let res = cx.vsema.resolve_path(qualifier);
                self.segment(qualifier, res, &main, through, ctx);
```

Hunk 40:

```rust
    fn method_call(&mut self, file: HirFileId, call: &ast::MethodCallExpr, ctx: Ctx) {
        let cx = self.cx;
        let Some(name) = call.name_ref() else { return };
        let Some(function) = cx.sema.resolve_method_call(call) else { return };
        let range = name.syntax().text_range();
        let used = Use {
            def: Def::Item(trait_item(cx.db, ModuleDef::Function(function))),
            owner: ctx.owner,
            at: InFile::new(file, SyntaxNodePtr::new(call.syntax())),
```

becomes

```rust
    fn method_call(&mut self, file: HirFileId, call: ast::MethodCallExpr<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(name) = call.name_ref() else { return };
        let Some(function) = cx.vsema.resolve_method_call(call) else { return };
        let range = name.syntax().text_range();
        let used = Use {
            def: Def::Item(trait_item(cx.db, ModuleDef::Function(function))),
            owner: ctx.owner,
            at: InFile::new(file, ast::ptr(call.syntax())),
```

Hunk 41:

```rust
    fn field(&mut self, file: HirFileId, field: &ast::FieldExpr, ctx: Ctx) {
        let cx = self.cx;
        let Some(Either::Left(def)) = cx.sema.resolve_field(field) else { return };
        if !self.may_report(Def::Field(def)) {
            return;
        }
        let Some(scope) = cx.sema.scope(field.syntax()) else { return };
```

becomes

```rust
    fn field(&mut self, file: HirFileId, field: ast::FieldExpr<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(Either::Left(def)) = cx.vsema.resolve_field(field) else { return };
        if !self.may_report(Def::Field(def)) {
            return;
        }
        let Some(scope) = cx.vsema.scope(field.syntax()) else { return };
```

Hunk 42:

```rust
            at: InFile::new(file, SyntaxNodePtr::new(field.syntax())),
```

becomes

```rust
            at: InFile::new(file, ast::ptr(field.syntax())),
```

Hunk 43:

```rust
    fn field_use(&mut self, file: HirFileId, node: &SyntaxNode, def: Def, ctx: Ctx) {
        let range = node.text_range();
        let used = Use {
            def,
            owner: ctx.owner,
            at: InFile::new(file, SyntaxNodePtr::new(node)),
```

becomes

```rust
    fn field_use(&mut self, file: HirFileId, node: Node<'_>, def: Def, ctx: Ctx) {
        let range = node.text_range();
        let used = Use {
            def,
            owner: ctx.owner,
            at: InFile::new(file, ast::ptr(node)),
```

Hunk 44:

```rust
    fn tuple_struct_pat(&mut self, file: HirFileId, pat: &ast::TupleStructPat, ctx: Ctx) {
        let cx = self.cx;
        let Some(fields) = cx.sema.resolve_tuple_struct_pat_fields(pat) else { return };
        // alloc: inline up to 8 subpatterns
        let subpats: SmallVec<[ast::Pat; 8]> = pat.fields().collect();
```

becomes

```rust
    fn tuple_struct_pat(&mut self, file: HirFileId, pat: ast::TupleStructPat<'db>, ctx: Ctx) {
        let cx = self.cx;
        let Some(fields) = cx.vsema.resolve_tuple_struct_pat_fields(pat) else { return };
        // alloc: inline up to 8 subpatterns
        let subpats: SmallVec<[ast::Pat<'db>; 8]> = pat.fields().collect();
```

Hunk 45:

```rust
    fn constraint(&mut self, file: HirFileId, arg: &ast::AssocTypeArg, ctx: Ctx) {
```

becomes

```rust
    fn constraint(&mut self, file: HirFileId, arg: ast::AssocTypeArg<'db>, ctx: Ctx) {
```

Hunk 46:

```rust
        let Some(PathResolution::Def(ModuleDef::Trait(trait_))) = cx.sema.resolve_path(&path) else { return };
```

becomes

```rust
        let Some(PathResolution::Def(ModuleDef::Trait(trait_))) = cx.vsema.resolve_path(path) else { return };
```

Hunk 47:

```rust
            at: InFile::new(file, SyntaxNodePtr::new(arg.syntax())),
```

becomes

```rust
            at: InFile::new(file, ast::ptr(arg.syntax())),
```

Hunk 48 (the casts above call `N::cast` on the view itself):

```rust
/// `node` as an `N`, if it is one.
fn cast<N: AstNode>(node: &SyntaxNode) -> Option<N> {
    // clone: a cursor's refcount, which the cast takes by value
    N::cast(node.clone())
}

/// The index of the first node of `t` from `from` on that the walk looks at, or `t`'s end.
fn next(t: &Tree, from: usize) -> usize {
```

becomes

```rust
/// The index of the first node of `t` from `from` on that the walk looks at, or `t`'s end.
fn next(t: &Tree<'_>, from: usize) -> usize {
```

Hunk 49:

```rust
fn past(t: &Tree, from: usize, node: &SyntaxNode) -> usize {
    let range = node.text_range();
    let mut at = next(t, from);
    while at < t.nodes.len() && inside(node, range, &t.nodes[at]) {
```

becomes

```rust
fn past(t: &Tree<'_>, from: usize, node: Node<'_>) -> usize {
    let range = node.text_range();
    let mut at = next(t, from);
    while at < t.nodes.len() && inside(node, range, t.nodes[at]) {
```

Hunk 50:

```rust
fn inside(parent: &SyntaxNode, outer: TextRange, node: &SyntaxNode) -> bool {
    let it = node.text_range();
    outer.start() <= it.start()
        && (it.start() < outer.end()
            || (it.is_empty() && it.start() == outer.end() && node.ancestors().any(|it| it == *parent)))
```

becomes

```rust
fn inside<'t>(parent: Node<'t>, outer: TextRange, node: Node<'t>) -> bool {
    let it = node.text_range();
    outer.start() <= it.start()
        && (it.start() < outer.end()
            || (it.is_empty() && it.start() == outer.end() && node.ancestors().any(|it| it == parent)))
```

Hunk 51:

```rust
fn replaced_in<'t>(t: &'t Tree, parent: Option<&SyntaxNode>) -> &'t [SyntaxNode] {
```

becomes

```rust
fn replaced_in<'t, 'db>(t: &'t Tree<'db>, parent: Option<Node<'_>>) -> &'t [Node<'db>] {
```

Hunk 52:

```rust
fn in_body(node: &SyntaxNode, ctx: Ctx) -> bool {
```

becomes

```rust
fn in_body(node: Node<'_>, ctx: Ctx) -> bool {
```

Hunk 53:

```rust
    qualifier: &ast::Path,
    res: &PathResolution<'db>,
) -> (bool, Option<Option<PathResolution<'db>>>) {
    match qualifier.segment().and_then(|it| it.kind()) {
        Some(ast::PathSegmentKind::Type { trait_ref, .. }) => (trait_ref.is_none(), None),
        Some(ast::PathSegmentKind::SelfTypeKw) if qualifier.qualifier().is_none() => (true, None),
        _ => {
            let resolved = cx.sema.resolve_path(qualifier);
```

becomes

```rust
    qualifier: ast::Path<'db>,
    res: &PathResolution<'db>,
) -> (bool, Option<Option<PathResolution<'db>>>) {
    match qualifier.segment().and_then(|it| it.kind()) {
        Some(ast::PathSegmentKind::Type { trait_ref, .. }) => (trait_ref.is_none(), None),
        Some(ast::PathSegmentKind::SelfTypeKw) if qualifier.qualifier().is_none() => (true, None),
        _ => {
            let resolved = cx.vsema.resolve_path(qualifier);
```

Hunk 54:

```rust
fn is_anchor_trait(path: &ast::Path) -> bool {
```

becomes

```rust
fn is_anchor_trait(path: ast::Path<'_>) -> bool {
```

Hunk 55:

```rust
fn written(node: &SyntaxNode) -> String {
```

becomes

```rust
fn written(node: Node<'_>) -> String {
```

Hunk 56:

```rust
fn derive_ranges(attr: &ast::Attr) -> SmallVec<[TextRange; 4]> {
```

becomes

```rust
fn derive_ranges(attr: ast::Attr<'_>) -> SmallVec<[TextRange; 4]> {
```

Hunk 57:

```rust
fn derive_text(attr: &ast::Attr, range: TextRange) -> String {
```

becomes

```rust
fn derive_text(attr: ast::Attr<'_>, range: TextRange) -> String {
```

Hunk 58:

```rust
fn item_target(item: &ast::Item, container: Container) -> Option<Target> {
```

becomes

```rust
fn item_target(item: ast::Item<'_>, container: Container) -> Option<Target> {
```

> forwarded to diagnostics-lints-names-hot-stability (window of that name); told by tmux (requests, pass 26).

Hunk 59 (the impl is a view by value; the walk's nodes and the view `Semantics`):

```rust
    fn ineffective_unstable(&mut self, t: &'a Tree, j: usize, imp: &ast::Impl, owner: u32) {
        let cx = self.cx;
        let node = &t.nodes[j];
        const INEFFECTIVE: LintId = LintId::named("ineffective_unstable_trait_impl");
        if !self.index.staged(cx.krate) || !cx.lint_may_fire(INEFFECTIVE) {
            return;
        }
        let Some((Stability::Unstable { feature, .. }, range)) = attr::stability_written(node, self.cfg) else {
            return;
        };
        if attr::unstable_feature_bound(node, self.cfg, &feature) {
            return;
        }
        let (Some(ast::Type::PathType(trait_ty)), Some(self_ty)) = (imp.trait_(), imp.self_ty()) else { return };
        let Some(trait_path) = trait_ty.path() else { return };
        if !matches!(cx.sema.resolve_path(&trait_path), Some(PathResolution::Def(ModuleDef::Trait(_)))) {
            return;
        }
        let mut header
```

becomes

```rust
    fn ineffective_unstable(&mut self, t: &'a Tree<'db>, j: usize, imp: ast::Impl<'db>, owner: u32) {
        let cx = self.cx;
        let node = t.nodes[j];
        const INEFFECTIVE: LintId = LintId::named("ineffective_unstable_trait_impl");
        if !self.index.staged(cx.krate) || !cx.lint_may_fire(INEFFECTIVE) {
            return;
        }
        let Some((Stability::Unstable { feature, .. }, range)) = attr::stability_written(node, self.cfg) else {
            return;
        };
        if attr::unstable_feature_bound(node, self.cfg, &feature) {
            return;
        }
        let (Some(ast::Type::PathType(trait_ty)), Some(self_ty)) = (imp.trait_(), imp.self_ty()) else { return };
        let Some(trait_path) = trait_ty.path() else { return };
        if !matches!(cx.vsema.resolve_path(trait_path), Some(PathResolution::Def(ModuleDef::Trait(_)))) {
            return;
        }
        let mut header
```

Hunk 60 (each node read by value; `ast::X::cast` in place of the removed helper):

```rust
    fn header_nodes(&mut self, t: &'a Tree, from: usize, range: Option<TextRange>, header: &mut Header) {
        let walk = self.walk;
        let mut k = from;
        while header.stable && k < t.nodes.len() {
            let node = &t.nodes[k];
            if range.is_some_and(|it| node.text_range().start() >= it.end()) {
                return;
            }
            match t.kinds[k] {
                SyntaxKind::ATTR => {
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::CONST_ARG => {
                    self.const_arg(t, node, header);
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::MACRO_CALL => {
                    if let Some(call) = cast::<ast::MacroCall>(node)
                        && let Some(expanded) = self.cx.sema.expand_macro_call(&call)
                    {
                        match walk.tree(expanded.file_id) {
                            Some(tree) => self.header_nodes(tree, 0, None, header),
                            // The walk of the impl reports that the crate walk has no tree of it.
                            None => header.stable = false,
                        }
                    }
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::PATH if node.parent().is_none_or(|it| it.kind() != SyntaxKind::PATH) => {
                    if let Some(path) = cast::<ast::Path>(node) {
                        self.header_path(t, &path, None, header);
                    }
                }
                SyntaxKind::FN_PTR_TYPE => {
                    if cast::<ast::FnPtrType>(node).is_some_and(|it| unstable_abi(&it)) {
                        header.stable = false;
                    }
                }
```

becomes

```rust
    fn header_nodes(&mut self, t: &'a Tree<'db>, from: usize, range: Option<TextRange>, header: &mut Header) {
        let walk = self.walk;
        let mut k = from;
        while header.stable && k < t.nodes.len() {
            let node = t.nodes[k];
            if range.is_some_and(|it| node.text_range().start() >= it.end()) {
                return;
            }
            match t.kinds[k] {
                SyntaxKind::ATTR => {
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::CONST_ARG => {
                    self.const_arg(t, node, header);
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::MACRO_CALL => {
                    if let Some(call) = ast::MacroCall::cast(node)
                        && let Some(expanded) = self.cx.vsema.expand_macro_call(call)
                    {
                        match walk.tree(expanded.file_id) {
                            Some(tree) => self.header_nodes(tree, 0, None, header),
                            // The walk of the impl reports that the crate walk has no tree of it.
                            None => header.stable = false,
                        }
                    }
                    k = after(t, k);
                    continue;
                }
                SyntaxKind::PATH if node.parent().is_none_or(|it| it.kind() != SyntaxKind::PATH) => {
                    if let Some(path) = ast::Path::cast(node) {
                        self.header_path(t, path, None, header);
                    }
                }
                SyntaxKind::FN_PTR_TYPE => {
                    if ast::FnPtrType::cast(node).is_some_and(unstable_abi) {
                        header.stable = false;
                    }
                }
```

Hunk 61 (the path is a view by value):

```rust
    fn header_path(&mut self, t: &Tree, path: &ast::Path, res: Option<PathResolution<'db>>, header: &mut Header) {
        let cx = self.cx;
        let qualifier = path.qualifier();
        if qualifier.is_none()
            && path.segment().and_then(|it| it.kind()).is_some_and(|it| matches!(it, ast::PathSegmentKind::SelfTypeKw))
        {
            header.stable = false;
            return;
        }
        if is_anchor_trait(path) {
            return;
        }
        let Some(res) = res.or_else(|| cx.sema.resolve_path(path)) else { return };
        if let Some(qualifier) = &qualifier {
```

becomes

```rust
    fn header_path(&mut self, t: &Tree<'db>, path: ast::Path<'db>, res: Option<PathResolution<'db>>, header: &mut Header) {
        let cx = self.cx;
        let qualifier = path.qualifier();
        if qualifier.is_none()
            && path.segment().and_then(|it| it.kind()).is_some_and(|it| matches!(it, ast::PathSegmentKind::SelfTypeKw))
        {
            header.stable = false;
            return;
        }
        if is_anchor_trait(path) {
            return;
        }
        let Some(res) = res.or_else(|| cx.vsema.resolve_path(path)) else { return };
        if let Some(qualifier) = qualifier {
```

Hunk 62 (the const argument is a view by value):

```rust
    fn const_arg(&mut self, t: &Tree, node: &SyntaxNode, header: &mut Header) {
```

becomes

```rust
    fn const_arg(&mut self, t: &Tree<'_>, node: Node<'_>, header: &mut Header) {
```

Hunk 63:

```rust
fn after(t: &Tree, k: usize) -> usize {
```

becomes

```rust
fn after(t: &Tree<'_>, k: usize) -> usize {
```

Hunk 64:

```rust
fn unstable_abi(ty: &ast::FnPtrType) -> bool {
```

becomes

```rust
fn unstable_abi(ty: ast::FnPtrType<'_>) -> bool {
```
