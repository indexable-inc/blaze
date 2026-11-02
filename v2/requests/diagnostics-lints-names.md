# Requests to diagnostics-lints-names

> diagnostics (lane parent, window 67, 2026-10-29): **granted.** `diagnostics-lints-names` is done, so the
> authors of the `## frontend-tree: ...: onto views` headings in this file apply them directly
> (frontend-tree-move-diag-lints-names, -uses and the rest of frontend-tree-move-diag-lints'
> children). The terms are those under "## frontend-tree: blaze-diagnostics cx.rs: onto views (1)"
> in `v2/requests/diagnostics.md`: after that heading and `lints/syntax.rs (1)`, a function at a time,
> re-reading first, keeping the hot-lint comments. Mark each heading `> applied by <window>`.
> Live in these paths: `diagnostics-lints-names-stability-walk` (`stability/walk.rs`) and
> `diagnostics-hot3` (`imports/used.rs` `Used::derive`).

> diagnostics-lints-names (2026-10-29 21:55): **not applied yet.** Of what the
> `lints/names/dead/*` headings need, only `hir_expand::attrs::expand_cfg_attr_view` is in the
> tree. These are still missing:
> - `Cx::vsema` and `Cx::syntax_view`;
> - `cx::{item_range_view, def_range_view, signature_range_view}`;
> - the view `Semantics`' `lint_attrs`, `expand_macro_call` and `resolve_offset_of_field`.
>
> Once they land, the authors may apply their own headings to my paths as exact edits, rebased
> on the current text, and mark each one applied. Two `lints/names/items*` headings are stale:
> - `items/walk.rs` was written against the old text. Each `Item`'s own nodes are now one shared
>   column (`column` and `own`, read through `Item::nodes()`) instead of `nodes: Vec<..>`.
> - `walk::attrs_named` is now `walk::any_named`, used by `docs.rs`.

## frontend-tree: blaze-diagnostics lints/names/dead/level.rs: onto views (1)

From `frontend-tree-move-diag-lints-names` (frontend.md item 16, `docs/v2/plan/lanes/frontend/tree-move-diag-lints-names.md`).
The level of `dead_code` at a node, walked up over views: no handle, count or clone per ancestor.

Needs first: `Cx::vsema` (`hir::view::Semantics<'db>`, with `lint_attrs(FileId, Crate,
ast::AnyHasAttrs<'db>)` and `file_to_module_def`) and `hir::Module::declaration_source_view`
(frontend-tree-move-hir / -diag-lower), `MacroCallId::call_node_view` (frontend-tree-move-expand).
Lands with every other `lints/names/dead/*` heading in this file (`Levels::at` takes a view, and
`defs::cast` goes).

`crates/blaze-diagnostics/src/lints/names/dead/level.rs`, hunk 1 (imports; `defs::cast` is gone):

```rust
use syntax::{AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, ast};

use super::defs::cast;
use crate::{
```

```rust
use syntax::{
    SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use crate::{
```

Hunk 2 (`at` takes the view):

```rust
    pub(super) fn at(&self, cx: &Cx<'_, '_>, node: InFile<&SyntaxNode>) -> LevelPlus {
```

```rust
    pub(super) fn at<'db>(&self, cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> LevelPlus {
```

Hunk 3 (`state`: the ancestors are views; `ancestors()` starts at the node, so `top` starts there):

```rust
    fn state(&self, cx: &Cx<'_, '_>, node: InFile<&SyntaxNode>) -> State {
        let mut pending: SmallVec<[(SyntaxNodePtr, ast::AnyHasAttrs); 4]> = SmallVec::new();
        let mut base = None;
        let mut top = None;
        for ancestor in node.value.ancestors() {
            if let Some(owner) = cast::<ast::AnyHasAttrs>(&ancestor)
                && ast::attrs_including_inner(&owner).next().is_some()
            {
                let ptr = SyntaxNodePtr::new(&ancestor);
                if let Some(&state) = self.owners.borrow().get(&node.with_value(ptr)) {
                    base = Some(state);
                    break;
                }
                pending.push((ptr, owner));
            }
            top = Some(ancestor);
        }
        let mut state = match base {
            Some(it) => it,
            None => self.above_file(cx, node.file_id, top.as_ref().unwrap_or(node.value)),
        };
```

```rust
    fn state<'db>(&self, cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> State {
        let mut pending: SmallVec<[(SyntaxNodePtr, ast::AnyHasAttrs<'db>); 4]> = SmallVec::new();
        let mut base = None;
        let mut top = node.value;
        for ancestor in node.value.ancestors() {
            if let Some(owner) = ast::AnyHasAttrs::cast(ancestor)
                && ast::attrs_including_inner(&owner).next().is_some()
            {
                let ptr = ast::ptr(ancestor);
                if let Some(&state) = self.owners.borrow().get(&node.with_value(ptr)) {
                    base = Some(state);
                    break;
                }
                pending.push((ptr, owner));
            }
            top = ancestor;
        }
        let mut state = match base {
            Some(it) => it,
            None => self.above_file(cx, node.file_id, top),
        };
```

Hunk 4 (`above_file`):

```rust
    fn above_file(&self, cx: &Cx<'_, '_>, file: HirFileId, top: &SyntaxNode) -> State {
        if let Some(&state) = self.files.borrow().get(&file) {
            return state;
        }
        let state = match above(cx, file, top) {
            Some(node) => self.state(cx, node.as_ref()),
```

```rust
    fn above_file<'db>(&self, cx: &Cx<'_, 'db>, file: HirFileId, top: ast::Node<'db>) -> State {
        if let Some(&state) = self.files.borrow().get(&file) {
            return state;
        }
        let state = match above(cx, file, top) {
            Some(node) => self.state(cx, node),
```

