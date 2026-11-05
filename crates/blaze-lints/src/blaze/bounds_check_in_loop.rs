//! `bounds_check_in_loop`: a `for i in a..n` loop whose body indexes a slice or `Vec` by the
//! loop variable (`v[i]`, `v[i + k]`) when nothing tells the optimizer that `n` fits in `v`.
//! LLVM removes the check of `v[i]` only when it can prove `i < v.len()` once for the whole
//! loop: the bound is `v.len()`, or an `assert!` or a reslice before the loop says so. Otherwise
//! every iteration compares and branches, and the check also keeps the loop from vectorizing.
//!
//! Found: the loop's iterable is a `Range` (`a..n`, not `..=`), its pattern a plain binding,
//! and an index expression in its body is that binding, or that binding plus a literal or a
//! local, into a place (a local or a field of one) whose type is a slice, a reference to one or
//! a `Vec`. Left alone, as the check is already gone or proven once:
//!
//! - the bound mentions `v.len()` (`0..v.len()`, `0..v.len() - 1`, `0..n.min(v.len())`);
//! - a statement of the function before the loop mentions both `v.len()` and the bound (`assert!(n
//!   <= v.len())`, `let n = v.len();`, `if n > v.len() { return }`); a `debug_assert!` does not
//!   count, as release builds drop it;
//! - `v` is resliced before the loop (`let v = &v[..n];`);
//! - an array (its length is a constant LLVM sees), an index a macro wrote, and a loop over
//!   `.enumerate()` or `zip` (no range, no index).
//!
//! The statement and reslice checks read the function's text, so they suppress, never find. No
//! fix: which of the reslice, the `assert!` or an iterator fits is the author's choice.

use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{ArithOp, BinaryOp, BindingAnnotation, BindingId, Expr, ExprId, LoopSource, Pat},
};

use syntax::TextRange;

use super::{Finding, Lint, exempt, facts, hash_order_output::local_of, loops};
use crate::check::Cx;

const LINT: Lint = Lint::BoundsCheckInLoop;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let text = cx.text();
    // alloc: the places of each loop already reported, a few
    let mut seen: Vec<(ExprId, String)> = Vec::new();
    scan(cx, analysis, |it| {
        if it.proven || seen.iter().any(|(at, place)| *at == it.loop_ && place == it.place) {
            return;
        }
        if exempt::exempt(text, it.range, LINT) {
            return;
        }
        seen.push((it.loop_, it.place.to_owned())); // alloc: as above
        let (place, bound) = (it.place, it.bound);
        let len = format!("{place}.len()");
        let message = format!(
            "`{place}` is indexed by the loop variable of a range ending at `{bound}`, which is not `{len}`: the \
             bounds check stays in every iteration"
        );
        let note = format!(
            "LLVM drops the check only when it can prove every index in bounds once, before the loop: reslice \
             (`let {place} = &{place}[..{bound}];`), or `assert!({bound} <= {len});`, or iterate \
             (`{place}.iter()`, `.zip(..)`) instead of indexing"
        );
        facts::push(&mut found, LINT, it.range, message, Some(note));
    });
    found
}

/// An index of a range loop's body by its variable, as the module docs find it.
pub(super) struct Indexed<'a> {
    /// The `v[i]` expression.
    pub(super) at: ExprId,
    pub(super) range: TextRange,
    /// The loop it is in.
    pub(super) loop_: ExprId,
    /// The indexed place's text (`v`, `self.v`).
    pub(super) place: &'a str,
    /// The range's bound's text.
    pub(super) bound: &'a str,
    /// The bound is `v.len()`, or the text before the loop proves it (the docs' list): LLVM
    /// drops the check. `blaze::checkfree` reads this too.
    pub(super) proven: bool,
}

