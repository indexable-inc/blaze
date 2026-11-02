//! `needless_borrow`: `&x` where `x` is already a reference and the compiler dereferences the
//! borrow away again.
//!
//! Follows clippy's `clippy_lints/src/dereference.rs` (the `DerefedBorrow` state) at
//! rust-lang/rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0), for a borrow
//! used as a call's or method's argument, a field's value, a `let`'s initializer or what a
//! `return` gives. Not ported: borrows that are a receiver, a callee or have a field read
//! (where clippy asks whether auto-borrow would pick the same method) and chains of `&` and
//! `*`. A `ref` binding to a reference whose every use the compiler
//! dereferences is linted too, as clippy's `check_pat` does.

use hir::{Adjust, AutoBorrow, Mutability, PathResolution};
use syntax::{TextRange, view as ast};

use ast::{AstNode as _, HasName as _};

use super::utils;
use crate::{
    check::Cx,
    diag::{Applicability, MultiSpan},
    sugg::has_enclosing_paren,
};

const LINT: super::Implemented = super::Implemented::NeedlessBorrow;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || cx.expansion().is_some() {
        return;
    }
    let ast::Expr::RefExpr(borrow) = expr else { return };
    if borrow.raw_token().is_some() {
        return;
    }
    let Some(sub) = borrow.expr() else { return };
    if !cx.is_written(sub.syntax()) || is_ref_op(sub.peel_parens()) {
        return;
    }
    // The node the borrow is used at, past the parentheses around it.
    let used = past_parens(expr.syntax());
    let Some(parent) = used.parent() else { return };
    let in_call = ast::ArgList::can_cast(parent.kind());
    let is_use = in_call
        || ast::RecordExprField::can_cast(parent.kind())
        || ast::LetStmt::can_cast(parent.kind())
        || ast::ReturnExpr::can_cast(parent.kind());
    if !is_use {
        return;
    }
    let Some(adjustments) = ast::Expr::cast(used).and_then(|it| cx.adjustments(it)) else {
        return;
    };
    let mut iter = adjustments.iter();
    let mut deref_count = 0usize;
    let next = loop {
        match iter.next() {
            Some(adjust) if !matches!(adjust.kind, Adjust::Deref(_)) => break Some(adjust),
            Some(adjust) if !adjust.target.is_reference() => {
                deref_count += 1;
                break iter.next();
            }
            Some(_) => deref_count += 1,
            None => break None,
        }
    };
    // A mutable reborrow the compiler does not insert by itself keeps one more reference, so
    // that the borrow is not moved.
    let reborrows_mut =
        next.is_some_and(|it| matches!(it.kind, Adjust::Borrow(AutoBorrow::Ref(Mutability::Mut))));
    let required = if reborrows_mut && !reborrow_stable(cx, used, parent) { 3 } else { 2 };
    if deref_count < required {
        return;
    }
    // `&*p` of a raw pointer stays, as rustc's `dangerous_implicit_autorefs` wants it.
    if let ast::Expr::PrefixExpr(prefix) = sub.peel_parens()
        && prefix.op_kind() == Some(ast::UnaryOp::Deref)
        && prefix.expr().and_then(|it| cx.adjusted_ty(it)).is_some_and(|it| it.is_raw_ptr())
    {
        return;
    }
    let (Some(range), Some(sub_range)) = (cx.range(expr.syntax()), cx.range(sub.syntax())) else {
        return;
    };
    let snip = cx.snippet(sub_range);
    // A method call binds tighter than an operator: a call's arguments are each whole.
    let in_method_call =
        in_call && parent.parent().is_some_and(|it| ast::MethodCallExpr::can_cast(it.kind()));
    let needs_paren = in_method_call
        && matches!(
            sub,
            ast::Expr::PrefixExpr(_)
                | ast::Expr::BinExpr(_)
                | ast::Expr::CastExpr(_)
                | ast::Expr::RangeExpr(_)
        );
    let sugg = match needs_paren && !has_enclosing_paren(snip) {
        true => format!("({snip})"),
        false => snip.to_owned(),
    };
    let span = cx.span(range);
    cx.lint(
        LINT,
        expr.syntax(),
        span.clone(),
        "this expression creates a reference which is immediately dereferenced by the compiler",
    )
    .suggestion(span, "change this to", sugg, Applicability::MachineApplicable);
}

