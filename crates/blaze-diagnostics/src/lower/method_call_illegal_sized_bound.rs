//! "the `f` method cannot be invoked on a trait object": a method call on a trait object of a
//! method with a `Self: Sized` bound, on the method's name, with "this has a `Sized`
//! requirement" on the bound; "the `f` method cannot be invoked on `&T`" where the receiver of
//! the other mutability picks another method, with "you need `&mut T` instead of `&T`".
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/mod.rs` (`lookup_method`),
//! `method/confirm.rs` (`predicates_require_illegal_sized_bound`) and `method/suggest.rs`
//! (`report_method_error`, `MethodError::IllegalSizedBound`, `suggest_use_candidates`) (MIT or
//! Apache-2.0).

use either::Either;
use hir::{HasVisibility as _, InFile, LangItem, PathResolution};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericParams as _, HasName as _, HasTypeBounds as _, PathSegmentKind, UnaryOp},
};

use super::{
    binary_operator_cannot_be_applied::code_fact,
    unresolved_method_call::{crate_path, inject_use_span},
};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MethodCallIllegalSizedBound) -> Vec<Found> {
    let node = cx.node_view(d.call_expr);
    let call = match node.value.left() {
        Some(ast::Expr::MethodCallExpr(it)) => Some(it),
        _ => None,
    };
    let name = call.and_then(|it| it.name_ref());
    let span = match name {
        Some(name) => cx.span(InFile::new(node.file_id, name.syntax().text_range())),
        None => cx.span_of_ptr(d.call_expr),
    };
    let method = name.map_or("", |it| it.text());
    let receiver = call.and_then(|it| it.receiver());
    let rcvr_ty = receiver.and_then(|it| cx.vsema.type_of_expr(it)).map(|it| it.original());
    let message = match (d.needs_mut, &rcvr_ty) {
        // alloc: the error's text, for an error hir found
        (true, Some(ty)) => format!("the `{method}` method cannot be invoked on `{}`", cx.ty(ty)),
        // alloc: the error's text, for an error hir found
        _ => format!("the `{method}` method cannot be invoked on a trait object"),
    };
    let mut diag = Diag::error(None, message, span);
    if !d.needs_mut
        && let Some(bound) = d.bound.and_then(|it| sized_bound(cx, it))
    {
        diag.span_label(bound, "this has a `Sized` requirement");
    }
    if !d.candidates.is_empty()
        && let Some(module) = call.and_then(|it| cx.vsema.scope(it.syntax())).map(|it| it.module())
    {
        use_candidates(cx, &mut diag, module, method, &d.candidates);
    }
    if d.needs_mut
        && let Some(ty) = &rcvr_ty
        && let Some((_, mutability)) = ty.as_reference()
    {
        let printed = cx.ty(ty);
        // alloc: the error's text, for an error hir found
        let message = format!("you need `{}` instead of `{printed}`", inverted(&printed, mutability));
        match receiver.and_then(|it| param_ref_type(cx, it)) {
            Some(at) => {
                diag.suggestion_verbose(at, message, "mut ", Applicability::MachineApplicable);
            }
            None => {
                diag.help(message);
            }
        }
    }
    code_fact(cx, d.call_expr.map(|it| it.syntax_node_ptr()), &mut diag);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// The `Sized` bound on `Self` written in `def`'s own bounds or where clause: rustc's span of the
/// predicate `predicates_require_illegal_sized_bound` finds.
fn sized_bound(cx: &Cx<'_, '_>, def: hir::GenericDef) -> Option<Span> {
    let sized = hir::Trait::lang(cx.db, cx.krate, LangItem::Sized)?;
    let (file, where_clause, supertraits) = match def {
        hir::GenericDef::Function(it) => {
            let source = cx.vsema.source(it)?;
            (source.file_id, source.value.where_clause(), None)
        }
        hir::GenericDef::Trait(it) => {
            let source = cx.vsema.source(it)?;
            (source.file_id, source.value.where_clause(), source.value.type_bound_list())
        }
        hir::GenericDef::Impl(it) => {
            let source = cx.vsema.source(it)?;
            (source.file_id, source.value.where_clause(), None)
        }
        other => unreachable!("hir gives a `Self: Sized` bound of a method, trait or impl, not of {other:?}"),
    };
    let on_self = where_clause.into_iter().flat_map(|it| it.predicates()).filter_map(|pred| {
        let Some(ast::Type::PathType(ty)) = pred.ty() else { return None };
        let segment = ty.path()?.as_single_segment()?;
        matches!(segment.kind(), Some(PathSegmentKind::SelfTypeKw)).then(|| pred.type_bound_list()).flatten()
    });
    for bound in supertraits.into_iter().chain(on_self).flat_map(|it| it.bounds()) {
        if bound.question_mark_token().is_some() {
            continue;
        }
        let Some(ast::Type::PathType(ty)) = bound.ty() else { continue };
        if let Some(PathResolution::Def(hir::ModuleDef::Trait(trait_))) =
            ty.path().and_then(|it| cx.vsema.resolve_path(it))
            && trait_ == sized
        {
            return Some(cx.span(InFile::new(file, ty.syntax().text_range())));
        }
    }
    None
}

/// rustc's help on the traits a probe of every crate's traits picks the method from, with
/// `suggest_use_candidates`' `use` of each at the module's `inject_use_span`.
fn use_candidates(cx: &Cx<'_, '_>, diag: &mut Diag, module: hir::Module, method: &str, candidates: &[hir::Trait]) {
    let s = if candidates.len() == 1 { "" } else { "s" };
    // alloc: the error's text, for an error hir found
    let help = format!(
        "{an}other candidate{s} {were} found in the following trait{s}",
        an = if candidates.len() == 1 { "an" } else { "" },
        were = if candidates.len() == 1 { "was" } else { "were" },
    );
    let (accessible, inaccessible): (Vec<_>, Vec<_>) =
        candidates.iter().copied().partition(|t| t.is_visible_from(cx.db, module));
    let sugg = |traits: &[hir::Trait], visible: bool| {
        let (prefix, postfix) = if visible { ("use ", ";") } else { ("", "") };
        let mut suggs: Vec<String> =
            // alloc: the error's text, for an error hir found
            traits.iter().map(|t| format!("{prefix}{}{postfix}\n", crate_path(cx, module, *t))).collect();
        suggs.sort();
        suggs
    };
    let accessible = sugg(&accessible, true);
    let inaccessible = sugg(&inaccessible, false);
    let Some(span) = inject_use_span(cx, module) else { return };
    let for_access = |diag: &mut Diag, mut message: String, suggs: Vec<String>| {
        // alloc: the error's text, for an error hir found
        message += &format!(
            ", perhaps add a `use` for {}:",
            if suggs.len() == 1 { "it" } else { "one_of_them" }
        );
        // clone: on the error path, for an error hir found
        diag.suggestions(span.clone(), message, suggs, Applicability::MaybeIncorrect, SuggestionStyle::ShowAlways);
    };
    let for_privacy = |diag: &mut Diag, mut message: String, suggs: Vec<String>| {
        if let [only] = suggs.as_slice() {
            // alloc: the error's text, for an error hir found
            diag.help(format!("trait `{}` provides `{method}` is implemented but not reachable", only.trim()));
        } else {
            // alloc: the error's text, for an error hir found
            message += &format!(" but {} not reachable", if suggs.len() == 1 { "is" } else { "are" });
            // clone: on the error path, for an error hir found
            diag.suggestions(span.clone(), message, suggs, Applicability::MaybeIncorrect, SuggestionStyle::ShowAlways);
        }
    };
    if accessible.is_empty() {
        for_privacy(diag, help, inaccessible);
    } else if inaccessible.is_empty() {
        for_access(diag, help, accessible);
    } else {
        // clone: on the error path, for an error hir found
        for_access(diag, help.clone(), accessible);
        for_privacy(diag, help, inaccessible);
    }
}

/// `printed`, a reference type as [`Cx::ty`] writes it (`&`, a lifetime if any, `mut ` if
/// mutable, then the referent), with the other mutability: rustc's
/// `Ty::new_ref(region, t_type, mutability.invert())`.
fn inverted(printed: &str, mutability: hir::Mutability) -> String {
    let rest = printed.strip_prefix('&').unwrap_or(printed);
    let (lifetime, rest) = match rest.strip_prefix('\'').and_then(|it| it.find(' ')) {
        Some(space) => rest.split_at(space + 2),
        None => ("", rest),
    };
    match mutability {
        // alloc: the error's text, for an error hir found
        hir::Mutability::Mut => format!("&{lifetime}{}", rest.strip_prefix("mut ").unwrap_or(rest)),
        // alloc: the error's text, for an error hir found
        hir::Mutability::Shared => format!("&{lifetime}mut {rest}"),
    }
}

/// Where `mut ` goes in the `&T` type of the function parameter `receiver` names through
/// `&` and `*`: rustc's `mut_ty.ty.span.shrink_to_lo()`.
fn param_ref_type<'db>(cx: &Cx<'_, 'db>, receiver: ast::Expr<'db>) -> Option<Span> {
    let mut expr = receiver;
    loop {
        expr = match expr {
            ast::Expr::RefExpr(it) => it.expr()?,
            ast::Expr::PrefixExpr(it) if it.op_kind() == Some(UnaryOp::Deref) => it.expr()?,
            _ => break,
        };
    }
    let ast::Expr::PathExpr(path) = expr else { return None };
    let Some(PathResolution::Local(local)) = cx.vsema.resolve_path(path.path()?) else { return None };
    let source = local.primary_source_view(cx.db);
    let file = source.source.file_id;
    let at = match source.source.value {
        Either::Left(pat) => {
            let param = pat.syntax().parent().and_then(ast::Param::cast)?;
            let ast::Type::RefType(ty) = param.ty()? else { return None };
            if ty.mut_token().is_some() {
                return None;
            }
            ty.ty()?.syntax().text_range().start()
        }
        Either::Right(param) => match param.ty() {
            Some(ast::Type::RefType(ty)) if ty.mut_token().is_none() => ty.ty()?.syntax().text_range().start(),
            Some(_) => return None,
            None if param.amp_token().is_some() && param.mut_token().is_none() => {
                param.name()?.syntax().text_range().start()
            }
            None => return None,
        },
    };
    Some(cx.span(InFile::new(file, TextRange::empty(at))))
}
