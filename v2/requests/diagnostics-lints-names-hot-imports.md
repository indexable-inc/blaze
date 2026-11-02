# Requests for diagnostics-lints-names-hot-imports

> diagnostics (lane parent, window 67, 2026-10-29): **granted.** `diagnostics-lints-names` is done, so the
> authors of the `## frontend-tree: ...: onto views` headings in this file apply them directly
> (frontend-tree-move-diag-lints-names, -uses and the rest of frontend-tree-move-diag-lints'
> children). The terms are those under "## frontend-tree: blaze-diagnostics cx.rs: onto views (1)"
> in `v2/requests/diagnostics.md`: after that heading and `lints/syntax.rs (1)`, a function at a time,
> re-reading first, keeping the hot-lint comments. Mark each heading `> applied by <window>`.
> Live in these paths: `diagnostics-lints-names-stability-walk` (`stability/walk.rs`) and
> `diagnostics-hot3` (`imports/used.rs` `Used::derive`).

Paths: `crates/blaze-diagnostics/src/lints/names/imports.rs`, `crates/blaze-diagnostics/src/lints/names/imports/`.

> frontend-tree-move-diag-lints-uses (22:15), the six `onto views` headings: all exact once
> against the current text.
> - Written for what hot-imports asked: a `used.rs` heading (32 hunks, `Uses<'f, 'db>`; the other
>   files `Uses<'_, '_>`).
> - In the tree: `Cx::vsema`. `MacroCallLoc::to_view` is no longer needed (`imports.rs` calls
>   `to_node_view`).
> - Not yet in the tree: the parent's `lints/syntax.rs` views heading,
>   `SemanticsScope::speculative_resolve_mod_path` (`qualifications.rs`), and hir-def
>   `use_tree_sources_view` (`used.rs`; my heading in `v2/requests/db-expand.md`).
> - `diagnostics-hot3` is live in `used.rs`: hunks 22 and 32 were rebased on its `derive`. Rerun the
>   exact-once check before applying.
>
> diagnostics-lints-names (owner, 22:13) grants frontend-tree-move-diag-lints-uses to apply these
> headings itself once their prerequisites land: as exact edits, rebased first, noting
> `> applied by frontend-tree-move-diag-lints-uses` under each. Not applied yet (22:16).
> - 22:25: `lints/syntax.rs` (1) and `speculative_resolve_mod_path` are in. These six now wait only on
>   hir-def `use_tree_sources_view` (`v2/requests/db-expand.md`, src.rs (2)). They land together.

## frontend-tree: blaze-diagnostics lints/names/imports/macros.rs: onto views (1)

From `frontend-tree-move-diag-lints-uses` (frontend.md item 16: syntax handles become borrowed
`Copy` views; design `docs/v2/plan/lanes/frontend/tree.md`, notes
`docs/v2/plan/lanes/frontend/tree-move-diag-lints-uses.md`).

Needs: `## frontend-tree: blaze-diagnostics lints/syntax.rs: onto views (1)` (the parent's, in
`v2/requests/diagnostics.md`: `Tree::of_kind` gives `ast::Node<'db>` by value, `Walk::attrs`
takes `ast::AnyHasAttrs<'_>` and is an iterator), `Cx::vsema` (`frontend-tree-move-diag-lower`,
`cx.rs`), and `## frontend-tree: blaze-diagnostics lints/names/imports/used.rs: onto views (1)`
(`Uses<'f, 'db>`). Lands with those.

File: `crates/blaze-diagnostics/src/lints/names/imports/macros.rs`.

Hunk 1 (the view types; `AstNode` is only needed for `cast`):

```rust
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, T, TextRange,
    ast::{self, HasName, HasVisibility},
};
```

becomes

```rust
use syntax::{
    NodeOrToken, SyntaxKind, T, TextRange,
    view::{self as ast, AstNode as _, HasName, HasVisibility, Node},
};
```