/// clippy's `TyCoercionStability::is_reborrow_stable` for where the borrow is used: whether
/// the type declared there is a reference, which the compiler reborrows into by itself.
fn reborrow_stable<'t>(cx: &Cx<'_, 't>, used: ast::Node<'t>, parent: ast::Node<'t>) -> bool {
    let db = cx.db();
    if let Some(args) = ast::ArgList::cast(parent) {
        let Some(index) = args.args().position(|it| it.syntax() == used) else { return false };
        let declared = match args.syntax().parent().and_then(ast::Expr::cast) {
            Some(ast::Expr::MethodCallExpr(call)) => cx
                .method(call)
                .and_then(|it| it.params_without_self(db).into_iter().nth(index))
                .map(|it| it.ty().clone()),
            Some(ast::Expr::CallExpr(call)) => {
                // `Semantics::resolve_expr_as_callable`: the callee's type as adjusted.
                let Some(callable) = call
                    .expr()
                    .and_then(|it| cx.adjusted_ty(it))
                    .and_then(|it| it.as_callable(db))
                else {
                    return false;
                };
                match callable.kind() {
                    hir::CallableKind::Function(function) => function
                        .params_without_self(db)
                        .into_iter()
                        .nth(index)
                        .map(|it| it.ty().clone()),
                    _ => callable.params().into_iter().nth(index).map(|it| it.ty().clone()),
                }
            }
            _ => None,
        };
        return declared.is_some_and(|it| it.is_reference());
    }
    if let Some(field) = ast::RecordExprField::cast(parent) {
        return record_field_ty(cx, field).is_some_and(|it| it.is_reference());
    }
    if let Some(stmt) = ast::LetStmt::cast(parent) {
        return matches!(stmt.ty(), Some(ast::Type::RefType(_)));
    }
    if ast::ReturnExpr::can_cast(parent.kind()) {
        let function = parent.ancestors().find_map(ast::Fn::cast);
        return function
            .and_then(|it| cx.fn_def(it))
            .is_some_and(|it| it.ret_type(db).is_reference());
    }
    false
}

/// The declared type of the field `field` sets, of the variant `Semantics::resolve_record_field`
/// finds: the enum variant the literal's path names, else the struct or union it is of.
fn record_field_ty<'q, 't>(
    cx: &Cx<'q, 't>,
    field: ast::RecordExprField<'t>,
) -> Option<hir::Type<'q>> {
    let db = cx.db();
    let name = field.field_name()?.text();
    let name = name.strip_prefix("r#").unwrap_or(name);
    let record = field.parent_record_lit();
    let variant = match record.path().and_then(|it| cx.path(it)) {
        Some(PathResolution::Def(hir::ModuleDef::EnumVariant(it))) => hir::Variant::EnumVariant(it),
        _ => match cx.ty(ast::Expr::RecordExpr(record))?.as_adt()? {
            hir::Adt::Struct(it) => hir::Variant::Struct(it),
            hir::Adt::Union(it) => hir::Variant::Union(it),
            hir::Adt::Enum(_) => return None,
        },
    };
    let field = variant.fields(db).into_iter().find(|it| it.name(db).as_str() == name)?;
    Some(field.ty(db))
}

/// clippy's `try_parse_ref_op`: a borrow or a dereference.
fn is_ref_op(expr: ast::Expr<'_>) -> bool {
    match expr {
        ast::Expr::RefExpr(it) => it.raw_token().is_none(),
        ast::Expr::PrefixExpr(it) => it.op_kind() == Some(ast::UnaryOp::Deref),
        _ => false,
    }
}

