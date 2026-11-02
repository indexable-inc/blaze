//! The lints rustc's parser buffers: `varargs_without_pattern` and `break_with_label_and_loop`.
//!
//! The parser buffers them at the crate (`CRATE_NODE_ID`), so the crate's attributes decide
//! their level and they are emitted as the early lint pass starts, in the order the source was
//! parsed: the crate's files, then what macros expand to. It parses what `cfg` then removes
//! and the items attribute macros are given, so all of each tree is linted.
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/function.rs` (`parse_param_general`),
//! `parser/item.rs` (the `FnParseMode` of free, impl, trait and foreign functions),
//! `parser/expr.rs` (`parse_expr_break`) and `diagnostics.rs` (`VarargsWithoutPattern`,
//! `BreakWithLabelAndLoop`) (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use syntax::{NodeOrToken, SyntaxKind, SyntaxNodePtr, T, TextRange, TextSize, view::{self as ast, AstNode as _}};

use super::{edition_at, emitted, span};
use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Stage, Tree, Walk},
    model::{Applicability, Diag, Span},
};

/// A lint the parser buffered, keyed by where rustc's parser comes to it: files before
/// expansions, then the walk's order of trees, then the offset in the tree.
struct Hit {
    key: (bool, u32, TextSize),
    emitted: Emitted,
}

/// The parser's lints, gathered as [`super::lints`] passes over each node and put in the
/// parser's order at the end. Hits are rare, so sorting them costs nothing next to the walk.
pub(super) struct Parsed {
    crate_at: Option<InFile<SyntaxNodePtr>>,
    hits: Vec<Hit>,
}

impl Parsed {
    pub(super) fn new(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Parsed {
        let crate_at = cx
            .krate
            .root_module(cx.db)
            .as_source_file_id(cx.db)
            .and_then(|it| walk.tree(HirFileId::from(it)))
            .map(|tree| tree.ptr(tree.root));
        // alloc: empty until a lint fires; holds only what fires
        Parsed { crate_at, hits: Vec::new() }
    }

    /// Checks `node` of `tree`, the `index`th tree of the walk: one `kind` read for any node
    /// that is neither a parameter nor a `break`.
    pub(super) fn check(&mut self, cx: &Cx<'_, '_>, tree: &Tree<'_>, index: u32, node: ast::Node<'_>) {
        let Some(crate_at) = self.crate_at else { return };
        let found = match node.kind() {
            SyntaxKind::PARAM => varargs_without_pattern(cx, tree, node, crate_at),
            SyntaxKind::BREAK_EXPR => break_with_label_and_loop(cx, tree, node, crate_at),
            _ => return,
        };
        if let Some(emitted) = found {
            let key = (!tree.in_file(), index, node.text_range().start());
            self.hits.push(Hit { key, emitted });
        }
    }

    pub(super) fn finish(mut self) -> impl Iterator<Item = Emitted> {
        self.hits.sort_by_key(|it| it.key);
        self.hits.into_iter().map(|it| it.emitted)
    }
}

/// `varargs_without_pattern`: a `...` parameter with no pattern where rustc's parser asks for
/// one: of a free function, an impl's, or a trait's from Rust 2018 (by the edition of the
/// `...`); never of a foreign function, a function pointer or a closure.
fn varargs_without_pattern(
    cx: &Cx<'_, '_>,
    tree: &Tree<'_>,
    param: ast::Node<'_>,
    crate_at: InFile<SyntaxNodePtr>,
) -> Option<Emitted> {
    // Most parameters start with their pattern: the first element read decides.
    let first = param.children_with_tokens().find(|it| !it.kind().is_trivia() && it.kind() != SyntaxKind::ATTR);
    let Some(NodeOrToken::Token(dots)) = first else { return None };
    if dots.kind() != T![...] {
        return None;
    }
    let list = param.parent().filter(|it| it.kind() == SyntaxKind::PARAM_LIST)?;
    let item = list.parent().filter(|it| it.kind() == SyntaxKind::FN)?;
    let required = match item.parent().map(|it| it.kind()) {
        Some(SyntaxKind::EXTERN_ITEM_LIST) => false,
        Some(SyntaxKind::ASSOC_ITEM_LIST) => {
            let in_trait = item.parent().and_then(|it| it.parent()).is_some_and(|it| it.kind() == SyntaxKind::TRAIT);
            !in_trait || edition_at(cx, tree.file, dots.text_range().start()).at_least_2018()
        }
        _ => true,
    };
    if !required {
        return None;
    }
    let span = span(cx, tree, dots.text_range());
    // clone: a lint's span, for a lint found
    let mut diag = Diag::lint("varargs_without_pattern", "missing pattern for `...` argument", span.clone());
    diag.suggestion(
        span,
        "name the argument, or use `_` to continue ignoring it",
        "_: ...",
        Applicability::MachineApplicable,
    );
    Some(emitted(cx, diag, Pass::Buffered, crate_at, tree.ptr(param)).buffered(stage(tree)))
}

/// `break_with_label_and_loop`: `break 'label` whose value is an unlabeled `loop`, `while`,
/// `for` or plain block, which reads as an unlabeled `break` of a labeled loop.
fn break_with_label_and_loop(
    cx: &Cx<'_, '_>,
    tree: &Tree<'_>,
    node: ast::Node<'_>,
    crate_at: InFile<SyntaxNodePtr>,
) -> Option<Emitted> {
    let expr = ast::BreakExpr::cast(node)?;
    let (Some(break_), Some(_), Some(value)) = (expr.break_token(), expr.lifetime(), expr.expr()) else { return None };
    let unlabeled = |node: ast::Node<'_>| !node.children().any(|it| it.kind() == SyntaxKind::LABEL);
    let confusable = match value {
        ast::Expr::LoopExpr(it) => unlabeled(it.syntax()),
        ast::Expr::WhileExpr(it) => unlabeled(it.syntax()),
        ast::Expr::ForExpr(it) => unlabeled(it.syntax()),
        ast::Expr::BlockExpr(it) => it.modifier().is_none(),
        _ => false,
    };
    if !confusable {
        return None;
    }
    let value_range = value.syntax().text_range();
    let span = span(cx, tree, TextRange::new(break_.text_range().start(), value_range.end()));
    let left = span_at(cx, tree, value_range.start());
    let right = span_at(cx, tree, value_range.end());
    let mut diag = Diag::lint(
        "break_with_label_and_loop",
        "this labeled break expression is easy to confuse with an unlabeled break with a labeled value expression",
        span,
    );
    diag.multipart_suggestion(
        "wrap this expression in parentheses",
        // alloc: the lint's text, for a lint found
        vec![(left, "(".to_owned()), (right, ")".to_owned())],
        Applicability::MachineApplicable,
    );
    Some(emitted(cx, diag, Pass::Buffered, crate_at, tree.ptr(node)).buffered(stage(tree)))
}

fn span_at(cx: &Cx<'_, '_>, tree: &Tree<'_>, offset: TextSize) -> Span {
    span(cx, tree, TextRange::empty(offset))
}

/// When the parser buffers a lint of `tree`: a file as the crate is parsed, what a macro
/// expands to as expansion parses it.
fn stage(tree: &Tree<'_>) -> Stage {
    match tree.call {
        None => Stage::Parsed,
        Some(_) => Stage::Expanded,
    }
}