Hunk 5 (`above`: the call's node and the module's declaration as views; `lint_attrs` is given the
file, so the trees need not be `cx.vsema`'s):

```rust
fn above(cx: &Cx<'_, '_>, file: HirFileId, top: &SyntaxNode) -> Option<InFile<SyntaxNode>> {
    if let Some(call) = file.macro_file() {
        let node = call.call_node(cx.db);
```

```rust
fn above<'db>(cx: &Cx<'_, 'db>, file: HirFileId, top: ast::Node<'db>) -> Option<InFile<ast::Node<'db>>> {
    if let Some(call) = file.macro_file() {
        let node = call.call_node_view(cx.db);
```

Hunk 6:

```rust
    let module = cx.sema.file_to_module_def(file.file_id(cx.db))?;
    if module.is_crate_root(cx.db) {
        return None;
    }
    let declaration = module.declaration_source(cx.db)?;
    Some(declaration.map(|it| it.syntax().clone())) // clone: a refcount, once per file
}
```

```rust
    let module = cx.vsema.file_to_module_def(file.file_id(cx.db))?;
    if module.is_crate_root(cx.db) {
        return None;
    }
    let declaration = module.declaration_source_view(cx.db)?;
    Some(declaration.map(|it| it.syntax()))
}
```

Hunk 7 (`apply` takes the view owner):

```rust
fn apply(cx: &Cx<'_, '_>, owner: InFile<ast::AnyHasAttrs>, ptr: SyntaxNodePtr, state: &mut State) {
    let file = owner.file_id.original_file(cx.db).file_id(cx.db);
    let ptr = owner.with_value(ptr);
    for (index, (attr, name)) in cx.sema.lint_attrs(file, cx.krate, owner.value).enumerate() {
```

```rust
fn apply<'db>(cx: &Cx<'_, 'db>, owner: InFile<ast::AnyHasAttrs<'db>>, ptr: SyntaxNodePtr, state: &mut State) {
    let file = owner.file_id.original_file(cx.db).file_id(cx.db);
    let ptr = owner.with_value(ptr);
    for (index, (attr, name)) in cx.vsema.lint_attrs(file, cx.krate, owner.value).enumerate() {
```

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 under the grant (2026-10-29 22:30): every hunk, old text matched today's file exactly.

## frontend-tree: blaze-diagnostics lints/names/dead/defs.rs: onto views (1)

From `frontend-tree-move-diag-lints-names`. The crate walk that numbers every definition, over
views: each step is arithmetic on the flat tree, with no count or clone per node; `cast` (a
handle clone behind a kind test) goes, since a view's `N::cast` is that test on a `Copy` node.

Needs first: `Cx::vsema` with `parse_or_expand`, `to_def` (the view `ToDef` of `Fn`, `Const`,
`Static`, `Struct`, `Union`, `Enum`, `Trait`, `TypeAlias`, `Impl`, `Variant`, `RecordField`,
`TupleField`, `MacroCall`), `is_attr_macro_call(ast::Item<'db>)`,
`expand_attr_macro(ast::Item<'db>)`; `hir::Module::definition_source_view` and
`hir::ModuleSourceView` (frontend-tree-move-hir / -def / -diag-lower);
`hir_expand::attrs::expand_cfg_attr_view` (frontend-tree-move-expand). Lands with every other
`lints/names/dead/*` heading here.

`crates/blaze-diagnostics/src/lints/names/dead/defs.rs`, hunk 1:

```rust
use hir::{HirFileId, InFile, ModuleSource, Name};
```

```rust
use hir::{HirFileId, InFile, ModuleSourceView, Name};
```

Hunk 2:

```rust
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, T,
    ast::{self, HasModuleItem},
};
```

```rust
use syntax::{
    SyntaxKind, SyntaxNodePtr, T,
    view::{self as ast, AstNode as _, HasModuleItem as _},
};
```

Hunk 3 (each module's items, found under the root `cx.vsema` handed out, as before: the view
`Semantics` finds a view's file by its tree):

```rust
            let source = module.definition_source(cx.db);
            let file = source.file_id;
            let root = cx.sema.parse_or_expand(file);
            let items = match &source.value {
                ModuleSource::SourceFile(it) => {
                    ast::SourceFile::cast(SyntaxNodePtr::new(it.syntax()).to_node(&root)).map(|it| it.items())
                }
                ModuleSource::Module(it) => ast::Module::cast(SyntaxNodePtr::new(it.syntax()).to_node(&root))
                    .and_then(|it| it.item_list())
                    .map(|it| it.items()),
                ModuleSource::BlockExpr(_) => continue,
            };
```

```rust
            let source = module.definition_source_view(cx.db);
            let file = source.file_id;
            let root = cx.vsema.parse_or_expand(file);
            let items = match source.value {
                ModuleSourceView::SourceFile(it) => {
                    ast::SourceFile::cast(ast::to_node(ast::ptr(it.syntax()), root)).map(|it| it.items())
                }
                ModuleSourceView::Module(it) => ast::Module::cast(ast::to_node(ast::ptr(it.syntax()), root))
                    .and_then(|it| it.item_list())
                    .map(|it| it.items()),
                ModuleSourceView::BlockExpr(_) => continue,
            };
```

Hunk 4 (`item`, by value):

```rust
impl Walk<'_, '_, '_> {
    fn item(&mut self, item: InFile<ast::Item>, container: Container, parent: Parent) {
        if !enabled(self.cx, item.value.syntax()) {
            return;
        }
        if let ast::Item::MacroCall(call) = &item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, parent);
            }
            return;
        }
        if self.cx.sema.is_attr_macro_call(item.as_ref()) {
            if let Some(expansion) = self.cx.sema.expand_attr_macro(&item.value) {
                self.expansion(expansion.value, container, parent);
            }
            return;
        }
        if let ast::Item::MacroDef(it) = &item.value
            && ast::HasVisibility::visibility(it).is_some()
        {
            self.unported = true;
        }
        let file = item.file_id;
        let def = self.define(&item, container, parent);
```

```rust
impl<'db> Walk<'_, '_, 'db> {
    fn item(&mut self, item: InFile<ast::Item<'db>>, container: Container, parent: Parent) {
        if !enabled(self.cx, item.value.syntax()) {
            return;
        }
        if let ast::Item::MacroCall(call) = item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, parent);
            }
            return;
        }
        if self.cx.vsema.is_attr_macro_call(item.value) {
            if let Some(expansion) = self.cx.vsema.expand_attr_macro(item.value) {
                self.expansion(expansion.value, container, parent);
            }
            return;
        }
        if let ast::Item::MacroDef(it) = item.value
            && ast::HasVisibility::visibility(&it).is_some()
        {
            self.unported = true;
        }
        let file = item.file_id;
        let def = self.define(item, container, parent);
```

Hunk 5:

```rust
    fn expansion(&mut self, root: InFile<SyntaxNode>, container: Container, parent: Parent) {
        for child in root.value.children() {
            if is_item(&child)
```

```rust
    fn expansion(&mut self, root: InFile<ast::Node<'db>>, container: Container, parent: Parent) {
        for child in root.value.children() {
            if is_item(child)
```

Hunk 6:

```rust
    fn node(&mut self, file: HirFileId, node: SyntaxNode, parent: Parent) {
        let kind = node.kind();
        if ast::AnyHasAttrs::can_cast(kind) && !enabled(self.cx, &node) {
            return;
        }
        if is_item(&node) {
            if kind == SyntaxKind::ASM_EXPR {
                self.global_asm(&node);
            } else if let Some(item) = ast::Item::cast(node) {
                self.item(InFile::new(file, item), Container::Module, parent);
            }
            return;
        }
        if kind == SyntaxKind::MACRO_CALL {
            let Some(call) = ast::MacroCall::cast(node) else { return };
            let Some(expansion) = self.expand(InFile::new(file, &call)) else { return };
```

```rust
    fn node(&mut self, file: HirFileId, node: ast::Node<'db>, parent: Parent) {
        let kind = node.kind();
        if ast::AnyHasAttrs::can_cast(kind) && !enabled(self.cx, node) {
            return;
        }
        if is_item(node) {
            if kind == SyntaxKind::ASM_EXPR {
                self.global_asm(node);
            } else if let Some(item) = ast::Item::cast(node) {
                self.item(InFile::new(file, item), Container::Module, parent);
            }
            return;
        }
        if kind == SyntaxKind::MACRO_CALL {
            let Some(call) = ast::MacroCall::cast(node) else { return };
            let Some(expansion) = self.expand(InFile::new(file, call)) else { return };
```

Hunk 7:

```rust
        let parent = match is_anon(&node) {
            true => Parent::Anon(anon_key(file, &node)),
```

```rust
        let parent = match is_anon(node) {
            true => Parent::Anon(anon_key(file, node)),
```

Hunk 8 (`to_def2` took the file with the node; the view `Semantics` finds it from the tree):

```rust
    fn global_asm(&mut self, node: &SyntaxNode) {
        let names = node.descendants().any(|it| matches!(it.kind(), SyntaxKind::ASM_SYM | SyntaxKind::ASM_CONST));
        self.unported |= names;
    }

    fn expand(&self, call: InFile<&ast::MacroCall>) -> Option<InFile<SyntaxNode>> {
        let id = self.cx.sema.to_def2(call)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.sema.parse_or_expand(file)))
    }
```

```rust
    fn global_asm(&mut self, node: ast::Node<'_>) {
        let names = node.descendants().any(|it| matches!(it.kind(), SyntaxKind::ASM_SYM | SyntaxKind::ASM_CONST));
        self.unported |= names;
    }

    fn expand(&self, call: InFile<ast::MacroCall<'db>>) -> Option<InFile<ast::Node<'db>>> {
        let id = self.cx.vsema.to_def(call.value)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.vsema.parse_or_expand(file)))
    }
```

Hunk 9 (`define`):

```rust
    fn define(&mut self, item: &InFile<ast::Item>, container: Container, parent: Parent) -> Option<DefIdx> {
        let cx = self.cx;
        let db = cx.db;
        let node = Some(item.as_ref().map(|it| SyntaxNodePtr::new(it.syntax())));
```

```rust
    fn define(&mut self, item: InFile<ast::Item<'db>>, container: Container, parent: Parent) -> Option<DefIdx> {
        let cx = self.cx;
        let db = cx.db;
        let node = Some(item.map(|it| ast::ptr(it.syntax())));
```

Hunk 10 (`define`'s match, each `to_def2(item.with_value(it))` the view's `to_def(it)`):

```rust
        let (def, adt) = match &item.value {
            ast::Item::Fn(it) => {
                let function = cx.sema.to_def2(item.with_value(it))?;
```

```rust
        let (def, adt) = match item.value {
            ast::Item::Fn(it) => {
                let function = cx.vsema.to_def(it)?;
```

Hunk 11:

```rust
            ast::Item::Const(it) => {
                let konst = cx.sema.to_def2(item.with_value(it))?;
```

```rust
            ast::Item::Const(it) => {
                let konst = cx.vsema.to_def(it)?;
```

Hunk 12:

```rust
            ast::Item::Static(it) => {
                let it = cx.sema.to_def2(item.with_value(it))?;
```

```rust
            ast::Item::Static(it) => {
                let it = cx.vsema.to_def(it)?;
```

Hunk 13:

```rust
            ast::Item::Struct(it) => {
                let it = cx.sema.to_def2(item.with_value(it))?;
```

```rust
            ast::Item::Struct(it) => {
                let it = cx.vsema.to_def(it)?;
```

Hunk 14:

```rust
            ast::Item::Union(it) => {
                let it = cx.sema.to_def2(item.with_value(it))?;
```

```rust
            ast::Item::Union(it) => {
                let it = cx.vsema.to_def(it)?;
```

Hunk 15:

```rust
            ast::Item::Enum(it) => {
                let it = cx.sema.to_def2(item.with_value(it))?;
```

```rust
            ast::Item::Enum(it) => {
                let it = cx.vsema.to_def(it)?;
```

Hunk 16:

```rust
                let kind = if it.eq_token().is_some() { Kind::TraitAlias } else { Kind::Trait };
                let it = cx.sema.to_def2(item.with_value(it))?;
```

```rust
                let kind = if it.eq_token().is_some() { Kind::TraitAlias } else { Kind::Trait };
                let it = cx.vsema.to_def(it)?;
```

Hunk 17:

```rust
            ast::Item::TypeAlias(it) => {
                let it = cx.sema.to_def2(item.with_value(it))?;
```

```rust
            ast::Item::TypeAlias(it) => {
                let it = cx.vsema.to_def(it)?;
```

Hunk 18:

```rust
                let id = ImplId::try_from(cx.sema.to_def2(item.with_value(it))?).ok()?;
```

```rust
                let id = ImplId::try_from(cx.vsema.to_def(it)?).ok()?;
```

Hunk 19:

```rust
            ast::Item::Use(_) => {
                let ptr = item.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
```

```rust
            ast::Item::Use(_) => {
                let ptr = item.map(|it| ast::ptr(it.syntax()));
```

Hunk 20 (`fields_and_variants`, by value):

```rust
    fn fields_and_variants(&mut self, item: &InFile<ast::Item>, adt: DefIdx) {
        let fields = match &item.value {
```

```rust
    fn fields_and_variants(&mut self, item: InFile<ast::Item<'db>>, adt: DefIdx) {
        let fields = match item.value {
```

Hunk 21:

```rust
                    let Some(id) = self.cx.sema.to_def2(item.with_value(&variant)) else { continue };
                    let index = self.push(Def {
                        id: Id::Variant(id.into()),
                        kind: Kind::Variant,
                        node: Some(item.with_value(SyntaxNodePtr::new(variant.syntax()))),
```

```rust
                    let Some(id) = self.cx.vsema.to_def(variant) else { continue };
                    let index = self.push(Def {
                        id: Id::Variant(id.into()),
                        kind: Kind::Variant,
                        node: Some(item.with_value(ast::ptr(variant.syntax()))),
```

Hunk 22 (`fields`, `field`):

```rust
    fn fields(&mut self, file: HirFileId, list: Option<ast::FieldList>, owner: DefIdx) {
        match list {
            Some(ast::FieldList::RecordFieldList(it)) => {
                for field in it.fields() {
                    if enabled(self.cx, field.syntax()) {
                        let def = self.cx.sema.to_def2(InFile::new(file, &field));
                        self.field(file, field.syntax(), def, owner);
                    }
                }
            }
            Some(ast::FieldList::TupleFieldList(it)) => {
                for field in it.fields() {
                    if enabled(self.cx, field.syntax()) {
                        let def = self.cx.sema.to_def2(InFile::new(file, &field));
                        self.field(file, field.syntax(), def, owner);
                    }
                }
            }
            None => {}
        }
    }

    fn field(&mut self, file: HirFileId, node: &SyntaxNode, field: Option<hir::Field>, owner: DefIdx) {
        let Some(field) = field else { return };
        let index = self.push(Def {
            id: Id::Field(FieldId::from(field)),
            kind: Kind::Field,
            node: Some(InFile::new(file, SyntaxNodePtr::new(node))),
```

```rust
    fn fields(&mut self, file: HirFileId, list: Option<ast::FieldList<'db>>, owner: DefIdx) {
        match list {
            Some(ast::FieldList::RecordFieldList(it)) => {
                for field in it.fields() {
                    if enabled(self.cx, field.syntax()) {
                        let def = self.cx.vsema.to_def(field);
                        self.field(file, field.syntax(), def, owner);
                    }
                }
            }
            Some(ast::FieldList::TupleFieldList(it)) => {
                for field in it.fields() {
                    if enabled(self.cx, field.syntax()) {
                        let def = self.cx.vsema.to_def(field);
                        self.field(file, field.syntax(), def, owner);
                    }
                }
            }
            None => {}
        }
    }

    fn field(&mut self, file: HirFileId, node: ast::Node<'_>, field: Option<hir::Field>, owner: DefIdx) {
        let Some(field) = field else { return };
        let index = self.push(Def {
            id: Id::Field(FieldId::from(field)),
            kind: Kind::Field,
            node: Some(InFile::new(file, ast::ptr(node))),
```

Hunk 23 (`is_item`, `is_anon`, `anon_key` on views; `cast` goes):

```rust
fn is_item(node: &SyntaxNode) -> bool {
```

```rust
fn is_item(node: ast::Node<'_>) -> bool {
```

Hunk 24:

```rust
pub(super) fn is_anon(node: &SyntaxNode) -> bool {
```

```rust
pub(super) fn is_anon(node: ast::Node<'_>) -> bool {
```

Hunk 25 (a view compares by value):

```rust
                    has_semi && parent.children().filter(|it| ast::Expr::can_cast(it.kind())).nth(1).as_ref() == Some(node)
```

```rust
                    has_semi && parent.children().filter(|it| ast::Expr::can_cast(it.kind())).nth(1) == Some(node)
```

Hunk 26:

```rust
pub(super) fn anon_key(file: HirFileId, node: &SyntaxNode) -> InFile<SyntaxNodePtr> {
    let inner = |node: &SyntaxNode| {
        (node.kind() == SyntaxKind::PAREN_EXPR)
            .then(|| node.children().find(|it| ast::Expr::can_cast(it.kind())))
            .flatten()
    };
    let mut ptr = SyntaxNodePtr::new(node);
    let mut next = inner(node);
    while let Some(node) = next {
        ptr = SyntaxNodePtr::new(&node);
        next = inner(&node);
    }
    InFile::new(file, ptr)
}

/// `node` as `N`, if it is one. rowan's `cast` takes the node by value, and a `SyntaxNode` clone
/// is a reference count, with no allocation; the kind is tested first, so a node of another
/// kind costs nothing.
pub(super) fn cast<N: AstNode>(node: &SyntaxNode) -> Option<N> {
    // clone: a refcount, no allocation; `cast` takes the node by value.
    if N::can_cast(node.kind()) { N::cast(node.clone()) } else { None }
}
```

```rust
pub(super) fn anon_key<'t>(file: HirFileId, node: ast::Node<'t>) -> InFile<SyntaxNodePtr> {
    let inner = |node: ast::Node<'t>| {
        (node.kind() == SyntaxKind::PAREN_EXPR)
            .then(|| node.children().find(|it| ast::Expr::can_cast(it.kind())))
            .flatten()
    };
    let mut ptr = ast::ptr(node);
    let mut next = inner(node);
    while let Some(node) = next {
        ptr = ast::ptr(node);
        next = inner(node);
    }
    InFile::new(file, ptr)
}
```

Hunk 27 (`enabled`: hir-expand's view `cfg_attr` expansion, `CfgExpr::parse_from_view`):

```rust
fn enabled(cx: &Cx<'_, '_>, node: &SyntaxNode) -> bool {
    if node.first_child().is_none_or(|it| it.kind() != SyntaxKind::ATTR) {
        return true;
    }
    let Some(owner) = cast::<ast::AnyHasAttrs>(node) else { return true };
    let cfg = cx.krate.cfg(cx.db);
    let off = hir_expand::attrs::expand_cfg_attr(
        ast::HasAttrs::attrs(&owner),
        || cfg,
        |meta, _| {
            if let ast::Meta::CfgMeta(meta) = meta
                && let Some(predicate) = meta.cfg_predicate()
                && cfg.check(&CfgExpr::parse_from_ast(predicate)) == Some(false)
```

```rust
fn enabled(cx: &Cx<'_, '_>, node: ast::Node<'_>) -> bool {
    if node.first_child().is_none_or(|it| it.kind() != SyntaxKind::ATTR) {
        return true;
    }
    let Some(owner) = ast::AnyHasAttrs::cast(node) else { return true };
    let cfg = cx.krate.cfg(cx.db);
    let off = hir_expand::attrs::expand_cfg_attr_view(
        ast::HasAttrs::attrs(&owner),
        || cfg,
        |meta, _| {
            if let ast::Meta::CfgMeta(meta) = meta
                && let Some(predicate) = meta.cfg_predicate()
                && cfg.check(&CfgExpr::parse_from_view(predicate)) == Some(false)
```

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 under the grant (2026-10-29 22:30): every hunk, old text matched today's file exactly.

## frontend-tree: blaze-diagnostics lints/names/dead/seed.rs: onto views (1)

From `frontend-tree-move-diag-lints-names`. The nodes a level is asked at are views; the crate
root's file is read without building its source handle; `#![crate_type]`'s value is lent from
the node's text, where two `String`s were built.

Needs first: `Cx::syntax_view`, `Cx::vsema` (frontend-tree-move-diag-lower), and
`crate::checks::crate_attrs` giving view `ast::Meta<'db>`s (`checks.rs`, frontend-tree-move-diag-lower;
`meta.simple_name()` and `meta.syntax()` read the same on views). Lands with every other
`lints/names/dead/*` heading here.

`crates/blaze-diagnostics/src/lints/names/dead/seed.rs`, hunk 1:

```rust
use syntax::{AstNode, SyntaxNode};
```

```rust
use syntax::view::{self as ast, AstNode as _};
```

Hunk 2:

```rust
impl Crate<'_, '_, '_> {
    /// The node whose attributes set the level at `def`: its own, or for what a builtin derive
    /// makes (a sibling of its ADT in rustc's HIR), what the ADT is in.
    fn level_node(&self, def: DefIdx) -> Option<InFile<SyntaxNode>> {
        let defs = self.defs;
        if let Some(node) = defs[def].node {
            return Some(self.cx.syntax(node));
        }
```

```rust
impl<'db> Crate<'_, '_, 'db> {
    /// The node whose attributes set the level at `def`: its own, or for what a builtin derive
    /// makes (a sibling of its ADT in rustc's HIR), what the ADT is in.
    fn level_node(&self, def: DefIdx) -> Option<InFile<ast::Node<'db>>> {
        let defs = self.defs;
        if let Some(node) = defs[def].node {
            return Some(self.cx.syntax_view(node));
        }
```

Hunk 3:

```rust
        let node = self.cx.syntax(defs[adt].node?);
        let parent = node.value.parent()?;
```

```rust
        let node = self.cx.syntax_view(defs[adt].node?);
        let parent = node.value.parent()?;
```

Hunk 4:

```rust
            Some(node) => self.levels.at(self.cx, node.as_ref()),
            None => self.levels.at(self.cx, self.crate_root().as_ref()),
```

```rust
            Some(node) => self.levels.at(self.cx, node),
            None => self.levels.at(self.cx, self.crate_root()),
```

Hunk 5 (`definition_source_file_id` is the file `definition_source` gives, without its node):

```rust
    fn crate_root(&self) -> InFile<SyntaxNode> {
        let db = self.cx.db;
        let source = self.cx.krate.root_module(db).definition_source(db);
        let file: HirFileId = source.file_id;
        InFile::new(file, self.cx.sema.parse_or_expand(file))
    }
```

```rust
    fn crate_root(&self) -> InFile<ast::Node<'db>> {
        let db = self.cx.db;
        let file: HirFileId = self.cx.krate.root_module(db).definition_source_file_id(db);
        InFile::new(file, self.cx.vsema.parse_or_expand(file))
    }
```

Hunk 6:

```rust
            if test && self.cx.syntax(node).value.parent().is_some_and(|it| it.parent().is_none()) {
```

```rust
            if test && self.cx.syntax_view(node).value.parent().is_some_and(|it| it.parent().is_none()) {
```

Hunk 7 (`string_value`: a view's text is one `&str`; the value is a slice of it):

```rust
fn string_value(node: &SyntaxNode) -> Option<String> {
    let text = node.text().to_string(); // alloc: setup, a `#![crate_type]`, once per crate
    let start = text.find('"')?;
    let end = text.rfind('"')?;
    (end > start).then(|| text[start + 1..end].to_owned()) // alloc: setup, as above
}
```

```rust
fn string_value(node: ast::Node<'_>) -> Option<&str> {
    let text = node.text();
    let start = text.find('"')?;
    let end = text.rfind('"')?;
    (end > start).then(|| &text[start + 1..end])
}
```

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 under the grant (2026-10-29 22:30): every hunk, old text matched today's file exactly.

## frontend-tree: blaze-diagnostics lints/names/dead/report.rs: onto views (1)

From `frontend-tree-move-diag-lints-names`. A dead item's name and header spans, read off views.

Needs first: `Cx::syntax_view`, `crate::cx::{item_range_view, def_range_view}` (`ast::Node<'_>
-> TextRange`, frontend-tree-move-diag-lower). Lands with every other `lints/names/dead/*`
heading here.

`crates/blaze-diagnostics/src/lints/names/dead/report.rs`, hunk 1:

```rust
use syntax::{
    AstNode, TextRange,
    ast::{self, HasName},
};
```

```rust
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};
```

Hunk 2:

```rust
        let node = cx.syntax(self.k.defs[def].node?);
        let name = ast::AnyHasName::cast(node.value)?.name()?;
```

```rust
        let node = cx.syntax_view(self.k.defs[def].node?);
        let name = ast::AnyHasName::cast(node.value)?.name()?;
```

Hunk 3:

```rust
        let node = cx.syntax(ptr);
        let range: TextRange = match self.k.defs[def].kind {
            Kind::Variant | Kind::Field => crate::cx::item_range(&node.value),
            _ => crate::cx::def_range(&node.value),
        };
```

```rust
        let node = cx.syntax_view(ptr);
        let range: TextRange = match self.k.defs[def].kind {
            Kind::Variant | Kind::Field => crate::cx::item_range_view(node.value),
            _ => crate::cx::def_range_view(node.value),
        };
```

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 under the grant (2026-10-29 22:30): every hunk, old text matched today's file exactly.

## frontend-tree: blaze-diagnostics lints/names/dead/mark.rs: onto views (1)

From `frontend-tree-move-diag-lints-names`. The walk of each live definition's signature (and
the constants in it), over views: one recursive walk of the signature's nodes, each step
arithmetic on the flat tree, no handle, count or clone per node. The tuple pattern's and the
destructuring call's subpatterns are walked again from their parent's accessor instead of
gathered into a `SmallVec`.

Needs first: `Cx::syntax_view`, `Cx::vsema` with `expand_macro_call(ast::MacroCall<'db>) ->
Option<InFile<Node<'db>>>`, `resolve_path`, `resolve_method_call`, `resolve_field`,
`resolve_record_field`, `resolve_record_pat_field`, `resolve_offset_of_field(ast::NameRef<'db>)`,
`resolve_bind_pat_to_const`, `type_of_expr`, `type_of_pat`, `expr_adjustments`, all taking the
view by value (frontend-tree-move-hir / -diag-lower); `lints::syntax::from_expansion(cx,
InFile<ast::Node<'_>>)` (`## frontend-tree: blaze-diagnostics lints/syntax.rs: onto views (1)` in
`v2/requests/diagnostics.md`). Lands with every other `lints/names/dead/*` heading here.

`crates/blaze-diagnostics/src/lints/names/dead/mark.rs`, hunk 1:

```rust
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, T,
    ast::{self, BinaryOp, HasArgList, RangeItem},
};

use super::{
    defs::{DefIdx, DefSet, Id, Kind, Parent, anon_key, cast, is_anon},
```

```rust
use syntax::{
    SyntaxKind, SyntaxNodePtr, T,
    view::{self as ast, AstNode as _, BinaryOp, HasArgList as _, RangeItem as _},
};

use super::{
    defs::{DefIdx, DefSet, Id, Kind, Parent, anon_key, is_anon},
```

Hunk 2:

```rust
impl Mark<'_, '_, '_, '_> {
    /// rustc's `own_comes_from_allow_expect`.
```

```rust
impl<'db> Mark<'_, '_, '_, 'db> {
    /// rustc's `own_comes_from_allow_expect`.
```

Hunk 3:

```rust
        let node = k.cx.syntax(ptr);
        if let Some(owner) = self.signature(node.file_id, &node.value, d.id) {
```

```rust
        let node = k.cx.syntax_view(ptr);
        if let Some(owner) = self.signature(node.file_id, node.value, d.id) {
```

Hunk 4 (`signature`):

```rust
    fn signature(&mut self, file: HirFileId, node: &SyntaxNode, id: Id) -> Option<DefWithBodyId> {
```

```rust
    fn signature(&mut self, file: HirFileId, node: ast::Node<'db>, id: Id) -> Option<DefWithBodyId> {
```

Hunk 5:

```rust
            if child.kind() != SyntaxKind::PARAM_LIST {
                stdx::ensure_sufficient_stack(|| self.walk(file, &child, false));
                continue;
            }
            for param in child.children() {
                if param.kind() != SyntaxKind::PARAM {
                    self.walk(file, &param, false);
                    continue;
                }
                for part in param.children() {
                    if !ast::Pat::can_cast(part.kind()) {
                        stdx::ensure_sufficient_stack(|| self.walk(file, &part, false));
```

```rust
            if child.kind() != SyntaxKind::PARAM_LIST {
                stdx::ensure_sufficient_stack(|| self.walk(file, child, false));
                continue;
            }
            for param in child.children() {
                if param.kind() != SyntaxKind::PARAM {
                    self.walk(file, param, false);
                    continue;
                }
                for part in param.children() {
                    if !ast::Pat::can_cast(part.kind()) {
                        stdx::ensure_sufficient_stack(|| self.walk(file, part, false));
```

Hunk 6 (`walk`):

```rust
    fn walk(&mut self, file: HirFileId, node: &SyntaxNode, root: bool) {
```

```rust
    fn walk(&mut self, file: HirFileId, node: ast::Node<'db>, root: bool) {
```

Hunk 7:

```rust
                if let Some(call) = cast::<ast::MacroCall>(node)
                    && let Some(expansion) = self.k.cx.sema.expand_macro_call(&call)
                {
                    stdx::ensure_sufficient_stack(|| self.walk(expansion.file_id, &expansion.value, false));
                }
```

```rust
                if let Some(call) = ast::MacroCall::cast(node)
                    && let Some(expansion) = self.k.cx.vsema.expand_macro_call(call)
                {
                    stdx::ensure_sufficient_stack(|| self.walk(expansion.file_id, expansion.value, false));
                }
```

Hunk 8 (the dispatch by kind):

```rust
            SyntaxKind::PATH => {
                if let Some(path) = cast::<ast::Path>(node) {
                    self.path(&path);
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(call) = cast::<ast::MethodCallExpr>(node)
                    && let Some(function) = self.k.cx.sema.resolve_method_call(&call)
                {
                    self.handle_res(Res::function(self.k.defs, function));
                }
            }
            SyntaxKind::FIELD_EXPR => {
                if let Some(field) = cast::<ast::FieldExpr>(node) {
                    self.field_read(&field);
                }
            }
            SyntaxKind::RECORD_EXPR => {
                if let Some(expr) = cast::<ast::RecordExpr>(node) {
                    self.union_fields(expr);
                }
            }
            SyntaxKind::OFFSET_OF_EXPR => {
                if let Some(expr) = cast::<ast::OffsetOfExpr>(node) {
                    for name in expr.fields() {
                        if let Some((Either::Right(field), _)) = self.k.cx.sema.resolve_offset_of_field(&name)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                }
            }
            SyntaxKind::BIN_EXPR => {
                if let Some(expr) = cast::<ast::BinExpr>(node)
                    && let Some(BinaryOp::Assignment { op: None }) = expr.op_kind()
                {
                    self.assign(file, &expr);
                    return;
                }
            }
            SyntaxKind::MATCH_ARM => {
                let len = self.ignore_variants.len();
                if let Some(pat) = cast::<ast::MatchArm>(node).and_then(|it| it.pat()) {
```

```rust
            SyntaxKind::PATH => {
                if let Some(path) = ast::Path::cast(node) {
                    self.path(path);
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => {
                if let Some(call) = ast::MethodCallExpr::cast(node)
                    && let Some(function) = self.k.cx.vsema.resolve_method_call(call)
                {
                    self.handle_res(Res::function(self.k.defs, function));
                }
            }
            SyntaxKind::FIELD_EXPR => {
                if let Some(field) = ast::FieldExpr::cast(node) {
                    self.field_read(field);
                }
            }
            SyntaxKind::RECORD_EXPR => {
                if let Some(expr) = ast::RecordExpr::cast(node) {
                    self.union_fields(expr);
                }
            }
            SyntaxKind::OFFSET_OF_EXPR => {
                if let Some(expr) = ast::OffsetOfExpr::cast(node) {
                    for name in expr.fields() {
                        if let Some((Either::Right(field), _)) = self.k.cx.vsema.resolve_offset_of_field(name)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                }
            }
            SyntaxKind::BIN_EXPR => {
                if let Some(expr) = ast::BinExpr::cast(node)
                    && let Some(BinaryOp::Assignment { op: None }) = expr.op_kind()
                {
                    self.assign(file, expr);
                    return;
                }
            }
            SyntaxKind::MATCH_ARM => {
                let len = self.ignore_variants.len();
                if let Some(pat) = ast::MatchArm::cast(node).and_then(|it| it.pat()) {
```

Hunk 9 (`children`, `anon`):

```rust
    fn children(&mut self, file: HirFileId, node: &SyntaxNode) {
        for child in node.children() {
            stdx::ensure_sufficient_stack(|| self.walk(file, &child, false));
        }
    }

    /// A closure (live unless an `allow` it has came with what reached it) or an anonymous
    /// constant (live), walked out of any pattern it is in.
    fn anon(&mut self, file: HirFileId, node: &SyntaxNode) {
```

```rust
    fn children(&mut self, file: HirFileId, node: ast::Node<'db>) {
        for child in node.children() {
            stdx::ensure_sufficient_stack(|| self.walk(file, child, false));
        }
    }

    /// A closure (live unless an `allow` it has came with what reached it) or an anonymous
    /// constant (live), walked out of any pattern it is in.
    fn anon(&mut self, file: HirFileId, node: ast::Node<'db>) {
```

Hunk 10 (`path`, `field_read`):

```rust
    fn path(&mut self, path: &ast::Path) -> Res {
        let cx = self.k.cx;
        let parent = path.syntax().parent().map(|it| it.kind());
        if matches!(parent, Some(SyntaxKind::MACRO_CALL | SyntaxKind::META)) {
            return Res::None;
        }
        let Some(res) = cx.sema.resolve_path(path) else { return Res::None };
```

```rust
    fn path(&mut self, path: ast::Path<'db>) -> Res {
        let cx = self.k.cx;
        let parent = path.syntax().parent().map(|it| it.kind());
        if matches!(parent, Some(SyntaxKind::MACRO_CALL | SyntaxKind::META)) {
            return Res::None;
        }
        let Some(res) = cx.vsema.resolve_path(path) else { return Res::None };
```

Hunk 11:

```rust
    fn field_read(&mut self, field: &ast::FieldExpr) {
        if let Some(Either::Left(field)) = self.k.cx.sema.resolve_field(field)
```

```rust
    fn field_read(&mut self, field: ast::FieldExpr<'db>) {
        if let Some(Either::Left(field)) = self.k.cx.vsema.resolve_field(field)
```

Hunk 12 (`union_fields`):

```rust
    fn union_fields(&mut self, expr: ast::RecordExpr) {
```

```rust
    fn union_fields(&mut self, expr: ast::RecordExpr<'db>) {
```

Hunk 13:

```rust
        let Some(ty) = cx.sema.type_of_expr(&ast::Expr::RecordExpr(expr)) else { return };
```

```rust
        let Some(ty) = cx.vsema.type_of_expr(ast::Expr::RecordExpr(expr)) else { return };
```

Hunk 14:

```rust
        for field in list.fields() {
            if let Some((field, _, _)) = cx.sema.resolve_record_field(&field)
```

```rust
        for field in list.fields() {
            if let Some((field, _, _)) = cx.vsema.resolve_record_field(field)
```

Hunk 15 (`pat`):

```rust
    fn pat(&mut self, file: HirFileId, node: &SyntaxNode) {
        let cx = self.k.cx;
        self.in_pat = true;
        let Some(pat) = cast::<ast::Pat>(node) else {
            self.children(file, node);
            self.in_pat = false;
            return;
        };
        match &pat {
            ast::Pat::RecordPat(it) => {
                self.pat_adt(&pat);
                if let Some(path) = it.path() {
                    self.path(&path);
                }
                if let Some(list) = it.record_pat_field_list() {
                    for field in list.fields() {
                        if matches!(field.pat(), Some(ast::Pat::WildcardPat(_))) {
                            continue;
                        }
                        if let Some((field, _)) = cx.sema.resolve_record_pat_field(&field)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                    self.children(file, list.syntax());
                }
            }
            ast::Pat::TupleStructPat(it) => {
                let adt = self.pat_adt(&pat);
                let res = it.path().map_or(Res::None, |path| self.path(&path));
                let subpats: SmallVec<[ast::Pat; 4]> = it.fields().collect(); // alloc: inline up to 4
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = subpats.iter().position(|it| matches!(it, ast::Pat::RestPat(_)));
                    let bound = subpats.len() - usize::from(rest.is_some());
                    let wildcard = subpats
                        .iter()
                        .filter(|it| !matches!(it, ast::Pat::RestPat(_)))
                        .map(|it| matches!(it, ast::Pat::WildcardPat(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for sub in subpats {
                    stdx::ensure_sufficient_stack(|| self.walk(file, sub.syntax(), false));
                    self.in_pat = true;
                }
            }
            ast::Pat::PathPat(_) => {
                self.pat_adt(&pat);
                self.children(file, node);
            }
            ast::Pat::IdentPat(it) => {
                if let Some(def) = cx.sema.resolve_bind_pat_to_const(it) {
                    self.pat_adt(&pat);
```

```rust
    fn pat(&mut self, file: HirFileId, node: ast::Node<'db>) {
        let cx = self.k.cx;
        self.in_pat = true;
        let Some(pat) = ast::Pat::cast(node) else {
            self.children(file, node);
            self.in_pat = false;
            return;
        };
        match pat {
            ast::Pat::RecordPat(it) => {
                self.pat_adt(pat);
                if let Some(path) = it.path() {
                    self.path(path);
                }
                if let Some(list) = it.record_pat_field_list() {
                    for field in list.fields() {
                        if matches!(field.pat(), Some(ast::Pat::WildcardPat(_))) {
                            continue;
                        }
                        if let Some((field, _)) = cx.vsema.resolve_record_pat_field(field)
                            && let Some(field) = self.k.defs.get(Id::Field(FieldId::from(field)))
                        {
                            self.insert_def_id(field);
                        }
                    }
                    self.children(file, list.syntax());
                }
            }
            ast::Pat::TupleStructPat(it) => {
                let adt = self.pat_adt(pat);
                let res = it.path().map_or(Res::None, |path| self.path(path));
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = it.fields().position(|it| matches!(it, ast::Pat::RestPat(_)));
                    let bound = it.fields().count() - usize::from(rest.is_some());
                    let wildcard = it
                        .fields()
                        .filter(|it| !matches!(it, ast::Pat::RestPat(_)))
                        .map(|it| matches!(it, ast::Pat::WildcardPat(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for sub in it.fields() {
                    stdx::ensure_sufficient_stack(|| self.walk(file, sub.syntax(), false));
                    self.in_pat = true;
                }
            }
            ast::Pat::PathPat(_) => {
                self.pat_adt(pat);
                self.children(file, node);
            }
            ast::Pat::IdentPat(it) => {
                if let Some(def) = cx.vsema.resolve_bind_pat_to_const(it) {
                    self.pat_adt(pat);
```

Hunk 16 (`pat_adt`):

```rust
    fn pat_adt(&mut self, pat: &ast::Pat) -> Option<DefIdx> {
        let ty = self.k.cx.sema.type_of_pat(pat)?;
```

```rust
    fn pat_adt(&mut self, pat: ast::Pat<'db>) -> Option<DefIdx> {
        let ty = self.k.cx.vsema.type_of_pat(pat)?;
```

Hunk 17 (`necessary_variants`, `is_resolved`):

```rust
    fn necessary_variants(&self, node: &SyntaxNode, out: &mut Vec<DefIdx>, from: usize) {
        let cx = self.k.cx;
        let path = match node.kind() {
            SyntaxKind::OR_PAT => return,
            SyntaxKind::PATH_PAT => cast::<ast::PathPat>(node).and_then(|it| it.path()),
            SyntaxKind::TUPLE_STRUCT_PAT => cast::<ast::TupleStructPat>(node).and_then(|it| it.path()),
            SyntaxKind::RECORD_PAT => cast::<ast::RecordPat>(node).and_then(|it| it.path()),
            SyntaxKind::IDENT_PAT => {
                if let Some(pat) = cast::<ast::IdentPat>(node)
                    && let Some(ModuleDef::EnumVariant(it)) = cx.sema.resolve_bind_pat_to_const(&pat)
```

```rust
    fn necessary_variants(&self, node: ast::Node<'db>, out: &mut Vec<DefIdx>, from: usize) {
        let cx = self.k.cx;
        let path = match node.kind() {
            SyntaxKind::OR_PAT => return,
            SyntaxKind::PATH_PAT => ast::PathPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::TUPLE_STRUCT_PAT => ast::TupleStructPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::RECORD_PAT => ast::RecordPat::cast(node).and_then(|it| it.path()),
            SyntaxKind::IDENT_PAT => {
                if let Some(pat) = ast::IdentPat::cast(node)
                    && let Some(ModuleDef::EnumVariant(it)) = cx.vsema.resolve_bind_pat_to_const(pat)
```

Hunk 18:

```rust
        if let Some(path) = path
            && self.is_resolved(&path)
            && let Some(res) = cx.sema.resolve_path(&path)
```

```rust
        if let Some(path) = path
            && self.is_resolved(path)
            && let Some(res) = cx.vsema.resolve_path(path)
```

Hunk 19:

```rust
            if ast::Pat::can_cast(kind) || kind == SyntaxKind::RECORD_PAT_FIELD_LIST {
                self.necessary_variants(&child, out, from);
            } else if kind == SyntaxKind::RECORD_PAT_FIELD {
                for it in child.children() {
                    self.necessary_variants(&it, out, from);
```

```rust
            if ast::Pat::can_cast(kind) || kind == SyntaxKind::RECORD_PAT_FIELD_LIST {
                self.necessary_variants(child, out, from);
            } else if kind == SyntaxKind::RECORD_PAT_FIELD {
                for it in child.children() {
                    self.necessary_variants(it, out, from);
```

Hunk 20:

```rust
    fn is_resolved(&self, path: &ast::Path) -> bool {
        let Some(qualifier) = path.qualifier() else { return true };
        if is_self(&qualifier) || qualifier.segments().any(|it| it.type_anchor().is_some()) {
            return false;
        }
        matches!(
            self.k.cx.sema.resolve_path(&qualifier),
```

```rust
    fn is_resolved(&self, path: ast::Path<'db>) -> bool {
        let Some(qualifier) = path.qualifier() else { return true };
        if is_self(qualifier) || qualifier.segments().any(|it| it.type_anchor().is_some()) {
            return false;
        }
        matches!(
            self.k.cx.vsema.resolve_path(qualifier),
```

Hunk 21 (`assign`, `handle_assign`):

```rust
    fn assign(&mut self, file: HirFileId, expr: &ast::BinExpr) {
        let Some(lhs) = expr.lhs() else {
            if let Some(rhs) = expr.rhs() {
                self.walk(file, rhs.syntax(), false);
            }
            return;
        };
        let destructuring = destructures(&lhs);
        match destructuring {
            true => self.assign_pattern(file, &lhs),
            false => self.handle_assign(file, &lhs),
        }
        if let Some(rhs) = expr.rhs() {
            if !destructuring {
                self.self_assign(file, expr, &lhs, &rhs);
            }
            stdx::ensure_sufficient_stack(|| self.walk(file, rhs.syntax(), false));
        }
    }

    fn handle_assign(&mut self, file: HirFileId, place: &ast::Expr) {
        let through_field = matches!(place, ast::Expr::FieldExpr(_) | ast::Expr::ParenExpr(_));
        let derefs = through_field
            && self
                .k
                .cx
                .sema
                .expr_adjustments(place)
                .is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::Deref(_))));
        match place {
            ast::Expr::FieldExpr(field) if !derefs => {
                if let Some(base) = field.expr() {
                    self.handle_assign(file, &base);
                }
            }
            ast::Expr::ParenExpr(it) if !derefs => {
                if let Some(inner) = it.expr() {
                    self.handle_assign(file, &inner);
                }
            }
```

```rust
    fn assign(&mut self, file: HirFileId, expr: ast::BinExpr<'db>) {
        let Some(lhs) = expr.lhs() else {
            if let Some(rhs) = expr.rhs() {
                self.walk(file, rhs.syntax(), false);
            }
            return;
        };
        let destructuring = destructures(lhs);
        match destructuring {
            true => self.assign_pattern(file, lhs),
            false => self.handle_assign(file, lhs),
        }
        if let Some(rhs) = expr.rhs() {
            if !destructuring {
                self.self_assign(file, expr, lhs, rhs);
            }
            stdx::ensure_sufficient_stack(|| self.walk(file, rhs.syntax(), false));
        }
    }

    fn handle_assign(&mut self, file: HirFileId, place: ast::Expr<'db>) {
        let through_field = matches!(place, ast::Expr::FieldExpr(_) | ast::Expr::ParenExpr(_));
        let derefs = through_field
            && self
                .k
                .cx
                .vsema
                .expr_adjustments(place)
                .is_some_and(|it| it.iter().any(|it| matches!(it.kind, Adjust::Deref(_))));
        match place {
            ast::Expr::FieldExpr(field) if !derefs => {
                if let Some(base) = field.expr() {
                    self.handle_assign(file, base);
                }
            }
            ast::Expr::ParenExpr(it) if !derefs => {
                if let Some(inner) = it.expr() {
                    self.handle_assign(file, inner);
                }
            }
```

Hunk 22 (`assign_pattern`; the call's arguments walked again from the argument list instead of
gathered):

```rust
    fn assign_pattern(&mut self, file: HirFileId, lhs: &ast::Expr) {
        let cx = self.k.cx;
        match lhs {
            ast::Expr::ParenExpr(it) => {
                if let Some(inner) = it.expr() {
                    self.assign_pattern(file, &inner);
                }
            }
            ast::Expr::TupleExpr(it) => {
                for field in it.fields() {
                    self.assign_pattern(file, &field);
                }
            }
            ast::Expr::ArrayExpr(it) => {
                for field in it.exprs() {
                    self.assign_pattern(file, &field);
                }
            }
            ast::Expr::UnderscoreExpr(_) => {}
            ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none() => {}
            ast::Expr::RecordExpr(it) => {
                self.expr_adt(lhs);
                if let Some(path) = it.path() {
                    self.in_pat = true;
                    self.path(&path);
                    self.in_pat = false;
                }
                for field in it.record_expr_field_list().into_iter().flat_map(|it| it.fields()) {
                    let value = field.expr();
                    if matches!(value, Some(ast::Expr::UnderscoreExpr(_))) {
                        continue;
                    }
                    if let Some((def, _, _)) = cx.sema.resolve_record_field(&field)
                        && let Some(def) = self.k.defs.get(Id::Field(FieldId::from(def)))
                    {
                        self.insert_def_id(def);
                    }
                    if let Some(value) = value {
                        self.assign_pattern(file, &value);
                    }
                }
            }
            ast::Expr::CallExpr(it) => {
                let path = match it.expr() {
                    Some(ast::Expr::PathExpr(it)) => it.path(),
                    _ => None,
                };
                let Some(path) = path else {
                    self.handle_assign(file, lhs);
                    return;
                };
                let adt = self.expr_adt(lhs);
                self.in_pat = true;
                let res = self.path(&path);
                self.in_pat = false;
                // alloc: inline up to 4
                let args: SmallVec<[ast::Expr; 4]> = it.arg_list().into_iter().flat_map(|it| it.args()).collect();
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = args.iter().position(is_rest);
                    let bound = args.len() - usize::from(rest.is_some());
                    let wildcard =
                        args.iter().filter(|it| !is_rest(it)).map(|it| matches!(it, ast::Expr::UnderscoreExpr(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for arg in &args {
                    self.assign_pattern(file, arg);
                }
            }
            ast::Expr::PathExpr(it) => {
                let path = it.path();
                let constructs = path.as_ref().and_then(|path| cx.sema.resolve_path(path)).is_some_and(|it| {
                    matches!(it, PathResolution::Def(ModuleDef::Adt(_) | ModuleDef::EnumVariant(_)))
                });
                match (constructs, path) {
                    (true, Some(path)) => {
                        self.expr_adt(lhs);
                        self.in_pat = true;
                        self.path(&path);
                        self.in_pat = false;
                    }
                    _ => self.handle_assign(file, lhs),
                }
            }
            _ => self.handle_assign(file, lhs),
        }
    }

    fn expr_adt(&mut self, expr: &ast::Expr) -> Option<DefIdx> {
        let ty = self.k.cx.sema.type_of_expr(expr)?;
```

```rust
    fn assign_pattern(&mut self, file: HirFileId, lhs: ast::Expr<'db>) {
        let cx = self.k.cx;
        match lhs {
            ast::Expr::ParenExpr(it) => {
                if let Some(inner) = it.expr() {
                    self.assign_pattern(file, inner);
                }
            }
            ast::Expr::TupleExpr(it) => {
                for field in it.fields() {
                    self.assign_pattern(file, field);
                }
            }
            ast::Expr::ArrayExpr(it) => {
                for field in it.exprs() {
                    self.assign_pattern(file, field);
                }
            }
            ast::Expr::UnderscoreExpr(_) => {}
            ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none() => {}
            ast::Expr::RecordExpr(it) => {
                self.expr_adt(lhs);
                if let Some(path) = it.path() {
                    self.in_pat = true;
                    self.path(path);
                    self.in_pat = false;
                }
                for field in it.record_expr_field_list().into_iter().flat_map(|it| it.fields()) {
                    let value = field.expr();
                    if matches!(value, Some(ast::Expr::UnderscoreExpr(_))) {
                        continue;
                    }
                    if let Some((def, _, _)) = cx.vsema.resolve_record_field(field)
                        && let Some(def) = self.k.defs.get(Id::Field(FieldId::from(def)))
                    {
                        self.insert_def_id(def);
                    }
                    if let Some(value) = value {
                        self.assign_pattern(file, value);
                    }
                }
            }
            ast::Expr::CallExpr(it) => {
                let path = match it.expr() {
                    Some(ast::Expr::PathExpr(it)) => it.path(),
                    _ => None,
                };
                let Some(path) = path else {
                    self.handle_assign(file, lhs);
                    return;
                };
                let adt = self.expr_adt(lhs);
                self.in_pat = true;
                let res = self.path(path);
                self.in_pat = false;
                let args = || it.arg_list().into_iter().flat_map(|it| it.args());
                if let Some(variant) = self.variant_of(res, adt) {
                    let rest = args().position(is_rest);
                    let bound = args().count() - usize::from(rest.is_some());
                    let wildcard = args().filter(|&it| !is_rest(it)).map(|it| matches!(it, ast::Expr::UnderscoreExpr(_)));
                    self.tuple_fields(variant, wildcard, bound, rest);
                }
                for arg in args() {
                    self.assign_pattern(file, arg);
                }
            }
            ast::Expr::PathExpr(it) => {
                let path = it.path();
                let constructs = path.and_then(|path| cx.vsema.resolve_path(path)).is_some_and(|it| {
                    matches!(it, PathResolution::Def(ModuleDef::Adt(_) | ModuleDef::EnumVariant(_)))
                });
                match (constructs, path) {
                    (true, Some(path)) => {
                        self.expr_adt(lhs);
                        self.in_pat = true;
                        self.path(path);
                        self.in_pat = false;
                    }
                    _ => self.handle_assign(file, lhs),
                }
            }
            _ => self.handle_assign(file, lhs),
        }
    }

    fn expr_adt(&mut self, expr: ast::Expr<'db>) -> Option<DefIdx> {
        let ty = self.k.cx.vsema.type_of_expr(expr)?;
```

Hunk 23 (`self_assign`):

```rust
    fn self_assign(&mut self, file: HirFileId, expr: &ast::BinExpr, lhs: &ast::Expr, rhs: &ast::Expr) {
        let cx = self.k.cx;
        if !same_place(cx, lhs, rhs) {
            return;
        }
        if crate::lints::syntax::from_expansion(cx, InFile::new(file, expr.syntax())) {
            return;
        }
        let at = InFile::new(file, SyntaxNodePtr::new(expr.syntax()));
        if !self.self_assigns.insert(at) {
            return;
        }
        let Some(ty) = cx.sema.type_of_expr(lhs) else { return };
```

```rust
    fn self_assign(&mut self, file: HirFileId, expr: ast::BinExpr<'db>, lhs: ast::Expr<'db>, rhs: ast::Expr<'db>) {
        let cx = self.k.cx;
        if !same_place(cx, lhs, rhs) {
            return;
        }
        if crate::lints::syntax::from_expansion(cx, InFile::new(file, expr.syntax())) {
            return;
        }
        let at = InFile::new(file, ast::ptr(expr.syntax()));
        if !self.self_assigns.insert(at) {
            return;
        }
        let Some(ty) = cx.vsema.type_of_expr(lhs) else { return };
```

Hunk 24 (the free helpers, by value; `unparen` returns the `Copy` view, no clone):

```rust
fn destructures(lhs: &ast::Expr) -> bool {
    match lhs {
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(|it| destructures(&it)),
```

```rust
fn destructures(lhs: ast::Expr<'_>) -> bool {
    match lhs {
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(destructures),
```

Hunk 25:

```rust
fn is_rest(expr: &ast::Expr) -> bool {
    matches!(expr, ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none())
}

fn unparen(expr: &ast::Expr) -> ast::Expr {
    match expr {
        // clone: a refcount, for an assignment in a signature's constant (bodies use the store).
        ast::Expr::ParenExpr(it) => it.expr().map_or_else(|| expr.clone(), |it| unparen(&it)),
        _ => expr.clone(), // clone: as above
    }
}

/// `check_for_self_assign_helper`: the same local (a one-segment path on each side with the same
/// name, resolved only then), or the same field of the same place.
fn same_place(cx: &crate::cx::Cx<'_, '_>, lhs: &ast::Expr, rhs: &ast::Expr) -> bool {
```

```rust
fn is_rest(expr: ast::Expr<'_>) -> bool {
    matches!(expr, ast::Expr::RangeExpr(it) if it.start().is_none() && it.end().is_none())
}

fn unparen(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    match expr {
        ast::Expr::ParenExpr(it) => it.expr().map_or(expr, unparen),
        _ => expr,
    }
}

/// `check_for_self_assign_helper`: the same local (a one-segment path on each side with the same
/// name, resolved only then), or the same field of the same place.
fn same_place<'db>(cx: &crate::cx::Cx<'_, 'db>, lhs: ast::Expr<'db>, rhs: ast::Expr<'db>) -> bool {
```

Hunk 26:

```rust
            let local = |it: &ast::Path| match cx.sema.resolve_path(it)? {
                PathResolution::Local(it) => Some(it),
                _ => None,
            };
            same_name && matches!((local(&l), local(&r)), (Some(l), Some(r)) if l == r)
```

```rust
            let local = |it: ast::Path<'db>| match cx.vsema.resolve_path(it)? {
                PathResolution::Local(it) => Some(it),
                _ => None,
            };
            same_name && matches!((local(l), local(r)), (Some(l), Some(r)) if l == r)
```

Hunk 27:

```rust
                (Some(l), Some(r)) if same_name => same_place(cx, &l, &r),
```

```rust
                (Some(l), Some(r)) if same_name => same_place(cx, l, r),
```

Hunk 28:

```rust
fn is_self(path: &ast::Path) -> bool {
```

```rust
fn is_self(path: ast::Path<'_>) -> bool {
```

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 under the grant (2026-10-29 22:30): every hunk, old text matched today's file exactly.

## frontend-tree: blaze-diagnostics lints/names/dead/mark/body.rs: onto views (1)

From `frontend-tree-move-diag-lints-names`. Bodies are walked over the expression store's
columns already; the three places that read syntax (`offset_of!`'s fields, a self-assignment's
expansion test, a closure's level) read views.

Needs first: `Cx::syntax_view`, `Cx::vsema` with `resolve_offset_of_field(ast::NameRef<'db>)`
(frontend-tree-move-diag-lower / -hir); `lints::syntax::from_expansion(cx, InFile<ast::Node<'_>>)`
(`## frontend-tree: blaze-diagnostics lints/syntax.rs: onto views (1)` in
`v2/requests/diagnostics.md`). Lands with every other `lints/names/dead/*` heading here
(`Levels::at` takes the view).

`crates/blaze-diagnostics/src/lints/names/dead/mark/body.rs`, hunk 1:

```rust
use syntax::{AstNode, SyntaxNodePtr, ast};
```

```rust
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};
```

Hunk 2:

```rust
        let Some(it) = ast::OffsetOfExpr::cast(cx.syntax(ptr).value) else { return };
        for name in it.fields() {
            if let Some((Either::Right(field), _)) = cx.sema.resolve_offset_of_field(&name)
```

```rust
        let Some(it) = ast::OffsetOfExpr::cast(cx.syntax_view(ptr).value) else { return };
        for name in it.fields() {
            if let Some((Either::Right(field), _)) = cx.vsema.resolve_offset_of_field(name)
```

Hunk 3:

```rust
        if crate::lints::syntax::from_expansion(cx, cx.syntax(at).as_ref()) {
```

```rust
        if crate::lints::syntax::from_expansion(cx, cx.syntax_view(at)) {
```

Hunk 4:

```rust
            let own = closure && self.m.propagated && k.levels.at(k.cx, k.cx.syntax(ptr).as_ref()).is_allow_or_expect();
```

```rust
            let own = closure && self.m.propagated && k.levels.at(k.cx, k.cx.syntax_view(ptr)).is_allow_or_expect();
```

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 under the grant (2026-10-29 22:30): every hunk, old text matched today's file exactly.

## frontend-tree: blaze-diagnostics lints/names/items/walk.rs: onto views (1)

From `frontend-tree-move-diag-lints-items` (frontend.md item 16, design
`docs/v2/plan/lanes/frontend/tree-move-diag-lints-items.md`). The item walk holds views of the
database's trees (`'db`, the `Cx<'_, 'db>` borrow) instead of counted handles: nothing is cloned
to keep a node, and every `// clone: a rowan cursor's refcount` goes with the clone it excused.
Rewritten 2026-10-29 22:00 against the walk with one column of own nodes (hot-items-b's request,
applied): the column holds views, so it stays one `Rc<[..]>` per walk and each item's range.

Needs, landing together: every other `## frontend-tree: blaze-diagnostics lints/names/items*`
heading in this file (the item lints read `Item<'db>` and pass owners by value);
`## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (frontend-tree-move-diag-lower:
`Cx::vsema`, `Cx::root`) and `## frontend-tree: blaze-diagnostics cx.rs: the item walk on views
(1)` (`ItemWalk<'db>`), both in `v2/requests/diagnostics.md`; `hir::view::Semantics`'s `to_def2`,
`is_attr_macro_call`, `expand_attr_macro` and `Module::definition_source_view` with
`hir::view::ModuleSourceView` (frontend-tree-move-hir); `hir_expand::attrs::expand_cfg_attr_view`
and `CfgExpr::parse_from_view` (in the tree).

1. The imports. Why: views and their traits; `ModuleSourceView` is `ModuleSource`'s view twin.

```rust
use hir::{HirFileId, InFile, ModuleSource};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasAttrs, HasModuleItem},
};
```

becomes

```rust
use hir::{HirFileId, InFile, view::ModuleSourceView};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode, HasAttrs, HasModuleItem},
};
```

2. `Container` and `Item`. Why: a container is `Copy` (views); `Item` holds views and no caller
   clones one, so it drops `Clone` (a big type); the column holds views.

```rust
#[derive(Clone, Debug)]
pub(super) enum Container {
    /// A module's or a block's own item.
    Module,
    Trait(InFile<ast::Trait>),
    Impl(InFile<ast::Impl>),
    Extern(InFile<ast::ExternBlock>),
}

/// An item, with where it is and the nodes that are its own.
#[derive(Clone, Debug)]
pub(super) struct Item {
    pub(super) node: InFile<ast::Item>,
    pub(super) container: Container,
```

becomes

```rust
#[derive(Clone, Copy, Debug)]
pub(super) enum Container<'db> {
    /// A module's or a block's own item.
    Module,
    Trait(InFile<ast::Trait<'db>>),
    Impl(InFile<ast::Impl<'db>>),
    Extern(InFile<ast::ExternBlock<'db>>),
}

/// An item, with where it is and the nodes that are its own.
#[derive(Debug)]
pub(super) struct Item<'db> {
    pub(super) node: InFile<ast::Item<'db>>,
    pub(super) container: Container<'db>,
```

and

```rust
    column: Rc<[InFile<SyntaxNode>]>,
```

becomes

```rust
    column: Rc<[InFile<ast::Node<'db>>]>,
```

3. `Item`'s methods. Why: views by value, nothing cloned.

```rust
impl Item {
    /// Its own nodes, in source order (preorder), itself first; in what the macros it calls
    /// expand to, too; without the items in it and what is in them.
    pub(super) fn nodes(&self) -> &[InFile<SyntaxNode>] {
        &self.column[self.own.0 as usize..self.own.1 as usize]
    }

    /// The node rustc emits an item's lints at.
    pub(super) fn ptr(&self) -> InFile<SyntaxNodePtr> {
        self.node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()))
    }

    pub(super) fn syntax(&self) -> InFile<SyntaxNode> {
        // clone: a rowan cursor's refcount, no allocation; callers need an owned node
        self.node.as_ref().map(|it| it.syntax().clone())
    }

    /// Its own nodes that are `N`, each kind tested before it is cast.
    pub(super) fn own<N: AstNode>(&self) -> impl Iterator<Item = InFile<N>> + '_ {
        self.nodes()
            .iter()
            .filter(|it| N::can_cast(it.value.kind()))
            // clone: a rowan cursor's refcount, only for nodes of `N`'s kind; `cast` takes it owned
            .filter_map(|it| N::cast(it.value.clone()).map(|n| it.with_value(n)))
    }

    /// The items it holds, among `items` (all of the walk's, it at `index`).
    pub(super) fn held<'i>(&self, items: &'i [Item], index: usize) -> &'i [Item] {
```

becomes

```rust
impl<'db> Item<'db> {
    /// Its own nodes, in source order (preorder), itself first; in what the macros it calls
    /// expand to, too; without the items in it and what is in them.
    pub(super) fn nodes(&self) -> &[InFile<ast::Node<'db>>] {
        &self.column[self.own.0 as usize..self.own.1 as usize]
    }

    /// The node rustc emits an item's lints at.
    pub(super) fn ptr(&self) -> InFile<SyntaxNodePtr> {
        self.node.map(|it| ast::ptr(it.syntax()))
    }

    pub(super) fn syntax(&self) -> InFile<ast::Node<'db>> {
        self.node.map(|it| it.syntax())
    }

    /// Its own nodes that are `N`, each kind tested before it is cast.
    pub(super) fn own<N: AstNode<'db>>(&self) -> impl Iterator<Item = InFile<N>> + '_ {
        self.nodes()
            .iter()
            .filter(|it| N::can_cast(it.value.kind()))
            .filter_map(|it| N::cast(it.value).map(|n| it.with_value(n)))
    }

    /// The items it holds, among `items` (all of the walk's, it at `index`).
    pub(super) fn held<'i>(&self, items: &'i [Item<'db>], index: usize) -> &'i [Item<'db>] {
```

4. `fields`. Why: views are `Copy` (`copied`, not `cloned`).

```rust
pub(super) fn fields(item: &Item) -> SmallVec<[InFile<SyntaxNode>; 8]> {
    let of = |kind: SyntaxKind| item.nodes().iter().filter(move |it| it.value.kind() == kind).cloned();
    let mut fields: SmallVec<[InFile<SyntaxNode>; 8]> = SmallVec::new();
```

becomes

```rust
pub(super) fn fields<'db>(item: &Item<'db>) -> SmallVec<[InFile<ast::Node<'db>>; 8]> {
    let of = |kind: SyntaxKind| item.nodes().iter().filter(move |it| it.value.kind() == kind).copied();
    let mut fields: SmallVec<[InFile<ast::Node<'db>>; 8]> = SmallVec::new();
```

5. `items`, `Walk`, `Walk::finish`, `Walk::keep`. Why: the module's items under the view
   `Semantics`' root (it knows a view's file only for trees it handed out; a source file is the
   root itself); the pairs and the column hold views.

```rust
pub(super) fn items(cx: &Cx<'_, '_>) -> Vec<Item> {
    let mut walk = Walk {
        cx,
        // alloc: the one walk of the check, held by `items::Walk` and read by every pass after it
        items: Vec::new(),
        own: Vec::new_in(cx.frame()),
        // alloc: one empty column for the whole walk, replaced by the real one at the end
        empty: Rc::from([]),
    };
    for module in crate::checks::modules(cx) {
        let source = module.definition_source(cx.db);
        let file = source.file_id;
        let root = cx.sema.parse_or_expand(file);
        let list: Option<ast::AstChildren<ast::Item>> = match &source.value {
            ModuleSource::SourceFile(it) => {
                let ptr = SyntaxNodePtr::new(it.syntax());
                ast::SourceFile::cast(ptr.to_node(&root)).map(|it| it.items())
            }
            ModuleSource::Module(it) => {
                let ptr = SyntaxNodePtr::new(it.syntax());
                ast::Module::cast(ptr.to_node(&root)).and_then(|it| it.item_list()).map(|it| it.items())
            }
            ModuleSource::BlockExpr(_) => continue,
        };
        for item in list.into_iter().flatten() {
            walk.item(InFile::new(file, item), &Container::Module, module, None);
        }
    }
    walk.finish()
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    items: Vec<Item>,
    /// Each item's own nodes as they are met, with the index of the item they are of. An
    /// item's nodes interleave with the items nested in it, so they are grouped at the end.
    own: Vec<(u32, InFile<SyntaxNode>), &'a Frame>,
    /// The column each item points at until the walk ends.
    empty: Rc<[InFile<SyntaxNode>]>,
}

impl Walk<'_, '_, '_> {
    /// The items, each pointing at its own nodes in one column: the nodes grouped by item
    /// (stable, so each item's stay in preorder).
    fn finish(self) -> Vec<Item> {
```

becomes

```rust
pub(super) fn items<'db>(cx: &Cx<'_, 'db>) -> Vec<Item<'db>> {
    let mut walk = Walk {
        cx,
        // alloc: the one walk of the check, held by `items::Walk` and read by every pass after it
        items: Vec::new(),
        own: Vec::new_in(cx.frame()),
        // alloc: one empty column for the whole walk, replaced by the real one at the end
        empty: Rc::from([]),
    };
    for module in crate::checks::modules(cx) {
        let source = module.definition_source_view(cx.db);
        let file = source.file_id;
        let root = cx.root(file);
        let list: Option<ast::AstChildren<'db, ast::Item<'db>>> = match source.value {
            ModuleSourceView::SourceFile(_) => ast::SourceFile::cast(root).map(|it| it.items()),
            ModuleSourceView::Module(it) => ast::Module::cast(ast::to_node(ast::ptr(it.syntax()), root))
                .and_then(|it| it.item_list())
                .map(|it| it.items()),
            ModuleSourceView::BlockExpr(_) => continue,
        };
        for item in list.into_iter().flatten() {
            walk.item(InFile::new(file, item), Container::Module, module, None);
        }
    }
    walk.finish()
}

struct Walk<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    items: Vec<Item<'db>>,
    /// Each item's own nodes as they are met, with the index of the item they are of. An
    /// item's nodes interleave with the items nested in it, so they are grouped at the end.
    own: Vec<(u32, InFile<ast::Node<'db>>), &'a Frame>,
    /// The column each item points at until the walk ends.
    empty: Rc<[InFile<ast::Node<'db>>]>,
}

impl<'db> Walk<'_, '_, 'db> {
    /// The items, each pointing at its own nodes in one column: the nodes grouped by item
    /// (stable, so each item's stay in preorder).
    fn finish(self) -> Vec<Item<'db>> {
```

and

```rust
        let column: Rc<[InFile<SyntaxNode>]> = own.into_iter().map(|(_, node)| node).collect();
```

becomes

```rust
        let column: Rc<[InFile<ast::Node<'db>>]> = own.into_iter().map(|(_, node)| node).collect();
```

and

```rust
    fn keep(&mut self, owner: usize, node: InFile<SyntaxNode>) {
```

becomes

```rust
    fn keep(&mut self, owner: usize, node: InFile<ast::Node<'db>>) {
```

6. `Walk::item`, `Walk::expansion`. Why: items, containers and nodes by value; macros found and
   expanded by the view `Semantics`.

```rust
    fn item(&mut self, item: InFile<ast::Item>, container: &Container, module: hir::Module, in_body: Option<usize>) {
        if !enabled(self.cx, &item.value) {
            return;
        }
        if let ast::Item::MacroCall(call) = &item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, module, in_body);
            }
            return;
        }
        if self.cx.sema.is_attr_macro_call(item.as_ref()) {
            if let Some(expansion) = self.cx.sema.expand_attr_macro(&item.value) {
                self.expansion(expansion.value, container, module, in_body);
            }
            return;
        }
        let index = self.items.len();
        self.items.push(Item {
            // clone: rowan cursors' refcounts (the item and its container), no allocation
            node: item.clone(),
            container: container.clone(), // clone: a rowan cursor's refcount, or nothing for `Module`
            module,
            in_body,
            end: index + 1,
            column: Rc::clone(&self.empty),
            own: (0, 0),
        });
        // clone: a rowan cursor's refcount, kept as the item's first own node
        self.keep(index, item.as_ref().map(|it| it.syntax().clone()));
        let file = item.file_id;
        let inner = match &item.value {
            // Another module's items, visited with it.
            ast::Item::Module(_) | ast::Item::MacroRules(_) | ast::Item::MacroDef(_) => return,
            ast::Item::Impl(it) => Some(Container::Impl(item.with_value(it.clone()))), // clone: rowan refcount
            ast::Item::Trait(it) => Some(Container::Trait(item.with_value(it.clone()))), // clone: rowan refcount
            ast::Item::ExternBlock(it) => Some(Container::Extern(item.with_value(it.clone()))), // clone: rowan refcount
            _ => None,
        };
        for child in item.value.syntax().children() {
            let list = matches!(child.kind(), SyntaxKind::ASSOC_ITEM_LIST | SyntaxKind::EXTERN_ITEM_LIST);
            match &inner {
                Some(container) if list => {
                    // clone: a rowan cursor's refcount, kept as one of the item's own nodes
                    self.keep(index, InFile::new(file, child.clone()));
                    for it in child.children() {
                        self.node(file, &it, index, Some(container), module);
                    }
                }
                _ => self.node(file, &child, index, None, module),
            }
        }
        self.items[index].end = self.items.len();
    }

    /// What a macro call or an attribute macro of an item expands to, in place of it.
    fn expansion(&mut self, root: InFile<SyntaxNode>, container: &Container, module: hir::Module, in_body: Option<usize>) {
        for child in root.value.children() {
            if is_item(&child)
```

becomes

```rust
    fn item(&mut self, item: InFile<ast::Item<'db>>, container: Container<'db>, module: hir::Module, in_body: Option<usize>) {
        if !enabled(self.cx, item.value) {
            return;
        }
        if let ast::Item::MacroCall(call) = item.value {
            if let Some(expansion) = self.expand(item.with_value(call)) {
                self.expansion(expansion, container, module, in_body);
            }
            return;
        }
        if self.cx.vsema.is_attr_macro_call(item) {
            if let Some(expansion) = self.cx.vsema.expand_attr_macro(item.value) {
                self.expansion(expansion.value, container, module, in_body);
            }
            return;
        }
        let index = self.items.len();
        self.items.push(Item {
            node: item,
            container,
            module,
            in_body,
            end: index + 1,
            column: Rc::clone(&self.empty),
            own: (0, 0),
        });
        self.keep(index, item.map(|it| it.syntax()));
        let file = item.file_id;
        let inner = match item.value {
            // Another module's items, visited with it.
            ast::Item::Module(_) | ast::Item::MacroRules(_) | ast::Item::MacroDef(_) => return,
            ast::Item::Impl(it) => Some(Container::Impl(item.with_value(it))),
            ast::Item::Trait(it) => Some(Container::Trait(item.with_value(it))),
            ast::Item::ExternBlock(it) => Some(Container::Extern(item.with_value(it))),
            _ => None,
        };
        for child in item.value.syntax().children() {
            let list = matches!(child.kind(), SyntaxKind::ASSOC_ITEM_LIST | SyntaxKind::EXTERN_ITEM_LIST);
            match inner {
                Some(container) if list => {
                    self.keep(index, InFile::new(file, child));
                    for it in child.children() {
                        self.node(file, it, index, Some(container), module);
                    }
                }
                _ => self.node(file, child, index, None, module),
            }
        }
        self.items[index].end = self.items.len();
    }

    /// What a macro call or an attribute macro of an item expands to, in place of it.
    fn expansion(&mut self, root: InFile<ast::Node<'db>>, container: Container<'db>, module: hir::Module, in_body: Option<usize>) {
        for child in root.value.children() {
            if is_item(child)
```

7. `Walk::node`, `Walk::expand`, `is_item`. Why: views by value, nothing cloned; the expansion
   found by the view `Semantics`, its root one it hands out.

```rust
    fn node(&mut self, file: HirFileId, node: &SyntaxNode, owner: usize, direct: Option<&Container>, module: hir::Module) {
        let kind = node.kind();
        if ast::AnyHasAttrs::can_cast(kind)
            // clone: a rowan cursor's refcount, only for nodes that can have attributes
            && let Some(it) = ast::AnyHasAttrs::cast(node.clone())
            && !enabled(self.cx, &it)
        {
            return;
        }
        if is_item(node) {
            // clone: a rowan cursor's refcount; the item is walked as its own
            let item = ast::Item::cast(node.clone()).expect("an item");
            match direct {
                Some(container) => self.item(InFile::new(file, item), container, module, self.items[owner].in_body),
                None => self.item(InFile::new(file, item), &Container::Module, module, Some(owner)),
            }
            return;
        }
        if kind == SyntaxKind::MACRO_CALL
            // clone: a rowan cursor's refcount, only for macro calls
            && let Some(call) = ast::MacroCall::cast(node.clone())
        {
            // clone: a rowan cursor's refcount, kept as one of the item's own nodes
            self.keep(owner, InFile::new(file, node.clone()));
            let Some(expansion) = self.expand(InFile::new(file, &call)) else { return };
            match expansion.value.kind() {
                SyntaxKind::MACRO_ITEMS | SyntaxKind::MACRO_STMTS => {
                    for child in expansion.value.children() {
                        self.node(expansion.file_id, &child, owner, direct, module);
                    }
                }
                _ => self.node(expansion.file_id, &expansion.value, owner, None, module),
            }
            return;
        }
        // clone: a rowan cursor's refcount, kept as one of the item's own nodes
        self.keep(owner, InFile::new(file, node.clone()));
        for child in node.children() {
            self.node(file, &child, owner, None, module);
        }
    }

    fn expand(&self, call: InFile<&ast::MacroCall>) -> Option<InFile<SyntaxNode>> {
        let id = self.cx.sema.to_def2(call)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.sema.parse_or_expand(file)))
    }
}

/// Whether `node` is an item of HIR's: an item, an associated item or an item of an `extern`
/// block, not a macro call (which is what it expands to) nor an `asm!` in a body.
fn is_item(node: &SyntaxNode) -> bool {
```

becomes

```rust
    fn node(&mut self, file: HirFileId, node: ast::Node<'db>, owner: usize, direct: Option<Container<'db>>, module: hir::Module) {
        let kind = node.kind();
        if ast::AnyHasAttrs::can_cast(kind)
            && let Some(it) = ast::AnyHasAttrs::cast(node)
            && !enabled(self.cx, it)
        {
            return;
        }
        if is_item(node) {
            let item = ast::Item::cast(node).expect("an item");
            match direct {
                Some(container) => self.item(InFile::new(file, item), container, module, self.items[owner].in_body),
                None => self.item(InFile::new(file, item), Container::Module, module, Some(owner)),
            }
            return;
        }
        if kind == SyntaxKind::MACRO_CALL
            && let Some(call) = ast::MacroCall::cast(node)
        {
            self.keep(owner, InFile::new(file, node));
            let Some(expansion) = self.expand(InFile::new(file, call)) else { return };
            match expansion.value.kind() {
                SyntaxKind::MACRO_ITEMS | SyntaxKind::MACRO_STMTS => {
                    for child in expansion.value.children() {
                        self.node(expansion.file_id, child, owner, direct, module);
                    }
                }
                _ => self.node(expansion.file_id, expansion.value, owner, None, module),
            }
            return;
        }
        self.keep(owner, InFile::new(file, node));
        for child in node.children() {
            self.node(file, child, owner, None, module);
        }
    }

    fn expand(&self, call: InFile<ast::MacroCall<'db>>) -> Option<InFile<ast::Node<'db>>> {
        let id = self.cx.vsema.to_def2(call)?;
        let file: HirFileId = id.into();
        Some(InFile::new(file, self.cx.root(file)))
    }
}

/// Whether `node` is an item of HIR's: an item, an associated item or an item of an `extern`
/// block, not a macro call (which is what it expands to) nor an `asm!` in a body.
fn is_item(node: ast::Node<'_>) -> bool {
```

8. `enabled`, `any_named`, `attr`, `named`. Why: owners by value (views are `Copy`), the view
   `cfg_attr` expansion, the meta's parent compared as views compare (no cursor made); `any_named`
   hands `f` the meta by value.

```rust
pub(super) fn enabled<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N) -> bool {
    if owner.attrs().next().is_none() {
        return true;
    }
    let cfg = cx.krate.cfg(cx.db);
    let off = hir_expand::attrs::expand_cfg_attr(
        owner.attrs(),
        || cfg,
        |meta, _| {
            if let ast::Meta::CfgMeta(meta) = meta
                && let Some(predicate) = meta.cfg_predicate()
                && cfg.check(&CfgExpr::parse_from_ast(predicate)) == Some(false)
```

becomes

```rust
pub(super) fn enabled<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>) -> bool {
    if owner.attrs().next().is_none() {
        return true;
    }
    let cfg = cx.krate.cfg(cx.db);
    let off = hir_expand::attrs::expand_cfg_attr_view(
        owner.attrs(),
        || cfg,
        |meta, _| {
            if let ast::Meta::CfgMeta(meta) = meta
                && let Some(predicate) = meta.cfg_predicate()
                && cfg.check(&CfgExpr::parse_from_view(predicate)) == Some(false)
```

and

```rust
pub(super) fn any_named<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N, name: &str, mut f: impl FnMut(&ast::Meta) -> bool) -> bool {
    named(cx, owner, name, |meta, _| match f(&meta) {
```

becomes

```rust
pub(super) fn any_named<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>, name: &str, mut f: impl FnMut(ast::Meta<'t>) -> bool) -> bool {
    named(cx, owner, name, |meta, _| match f(meta) {
```

and

```rust
pub(super) fn attr<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N, name: &str) -> Option<(ast::Meta, TextRange)> {
```

becomes

```rust
pub(super) fn attr<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>, name: &str) -> Option<(ast::Meta<'t>, TextRange)> {
```

and

```rust
fn named<N: HasAttrs, B>(
    cx: &Cx<'_, '_>,
    owner: &N,
    name: &str,
    mut f: impl FnMut(ast::Meta, TextRange) -> ControlFlow<B>,
) -> Option<B> {
    if owner.attrs().next().is_none() {
        return None;
    }
    let cfg = cx.krate.cfg(cx.db);
    hir_expand::attrs::expand_cfg_attr(
        owner.attrs(),
        || cfg,
        |meta, attr| {
            if !meta.simple_name().is_some_and(|it| it == name) {
                return ControlFlow::Continue(());
            }
            let range = match meta.syntax().parent().is_some_and(|it| &it == attr.syntax()) {
```

becomes

```rust
fn named<'t, B>(
    cx: &Cx<'_, '_>,
    owner: impl HasAttrs<'t>,
    name: &str,
    mut f: impl FnMut(ast::Meta<'t>, TextRange) -> ControlFlow<B>,
) -> Option<B> {
    if owner.attrs().next().is_none() {
        return None;
    }
    let cfg = cx.krate.cfg(cx.db);
    hir_expand::attrs::expand_cfg_attr_view(
        owner.attrs(),
        || cfg,
        |meta, attr| {
            if !meta.simple_name().is_some_and(|it| it == name) {
                return ControlFlow::Continue(());
            }
            let range = match meta.syntax().parent() == Some(attr.syntax()) {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)
> rewritten 2026-10-29 22:00 by frontend-tree-move-diag-lints-items (second agent) against the
> walk with one column of own nodes.

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs).
Also needs `cx::{item_range_view, def_range_view, signature_range_view}`
(frontend-tree-move-diag-lower's `cx.rs` twins). `def_span`'s callers outside these paths
(`checks/attrs/parsers/stability.rs`, frontend-tree-move-diag-lower, told by tmux) pass the view.

1. The imports. Why: views.

```rust
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange, TextSize,
    ast::{self, HasName},
};

use self::walk::Item;
use super::effective::Effective;
use crate::{
    cx::{Cx, def_range, item_range, signature_range},
```

becomes

```rust
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode, HasName},
};

use self::walk::Item;
use super::effective::Effective;
use crate::{
    cx::{Cx, def_range_view, item_range_view, signature_range_view},
```

2. `Walk`. Why: its items hold views borrowed from the database for `'db`.

```rust
pub(crate) struct Walk {
    items: Vec<Item>,
}

impl Walk {
    pub(crate) fn of(cx: &Cx<'_, '_>) -> Walk {
```

becomes

```rust
pub(crate) struct Walk<'db> {
    items: Vec<Item<'db>>,
}

impl<'db> Walk<'db> {
    pub(crate) fn of(cx: &Cx<'_, 'db>) -> Walk<'db> {
```

3. The passes' signatures. Why: the same.

```rust
pub(super) fn private_in_public(cx: &Cx<'_, '_>, walk: &Walk, effective: &Effective) -> Vec<Found> {
```

becomes

```rust
pub(super) fn private_in_public<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>, effective: &Effective) -> Vec<Found> {
```

and

```rust
pub(super) fn after_abort(cx: &Cx<'_, '_>, walk: &Walk, effective: &Effective) -> Vec<Found> {
```

becomes

```rust
pub(super) fn after_abort<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>, effective: &Effective) -> Vec<Found> {
```

and

```rust
pub(super) fn clashing_extern_declarations(cx: &Cx<'_, '_>, walk: &Walk) -> Vec<Found> {
```

becomes

```rust
pub(super) fn clashing_extern_declarations<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Found> {
```

4. `def_span`. Why: the node by value; a child found by a kind read per child (`cast` tests the
   kind first), nothing cloned for `signature_range_view`.

```rust
pub(crate) fn def_span(node: &SyntaxNode) -> TextRange {
    fn child<N: AstNode>(node: &SyntaxNode) -> Option<N> {
        node.children_where(N::can_cast).find_map(N::cast)
    }
    let range = item_range(node);
    let until = |end: Option<TextSize>| TextRange::new(range.start(), end.unwrap_or(range.end()).max(range.start()));
    let end_of = |it: Option<&SyntaxNode>| it.map(|it| it.text_range().end());
    let named = |name: Option<ast::Name>, generics: Option<ast::GenericParamList>| {
        let name = end_of(name.as_ref().map(AstNode::syntax));
        until(end_of(generics.as_ref().map(AstNode::syntax)).or(name))
    };
    match node.kind() {
        // clone: a rowan cursor's refcount, for the `ast::Fn` view `signature_range` takes
        SyntaxKind::FN => ast::Fn::cast(node.clone()).map_or(range, |it| signature_range(&it)),
        SyntaxKind::IMPL => def_range(node),
        SyntaxKind::CONST | SyntaxKind::STATIC => until(end_of(child::<ast::Type>(node).as_ref().map(AstNode::syntax))),
        SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD | SyntaxKind::EXTERN_BLOCK => range,
        SyntaxKind::USE_TREE => node.text_range(),
        SyntaxKind::TRAIT | SyntaxKind::TYPE_ALIAS => match child::<ast::TypeBoundList>(node).and_then(|it| it.bounds().last()) {
            Some(bound) => until(end_of(Some(bound.syntax()))),
            None => named(child(node), child(node)),
        },
        SyntaxKind::EXTERN_CRATE => {
            let rename = child::<ast::Rename>(node).and_then(|it| it.name());
            let name_ref = child::<ast::NameRef>(node).map(|it| it.syntax().text_range().end());
            until(end_of(rename.as_ref().map(AstNode::syntax)).or(name_ref))
        }
        _ => match child::<ast::Name>(node) {
```

becomes

```rust
pub(crate) fn def_span<'t>(node: ast::Node<'t>) -> TextRange {
    fn child<'t, N: AstNode<'t>>(node: ast::Node<'t>) -> Option<N> {
        node.children().find_map(N::cast)
    }
    let range = item_range_view(node);
    let until = |end: Option<TextSize>| TextRange::new(range.start(), end.unwrap_or(range.end()).max(range.start()));
    let end_of = |it: Option<ast::Node<'t>>| it.map(|it| it.text_range().end());
    let named = |name: Option<ast::Name<'t>>, generics: Option<ast::GenericParamList<'t>>| {
        let name = end_of(name.map(|it| it.syntax()));
        until(end_of(generics.map(|it| it.syntax())).or(name))
    };
    match node.kind() {
        SyntaxKind::FN => ast::Fn::cast(node).map_or(range, signature_range_view),
        SyntaxKind::IMPL => def_range_view(node),
        SyntaxKind::CONST | SyntaxKind::STATIC => until(end_of(child::<ast::Type<'t>>(node).map(|it| it.syntax()))),
        SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD | SyntaxKind::EXTERN_BLOCK => range,
        SyntaxKind::USE_TREE => node.text_range(),
        SyntaxKind::TRAIT | SyntaxKind::TYPE_ALIAS => match child::<ast::TypeBoundList<'t>>(node).and_then(|it| it.bounds().last()) {
            Some(bound) => until(end_of(Some(bound.syntax()))),
            None => named(child(node), child(node)),
        },
        SyntaxKind::EXTERN_CRATE => {
            let rename = child::<ast::Rename<'t>>(node).and_then(|it| it.name());
            let name_ref = child::<ast::NameRef<'t>>(node).map(|it| it.syntax().text_range().end());
            until(end_of(rename.map(|it| it.syntax())).or(name_ref))
        }
        _ => match child::<ast::Name<'t>>(node) {
```

5. `item_id`. Why: the item is a `Copy` view; its defs from the view `Semantics`; a name's text
   is `&str` (the `String` stays: the fact keeps it).

```rust
fn item_id(cx: &Cx<'_, '_>, item: &Item) -> Option<(ItemId, String)> {
    let node = item.node.as_ref();
    fn name(it: &impl HasName) -> String {
        // alloc: the name in a firing lint's fact; every caller asks only once its lint fires
        it.name().map_or_else(|| "_".to_owned(), |it| it.text().to_string())
    }
    let id = match node.value {
        ast::Item::Fn(it) => ItemId::Function(FunctionId::try_from(cx.sema.to_def2(node.with_value(it))?).ok()?),
        ast::Item::Struct(it) => ItemId::Adt(AdtId::StructId(cx.sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Enum(it) => ItemId::Adt(AdtId::EnumId(cx.sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Union(it) => ItemId::Adt(AdtId::UnionId(cx.sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Trait(it) => ItemId::Trait(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Const(it) => ItemId::Const(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Static(it) => ItemId::Static(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::TypeAlias(it) => ItemId::Alias(cx.sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Impl(it) => {
            let id = hir_def::ImplId::try_from(cx.sema.to_def2(node.with_value(it))?).ok()?;
```

becomes

```rust
fn item_id<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<(ItemId, String)> {
    let node = item.node;
    fn name<'t>(it: impl HasName<'t>) -> String {
        // alloc: the name in a firing lint's fact; every caller asks only once its lint fires
        it.name().map_or_else(|| "_".to_owned(), |it| it.text().to_owned())
    }
    let sema = &cx.vsema;
    let id = match node.value {
        ast::Item::Fn(it) => ItemId::Function(FunctionId::try_from(sema.to_def2(node.with_value(it))?).ok()?),
        ast::Item::Struct(it) => ItemId::Adt(AdtId::StructId(sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Enum(it) => ItemId::Adt(AdtId::EnumId(sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Union(it) => ItemId::Adt(AdtId::UnionId(sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Trait(it) => ItemId::Trait(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Const(it) => ItemId::Const(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Static(it) => ItemId::Static(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::TypeAlias(it) => ItemId::Alias(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Impl(it) => {
            let id = hir_def::ImplId::try_from(sema.to_def2(node.with_value(it))?).ok()?;
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/track_caller.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs),
and needs `cx::item_range_view` (frontend-tree-move-diag-lower).

1. Imports. Why: views.

```rust
use syntax::{AstNode, ast};

use super::walk::{self, Item};
use crate::{
    cx::{Cx, item_range},
```

becomes

```rust
use syntax::view::{self as ast, AstNode};

use super::walk::{self, Item};
use crate::{
    cx::{Cx, item_range_view},
```

2. `item`. Why: the item's view by value, passed to `walk::attr` by value.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, enabled: bool) -> Option<Found> {
    let ast::Item::Fn(f) = &item.node.value else { return None };
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, enabled: bool) -> Option<Found> {
    let ast::Item::Fn(f) = item.node.value else { return None };
```

and

```rust
    diag.span_label(span(item_range(f.syntax())), "this function will not propagate the caller location");
```

becomes

```rust
    diag.span_label(span(item_range_view(f.syntax())), "this function will not propagate the caller location");
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/traits.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs).

1. Imports. Why: views (`NodeOrToken` is the views' element type too).

```rust
use syntax::{AstNode, NodeOrToken, TextRange, TextSize, ast};
```

becomes

```rust
use syntax::{
    NodeOrToken, TextRange, TextSize,
    view::{self as ast, AstNode},
};
```

2. `async_fn`. Why: the item and its container are `Copy` views; the tokens after `async` are
   views, stepped by value.

```rust
pub(super) fn async_fn(cx: &Cx<'_, '_>, item: &Item, effective: &Effective, krate: &Krate) -> Option<Found> {
    let (ast::Item::Fn(f), Container::Trait(_)) = (&item.node.value, &item.container) else { return None };
```

becomes

```rust
pub(super) fn async_fn<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective, krate: &Krate) -> Option<Found> {
    let (ast::Item::Fn(f), Container::Trait(_)) = (item.node.value, item.container) else { return None };
```

and

```rust
    while let Some(NodeOrToken::Token(token)) = &next {
```

becomes

```rust
    while let Some(NodeOrToken::Token(token)) = next {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/statics.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` and `items.rs` headings above
(same needs; `def_span` takes the node by value).

1. Imports. Why: views.

```rust
use syntax::{
    AstNode, SyntaxNodePtr,
    ast::{self, HasGenericParams},
};
```

becomes

```rust
use syntax::view::{self as ast, AstNode, HasGenericParams};
```

2. Signatures and the item's view by value. Why: `Item<'db>`; views are `Copy`.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
```

and

```rust
fn type_param_defaults(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
    let params = match (&item.node.value, &item.container) {
```

becomes

```rust
fn type_param_defaults<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
    let params = match (item.node.value, item.container) {
```

3. The default's id. Why: a pointer made from the view.

```rust
        let at = item.node.with_value(SyntaxNodePtr::new(param.syntax()));
```

becomes

```rust
        let at = item.node.with_value(ast::ptr(param.syntax()));
```

4. `uninhabited`. Why: the static's def from the view `Semantics`.

```rust
fn uninhabited(cx: &Cx<'_, '_>, item: &Item) -> Option<Found> {
    let ast::Item::Static(it) = &item.node.value else { return None };
    let def = cx.sema.to_def2(item.node.with_value(it))?;
```

becomes

```rust
fn uninhabited<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Found> {
    let ast::Item::Static(it) = item.node.value else { return None };
    let def = cx.vsema.to_def2(item.node.with_value(it))?;
```
Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/derives.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs),
and needs `cx::item_range_view` (frontend-tree-move-diag-lower).

1. Imports. Why: views.

```rust
use syntax::{
    AstNode,
    ast::{self, HasGenericParams},
};
```

becomes

```rust
use syntax::view::{self as ast, AstNode, HasGenericParams};
```

and

```rust
    cx::{Cx, item_range},
```

becomes

```rust
    cx::{Cx, item_range_view},
```

2. `adt`. Why: the item's view by value; defs from the view `Semantics`.

```rust
fn adt(cx: &Cx<'_, '_>, item: &Item) -> Option<(Adt, bool)> {
    let node = item.node.as_ref();
    let (adt, params): (Adt, Option<ast::GenericParamList>) = match node.value {
        ast::Item::Struct(it) => (cx.sema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Enum(it) => (cx.sema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Union(it) => (cx.sema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
```

becomes

```rust
fn adt<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<(Adt, bool)> {
    let node = item.node;
    let (adt, params): (Adt, Option<ast::GenericParamList<'_>>) = match node.value {
        ast::Item::Struct(it) => (cx.vsema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Enum(it) => (cx.vsema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
        ast::Item::Union(it) => (cx.vsema.to_def2(node.with_value(it))?.into(), it.generic_param_list()),
```

3. `copy`. Why: owners passed by value.

```rust
pub(super) fn copy(cx: &Cx<'_, '_>, item: &Item, effective: &Effective, krate: &Krate) -> Vec<Found> {
```

becomes

```rust
pub(super) fn copy<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective, krate: &Krate) -> Vec<Found> {
```

and

```rust
    let non_exhaustive = walk::attr(cx, &item.node.value, "non_exhaustive").is_some()
        || item.own::<ast::Variant>().any(|it| walk::attr(cx, &it.value, "non_exhaustive").is_some());
```

becomes

```rust
    let non_exhaustive = walk::attr(cx, item.node.value, "non_exhaustive").is_some()
        || item.own::<ast::Variant<'_>>().any(|it| walk::attr(cx, it.value, "non_exhaustive").is_some());
```

4. The two spans (`copy` and `debug`, the same line in each). Why: `item_range_view`.

```rust
    let span = cx.span(item.node.with_value(item_range(item.node.value.syntax())));
```

becomes (both)

```rust
    let span = cx.span(item.node.with_value(item_range_view(item.node.value.syntax())));
```

5. `debug`. Why: `Item<'db>`.

```rust
pub(super) fn debug(cx: &Cx<'_, '_>, item: &Item, effective: &Effective, krate: &Krate) -> Vec<Found> {
```

becomes

```rust
pub(super) fn debug<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective, krate: &Krate) -> Vec<Found> {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/c_void.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs)
and the `externs.rs` heading below (`abi_of` takes a view); needs `hir::view::Semantics::resolve_path`.

1. Imports. Why: views.

```rust
use syntax::{AstNode, SyntaxKind, SyntaxNodePtr, TextRange, TextSize, ast};
```

becomes

```rust
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode},
};
```

2. `item`. Why: own nodes are `Copy` views, cast after the kind test with nothing cloned.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, krate: &Krate) -> Vec<Found> {
    let Some(c_void) = krate.c_void else { return Vec::new() }; // alloc: empty, allocates nothing
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    for node in item.nodes() {
        let kind = node.value.kind();
        if !matches!(kind, SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR | SyntaxKind::FN_PTR_TYPE) {
            continue;
        }
        let at = node.as_ref().map(SyntaxNodePtr::new);
        if kind == SyntaxKind::FN
            // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
            && let Some(f) = ast::Fn::cast(node.value.clone())
        {
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, krate: &Krate) -> Vec<Found> {
    let Some(c_void) = krate.c_void else { return Vec::new() }; // alloc: empty, allocates nothing
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    for &node in item.nodes() {
        let kind = node.value.kind();
        if !matches!(kind, SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR | SyntaxKind::FN_PTR_TYPE) {
            continue;
        }
        let at = node.map(ast::ptr);
        if kind == SyntaxKind::FN
            && let Some(f) = ast::Fn::cast(node.value)
        {
```

and

```rust
            && let Some(closure) = ast::ClosureExpr::cast(node.value.clone()) // clone: as above
        {
            let Some(ret) = closure.ret_type() else { continue };
            check(cx, node.with_value(ret), None, true, c_void, at, &mut found);
        } else if let Some(ptr) = ast::FnPtrType::cast(node.value.clone()) { // clone: as above
```

becomes

```rust
            && let Some(closure) = ast::ClosureExpr::cast(node.value)
        {
            let Some(ret) = closure.ret_type() else { continue };
            check(cx, node.with_value(ret), None, true, c_void, at, &mut found);
        } else if let Some(ptr) = ast::FnPtrType::cast(node.value) {
```

3. `check`. Why: the return type's view, of `'db` (the view `Semantics<'db>` takes `'db` views
   only); its path resolved by the view `Semantics`.

```rust
fn check(
    cx: &Cx<'_, '_>,
    ret: InFile<ast::RetType>,
```

becomes

```rust
fn check<'db>(
    cx: &Cx<'_, 'db>,
    ret: InFile<ast::RetType<'db>>,
```

and

```rust
    let Some(PathResolution::Def(ModuleDef::Adt(hir::Adt::Enum(it)))) = cx.sema.resolve_path(&path) else { return };
```

becomes

```rust
    let Some(PathResolution::Def(ModuleDef::Adt(hir::Adt::Enum(it)))) = cx.vsema.resolve_path(path) else { return };
```

and

```rust
    let suggestion = ret.with_value(extend_to_prev_paren(&ret.value, range, params_end));
```

becomes

```rust
    let suggestion = ret.with_value(extend_to_prev_paren(ret.value, range, params_end));
```

4. `extend_to_prev_paren`. Why: the root's text is one `&str` slice of the source a view shares;
   the file's text is no longer copied into a `String`.

```rust
fn extend_to_prev_paren(ret: &ast::RetType, range: TextRange, params_end: Option<TextSize>) -> TextRange {
    if let Some(end) = params_end {
        return TextRange::new(end, range.end());
    }
    // clone: the root, read only for the suggestion of a lint that fires
    let root = ret.syntax().ancestors().last().unwrap_or_else(|| ret.syntax().clone());
    let text = root.text().to_string(); // alloc: the file's text, for the firing lint's suggestion
    let before = &text[..usize::from(range.start())];
```

becomes

```rust
fn extend_to_prev_paren(ret: ast::RetType<'_>, range: TextRange, params_end: Option<TextSize>) -> TextRange {
    if let Some(end) = params_end {
        return TextRange::new(end, range.end());
    }
    let root = ret.syntax().ancestors().last().unwrap_or(ret.syntax());
    let before = &root.text()[..usize::from(range.start())];
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/mangle.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` and `items.rs` headings above
(same needs), and needs `cx::item_range_view` (frontend-tree-move-diag-lower).

1. Imports. Why: views.

```rust
use syntax::{
    AstNode, SyntaxNodePtr, TextRange,
    ast::{self, HasGenericParams, HasName},
};
```

becomes

```rust
use syntax::{
    TextRange,
    view::{self as ast, AstNode, HasAttrs, HasGenericParams, HasName},
};
```

and

```rust
    cx::{Cx, item_range},
```

becomes

```rust
    cx::{Cx, item_range_view},
```

2. `item`. Why: the item and its container are `Copy` views, passed by value.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item) -> Option<Found> {
    match &item.node.value {
        ast::Item::Fn(it) if matches!(item.container, Container::Module | Container::Impl(_)) => {
            let attr = attr_span(cx, it, "export_name").or_else(|| attr_span(cx, it, "no_mangle"))?;
            let impl_generic = match &item.container {
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Found> {
    match item.node.value {
        ast::Item::Fn(it) if matches!(item.container, Container::Module | Container::Impl(_)) => {
            let attr = attr_span(cx, it, "export_name").or_else(|| attr_span(cx, it, "no_mangle"))?;
            let impl_generic = match item.container {
```

and

```rust
            let range = item_range(it.syntax());
```

becomes

```rust
            let range = item_range_view(it.syntax());
```

and

```rust
            let at = item.node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
```

becomes

```rust
            let at = item.node.map(|it| ast::ptr(it.syntax()));
```

3. `attr_span`, `generic`, `impl_trait_param`, `is_fn_sugar`. Why: views by value.

```rust
fn attr_span<N: syntax::ast::HasAttrs>(cx: &Cx<'_, '_>, owner: &N, name: &str) -> Option<TextRange> {
```

becomes

```rust
fn attr_span<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>, name: &str) -> Option<TextRange> {
```

and

```rust
fn generic(params: Option<ast::GenericParamList>) -> bool {
```

becomes

```rust
fn generic(params: Option<ast::GenericParamList<'_>>) -> bool {
```

and

```rust
fn impl_trait_param(f: &ast::Fn) -> bool {
    f.param_list().into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty()).any(|ty| {
        ty.syntax().descendants().any(|it| {
            ast::ImplTraitType::can_cast(it.kind())
                && !it.ancestors().any(|it| ast::FnPtrType::can_cast(it.kind()) || ast::PathType::cast(it).is_some_and(|p| is_fn_sugar(&p)))
        })
    })
}

/// A `Fn(..) -> ..` path, in whose arguments an `impl Trait` is an error, not a parameter.
fn is_fn_sugar(ty: &ast::PathType) -> bool {
```

becomes

```rust
fn impl_trait_param(f: ast::Fn<'_>) -> bool {
    f.param_list().into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty()).any(|ty| {
        ty.syntax().descendants().any(|it| {
            ast::ImplTraitType::can_cast(it.kind())
                && !it.ancestors().any(|it| ast::FnPtrType::can_cast(it.kind()) || ast::PathType::cast(it).is_some_and(is_fn_sugar))
        })
    })
}

/// A `Fn(..) -> ..` path, in whose arguments an `impl Trait` is an error, not a parameter.
fn is_fn_sugar(ty: ast::PathType<'_>) -> bool {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/visibility.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs)
and the `privacy.rs` heading below (`interface_node`'s caller); needs
`hir::view::Semantics::source` with `HasSourceView` (frontend-tree-move-hir; `Field`'s view source
is `FieldSourceView<'db>`, itself a view `AstNode`).

1. Imports. Why: views; the sources come from the view `Semantics`, so `HasSource`,
   `FieldSource` and `AstPtr` go.

```rust
use hir::{Adt, FieldSource, HasSource, HasVisibility, InFile, ModuleDef, Visibility};
use syntax::{AstNode, AstPtr, SyntaxNode, ast};
```

becomes

```rust
use hir::{Adt, HasVisibility, InFile, ModuleDef, Visibility};
use syntax::view::{self as ast, AstNode};
```

2. `def`. Why: the item's view by value; defs from the view `Semantics`.

```rust
pub(super) fn def(cx: &Cx<'_, '_>, item: &Item) -> Option<Def> {
    let node = item.node.as_ref();
    let def: ModuleDef = match node.value {
        ast::Item::Fn(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Struct(it) => Adt::from(cx.sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Enum(it) => Adt::from(cx.sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Union(it) => Adt::from(cx.sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Trait(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Const(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Static(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::TypeAlias(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Module(it) => cx.sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Impl(it) => return Some(Def::Impl(cx.sema.to_def2(node.with_value(it))?)),
```

becomes

```rust
pub(super) fn def<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Def> {
    let node = item.node;
    let sema = &cx.vsema;
    let def: ModuleDef = match node.value {
        ast::Item::Fn(it) => sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Struct(it) => Adt::from(sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Enum(it) => Adt::from(sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Union(it) => Adt::from(sema.to_def2(node.with_value(it))?).into(),
        ast::Item::Trait(it) => sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Const(it) => sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Static(it) => sema.to_def2(node.with_value(it))?.into(),
        ast::Item::TypeAlias(it) => sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Module(it) => sema.to_def2(node.with_value(it))?.into(),
        ast::Item::Impl(it) => return Some(Def::Impl(sema.to_def2(node.with_value(it))?)),
```

3. `restricted_to`. Why: `Item<'db>`.

```rust
pub(super) fn restricted_to(cx: &Cx<'_, '_>, item: &Item) -> Option<hir::Module> {
```

becomes

```rust
pub(super) fn restricted_to<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<hir::Module> {
```

4. `interface_node`. Why: the def's source as a view the view `Semantics` hands out (and so
   knows the file of), where the handle version re-found the node under the handle
   `Semantics`' root; nothing cloned.

```rust
pub(super) fn interface_node(cx: &Cx<'_, '_>, def: Def) -> Option<InFile<SyntaxNode>> {
    fn of<N: AstNode>(cx: &Cx<'_, '_>, src: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
        let src = src?;
        let node = cx.node(src.as_ref().map(AstPtr::new));
        Some(node.map(|it| it.syntax().clone())) // clone: a rowan cursor's refcount, no allocation
    }
    let db = cx.db;
    match def {
        Def::Item(ModuleDef::Function(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Adt(Adt::Struct(it))) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Adt(Adt::Union(it))) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Adt(Adt::Enum(it))) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Trait(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::TypeAlias(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Const(it)) => of(cx, it.source(db)),
        Def::Item(ModuleDef::Static(it)) => of(cx, it.source(db)),
        Def::Impl(it) => of(cx, it.source(db)),
        Def::Field(it) => match it.source(db)? {
            InFile { file_id, value: FieldSource::Named(it) } => of(cx, Some(InFile::new(file_id, it))),
            InFile { file_id, value: FieldSource::Pos(it) } => of(cx, Some(InFile::new(file_id, it))),
        },
        _ => None,
    }
}
```

becomes

```rust
pub(super) fn interface_node<'db>(cx: &Cx<'_, 'db>, def: Def) -> Option<InFile<ast::Node<'db>>> {
    fn of<'db, N: AstNode<'db>>(src: Option<InFile<N>>) -> Option<InFile<ast::Node<'db>>> {
        Some(src?.map(|it| it.syntax()))
    }
    let sema = &cx.vsema;
    match def {
        Def::Item(ModuleDef::Function(it)) => of(sema.source(it)),
        Def::Item(ModuleDef::Adt(Adt::Struct(it))) => of(sema.source(it)),
        Def::Item(ModuleDef::Adt(Adt::Union(it))) => of(sema.source(it)),
        Def::Item(ModuleDef::Adt(Adt::Enum(it))) => of(sema.source(it)),
        Def::Item(ModuleDef::Trait(it)) => of(sema.source(it)),
        Def::Item(ModuleDef::TypeAlias(it)) => of(sema.source(it)),
        Def::Item(ModuleDef::Const(it)) => of(sema.source(it)),
        Def::Item(ModuleDef::Static(it)) => of(sema.source(it)),
        Def::Impl(it) => of(sema.source(it)),
        Def::Field(it) => of(sema.source(it)),
        _ => None,
    }
}
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/publicity.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` and `items.rs` headings above
(same needs).

1. Imports. Why: views.

```rust
use syntax::{
    AstNode,
    ast::{self, HasVisibility},
};
```

becomes

```rust
use syntax::view::{self as ast, AstNode, HasVisibility};
```

2. `item`. Why: the item and its container are `Copy` views.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, effective: &Effective) -> Vec<Found> {
    let node = &item.node.value;
    let Some(vis) = public(node) else { return Vec::new() }; // alloc: empty, no allocation
    let exportable = match &item.container {
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective) -> Vec<Found> {
    let node = item.node.value;
    let Some(vis) = public(node) else { return Vec::new() }; // alloc: empty, no allocation
    let exportable = match item.container {
```

3. `public`, `leaves`. Why: views by value.

```rust
fn public(node: &ast::Item) -> Option<ast::Visibility> {
```

becomes

```rust
fn public(node: ast::Item<'_>) -> Option<ast::Visibility<'_>> {
```

and

```rust
fn leaves(tree: ast::UseTree, out: &mut Vec<syntax::TextRange>) {
```

becomes

```rust
fn leaves(tree: ast::UseTree<'_>, out: &mut Vec<syntax::TextRange>) {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1; diagnostics-next2 told too); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/runtime_symbols.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs);
needs `cx::{item_range_view, signature_range_view}` (frontend-tree-move-diag-lower) and
`hir::view::HasSourceView` (frontend-tree-move-hir).

1. Imports. Why: views; a foreign function's attributes read from its view source.

```rust
use hir::{HasSource as _, ModuleDef};
```

becomes

```rust
use hir::{ModuleDef, view::HasSourceView as _};
```

and

```rust
use syntax::{
    AstNode, SyntaxNodePtr,
    ast::{self, HasAttrs},
};

use super::walk::{Container, Item};
use crate::{
    cx::{Cx, item_range, signature_range},
```

becomes

```rust
use syntax::view::{self as ast, AstNode, HasAttrs};

use super::walk::{Container, Item};
use crate::{
    cx::{Cx, item_range_view, signature_range_view},
```

2. `Canonical::of`, `is_canonical`. Why: `Item<'db>`; the attribute's name is a `&str` of the view.

```rust
    pub(super) fn of(cx: &Cx<'_, '_>, items: &[Item]) -> Canonical {
```

becomes

```rust
    pub(super) fn of<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>]) -> Canonical {
```

and

```rust
    let Some(source) = f.source(cx.db) else { return false };
```

becomes

```rust
    let Some(source) = f.source_view(cx.db) else { return false };
```

3. `candidate`. Why: the item's view by value; defs from the view `Semantics`.

```rust
fn candidate(cx: &Cx<'_, '_>, item: &Item) -> Option<Candidate> {
```

becomes

```rust
fn candidate<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Candidate> {
```

and

```rust
    match &item.node.value {
        ast::Item::Fn(f) => {
            let def = cx.sema.to_def2(item.node.with_value(f))?;
```

becomes

```rust
    match item.node.value {
        ast::Item::Fn(f) => {
            let def = cx.vsema.to_def2(item.node.with_value(f))?;
```

and

```rust
            let def = cx.sema.to_def2(item.node.with_value(s))?;
```

becomes

```rust
            let def = cx.vsema.to_def2(item.node.with_value(s))?;
```

4. `item`, `check`. Why: `Item<'db>`; ranges and the id from views.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, foreign: &[Item], canonical: &Canonical) -> Vec<Found> {
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, foreign: &[Item<'db>], canonical: &Canonical) -> Vec<Found> {
```

and

```rust
fn check(cx: &Cx<'_, '_>, item: &Item, at: &Item, canonical: &Canonical, found: &mut Vec<Found>) {
```

becomes

```rust
fn check<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, at: &Item<'db>, canonical: &Canonical, found: &mut Vec<Found>) {
```

and

```rust
            let ast::Item::Fn(f) = &item.node.value else { return };
            (def.ret_type(db), Ok(sig_of(id)), item.node.with_value(signature_range(f)))
```

becomes

```rust
            let ast::Item::Fn(f) = item.node.value else { return };
            (def.ret_type(db), Ok(sig_of(id)), item.node.with_value(signature_range_view(f)))
```

and

```rust
            (def.ty(db), user, item.node.as_ref().map(|it| item_range(it.syntax())))
```

becomes

```rust
            (def.ty(db), user, item.node.map(|it| item_range_view(it.syntax())))
```

and

```rust
    let at = at.node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
```

becomes

```rust
    let at = at.node.map(|it| ast::ptr(it.syntax()));
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/docs.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs`, `items.rs` and
`visibility.rs` headings above (`Item<'db>`, `walk::{attr, any_named}` taking owners by value,
`visibility::def`); needs `hir::view::Semantics::to_def2` and `hir::Module::definition_source_view`
(frontend-tree-move-hir). Rewritten 2026-10-29 22:05 against `item.nodes()` and `walk::any_named`.

1. Imports. Why: views.

```rust
use blaze_items::Fact;
use hir::{InFile, ModuleSource};
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize, T,
    ast::{self, HasAttrs},
};
```

becomes

```rust
use blaze_items::Fact;
use hir::{InFile, view::ModuleSourceView};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize, T,
    view::{self as ast, AstNode, HasAttrs},
};
```

2. `krate`. Why: the crate root's view from the view `Semantics`.

```rust
    let source = root.definition_source(cx.db);
    let ModuleSource::SourceFile(_) = source.value else { return Vec::new() }; // alloc: empty, allocates nothing
    let file = source.file_id;
    // alloc: empty, allocates nothing
    let Some(source_file) = ast::SourceFile::cast(cx.sema.parse_or_expand(file)) else { return Vec::new() };
    if has_doc(cx, &source_file, Some(source_file.syntax()), None) {
```

becomes

```rust
    let source = root.definition_source_view(cx.db);
    let ModuleSourceView::SourceFile(_) = source.value else { return Vec::new() }; // alloc: empty, allocates nothing
    let file = source.file_id;
    // alloc: empty, allocates nothing
    let Some(source_file) = ast::SourceFile::cast(cx.root(file)) else { return Vec::new() };
    if has_doc(cx, source_file, Some(source_file.syntax()), None) {
```

and

```rust
    vec![Found::lint(diag, Phase::Late, InFile::new(file, SyntaxNodePtr::new(source_file.syntax())))] // alloc: the lint that fires
```

becomes

```rust
    vec![Found::lint(diag, Phase::Late, InFile::new(file, ast::ptr(source_file.syntax())))] // alloc: the lint that fires
```

3. `item`. Why: the item, its container and own nodes are `Copy` views of `'db` (the view
   `Semantics<'db>` takes `'db` views only); defs from the view `Semantics`; nothing cloned to
   cast.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, effective: &Effective) -> Vec<Found> {
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    let node = &item.node.value;
    if matches!(node, ast::Item::Impl(_) | ast::Item::Use(_) | ast::Item::ExternCrate(_)) {
        return found;
    }
    if let Container::Impl(imp) = &item.container
        && imp.value.trait_().is_some()
    {
        return found;
    }
    // Asked last: only an exported item with no docs of its own needs it.
    let hidden_impl = || matches!(&item.container, Container::Impl(imp) if hidden_self_ty(cx, imp));
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective) -> Vec<Found> {
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    let node = item.node.value;
    if matches!(node, ast::Item::Impl(_) | ast::Item::Use(_) | ast::Item::ExternCrate(_)) {
        return found;
    }
    if let Container::Impl(imp) = item.container
        && imp.value.trait_().is_some()
    {
        return found;
    }
    // Asked last: only an exported item with no docs of its own needs it.
    let hidden_impl = || matches!(item.container, Container::Impl(imp) if hidden_self_ty(cx, imp));
```

and

```rust
    if exported && !has_doc(cx, node, list.as_ref().map(AstNode::syntax), list.as_ref()) && !hidden_impl() {
        found.push(lint(cx, item.node.as_ref().map(AstNode::syntax), desc));
    }
    if !matches!(node, ast::Item::Struct(_) | ast::Item::Union(_) | ast::Item::Enum(_)) {
        return found;
    }
    for own in &item.nodes()[1..] {
        match own.value.kind() {
            // A variant `pub use` names is exported without its enum.
            SyntaxKind::VARIANT => {
                // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
                let Some(variant) = ast::Variant::cast(own.value.clone()) else { continue };
                let def = cx.sema.to_def2(own.with_value(&variant));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Item(hir::ModuleDef::EnumVariant(it))));
                if exported && !has_doc(cx, &variant, None, None) {
                    found.push(lint(cx, own.as_ref(), ("a", "variant")));
                }
            }
            // A field is exported only with the type it is of.
            SyntaxKind::RECORD_FIELD if exported => {
                // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
                let Some(field) = ast::RecordField::cast(own.value.clone()) else { continue };
                let def = cx.sema.to_def2(own.with_value(&field));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Field(it)));
                if exported && !has_doc(cx, &field, None, None) {
                    found.push(lint(cx, own.as_ref(), ("a", "struct field")));
                }
```

becomes

```rust
    if exported && !has_doc(cx, node, list.map(|it| it.syntax()), list) && !hidden_impl() {
        found.push(lint(cx, item.syntax(), desc));
    }
    if !matches!(node, ast::Item::Struct(_) | ast::Item::Union(_) | ast::Item::Enum(_)) {
        return found;
    }
    for &own in &item.nodes()[1..] {
        match own.value.kind() {
            // A variant `pub use` names is exported without its enum.
            SyntaxKind::VARIANT => {
                let Some(variant) = ast::Variant::cast(own.value) else { continue };
                let def = cx.vsema.to_def2(own.with_value(variant));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Item(hir::ModuleDef::EnumVariant(it))));
                if exported && !has_doc(cx, variant, None, None) {
                    found.push(lint(cx, own, ("a", "variant")));
                }
            }
            // A field is exported only with the type it is of.
            SyntaxKind::RECORD_FIELD if exported => {
                let Some(field) = ast::RecordField::cast(own.value) else { continue };
                let def = cx.vsema.to_def2(own.with_value(field));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Field(it)));
                if exported && !has_doc(cx, field, None, None) {
                    found.push(lint(cx, own, ("a", "struct field")));
                }
```

4. `lint`, `has_doc`, `is_doc_comment`, `tokens_range`. Why: views by value; `walk::any_named`
   hands the meta by value; the doc comment's tokens read off the view.

```rust
fn lint(cx: &Cx<'_, '_>, node: InFile<&SyntaxNode>, (article, desc): (&str, &str)) -> Found {
    let span = cx.span(node.with_value(def_span(node.value)));
    // alloc: the firing lint's message
    let mut diag = Diag::lint("missing_docs", format!("missing documentation for {article} {desc}"), span);
    let at = node.map(SyntaxNodePtr::new);
```

becomes

```rust
fn lint(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>, (article, desc): (&str, &str)) -> Found {
    let span = cx.span(node.with_value(def_span(node.value)));
    // alloc: the firing lint's message
    let mut diag = Diag::lint("missing_docs", format!("missing documentation for {article} {desc}"), span);
    let at = node.map(ast::ptr);
```

and

```rust
fn has_doc<N: HasAttrs>(cx: &Cx<'_, '_>, owner: &N, inner: Option<&SyntaxNode>, list: Option<&ast::ItemList>) -> bool {
    let outer_comment = owner.syntax().children().any(|it| is_doc_comment(&it, T![outer_doc_comment]));
    let inner_comment = inner.is_some_and(|it| it.children().any(|it| is_doc_comment(&it, T![inner_doc_comment])));
    if outer_comment || inner_comment {
        return true;
    }
    let documented = |meta: &ast::Meta| match meta {
```

becomes

```rust
fn has_doc<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>, inner: Option<ast::Node<'t>>, list: Option<ast::ItemList<'t>>) -> bool {
    let outer_comment = owner.syntax().children().any(|it| is_doc_comment(it, T![outer_doc_comment]));
    let inner_comment = inner.is_some_and(|it| it.children().any(|it| is_doc_comment(it, T![inner_doc_comment])));
    if outer_comment || inner_comment {
        return true;
    }
    let documented = |meta: ast::Meta<'t>| match meta {
```

and

```rust
fn is_doc_comment(node: &SyntaxNode, kind: SyntaxKind) -> bool {
```

becomes

```rust
fn is_doc_comment(node: ast::Node<'_>, kind: SyntaxKind) -> bool {
```

and

```rust
fn tokens_range(node: &SyntaxNode) -> Option<TextRange> {
```

becomes

```rust
fn tokens_range(node: ast::Node<'_>) -> Option<TextRange> {
```

5. `hidden_self_ty`, `describe`. Why: the impl's view of `'db` by value; its def from the view
   `Semantics`.

```rust
fn hidden_self_ty(cx: &Cx<'_, '_>, imp: &InFile<ast::Impl>) -> bool {
    let Some(def) = cx.sema.to_def2(imp.as_ref()) else { return false };
```

becomes

```rust
fn hidden_self_ty<'db>(cx: &Cx<'_, 'db>, imp: InFile<ast::Impl<'db>>) -> bool {
    let Some(def) = cx.vsema.to_def2(imp) else { return false };
```

and

```rust
fn describe(item: &Item) -> Option<(&'static str, &'static str)> {
```

becomes

```rust
fn describe(item: &Item<'_>) -> Option<(&'static str, &'static str)> {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)
> rewritten 2026-10-29 22:05 by frontend-tree-move-diag-lints-items (second agent) against
> `item.nodes()` and `walk::any_named`, with `'db` tied.

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/bounds.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs);
needs `hir::view::Semantics::resolve_path` (frontend-tree-move-hir).

1. Imports. Why: views.

```rust
use syntax::{
    AstNode, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasGenericArgs, HasGenericParams, HasName, HasTypeBounds},
};
```

becomes

```rust
use syntax::{
    TextRange,
    view::{self as ast, AstNode, HasGenericArgs, HasGenericParams, HasName, HasTypeBounds},
};
```

2. `type_alias`. Why: the item's view by value; parameters and predicates passed by value.

```rust
pub(super) fn type_alias(cx: &Cx<'_, '_>, item: &Item, krate: &Krate) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let ast::Item::TypeAlias(alias) = &item.node.value else { return Vec::new() };
```

becomes

```rust
pub(super) fn type_alias<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, krate: &Krate) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let ast::Item::TypeAlias(alias) = item.node.value else { return Vec::new() };
```

and

```rust
    let inline = || params().filter_map(|it| inline_bounds(&it));
```

becomes

```rust
    let inline = || params().filter_map(inline_bounds);
```

and

```rust
        let affects = preds().any(|it| affects_object_lifetime_defaults(&it));
```

becomes

```rust
        let affects = preds().any(affects_object_lifetime_defaults);
```

3. `bounds_lint`, `inline_bounds`, `affects_object_lifetime_defaults`, `outlives`,
   `shorthand_qselves`. Why: views by value; the qualifier resolved by the view `Semantics`.

```rust
    item: &Item,
    in_where_clause: bool,
```

becomes

```rust
    item: &Item<'_>,
    in_where_clause: bool,
```

and

```rust
fn inline_bounds(param: &ast::GenericParam) -> Option<(TextRange, ast::TypeBoundList)> {
```

becomes

```rust
fn inline_bounds(param: ast::GenericParam<'_>) -> Option<(TextRange, ast::TypeBoundList<'_>)> {
```

and

```rust
fn affects_object_lifetime_defaults(pred: &ast::WherePred) -> bool {
```

becomes

```rust
fn affects_object_lifetime_defaults(pred: ast::WherePred<'_>) -> bool {
```

and

```rust
fn outlives(list: Option<ast::TypeBoundList>) -> bool {
```

becomes

```rust
fn outlives(list: Option<ast::TypeBoundList<'_>>) -> bool {
```

and

```rust
fn shorthand_qselves(cx: &Cx<'_, '_>, ty: &SyntaxNode) -> impl Iterator<Item = TextRange> {
```

becomes

```rust
fn shorthand_qselves<'db>(cx: &Cx<'_, 'db>, ty: ast::Node<'db>) -> impl Iterator<Item = TextRange> {
```

and

```rust
        match cx.sema.resolve_path(&qualifier) {
```

becomes

```rust
        match cx.vsema.resolve_path(qualifier) {
```

4. `drop`. Why: paths and bounds by value; `bound_path` a function (its result borrows its
   argument's tree, which a closure's elided lifetimes cannot say).

```rust
pub(super) fn drop(cx: &Cx<'_, '_>, item: &Item, krate: &Krate) -> Vec<Found> {
    let Some(drop) = krate.drop else { return Vec::new() }; // alloc: empty, allocates nothing
    let is_drop = |path: Option<ast::Path>| {
        path.and_then(|it| cx.sema.resolve_path(&it)).is_some_and(|it| {
            matches!(it, PathResolution::Def(ModuleDef::Trait(t)) if hir_def::TraitId::from(t) == drop)
        })
    };
    let bound_path = |bound: &ast::TypeBound| match bound.ty() {
        Some(ast::Type::PathType(it)) => it.path(),
        _ => None,
    };
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    if matches!(item.container, Container::Module) {
        clauses(&item.node, |bounded, bound| {
            if !is_drop(bound_path(&bound.value)) {
```

becomes

```rust
pub(super) fn drop<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, krate: &Krate) -> Vec<Found> {
    let Some(drop) = krate.drop else { return Vec::new() }; // alloc: empty, allocates nothing
    let is_drop = |path: Option<ast::Path<'_>>| {
        path.and_then(|it| cx.vsema.resolve_path(it)).is_some_and(|it| {
            matches!(it, PathResolution::Def(ModuleDef::Trait(t)) if hir_def::TraitId::from(t) == drop)
        })
    };
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    if matches!(item.container, Container::Module) {
        clauses(item.node, |bounded, bound| {
            if !is_drop(bound_path(bound.value)) {
```

and

```rust
    for ty in item.own::<ast::DynTraitType>() {
        for bound in ty.value.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
            if !is_drop(bound_path(&bound)) {
```

becomes

```rust
    for ty in item.own::<ast::DynTraitType<'_>>() {
        for bound in ty.value.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
            if !is_drop(bound_path(bound)) {
```

and

```rust
            let at = ty.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
```

becomes

```rust
            let at = ty.map(|it| ast::ptr(it.syntax()));
```

5. `clauses`. Why: the item and the bounded node by value; `bound_path` beside it.

```rust
/// An item's explicit trait bounds, each given to `each` with the node whose text is the type it
/// bounds as rustc prints the clause (`None`: `Self`): its parameters' bounds, its `where`
/// clause's, a trait's supertraits, and its `impl Trait` parameters; in that order.
fn clauses(item: &InFile<ast::Item>, mut each: impl FnMut(Option<&SyntaxNode>, InFile<ast::TypeBound>)) {
```

becomes

```rust
/// The path of a bound's trait, when it is a path.
fn bound_path(bound: ast::TypeBound<'_>) -> Option<ast::Path<'_>> {
    match bound.ty() {
        Some(ast::Type::PathType(it)) => it.path(),
        _ => None,
    }
}

/// An item's explicit trait bounds, each given to `each` with the node whose text is the type it
/// bounds as rustc prints the clause (`None`: `Self`): its parameters' bounds, its `where`
/// clause's, a trait's supertraits, and its `impl Trait` parameters; in that order.
fn clauses<'t>(item: InFile<ast::Item<'t>>, mut each: impl FnMut(Option<ast::Node<'t>>, InFile<ast::TypeBound<'t>>)) {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/naming.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` heading above (same needs);
needs `hir::view::Semantics::{resolve_path, resolve_bind_pat_to_const, to_def2}` (frontend-tree-move-hir).

1. Imports. Why: views; `AstToken` for an ABI string's `text`.

```rust
use syntax::{
    AstNode, SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasGenericArgs, HasName},
};
```

becomes

```rust
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode, AstToken as _, HasGenericArgs, HasName},
};
```

2. `krate`. Why: the crate root's view from the view `Semantics`.

```rust
    let source_file = ast::SourceFile::cast(cx.sema.parse_or_expand(file))?;
    let at = InFile::new(file, SyntaxNodePtr::new(source_file.syntax()));
```

becomes

```rust
    let source_file = ast::SourceFile::cast(cx.root(file))?;
    let at = InFile::new(file, ast::ptr(source_file.syntax()));
```

and

```rust
    if let Some((ast::Meta::KeyValueMeta(kv), _)) = walk::attr(cx, &source_file, "crate_name")
```

becomes

```rust
    if let Some((ast::Meta::KeyValueMeta(kv), _)) = walk::attr(cx, source_file, "crate_name")
```

3. `item`. Why: the item and its own nodes are `Copy` views of `'db` (the view `Semantics<'db>`
   takes `'db` views only), cast after the kind test with nothing cloned; paths resolved by the
   view `Semantics`.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
```

and

```rust
    if let ast::Item::Fn(it) = &item.node.value
```

becomes

```rust
    if let ast::Item::Fn(it) = item.node.value
```

and

```rust
    for node in item.nodes() {
        let at = || node.as_ref().map(SyntaxNodePtr::new);
        match node.value.kind() {
            SyntaxKind::LIFETIME_PARAM => {
                // clone: a rowan cursor's refcount, after the kind test; `cast` takes it owned
                let Some(lifetime) = ast::LifetimeParam::cast(node.value.clone()).and_then(|it| it.lifetime()) else {
```

becomes

```rust
    for &node in item.nodes() {
        let at = || node.map(ast::ptr);
        match node.value.kind() {
            SyntaxKind::LIFETIME_PARAM => {
                let Some(lifetime) = ast::LifetimeParam::cast(node.value).and_then(|it| it.lifetime()) else {
```

and

```rust
                let Some(param) = ast::ConstParam::cast(node.value.clone()) else { continue }; // clone: rowan refcount, after the kind test
                let Some(name) = param.name() else { continue };
                let range = node.with_value(name.syntax().text_range());
                let usages = || const_param_usages(cx, item, node.with_value(&param), name.text());
```

becomes

```rust
                let Some(param) = ast::ConstParam::cast(node.value) else { continue };
                let Some(name) = param.name() else { continue };
                let range = node.with_value(name.syntax().text_range());
                let usages = || const_param_usages(cx, item, node.with_value(param), name.text());
```

and

```rust
                let Some(ty) = ast::FnPtrType::cast(node.value.clone()) else { continue }; // clone: rowan refcount, after the kind test
```

becomes

```rust
                let Some(ty) = ast::FnPtrType::cast(node.value) else { continue };
```

and

```rust
                        let at = node.with_value(SyntaxNodePtr::new(pat.syntax()));
```

becomes

```rust
                        let at = node.with_value(ast::ptr(pat.syntax()));
```

and

```rust
                let Some(pat) = ast::IdentPat::cast(node.value.clone()) else { continue }; // clone: rowan refcount, after the kind test
                found.extend(ident_pat(cx, node.with_value(&pat), bindings, read(Fact::Code, "code")));
            }
            SyntaxKind::PATH_PAT => {
                // clone: a rowan cursor's refcount, after the kind test; `cast` takes it owned
                if let Some(pat) = ast::PathPat::cast(node.value.clone())
```

becomes

```rust
                let Some(pat) = ast::IdentPat::cast(node.value) else { continue };
                found.extend(ident_pat(cx, node.with_value(pat), bindings, read(Fact::Code, "code")));
            }
            SyntaxKind::PATH_PAT => {
                if let Some(pat) = ast::PathPat::cast(node.value)
```

and

```rust
                    && let Some(PathResolution::Def(ModuleDef::Const(_))) = cx.sema.resolve_path(&path)
```

becomes

```rust
                    && let Some(PathResolution::Def(ModuleDef::Const(_))) = cx.vsema.resolve_path(path)
```

4. `ident_pat`. Why: the pattern's view by value; resolved by the view `Semantics`.

```rust
fn ident_pat(cx: &Cx<'_, '_>, pat: InFile<&ast::IdentPat>, bindings: bool, read: impl FnOnce() -> Read) -> Option<Found> {
```

becomes

```rust
fn ident_pat<'db>(cx: &Cx<'_, 'db>, pat: InFile<ast::IdentPat<'db>>, bindings: bool, read: impl FnOnce() -> Read) -> Option<Found> {
```

and

```rust
    let at = pat.map(|it| SyntaxNodePtr::new(it.syntax()));
    let plain = pat.value.pat().is_none() && pat.value.ref_token().is_none() && pat.value.mut_token().is_none();
    if plain && let Some(def) = cx.sema.resolve_bind_pat_to_const(pat.value) {
```

becomes

```rust
    let at = pat.map(|it| ast::ptr(it.syntax()));
    let plain = pat.value.pat().is_none() && pat.value.ref_token().is_none() && pat.value.mut_token().is_none();
    if plain && let Some(def) = cx.vsema.resolve_bind_pat_to_const(pat.value) {
```

5. `const_param_usages`, `is_rust_abi`. Why: views by value; the view `Semantics`.

```rust
fn const_param_usages(cx: &Cx<'_, '_>, item: &Item, param: InFile<&ast::ConstParam>, name: &str) -> Vec<Span> {
    // alloc: the renames of a lint that fires, read only then
    let Some(def) = cx.sema.to_def2(param) else { return Vec::new() };
    let mut usages = Vec::new(); // alloc: as above
    for path in item.own::<ast::Path>() {
```

becomes

```rust
fn const_param_usages<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, param: InFile<ast::ConstParam<'db>>, name: &str) -> Vec<Span> {
    // alloc: the renames of a lint that fires, read only then
    let Some(def) = cx.vsema.to_def2(param) else { return Vec::new() };
    let mut usages = Vec::new(); // alloc: as above
    for path in item.own::<ast::Path<'db>>() {
```

and

```rust
        if let Some(PathResolution::ConstParam(it)) = cx.sema.resolve_path(&path.value)
```

becomes

```rust
        if let Some(PathResolution::ConstParam(it)) = cx.vsema.resolve_path(path.value)
```

and

```rust
pub(super) fn is_rust_abi(abi: Option<ast::Abi>) -> bool {
```

becomes

```rust
pub(super) fn is_rust_abi(abi: Option<ast::Abi<'_>>) -> bool {
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/privacy.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs`, `items.rs` and
`visibility.rs` headings above (`Item<'db>`, `item_id`, `visibility::{def, interface_node}`);
needs `hir::view::Semantics::{resolve_path, resolve_type, source, to_def2}` and
`hir::Module::declaration_source_view` (frontend-tree-move-hir). The view `Semantics<'db>` takes
views of `'db` only (it is invariant in `'db`), so the checker names `'db` once and every view and
item it holds is of it.

1. Imports. Why: views; sources come from the view `Semantics`, so `HasSource`, `AstPtr`,
   `SyntaxNode` and `SyntaxNodePtr` go.

```rust
use hir::{AsAssocItem as _, AssocItemContainer, HasSource, InFile, ModuleDef, PathResolution};
use rustc_hash::FxHashSet;
use syntax::{
    AstNode, AstPtr, SyntaxKind, SyntaxNode, SyntaxNodePtr,
    ast::{self, HasGenericArgs, HasTypeBounds},
};
```

becomes

```rust
use hir::{AsAssocItem as _, AssocItemContainer, InFile, ModuleDef, PathResolution};
use rustc_hash::FxHashSet;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode, HasGenericArgs, HasTypeBounds},
};
```

2. `lints`, `Check`, `Checker::check`. Why: the items and the checked node are views of `'db`.

```rust
pub(super) fn lints(cx: &Cx<'_, '_>, items: &[Item], effective: &Effective) -> Vec<Found> {
```

becomes

```rust
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>], effective: &Effective) -> Vec<Found> {
```

and

```rust
struct Check<'i> {
    item: Def,
    node: InFile<SyntaxNode>,
```

becomes

```rust
struct Check<'i, 'db> {
    item: Def,
    node: InFile<ast::Node<'db>>,
```

and

```rust
    owner: &'i Item,
}
```

becomes

```rust
    owner: &'i Item<'db>,
}
```

and

```rust
impl Checker<'_, '_, '_> {
    #[allow(clippy::too_many_arguments)]
    fn check<'i>(
        &self,
        owner: &'i Item,
        item: Def,
        node: InFile<SyntaxNode>,
        kind: &'static str,
        required: Vis,
        required_ev: Option<Vis>,
    ) -> Check<'i> {
```

becomes

```rust
impl<'db> Checker<'_, '_, 'db> {
    #[allow(clippy::too_many_arguments)]
    fn check<'i>(
        &self,
        owner: &'i Item<'db>,
        item: Def,
        node: InFile<ast::Node<'db>>,
        kind: &'static str,
        required: Vis,
        required_ev: Option<Vis>,
    ) -> Check<'i, 'db> {
```

3. `item`. Why: the item is a `Copy` view, matched and passed by value; containers compare as
   views do (tree and element), as the handles compared.

```rust
    fn item(&mut self, item: &Item, held: &[Item]) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let node = item.syntax();
        let vis = nominal(cx, def);
        let ev = reachable(self.effective, def);
        let file = item.node.file_id;
        match &item.node.value {
            ast::Item::Const(_) | ast::Item::Static(_) | ast::Item::Fn(_) | ast::Item::TypeAlias(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                self.ty(&mut check, &item.node.value, None);
            }
            ast::Item::Trait(t) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                let own = |it: &Item| matches!(&it.container, Container::Trait(c) if c.file_id == file && c.value == *t);
                for assoc in held.iter().filter(|it| own(it)) {
                    self.assoc_item(assoc, vis, ev, true, None);
                }
            }
            ast::Item::Enum(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                for field in walk::fields(item) {
                    self.field(item, field, |_| (vis, ev));
                }
            }
            ast::Item::Struct(_) | ast::Item::Union(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, &item.node.value);
                self.clauses(&mut check, &item.node.value);
                let effective = self.effective;
                for field in walk::fields(item) {
                    self.field(item, field, |field| (min(cx, vis, nominal(cx, field)), reachable(effective, field)));
                }
            }
            ast::Item::Impl(imp) => {
                let Def::Impl(id) = def else { return };
                let of_trait = imp.trait_().is_some();
                let defs = effective::impl_defs(cx, id, false);
                let impl_vis = defs.iter().fold(Vis::Public, |at, it| min(cx, nominal(cx, *it), at));
                let impl_ev = self.effective.of_impl(cx, id).at(Level::Reachable);
                let mut check = self.check(item, def, node, "implementation", impl_vis, Some(impl_ev));
                if !of_trait {
                    self.generics(&mut check, &item.node.value);
                    self.clauses(&mut check, &item.node.value);
                }
```

becomes

```rust
    fn item(&mut self, item: &Item<'db>, held: &[Item<'db>]) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let node = item.syntax();
        let vis = nominal(cx, def);
        let ev = reachable(self.effective, def);
        let file = item.node.file_id;
        match item.node.value {
            ast::Item::Const(_) | ast::Item::Static(_) | ast::Item::Fn(_) | ast::Item::TypeAlias(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                self.ty(&mut check, item.node.value, None);
            }
            ast::Item::Trait(t) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                let own = |it: &Item<'db>| matches!(it.container, Container::Trait(c) if c.file_id == file && c.value == t);
                for assoc in held.iter().filter(|it| own(it)) {
                    self.assoc_item(assoc, vis, ev, true, None);
                }
            }
            ast::Item::Enum(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                for field in walk::fields(item) {
                    self.field(item, field, |_| (vis, ev));
                }
            }
            ast::Item::Struct(_) | ast::Item::Union(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                let effective = self.effective;
                for field in walk::fields(item) {
                    self.field(item, field, |field| (min(cx, vis, nominal(cx, field)), reachable(effective, field)));
                }
            }
            ast::Item::Impl(imp) => {
                let Def::Impl(id) = def else { return };
                let of_trait = imp.trait_().is_some();
                let defs = effective::impl_defs(cx, id, false);
                let impl_vis = defs.iter().fold(Vis::Public, |at, it| min(cx, nominal(cx, *it), at));
                let impl_ev = self.effective.of_impl(cx, id).at(Level::Reachable);
                let mut check = self.check(item, def, node, "implementation", impl_vis, Some(impl_ev));
                if !of_trait {
                    self.generics(&mut check, item.node.value);
                    self.clauses(&mut check, item.node.value);
                }
```

and

```rust
                let own = |it: &Item| matches!(&it.container, Container::Impl(c) if c.file_id == file && c.value == *imp);
```

becomes

```rust
                let own = |it: &Item<'db>| matches!(it.container, Container::Impl(c) if c.file_id == file && c.value == imp);
```

4. `foreign_item`, `field`. Why: views by value; a field's def from the view `Semantics`, its
   node cast by value (nothing cloned).

```rust
    fn foreign_item(&mut self, item: &Item) {
        let Some(def) = visibility::def(self.cx, item) else { return };
        let vis = nominal(self.cx, def);
        let ev = reachable(self.effective, def);
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, false), vis, ev);
        self.generics(&mut check, &item.node.value);
        self.clauses(&mut check, &item.node.value);
        self.ty(&mut check, &item.node.value, None);
    }

    fn field(&mut self, owner: &Item, field: InFile<SyntaxNode>, required: impl FnOnce(Def) -> (Vis, Option<Vis>)) {
        let cx = self.cx;
        let def = match field.value.kind() {
            // clone: a rowan cursor's refcount, once per field; `cast` takes it owned
            SyntaxKind::RECORD_FIELD => ast::RecordField::cast(field.value.clone()).and_then(|it| cx.sema.to_def2(field.with_value(&it))),
            // clone: as above
            _ => ast::TupleField::cast(field.value.clone()).and_then(|it| cx.sema.to_def2(field.with_value(&it))),
        };
```

becomes

```rust
    fn foreign_item(&mut self, item: &Item<'db>) {
        let Some(def) = visibility::def(self.cx, item) else { return };
        let vis = nominal(self.cx, def);
        let ev = reachable(self.effective, def);
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, false), vis, ev);
        self.generics(&mut check, item.node.value);
        self.clauses(&mut check, item.node.value);
        self.ty(&mut check, item.node.value, None);
    }

    fn field(&mut self, owner: &Item<'db>, field: InFile<ast::Node<'db>>, required: impl FnOnce(Def) -> (Vis, Option<Vis>)) {
        let cx = self.cx;
        let def = match field.value.kind() {
            SyntaxKind::RECORD_FIELD => ast::RecordField::cast(field.value).and_then(|it| cx.vsema.to_def2(field.with_value(it))),
            _ => ast::TupleField::cast(field.value).and_then(|it| cx.vsema.to_def2(field.with_value(it))),
        };
```

5. `assoc_item`. Why: the item and the impl's self type by value.

```rust
    fn assoc_item(&mut self, item: &Item, vis: Vis, ev: Option<Vis>, in_trait: bool, impl_self: Option<ast::Type>) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let is_assoc_ty = matches!(item.node.value, ast::Item::TypeAlias(_));
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, true), vis, ev);
        check.hard_error = is_assoc_ty;
        self.generics(&mut check, &item.node.value);
        self.clauses(&mut check, &item.node.value);
        let has_type_of = match &item.node.value {
            ast::Item::TypeAlias(it) => !in_trait || it.ty().is_some(),
            _ => true,
        };
        if has_type_of {
            let impl_self = impl_self.as_ref().map(|it| item.node.with_value(it.syntax()));
            self.ty(&mut check, &item.node.value, impl_self);
        }
        if is_assoc_ty && in_trait {
            check.hard_error = false;
            check.primary = false;
            if let ast::Item::TypeAlias(it) = &item.node.value {
```

becomes

```rust
    fn assoc_item(&mut self, item: &Item<'db>, vis: Vis, ev: Option<Vis>, in_trait: bool, impl_self: Option<ast::Type<'db>>) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let is_assoc_ty = matches!(item.node.value, ast::Item::TypeAlias(_));
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, true), vis, ev);
        check.hard_error = is_assoc_ty;
        self.generics(&mut check, item.node.value);
        self.clauses(&mut check, item.node.value);
        let has_type_of = match item.node.value {
            ast::Item::TypeAlias(it) => !in_trait || it.ty().is_some(),
            _ => true,
        };
        if has_type_of {
            let impl_self = impl_self.map(|it| item.node.with_value(it.syntax()));
            self.ty(&mut check, item.node.value, impl_self);
        }
        if is_assoc_ty && in_trait {
            check.hard_error = false;
            check.primary = false;
            if let ast::Item::TypeAlias(it) = item.node.value {
```

6. `generics`, `clauses`, `ty`. Why: the item by value; the impl's self type a view.

```rust
    fn generics(&mut self, check: &mut Check<'_>, node: &ast::Item) {
```

becomes

```rust
    fn generics(&mut self, check: &mut Check<'_, 'db>, node: ast::Item<'db>) {
```

and

```rust
    fn clauses(&mut self, check: &mut Check<'_>, node: &ast::Item) {
```

becomes

```rust
    fn clauses(&mut self, check: &mut Check<'_, 'db>, node: ast::Item<'db>) {
```

and

```rust
    fn ty(&mut self, check: &mut Check<'_>, node: &ast::Item, impl_self: Option<InFile<&SyntaxNode>>) {
```

becomes

```rust
    fn ty(&mut self, check: &mut Check<'_, 'db>, node: ast::Item<'db>, impl_self: Option<InFile<ast::Node<'db>>>) {
```

7. `visit`. Why: the walk is the view's preorder (same events, same `skip_subtree`); paths and
   types resolved by the view `Semantics`; an alias's source is the view the view `Semantics`
   hands out (the node the handle version found again under its root); nothing cloned.

```rust
    fn visit(&mut self, check: &Check<'_>, node: InFile<&SyntaxNode>, skip_impl_trait: bool, aliases: &mut FxHashSet<hir::TypeAlias>) {
```

becomes

```rust
    fn visit(&mut self, check: &Check<'_, 'db>, node: InFile<ast::Node<'db>>, skip_impl_trait: bool, aliases: &mut FxHashSet<hir::TypeAlias>) {
```

and

```rust
            match cx.sema.resolve_path(&path) {
```

becomes

```rust
            match cx.vsema.resolve_path(path) {
```

and

```rust
                    if aliases.insert(alias)
                        && let Some(src) = alias.source(cx.db)
                    {
                        let alias_node = cx.node(src.as_ref().map(AstPtr::new));
                        if let Some(aliased) = alias_node.value.ty() {
                            self.visit(check, alias_node.with_value(aliased.syntax()), skip_impl_trait, aliases);
                        }
                    }
```

becomes

```rust
                    if aliases.insert(alias)
                        && let Some(alias_node) = cx.vsema.source(alias)
                        && let Some(aliased) = alias_node.value.ty()
                    {
                        self.visit(check, alias_node.with_value(aliased.syntax()), skip_impl_trait, aliases);
                    }
```

and

```rust
                    // clone: a rowan cursor's refcount, printed only for a lint that fires
                    let descr = || match cx.sema.resolve_type(&ast::Type::PathType(ty.clone())) {
```

becomes

```rust
                    let descr = || match cx.vsema.resolve_type(ast::Type::PathType(ty)) {
```

and

```rust
                    self.def_id(check, Def::Item(ModuleDef::Trait(tr)), "trait", || trait_descr(cx, tr, &path));
```

becomes

```rust
                    self.def_id(check, Def::Item(ModuleDef::Trait(tr)), "trait", || trait_descr(cx, tr, path));
```

8. `projection`. Why: the path a view of `'db`; the trait's path by value.

```rust
    fn projection(&mut self, check: &Check<'_>, path: InFile<ast::Path>, alias: hir::TypeAlias, aliases: &mut FxHashSet<hir::TypeAlias>) {
```

becomes

```rust
    fn projection(&mut self, check: &Check<'_, 'db>, path: InFile<ast::Path<'db>>, alias: hir::TypeAlias, aliases: &mut FxHashSet<hir::TypeAlias>) {
```

and

```rust
        let descr = || match &trait_path {
```

becomes

```rust
        let descr = || match trait_path {
```

9. `def_id`, `fact`. Why: the checked node and the def's node are views, by value; the lint's
   node an id made from the view.

```rust
    fn def_id(&mut self, check: &Check<'_>, def: Def, kind: &str, descr: impl FnOnce() -> String) {
```

becomes

```rust
    fn def_id(&mut self, check: &Check<'_, 'db>, def: Def, kind: &str, descr: impl FnOnce() -> String) {
```

and

```rust
        let item_span = cx.span(check.node.as_ref().map(def_span));
        let vis_span = cx.span(def_node.as_ref().map(def_span));
```

becomes

```rust
        let item_span = cx.span(check.node.map(def_span));
        let vis_span = cx.span(def_node.map(def_span));
```

and

```rust
        let at = check.node.as_ref().map(SyntaxNodePtr::new);
```

becomes

```rust
        let at = check.node.map(ast::ptr);
```

and

```rust
    fn fact(&self, check: &Check<'_>, diag: &mut Diag) {
```

becomes

```rust
    fn fact(&self, check: &Check<'_, 'db>, diag: &mut Diag) {
```

10. `trait_descr`, `descr_kind`. Why: the path by value, its arguments' types resolved by the view
    `Semantics`; an argument's text is `&str`, pushed without a `String` of its own.

```rust
fn trait_descr(cx: &Cx<'_, '_>, tr: hir::Trait, path: &ast::Path) -> String {
```

becomes

```rust
fn trait_descr<'db>(cx: &Cx<'_, 'db>, tr: hir::Trait, path: ast::Path<'db>) -> String {
```

and

```rust
            ast::GenericArg::TypeArg(it) => it.ty().and_then(|ty| cx.sema.resolve_type(&ty)),
```

becomes

```rust
            ast::GenericArg::TypeArg(it) => it.ty().and_then(|ty| cx.vsema.resolve_type(ty)),
```

and

```rust
            None => descr.push_str(&arg.syntax().text().to_string()), // alloc: a lint that fires
```

becomes

```rust
            None => descr.push_str(arg.syntax().text()),
```

and

```rust
fn descr_kind(item: &Item, assoc: bool) -> &'static str {
```

becomes

```rust
fn descr_kind(item: &Item<'_>, assoc: bool) -> &'static str {
```

11. `source`. Why: a variant's source from the view `Semantics`, a module's declaration as its
    view; nothing cloned.

```rust
fn source(cx: &Cx<'_, '_>, def: Def) -> Option<InFile<SyntaxNode>> {
    fn of<N: AstNode>(cx: &Cx<'_, '_>, src: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
        let src = src?;
        Some(cx.node(src.as_ref().map(AstPtr::new)).map(|it| it.syntax().clone())) // clone: rowan refcount, a lint that fires
    }
    let db = cx.db;
    match def {
        Def::Item(ModuleDef::EnumVariant(it)) => of(cx, it.source(db)),
        // clone: a rowan cursor's refcount, for a lint that fires
        Def::Item(ModuleDef::Module(it)) => it.declaration_source(db).map(|it| it.map(|it| it.syntax().clone())),
        _ => visibility::interface_node(cx, def),
    }
}
```

becomes

```rust
fn source<'db>(cx: &Cx<'_, 'db>, def: Def) -> Option<InFile<ast::Node<'db>>> {
    match def {
        Def::Item(ModuleDef::EnumVariant(it)) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        Def::Item(ModuleDef::Module(it)) => it.declaration_source_view(cx.db).map(|it| it.map(|it| it.syntax())),
        _ => visibility::interface_node(cx, def),
    }
}
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1); told by tmux (requests, pass 26). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/non_local.rs: onto views (1)

From `frontend-tree-move-diag-lints-items`. Lands with the `walk.rs` and `items.rs` headings above
(`Item<'db>`, `item_id`, `def_span` by value); needs `cx::item_range_view`
(frontend-tree-move-diag-lower), `hir::view::Semantics::{ancestors_with_macros_file, resolve_path,
source}` and `hir::view::HasSourceView` (frontend-tree-move-hir). An owner holds a view of `'db`
and is `Copy`; every node walked up from comes from the view `Semantics` (the walk's items, a
def's source), so `ancestors_with_macros_file` knows its file.

1. Imports. Why: views.

```rust
use hir::{HasSource, InFile, ModuleDef, PathResolution};
use smallvec::SmallVec;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, TextRange,
    ast::{self, HasGenericArgs, HasName},
};

use super::walk::{self, Item};
use crate::{
    cx::{Cx, item_range},
```

becomes

```rust
use hir::{InFile, ModuleDef, PathResolution, view::HasSourceView};
use smallvec::SmallVec;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode, HasGenericArgs, HasName},
};

use super::walk::{self, Item};
use crate::{
    cx::{Cx, item_range_view},
```

2. `Owner`. Why: a view of `'db`, so `Copy`.

```rust
#[derive(Clone, Debug)]
struct Owner {
    /// Where it is, and whether it is the closure an `async fn` or `async` closure's body is.
    node: InFile<SyntaxNode>,
```

becomes

```rust
#[derive(Clone, Copy, Debug)]
struct Owner<'db> {
    /// Where it is, and whether it is the closure an `async fn` or `async` closure's body is.
    node: InFile<ast::Node<'db>>,
```

and

```rust
impl Owner {
    fn key(&self) -> (Option<hir::HirFileId>, TextRange, bool) {
```

becomes

```rust
impl Owner<'_> {
    fn key(&self) -> (Option<hir::HirFileId>, TextRange, bool) {
```

3. `Owner::{descr, name, span}`, `closure_decl`. Why: the node by value, cast without a clone; a
   name's text is `&str` (the `String` stays: the lint keeps it).

```rust
    fn descr(&self) -> &'static str {
        let node = &self.node.value;
```

becomes

```rust
    fn descr(&self) -> &'static str {
        let node = self.node.value;
```

and

```rust
                // clone: a rowan cursor's refcount, for a lint that fires
                let method = ast::Fn::cast(node.clone()).and_then(|it| it.param_list()).is_some_and(|it| it.self_param().is_some());
```

becomes

```rust
                let method = ast::Fn::cast(node).and_then(|it| it.param_list()).is_some_and(|it| it.self_param().is_some());
```

and

```rust
                let block = ast::BlockExpr::cast(node.clone()); // clone: rowan refcount, for a lint that fires
```

becomes

```rust
                let block = ast::BlockExpr::cast(node);
```

and

```rust
            SyntaxKind::FN => ast::Fn::cast(self.node.value.clone()).and_then(|it| it.name()), // clone: rowan refcount, a lint that fires
            SyntaxKind::CONST => {
                let it = ast::Const::cast(self.node.value.clone()); // clone: rowan refcount, a lint that fires
                if it.as_ref().is_some_and(|it| it.underscore_token().is_some()) {
                    return "_".to_owned(); // alloc: the name in a lint that fires
                }
                it.and_then(|it| it.name())
            }
            SyntaxKind::STATIC => ast::Static::cast(self.node.value.clone()).and_then(|it| it.name()), // clone: rowan refcount, a lint that fires
            _ => None,
        };
        named.map_or_else(|| "<unnameable>".to_owned(), |it| it.text().to_string()) // alloc: the name in a lint that fires
    }

    /// rustc's `def_span`.
    fn span(&self) -> TextRange {
        let node = &self.node.value;
        if self.inner {
            // clone: a rowan cursor's refcount, for a lint that fires
            return match ast::Fn::cast(node.clone()).and_then(|it| it.body()) {
                Some(body) => body.syntax().text_range(),
                None => closure_decl(node),
            };
        }
        match node.kind() {
            SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC => super::def_span(node),
            SyntaxKind::CLOSURE_EXPR => closure_decl(node),
            SyntaxKind::BLOCK_EXPR => match ast::BlockExpr::cast(node.clone()) { // clone: rowan refcount, a lint that fires
                Some(it) if it.const_token().is_some() => it.stmt_list().map_or(item_range(node), |it| it.syntax().text_range()),
                _ => item_range(node),
            },
            _ => node.text_range(),
        }
    }
}

/// A closure's `fn_decl_span`: up to its return type, or its parameters.
fn closure_decl(node: &SyntaxNode) -> TextRange {
    let start = item_range(node).start();
    let closure = ast::ClosureExpr::cast(node.clone()); // clone: rowan refcount, a lint that fires
    let end = closure.as_ref().and_then(|it| {
```

becomes

```rust
            SyntaxKind::FN => ast::Fn::cast(self.node.value).and_then(|it| it.name()),
            SyntaxKind::CONST => {
                let it = ast::Const::cast(self.node.value);
                if it.is_some_and(|it| it.underscore_token().is_some()) {
                    return "_".to_owned(); // alloc: the name in a lint that fires
                }
                it.and_then(|it| it.name())
            }
            SyntaxKind::STATIC => ast::Static::cast(self.node.value).and_then(|it| it.name()),
            _ => None,
        };
        named.map_or_else(|| "<unnameable>".to_owned(), |it| it.text().to_owned()) // alloc: the name in a lint that fires
    }

    /// rustc's `def_span`.
    fn span(&self) -> TextRange {
        let node = self.node.value;
        if self.inner {
            return match ast::Fn::cast(node).and_then(|it| it.body()) {
                Some(body) => body.syntax().text_range(),
                None => closure_decl(node),
            };
        }
        match node.kind() {
            SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC => super::def_span(node),
            SyntaxKind::CLOSURE_EXPR => closure_decl(node),
            SyntaxKind::BLOCK_EXPR => match ast::BlockExpr::cast(node) {
                Some(it) if it.const_token().is_some() => it.stmt_list().map_or(item_range_view(node), |it| it.syntax().text_range()),
                _ => item_range_view(node),
            },
            _ => node.text_range(),
        }
    }
}

/// A closure's `fn_decl_span`: up to its return type, or its parameters.
fn closure_decl(node: ast::Node<'_>) -> TextRange {
    let start = item_range_view(node).start();
    let closure = ast::ClosureExpr::cast(node);
    let end = closure.and_then(|it| {
```

4. `Owners`, `owners`. Why: the walk up is the view `Semantics`' (same nodes, same files, through
   the same macro calls); each step's node and the child it came from are `Copy` views compared
   as views compare (tree and element), where the handles compared their cursors; nothing cloned.

```rust
type Owners = SmallVec<[Owner; 8]>;

/// The definitions around `node`, innermost first, up to its crate's root or its file.
fn owners(cx: &Cx<'_, '_>, node: InFile<SyntaxNode>) -> Owners {
    let mut found = Owners::new();
    let mut prev = node.value.clone(); // clone: a rowan cursor's refcount; `node` goes to the ancestors
    for ancestor in cx.sema.ancestors_with_macros_file(node).skip(1) {
        let it = &ancestor.value;
        // Out of an expansion `child` is its root, no child of `it`: no body of `it` is it.
        let child = std::mem::replace(&mut prev, it.clone()); // clone: a rowan cursor's refcount
        // clone: a rowan cursor's refcount, for each definition kept
        let owner = |kind, body| Owner { node: ancestor.clone(), inner: false, kind, body };
        match it.kind() {
            SyntaxKind::SOURCE_FILE => {
                found.push(owner(Kind::Module, false));
                break;
            }
            SyntaxKind::MODULE => found.push(owner(Kind::Module, false)),
            SyntaxKind::FN => {
                let f = ast::Fn::cast(it.clone()); // clone: rowan refcount, after the kind test
                let body = f.as_ref().and_then(|it| it.body()).is_some_and(|it| *it.syntax() == child);
                if body && f.is_some_and(|it| it.async_token().is_some() || it.gen_token().is_some()) {
                    // clone: a rowan cursor's refcount, for the `async fn`'s closure kept
                    found.push(Owner { node: ancestor.clone(), inner: true, kind: Kind::Body, body: true });
                }
                found.push(owner(if body { Kind::Body } else { Kind::Other }, body));
            }
            SyntaxKind::CONST => {
                let c = ast::Const::cast(it.clone()); // clone: rowan refcount, after the kind test
                let body = c.as_ref().and_then(|it| it.body()).is_some_and(|it| *it.syntax() == child);
```

becomes

```rust
type Owners<'db> = SmallVec<[Owner<'db>; 8]>;

/// The definitions around `node`, innermost first, up to its crate's root or its file.
fn owners<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Owners<'db> {
    let mut found = Owners::new();
    let mut prev = node.value;
    for ancestor in cx.vsema.ancestors_with_macros_file(node).skip(1) {
        let it = ancestor.value;
        // Out of an expansion `child` is its root, no child of `it`: no body of `it` is it.
        let child = std::mem::replace(&mut prev, it);
        let owner = |kind, body| Owner { node: ancestor, inner: false, kind, body };
        match it.kind() {
            SyntaxKind::SOURCE_FILE => {
                found.push(owner(Kind::Module, false));
                break;
            }
            SyntaxKind::MODULE => found.push(owner(Kind::Module, false)),
            SyntaxKind::FN => {
                let f = ast::Fn::cast(it);
                let body = f.and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
                if body && f.is_some_and(|it| it.async_token().is_some() || it.gen_token().is_some()) {
                    found.push(Owner { node: ancestor, inner: true, kind: Kind::Body, body: true });
                }
                found.push(owner(if body { Kind::Body } else { Kind::Other }, body));
            }
            SyntaxKind::CONST => {
                let c = ast::Const::cast(it);
                let body = c.and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
```

and

```rust
            SyntaxKind::STATIC => {
                // clone: a rowan cursor's refcount, after the kind test
                let body = ast::Static::cast(it.clone()).and_then(|it| it.body()).is_some_and(|it| *it.syntax() == child);
                found.push(owner(if body { Kind::Body } else { Kind::Other }, body));
            }
            SyntaxKind::CLOSURE_EXPR => {
                let closure = ast::ClosureExpr::cast(it.clone()); // clone: rowan refcount, after the kind test
                let body = closure.as_ref().and_then(|it| it.body()).is_some_and(|it| *it.syntax() == child);
                if body && closure.is_some_and(|it| it.async_token().is_some() || it.gen_token().is_some()) {
                    // clone: a rowan cursor's refcount, for the `async` closure's body kept
                    found.push(Owner { node: ancestor.clone(), inner: true, kind: Kind::Body, body: true });
                }
                found.push(owner(Kind::Body, body));
            }
            SyntaxKind::BLOCK_EXPR => {
                let block = ast::BlockExpr::cast(it.clone()); // clone: rowan refcount, after the kind test
```

becomes

```rust
            SyntaxKind::STATIC => {
                let body = ast::Static::cast(it).and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
                found.push(owner(if body { Kind::Body } else { Kind::Other }, body));
            }
            SyntaxKind::CLOSURE_EXPR => {
                let closure = ast::ClosureExpr::cast(it);
                let body = closure.and_then(|it| it.body()).is_some_and(|it| it.syntax() == child);
                if body && closure.is_some_and(|it| it.async_token().is_some() || it.gen_token().is_some()) {
                    found.push(Owner { node: ancestor, inner: true, kind: Kind::Body, body: true });
                }
                found.push(owner(Kind::Body, body));
            }
            SyntaxKind::BLOCK_EXPR => {
                let block = ast::BlockExpr::cast(it);
```

and

```rust
            // A repeat count.
            // clone: a rowan cursor's refcount, after the kind test
            SyntaxKind::ARRAY_EXPR if ast::ArrayExpr::cast(it.clone()).is_some_and(|it| it.semicolon_token().is_some()) => {
                let count = it.children().filter(|it| ast::Expr::can_cast(it.kind())).nth(1);
                if count.as_ref() == Some(&child) {
```

becomes

```rust
            // A repeat count.
            SyntaxKind::ARRAY_EXPR if ast::ArrayExpr::cast(it).is_some_and(|it| it.semicolon_token().is_some()) => {
                let count = it.children().filter(|it| ast::Expr::can_cast(it.kind())).nth(1);
                if count == Some(child) {
```

5. `peel`, `depth`. Why: owners of `'db`.

```rust
fn peel(owners: &[Owner]) -> Option<&Owner> {
```

becomes

```rust
fn peel<'db>(owners: &[Owner<'db>]) -> Option<&Owner<'db>> {
```

and

```rust
fn depth(owners: &[Owner]) -> usize {
```

becomes

```rust
fn depth(owners: &[Owner<'_>]) -> usize {
```

6. `item`, `impl_`. Why: the item and its impl `Copy` views of `'db`; paths resolved by the view
   `Semantics`; the self type's and trait's roots by value.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item) -> Option<Found> {
    item.in_body?;
    match &item.node.value {
        ast::Item::Impl(imp) => impl_(cx, item, imp),
        ast::Item::MacroRules(rules) => macro_rules(cx, item, rules),
        _ => None,
    }
}

fn impl_(cx: &Cx<'_, '_>, item: &Item, imp: &ast::Impl) -> Option<Found> {
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<Found> {
    item.in_body?;
    match item.node.value {
        ast::Item::Impl(imp) => impl_(cx, item, imp),
        ast::Item::MacroRules(rules) => macro_rules(cx, item, rules),
        _ => None,
    }
}

fn impl_<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, imp: ast::Impl<'db>) -> Option<Found> {
```

and

```rust
    let mut paths: SmallVec<[(ast::Path, Option<PathResolution<'_>>); 4]> = SmallVec::new();
    // clone: rowan cursors' refcounts, the self type's and the trait's
    let roots = std::iter::once(self_ty.syntax().clone()).chain(imp.trait_().map(|it| it.syntax().clone()));
```

becomes

```rust
    let mut paths: SmallVec<[(ast::Path<'db>, Option<PathResolution<'_>>); 4]> = SmallVec::new();
    let roots = std::iter::once(self_ty.syntax()).chain(imp.trait_().map(|it| it.syntax()));
```

and

```rust
            let res = cx.sema.resolve_path(&path);
```

becomes

```rust
            let res = cx.vsema.resolve_path(path);
```

and

```rust
    let start = item_range(imp.syntax()).start();
```

becomes

```rust
    let start = item_range_view(imp.syntax()).start();
```

and

```rust
        diag.span_label(span(without_args(path)), format!("`{}` is not local", last_name(path))); // alloc: a lint that fires
```

becomes

```rust
        diag.span_label(span(without_args(*path)), format!("`{}` is not local", last_name(*path))); // alloc: a lint that fires
```

and

```rust
    let parent_node = &parent.node.value;
```

becomes

```rust
    let parent_node = parent.node.value;
```

and

```rust
        // clone: a rowan cursor's refcount, for a lint that fires
        let named_unit = ast::Const::cast(parent_node.clone()).filter(|it| it.underscore_token().is_none()).and_then(|it| {
```

becomes

```rust
        let named_unit = ast::Const::cast(parent_node).filter(|it| it.underscore_token().is_none()).and_then(|it| {
```

7. `macro_rules`. Why: the item and the rules by value; `walk::attr` takes its owner by value.

```rust
fn macro_rules(cx: &Cx<'_, '_>, item: &Item, rules: &ast::MacroRules) -> Option<Found> {
```

becomes

```rust
fn macro_rules<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, rules: ast::MacroRules<'db>) -> Option<Found> {
```

and

```rust
    let span = cx.span(item.node.with_value(item_range(rules.syntax())));
```

becomes

```rust
    let span = cx.span(item.node.with_value(item_range_view(rules.syntax())));
```

8. `source`, `without_args`, `last_name`. Why: a def's source the view the view `Semantics` hands
   out (and so knows the file of), nothing cloned; paths by value; a name's text is `&str` (the
   `String` stays: the label keeps it).

```rust
fn source(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<InFile<SyntaxNode>> {
    fn node<D: HasSource>(cx: &Cx<'_, '_>, def: D) -> Option<InFile<SyntaxNode>> {
        cx.sema.source(def).map(|it| it.map(|it| it.syntax().clone())) // clone: a rowan cursor's refcount
    }
```

becomes

```rust
fn source<'db>(cx: &Cx<'_, 'db>, def: ModuleDef) -> Option<InFile<ast::Node<'db>>> {
    fn node<'db, D: HasSourceView>(cx: &Cx<'_, 'db>, def: D) -> Option<InFile<ast::Node<'db>>> {
        cx.vsema.source(def).map(|it| it.map(|it| it.syntax()))
    }
```

and

```rust
fn without_args(path: &ast::Path) -> TextRange {
```

becomes

```rust
fn without_args(path: ast::Path<'_>) -> TextRange {
```

and

```rust
fn last_name(path: &ast::Path) -> String {
```

becomes

```rust
fn last_name(path: ast::Path<'_>) -> String {
```

and

```rust
        Some(name) => name.text().to_string(), // alloc: the label of a lint that fires
```

becomes

```rust
        Some(name) => name.text().to_owned(), // alloc: the label of a lint that fires
```

Status: open

> forwarded to diagnostics-lints-names (diagnostics-lints-names-next1); told by tmux (requests, pass 26). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

## frontend-tree: blaze-diagnostics lints/names/items/externs.rs: onto views (1)

From `frontend-tree-move-diag-lints-items` (`docs/v2/plan/lanes/frontend/tree-move-diag-lints-items.md`).
Lands with the `walk.rs` and `items.rs` headings above (`Item<'db>`, `Container<'db>`, `def_span` of a
view) and the `c_void.rs` heading (it calls `abi_of` with a view). Needs `Cx::vsema` and
`cx::item_range_view` (`## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` in
`v2/requests/diagnostics.md`, frontend-tree-move-diag-lower), `hir::view::Semantics::to_def2`
(`InFile<T>`, tree-move-hir.md) and hir-def's `HasSource::source_view` (tree-move-def.md). Same
items, same nodes, same ranges and the same diagnostics in the same order; nothing cloned or
allocated to walk a type or cast a node.

1. Imports. Why: views.

```rust
use syntax::{AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, WalkEvent, ast};
```

becomes

```rust
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, WalkEvent,
    view::{self as ast, AstNode},
};
```

and

```rust
    cx::{Cx, item_range},
```

becomes

```rust
    cx::{Cx, item_range_view},
```

2. `adt_source`. Why: the ADT's node as a view of the database's tree (`source_view`, the same node
   as `source`); read only for the span of a firing lint's note.

```rust
fn adt_source(cx: &Cx<'_, '_>, adt: AdtId) -> InFile<ast::Adt> {
    let db = cx.db;
    match adt {
        AdtId::StructId(it) => it.lookup(db).source(db).map(ast::Adt::Struct),
        AdtId::UnionId(it) => it.lookup(db).source(db).map(ast::Adt::Union),
        AdtId::EnumId(it) => it.lookup(db).source(db).map(ast::Adt::Enum),
    }
```

becomes

```rust
fn adt_source<'db>(cx: &Cx<'_, 'db>, adt: AdtId) -> InFile<ast::Adt<'db>> {
    let db = cx.db;
    match adt {
        AdtId::StructId(it) => it.lookup(db).source_view(db).map(ast::Adt::Struct),
        AdtId::UnionId(it) => it.lookup(db).source_view(db).map(ast::Adt::Union),
        AdtId::EnumId(it) => it.lookup(db).source_view(db).map(ast::Adt::Enum),
    }
```

and

```rust
            diag.span_note(cx.span(source.as_ref().map(|it| def_span(it.syntax()))), "the type is defined here");
```

becomes

```rust
            diag.span_note(cx.span(source.map(|it| def_span(it.syntax()))), "the type is defined here");
```

3. `abi_of` (also `c_void.rs`'s). Why: the ABI's view, by value.

```rust
pub(super) fn abi_of(abi: Option<ast::Abi>, default: ExternAbi) -> Option<ExternAbi> {
```

becomes

```rust
pub(super) fn abi_of(abi: Option<ast::Abi<'_>>, default: ExternAbi) -> Option<ExternAbi> {
```

4. `Site`. Why: `Item<'db>`.

```rust
    item: &'i Item,
```

becomes

```rust
    item: &'i Item<'db>,
```

5. `fnptrs`. Why: the written type is a `Copy` view and its preorder a scan of element numbers;
   nothing cloned to cast a node.

```rust
        hir_ty: InFile<&ast::Type>,
```

becomes

```rust
        hir_ty: InFile<ast::Type<'_>>,
```

and

```rust
                    if !counted(&node) {
                        continue;
                    }
                    depth += 1;
                    // clone: a cursor's refcount (no allocation), only of the types in a type that has a non-Rust `fn` pointer
                    if let Some(ptr) = ast::FnPtrType::cast(node.clone())
```

becomes

```rust
                    if !counted(node) {
                        continue;
                    }
                    depth += 1;
                    if let Some(ptr) = ast::FnPtrType::cast(node)
```

and

```rust
                    if counted(&node) {
```

becomes

```rust
                    if counted(node) {
```

6. `function`. Why: `Item<'db>` and the function's view by value (the view `Semantics<'db>` takes
   `'db` views only); its parameters and return type read as views.

```rust
    fn function(&mut self, item: &Item, f: &ast::Fn, mode: CItemKind) {
```

becomes

```rust
    fn function(&mut self, item: &Item<'db>, f: ast::Fn<'db>, mode: CItemKind) {
```

and

```rust
        let Some(def) = cx.sema.to_def2(node) else { return };
```

becomes

```rust
        let Some(def) = cx.vsema.to_def2(node) else { return };
```

and

```rust
        let this = params.as_ref().and_then(|it| it.self_param());
        let written = || params.iter().flat_map(|it| it.params()).filter_map(|it| it.ty());
```

becomes

```rust
        let this = params.and_then(|it| it.self_param());
        let written = || params.into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty());
```

and

```rust
            let this_span = this.as_ref().map(|this| this.ty().map_or(this.syntax().text_range(), |it| it.syntax().text_range()));
```

becomes

```rust
            let this_span = this.map(|this| this.ty().map_or(this.syntax().text_range(), |it| it.syntax().text_range()));
```

and

```rust
            let this_ty = this.as_ref().and_then(|it| it.ty());
```

becomes

```rust
            let this_ty = this.and_then(|it| it.ty());
```

and

```rust
                self.fnptrs(&mut types, &site, state, item.node.with_value(&hir_ty), input, mode);
```

becomes

```rust
                self.fnptrs(&mut types, &site, state, item.node.with_value(hir_ty), input, mode);
```

and

```rust
            if let Some(output) = &output {
```

becomes

```rust
            if let Some(output) = output {
```

7. `foreign_static` and `typed`. Why: as `function`.

```rust
    fn foreign_static(&mut self, item: &Item, s: &ast::Static) {
        let cx = self.cx;
        let Some(def) = cx.sema.to_def2(item.node.with_value(s)) else { return };
```

becomes

```rust
    fn foreign_static(&mut self, item: &Item<'db>, s: ast::Static<'db>) {
        let cx = self.cx;
        let Some(def) = cx.vsema.to_def2(item.node.with_value(s)) else { return };
```

and

```rust
    fn typed(&mut self, item: &Item, owner: GenericDefId, printed: hir::Type<'db>, ty: Ty<'db>, hir_ty: Option<ast::Type>) {
```

becomes

```rust
    fn typed(&mut self, item: &Item<'db>, owner: GenericDefId, printed: hir::Type<'db>, ty: Ty<'db>, hir_ty: Option<ast::Type<'db>>) {
```

and

```rust
        self.fnptrs(&mut types, &site, state, item.node.with_value(&hir_ty), ty, CItemKind::Definition);
```

becomes

```rust
        self.fnptrs(&mut types, &site, state, item.node.with_value(hir_ty), ty, CItemKind::Definition);
```

8. `power_alignment` and `fields`. Why: the ADT cast from the item's view after the kind test with
   nothing cloned; the walk's fields are `Copy` views, taken by value.

```rust
    fn power_alignment(&mut self, item: &Item) {
        let cx = self.cx;
        // clone: a cursor's refcount (no allocation), once per struct on AIX
        let Some(adt) = ast::Adt::cast(item.node.value.syntax().clone()) else { return };
        let Some(def) = cx.sema.to_def2(item.node.with_value(&adt)) else { return };
```

becomes

```rust
    fn power_alignment(&mut self, item: &Item<'db>) {
        let cx = self.cx;
        let Some(adt) = ast::Adt::cast(item.node.value.syntax()) else { return };
        let Some(def) = cx.vsema.to_def2(item.node.with_value(adt)) else { return };
```

and

```rust
        for field in walk::fields(item).iter().skip(1) {
            let Some(ty) = field_ty(cx, field) else { continue };
```

becomes

```rust
        for &field in walk::fields(item).iter().skip(1) {
            let Some(ty) = field_ty(cx, field) else { continue };
```

and

```rust
            let span = cx.span(field.as_ref().map(def_span));
```

becomes

```rust
            let span = cx.span(field.map(def_span));
```

and

```rust
            self.found.push(Found::lint(diag, Phase::Late, field.as_ref().map(SyntaxNodePtr::new)));
```

becomes

```rust
            self.found.push(Found::lint(diag, Phase::Late, field.map(ast::ptr)));
```

and

```rust
    fn fields(&mut self, item: &Item) {
        let cx = self.cx;
        let db = cx.db;
        // clone: a cursor's refcount (no allocation), once per ADT for its typed view
        let Some(adt) = ast::Adt::cast(item.node.value.syntax().clone()) else { return };
        let Some(def) = cx.sema.to_def2(item.node.with_value(&adt)) else { return };
        let mut types = Types::new(cx, GenericDefId::AdtId(AdtId::from(def)));
        for field in &walk::fields(item) {
            let Some(ty) = field_ty(cx, field) else { continue };
            let Some(hir_ty) = field.value.children().find_map(ast::Type::cast) else { continue };
            let Some(owner) = field_def(cx, field) else { continue };
            let site = Site { owner: owner.ty(db), at: field.as_ref().map(SyntaxNodePtr::new), item };
            let state = State::static_entry();
            self.fnptrs(&mut types, &site, state, field.with_value(&hir_ty), ty, CItemKind::Definition);
```

becomes

```rust
    fn fields(&mut self, item: &Item<'db>) {
        let cx = self.cx;
        let db = cx.db;
        let Some(adt) = ast::Adt::cast(item.node.value.syntax()) else { return };
        let Some(def) = cx.vsema.to_def2(item.node.with_value(adt)) else { return };
        let mut types = Types::new(cx, GenericDefId::AdtId(AdtId::from(def)));
        for &field in &walk::fields(item) {
            let Some(ty) = field_ty(cx, field) else { continue };
            let Some(hir_ty) = field.value.children().find_map(ast::Type::cast) else { continue };
            let Some(owner) = field_def(cx, field) else { continue };
            let site = Site { owner: owner.ty(db), at: field.map(ast::ptr), item };
            let state = State::static_entry();
            self.fnptrs(&mut types, &site, state, field.with_value(hir_ty), ty, CItemKind::Definition);
```

9. `field_def`, `field_ty`, `counted`. Why: a field's node by value, cast after the kind test with
   nothing cloned; its def by the view `Semantics` (so `'db` for both).

```rust
fn field_def(cx: &Cx<'_, '_>, field: &InFile<SyntaxNode>) -> Option<hir::Field> {
    match field.value.kind() {
        SyntaxKind::RECORD_FIELD => {
            // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
            let it = ast::RecordField::cast(field.value.clone())?;
            cx.sema.to_def2(field.with_value(&it))
        }
        SyntaxKind::TUPLE_FIELD => {
            // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
            let it = ast::TupleField::cast(field.value.clone())?;
            cx.sema.to_def2(field.with_value(&it))
        }
        _ => None,
    }
}

/// A field's type as declared.
fn field_ty<'db>(cx: &Cx<'_, 'db>, field: &InFile<SyntaxNode>) -> Option<Ty<'db>> {
    let id = hir_def::FieldId::from(field_def(cx, field)?);
    Some(cx.db.field_types(id.parent)[id.local_id].ty().skip_binder())
}

/// Whether a syntax node is a type rustc's HIR has one of: not the parentheses around one,
/// nor the `for<..>` of a `fn` pointer type.
fn counted(node: &SyntaxNode) -> bool {
    ast::Type::can_cast(node.kind()) && !ast::ParenType::can_cast(node.kind()) && !ast::ForType::can_cast(node.kind())
```

becomes

```rust
fn field_def<'db>(cx: &Cx<'_, 'db>, field: InFile<ast::Node<'db>>) -> Option<hir::Field> {
    match field.value.kind() {
        SyntaxKind::RECORD_FIELD => {
            let it = ast::RecordField::cast(field.value)?;
            cx.vsema.to_def2(field.with_value(it))
        }
        SyntaxKind::TUPLE_FIELD => {
            let it = ast::TupleField::cast(field.value)?;
            cx.vsema.to_def2(field.with_value(it))
        }
        _ => None,
    }
}

/// A field's type as declared.
fn field_ty<'db>(cx: &Cx<'_, 'db>, field: InFile<ast::Node<'db>>) -> Option<Ty<'db>> {
    let id = hir_def::FieldId::from(field_def(cx, field)?);
    Some(cx.db.field_types(id.parent)[id.local_id].ty().skip_binder())
}

/// Whether a syntax node is a type rustc's HIR has one of: not the parentheses around one,
/// nor the `for<..>` of a `fn` pointer type.
fn counted(node: ast::Node<'_>) -> bool {
    ast::Type::can_cast(node.kind()) && !ast::ParenType::can_cast(node.kind()) && !ast::ForType::can_cast(node.kind())
```

10. `ctypes_item` and `ctypes_nested`. Why: `Item<'db>`; the item and its container matched by value
   (both `Copy`); defs by the view `Semantics`.

```rust
pub(super) fn ctypes_item(cx: &Cx<'_, '_>, item: &Item, krate: &Krate) -> Vec<Found> {
    let mut pass = Pass { cx, found: Vec::new() }; // alloc: empty until a lint fires; holds only what fires
    let db = cx.db;
    match (&item.node.value, &item.container) {
        (ast::Item::Fn(f), Container::Extern(_)) => pass.function(item, f, CItemKind::Declaration),
        (ast::Item::Static(s), Container::Extern(block)) => {
            if abi_of(block.value.abi(), ExternAbi::C { unwind: false }).is_some_and(|it| !it.is_rustic_abi()) {
                pass.foreign_static(item, s);
            }
        }
        (ast::Item::Static(s), Container::Module) => {
            if let Some(def) = cx.sema.to_def2(item.node.with_value(s)) {
```

becomes

```rust
pub(super) fn ctypes_item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, krate: &Krate) -> Vec<Found> {
    let mut pass = Pass { cx, found: Vec::new() }; // alloc: empty until a lint fires; holds only what fires
    let db = cx.db;
    match (item.node.value, item.container) {
        (ast::Item::Fn(f), Container::Extern(_)) => pass.function(item, f, CItemKind::Declaration),
        (ast::Item::Static(s), Container::Extern(block)) => {
            if abi_of(block.value.abi(), ExternAbi::C { unwind: false }).is_some_and(|it| !it.is_rustic_abi()) {
                pass.foreign_static(item, s);
            }
        }
        (ast::Item::Static(s), Container::Module) => {
            if let Some(def) = cx.vsema.to_def2(item.node.with_value(s)) {
```

and

```rust
            if let Some(def) = cx.sema.to_def2(item.node.with_value(c)) {
```

becomes

```rust
            if let Some(def) = cx.vsema.to_def2(item.node.with_value(c)) {
```

and

```rust
            if let Some(def) = cx.sema.to_def2(item.node.with_value(t)) {
```

becomes

```rust
            if let Some(def) = cx.vsema.to_def2(item.node.with_value(t)) {
```

and

```rust
pub(super) fn ctypes_nested(cx: &Cx<'_, '_>, item: &Item) -> Vec<Found> {
    let mut pass = Pass { cx, found: Vec::new() }; // alloc: empty until a lint fires; holds only what fires
    match (&item.node.value, &item.container) {
        (ast::Item::Fn(_), Container::Extern(_)) => {}
        (ast::Item::Fn(f), _) if f.body().is_some() => pass.function(item, f, CItemKind::Definition),
```

becomes

```rust
pub(super) fn ctypes_nested<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
    let mut pass = Pass { cx, found: Vec::new() }; // alloc: empty until a lint fires; holds only what fires
    match (item.node.value, item.container) {
        (ast::Item::Fn(_), Container::Extern(_)) => {}
        (ast::Item::Fn(f), _) if f.body().is_some() => pass.function(item, f, CItemKind::Definition),
```

11. `link_name` and `lints`. Why: the function's view by value; `Item<'db>`; the item range of a
   view (`item_range_view`).

```rust
fn link_name(cx: &Cx<'_, '_>, id: FunctionId, f: &ast::Fn) -> Option<TextRange> {
```

becomes

```rust
fn link_name(cx: &Cx<'_, '_>, id: FunctionId, f: ast::Fn<'_>) -> Option<TextRange> {
```

and

```rust
pub(super) fn lints(cx: &Cx<'_, '_>, items: &[Item]) -> Vec<Found> {
    let db = cx.db;
    let mut decls = Vec::new_in(cx.frame());
    let mut orders = Orders::default();
    for item in items {
        let (ast::Item::Fn(f), Container::Extern(_)) = (&item.node.value, &item.container) else { continue };
        let Some(def) = cx.sema.to_def2(item.node.with_value(f)) else { continue };
        let Ok(id) = FunctionId::try_from(def) else { continue };
        let lo = cx.span(item.node.as_ref().map(|it| item_range(it.syntax()))).lo;
```

becomes

```rust
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>]) -> Vec<Found> {
    let db = cx.db;
    let mut decls = Vec::new_in(cx.frame());
    let mut orders = Orders::default();
    for item in items {
        let (ast::Item::Fn(f), Container::Extern(_)) = (item.node.value, item.container) else { continue };
        let Some(def) = cx.vsema.to_def2(item.node.with_value(f)) else { continue };
        let Ok(id) = FunctionId::try_from(def) else { continue };
        let lo = cx.span(item.node.map(|it| item_range_view(it.syntax()))).lo;
```

and

```rust
        let (orig_link, this_link) = (link_name(cx, *first_id, first_f), link_name(cx, *id, f));
        let relevant = |item: &Item, link: Option<TextRange>| {
            cx.span(item.node.as_ref().map(|it| link.unwrap_or_else(|| item_range(it.syntax()))))
        };
```

becomes

```rust
        let (orig_link, this_link) = (link_name(cx, *first_id, *first_f), link_name(cx, *id, *f));
        let relevant = |item: &Item<'_>, link: Option<TextRange>| {
            cx.span(item.node.map(|it| link.unwrap_or_else(|| item_range_view(it.syntax()))))
        };
```

Status: open

> forwarded to diagnostics (diagnostics-next2, the lead; diagnostics-lints-names is DONE); told by tmux (requests, pass 28).

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).


## frontend-tree: blaze-diagnostics lints/names/items/lifetime_syntax.rs: onto views (1)

From `frontend-tree-move-diag-lints-items` (`docs/v2/plan/lanes/frontend/tree-move-diag-lints-items.md`).
Lands with the `walk.rs` and `items.rs` headings above (`Item<'db>`, `item_id` of `'db`). Needs
`Cx::vsema` (`## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` in `v2/requests/diagnostics.md`,
frontend-tree-move-diag-lower) and `hir::view::Semantics::resolve_path(ast::Path<'db>)`
(tree-move-hir.md). The signature's types are walked as `Copy` views (a kind read and the
element numbers per step), nothing cloned; a lifetime's text is the `&str` of its token, compared
as is; a `SmolStr` is made only where it was (the names kept). Same lifetimes, same order, same
groups and the same diagnostics.

1. Imports. Why: views.

```rust
use syntax::{
    AstNode, SmolStr, SyntaxKind, SyntaxNode, TextRange, TextSize,
    ast::{self, HasGenericArgs, HasName},
};
```

becomes

```rust
use syntax::{
    SmolStr, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode, HasGenericArgs, HasName},
};
```

2. `item` and `check`. Why: `Item<'db>` and the function's view by value (the view `Semantics<'db>`
   takes `'db` views only); the referents of `self`'s references are `Copy` views.

```rust
pub(super) fn item(cx: &Cx<'_, '_>, item: &Item, anonymous_in_impl_trait: bool) -> Vec<Found> {
    let ast::Item::Fn(f) = &item.node.value else { return Vec::new() }; // alloc: empty, allocates nothing
```

becomes

```rust
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, anonymous_in_impl_trait: bool) -> Vec<Found> {
    let ast::Item::Fn(f) = item.node.value else { return Vec::new() }; // alloc: empty, allocates nothing
```

and

```rust
fn check(cx: &Cx<'_, '_>, item: &Item, f: &ast::Fn, anonymous_in_impl_trait: bool, found: &mut Vec<Found>) {
```

becomes

```rust
fn check<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, f: ast::Fn<'db>, anonymous_in_impl_trait: bool, found: &mut Vec<Found>) {
```

and

```rust
                for (inner, res) in &inputs.refs {
                    if names_self(cx, inner, impl_self.as_ref()) && !self_refs.contains(res) {
                        self_refs.push(*res);
                    }
```

becomes

```rust
                for &(inner, res) in &inputs.refs {
                    if names_self(cx, inner, impl_self) && !self_refs.contains(&res) {
                        self_refs.push(res);
                    }
```

and

```rust
                        Some(lifetime) => (
                            walk.named(&lifetime, &scope),
                            SmolStr::new(lifetime.text()),
                            syntax_of(&lifetime),
```

becomes

```rust
                        Some(lifetime) => (
                            walk.named(lifetime, &scope),
                            SmolStr::new(lifetime.text()),
                            syntax_of(lifetime),
```

3. `names_self`. Why: the referent and the impl's self type as views; paths resolved by the view
   `Semantics`.

```rust
fn names_self(cx: &Cx<'_, '_>, ty: &SyntaxNode, impl_self: Option<&ast::Type>) -> bool {
    let impl_def = impl_self.and_then(|it| match it {
        ast::Type::PathType(p) => p.path().and_then(|p| cx.sema.resolve_path(&p)),
```

becomes

```rust
fn names_self<'db>(cx: &Cx<'_, 'db>, ty: ast::Node<'db>, impl_self: Option<ast::Type<'db>>) -> bool {
    let impl_def = impl_self.and_then(|it| match it {
        ast::Type::PathType(p) => p.path().and_then(|p| cx.vsema.resolve_path(p)),
```

and

```rust
        matches!(cx.sema.resolve_path(&path), Some(PathResolution::Def(it)) if it == *def)
```

becomes

```rust
        matches!(cx.vsema.resolve_path(path), Some(PathResolution::Def(it)) if it == *def)
```

4. `Walked`. Why: a reference's referent is a `Copy` view of `'db`.

```rust
#[derive(Default)]
struct Walked {
    infos: SmallVec<[Info; 4]>,
    candidates: SmallVec<[Res; 4]>,
    /// Each reference's referent and lifetime, when [`Walk::refs`] asks for them.
    refs: SmallVec<[(SyntaxNode, Res); 2]>,
```

becomes

```rust
#[derive(Default)]
struct Walked<'db> {
    infos: SmallVec<[Info; 4]>,
    candidates: SmallVec<[Res; 4]>,
    /// Each reference's referent and lifetime, when [`Walk::refs`] asks for them.
    refs: SmallVec<[(ast::Node<'db>, Res); 2]>,
```

5. `Walk`'s methods. Why: nodes and lifetimes by value, cast after the kind test with nothing
   cloned; a path resolved by the view `Semantics` (so the walk names `'db`).

```rust
impl Walk<'_, '_, '_> {
```

becomes

```rust
impl<'db> Walk<'_, '_, 'db> {
```

and

```rust
    fn named(&mut self, lifetime: &ast::Lifetime, scope: &Scope) -> Option<Res> {
```

becomes

```rust
    fn named(&mut self, lifetime: ast::Lifetime<'_>, scope: &Scope) -> Option<Res> {
```

and

```rust
    fn push(&self, out: &mut Walked, scope: &Scope, mut info: Info) {
```

becomes

```rust
    fn push(&self, out: &mut Walked<'_>, scope: &Scope, mut info: Info) {
```

and

```rust
    fn ty(&mut self, node: &SyntaxNode, mut scope: Scope, out: &mut Walked) {
```

becomes

```rust
    fn ty(&mut self, node: ast::Node<'db>, mut scope: Scope, out: &mut Walked<'db>) {
```

and

```rust
    fn node(&mut self, node: &SyntaxNode, kind: SyntaxKind, scope: &Scope, out: &mut Walked) {
        if kind == SyntaxKind::REF_TYPE
            // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
            && let Some(r) = ast::RefType::cast(node.clone())
```

becomes

```rust
    fn node(&mut self, node: ast::Node<'db>, kind: SyntaxKind, scope: &Scope, out: &mut Walked<'db>) {
        if kind == SyntaxKind::REF_TYPE
            && let Some(r) = ast::RefType::cast(node)
```

and

```rust
                Some(lifetime) => {
                    (self.named(&lifetime, scope), syntax_of(&lifetime), lifetime.syntax().text_range(), SmolStr::new(lifetime.text()))
```

becomes

```rust
                Some(lifetime) => {
                    (self.named(lifetime, scope), syntax_of(lifetime), lifetime.syntax().text_range(), SmolStr::new(lifetime.text()))
```

and

```rust
                if self.refs
                    && let Some(inner) = &inner
                {
                    // clone: a cursor's refcount (no allocation), only in a `self` written with its type
                    out.refs.push((inner.syntax().clone(), res));
                }
```

becomes

```rust
                if self.refs
                    && let Some(inner) = inner
                {
                    out.refs.push((inner.syntax(), res));
                }
```

and

```rust
        if kind == SyntaxKind::PATH_TYPE
            // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
            && let Some(path) = ast::PathType::cast(node.clone()).and_then(|it| it.path())
        {
            let hidden = self.hidden(&path);
```

becomes

```rust
        if kind == SyntaxKind::PATH_TYPE
            && let Some(path) = ast::PathType::cast(node).and_then(|it| it.path())
        {
            let hidden = self.hidden(path);
```

and

```rust
        if kind == SyntaxKind::LIFETIME {
            // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
            let Some(lifetime) = ast::Lifetime::cast(node.clone()) else { return };
            let Some(source) = source_of(node) else { return };
            if let Some(res) = self.named(&lifetime, scope) {
                let (name, syntax, ident) = (SmolStr::new(lifetime.text()), syntax_of(&lifetime), node.text_range());
```

becomes

```rust
        if kind == SyntaxKind::LIFETIME {
            let Some(lifetime) = ast::Lifetime::cast(node) else { return };
            let Some(source) = source_of(node) else { return };
            if let Some(res) = self.named(lifetime, scope) {
                let (name, syntax, ident) = (SmolStr::new(lifetime.text()), syntax_of(lifetime), node.text_range());
```

and

```rust
        for child in node.children() {
            if child.kind() == SyntaxKind::LIFETIME && source_of(&child).is_none() {
                // An object lifetime bound or a declaration: elision still counts the former.
                if kind == SyntaxKind::TYPE_BOUND
                    && let Some(lifetime) = ast::Lifetime::cast(child)
                    && let Some(res) = self.named(&lifetime, scope)
```

becomes

```rust
        for child in node.children() {
            if child.kind() == SyntaxKind::LIFETIME && source_of(child).is_none() {
                // An object lifetime bound or a declaration: elision still counts the former.
                if kind == SyntaxKind::TYPE_BOUND
                    && let Some(lifetime) = ast::Lifetime::cast(child)
                    && let Some(res) = self.named(lifetime, scope)
```

and

```rust
            self.ty(&child, *scope, out);
```

becomes

```rust
            self.ty(child, *scope, out);
```

and

```rust
    fn hidden(&self, path: &ast::Path) -> usize {
        let given = path.segment().and_then(|it| it.generic_arg_list()).is_some_and(|it| it.lifetime_args().next().is_some());
        if given {
            return 0;
        }
        let def = match self.cx.sema.resolve_path(path) {
```

becomes

```rust
    fn hidden(&self, path: ast::Path<'db>) -> usize {
        let given = path.segment().and_then(|it| it.generic_arg_list()).is_some_and(|it| it.lifetime_args().next().is_some());
        if given {
            return 0;
        }
        let def = match self.cx.vsema.resolve_path(path) {
```

6. `source_of`, `syntax_of`, `mismatch`. Why: by value; `mismatch` hands the item to `item_id`
   (`'db`).

```rust
fn source_of(lifetime: &SyntaxNode) -> Option<Source> {
```

becomes

```rust
fn source_of(lifetime: ast::Node<'_>) -> Option<Source> {
```

and

```rust
fn syntax_of(lifetime: &ast::Lifetime) -> Syntax {
```

becomes

```rust
fn syntax_of(lifetime: ast::Lifetime<'_>) -> Syntax {
```

and

```rust
fn mismatch(cx: &Cx<'_, '_>, item: &Item, inputs: &[&Info], outputs: &[&Info]) -> Found {
```

becomes

```rust
fn mismatch<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, inputs: &[&Info], outputs: &[&Info]) -> Found {
```

Status: open

> forwarded to diagnostics (diagnostics-next2, the lead; diagnostics-lints-names is DONE); told by tmux (requests, pass 28).

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).


## frontend-tree: blaze-diagnostics lints/names/items/lifetimes.rs: onto views (1)

From `frontend-tree-move-diag-lints-items` (`docs/v2/plan/lanes/frontend/tree-move-diag-lints-items.md`).
Lands with the `walk.rs` and `items.rs` headings above (`Item<'db>`, `Item::nodes()` of views,
`item_id` of `'db`). Needs `Cx::vsema` (`## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` in
`v2/requests/diagnostics.md`, frontend-tree-move-diag-lower) and
`hir::view::Semantics::resolve_path(ast::Path<'db>)` (tree-move-hir.md). Binders, nodes and the
lifetime tokens a binder's scope names are `Copy` views: the uses of a binder are up to 8 inline
`Token<'db>`s with no refcount each, the scope walk is a scan of element numbers, and nothing is
cloned to cast a node. Same parameters, same uses, same order and the same diagnostics. The
tests parse into a `Parse` and read its root view.

1. Imports. Why: views (`ast::ptr` for the parameter's pointer).

```rust
use syntax::{
    AstNode, NodeOrToken, SmolStr, SyntaxKind, SyntaxNode, SyntaxNodePtr, SyntaxToken, TextRange, T,
    ast::{self, HasGenericArgs, HasGenericParams, HasTypeBounds},
};
```

becomes

```rust
use syntax::{
    NodeOrToken, SmolStr, SyntaxKind, TextRange, T,
    view::{self as ast, AstNode, HasGenericArgs, HasGenericParams, HasTypeBounds},
};
```

2. `Binder`. Why: `Copy` views by value, not references to handles.

```rust
enum Binder<'n> {
    Fn(&'n ast::Fn),
    Impl(&'n ast::Impl),
    FnPtr(&'n ast::FnPtrType),
```

becomes

```rust
#[derive(Clone, Copy)]
enum Binder<'t> {
    Fn(ast::Fn<'t>),
    Impl(ast::Impl<'t>),
    FnPtr(ast::FnPtrType<'t>),
```

3. `lints`. Why: `Item<'db>`; the item matched by value; its own nodes are `Copy` views, cast
   after the kind test with nothing cloned; binders and parameter lists passed by value.

```rust
pub(super) fn lints(cx: &Cx<'_, '_>, items: &[Item]) -> Vec<Found> {
```

becomes

```rust
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>]) -> Vec<Found> {
```

and

```rust
        let node = item.node.as_ref();
        match node.value {
```

becomes

```rust
        match item.node.value {
```

and

```rust
                    lints.binder(item, item.node.file_id, &Binder::Fn(f), f.syntax(), &params, span);
```

becomes

```rust
                    lints.binder(item, item.node.file_id, Binder::Fn(f), f.syntax(), params, span);
```

and

```rust
                    lints.binder(item, item.node.file_id, &Binder::Impl(imp), imp.syntax(), &params, span);
```

becomes

```rust
                    lints.binder(item, item.node.file_id, Binder::Impl(imp), imp.syntax(), params, span);
```

and

```rust
        for own in item.nodes() {
            match own.value.kind() {
                SyntaxKind::FOR_TYPE => {
                    // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
                    if let Some(ty) = ast::ForType::cast(own.value.clone())
```

becomes

```rust
        for &own in item.nodes() {
            match own.value.kind() {
                SyntaxKind::FOR_TYPE => {
                    if let Some(ty) = ast::ForType::cast(own.value)
```

and

```rust
                        lints.binder(item, own.file_id, &Binder::FnPtr(&ptr), ty.syntax(), &params, span);
```

becomes

```rust
                        lints.binder(item, own.file_id, Binder::FnPtr(ptr), ty.syntax(), params, span);
```

and

```rust
                    // clone: a cursor's refcount (no allocation) for the typed view, after the kind test
                    if let Some(pred) = ast::WherePred::cast(own.value.clone())
```

becomes

```rust
                    if let Some(pred) = ast::WherePred::cast(own.value)
```

and

```rust
                        lints.binder(item, own.file_id, &Binder::Where, pred.syntax(), &params, span);
```

becomes

```rust
                        lints.binder(item, own.file_id, Binder::Where, pred.syntax(), params, span);
```

4. `Lints::binder`. Why: `Item<'db>` and views of `'db` by value (the view `Semantics<'db>` takes
   `'db` views only); the uses are copied `Token`s; the parameter's pointer by `ast::ptr`.

```rust
impl Lints<'_, '_, '_, '_> {
```

becomes

```rust
impl<'db> Lints<'_, '_, 'db, '_> {
```

and

```rust
        item: &Item,
        file: hir::HirFileId,
        binder: &Binder<'_>,
        scope: &SyntaxNode,
        params: &ast::GenericParamList,
```

becomes

```rust
        item: &Item<'db>,
        file: hir::HirFileId,
        binder: Binder<'db>,
        scope: ast::Node<'db>,
        params: ast::GenericParamList<'db>,
```

and

```rust
        let linted = |param: &ast::GenericParam| match param {
```

becomes

```rust
        let linted = |param: ast::GenericParam<'_>| match param {
```

and

```rust
        if !params.generic_params().any(|it| linted(&it)) {
```

becomes

```rust
        if !params.generic_params().any(linted) {
```

and

```rust
        let all: SmallVec<[ast::GenericParam; 4]> = params.generic_params().collect();
```

becomes

```rust
        let all: SmallVec<[ast::GenericParam<'db>; 4]> = params.generic_params().collect();
```

and

```rust
            let mut named = uses.iter().filter(|it| it.text() == name);
```

becomes

```rust
            let mut named = uses.iter().copied().filter(|it| it.text() == name);
```

and

```rust
            let at = InFile::new(file, SyntaxNodePtr::new(param.syntax()));
```

becomes

```rust
            let at = InFile::new(file, ast::ptr(param.syntax()));
```

5. `Lints::single`. Why: the use, the scope and the binder by value; views compare as nodes do
   (tree and element number).

```rust
    fn single(&self, binder: &Binder<'_>, token: &SyntaxToken, scope: &SyntaxNode, name: &str, elision: &Elision) -> bool {
```

becomes

```rust
    fn single(&self, binder: Binder<'db>, token: ast::Token<'db>, scope: ast::Node<'db>, name: &str, elision: &Elision) -> bool {
```

and

```rust
        for node in lifetime.ancestors().take_while(|it| it != scope) {
            if own_ptr == Some(&node) {
```

becomes

```rust
        for node in lifetime.ancestors().take_while(|it| *it != scope) {
            if own_ptr == Some(node) {
```

and

```rust
        let within = |node: Option<&SyntaxNode>| node.is_some_and(|it| it.text_range().contains_range(lifetime.text_range()));
        let returns = |ret: Option<ast::RetType>| {
```

becomes

```rust
        let within = |node: Option<ast::Node<'_>>| node.is_some_and(|it| it.text_range().contains_range(lifetime.text_range()));
        let returns = |ret: Option<ast::RetType<'_>>| {
```

6. `Lints::elision`, `candidates`, `elided_in_path`, `self_lifetimes`, `named`. Why: types,
   paths and lifetimes by value; paths resolved by the view `Semantics`.

```rust
    fn elision(&self, binder: &Binder<'_>) -> Elision {
```

becomes

```rust
    fn elision(&self, binder: Binder<'db>) -> Elision {
```

and

```rust
                Some(ty) => self.self_lifetimes(&ty, impl_self.as_ref(), &mut fresh, &mut distinct),
```

becomes

```rust
                Some(ty) => self.self_lifetimes(ty, impl_self, &mut fresh, &mut distinct),
```

and

```rust
                    (Some(_), Some(lifetime)) => distinct.push(named(&lifetime, &mut fresh)),
```

becomes

```rust
                    (Some(_), Some(lifetime)) => distinct.push(named(lifetime, &mut fresh)),
```

and

```rust
            self.candidates(&ty, &mut fresh, &mut distinct);
```

becomes

```rust
            self.candidates(ty, &mut fresh, &mut distinct);
```

and

```rust
    fn candidates(&self, ty: &ast::Type, fresh: &mut u32, found: &mut SmallVec<[Res; 4]>) {
```

becomes

```rust
    fn candidates(&self, ty: ast::Type<'db>, fresh: &mut u32, found: &mut SmallVec<[Res; 4]>) {
```

and

```rust
                        found.push(named(&lifetime, fresh));
```

becomes

```rust
                        found.push(named(lifetime, fresh));
```

and

```rust
                        for _ in 0..self.elided_in_path(&path) {
```

becomes

```rust
                        for _ in 0..self.elided_in_path(path) {
```

and

```rust
    fn elided_in_path(&self, path: &ast::Path) -> usize {
```

becomes

```rust
    fn elided_in_path(&self, path: ast::Path<'db>) -> usize {
```

and

```rust
        let def = match self.cx.sema.resolve_path(path) {
```

becomes

```rust
        let def = match self.cx.vsema.resolve_path(path) {
```

and

```rust
    fn self_lifetimes(&self, ty: &ast::Type, impl_self: Option<&ast::Type>, fresh: &mut u32, found: &mut SmallVec<[Res; 4]>) {
        let impl_adt = impl_self.and_then(|it| match it {
            ast::Type::PathType(p) => p.path().and_then(|p| self.cx.sema.resolve_path(&p)),
```

becomes

```rust
    fn self_lifetimes(&self, ty: ast::Type<'db>, impl_self: Option<ast::Type<'db>>, fresh: &mut u32, found: &mut SmallVec<[Res; 4]>) {
        let impl_adt = impl_self.and_then(|it| match it {
            ast::Type::PathType(p) => p.path().and_then(|p| self.cx.vsema.resolve_path(p)),
```

and

```rust
        let names_self = |node: &SyntaxNode| {
```

becomes

```rust
        let names_self = |node: ast::Node<'db>| {
```

and

```rust
                match (&impl_adt, self.cx.sema.resolve_path(&path)) {
```

becomes

```rust
                match (&impl_adt, self.cx.vsema.resolve_path(path)) {
```

and

```rust
                Some(lifetime) => named(&lifetime, fresh),
```

becomes

```rust
                Some(lifetime) => named(lifetime, fresh),
```

and

```rust
fn named(lifetime: &ast::Lifetime, fresh: &mut u32) -> Res {
```

becomes

```rust
fn named(lifetime: ast::Lifetime<'_>, fresh: &mut u32) -> Res {
```

7. `uses`, `is_label_in_tokens`, `is_ref`, `extended_over_whitespace`, `deletion_span`. Why:
   the scope's preorder with tokens over views; tokens by value, up to 8 kept inline with no
   refcount; `text()` is the token's `&str`.

```rust
fn uses(scope: &SyntaxNode, binder: &Binder<'_>) -> SmallVec<[SyntaxToken; 8]> {
```

becomes

```rust
fn uses<'t>(scope: ast::Node<'t>, binder: Binder<'_>) -> SmallVec<[ast::Token<'t>; 8]> {
```

and

```rust
                let nested = &node != scope
```

becomes

```rust
                let nested = node != scope
```

and

```rust
                if !is_label_in_tokens(&token) {
```

becomes

```rust
                if !is_label_in_tokens(token) {
```

and

```rust
fn is_label_in_tokens(token: &SyntaxToken) -> bool {
```

becomes

```rust
fn is_label_in_tokens(token: ast::Token<'_>) -> bool {
```

and

```rust
fn is_ref(token: &SyntaxToken) -> bool {
```

becomes

```rust
fn is_ref(token: ast::Token<'_>) -> bool {
```

and

```rust
fn extended_over_whitespace(token: &SyntaxToken) -> TextRange {
```

becomes

```rust
fn extended_over_whitespace(token: ast::Token<'_>) -> TextRange {
```

and

```rust
fn deletion_span(params: &[ast::GenericParam], index: usize, generics_span: TextRange) -> Option<TextRange> {
    let range = |it: &ast::GenericParam| it.syntax().text_range();
```

becomes

```rust
fn deletion_span(params: &[ast::GenericParam<'_>], index: usize, generics_span: TextRange) -> Option<TextRange> {
    let range = |it: &ast::GenericParam<'_>| it.syntax().text_range();
```

8. Tests. Why: the fixtures parsed into a `Parse` whose root view the views borrow; the uses'
   texts compared as `&str`s.

```rust
    fn params(text: &str) -> (Vec<ast::GenericParam>, TextRange) {
        let file = syntax::SourceFile::parse(text, span::Edition::CURRENT).tree();
        let list = file.syntax().descendants().find_map(ast::GenericParamList::cast).expect("generics");
        (list.generic_params().collect(), list.syntax().text_range())
    }

    #[test]
    fn deletion_spans_take_a_comma() {
        let text = "fn f<'a, 'b, T>() {}";
        let (all, span) = params(text);
        assert_eq!(&text[deletion_span(&all, 0, span).unwrap()], "'a, ");
        assert_eq!(&text[deletion_span(&all, 1, span).unwrap()], ", 'b");
        let text = "fn f<'a>() {}";
        let (all, span) = params(text);
        assert_eq!(&text[deletion_span(&all, 0, span).unwrap()], "<'a>");
    }
```

becomes

```rust
    fn params(parse: &syntax::Parse<syntax::SourceFile>) -> (Vec<ast::GenericParam<'_>>, TextRange) {
        let list = parse.root().descendants().find_map(ast::GenericParamList::cast).expect("generics");
        (list.generic_params().collect(), list.syntax().text_range())
    }

    #[test]
    fn deletion_spans_take_a_comma() {
        let text = "fn f<'a, 'b, T>() {}";
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        let (all, span) = params(&parse);
        assert_eq!(&text[deletion_span(&all, 0, span).unwrap()], "'a, ");
        assert_eq!(&text[deletion_span(&all, 1, span).unwrap()], ", 'b");
        let text = "fn f<'a>() {}";
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        let (all, span) = params(&parse);
        assert_eq!(&text[deletion_span(&all, 0, span).unwrap()], "<'a>");
    }
```

and

```rust
        let file = syntax::SourceFile::parse(text, span::Edition::CURRENT).tree();
        let f = file.syntax().descendants().find_map(ast::Fn::cast).unwrap();
        let found: Vec<String> = uses(f.syntax(), &Binder::Fn(&f)).iter().map(|it| it.text().to_owned()).collect();
        assert_eq!(found, ["'a", "'a", "'b"]);
```

becomes

```rust
        let parse = syntax::SourceFile::parse(text, span::Edition::CURRENT);
        let f = parse.root().descendants().find_map(ast::Fn::cast).unwrap();
        let found: Vec<&str> = uses(f.syntax(), Binder::Fn(f)).iter().map(|it| it.text()).collect();
        assert_eq!(found, ["'a", "'a", "'b"]);
```

Status: open

> applied by frontend-tree-move-diag-lints-items under the grant (2026-10-29).

