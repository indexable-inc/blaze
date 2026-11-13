//! `truncating_cast` (internal, allowed by default, the workspace's ratchet included): each
//! call that keeps the low bits of an integer, or its bits read in the other sign, where the
//! value may not fit: core's `truncate`, `cast_signed` and `cast_unsigned`, and blaze-cast's
//! `*_truncated_from_*`. These are what `blaze::as_cast`'s fix writes for an `as` that wraps,
//! so the behaviour stays as it was; this lint lists them as the places to decide, one by one,
//! whether wrapping is meant (a hash, a bit pattern, a value a check just above bounds) or a
//! value that doesn't fit is a bug, for which the change is `T::try_from(x)` and a typed error
//! naming the thing (`narrowed_len` refuses a `try_from(..).unwrap()`).
//!
//! There is no fix: which of the two a site is, only its reader knows. A call a macro writes is
//! not listed.

use hir::AsAssocItem as _;
use hir_def::DefWithBodyId;
use hir_def::hir::Expr;

use super::{Finding, Lint, alloc, facts};
use crate::check::Cx;

const LINT: Lint = Lint::TruncatingCast;

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
    if DefWithBodyId::try_from(analysis.def()).is_err() {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let file = analysis.tree().1;
    let item = super::body_item(db, analysis);
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::MethodCall { .. } | Expr::Call { .. }) {
            continue;
        }
        let Some((callee, _)) = alloc::written_callee(infer, body, id) else { continue };
        let function = hir::Function::from(callee);
        let name = function.name(db);
        let name = name.as_str();
        let krate = function.module(db).krate(db);
        let what = if facts::crate_is(db, krate, "core") {
            let on_integer = function
                .as_assoc_item(db)
                .and_then(|it| it.implementing_ty(db))
                .and_then(|it| it.as_builtin())
                .is_some_and(|it| it.is_int() || it.is_uint());
            match (on_integer, name) {
                (true, "truncate") => "drops the high bits of a value that doesn't fit",
                (true, "cast_signed") => "turns a value above the signed type's `MAX` negative",
                (true, "cast_unsigned") => "turns a negative value into a large positive one",
                _ => continue,
            }
        } else if facts::crate_is(db, krate, "blaze_cast") && name.contains("_truncated_from_") {
            "drops the high bits of a value that doesn't fit, on a target where the pointer is narrower"
        } else {
            continue;
        };
        if !analysis.source_map().expr_syntax(id).is_ok_and(|it| it.file_id == file) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        let message = format!("this `{name}` in {item} {what}");
        let note = "keep it where wrapping is meant (a hash, a bit pattern, a value bounded just above) and say \
                    so with `#[expect(blaze::truncating_cast, reason = \"..\")]`; where a value that doesn't fit \
                    is a bug, convert with `T::try_from(x)` and return a typed error naming the thing";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned()));
    }
    found
}
