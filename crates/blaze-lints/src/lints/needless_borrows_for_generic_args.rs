//! `needless_borrows_for_generic_args`: `&x` passed where the parameter is a type parameter
//! whose bounds `x` meets itself, so the borrow is only dereferenced again.
//!
//! Follows clippy's `clippy_lints/src/needless_borrows_for_generic_args.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). Where clippy asks MIR whether
//! a `&mut` referent is used once, this does not lint; bounds on other parameters, which
//! clippy instantiates with the referent's type too, are not checked, nor are bounds whose
//! arguments are the callee's parameters. A projection the signature binds or bounds
//! (`I: IntoIterator<Item = &'a T>`) is held to be the borrow's, where clippy checks the bound
//! on what the referent's projection is.

use hir::{GenericParam, HasSource as _, PathResolution};
use ide_db::famous_defs::FamousDefs;
use syntax::{
    AstNode, SyntaxNode,
    ast::{self, HasGenericArgs as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: &str = "needless_borrows_for_generic_args";

pub(super) fn check(cx: &mut Cx<'_, '_, '_>, expr: &ast::Expr) {
    if !cx.on(LINT) || cx.expansion.is_some() {
        return;
    }
    let ast::Expr::RefExpr(_) = expr else { return };
    let Some((function, index)) = callee(cx, expr.syntax()) else { return };
    let count = needless_borrow_count(cx, function, index, expr);
    if count == 0 {
        return;
    }
    let mut peeled = expr.clone();
    for _ in 0..count {
        let ast::Expr::RefExpr(reference) = &peeled else { break };
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
fn callee(cx: &Cx<'_, '_, '_>, node: &SyntaxNode) -> Option<(hir::Function, usize)> {
    let mut arg = node.clone();
    while let Some(paren) = arg.parent().and_then(ast::ParenExpr::cast) {
        arg = paren.syntax().clone();
    }
    let list = ast::ArgList::cast(arg.parent()?)?;
    let index = list.args().position(|it| *it.syntax() == arg)?;
    let call = list.syntax().parent()?;
    // clippy's `same_ctxt`: in a macro's expansion, the call is one of what it was handed, not
    // one the macro makes of it (`insta::with_settings!({ snapshot_path => &dir }, ..)`).
    if !cx.in_file() && !cx.is_written(&call) {
        return None;
    }
    if let Some(call) = ast::MethodCallExpr::cast(call.clone()) {
        if call.generic_arg_list().is_some() {
            return None;
        }
        let function = cx.sema().resolve_method_call(&call)?;
        return Some((function, index + 1));
    }
    let call = ast::CallExpr::cast(call)?;
    let ast::Expr::PathExpr(path) = call.expr()? else { return None };
    let path = path.path()?;
    if path.segment()?.generic_arg_list().is_some() {
        return None;
    }
    match cx.sema().resolve_path(&path)? {
        PathResolution::Def(hir::ModuleDef::Function(function)) => Some((function, index)),
        _ => None,
    }
}

fn needless_borrow_count(
    cx: &Cx<'_, '_, '_>,
    function: hir::Function,
    index: usize,
    expr: &ast::Expr,
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
    let into_iter = FamousDefs(cx.sema(), krate).core_iter_IntoIterator();
    let drop = lang(hir::LangItem::Drop);
    let projected = bound_assoc_types(db, function, type_param);
    let holds = |borrowed: &ast::Expr, referent: &ast::Expr| -> bool {
        let inner = peel_parens(referent);
        if let ast::Expr::FieldExpr(field) = &inner
            && let Some(base) = field.expr().and_then(|it| cx.ty(&it))
            && drop.is_some_and(|it| base.impls_trait(db, it, &[]))
        {
            return false;
        }
        let Some(ty) = cx.ty(referent) else { return false };
        if !(ty.is_copy(db) || is_call(cx, &inner)) {
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
            let mut args = Vec::new();
            for at in 1.. {
                let Some(arg) = bound.get_type_argument(at) else { break };
                let Some(arg) = arg.without_params(db) else { return false };
                args.push(arg);
            }
            if !ty.impls_trait(db, trait_, &args) {
                return false;
            }
            // A bound through a projection holds for the referent where the projection is the
            // one the borrow has: `extend(&set)` takes `&T`s, `extend(set)` would take `T`s.
            // Bound to a parameter of the callee, the projection is that parameter's, which
            // holds where its bounds do: `args(&["a"])` and `args(["a"])` both give `AsRef<OsStr>`s.
            let mut projections = Vec::new();
            for item in trait_.items(db) {
                let hir::AssocItem::TypeAlias(alias) = item else { continue };
                let alias_name = alias.name(db);
                for it in projected.iter().filter(|it| it.name == alias_name.as_str()) {
                    projections.push((alias, it.to));
                }
            }
            if projections.is_empty() {
                return true;
            }
            let Some(borrowed) = cx.ty(borrowed) else { return false };
            projections.into_iter().all(|(alias, to)| {
                let Some(now) = ty.normalize_trait_assoc_type(db, &args, alias) else {
                    return false;
                };
                if let Some(to) = to {
                    let generic = GenericParam::TypeParam(to);
                    let elsewhere =
                        params.iter().any(|it| it.ty().generic_params(db).contains(&generic))
                            || function.ret_type(db).generic_params(db).contains(&generic);
                    return !elsewhere
                        && to.trait_refs(db).iter().all(|bound| {
                            let mut args = Vec::new();
                            for at in 1.. {
                                let Some(arg) = bound.get_type_argument(at) else { break };
                                let Some(arg) = arg.without_params(db) else { return false };
                                args.push(arg);
                            }
                            now.impls_trait(db, bound.trait_(), &args)
                        });
                }
                borrowed
                    .normalize_trait_assoc_type(db, &args, alias)
                    .is_some_and(|was| was.could_unify_with_deeply(db, &now))
            })
        })
    };
    let mut count = 0;
    let mut at = expr.clone();
    while let ast::Expr::RefExpr(reference) = &at {
        let Some(referent) = reference.expr() else { break };
        if !holds(&at, &referent) {
            break;
        }
        at = referent;
        count += 1;
    }
    count
}

fn has_ref_mut_self_method(db: &dyn hir::db::HirDatabase, trait_: hir::Trait) -> bool {
    trait_.items(db).into_iter().any(|it| match it {
        hir::AssocItem::Function(function) => {
            function.self_param(db).is_some_and(|it| it.access(db) == hir::Access::Exclusive)
        }
        _ => false,
    })
}

/// An associated type `function`'s signature binds or bounds for a type parameter, and the
/// callee's type parameter it is bound to, when it is one: `Item` and `S` of
/// `I: IntoIterator<Item = S>`.
struct Projected {
    name: String,
    to: Option<hir::TypeParam>,
}

/// The associated types `function`'s signature binds or bounds for `type_param`: `Item` of
/// `I: IntoIterator<Item = &'a T>` or of `where I::Item: Debug`, `Output` of `F: Fn() -> R`.
fn bound_assoc_types(
    db: &dyn hir::db::HirDatabase,
    function: hir::Function,
    type_param: hir::TypeParam,
) -> Vec<Projected> {
    let params: Vec<_> = hir::GenericDef::from(function)
        .type_or_const_params(db)
        .into_iter()
        .filter_map(|it| it.as_type_param(db))
        .collect();
    let param_named = |ty: Option<ast::Type>| {
        let ast::Type::PathType(ty) = ty? else { return None };
        let path = ty.path()?;
        if path.qualifier().is_some() || path.segment()?.generic_arg_list().is_some() {
            return None;
        }
        let name = path.segment()?.name_ref()?;
        params.iter().copied().find(|it| it.name(db).as_str() == name.text())
    };
    let Some(source) = function.source(db) else { return Vec::new() };
    let function = source.value;
    let name = type_param.name(db);
    let name = name.as_str();
    let mut found = Vec::new();
    let bindings = |bounds: Option<ast::TypeBoundList>, found: &mut Vec<Projected>| {
        let Some(bounds) = bounds else { return };
        for node in bounds.syntax().descendants() {
            if let Some(arg) = ast::AssocTypeArg::cast(node.clone()) {
                let Some(name) = arg.name_ref() else { continue };
                let to = param_named(arg.ty());
                found.push(Projected { name: name.text().to_string(), to });
            } else if let Some(segment) = ast::PathSegment::cast(node)
                && segment.parenthesized_arg_list().is_some()
            {
                found.push(Projected { name: "Output".to_owned(), to: None });
            }
        }
    };
    for param in function.generic_param_list().iter().flat_map(|it| it.generic_params()) {
        if let ast::GenericParam::TypeParam(param) = param
            && param.name().is_some_and(|it| it.text() == name)
        {
            bindings(param.type_bound_list(), &mut found);
        }
    }
    for predicate in function.where_clause().iter().flat_map(|it| it.predicates()) {
        let Some(ast::Type::PathType(ty)) = predicate.ty() else { continue };
        let Some(path) = ty.path() else { continue };
        let Some(last) = path.segment().and_then(|it| it.name_ref()) else { continue };
        match path.qualifier() {
            None if last.text() == name => bindings(predicate.type_bound_list(), &mut found),
            Some(qualifier) => {
                let first = qualifier.first_segment();
                let named = first.as_ref().is_some_and(|it| {
                    it.name_ref().is_some_and(|it| it.text() == name)
                        || it.type_anchor().and_then(|it| it.ty()).is_some_and(|it| {
                            it.syntax().text() == name
                                || it.syntax().to_string().starts_with(&format!("{name} "))
                        })
                });
                if named {
                    found.push(Projected { name: last.text().to_string(), to: None });
                }
            }
            None => {}
        }
    }
    found
}

fn peel_parens(expr: &ast::Expr) -> ast::Expr {
    let mut expr = expr.clone();
    while let ast::Expr::ParenExpr(paren) = &expr {
        let Some(inner) = paren.expr() else { break };
        expr = inner;
    }
    expr
}

/// Whether `expr` is a call, or a macro call that expands to one, as `format!` does.
fn is_call(cx: &Cx<'_, '_, '_>, expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::CallExpr(_) | ast::Expr::MethodCallExpr(_) => true,
        ast::Expr::MacroExpr(mac) => {
            let Some(call) = mac.macro_call() else { return false };
            let Some(expanded) = cx.sema().expand_macro_call(&call) else { return false };
            let root = expanded.value;
            let expr =
                ast::Expr::cast(root.clone()).or_else(|| root.children().find_map(ast::Expr::cast));
            expr.is_some_and(|it| is_call(cx, &peel_parens(&it)))
        }
        _ => false,
    }
}