/// clippy's `check_pat` and `check_local_usage`: `ref x` where `x` is bound to a `&&T`, used
/// only where the compiler dereferences it twice, as a field's base or dereferenced.
pub(super) fn check_ref_pat<'t>(cx: &mut Cx<'_, 't>, pat: ast::IdentPat<'t>) {
    if !cx.on(LINT) || pat.ref_token().is_none() || pat.mut_token().is_some() {
        return;
    }
    let db = cx.db();
    let Some(local) = cx.local(pat) else { return };
    let ty = local.ty(db);
    let Some((inner, _)) = ty.as_reference() else { return };
    if !matches!(inner.as_reference(), Some((_, Mutability::Shared))) {
        return;
    }
    let Some(body) = utils::enclosing_body(pat.syntax()) else { return };
    // The bindings of `local`, more than one in alternatives of an or-pattern.
    let shared = &*cx;
    let bindings = move || {
        body_and_params(body)
            .flat_map(|it| it.descendants())
            .filter_map(ast::IdentPat::cast)
            .filter(move |it| shared.local(*it) == Some(local))
    };
    if bindings().next().is_some_and(|it| it != pat) {
        return;
    }
    // The dereferences of `local` that become the binding itself.
    let mut derefs: Vec<(TextRange, &'t str)> = Vec::new();
    let mut cancelled = false;
    let mut always_deref = true;
    utils::each_path(shared, body, None, &mut |path| {
        if cancelled || utils::local_of(shared, path) != Some(local) {
            return;
        }
        let adjustments = shared.adjustments(ast::Expr::PathExpr(path)).unwrap_or_default();
        if matches!(&adjustments[..], [first, second, ..] if matches!(first.kind, Adjust::Deref(_)) && matches!(second.kind, Adjust::Deref(_)))
        {
            return;
        }
        let used = past_parens(path.syntax());
        match used.parent().and_then(parent_expr) {
            Some(ast::Expr::FieldExpr(_)) => {}
            Some(ast::Expr::PrefixExpr(prefix))
                if prefix.op_kind() == Some(ast::UnaryOp::Deref) =>
            {
                match (shared.range(prefix.syntax()), shared.range(path.syntax())) {
                    (Some(at), Some(name)) => derefs.push((at, shared.snippet(name))),
                    _ => cancelled = true,
                }
            }
            Some(parent) if shared.range(parent.syntax()).is_some() => {
                if unambiguous(parent) {
                    cancelled = true;
                } else {
                    always_deref = false;
                }
            }
            _ if shared.range(path.syntax()).is_some() => always_deref = false,
            _ => cancelled = true,
        }
    });
    // A binding some use needs as a double reference is `ref_binding_to_reference`'s.
    if cancelled || !always_deref {
        return;
    }
    let mut span = MultiSpan::default();
    let mut parts = Vec::new();
    for binding in bindings() {
        let (Some(range), Some(name)) = (shared.range(binding.syntax()), binding.name()) else {
            return;
        };
        let Some(name) = shared.range(name.syntax()) else { return };
        span.primary.push(shared.span(range));
        parts.push((shared.span(range), shared.snippet(name).to_owned()));
    }
    parts.extend(derefs.into_iter().map(|(at, text)| (cx.span(at), text.to_owned())));
    cx.lint_after(LINT, pat.syntax(), body, span, "this pattern creates a reference to a reference")
        .multipart_suggestion("try", parts, Applicability::MachineApplicable);
}

/// The body and, for a closure or a function, its parameters: where its bindings are.
fn body_and_params(body: ast::Node<'_>) -> impl Iterator<Item = ast::Node<'_>> {
    let params = body.parent().and_then(|owner| {
        ast::ClosureExpr::cast(owner)
            .and_then(|it| it.param_list())
            .or_else(|| ast::Fn::cast(owner).and_then(|it| it.param_list()))
    });
    params.map(|it| it.syntax()).into_iter().chain([body])
}

/// The expression rustc's HIR has as `node`'s parent, past what is not an expression there.
fn parent_expr(node: ast::Node<'_>) -> Option<ast::Expr<'_>> {
    let node = if ast::ArgList::can_cast(node.kind()) { node.parent()? } else { node };
    ast::Expr::cast(node)
}

/// Whether rustc's `ExprPrecedence` of `expr` is `Unambiguous`: an operand needs no
/// parentheses around it.
fn unambiguous(expr: ast::Expr<'_>) -> bool {
    expr.precedence() >= ast::prec::ExprPrecedence::Postfix
}

fn past_parens(node: ast::Node<'_>) -> ast::Node<'_> {
    let mut node = node;
    while let Some(parent) = node.parent()
        && ast::ParenExpr::can_cast(parent.kind())
    {
        node = parent;
    }
    node
}
