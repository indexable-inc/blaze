//! `sentinel_value`: one value of a number's range stands in for "none": `pos == -1`,
//! `slot != u32::MAX`, `node_id == 0`. Every reader must know the convention and test for it
//! before using the number, and a reader that forgets does arithmetic on the sentinel. The type
//! is `Option<T>`: "none" is a variant the compiler makes every reader decide, and with a niche
//! (`Option<NonZero<u32>>`, a `NonMax` newtype) it costs no space.
//!
//! A finding is an `==` or `!=`, written in the body's own file, of an integer (by its inferred
//! type) with one of: the literal `-1`; std's `MAX` of that integer type (resolved, not read);
//! or `0`, where the other side is a local or field whose name ends in `id`, `idx` or `index`.
//! Not looked at: test code, and what a macro wrote.

use hir_def::{
    DefWithBodyId,
    hir::{BinaryOp, CmpOp, Expr, ExprId, Literal, UnaryOp},
};
use hir_ty::method_resolution::CandidateId;

use super::{Found, report, written};
use crate::{
    blaze::{Lint, general_facts, per_item},
    check::Cx,
};

const LINT: Lint = Lint::SentinelValue;

/// The last words of a name that make `0` a sentinel for it.
const IDENTITY: &[&str] = &["id", "idx", "index"];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    report(cx, found);
}

/// Which sentinel a side of a comparison is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sentinel {
    MinusOne,
    Max,
    Zero,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        let Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(CmpOp::Eq { .. })) } = expr else {
            continue;
        };
        if !written(analysis, id) {
            continue;
        }
        let ty = analysis.ty(infer.expr_ty(lhs)).strip_references();
        if !ty.is_int_or_uint() {
            continue;
        }
        let (value, sentinel) = match (sentinel_of(cx, analysis, rhs), sentinel_of(cx, analysis, lhs)) {
            (Some(it), _) => (lhs, it),
            (None, Some(it)) => (rhs, it),
            (None, None) => continue,
        };
        let name = name_of(db, def, body, value);
        if sentinel == Sentinel::Zero
            && !name.as_deref().is_some_and(|it| {
                it.rsplit('_').next().is_some_and(|last| IDENTITY.iter().any(|w| last.eq_ignore_ascii_case(w)))
            })
        {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let shown = name.map_or_else(|| "a number".to_owned(), |it| format!("`{it}`")); // alloc: a message's part
        let (said, carry) = match sentinel {
            Sentinel::MinusOne => ("`-1`", "`Option<T>` over the unsigned value (`Option<u32>`), so \"none\" is a variant and the value can't be negative"),
            Sentinel::Max => ("`MAX`", "`Option<T>`, or a `NonMax` newtype whose `Option` keeps the size"),
            Sentinel::Zero => ("`0`", "`Option<NonZero<T>>`, the same size as `T`, with \"none\" a variant"),
        };
        found.push(Found {
            lint: LINT,
            range,
            what: format!("{shown} is compared with {said}, a value of its range standing in for \"none\""), // alloc: a message
            carry: carry.to_owned(), // alloc: a message's part
            removes: "the convention every reader must know: a reader that forgets the test can't use the sentinel as a number",
            fix: None,
        });
    }
    found
}

/// The sentinel `expr` is, if it is one.
fn sentinel_of(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, expr: ExprId) -> Option<Sentinel> {
    let db = cx.db();
    let body = analysis.body();
    match body.expr(expr) {
        Expr::UnaryOp { expr: inner, op: UnaryOp::Neg } => match body.expr(inner) {
            Expr::Literal(Literal::Int(1, _) | Literal::Uint(1, _)) => Some(Sentinel::MinusOne),
            _ => None,
        },
        Expr::Literal(Literal::Int(0, _) | Literal::Uint(0, _)) => Some(Sentinel::Zero),
        Expr::Path(_) => {
            let Some((CandidateId::ConstId(c), _)) = analysis.infer().assoc_resolutions_for_expr(expr)
            else {
                return None;
            };
            let c = hir::Const::from(c);
            let max = c.name(db).is_some_and(|it| it.as_str() == "MAX");
            (max && general_facts::std_top(db, c.module(db)).is_some()).then_some(Sentinel::Max)
        }
        _ => None,
    }
}

/// The name of the local or field `expr` is, through borrows and derefs.
fn name_of(
    db: &base_db::Db,
    def: DefWithBodyId,
    body: &hir_def::expr_store::Body,
    expr: ExprId,
) -> Option<String> {
    match body.expr(expr) {
        Expr::Path(_) => Some(body[per_item::local_of(db, def, body, expr)?].name.as_str().to_owned()), // alloc: a message's part
        Expr::Field { name, .. } => Some(name.as_str().to_owned()), // alloc: a message's part
        Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => name_of(db, def, body, expr),
        _ => None,
    }
}
