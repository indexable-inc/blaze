//! `front_vec_ops`: `Vec::remove(0)`, `Vec::insert(0, x)` or `Vec::drain(..1)` inside a loop.
//! Each one shifts every other element by one place, so a loop of them is quadratic in the
//! `Vec`'s length; a `VecDeque` pops and pushes at either end in constant time. A `Vec` also
//! pushed to in the same loop is a queue, and the message says so.
//!
//! Found: the call resolves to `Vec`'s own `remove`, `insert` or `drain` (the diagnostic item
//! `Vec`), its index is the literal `0` (`drain`'s range `..1` or `0..1`), and it runs once per
//! iteration of a `loop`, `while`, `for` or iterator closure. A call outside a loop is one shift
//! and is left alone. No fix: the type change reaches every use of the `Vec`.

use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{Expr, ExprId},
    signatures::FunctionSignature,
};

use super::{
    Finding, Lint, alloc::written_callee, exempt, facts, hash_order_output::local_of, loops,
};
use crate::check::Cx;

const LINT: Lint = Lint::FrontVecOps;

/// How many expressions up a `push` is followed to the loop of the front operation.
const UP: usize = 64;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// Whether `range` is `..1` or `0..1`.
fn first_only(body: &Body, range: ExprId) -> bool {
    let Expr::RecordLit { fields, .. } = body.expr(range) else { return false };
    let field = |name: &str| fields.iter().find(|it| it.name.as_str() == name).map(|it| it.expr);
    let start = field("start").is_none_or(|it| loops::is_int(body, it, 0));
    let end = field("end").is_some_and(|it| loops::is_int(body, it, 1));
    start && end && fields.len() <= 2
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let Some(vec) = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Vec")) else {
        return found;
    };
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let Expr::MethodCall { receiver, args, .. } = expr else { continue };
        let Some((callee, _)) = written_callee(infer, body, id) else { continue };
        let name = FunctionSignature::of(db, callee).name.as_str();
        let front = match (name, args) {
            ("remove", [at]) => loops::is_int(body, *at, 0),
            ("insert", [at, _]) => loops::is_int(body, *at, 0),
            ("drain", [range]) => first_only(body, *range),
            _ => false,
        };
        if !front || facts::impl_adt(db, callee) != Some(vec) {
            continue;
        }
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        let Some(loop_) = loops::enclosing_loop(cx, analysis, parents, id) else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let local = local_of(db, def, body, peel(body, receiver));
        let pushed = local.is_some_and(|local| {
            body.exprs().any(|(other, expr)| {
                let Expr::MethodCall { receiver, .. } = expr else { return false };
                written_callee(infer, body, other)
                    .is_some_and(|(f, _)| FunctionSignature::of(db, f).name.as_str() == "push")
                    && local_of(db, def, body, peel(body, receiver)) == Some(local)
                    && parents.up(other, UP).any(|it| it == loop_)
            })
        });
        let op = match name {
            "remove" => "`remove(0)`",
            "insert" => "`insert(0, ..)`",
            _ => "`drain(..1)`",
        };
        let queue = match pushed {
            true => ", and the loop also pushes to it: it is used as a queue",
            false => "",
        };
        let message =
            format!("{op} on a `Vec` inside a loop shifts every other element each time{queue}");
        let note = "a loop of these is quadratic in the length; `std::collections::VecDeque` takes from and adds at \
                    either end in constant time (`pop_front`, `push_front`, `push_back`)"
            .to_owned(); // alloc: a message's part
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// `expr` with its borrows and derefs peeled.
fn peel(body: &Body, expr: ExprId) -> ExprId {
    let mut at = expr;
    for _ in 0..8 {
        match body.expr(at) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: hir_def::hir::UnaryOp::Deref } => {
                at = expr
            }
            _ => break,
        }
    }
    at
}