/// Each index the module docs find in the body of `analysis`, written in the file, in order.
pub(super) fn scan(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    mut f: impl FnMut(Indexed<'_>),
) {
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
    let db = cx.db();
    let body = analysis.body();
    let text = cx.text();
    let start = usize::from(cx.root().text_range().start());
    let vec = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Vec"));
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let Expr::Loop { body: inner, source: LoopSource::ForLoop, .. } = expr else { continue };
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        // The desugared `match IntoIterator::into_iter(head) { mut iter => loop { .. } }`.
        let Some(for_) = parents.of_expr(id) else { continue };
        let Expr::Match { expr: iterator, .. } = body.expr(for_) else { continue };
        let Expr::Call { args: [head], .. } = body.expr(iterator) else { continue };
        let Some(end) = range_end(db, analysis, *head) else { continue };
        let Some((var, loop_body)) = loop_var(body, inner) else { continue };
        let (Some(end_range), Some(loop_range)) =
            (analysis.expr_range(end), analysis.expr_range(for_))
        else {
            continue;
        };
        let bound = cx.snippet(end_range).trim();
        let from = start.min(text.len());
        let before = &text[from..usize::from(loop_range.start()).clamp(from, text.len())];
        let mut stack = vec![loop_body]; // alloc: the expressions of one loop body, walked once
        while let Some(at) = stack.pop() {
            body.walk_child_exprs(at, |child| stack.push(child));
            let Expr::Index { base, index } = body.expr(at) else { continue };
            if !indexed_by(cx, def, body, index, var)
                || !loops::written(analysis, at)
                || !place(body, base)
            {
                continue;
            }
            let ty = analysis.ty(analysis.infer().expr_ty(base)).strip_references();
            if !(ty.is_slice() || (ty.as_adt().is_some() && ty.as_adt() == vec)) {
                continue;
            }
            let (Some(base_range), Some(range)) =
                (analysis.expr_range(base), analysis.expr_range(at))
            else {
                continue;
            };
            let place = cx.snippet(base_range).trim();
            let len = format!("{place}.len()");
            let proven = bound.contains(&len) || proven_before(before, place, &len, bound);
            f(Indexed { at, range, loop_: id, place, bound, proven });
        }
    }
}

/// The bound of `head` when it is a `start..end` of core's `Range`.
fn range_end(db: &base_db::Db, analysis: &hir::BodyAnalysis<'_>, head: ExprId) -> Option<ExprId> {
    let Expr::RecordLit { fields, .. } = analysis.body().expr(head) else { return None };
    let adt = analysis.ty(analysis.infer().expr_ty(head)).as_adt()?;
    if !facts::adt_is(db, adt, "core", "Range") {
        return None;
    }
    fields.iter().find(|it| it.name.as_str() == "end").map(|it| it.expr)
}

/// The loop's variable, a plain binding of the `Some(pat)` arm, and the loop's own body.
fn loop_var(body: &Body, inner: ExprId) -> Option<(BindingId, ExprId)> {
    let Expr::Block { tail: Some(tail), .. } = body.expr(inner) else { return None };
    let Expr::Match { arms, .. } = body.expr(tail) else { return None };
    arms.iter().find_map(|arm| {
        let Pat::TupleStruct { args: [arg], .. } = body.pat(arm.pat) else { return None };
        let Pat::Bind { id, subpat: None } = body.pat(*arg) else { return None };
        matches!(body[id].mode, BindingAnnotation::Unannotated).then_some((id, arm.expr))
    })
}

/// Whether `index` is `var`, or `var + k` / `k + var` with `k` a literal or another local.
fn indexed_by(
    cx: &Cx<'_, '_>,
    def: DefWithBodyId,
    body: &Body,
    index: ExprId,
    var: BindingId,
) -> bool {
    let db = cx.db();
    let is_var = |it: ExprId| local_of(db, def, body, it) == Some(var);
    let offset = |it: ExprId| match body.expr(it) {
        Expr::Literal(_) => true,
        Expr::Path(_) => local_of(db, def, body, it).is_some_and(|it| it != var),
        _ => false,
    };
    match body.expr(index) {
        Expr::Path(_) => is_var(index),
        Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::ArithOp(ArithOp::Add)) } => {
            (is_var(lhs) && offset(rhs)) || (is_var(rhs) && offset(lhs))
        }
        _ => false,
    }
}

/// Whether `expr` is a place the loop does not recompute: a path, or fields of one.
fn place(body: &Body, expr: ExprId) -> bool {
    let mut at = expr;
    for _ in 0..8 {
        match body.expr(at) {
            Expr::Path(_) => return true,
            Expr::Field { expr, .. } => at = expr,
            _ => return false,
        }
    }
    false
}

/// Whether the text of the function before the loop proves the bound: a reslice of `place`, or
/// a statement (not a `debug_assert!`) naming both `len` and `bound`.
fn proven_before(before: &str, place: &str, len: &str, bound: &str) -> bool {
    let reslice = format!("let {place} = &");
    if before.split(';').any(|it| it.contains(&reslice) && it.contains("[..")) {
        return true;
    }
    before
        .split([';', '{', '}'])
        .any(|it| it.contains(len) && has_word(it, bound) && !it.contains("debug_assert"))
}

/// Whether `word` is in `text` with no identifier character on either side.
fn has_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let ident = |it: char| it.is_alphanumeric() || it == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back().is_none_or(|it| !ident(it));
        let after = text[at + word.len()..].chars().next().is_none_or(|it| !ident(it));
        before && after
    })
}
