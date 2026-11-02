//! `needless_borrows_for_generic_args`: `&x` passed where the parameter is a type parameter
//! whose bounds `x` meets itself, so the borrow is only dereferenced again.
//!
//! Follows clippy's `clippy_lints/src/needless_borrows_for_generic_args.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). Where clippy asks MIR whether
//! a `&mut` referent is used once, this does not lint; bounds on other parameters and on
//! projections, which clippy instantiates with the referent's type too, are not checked, nor
//! are bounds whose arguments are the callee's parameters.

use hir::{GenericParam, PathResolution};
use syntax::view as ast;

use ast::{AstNode as _, HasGenericArgs as _};

use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: super::Implemented = super::Implemented::NeedlessBorrowsForGenericArgs;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || cx.expansion().is_some() {
        return;
    }
    let ast::Expr::RefExpr(_) = expr else { return };
    let Some((function, index)) = callee(cx, expr.syntax()) else { return };
    let count = needless_borrow_count(cx, function, index, expr);
    if count == 0 {
        return;
    }
    let mut peeled = expr;
    for _ in 0..count {
        let ast::Expr::RefExpr(reference) = peeled else { break };
        let Some(inner) = reference.expr() else { return };
        peeled = inner;
    }
    let (Some(range), Some(inner)) = (cx.range(expr.syntax()), cx.range(peeled.syntax())) else {
        return;
    };
    let snippet = cx.snippet(inner);
    let span = cx.span(range);
    cx.lint(
        LINT,
        expr.syntax(),
        span.clone(),
        "the borrowed expression implements the required traits",
    )
    .suggestion(span, "change this to", snippet, Applicability::MachineApplicable);
}

/// The function `node` is an argument of, and which of its parameters it is: clippy's
/// `ExprUseNode::FnArg` of a path with no generic arguments, or `MethodArg` other than the
/// receiver of a call with none.
fn callee<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<(hir::Function, usize)> {
    let mut arg = node;
    while let Some(paren) = arg.parent().and_then(ast::ParenExpr::cast) {
        arg = paren.syntax();
    }
    let list = ast::ArgList::cast(arg.parent()?)?;
    let index = list.args().position(|it| it.syntax() == arg)?;
    let call = list.syntax().parent()?;
    if let Some(call) = ast::MethodCallExpr::cast(call) {
        if call.generic_arg_list().is_some() {
            return None;
        }
        let function = cx.method(call)?;
        return Some((function, index + 1));
    }
    let call = ast::CallExpr::cast(call)?;
    let ast::Expr::PathExpr(path) = call.expr()? else { return None };
    let path = path.path()?;
    if path.segment()?.generic_arg_list().is_some() {
        return None;
    }
    match cx.path(path)? {
        PathResolution::Def(hir::ModuleDef::Function(function)) => Some((function, index)),
        _ => None,
    }
}

fn needless_borrow_count<'t>(
    cx: &Cx<'_, 't>,
    function: hir::Function,
    index: usize,
    expr: ast::Expr<'t>,
) -> usize {
    let db = cx.db();
    let params = function.assoc_fn_params(db);
    let Some(param) = params.get(index) else { return 0 };
    let Some(type_param) = param.ty().as_type_param(db) else { return 0 };
    let krate = cx.krate();
    let lang = |item| hir::Trait::lang(db, krate, item);
    let trivial = [
        lang(hir::LangItem::Destruct),
        lang(hir::LangItem::Sized),
        lang(hir::LangItem::MetaSized),
        cx.std_trait(&["core", "any", "Any"]),
    ];
    let bounds = type_param.trait_refs(db);
    if bounds.iter().all(|it| trivial.contains(&Some(it.trait_()))) {
        return 0;
    }
    let ref_mut_self = bounds.iter().any(|it| has_ref_mut_self_method(db, it.trait_()));
    // `replace_types`: the parameter is in the signature only where the argument is.
    let generic = GenericParam::TypeParam(type_param);
    let elsewhere = params
        .iter()
        .enumerate()
        .any(|(at, it)| at != index && it.ty().generic_params(db).contains(&generic))
        || function.ret_type(db).generic_params(db).contains(&generic);
    if elsewhere {
        return 0;
    }
    let into_iter = cx.famous().core_iter_IntoIterator();
    let drop = lang(hir::LangItem::Drop);
    // The type arguments of the bound being asked of, past its `Self`.
    let mut args = Vec::new();
    let mut holds = |referent: ast::Expr<'t>| -> bool {
        let inner = referent.peel_parens();
        if let ast::Expr::FieldExpr(field) = inner
            && let Some(base) = field.expr().and_then(|it| cx.ty(it))
            && drop.is_some_and(|it| base.impls_trait(db, it, &[]))
        {
            return false;
        }
        let Some(ty) = cx.ty(referent) else { return false };
        if !(ty.is_copy(db) || is_call(cx, inner)) {
            return false;
        }
        if ref_mut_self && !ty.is_mutable_reference() {
            return false;
        }
        bounds.iter().all(|bound| {
            let trait_ = bound.trait_();
            if Some(trait_) == into_iter
                && ty.is_array()
                && !cx.conf().meets(msrvs::ARRAY_INTO_ITERATOR)
            {
                return false;
            }
            args.clear();
            for at in 1.. {
                let Some(arg) = bound.get_type_argument(at) else { break };
                let Some(arg) = arg.without_params(db) else { return false };
                args.push(arg);
            }
            ty.impls_trait(db, trait_, &args)
        })
    };
    let mut count = 0;
    let mut at = expr;
    while let ast::Expr::RefExpr(reference) = at {
        let Some(referent) = reference.expr() else { break };
        if !holds(referent) {
            break;
        }
        at = referent;
        count += 1;
    }
    count
}

fn has_ref_mut_self_method(db: &base_db::Db, trait_: hir::Trait) -> bool {
    trait_.items(db).into_iter().any(|it| match it {
        hir::AssocItem::Function(function) => {
            function.self_param(db).is_some_and(|it| it.access(db) == hir::Access::Exclusive)
        }
        _ => false,
    })
}

/// Whether `expr` is a call, or a macro call that expands to one, as `format!` does.
fn is_call<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    match expr {
        ast::Expr::CallExpr(_) | ast::Expr::MethodCallExpr(_) => true,
        ast::Expr::MacroExpr(mac) => {
            let Some(call) = mac.macro_call() else { return false };
            let Some(root) = cx.expand(call) else { return false };
            let expr = ast::Expr::cast(root).or_else(|| root.children().find_map(ast::Expr::cast));
            expr.is_some_and(|it| is_call(cx, it.peel_parens()))
        }
        _ => false,
    }
}
