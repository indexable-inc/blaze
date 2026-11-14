//! `proof_forgotten`: a check decided a value (`x.is_some()`, `r.is_ok()`, `i < v.len()`), and
//! then the code asks again, at run time, with an `unwrap`, an `expect` or an index that
//! `hir_ty_pure::reach` proves cannot fail there. The check's answer was a proof, and it was
//! dropped: the second ask is a panic site the reader must prove away by hand, and an edit
//! that moves either one apart makes it a real panic.
//!
//! A finding is a site that is (a) an `unwrap` or `expect` of std's `Option` or `Result`, or an
//! index, written in the body's own file, (b) discharged by the body's proof
//! ([`hir_ty_pure::reach::Reach::discharged`]), and (c) of a local that an `if` condition or a
//! `match` scrutinee written before it in the same body reads: the check that proved it. A site
//! discharged by what it is (`Some(3).unwrap()`) has no such check and is not this lint's.
//!
//! The fix, where the shape is the plain one (`if x.is_some() { .. x.unwrap() .. }`, or
//! `is_ok`, with `x` a local holding an owned `Option` or `Result` and every site in the
//! `then` branch), rewrites the condition to `let Some(x) = x` (`let Ok(x) = x`) and each site
//! to `x`. It moves `x` into the branch, so it is `MaybeIncorrect`: `x` used after the `if`
//! needs `ref` or a borrow (`let Some(x) = &x`).

use hir_def::{
    DefWithBodyId,
    hir::{BindingId, Expr, ExprId},
};

use super::{Found, Fix, report, written};
use crate::{
    blaze::{Lint, general_facts, per_item},
    check::Cx,
    diag::Applicability,
};

const LINT: Lint = Lint::ProofForgotten;

/// How many parents of a condition's local are looked through for the check.
const CHECK_DEPTH: usize = 16;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    report(cx, found);
}

/// One site that asks again: the expression, the local it asks about, and what it is.
struct Site {
    expr: ExprId,
    local: BindingId,
    /// `Some(true)` for an `Option`'s `unwrap`/`expect`, `Some(false)` for a `Result`'s, `None`
    /// for an index.
    option: Option<bool>,
    /// The site's receiver, for an `unwrap` or `expect`.
    receiver: Option<ExprId>,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let body = analysis.body();
    // alloc: the few candidate sites of one body
    let mut sites = Vec::new();
    for (id, expr) in body.exprs() {
        let site = match expr {
            Expr::MethodCall { receiver, .. } => {
                if !per_item::written_call(analysis, id) {
                    continue;
                }
                let Some(callee) = general_facts::callee(db, analysis, id) else { continue };
                if !matches!(callee.name, "unwrap" | "expect") {
                    continue;
                }
                let Some(adt) = callee.self_adt else { continue };
                let option = if general_facts::std_adt(db, adt, "option", &["Option"]) {
                    true
                } else if general_facts::std_adt(db, adt, "result", &["Result"]) {
                    false
                } else {
                    continue;
                };
                let Some(local) = per_item::local_of(db, def, body, receiver) else { continue };
                Site { expr: id, local, option: Some(option), receiver: Some(receiver) }
            }
            Expr::Index { index, .. } => {
                if !written(analysis, id) {
                    continue;
                }
                let Some(local) = per_item::local_of(db, def, body, index) else { continue };
                Site { expr: id, local, option: None, receiver: None }
            }
            _ => continue,
        };
        sites.push(site);
    }
    if sites.is_empty() {
        return found;
    }
    let reach = hir_ty_pure::reach::of_body(db, def);
    if reach.refused.is_some() {
        return found;
    }
    sites.retain(|site| reach.discharged(site.expr));
    if sites.is_empty() {
        return found;
    }
    // The checks: each `if` condition and `match` scrutinee written in the file, with the
    // locals it reads.
    // alloc: the few checks of one body
    let mut checks = Vec::new();
    for (id, expr) in body.exprs() {
        let (scrutinee, then) = match expr {
            Expr::If { condition, then_branch, .. } => (condition, Some(then_branch)),
            Expr::Match { expr, .. } => (expr, None),
            _ => continue,
        };
        if !written(analysis, id) {
            continue;
        }
        let Some(range) = analysis.expr_range(scrutinee) else { continue };
        // alloc: the locals one check reads
        let mut reads = Vec::new();
        collect_locals(db, def, body, scrutinee, &mut reads, CHECK_DEPTH);
        checks.push(Check { scrutinee, range, then, reads });
    }
    // One fix per `if`: the sites under its `then` branch.
    // alloc: the fixes of one body, few
    let mut fixed: Vec<ExprId> = Vec::new();
    for site in &sites {
        let Some(at) = analysis.expr_range(site.expr) else { continue };
        let Some(check) = checks
            .iter()
            .filter(|it| it.range.end() <= at.start() && it.reads.contains(&site.local))
            .max_by_key(|it| it.range.start())
        else {
            continue;
        };
        let name = body[site.local].name.as_str();
        let (what, carry) = match site.option {
            Some(true) => (
                format!("`{name}` was checked to be `Some` above, and this asks again with a panic blaze proves never happens"), // alloc: a message
                format!("the `{name}` the check hands out: `if let Some({name}) = {name}` or `let Some({name}) = {name} else {{ .. }}`"), // alloc: a message's part
            ),
            Some(false) => (
                format!("`{name}` was checked to be `Ok` above, and this asks again with a panic blaze proves never happens"), // alloc: a message
                format!("the value the check hands out: `if let Ok({name}) = {name}` or `let Ok({name}) = {name} else {{ .. }}`"), // alloc: a message's part
            ),
            None => (
                format!("`{name}` was checked against the length above, and this index checks it again with a panic blaze proves never happens"), // alloc: a message
                format!("the element the check hands out: `if let Some(item) = v.get({name})`, or an index type that is in range by construction (`Int<0, N>`)"), // alloc: a message's part
            ),
        };
        let fix = match (site.option, check.then) {
            (Some(option), Some(then)) if !fixed.contains(&check.scrutinee) => {
                fix_of(cx, analysis, check, then, option, site.local, &sites)
            }
            _ => None,
        };
        if fix.is_some() {
            fixed.push(check.scrutinee);
        }
        found.push(Found {
            lint: LINT,
            range: at,
            what,
            carry,
            removes: "the second check and its panic site: the value can't be reached without the proof",
            fix,
        });
    }
    found
}

