//! `iter_overeager_cloned`: `.cloned()` of an iterator of references before an adaptor that
//! drops items or only looks at them (`next`, `last`, `nth`, `skip`, `take`, `flatten`,
//! `filter`, `find`), which clones items it then throws away; and, as clippy reports it under
//! `redundant_clone`, `.cloned().count()`, which clones every item for nothing.
//!
//! Follows clippy's `clippy_lints/src/methods/iter_overeager_cloned.rs` and its dispatch in
//! `methods/mod.rs` (MIT or Apache-2.0), written from what is known of its source rather than
//! read from a checkout. As in clippy: both calls are `Iterator`'s methods, the receiver of
//! `cloned` is an iterator whose `Item` is a reference to a type that is not `Copy` (so
//! `copied`, which needs `Copy`, never qualifies), and for `flatten` that reference implements
//! `IntoIterator`. The fix moves `.cloned()` after the adaptor (`filter` and `find` take `|&x|`)
//! or, for `count`, removes it; both are MachineApplicable. A predicate that is not a closure
//! is wrapped as `|&x| pred(x)`, which hands it the `&T` it took before. Not ported: clippy's
//! `NeedlessMove` case (`map`, `for_each`, `all`, `any` whose closure never moves its item),
//! which needs a use analysis of the closure's body.

use hir::AsAssocItem as _;
use syntax::{TextRange, view as ast};

use ast::AstNode as _;

use super::{Implemented, utils};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::IterOvereagerCloned;

/// What the fix does with the `.cloned()`.
#[derive(Clone, Copy)]
enum Op<'t> {
    /// Move it after the adaptor.
    LaterCloned,
    /// Move it after the adaptor, which takes its item by reference: `filter` and `find`.
    FixClosure(ast::Expr<'t>),
    /// Remove it: `count` (reported as `redundant_clone`).
    RmCloned,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    let Some(call) = utils::method_call(cx, expr) else { return };
    let (op, needs_into_iter) = match (call.name, call.args()) {
        ("next" | "last" | "skip" | "take" | "nth", _) => (Op::LaterCloned, false),
        ("flatten", []) => (Op::LaterCloned, true),
        ("filter" | "find", [predicate]) => (Op::FixClosure(*predicate), false),
        ("count", []) => (Op::RmCloned, false),
        _ => return,
    };
    let lint = match op {
        Op::RmCloned => Implemented::RedundantClone,
        Op::LaterCloned | Op::FixClosure(_) => LINT,
    };
    if !cx.on(lint) {
        return;
    }
    let arity_ok = match call.name {
        "next" | "last" => call.args().is_empty(),
        "skip" | "take" | "nth" => call.args().len() == 1,
        _ => true,
    };
    if !arity_ok {
        return;
    }
    let cloned_expr = call.receiver;
    let Some(cloned) = utils::method_call(cx, cloned_expr) else { return };
    if cloned.name != "cloned" || !cloned.args().is_empty() {
        return;
    }
    let (ast::Expr::MethodCallExpr(outer), ast::Expr::MethodCallExpr(inner)) = (expr, cloned_expr)
    else {
        return;
    };
    let Some(iterator) = cx.famous().core_iter_Iterator() else { return };
    let db = cx.db();
    let of_iterator = |method: ast::MethodCallExpr<'t>| {
        cx.method(method).is_some_and(|f| {
            f.as_assoc_item(db).and_then(|it| it.container_or_implemented_trait(db))
                == Some(iterator)
        })
    };
    if !of_iterator(outer) || !of_iterator(inner) {
        return;
    }
    let Some(recv_ty) = cx.ty(cloned.receiver) else { return };
    let recv_ty = cx
        .adjustments(cloned.receiver)
        .and_then(|it| it.last().map(|it| it.target.clone()))
        .unwrap_or(recv_ty);
    let Some(item) = cx.assoc_ty(&recv_ty, iterator, "Item") else { return };
    let Some((pointee, _)) = item.as_reference() else { return };
    if pointee.is_copy(db) {
        return;
    }
    if needs_into_iter {
        let Some(into_iter) = cx.famous().core_iter_IntoIterator() else { return };
        if !item.impls_trait(db, into_iter, &[]) {
            return;
        }
    }
    let (Some(range), Some(cloned_range), Some(recv_range)) = (
        cx.range(expr.syntax()),
        cx.range(cloned_expr.syntax()),
        cx.range(cloned.receiver.syntax()),
    ) else {
        return;
    };
    let replace = cx.span(TextRange::new(recv_range.end(), range.end()));
    let code = match op {
        Op::RmCloned => cx.snippet(TextRange::new(cloned_range.end(), range.end())).to_owned(),
        Op::LaterCloned => {
            format!("{}.cloned()", cx.snippet(TextRange::new(cloned_range.end(), range.end())))
        }
        Op::FixClosure(predicate) => {
            let Some(predicate_range) = cx.range(predicate.syntax()) else { return };
            let predicate_text = cx.snippet(predicate_range);
            let closure = match predicate {
                ast::Expr::ClosureExpr(_) => predicate_text.replacen('|', "|&", 1),
                _ => format!("|&x| {predicate_text}(x)"),
            };
            format!(".{}({closure}).cloned()", call.name)
        }
    };
    let message = match op {
        Op::RmCloned => "unneeded cloning of iterator items",
        Op::LaterCloned | Op::FixClosure(_) => "unnecessarily eager cloning of iterator items",
    };
    let span = cx.span(range);
    cx.lint(lint, expr.syntax(), span, message).suggestion(
        replace,
        "try",
        code,
        Applicability::MachineApplicable,
    );
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const FOUND: &str = "unnecessarily eager cloning of iterator items";

    #[test]
    fn cloned_before_a_dropping_adaptor_is_found_and_after_it_or_of_copy_items_is_not() {
        let found = lint_only(
            "iter_overeager_cloned",
            r#"
pub fn f(v: &[String], n: &[u32]) {
    let _ = v.iter().cloned().last();
    let _: Vec<String> = v.iter().cloned().take(2).collect();
    let _ = v.iter().cloned().filter(|x| x.is_empty());
    let _ = v.iter().filter(|x| x.is_empty()).cloned();
    let _ = n.iter().cloned().last();
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains(".filter(|&x| x.is_empty()).cloned()"), "{found}");
        assert!(found.contains(".last().cloned()"), "{found}");
    }
}
