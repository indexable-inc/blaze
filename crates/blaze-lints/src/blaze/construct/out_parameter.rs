//! `out_parameter`: a `&mut` parameter the function only ever writes through (`*out = v`),
//! beside a `bool` or `()` return. The parameter is a second return value kept by convention:
//! the caller makes a placeholder, the function may return without writing it, and the `bool`
//! says (by convention) whether it did. The return type carries it: `Option<T>` when it may not
//! be written, `T` when it always is, `Result<T, E>` when the `bool` was an error.
//!
//! A finding is a parameter of a function (not a trait impl's method, whose signature is the
//! trait's) whose type is `&mut T`, bound by a plain name, every use of which in the body is
//! the place of an assignment `*name = ..`, at least one, where the function returns `bool` or
//! `()`. Not looked at: test code, and what a macro wrote.

use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId,
    hir::{Expr, Pat, UnaryOp},
};

use super::{Found, report};
use crate::{
    blaze::{Lint, per_item},
    check::Cx,
};

const LINT: Lint = Lint::OutParameter;

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
    let func = hir::Function::from(f);
    if func.as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some()) {
        return found;
    }
    let ret = func.ret_type(db);
    let returns = if ret.is_bool() {
        "`bool`"
    } else if ret.is_unit() {
        "`()`"
    } else {
        return found;
    };
    let body = analysis.body();
    let infer = analysis.infer();
    let file = analysis.tree().1;
    for param in body.params.iter() {
        let Pat::Bind { id: local, subpat: None } = body.pat(param.formal) else { continue };
        if !analysis.ty(infer.binding_ty(local)).is_mutable_reference() {
            continue;
        }
        let mut uses = 0u32;
        let mut writes = 0u32;
        for (id, expr) in body.exprs() {
            match expr {
                Expr::Path(_) if per_item::local_of(db, def, body, id) == Some(local) => uses += 1,
                Expr::Assignment { target, .. } => {
                    let Pat::Expr(place) = body.pat(target) else { continue };
                    let Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } = body.expr(place) else {
                        continue;
                    };
                    if per_item::local_of(db, def, body, inner) == Some(local) {
                        writes += 1;
                    }
                }
                _ => {}
            }
        }
        if writes == 0 || uses != writes {
            continue;
        }
        let Ok(source) = analysis.source_map().pat_syntax(param.formal) else { continue };
        if source.file_id != file {
            continue;
        }
        let range = source.value.syntax_node_ptr().text_range();
        let name = body[local].name.as_str();
        found.push(Found {
            lint: LINT,
            range,
            what: format!(
                "`{name}` is only written through (`*{name} = ..`), never read: a second return value beside the {returns} the function returns"
            ), // alloc: a message
            carry: format!(
                "the return type: `Option<T>` when `{name}` may be left unwritten, `T` when it is always written, `Result<T, E>` when the {returns} reports a failure"
            ), // alloc: a message's part
            removes: "the caller's placeholder and the convention tying the flag to whether the value was written",
            fix: None,
        });
    }
    found
}