/// A check: an `if` condition or a `match` scrutinee, where it is written, the `then` branch
/// of an `if`, and the locals it reads.
struct Check {
    scrutinee: ExprId,
    range: syntax::TextRange,
    then: Option<ExprId>,
    reads: Vec<BindingId>,
}

/// The locals `expr` reads, at most `depth` levels down.
fn collect_locals(
    db: &base_db::Db,
    def: DefWithBodyId,
    body: &hir_def::expr_store::Body,
    expr: ExprId,
    into: &mut Vec<BindingId>,
    depth: usize,
) {
    if let Some(local) = per_item::local_of(db, def, body, expr) {
        if !into.contains(&local) {
            into.push(local);
        }
        return;
    }
    if depth == 0 {
        return;
    }
    body.walk_child_exprs(expr, |child| collect_locals(db, def, body, child, into, depth - 1));
}

/// The fix of the plain shape: the condition is `x.is_some()` (`x.is_ok()`) of a local `x` of
/// an owned `Option` (`Result`), and every site of `x` is an `unwrap` or `expect` in `then`.
fn fix_of(
    cx: &Cx<'_, '_>,
    analysis: &hir::BodyAnalysis<'_>,
    check: &Check,
    then: ExprId,
    option: bool,
    local: BindingId,
    sites: &[Site],
) -> Option<Fix> {
    let db = cx.db();
    let body = analysis.body();
    let def = DefWithBodyId::try_from(analysis.def()).ok()?;
    let Expr::MethodCall { receiver, method_name, args, .. } = body.expr(check.scrutinee) else {
        return None;
    };
    let asks = if option { "is_some" } else { "is_ok" };
    if method_name.as_str() != asks || !args.is_empty() {
        return None;
    }
    if per_item::local_of(db, def, body, receiver) != Some(local) {
        return None;
    }
    let ty = analysis.ty(analysis.infer().expr_ty(receiver));
    if ty.is_reference() {
        return None;
    }
    let then_range = analysis.expr_range(then)?;
    let name = body[local].name.as_str();
    let pattern = if option { "Some" } else { "Ok" };
    // alloc: one fix's parts
    let mut parts = vec![(check.range, format!("let {pattern}({name}) = {name}"))];
    for site in sites.iter().filter(|it| it.local == local && it.option == Some(option)) {
        let at = analysis.expr_range(site.expr)?;
        if !then_range.contains_range(at) {
            return None;
        }
        site.receiver?;
        parts.push((at, name.to_owned())); // alloc: a fix's part
    }
    Some(Fix {
        message: "bind the value the check proves, and use it",
        parts,
        applicability: Applicability::MaybeIncorrect,
    })
}
