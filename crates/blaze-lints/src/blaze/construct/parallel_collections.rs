//! `parallel_collections`: two `Vec` fields of one value kept the same length by convention:
//! pushed one right after the other (`self.names.push(..); self.ages.push(..);`), or read at
//! the same index (`self.names[i]`, `self.ages[i]`). Nothing but the code at each push keeps
//! them in step; a push or a `remove` that misses one shifts every later element of the other.
//! One `Vec` of a struct holds each element's parts together, so there is one length.
//! (Where the split is for speed, a struct-of-arrays type that owns both `Vec`s and has one
//! `push` keeps the layout and still makes the step impossible to miss.)
//!
//! A finding is a pair of distinct fields of the same local (`self` included), each a std
//! `Vec` by its inferred type, either pushed in two consecutive statements of one block or
//! indexed by the same local in one body. One finding per pair, at the second use. Not looked
//! at: test code, and what a macro wrote.

use hir_def::{
    DefWithBodyId,
    hir::{BindingId, Expr, ExprId, Statement},
};

use super::{Found, report, written};
use crate::{
    blaze::{Lint, general_facts, per_item},
    check::Cx,
};

const LINT: Lint = Lint::ParallelCollections;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    report(cx, found);
}

/// A field of a local that is a `Vec`: the local, and the field's name.
type Place = (BindingId, String);

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let body = analysis.body();
    // The `Vec` field `expr` is, of a local.
    let vec_field = |expr: ExprId| -> Option<Place> {
        let Expr::Field { expr: base, name } = body.expr(expr) else { return None };
        let ty = analysis.ty(analysis.infer().expr_ty(expr)).strip_references();
        let adt = ty.as_adt()?;
        if !general_facts::std_adt(db, adt, "vec", &["Vec"]) {
            return None;
        }
        let local = per_item::local_of(db, def, body, base)?;
        Some((local, name.as_str().to_owned())) // alloc: a field's name
    };
    // alloc: the pairs found, few
    let mut pairs: Vec<(Place, Place)> = Vec::new();
    let mut push = |found: &mut Vec<Found>, first: Place, second: Place, at: ExprId, how: &str| {
        if first.0 != second.0 || first.1 == second.1 {
            return;
        }
        let key = if first.1 < second.1 { (first.clone(), second.clone()) } else { (second.clone(), first.clone()) }; // clone: a pair's key, two short names
        if pairs.contains(&key) {
            return;
        }
        let Some(range) = analysis.expr_range(at) else { return };
        pairs.push(key);
        let owner = body[first.0].name.as_str();
        found.push(Found {
            lint: LINT,
            range,
            what: format!(
                "`{owner}.{}` and `{owner}.{}` are {how}: two `Vec`s kept the same length by convention",
                first.1, second.1
            ), // alloc: a message
            carry: format!(
                "one `Vec` of a struct with a field for each (`{}` and `{}`), or a struct-of-arrays type with one `push`",
                first.1, second.1
            ), // alloc: a message's part
            removes: "the step between the two that each push, remove and sort must keep by hand",
            fix: None,
        });
    };
    // Pushed in two consecutive statements.
    let pushed = |expr: ExprId| -> Option<Place> {
        let Expr::MethodCall { receiver, method_name, .. } = body.expr(expr) else { return None };
        (method_name.as_str() == "push" && written(analysis, expr)).then(|| vec_field(receiver))?
    };
    for (_, expr) in body.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        let mut last: Option<Place> = None;
        for statement in statements.iter() {
            let now = match statement {
                Statement::Expr { expr, .. } => pushed(expr).map(|it| (it, expr)),
                _ => None,
            };
            if let (Some(before), Some((now, at))) = (last.take(), now.clone()) {
                push(&mut found, before, now, at, "pushed one after the other");
            }
            last = now.map(|it| it.0);
        }
    }
    // Indexed by the same local.
    // alloc: the indexed fields of one body, few
    let mut indexed: Vec<(BindingId, Place, ExprId)> = Vec::new();
    for (id, expr) in body.exprs() {
        let Expr::Index { base, index } = expr else { continue };
        if !written(analysis, id) {
            continue;
        }
        let Some(place) = vec_field(base) else { continue };
        let Some(by) = per_item::local_of(db, def, body, index) else { continue };
        for (other_by, other, _) in &indexed {
            if *other_by == by {
                push(&mut found, other.clone(), place.clone(), id, "read at the same index"); // clone: two short names
            }
        }
        indexed.push((by, place, id));
    }
    found
}
