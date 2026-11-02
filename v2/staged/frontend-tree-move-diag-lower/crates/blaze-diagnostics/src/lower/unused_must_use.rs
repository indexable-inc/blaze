//! `unused_must_use`: a statement's value whose type, or the function it is the result of,
//! is `#[must_use]`.
//!
//! Follows rustc's `compiler/rustc_lint/src/unused/must_use.rs` (`UnusedResults::check_stmt`,
//! `is_ty_must_use`, `check_fn_must_use`, `emit_must_use_untranslated`) and
//! `compiler/rustc_lint/src/lints.rs` (`UnusedDef`, `UnusedDefSuggestion`) (MIT or
//! Apache-2.0). rustc gives the lint of the type first, then the one of the function, both
//! when both are `#[must_use]`.

use hir::{InFile, ModuleDef, PathResolution};
use hir_def::{Lookup as _, attrs::AttrFlags};
use syntax::{
    SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::UnusedMustUse<'db>) -> Vec<Found> {
    const LINT: LintId = LintId::named("unused_must_use");
    if !cx.lint_may_fire(LINT) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let node = cx.syntax_view(at);
    // alloc: empty, allocates nothing
    let Some(expr) = ast::Expr::cast(node.value) else { return Vec::new() };
    let span = cx.span_of(at);
    // Through the tails of blocks, as rustc's `check_stmt` goes: then the suggestion ends
    // the tail with `;`.
    let from_block = node.value.parent().is_some_and(|it| it.kind() == SyntaxKind::STMT_LIST);
    // alloc: the lint's parts, for a value hir found unused
    let mut found = Vec::new();
    if let Some(ty) = cx.vsema.type_of_expr(expr).map(|it| it.original)
        && let Some((adt, post)) = must_use_adt(cx, &ty, "")
    {
        let def = ModuleDef::Adt(adt);
        let reason = AttrFlags::must_use_message(cx.db, hir_def::AdtId::from(adt).into()).flatten();
        let diag = unused_def(cx, "", &post, def, reason, &span, post.is_empty().then_some(from_block));
        found.push(lint(cx, diag, at));
    }
    if let Some(function) = callee(cx, expr)
        && let Ok(id) = hir_def::FunctionId::try_from(function)
        && let Some(reason) = AttrFlags::must_use_message(cx.db, id.into())
    {
        let diag =
            unused_def(cx, "return value of ", "", ModuleDef::Function(function), reason, &span, Some(from_block));
        found.push(lint(cx, diag, at));
    }
    found
}

fn lint(cx: &Cx<'_, '_>, mut diag: Diag, at: InFile<SyntaxNodePtr>) -> Found {
    code_fact(cx, &mut diag, at);
    Found::lint(diag, Phase::Late, at)
}

/// rustc's `UnusedDef`: "unused {pre}`{def}`{post} that must be used", the attribute's reason
/// as a note, and when the value is the statement's own (`suggest` is whether it is a block's
/// tail), the suggestion to bind it to `_`.
fn unused_def(
    cx: &Cx<'_, '_>,
    pre: &str,
    post: &str,
    def: ModuleDef,
    reason: Option<&str>,
    span: &Span,
    suggest: Option<bool>,
) -> Diag {
    let path = cx.def_path(def);
    // alloc: clone: the lint's parts, for a value hir found unused
    let mut diag = Diag::lint("unused_must_use", format!("unused {pre}`{path}`{post} that must be used"), span.clone());
    if let Some(reason) = reason {
        diag.note(reason);
    }
    let message = "use `let _ = ...` to ignore the resulting value";
    match suggest {
        Some(true) => {
            diag.multipart_suggestion(
                message,
                // alloc: the lint's text, for a value hir found unused
                vec![(span.shrink_to_lo(), "let _ = ".to_owned()), (span.shrink_to_hi(), ";".to_owned())],
                Applicability::MaybeIncorrect,
            );
        }
        Some(false) => {
            diag.suggestion_verbose(span.shrink_to_lo(), message, "let _ = ", Applicability::MaybeIncorrect);
        }
        None => {}
    }
    match def {
        ModuleDef::Adt(adt) => {
            let key = cx.key(blaze_items::ItemId::Adt(adt.into()));
            // alloc: the lint's text, for a value hir found unused
            diag.fact(blaze_items::Fact::Signature(key), format!("the attributes of `{path}`"));
        }
        ModuleDef::Function(function) => {
            if let Ok(id) = hir_def::FunctionId::try_from(function) {
                let key = cx.key(blaze_items::ItemId::Function(id));
                // alloc: the lint's text, for a value hir found unused
                diag.fact(blaze_items::Fact::Signature(key), format!("the signature of `{path}`"));
            }
        }
        _ => {}
    }
    diag
}

/// The `#[must_use]` ADT a value of `ty` is one to use for, and what rustc says after its
/// name: `Result<T, E>` with `E` uninhabited is what `T` is, " in a `Result` with an
/// uninhabited error".
fn must_use_adt<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>, post: &str) -> Option<(hir::Adt, String)> {
    let adt = ty.as_adt()?;
    let two = |ty: &hir::Type<'db>| {
        let mut args = ty.type_arguments();
        let pair = (args.next()?, args.next()?);
        args.next().is_none().then_some(pair)
    };
    // `Result` and `ControlFlow` by their lang items, as hir's check found the value.
    let enum_ = match hir_def::AdtId::from(adt) {
        hir_def::AdtId::EnumId(it) => Some(it),
        _ => None,
    };
    let lang = hir_def::lang_item::lang_items(cx.db, cx.krate.base());
    let is = |variant: Option<hir_def::EnumVariantId>| {
        enum_.is_some() && variant.map(|it| it.lookup(cx.db).parent) == enum_
    };
    if is(lang.ResultOk)
        && let Some((ok, err)) = two(ty)
        && err.is_never()
    {
        // alloc: the lint's text, for a value hir found unused
        return must_use_adt(cx, &ok, &format!(" in a `Result` with an uninhabited error{post}"));
    }
    if is(lang.ControlFlowContinue)
        && let Some((brk, cont)) = two(ty)
        && brk.is_never()
    {
        // alloc: the lint's text, for a value hir found unused
        return must_use_adt(cx, &cont, &format!(" in a `ControlFlow` with an uninhabited break {post}"));
    }
    AttrFlags::must_use_message(cx.db, hir_def::AdtId::from(adt).into())?;
    // alloc: the lint's text, for a value hir found unused
    Some((adt, post.to_owned()))
}

/// The function a call or a method call calls, when it names one: rustc's
/// `check_fn_must_use` (a local holding a function names none).
fn callee(cx: &Cx<'_, '_>, expr: ast::Expr<'_>) -> Option<hir::Function> {
    match expr {
        ast::Expr::CallExpr(call) => {
            let ast::Expr::PathExpr(path) = call.expr()? else { return None };
            match cx.vsema.resolve_path(path.path()?)? {
                PathResolution::Def(ModuleDef::Function(it)) => Some(it),
                _ => None,
            }
        }
        ast::Expr::MethodCallExpr(call) => cx.vsema.resolve_method_call(call),
        _ => None,
    }
}