Hunk 2 (`Uses` holds views, so it has the tree's lifetime):

```rust
pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>, uses: &Uses<'_>) -> Vec<Found> {
```

becomes

```rust
pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>, uses: &Uses<'_, '_>) -> Vec<Found> {
```

Hunk 3 (a view is `Copy`: no clone to cast, the macro passed by value, resolved through the view
`Semantics`):

```rust
            // clone: a rowan cursor's refcount, one per macro definition; `cast` takes the node
            let Some(def) = ast::Macro::cast(node.clone()) else { continue };
            let Some(name) = def.name() else { continue };
            let text = name.text();
            if text.starts_with('_') || uses.macros_unknown.contains(&name.as_name()) || exported(walk, &def) {
                continue;
            }
            let Some(it) = cx.sema.to_def(&def) else { continue };
```

becomes

```rust
            let Some(def) = ast::Macro::cast(node) else { continue };
            let Some(name) = def.name() else { continue };
            let text = name.text();
            if text.starts_with('_') || uses.macros_unknown.contains(&name.as_name()) || exported(walk, def) {
                continue;
            }
            let Some(it) = cx.vsema.to_def(def) else { continue };
```

Hunk 4:

```rust
            for (n, rule) in rules(&def).enumerate() {
```

becomes

```rust
            for (n, rule) in rules(def).enumerate() {
```

Hunk 5 (`Walk::attrs` takes the owner by value and is an iterator):

```rust
fn exported(walk: &Walk<'_>, def: &ast::Macro) -> bool {
    match def {
        // clone: a rowan cursor's refcount; `AnyHasAttrs` takes the node and has no borrowed form
        ast::Macro::MacroRules(it) => walk.attrs(&ast::AnyHasAttrs::new(it.clone())).iter().any(|(meta, _)| {
```

becomes

```rust
fn exported(walk: &Walk<'_>, def: ast::Macro<'_>) -> bool {
    match def {
        ast::Macro::MacroRules(it) => walk.attrs(ast::AnyHasAttrs::new(it)).any(|(meta, _)| {
```

Hunk 6:

```rust
fn rules(def: &ast::Macro) -> impl Iterator<Item = Rule> {
```

becomes

```rust
fn rules(def: ast::Macro<'_>) -> impl Iterator<Item = Rule> {
```

Hunk 7 (elements are `Copy`; match them by value):

```rust
        while let Some(element) = elements.next() {
            match &element {
```

becomes

```rust
        while let Some(element) = elements.next() {
            match element {
```

Hunk 8:

```rust
fn has_compile_error(tt: &SyntaxNode) -> bool {
```

becomes

```rust
fn has_compile_error(tt: Node<'_>) -> bool {
```

Hunk 9:

```rust
                if node.kind() == SyntaxKind::TOKEN_TREE && (seen == 2 || has_compile_error(&node)) {
```

becomes

```rust
                if node.kind() == SyntaxKind::TOKEN_TREE && (seen == 2 || has_compile_error(node)) {
```

Hunk 10 (the test reads the parse's view; the parse lives through the walk):

```rust
    use syntax::{AstNode, SourceFile, ast};

    use super::rules;

    fn spans(text: &str) -> Vec<(Vec<String>, bool)> {
        let file = SourceFile::parse(text, span::Edition::CURRENT).tree();
        let def = file.syntax().descendants().find_map(ast::Macro::cast).unwrap();
        rules(&def)
```

becomes

```rust
    use syntax::{
        SourceFile,
        view::{self as ast, AstNode as _},
    };

    use super::rules;

    fn spans(text: &str) -> Vec<(Vec<String>, bool)> {
        let parse = SourceFile::parse(text, span::Edition::CURRENT);
        let def = parse.root().descendants().find_map(ast::Macro::cast).unwrap();
        rules(def)
```

> forwarded to diagnostics-lints-names-hot-imports (window of that name); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 (2026-10-29), for `-uses` (no window left): every hunk as written; each still matched today's file.

## frontend-tree: blaze-diagnostics lints/names/imports/qualifications.rs: onto views (1)

From `frontend-tree-move-diag-lints-uses`. Needs: the parent's `lints/syntax.rs` heading,
`Cx::vsema`, the `used.rs` heading below (`Uses<'f, 'db>`), and of `frontend-tree-move-hir`
`SemanticsScope::speculative_resolve_mod_path(&self, path: &ModPath) -> Option<PathResolution<'db>>`
(the tail of `speculative_resolve`, after it builds its `ModPath`). The probe of each name was
a path made by `make::ext::ident_path`, a tree of its own with no view; it is now the `ModPath`
`speculative_resolve` built from that path (`self`, `super`, `crate` as their kinds, any other
name one segment, `Self` included), so nothing is parsed and the `alone` cache goes. Same
resolutions, so the same lints.

File: `crates/blaze-diagnostics/src/lints/names/imports/qualifications.rs`.

Hunk 1:

```rust
use hir_expand::{
    mod_path::PathKind,
    name::{AsName, Name},
};
use rustc_hash::FxHashMap;
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasGenericArgs, make},
};
```

becomes

```rust
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
};
use syntax::{
    SyntaxKind, T, TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs, Node},
};
```

Hunk 2 (the paths borrow the walk's trees for `'db`):

```rust
pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>, uses: &Uses<'_>, unused: &Unused) -> Vec<Found> {
```

becomes

```rust
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>, uses: &Uses<'_, '_>, unused: &Unused) -> Vec<Found> {
```

Hunk 3:

```rust
            // clone: a rowan cursor's refcount, one per path; `cast` takes the node
            let Some(path) = ast::Path::cast(node.clone()) else { continue };
            found.extend(qualification(cx, tree.file, &path, uses, unused, &mut scratch));
```

becomes

```rust
            let Some(path) = ast::Path::cast(node) else { continue };
            found.extend(qualification(cx, tree.file, path, uses, unused, &mut scratch));
```

Hunk 4:

```rust
/// What one path's check needs, kept from path to path: the segments, what each resolves to,
/// and the one-segment path of each name, parsed once.
#[derive(Default)]
struct Scratch<'db> {
    chain: Vec<ast::Path>,
    /// The trait's chain of a `<T as Trait>::a`.
    inner: Vec<ast::Path>,
    segments: Vec<(ast::Path, ast::PathSegment)>,
    resolved: Vec<Option<PathResolution<'db>>>,
    alone: FxHashMap<Name, ast::Path>,
}

fn qualification<'db>(
    cx: &Cx<'_, 'db>,
    file: HirFileId,
    path: &ast::Path,
    uses: &Uses<'_>,
```

becomes

```rust
/// What one path's check needs, kept from path to path: the segments and what each resolves to.
#[derive(Default)]
struct Scratch<'db> {
    chain: Vec<ast::Path<'db>>,
    /// The trait's chain of a `<T as Trait>::a`.
    inner: Vec<ast::Path<'db>>,
    segments: Vec<(ast::Path<'db>, ast::PathSegment<'db>)>,
    resolved: Vec<Option<PathResolution<'db>>>,
}

fn qualification<'db>(
    cx: &Cx<'_, 'db>,
    file: HirFileId,
    path: ast::Path<'db>,
    uses: &Uses<'_, '_>,
```

Hunk 5:

```rust
        || (ast::PathType::can_cast(kind) && !is_qualifying_trait(&parent))
```

becomes

```rust
        || (ast::PathType::can_cast(kind) && !is_qualifying_trait(parent))
```

Hunk 6:

```rust
    let Scratch { chain, inner, segments, resolved, alone } = scratch;
```

becomes

```rust
    let Scratch { chain, inner, segments, resolved } = scratch;
```

Hunk 7:

```rust
    let end = segments.iter().position(|(_, segment)| has_generic_args(segment)).map_or(segments.len(), |it| it + 1);
    resolve_into(cx, &segments[..end], resolved);
    let scope = cx.sema.scope(path.syntax())?;
    let module = ModuleId::from(scope.module());
    let (seg, name_ref, name) = (1..end).rev().find_map(|i| {
        let res = resolved[i]?;
        let name_ref = segments[i].1.name_ref()?;
        let name = name_ref.as_name();
        let probe = alone.entry(name).or_insert_with(|| make::ext::ident_path(name_ref.text()));
        let lexical = scope.speculative_resolve(probe)?;
```

becomes

```rust
    let end = segments.iter().position(|&(_, segment)| has_generic_args(segment)).map_or(segments.len(), |it| it + 1);
    resolve_into(cx, &segments[..end], resolved);
    let scope = cx.vsema.scope(path.syntax())?;
    let module = ModuleId::from(scope.module());
    let (seg, name_ref, name) = (1..end).rev().find_map(|i| {
        let res = resolved[i]?;
        let name_ref = segments[i].1.name_ref()?;
        let name = name_ref.as_name();
        let lexical = scope.speculative_resolve_mod_path(&alone(name_ref, name))?;
```

Hunk 8 (`view::ptr` is `SyntaxNodePtr::new` of a view; `alone` is the probe):

```rust
    Some(Found::lint(diag, Phase::Resolve, InFile::new(file, SyntaxNodePtr::new(&parent))))
}
```

becomes

```rust
    Some(Found::lint(diag, Phase::Resolve, InFile::new(file, ast::ptr(parent))))
}

/// The path `speculative_resolve` makes of `make::ext::ident_path` of `name_ref`'s text:
/// `self`, `super` and `crate` as the path kinds they are, any other name one segment.
fn alone(name_ref: ast::NameRef<'_>, name: Name) -> ModPath {
    match name_ref.syntax().first_token().map(|it| it.kind()) {
        Some(T![self]) => ModPath::from_kind(PathKind::Super(0)),
        Some(T![super]) => ModPath::from_kind(PathKind::Super(1)),
        Some(T![crate]) => ModPath::from_kind(PathKind::Crate),
        _ => ModPath::from_segments(PathKind::Plain, [name]),
    }
}
```

Hunk 9:

```rust
fn segments_into(
    path: &ast::Path,
    chain: &mut Vec<ast::Path>,
    inner: &mut Vec<ast::Path>,
    segments: &mut Vec<(ast::Path, ast::PathSegment)>,
) -> Option<()> {
```

becomes

```rust
fn segments_into<'t>(
    path: ast::Path<'t>,
    chain: &mut Vec<ast::Path<'t>>,
    inner: &mut Vec<ast::Path<'t>>,
    segments: &mut Vec<(ast::Path<'t>, ast::PathSegment<'t>)>,
) -> Option<()> {
```

Hunk 10:

```rust
                chain_into(&first.qualifying_trait()?.path()?, inner);
```

becomes

```rust
                chain_into(first.qualifying_trait()?.path()?, inner);
```

Hunk 11:

```rust
fn chain_into(path: &ast::Path, chain: &mut Vec<ast::Path>) {
    chain.clear();
    // clone: a rowan cursor's refcount; the chain owns its paths to walk up their qualifiers
    chain.push(path.clone());
```

becomes

```rust
fn chain_into<'t>(path: ast::Path<'t>, chain: &mut Vec<ast::Path<'t>>) {
    chain.clear();
    chain.push(path);
```

Hunk 12:

```rust
    segments: &[(ast::Path, ast::PathSegment)],
    resolved: &mut Vec<Option<PathResolution<'db>>>,
) {
    resolved.clear();
    let mut open = true;
    for (path, _) in segments {
        if !open {
            resolved.push(None);
            continue;
        }
        let res = cx.sema.resolve_path(path);
```

becomes

```rust
    segments: &[(ast::Path<'db>, ast::PathSegment<'db>)],
    resolved: &mut Vec<Option<PathResolution<'db>>>,
) {
    resolved.clear();
    let mut open = true;
    for &(path, _) in segments {
        if !open {
            resolved.push(None);
            continue;
        }
        let res = cx.vsema.resolve_path(path);
```

Hunk 13:

```rust
fn has_generic_args(segment: &ast::PathSegment) -> bool {
```

becomes

```rust
fn has_generic_args(segment: ast::PathSegment<'_>) -> bool {
```

Hunk 14:

```rust
fn is_qualifying_trait(ty: &SyntaxNode) -> bool {
```

becomes

```rust
fn is_qualifying_trait(ty: Node<'_>) -> bool {
```

> forwarded to diagnostics-lints-names-hot-imports (window of that name); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 (2026-10-29), for `-uses` (no window left): every hunk as written; each still matched today's file.

## frontend-tree: blaze-diagnostics lints/names/imports/globs.rs: onto views (1)

From `frontend-tree-move-diag-lints-uses`. Needs: the `used.rs` heading below (`Uses<'f, 'db>`,
`UseItem<'db>`, `Tree<'db>`, `ExternCrate<'db>` holding views), `cx::item_range_view`
(`frontend-tree-move-diag-lower`), `hir::view::HasSourceView` and
`Module::declaration_source_view` (`frontend-tree-move-hir`), `ModPath::from_src_view`
(`frontend-tree-move-expand`). The nodes a lint reports at are views of the same items: the
same spans and pointers, and nothing is cloned to keep one.

File: `crates/blaze-diagnostics/src/lints/names/imports/globs.rs`.

Hunk 1:

```rust
use hir::{HasSource, InFile, ModuleDef};
```

becomes

```rust
use hir::{InFile, ModuleDef, view::HasSourceView as _};
```

Hunk 2:

```rust
use syntax::{
    AstNode, SyntaxNode, SyntaxNodePtr,
    ast::{self, HasVisibility},
};

use super::used::{Binding, Entry, Ns, Tree, TreeKind, UseItem, Uses, entry};
use crate::{
    cx::{Cx, item_range},
```

becomes

```rust
use syntax::view::{self as ast, AstNode as _, HasVisibility, Node};

use super::used::{Binding, Entry, Ns, Tree, TreeKind, UseItem, Uses, entry};
use crate::{
    cx::{Cx, item_range_view},
```

Hunk 3 (`binding_node` gives views of `uses`' trees, so `uses` and the database share `'db`):

```rust
pub(super) fn lints(cx: &Cx<'_, '_>, uses: &Uses<'_>) -> Vec<Found> {
```

becomes

```rust
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, uses: &Uses<'_, 'db>) -> Vec<Found> {
```

Hunk 4:

```rust
fn glob_visibility(cx: &Cx<'_, '_>, uses: &Uses<'_>, item: &UseItem, tree: &Tree) -> Option<Found> {
```

becomes

```rust
fn glob_visibility(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>, tree: &Tree<'_>) -> Option<Found> {
```

Hunk 5 (`view::ptr` is `SyntaxNodePtr::new` of a view):

```rust
    diag.fact(cx.resolves_fact(from.into(), "*"), "what the glob import brings in");
    let at = InFile::new(item.node.file_id, SyntaxNodePtr::new(tree.node.syntax()));
```

becomes

```rust
    diag.fact(cx.resolves_fact(from.into(), "*"), "what the glob import brings in");
    let at = InFile::new(item.node.file_id, ast::ptr(tree.node.syntax()));
```

Hunk 6:

```rust
fn private_extern_crate(cx: &Cx<'_, '_>, uses: &Uses<'_>, item: &UseItem, tree: &Tree) -> Option<Found> {
```

becomes

```rust
fn private_extern_crate(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>, tree: &Tree<'_>) -> Option<Found> {
```

Hunk 7:

```rust
    let start = item_range(krate.node.value.syntax());
```

becomes

```rust
    let start = item_range_view(krate.node.value.syntax());
```

Hunk 8:

```rust
    diag.fact(cx.resolves_fact(item.module.into(), &ident), format!("what `{ident}` is imported from"));
    let at = InFile::new(item.node.file_id, SyntaxNodePtr::new(tree.node.syntax()));
```

becomes

```rust
    diag.fact(cx.resolves_fact(item.module.into(), &ident), format!("what `{ident}` is imported from"));
    let at = InFile::new(item.node.file_id, ast::ptr(tree.node.syntax()));
```

Hunk 9:

```rust
fn reexports(cx: &Cx<'_, '_>, uses: &Uses<'_>, module: ModuleId) -> Vec<Found> {
    let db = cx.db;
    // alloc: holds only diagnostics that fire; empty, it allocates nothing
    let mut found = Vec::new();
    let globs: SmallVec<[(&UseItem, &Tree, ModuleId); 2]> = uses
```

becomes

```rust
fn reexports<'db>(cx: &Cx<'_, 'db>, uses: &Uses<'_, 'db>, module: ModuleId) -> Vec<Found> {
    let db = cx.db;
    // alloc: holds only diagnostics that fire; empty, it allocates nothing
    let mut found = Vec::new();
    let globs: SmallVec<[(&UseItem<'db>, &Tree<'db>, ModuleId); 2]> = uses
```

Hunk 10:

```rust
            let imported: SmallVec<[(&UseItem, &Tree, Entry); 2]> = globs
```

becomes

```rust
            let imported: SmallVec<[(&UseItem<'db>, &Tree<'db>, Entry); 2]> = globs
```

Hunk 11:

```rust
                    let at = InFile::new(first_item.node.file_id, SyntaxNodePtr::new(first_item.node.value.syntax()));
```

becomes

```rust
                    let at = InFile::new(first_item.node.file_id, ast::ptr(first_item.node.value.syntax()));
```

Hunk 12 (`InFile<Node<'_>>` is `Copy`):

```rust
                    let span = cx.span(private.map(|it| item_span(&it)));
```

becomes

```rust
                    let span = cx.span(private.map(item_span));
```

Hunk 13:

```rust
                    found.push(Found::lint(diag, Phase::Resolve, private.map(|it| SyntaxNodePtr::new(&it))));
```

becomes

```rust
                    found.push(Found::lint(diag, Phase::Resolve, private.map(ast::ptr)));
```

Hunk 14 (views of the items, no cursor clone; `source_view` finds the node from the def's
`AstPtr` under `HirFileId::root(db)`, the same node `source` gave):

```rust
fn binding_node(cx: &Cx<'_, '_>, uses: &Uses<'_>, it: Entry) -> Option<InFile<SyntaxNode>> {
    match it.import {
        Some(binding @ Binding::Import(_)) => {
            let (item, tree) = uses.tree(binding)?;
            // clone: a rowan cursor's refcount, for `hidden_glob_reexports` once it fires
            Some(InFile::new(item.node.file_id, tree.node.syntax().clone()))
        }
        Some(Binding::ExternCrate(id)) => {
            let krate = uses.extern_crate_of(id)?;
            // clone: a rowan cursor's refcount, for `hidden_glob_reexports` once it fires
            Some(krate.node.as_ref().map(|it| it.syntax().clone()))
        }
        Some(Binding::Glob(_)) => None,
        None => def_node(cx, it.def),
    }
}

/// The item that defines `def`.
fn def_node(cx: &Cx<'_, '_>, def: ModuleDefId) -> Option<InFile<SyntaxNode>> {
    fn node<N: AstNode>(it: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
        // clone: a rowan cursor's refcount, for `hidden_glob_reexports` once it fires
        it.map(|it| it.map(|it| it.syntax().clone()))
    }
    let db = cx.db;
    match ModuleDef::from(def) {
        ModuleDef::Module(it) => node(it.declaration_source(db)),
        ModuleDef::Function(it) => node(it.source(db)),
        ModuleDef::Adt(hir::Adt::Struct(it)) => node(it.source(db)),
        ModuleDef::Adt(hir::Adt::Union(it)) => node(it.source(db)),
        ModuleDef::Adt(hir::Adt::Enum(it)) => node(it.source(db)),
        ModuleDef::Const(it) => node(it.source(db)),
        ModuleDef::Static(it) => node(it.source(db)),
        ModuleDef::Trait(it) => node(it.source(db)),
        ModuleDef::TypeAlias(it) => node(it.source(db)),
        ModuleDef::Macro(it) => node(it.source(db)),
        ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's span of a binding: an item's without its attributes, a `use` tree's whole.
fn item_span(node: &SyntaxNode) -> syntax::TextRange {
    if ast::UseTree::can_cast(node.kind()) { node.text_range() } else { item_range(node) }
}
```

becomes

```rust
fn binding_node<'db>(cx: &Cx<'_, 'db>, uses: &Uses<'_, 'db>, it: Entry) -> Option<InFile<Node<'db>>> {
    match it.import {
        Some(binding @ Binding::Import(_)) => {
            let (item, tree) = uses.tree(binding)?;
            Some(InFile::new(item.node.file_id, tree.node.syntax()))
        }
        Some(Binding::ExternCrate(id)) => {
            let krate = uses.extern_crate_of(id)?;
            Some(krate.node.map(|it| it.syntax()))
        }
        Some(Binding::Glob(_)) => None,
        None => def_node(cx, it.def),
    }
}

/// The item that defines `def`.
fn def_node<'db>(cx: &Cx<'_, 'db>, def: ModuleDefId) -> Option<InFile<Node<'db>>> {
    fn node<'db, N: ast::AstNode<'db>>(it: Option<InFile<N>>) -> Option<InFile<Node<'db>>> {
        it.map(|it| it.map(|it| it.syntax()))
    }
    let db = cx.db;
    match ModuleDef::from(def) {
        ModuleDef::Module(it) => node(it.declaration_source_view(db)),
        ModuleDef::Function(it) => node(it.source_view(db)),
        ModuleDef::Adt(hir::Adt::Struct(it)) => node(it.source_view(db)),
        ModuleDef::Adt(hir::Adt::Union(it)) => node(it.source_view(db)),
        ModuleDef::Adt(hir::Adt::Enum(it)) => node(it.source_view(db)),
        ModuleDef::Const(it) => node(it.source_view(db)),
        ModuleDef::Static(it) => node(it.source_view(db)),
        ModuleDef::Trait(it) => node(it.source_view(db)),
        ModuleDef::TypeAlias(it) => node(it.source_view(db)),
        ModuleDef::Macro(it) => node(it.source_view(db)),
        ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's span of a binding: an item's without its attributes, a `use` tree's whole.
fn item_span(node: Node<'_>) -> syntax::TextRange {
    if ast::UseTree::can_cast(node.kind()) { node.text_range() } else { item_range_view(node) }
}
```

Hunk 15:

```rust
fn item_visibility(cx: &Cx<'_, '_>, uses: &Uses<'_>, item: &UseItem) -> Option<Vis> {
```

becomes

```rust
fn item_visibility(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>) -> Option<Vis> {
```

Hunk 16:

```rust
            let path = ModPath::from_src(db, path, &mut |range| span_map.span_for_range(range).ctx)?;
```

becomes

```rust
            let path = ModPath::from_src_view(db, path, &mut |range| span_map.span_for_range(range).ctx)?;
```

> forwarded to diagnostics-lints-names-hot-imports (window of that name); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 (2026-10-29), for `-uses` (no window left): every hunk as written; each still matched today's file.

## frontend-tree: blaze-diagnostics lints/names/imports/unused.rs: onto views (1)

From `frontend-tree-move-diag-lints-uses`. Needs: the `used.rs` heading below (`Uses<'f, 'db>`,
`UseItem<'db>`, `Tree<'db>`, `ExternCrate<'db>`), `cx::item_range_view`
(`frontend-tree-move-diag-lower`), `hir::view::{HasSourceView, ModuleSourceView}` and
`Module::{definition,declaration}_source_view` (`frontend-tree-move-hir`). Same spans and
pointers from views of the same nodes; nothing cloned.

File: `crates/blaze-diagnostics/src/lints/names/imports/unused.rs`.

Hunk 1:

```rust
use syntax::{
    AstNode, NodeOrToken, SyntaxNode, SyntaxNodePtr, TextRange,
    ast::{self, HasName},
};

use super::used::{Binding, Entry, ExternCrate, Ns, SmallSet, Tree, TreeKind, UseItem, Used, Uses, entry};
use crate::{
    cx::{Cx, item_range},
```

becomes

```rust
use syntax::{
    NodeOrToken, TextRange,
    view::{self as ast, AstNode as _, HasName, Node},
};

use super::used::{Binding, Entry, ExternCrate, Ns, SmallSet, Tree, TreeKind, UseItem, Used, Uses, entry};
use crate::{
    cx::{Cx, item_range_view},
```

Hunk 2:

```rust
    pub(super) fn of(cx: &Cx<'_, '_>, uses: &Uses<'_>) -> Unused {
```

becomes

```rust
    pub(super) fn of(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>) -> Unused {
```

Hunk 3 (`view::ptr` is `SyntaxNodePtr::new` of a view):

```rust
            out.bodies.push(Found::lint(diag, Phase::Bodies, InFile::new(item.node.file_id, SyntaxNodePtr::new(at))));
```

becomes

```rust
            out.bodies.push(Found::lint(diag, Phase::Bodies, InFile::new(item.node.file_id, ast::ptr(at))));
```

Hunk 4 (`InFile<ast::ExternCrate<'_>>` is `Copy`):

```rust
    fn macro_use(&mut self, cx: &Cx<'_, '_>, uses: &Uses<'_>, krate: &ExternCrate) {
        let Some(loaded) = krate.krate else { return };
        let at = krate.node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
```

becomes

```rust
    fn macro_use(&mut self, cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, krate: &ExternCrate<'_>) {
        let Some(loaded) = krate.krate else { return };
        let at = krate.node.map(|it| ast::ptr(it.syntax()));
```

Hunk 5:

```rust
    fn extern_crate(&mut self, cx: &Cx<'_, '_>, uses: &Uses<'_>, krate: &ExternCrate, maybe_unused: &FxHashSet<hir_def::ExternCrateId>) {
        let node = &krate.node;
        let at = node.as_ref().map(|it| SyntaxNodePtr::new(it.syntax()));
        let name = krate.name.as_str();
        let module = hir::Module::from(krate.module);
        if !name.starts_with('_') && maybe_unused.contains(&krate.id) {
            let span = cx.span(node.as_ref().map(|it| item_range(it.syntax())));
```

becomes

```rust
    fn extern_crate(&mut self, cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, krate: &ExternCrate<'_>, maybe_unused: &FxHashSet<hir_def::ExternCrateId>) {
        let node = krate.node;
        let at = node.map(|it| ast::ptr(it.syntax()));
        let name = krate.name.as_str();
        let module = hir::Module::from(krate.module);
        if !name.starts_with('_') && maybe_unused.contains(&krate.id) {
            let span = cx.span(node.map(|it| item_range_view(it.syntax())));
```

Hunk 6:

```rust
            let removal = cx.span(node.as_ref().map(|it| with_attrs(it.syntax())));
```

becomes

```rust
            let removal = cx.span(node.map(|it| with_attrs(it.syntax())));
```

Hunk 7:

```rust
        let span = cx.span(node.as_ref().map(|it| item_range(it.syntax())));
        let Some(ident) = node.value.name_ref() else { return };
        let item = item_range(node.value.syntax());
```

becomes

```rust
        let span = cx.span(node.map(|it| item_range_view(it.syntax())));
        let Some(ident) = node.value.name_ref() else { return };
        let item = item_range_view(node.value.syntax());
```

Hunk 8:

```rust
    fn unused_imports(&mut self, cx: &Cx<'_, '_>, item: &UseItem, unused: &Trees) {
```

becomes

```rust
    fn unused_imports(&mut self, cx: &Cx<'_, '_>, item: &UseItem<'_>, unused: &Trees) {
```

Hunk 9:

```rust
        let at = InFile::new(file, SyntaxNodePtr::new(item.node.value.syntax()));
```

becomes

```rust
        let at = InFile::new(file, ast::ptr(item.node.value.syntax()));
```

Hunk 10:

```rust
fn is_used(uses: &Uses<'_>, binding: Binding, tree: &Tree) -> bool {
```

becomes

```rust
fn is_used(uses: &Uses<'_, '_>, binding: Binding, tree: &Tree<'_>) -> bool {
```

Hunk 11:

```rust
fn children(item: &UseItem) -> FxHashMap<usize, Vec<usize>> {
```

becomes

```rust
fn children(item: &UseItem<'_>) -> FxHashMap<usize, Vec<usize>> {
```

Hunk 12:

```rust
fn calc_unused_spans(
    item: &UseItem,
```

becomes

```rust
fn calc_unused_spans(
    item: &UseItem<'_>,
```

Hunk 13:

```rust
fn redundant(cx: &Cx<'_, '_>, uses: &Uses<'_>, item: &UseItem, tree: &Tree, binding: Binding) -> Option<Found> {
```

becomes

```rust
fn redundant(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, item: &UseItem<'_>, tree: &Tree<'_>, binding: Binding) -> Option<Found> {
```

Hunk 14:

```rust
            Bound::ExternCrate(krate) => (Some(krate.node.as_ref().map(|it| item_range(it.syntax()))), true),
```

becomes

```rust
            Bound::ExternCrate(krate) => (Some(krate.node.map(|it| item_range_view(it.syntax()))), true),
```

Hunk 15:

```rust
    let at = InFile::new(item.node.file_id, SyntaxNodePtr::new(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// Where the binding [`without`] finds is: placed only once its definition is known to be
/// the import's, since placing an item reads its source.
enum Bound<'u> {
```

becomes

```rust
    let at = InFile::new(item.node.file_id, ast::ptr(tree.node.syntax()));
    Some(Found::lint(diag, Phase::Resolve, at))
}

/// Where the binding [`without`] finds is: placed only once its definition is known to be
/// the import's, since placing an item reads its source.
enum Bound<'u, 'db> {
```

Hunk 16:

```rust
    ExternCrate(&'u ExternCrate),
```

becomes

```rust
    ExternCrate(&'u ExternCrate<'db>),
```

Hunk 17:

```rust
fn without<'u>(
    cx: &Cx<'_, '_>,
    uses: &'u Uses<'_>,
    item: &UseItem,
    binding: Binding,
    name: &Name,
    ns: Ns,
) -> Option<(ModuleDefId, Bound<'u>)> {
```

becomes

```rust
fn without<'u, 'db>(
    cx: &Cx<'_, '_>,
    uses: &'u Uses<'_, 'db>,
    item: &UseItem<'_>,
    binding: Binding,
    name: &Name,
    ns: Ns,
) -> Option<(ModuleDefId, Bound<'u, 'db>)> {
```

Hunk 18:

```rust
fn binding_range(cx: &Cx<'_, '_>, uses: &Uses<'_>, found: Entry) -> Option<(InFile<TextRange>, bool)> {
```

becomes

```rust
fn binding_range(cx: &Cx<'_, '_>, uses: &Uses<'_, '_>, found: Entry) -> Option<(InFile<TextRange>, bool)> {
```

Hunk 19:

```rust
            Some((krate.node.as_ref().map(|it| item_range(it.syntax())), true))
        }
        None => {
            let node = def_item(cx, found.def)?;
            Some((node.map(|it| item_range(&it)), false))
        }
    }
}

/// The item defining `def`, when it is one a block can have.
fn def_item(cx: &Cx<'_, '_>, def: ModuleDefId) -> Option<InFile<SyntaxNode>> {
    use hir::HasSource;
    fn node<N: AstNode>(it: Option<InFile<N>>) -> Option<InFile<SyntaxNode>> {
        // clone: a rowan cursor's refcount, for `redundant_imports` once it fires
        it.map(|it| it.map(|it| it.syntax().clone()))
    }
    let db = cx.db;
    match hir::ModuleDef::from(def) {
        hir::ModuleDef::Module(it) => node(it.declaration_source(db)),
        hir::ModuleDef::Function(it) => node(it.source(db)),
        hir::ModuleDef::Adt(hir::Adt::Struct(it)) => node(it.source(db)),
        hir::ModuleDef::Adt(hir::Adt::Union(it)) => node(it.source(db)),
        hir::ModuleDef::Adt(hir::Adt::Enum(it)) => node(it.source(db)),
        hir::ModuleDef::Const(it) => node(it.source(db)),
        hir::ModuleDef::Static(it) => node(it.source(db)),
        hir::ModuleDef::Trait(it) => node(it.source(db)),
        hir::ModuleDef::TypeAlias(it) => node(it.source(db)),
        hir::ModuleDef::Macro(it) => node(it.source(db)),
        hir::ModuleDef::EnumVariant(_) | hir::ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's `span_with_attributes` of an item: from its first attribute or doc comment.
fn with_attrs(node: &SyntaxNode) -> TextRange {
```

becomes

```rust
            Some((krate.node.map(|it| item_range_view(it.syntax())), true))
        }
        None => {
            let node = def_item(cx, found.def)?;
            Some((node.map(item_range_view), false))
        }
    }
}

/// The item defining `def`, when it is one a block can have.
fn def_item<'db>(cx: &Cx<'_, 'db>, def: ModuleDefId) -> Option<InFile<Node<'db>>> {
    use hir::view::HasSourceView as _;
    fn node<'db, N: ast::AstNode<'db>>(it: Option<InFile<N>>) -> Option<InFile<Node<'db>>> {
        it.map(|it| it.map(|it| it.syntax()))
    }
    let db = cx.db;
    match hir::ModuleDef::from(def) {
        hir::ModuleDef::Module(it) => node(it.declaration_source_view(db)),
        hir::ModuleDef::Function(it) => node(it.source_view(db)),
        hir::ModuleDef::Adt(hir::Adt::Struct(it)) => node(it.source_view(db)),
        hir::ModuleDef::Adt(hir::Adt::Union(it)) => node(it.source_view(db)),
        hir::ModuleDef::Adt(hir::Adt::Enum(it)) => node(it.source_view(db)),
        hir::ModuleDef::Const(it) => node(it.source_view(db)),
        hir::ModuleDef::Static(it) => node(it.source_view(db)),
        hir::ModuleDef::Trait(it) => node(it.source_view(db)),
        hir::ModuleDef::TypeAlias(it) => node(it.source_view(db)),
        hir::ModuleDef::Macro(it) => node(it.source_view(db)),
        hir::ModuleDef::EnumVariant(_) | hir::ModuleDef::BuiltinType(_) => None,
    }
}

/// rustc's `span_with_attributes` of an item: from its first attribute or doc comment.
fn with_attrs(node: Node<'_>) -> TextRange {
```

Hunk 20:

```rust
    let declared = hir::Module::from(module).declaration_source(db)?;
    let span = cx.span(declared.as_ref().map(|it| item_range(it.syntax())));
```

becomes

```rust
    let declared = hir::Module::from(module).declaration_source_view(db)?;
    let span = cx.span(declared.map(|it| item_range_view(it.syntax())));
```

Hunk 21 (the module's sources as views, the same attributes in the same order):

```rust
        let source = module.definition_source(db).value;
        let inner = match &source {
            hir::ModuleSource::SourceFile(it) => ast::attrs_including_inner(it).any(is_no_implicit_prelude),
            hir::ModuleSource::Module(it) => ast::attrs_including_inner(it).any(is_no_implicit_prelude),
            hir::ModuleSource::BlockExpr(_) => false,
        };
        if inner
            || module
                .declaration_source(db)
```

becomes

```rust
        let source = module.definition_source_view(db).value;
        let inner = match source {
            hir::view::ModuleSourceView::SourceFile(it) => ast::attrs_including_inner(&it).any(is_no_implicit_prelude),
            hir::view::ModuleSourceView::Module(it) => ast::attrs_including_inner(&it).any(is_no_implicit_prelude),
            hir::view::ModuleSourceView::BlockExpr(_) => false,
        };
        if inner
            || module
                .declaration_source_view(db)
```

Hunk 22:

```rust
fn is_no_implicit_prelude(attr: ast::Attr) -> bool {
```

becomes

```rust
fn is_no_implicit_prelude(attr: ast::Attr<'_>) -> bool {
```

> forwarded to diagnostics-lints-names-hot-imports (window of that name); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 (2026-10-29), for `-uses` (no window left): every hunk as written; each still matched today's file. Moved onto hir-def `use_tree_sources_view` (db-expand-defviews, `db-expand.md` src.rs (2)).

## frontend-tree: blaze-diagnostics lints/names/imports.rs: onto views (1)

From `frontend-tree-move-diag-lints-uses`. Needs nothing outside: `MacroCallLoc::to_node_view`
is in the tree (`crates/hir-expand/src/lib.rs`), the call node `to_node` gives as a view, and
only its ranges are read, so no `Semantics` is asked. Lands with the `used.rs` heading below
(`Uses::of` keeps its call). The same call node's range and its parent's: the same place, with no
cursor built.

File: `crates/blaze-diagnostics/src/lints/names/imports.rs`.

Hunk 1:

```rust
            let node = loc.to_node(cx.db);
```

becomes

```rust
            let node = loc.to_node_view(cx.db);
```

> forwarded to diagnostics-lints-names-hot-imports (window of that name); told by tmux (requests, pass 25). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 (2026-10-29), for `-uses` (no window left): every hunk as written; each still matched today's file.

## frontend-tree: blaze-diagnostics lints/names/imports/used.rs: onto views (1)

From `frontend-tree-move-diag-lints-uses`. Needs first: the parent's `lints/syntax.rs` views heading
(`Walk<'db>`, `Tree::of_kind` and `replaced` by value, `Walk::attrs(ast::AnyHasAttrs<'t>)` an iterator);
`hir_def::src::use_tree_sources_view(db, UseId) -> InFile<ArenaMap<Idx<ast::UseTree>,
view::UseTree<'db>>>`, the view twin of `UseId`'s `child_source` (requested in
`v2/requests/db-expand.md`, `## frontend-tree: hir-def src.rs: use_tree_sources_view, the UseId
child source as views (2)`). In the tree already: `Cx::vsema` and `cx::derive_path_tokens_view`
(`cx.rs`); the view `Semantics`' `scope`, `resolve_path`, `resolve_path_per_ns`,
`resolve_method_call`, `resolve_macro_call`, `resolve_macro_call_arm`, `resolve_attr_macro_call`,
`to_def`, `resolve_derive_macro(Meta)`; `ModPath::from_src_view` and `AsName` for the view names. Lands with the `macros.rs`,
`qualifications.rs`, `globs.rs`, `unused.rs` and `imports.rs` headings above. The items keep views
of the walk's trees (`'db`), so the eight cursor clones go; the same trees, bindings, places and
marks.

File: `crates/blaze-diagnostics/src/lints/names/imports/used.rs`.

Hunk 1:

```rust
    src::HasChildSource,
```

becomes

```rust
    src::use_tree_sources_view,
```

Hunk 2:

```rust
use syntax::{
    AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, TextRange, TextSize,
    ast::{self, HasAttrs, HasVisibility},
};
```

becomes

```rust
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs, HasVisibility, Node},
};
```

Hunk 3:

```rust
pub(super) struct Tree {
    pub(super) node: ast::UseTree,
```

becomes

```rust
pub(super) struct Tree<'db> {
    pub(super) node: ast::UseTree<'db>,
```

Hunk 4:

```rust
pub(super) struct UseItem {
    pub(super) node: InFile<ast::Use>,
    pub(super) id: UseId,
    /// The module it is in, a block's when it is in one.
    pub(super) module: ModuleId,
    /// `pub use`.
    pub(super) public: bool,
    /// Its trees in pre-order, the root first.
    pub(super) trees: Vec<Tree>,
}

/// An `extern crate` item of the crate.
pub(super) struct ExternCrate {
    pub(super) node: InFile<ast::ExternCrate>,
```

becomes

```rust
pub(super) struct UseItem<'db> {
    pub(super) node: InFile<ast::Use<'db>>,
    pub(super) id: UseId,
    /// The module it is in, a block's when it is in one.
    pub(super) module: ModuleId,
    /// `pub use`.
    pub(super) public: bool,
    /// Its trees in pre-order, the root first.
    pub(super) trees: Vec<Tree<'db>>,
}

/// An `extern crate` item of the crate.
pub(super) struct ExternCrate<'db> {
    pub(super) node: InFile<ast::ExternCrate<'db>>,
```

Hunk 5:

```rust
pub(super) struct Uses<'f> {
    frame: &'f Frame,
    /// The `use` items, in the order rustc's walk comes to them.
    pub(super) items: Vec<UseItem, &'f Frame>,
    /// The `extern crate` items, in that order.
    pub(super) crates: Vec<ExternCrate, &'f Frame>,
```

becomes

```rust
pub(super) struct Uses<'f, 'db> {
    frame: &'f Frame,
    /// The `use` items, in the order rustc's walk comes to them.
    pub(super) items: Vec<UseItem<'db>, &'f Frame>,
    /// The `extern crate` items, in that order.
    pub(super) crates: Vec<ExternCrate<'db>, &'f Frame>,
```

Hunk 6:

```rust
impl<'f> Uses<'f> {
    pub(super) fn of(cx: &Cx<'f, '_>, walk: &Walk<'_>, places: &Places) -> Uses<'f> {
```

becomes

```rust
impl<'f, 'db> Uses<'f, 'db> {
    pub(super) fn of(cx: &Cx<'f, 'db>, walk: &Walk<'db>, places: &Places) -> Uses<'f, 'db> {
```

Hunk 7 (a view is `Copy`: the item keeps the walk's node itself):

```rust
                match node.kind() {
                    // clone: a rowan cursor's refcount; the item keeps its node
                    SyntaxKind::USE => uses.use_item(cx, InFile::new(tree.file, ast::Use::cast(node.clone()).unwrap())),
                    SyntaxKind::EXTERN_CRATE => {
                        // clone: a rowan cursor's refcount; the item keeps its node
                        uses.extern_crate(cx, walk, InFile::new(tree.file, ast::ExternCrate::cast(node.clone()).unwrap()))
                    }
                    _ => {}
                }
            }
        }
        let place = |node: InFile<&SyntaxNode>| places.place(cx, node.map(SyntaxNodePtr::new));
        uses.items.sort_by_cached_key(|it| place(it.node.as_ref().map(|it| it.syntax())));
        uses.crates.sort_by_cached_key(|it| place(it.node.as_ref().map(|it| it.syntax())));
```

becomes

```rust
                match node.kind() {
                    SyntaxKind::USE => uses.use_item(cx, InFile::new(tree.file, ast::Use::cast(node).unwrap())),
                    SyntaxKind::EXTERN_CRATE => {
                        uses.extern_crate(cx, walk, InFile::new(tree.file, ast::ExternCrate::cast(node).unwrap()))
                    }
                    _ => {}
                }
            }
        }
        let place = |node: InFile<Node<'_>>| places.place(cx, node.map(ast::ptr));
        uses.items.sort_by_cached_key(|it| place(it.node.map(|it| it.syntax())));
        uses.crates.sort_by_cached_key(|it| place(it.node.map(|it| it.syntax())));
```

Hunk 8:

```rust
            // clone: a rowan cursor's refcount, one per item an attribute macro replaces
            for item in tree.replaced.iter().filter_map(|it| ast::Item::cast(it.clone())) {
```

becomes

```rust
            for item in tree.replaced.iter().filter_map(|&it| ast::Item::cast(it)) {
```

Hunk 9:

```rust
    pub(super) fn tree(&self, binding: Binding) -> Option<(&UseItem, &Tree)> {
```

becomes

```rust
    pub(super) fn tree(&self, binding: Binding) -> Option<(&UseItem<'db>, &Tree<'db>)> {
```

Hunk 10:

```rust
    pub(super) fn items_of(&self, module: ModuleId) -> impl Iterator<Item = &UseItem> + '_ {
```

becomes

```rust
    pub(super) fn items_of(&self, module: ModuleId) -> impl Iterator<Item = &UseItem<'db>> + '_ {
```

Hunk 11:

```rust
    pub(super) fn globs_of(&self, module: ModuleId) -> impl Iterator<Item = (&UseItem, &Tree)> + '_ {
```

becomes

```rust
    pub(super) fn globs_of(&self, module: ModuleId) -> impl Iterator<Item = (&UseItem<'db>, &Tree<'db>)> + '_ {
```

Hunk 12:

```rust
    pub(super) fn extern_crate_of(&self, id: ExternCrateId) -> Option<&ExternCrate> {
```

becomes

```rust
    pub(super) fn extern_crate_of(&self, id: ExternCrateId) -> Option<&ExternCrate<'db>> {
```

Hunk 13:

```rust
    pub(super) fn introduces(&self, it: &ExternCrate) -> bool {
```

becomes

```rust
    pub(super) fn introduces(&self, it: &ExternCrate<'_>) -> bool {
```

Hunk 14:

```rust
    fn use_item(&mut self, cx: &Cx<'_, '_>, node: InFile<ast::Use>) {
        let db = cx.db;
        let Some(scope) = cx.sema.scope(node.value.syntax()) else { return };
```

becomes

```rust
    fn use_item(&mut self, cx: &Cx<'_, 'db>, node: InFile<ast::Use<'db>>) {
        let db = cx.db;
        let Some(scope) = cx.vsema.scope(node.value.syntax()) else { return };
```

Hunk 15 (the same trees by the same indices, as views; the range match below is unchanged):

```rust
        let sources = id.child_source(db);
        let public = node
            .value
            .visibility()
            .is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub));
        let mut trees: Vec<Tree> = Vec::with_capacity(sources.value.values().count());
```

becomes

```rust
        let sources = use_tree_sources_view(db, id);
        let public = node
            .value
            .visibility()
            .is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub));
        let mut trees: Vec<Tree<'db>> = Vec::with_capacity(sources.value.values().count());
```

Hunk 16:

```rust
    fn extern_crate(&mut self, cx: &Cx<'_, '_>, walk: &Walk<'_>, node: InFile<ast::ExternCrate>) {
        let db = cx.db;
        let Some(decl) = cx.sema.to_def(&node.value) else { return };
```

becomes

```rust
    fn extern_crate(&mut self, cx: &Cx<'_, 'db>, walk: &Walk<'db>, node: InFile<ast::ExternCrate<'db>>) {
        let db = cx.db;
        let Some(decl) = cx.vsema.to_def(node.value) else { return };
```

Hunk 17:

```rust
        // clone: a rowan cursor's refcount; `AnyHasAttrs` takes the node and has no borrowed form
        let owner = ast::AnyHasAttrs::new(node.value.clone());
        for (meta, attr) in walk.attrs(&owner) {
```

becomes

```rust
        let owner = ast::AnyHasAttrs::new(node.value);
        for (meta, attr) in walk.attrs(owner) {
```

Hunk 18:

```rust
            // clone: a rowan cursor's refcount; the tree is read while `self` is written
            let tree = self.items[item].trees[j].node.clone();
```

becomes

```rust
            let tree = self.items[item].trees[j].node;
```

Hunk 19:

```rust
                Some(path) => ModPath::from_src(cx.db, path, &mut |range| span_map.span_for_range(range).ctx),
```

becomes

```rust
                Some(path) => ModPath::from_src_view(cx.db, path, &mut |range| span_map.span_for_range(range).ctx),
```

Hunk 20:

```rust
    fn node(&mut self, cx: &Cx<'_, '_>, walk: &Walk<'_>, file: HirFileId, node: &SyntaxNode) {
        let kind = node.kind();
        if kind == SyntaxKind::PATH {
            // clone: a rowan cursor's refcount, one per path; `cast` takes the node
            let path = ast::Path::cast(node.clone()).unwrap();
            self.path(cx, file, &path);
            if path.parent_path().is_none()
                && let Some(parent) = path.syntax().parent()
                && matches!(parent.kind(), SyntaxKind::PATH_EXPR | SyntaxKind::PATH_PAT | SyntaxKind::TUPLE_STRUCT_PAT)
            {
                self.type_relative(cx, &path);
            }
        } else if kind == SyntaxKind::METHOD_CALL_EXPR {
            // clone: a rowan cursor's refcount, one per method call; `cast` takes the node
            let call = ast::MethodCallExpr::cast(node.clone()).unwrap();
            let Some(name) = call.name_ref() else { return };
            let resolved = cx.sema.resolve_method_call(&call).map(|it| it.as_assoc_item(cx.db));
            self.lookup_trait_item(cx, call.syntax(), &name.as_name(), Some(resolved.flatten()));
        } else if kind == SyntaxKind::FIELD_EXPR {
            // clone: a rowan cursor's refcount, one per field access; `cast` takes the node
            let field = ast::FieldExpr::cast(node.clone()).unwrap();
            let Some(name) = field.name_ref() else { return };
            self.lookup_trait_item(cx, field.syntax(), &name.as_name(), None);
        } else if kind == SyntaxKind::MACRO_CALL {
            // clone: a rowan cursor's refcount, one per macro call; `cast` takes the node
            let call = ast::MacroCall::cast(node.clone()).unwrap();
            match cx.sema.resolve_macro_call(&call) {
                Some(it) => {
                    let id = MacroId::from(it);
                    self.macros.insert(id);
                    match cx.sema.resolve_macro_call_arm(&call) {
```

becomes

```rust
    fn node(&mut self, cx: &Cx<'_, 'db>, walk: &Walk<'db>, file: HirFileId, node: Node<'db>) {
        let kind = node.kind();
        if kind == SyntaxKind::PATH {
            let path = ast::Path::cast(node).unwrap();
            self.path(cx, file, path);
            if path.parent_path().is_none()
                && let Some(parent) = path.syntax().parent()
                && matches!(parent.kind(), SyntaxKind::PATH_EXPR | SyntaxKind::PATH_PAT | SyntaxKind::TUPLE_STRUCT_PAT)
            {
                self.type_relative(cx, path);
            }
        } else if kind == SyntaxKind::METHOD_CALL_EXPR {
            let call = ast::MethodCallExpr::cast(node).unwrap();
            let Some(name) = call.name_ref() else { return };
            let resolved = cx.vsema.resolve_method_call(call).map(|it| it.as_assoc_item(cx.db));
            self.lookup_trait_item(cx, call.syntax(), &name.as_name(), Some(resolved.flatten()));
        } else if kind == SyntaxKind::FIELD_EXPR {
            let field = ast::FieldExpr::cast(node).unwrap();
            let Some(name) = field.name_ref() else { return };
            self.lookup_trait_item(cx, field.syntax(), &name.as_name(), None);
        } else if kind == SyntaxKind::MACRO_CALL {
            let call = ast::MacroCall::cast(node).unwrap();
            match cx.vsema.resolve_macro_call(call) {
                Some(it) => {
                    let id = MacroId::from(it);
                    self.macros.insert(id);
                    match cx.vsema.resolve_macro_call_arm(call) {
```

Hunk 21:

```rust
        } else if ast::Adt::can_cast(kind)
            // clone: a rowan cursor's refcount, one per ADT; `cast` takes the node
            && let Some(owner) = ast::AnyHasAttrs::cast(node.clone())
            && owner.attrs().next().is_some()
        {
            for (meta, attr) in walk.attrs(&owner) {
                let ast::Meta::TokenTreeMeta(it) = &meta else { continue };
                if it.path().and_then(|it| it.as_single_name_ref()).is_none_or(|it| it.text() != "derive") {
                    continue;
                }
                self.derive(cx, node, &meta, &attr);
```

becomes

```rust
        } else if ast::Adt::can_cast(kind)
            && let Some(owner) = ast::AnyHasAttrs::cast(node)
            && owner.attrs().next().is_some()
        {
            for (meta, attr) in walk.attrs(owner) {
                let ast::Meta::TokenTreeMeta(it) = &meta else { continue };
                if it.path().and_then(|it| it.as_single_name_ref()).is_none_or(|it| it.text() != "derive") {
                    continue;
                }
                self.derive(cx, node, meta, attr);
```

Hunk 22:

```rust
    fn derive(&mut self, cx: &Cx<'_, '_>, owner: &SyntaxNode, meta: &ast::Meta, attr: &ast::Attr) {
        let Some(scope) = cx.sema.scope(owner) else { return };
        let module = ModuleId::from(scope.module());
        let resolved = cx.sema.resolve_derive_macro(meta).unwrap_or_default();
        let mut next = 0usize;
        crate::cx::derive_path_tokens(attr, |_, tokens| {
```

becomes

```rust
    fn derive(&mut self, cx: &Cx<'_, 'db>, owner: Node<'db>, meta: ast::Meta<'db>, attr: ast::Attr<'_>) {
        let Some(scope) = cx.vsema.scope(owner) else { return };
        let module = ModuleId::from(scope.module());
        let resolved = cx.vsema.resolve_derive_macro(meta).unwrap_or_default();
        let mut next = 0usize;
        crate::cx::derive_path_tokens_view(attr, |_, tokens| {
```

Hunk 23:

```rust
    fn attr_macro(&mut self, cx: &Cx<'_, '_>, item: InFile<ast::Item>) {
        if let Some(it) = cx.sema.resolve_attr_macro_call(&item.value) {
```

becomes

```rust
    fn attr_macro(&mut self, cx: &Cx<'_, 'db>, item: InFile<ast::Item<'db>>) {
        if let Some(it) = cx.vsema.resolve_attr_macro_call(item.value) {
```

Hunk 24:

```rust
                    self.path(cx, item.file_id, &path);
```

becomes

```rust
                    self.path(cx, item.file_id, path);
```

Hunk 25 (the chain holds `Copy` views; `from_src_view` takes one by value and it is read again):

```rust
    fn path(&mut self, cx: &Cx<'_, '_>, file: HirFileId, path: &ast::Path) {
        if path.parent_path().is_some() || path.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::USE_TREE) {
            return;
        }
        let Some(scope) = cx.sema.scope(path.syntax()) else { return };
        let module = ModuleId::from(scope.module());
        let span_map = file.span_map(cx.db);
        let mut chain: SmallVec<[ast::Path; 4]> = SmallVec::new();
        // clone: a rowan cursor's refcount; the chain owns its paths to walk up their qualifiers
        chain.push(path.clone());
        while let Some(qualifier) = chain.last().and_then(|it| it.qualifier()) {
            chain.push(qualifier);
        }
        chain.reverse();
        let mut done = 0;
        for (i, prefix) in chain.iter().enumerate() {
            // clone: a rowan cursor's refcount; `from_src` takes the path, read again below
            let Some(mod_path) = ModPath::from_src(cx.db, prefix.clone(), &mut |range| span_map.span_for_range(range).ctx) else {
```

becomes

```rust
    fn path(&mut self, cx: &Cx<'_, 'db>, file: HirFileId, path: ast::Path<'db>) {
        if path.parent_path().is_some() || path.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::USE_TREE) {
            return;
        }
        let Some(scope) = cx.vsema.scope(path.syntax()) else { return };
        let module = ModuleId::from(scope.module());
        let span_map = file.span_map(cx.db);
        let mut chain: SmallVec<[ast::Path<'db>; 4]> = SmallVec::new();
        chain.push(path);
        while let Some(qualifier) = chain.last().and_then(|it| it.qualifier()) {
            chain.push(qualifier);
        }
        chain.reverse();
        let mut done = 0;
        for (i, &prefix) in chain.iter().enumerate() {
            let Some(mod_path) = ModPath::from_src_view(cx.db, prefix, &mut |range| span_map.span_for_range(range).ctx) else {
```

Hunk 26:

```rust
                match cx.sema.resolve_path(prefix) {
```

becomes

```rust
                match cx.vsema.resolve_path(prefix) {
```

Hunk 27:

```rust
    fn type_relative(&mut self, cx: &Cx<'_, '_>, path: &ast::Path) {
```

becomes

```rust
    fn type_relative(&mut self, cx: &Cx<'_, 'db>, path: ast::Path<'db>) {
```

Hunk 28:

```rust
            _ => match cx.sema.resolve_path(&qualifier) {
                Some(PathResolution::Def(ModuleDef::Adt(hir::Adt::Enum(_)))) => {
                    !matches!(cx.sema.resolve_path(path), Some(PathResolution::Def(ModuleDef::EnumVariant(_))))
```

becomes

```rust
            _ => match cx.vsema.resolve_path(qualifier) {
                Some(PathResolution::Def(ModuleDef::Adt(hir::Adt::Enum(_)))) => {
                    !matches!(cx.vsema.resolve_path(path), Some(PathResolution::Def(ModuleDef::EnumVariant(_))))
```

Hunk 29:

```rust
        let resolved = match cx.sema.resolve_path(path) {
```

becomes

```rust
        let resolved = match cx.vsema.resolve_path(path) {
```

Hunk 30:

```rust
    fn lookup_trait_item(&mut self, cx: &Cx<'_, '_>, at: &SyntaxNode, name: &Name, picked: Option<Option<hir::AssocItem>>) {
        let Some(scope) = cx.sema.scope(at) else { return };
```

becomes

```rust
    fn lookup_trait_item(&mut self, cx: &Cx<'_, 'db>, at: Node<'db>, name: &Name, picked: Option<Option<hir::AssocItem>>) {
        let Some(scope) = cx.vsema.scope(at) else { return };
```

Hunk 31:

```rust
fn expected(cx: &Cx<'_, '_>, path: &ast::Path) -> Expected {
    match path.syntax().parent() {
        Some(parent) if ast::MacroCall::can_cast(parent.kind()) => {
            let Some(call) = ast::MacroCall::cast(parent) else { return Expected::Unknown };
            match cx.sema.resolve_macro_call(&call) {
                Some(it) => Expected::one(Ns::Macros, ModuleDefId::MacroId(it.into())),
                None => Expected::Unknown,
            }
        }
        Some(parent) if ast::Meta::can_cast(parent.kind()) => Expected::of([(Ns::Macros, cx.sema.resolve_path(path))]),
        _ => match cx.sema.resolve_path_per_ns(path) {
```

becomes

```rust
fn expected<'db>(cx: &Cx<'_, 'db>, path: ast::Path<'db>) -> Expected {
    match path.syntax().parent() {
        Some(parent) if ast::MacroCall::can_cast(parent.kind()) => {
            let Some(call) = ast::MacroCall::cast(parent) else { return Expected::Unknown };
            match cx.vsema.resolve_macro_call(call) {
                Some(it) => Expected::one(Ns::Macros, ModuleDefId::MacroId(it.into())),
                None => Expected::Unknown,
            }
        }
        Some(parent) if ast::Meta::can_cast(parent.kind()) => Expected::of([(Ns::Macros, cx.vsema.resolve_path(path))]),
        _ => match cx.vsema.resolve_path_per_ns(path) {
```

> forwarded to diagnostics-lints-names-hot-imports (window of that name); told by tmux (requests, pass 27). (requests, pass 27: now diagnostics-next2, the lead, since that window is gone and its lane DONE; told by tmux.)

Hunk 32 (the tokens `derive_path_tokens_view` gives are views; only their kind and text are read):

```rust
    fn derive_path(tokens: &[syntax::SyntaxToken]) -> (PathKind, SmallVec<[Name; 4]>) {
```

becomes

```rust
    fn derive_path(tokens: &[ast::Token<'_>]) -> (PathKind, SmallVec<[Name; 4]>) {
```

> granted by diagnostics (window 113, 2026-10-29 22:20): frontend-tree's agents apply this directly under the grant in `v2/requests/diagnostics.md` `## frontend-tree: blaze-diagnostics cx.rs: onto views (1)` (heading order, one function at a time, rereading before each edit), and mark it `> applied by <window>`. The `diagnostics-hot2-*` hot-lint pass (22:00) edited many of these files, so a staged base may have moved: merge onto today's file, and keep each `// alloc:`/`// clone:` comment where its line still allocates or clones.

> applied by frontend-tree-move-diag-lints-next1 (2026-10-29), for `-uses` (no window left): every hunk as written; each still matched today's file. Moved onto hir-def `use_tree_sources_view` (db-expand-defviews, `db-expand.md` src.rs (2)).

