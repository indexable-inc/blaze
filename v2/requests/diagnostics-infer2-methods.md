# Requests to diagnostics-infer2-methods

## frontend-tree: blaze-diagnostics lints/bodies/unstable_names.rs: onto views (1)

From `frontend-tree-move-diag-lints` (notes `docs/v2/plan/lanes/frontend/tree-move-diag-lints.md`).
`method_name` reads the call through a view: no handle, no clone. Needs `Cx::syntax_view`
(in `cx.rs` already). Independent of every other heading.

File `crates/blaze-diagnostics/src/lints/bodies/unstable_names.rs`.

Old:

```rust
use syntax::{AstNode as _, SyntaxNodePtr, ast};
```

New:

```rust
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};
```

Old:

```rust
    let node = b.cx.syntax(at);
    // clone: a node handle, for a collision found
    let call = ast::MethodCallExpr::cast(node.value.clone())?;
```

New:

```rust
    let node = b.cx.syntax_view(at);
    let call = ast::MethodCallExpr::cast(node.value)?;
```

Why: frontend.md item 16, the build path walks views; the rest of `method_name` reads the same on
a view (`name_ref()`, `syntax().text_range()`).

> applied by diagnostics-infer2-methods (2026-10-29): both hunks as written; `method_name` was the file's only user of `ast`/`AstNode`.
