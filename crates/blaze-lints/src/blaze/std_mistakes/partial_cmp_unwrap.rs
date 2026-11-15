//! `partial_cmp_unwrap`: `a.partial_cmp(b).unwrap()` (or `.expect(..)`) on floats, most often
//! as `v.sort_by(|a, b| a.partial_cmp(b).unwrap())`. `partial_cmp` of two floats is `None` when
//! either is NaN, so the comparison panics on the first NaN in the data: a sort of measured
//! values aborts on one bad sample.
//!
//! Found: an `unwrap` or `expect` of `Option` whose receiver is a call resolved to
//! `PartialOrd::partial_cmp` (the lang item's method, or an impl's of it) on an `f32` or `f64`
//! receiver, references peeled.
//!
//! The fix rewrites the pair to `a.total_cmp(b)`: IEEE 754's total order, which never panics
//! and puts NaN after every number. It is offered, not made (`MaybeIncorrect`): it orders
//! `-0.0` before `+0.0`, which `partial_cmp` calls equal, and it gives NaN a place where the
//! code panicked, which may hide a NaN that should have been refused before the sort.

use syntax::view::{self as ast, AstNode as _, HasArgList as _};

use super::{Fix, in_test, receiver_text, report, strip, unwrap_of};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::PartialCmpUnwrap;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(unwrap) = ast::MethodCallExpr::cast(node) else { return };
    let Some(ast::Expr::MethodCallExpr(cmp)) = unwrap.receiver().map(strip) else { return };
    if cmp.name_ref().is_none_or(|it| it.text() != "partial_cmp") {
        return;
    }
    let Some(how) = unwrap_of(cx, unwrap) else { return };
    let Some(f) = cx.method(cmp) else { return };
    let db = cx.db();
    let Some(partial_ord) = cx.lang_trait(hir::LangItem::PartialOrd) else { return };
    let trait_ = hir::AsAssocItem::as_assoc_item(f, db).and_then(|it| it.container_or_implemented_trait(db));
    if trait_ != Some(partial_ord) {
        return;
    }
    let Some(lhs) = cmp.receiver() else { return };
    let Some(float) = cx.ty(lhs).and_then(|it| float(&it.strip_references())) else { return };
    if in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let mut args = cmp.arg_list().into_iter().flat_map(|it| it.args());
    let fix = match (args.next(), args.next()) {
        (Some(rhs), None) => match (cx.range(lhs.syntax()), cx.range(rhs.syntax())) {
            (Some(l), Some(r)) => Some(Fix {
                message: "order by IEEE 754's total order, which puts NaN last and `-0.0` before `+0.0`",
                // alloc: the fix's one part
                parts: vec![(
                    range,
                    format!("{}.total_cmp({})", receiver_text(cx, lhs, l), cx.snippet(r)),
                )],
                applicability: Applicability::MaybeIncorrect,
            }),
            _ => None,
        },
        _ => None,
    };
    report(
        cx,
        LINT,
        node,
        range,
        format!("`partial_cmp` of two `{float}`s is `None` when either is NaN, and `{how}` panics there"),
        "one NaN in the data panics the comparison, and a sort that uses it aborts",
        fix,
    );
}

/// The float type `ty` is, by name.
fn float(ty: &hir::Type<'_>) -> Option<&'static str> {
    let builtin = ty.as_builtin()?;
    if builtin.is_f32() {
        Some("f32")
    } else if builtin.is_f64() {
        Some("f64")
    } else {
        None
    }
}
