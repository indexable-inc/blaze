---
status: plan
summary: The context every lint of blaze-lints is written against in v2 (`check::Cx`): views of one query's tree, typed facts through hir::BodyAnalysis, findings relative to the tree.
date: 2026-10-29
---
# `check::Cx`: what a lint is written against

Owner of the implementation: `lints-driver` (`crates/blaze-lints/src/check.rs`,
`query.rs`). The lints (`lints-port-*`, `lints-blaze`) are written against exactly this; a
change goes through the lane `lints` (tmux window `lints`) so every side changes at once.

## Syntax: views

Every lint file writes `use syntax::view as ast;` and walks `ast::Node<'t>`, `ast::Token<'t>`
and the typed views (`ast::Expr<'t>`, `ast::MethodCallExpr<'t>`, ...), `Copy`, accessors taking
`self` ([frontend/tree.md](../frontend/tree.md)). No `syntax::SyntaxNode`, no `.clone()` of a
view (it is `Copy`), no `node.text().to_string()` (`text()` is `&'t str`). Ranges are
`syntax::TextRange` of the tree being walked.

## The context

```rust
pub(crate) struct Cx<'q, 't> { /* private */ }
// 'q: the database borrow (queries' values live as long); 't: the tree walked ('q: 't).

impl<'q, 't> Cx<'q, 't> {
    // what is linted
    pub(crate) fn db(&self) -> &'q base_db::Db;
    pub(crate) fn conf(&self) -> &'q crate::conf::Conf;
    pub(crate) fn krate(&self) -> hir::Crate;
    pub(crate) fn edition(&self) -> span::Edition;
    pub(crate) fn on(&self, lint: Implemented) -> bool;      // clippy lint may be above allow here
    pub(crate) fn on_any(&self, lints: lints::On) -> bool;
    pub(crate) fn on_blaze(&self, lint: blaze::Lint) -> bool;

    // the tree
    pub(crate) fn root(&self) -> ast::Node<'t>;              // the anchor (a body's) or the file
    pub(crate) fn text(&self) -> &'t str;                    // text of `root`, its ranges index it
    pub(crate) fn snippet(&self, range: TextRange) -> &'t str;
    pub(crate) fn range(&self, node: ast::Node<'t>) -> Option<TextRange>; // where written in `root`;
                                                             // None for what a macro made up
    pub(crate) fn original_range(&self, node: ast::Node<'t>) -> Option<TextRange>;
    pub(crate) fn source_of(&self, node: ast::Node<'t>) -> std::borrow::Cow<'t, str>;
    pub(crate) fn in_file(&self) -> bool;                    // not in a macro's expansion
    pub(crate) fn in_bang_call(&self) -> bool;
    pub(crate) fn is_written(&self, node: ast::Node<'t>) -> bool;
    pub(crate) fn expansion(&self) -> Option<&crate::diag::Expansion>;

    // macros
    pub(crate) fn std_macro(&self, call: ast::MacroCall<'t>) -> Option<intern::Symbol>;
    pub(crate) fn expand(&self, call: ast::MacroCall<'t>) -> Option<ast::Node<'t>>; // its expansion's root
    pub(crate) fn macro_args(&self, call: ast::MacroCall<'t>, out: &mut Vec<(TextRange, ast::Expr<'t>)>);

    // types: each asks hir::BodyAnalysis, which infers the body the first time; None outside a body
    pub(crate) fn ty(&self, expr: ast::Expr<'t>) -> Option<hir::Type<'q>>;          // before adjustments
    pub(crate) fn adjusted_ty(&self, expr: ast::Expr<'t>) -> Option<hir::Type<'q>>;
    pub(crate) fn adjustments(&self, expr: ast::Expr<'t>) -> Option<Vec<hir::Adjustment<'q>>>;
    pub(crate) fn method(&self, call: ast::MethodCallExpr<'t>) -> Option<hir::Function>;
    pub(crate) fn path(&self, path: ast::Path<'t>) -> Option<hir::PathResolution<'q>>;
    pub(crate) fn pat_ty(&self, pat: ast::Pat<'t>) -> Option<hir::Type<'q>>;
    pub(crate) fn local(&self, pat: ast::IdentPat<'t>) -> Option<hir::Local<'q>>;
    pub(crate) fn analysis(&self) -> Option<&hir::BodyAnalysis<'q>>; // for HIR-level walks (blaze::)

    // known items: what ide-db's FamousDefs gave, from crate::famous
    pub(crate) fn famous(&self) -> crate::famous::Famous<'q>;
    pub(crate) fn lang_trait(&self, item: hir::LangItem) -> Option<hir::Trait>;
    pub(crate) fn std_def(&self, path: &[&str]) -> Option<hir::ModuleDef>;
    pub(crate) fn std_trait(&self, path: &[&str]) -> Option<hir::Trait>;
    pub(crate) fn assoc_ty(&self, ty: &hir::Type<'q>, trait_: hir::Trait, name: &str) -> Option<hir::Type<'q>>;

    // reporting: spans are of `root`; the driver moves them into the file and names it
    pub(crate) fn span(&self, range: TextRange) -> crate::diag::Span;
    pub(crate) fn item_span(&self, node: ast::Node<'t>) -> Option<crate::diag::Span>;
    pub(crate) fn lint(&mut self, lint: Implemented, at: ast::Node<'t>,
        span: impl Into<MultiSpan>, message: impl Into<String>) -> &mut Diag;
    pub(crate) fn lint_when(&mut self, lint: Implemented, at: ast::Node<'t>, when: ast::Node<'t>,
        span: impl Into<MultiSpan>, message: impl Into<String>) -> &mut Diag;
    pub(crate) fn lint_after(&mut self, lint: Implemented, at: ast::Node<'t>, after: ast::Node<'t>,
        span: impl Into<MultiSpan>, message: impl Into<String>) -> &mut Diag;
    pub(crate) fn blaze(&mut self, lint: blaze::Lint, at: ast::Node<'t>,
        span: impl Into<MultiSpan>, message: impl Into<String>) -> &mut Diag;
}
```

`Diag` (`blaze_diagnostics::model`'s, re-exported by `crate::diag`), `MultiSpan`, `Span`,
`Applicability` and their builders (`suggestion`, `multipart_suggestion`,
`suggestion_with_style`, `help`, `note`, `span_note`, `span_help`, `span_label`, `sub`) are
used as today; a lint builds them as it does now (`span_note` and `span_help` take
`impl Into<MultiSpan>`, which a `Span` is).

## Where lints are run

`lints::check_node(cx, node: ast::Node<'t>)` (`lints/mod.rs`) is called by the walk for each
node of the tree written in the file or handed to a macro, as now. Item-level lints
(`crate_in_macro_def`, `empty_enums`, `new_without_default`, `nonstandard_macro_braces`,
`redundant_static_lifetimes`, `upper_case_acronyms`) run in `item_lints`; the rest in
`body_lints`. `blaze::check_node(cx, node)` runs blaze's own, called from both walks for every
node, the root included; its body pass runs once, at `node == cx.root()` with
`cx.analysis()` `Some`. `cx.path` answers at item level too, in the scope of the item the path
is written in.
Each lint module keeps its `pub(super) fn check(cx: &mut Cx<'_, '_>, node: ast::X<'_>)` shape.

## Buffers

`macro_args` and anything else that returns several things writes into a buffer the caller
passes. A lint allocates only to report (the message, the suggestion).
