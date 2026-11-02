//! `multiple_unsafe_ops_per_block`: an `unsafe` block holding more than one unsafe operation,
//! so that one safety comment cannot speak for each.
//!
//! Follows clippy's `clippy_lints/src/multiple_unsafe_ops_per_block.rs` (`UnsafeExprCollector`)
//! (MIT or Apache-2.0), ported from what is known of its source at rust-lang/rust's nightly of
//! 2026-10: no reference checkout of clippy was at hand. As its visitor does, the walk goes into
//! closures, nested `unsafe` blocks, `const` blocks and what macro calls expand to, not into
//! nested items. `.await` is not desugared here, so only what is awaited is looked at, as clippy
//! does. An operation a macro made up is noted at the call it came from.

use hir::{PathResolution, Safety};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, BinaryOp, UnaryOp},
};

use crate::check::Cx;

const LINT: super::Implemented = super::Implemented::MultipleUnsafeOpsPerBlock;

/// How many macro calls deep the walk goes into expansions.
const DEEPEST: u32 = 8;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(block) = ast::BlockExpr::cast(node) else { return };
    let Some(ast::BlockModifier::Unsafe(_)) = block.modifier() else { return };
    if !cx.is_written(node) {
        return;
    }
    let Some(range) = super::undocumented_unsafe_blocks::block_range(cx, block) else { return };
    let ops = {
        let mut collector =
            Collector { cx: &*cx, ops: Vec::new(), call: None, block: range, depth: 0 };
        if let Some(list) = block.stmt_list() {
            collector.visit(list.syntax());
        }
        collector.ops
    };
    if ops.len() <= 1 {
        return;
    }
    let message =
        format!("this `unsafe` block contains {} unsafe operations, expected only one", ops.len());
    let span = cx.span(range);
    let notes: Vec<_> = ops.into_iter().map(|(note, at)| (note, cx.span(at))).collect();
    let diagnostic = cx.lint(LINT, node, span, message);
    for (note, at) in notes {
        diagnostic.span_note(at, note);
    }
}

/// clippy's `UnsafeExprCollector`: each unsafe operation, in the order its visitor meets them.
struct Collector<'c, 'q, 't> {
    cx: &'c Cx<'q, 't>,
    ops: Vec<(&'static str, TextRange)>,
    /// The outermost macro call written in the block whose expansion the walk is in.
    call: Option<TextRange>,
    block: TextRange,
    depth: u32,
}

impl<'t> Collector<'_, '_, 't> {
    fn visit(&mut self, node: ast::Node<'t>) {
        if let Some(call) = ast::MacroCall::cast(node) {
            self.visit_call(call);
            return;
        }
        // A nested item is not of this body.
        if ast::Item::can_cast(node.kind()) {
            return;
        }
        if let Some(expr) = ast::Expr::cast(node)
            && let Some(next) = self.expr(expr)
        {
            if let Some(next) = next {
                self.visit(next);
            }
            return;
        }
        for child in node.children() {
            self.visit(child);
        }
    }

    fn visit_call(&mut self, call: ast::MacroCall<'t>) {
        if self.depth >= DEEPEST {
            return;
        }
        let Some(expansion) = self.cx.expand(call) else { return };
        let outer = self.call;
        if outer.is_none() {
            self.call = self.cx.range(call.syntax());
        }
        self.depth += 1;
        self.visit(expansion);
        self.depth -= 1;
        self.call = outer;
    }

    /// Where `node` is reported: where it is written, else at the macro call that made it.
    fn place(&self, node: ast::Node<'t>) -> TextRange {
        self.cx.range(node).or(self.call).unwrap_or(self.block)
    }

    /// clippy's `visit_expr` of `expr`: `None` to walk its children next, `Some(next)` to walk
    /// only `next` (nothing when it is `None`).
    fn expr(&mut self, expr: ast::Expr<'t>) -> Option<Option<ast::Node<'t>>> {
        let cx = self.cx;
        let db = cx.db();
        match expr {
            ast::Expr::AsmExpr(_) => self.push("inline assembly used here", expr),
            // `&raw const u.field` reads no field: what the place is built from is walked.
            ast::Expr::RefExpr(reference) if reference.raw_token().is_some() => {
                let mut inner = reference.expr()?.peel_parens();
                while let ast::Expr::FieldExpr(field) = inner
                    && let Some(prefix) = field.expr()
                    && cx.adjustments(prefix).is_none_or(|it| it.is_empty())
                {
                    inner = prefix.peel_parens();
                }
                return Some(Some(inner.syntax()));
            }
            ast::Expr::FieldExpr(field) => {
                let union = field
                    .expr()
                    .and_then(|it| cx.ty(it))
                    .is_some_and(|it| matches!(it.as_adt(), Some(hir::Adt::Union(_))));
                if union {
                    self.push("union field access occurs here", expr);
                }
            }
            ast::Expr::PathExpr(_) if is_static_mut(cx, expr) => {
                self.push("access of a mutable static occurs here", expr);
            }
            ast::Expr::PrefixExpr(prefix) if prefix.op_kind() == Some(UnaryOp::Deref) => {
                if prefix.expr().and_then(|it| cx.ty(it)).is_some_and(|it| it.is_raw_ptr()) {
                    self.push("raw pointer dereference occurs here", expr);
                }
            }
            ast::Expr::CallExpr(call) => {
                let unsafe_callee = call
                    .expr()
                    .and_then(|it| cx.adjusted_ty(it))
                    .and_then(|it| it.as_callable(db))
                    .is_some_and(|it| it.safety() == Safety::Unsafe);
                if unsafe_callee {
                    self.push("unsafe function call occurs here", expr);
                }
            }
            ast::Expr::MethodCallExpr(call) => {
                let unsafe_method = cx
                    .method(call)
                    .and_then(|it| it.signature(db))
                    .is_some_and(|it| it.safety() == Safety::Unsafe);
                if unsafe_method {
                    self.push("unsafe method call occurs here", expr);
                }
            }
            ast::Expr::BinExpr(bin) if matches!(bin.op_kind(), Some(BinaryOp::Assignment { .. })) => {
                if bin.lhs().is_some_and(|it| is_static_mut(cx, it.peel_parens())) {
                    self.push("modification of a mutable static occurs here", expr);
                    return Some(bin.rhs().map(|it| it.syntax()));
                }
            }
            _ => {}
        }
        None
    }

    fn push(&mut self, note: &'static str, expr: ast::Expr<'t>) {
        let at = self.place(expr.syntax());
        self.ops.push((note, at));
    }
}

/// `expr` is a path naming a `static mut` (`DefKind::Static { mutability: Mut }`).
fn is_static_mut(cx: &Cx<'_, '_>, expr: ast::Expr<'_>) -> bool {
    let ast::Expr::PathExpr(path) = expr else { return false };
    let Some(path) = path.path() else { return false };
    matches!(cx.path(path), Some(PathResolution::Def(hir::ModuleDef::Static(it))) if it.is_mut(cx.db()))
}
