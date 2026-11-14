//! `validated_but_raw`: a function checks its primitive parameter (`if port == 0 { return
//! Err(..) }`) and then hands the same raw value back (`Ok(port)`, `Some(port)`, `port`). The
//! check's result is thrown away: the caller holds a `u16` that is valid only because this
//! function said so, and every reader after it either trusts that by convention or checks
//! again. A newtype whose one constructor is this check (`struct Port(NonZero<u16>)`, `impl
//! TryFrom<u16> for Port`) keeps the proof with the value; with ranged integers
//! (`Int<1, 65535>`) or a `#[blaze::invariant]` the check becomes the type's.
//!
//! A finding is a function whose body starts with one or more `if` statements with no `else`,
//! each of whose branch diverges (`return`, a panic), at least one of whose conditions reads a
//! parameter `p` of a bare number or string type, and whose value is `p`, `Ok(p)` or `Some(p)`
//! (std's). Not looked at: a trait impl's method (its signature is the trait's), test code,
//! and what a macro wrote.

use hir::{AsAssocItem as _, HirDisplay as _};
use hir_def::{
    DefWithBodyId,
    hir::{BindingId, Expr, ExprId, Pat, Statement},
};

use super::{Found, report, written};
use crate::{
    blaze::{Lint, general_facts, per_item},
    check::Cx,
};

const LINT: Lint = Lint::ValidatedButRaw;

/// How deep a condition is looked through for the parameter it reads.
const CONDITION_DEPTH: usize = 8;
/// How deep a branch is looked through for whether it diverges.
const DIVERGE_DEPTH: usize = 4;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let DefWithBodyId::FunctionId(f) = def else { return found };
    if hir::Function::from(f).as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some()) {
        return found;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    // The parameters of a bare number or string type.
    // alloc: one function's parameters, few
    let raw: Vec<BindingId> = body
        .params
        .iter()
        .filter_map(|param| match body.pat(param.formal) {
            Pat::Bind { id, subpat: None } => Some(id),
            _ => None,
        })
        .filter(|&id| {
            let ty = analysis.ty(infer.binding_ty(id)).strip_references();
            ty.is_int_or_uint() || ty.is_float() || ty.is_str() || ty.as_adt().is_some_and(|it| general_facts::std_adt(db, it, "string", &["String"]))
        })
        .collect();
    if raw.is_empty() {
        return found;
    }
    let Expr::Block { statements, tail: Some(tail), .. } = body.expr(body.root_expr()) else {
        return found;
    };
    // The guards at the top of the body, and the parameters they read.
    // alloc: the parameters one body's guards read, few
    let mut guarded: Vec<BindingId> = Vec::new();
    for statement in statements.iter() {
        let Statement::Expr { expr, .. } = statement else { break };
        let Expr::If { condition, then_branch, else_branch: None } = body.expr(expr) else { break };
        if !written(analysis, expr) || !diverges(analysis, then_branch, DIVERGE_DEPTH) {
            break;
        }
        reads(db, def, body, condition, &raw, &mut guarded, CONDITION_DEPTH);
    }
    if guarded.is_empty() {
        return found;
    }
    let (value, wrapped) = match body.expr(tail) {
        Expr::Path(_) => (tail, None),
        Expr::Call { args: [arg], .. } => {
            let ty = analysis.ty(infer.expr_ty(tail));
            let Some(adt) = ty.as_adt() else { return found };
            let wrap = if general_facts::std_adt(db, adt, "result", &["Result"]) {
                "Ok"
            } else if general_facts::std_adt(db, adt, "option", &["Option"]) {
                "Some"
            } else {
                return found;
            };
            (*arg, Some(wrap))
        }
        _ => return found,
    };
    let Some(local) = per_item::local_of(db, def, body, value) else { return found };
    if !guarded.contains(&local) {
        return found;
    }
    let Some(range) = analysis.expr_range(tail) else { return found };
    let name = body[local].name.as_str();
    let ty = analysis.ty(infer.binding_ty(local));
    let shown = ty.display(db, cx.krate().to_display_target(db));
    let returned = match wrapped {
        Some(wrap) => format!("`{wrap}({name})`"), // alloc: a message's part
        None => format!("`{name}`"),               // alloc: a message's part
    };
    found.push(Found {
        lint: LINT,
        range,
        what: format!(
            "`{name}` is checked at the top of this function and then returned raw as {returned}: the caller gets a `{shown}` that is valid only by this function's word"
        ), // alloc: a message
        carry: format!(
            "a newtype whose one constructor is this check (`impl TryFrom<{shown}> for Valid`), returned in place of the `{shown}`; with ranged integers or `#[blaze::invariant]` the check is the type's"
        ), // alloc: a message's part
        removes: "the checks every later reader repeats or skips by convention: a value of the newtype was checked",
        fix: None,
    });
    found
}

/// Adds to `into` the parameters of `of` that `expr` reads, at most `depth` levels down.
fn reads(
    db: &base_db::Db,
    def: DefWithBodyId,
    body: &hir_def::expr_store::Body,
    expr: ExprId,
    of: &[BindingId],
    into: &mut Vec<BindingId>,
    depth: usize,
) {
    if let Some(local) = per_item::local_of(db, def, body, expr) {
        if of.contains(&local) && !into.contains(&local) {
            into.push(local);
        }
        return;
    }
    if depth == 0 {
        return;
    }
    body.walk_child_exprs(expr, |child| reads(db, def, body, child, of, into, depth - 1));
}

/// Whether `expr` never finishes normally: it has the type `!`, is a `return`, or is a block
/// whose tail or last statement diverges.
fn diverges(analysis: &hir::BodyAnalysis<'_>, expr: ExprId, depth: usize) -> bool {
    if analysis.ty(analysis.infer().expr_ty(expr)).is_never() {
        return true;
    }
    let body = analysis.body();
    match body.expr(expr) {
        Expr::Return { .. } => true,
        Expr::Block { statements, tail, .. } if depth > 0 => match tail {
            Some(tail) => diverges(analysis, tail, depth - 1),
            None => match statements.last() {
                Some(Statement::Expr { expr, .. }) => diverges(analysis, expr, depth - 1),
                _ => false,
            },
        },
        _ => false,
    }
}
