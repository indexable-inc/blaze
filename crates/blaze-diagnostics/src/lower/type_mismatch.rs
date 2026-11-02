//! E0308 "mismatched types" in the forms rustc gives it by where the types had to agree: "expected
//! due to this" on a `let`'s type; "arguments to this function are incorrect" with the note of
//! where the function is defined; "expected `u32` because of return type"; a body with no tail
//! pointed at its return type, with "remove this semicolon to return this value"; "`match` arms
//! have incompatible types"; "`if` and `else` have incompatible types". It also gives the two
//! errors the front end reports as a mismatch of `()`: E0317, an `if` with no `else` where a
//! value is expected, and E0069, `return;` in a function that returns a value. With each:
//! rustc's "expected `u32`, found `&str`" of the types as inference had them at the error
//! ("expected integer, found `&str`" before literal fallback), the "expected struct `Foo<u8>` / found struct
//! `Foo<u16>`" note when the types are not simple, and the suggestions to borrow, dereference,
//! remove a borrow, convert with `.to_string()`, wrap in a variant, or add a return type.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/infer/mod.rs`
//! (`note_type_err`, `note_error_origin`, `note_expected_found_extra`), `suggest.rs`
//! (`could_remove_semicolon`, `consider_removing_semicolon`), `compiler/rustc_middle/src/ty/
//! error.rs` (`TypeError::Sorts`, `report_maybe_different`), `compiler/rustc_middle/src/ty/
//! sty.rs` (`is_simple_ty`, `is_simple_text`), `compiler/rustc_hir_typeck/src/_match.rs`
//! (`explain_if_expr`, `maybe_get_coercion_reason`), `coercion.rs` (`report_return_mismatched_
//! types`, `ReturnNoExpression`), `fn_ctxt/checks.rs` (`check_block_with_expected`,
//! `label_fn_like`), `fn_ctxt/suggestions.rs` (`suggest_missing_return_type`,
//! `suggest_deref_or_ref`, `suggest_compatible_variants`, the conversion methods),
//! `compiler/rustc_trait_selection/src/error_reporting/traits/suggestions.rs`
//! (`ty_kind_suggestion`) and `compiler/rustc_trait_selection/src/diagnostics.rs` (`NoElse`)
//! (MIT or Apache-2.0).
//!
//! The front end's `cause` says why the types had to agree where it is an `if`'s `else`, a
//! `match` arm after others, or a block's tail (whose local initialized by blocks gets rustc's
//! `suggest_turning_stmt_into_expr`); the rest (a `let`'s type, an argument, a return) is found
//! from the syntax around the expression, which the front end reports at the innermost tail of a
//! block as rustc does.
//!
//! Cost: nothing until type checking reports the mismatch. Then O(the syntax from the
//! expression up to the item, and a `match`'s earlier arms, each typed once): suggestions
//! compare types as interned ids, never printed.

use base_db::{CrateOrigin, LangCrateOrigin};
use hir::{AsAssocItem as _, InFile};
use syntax::{
    TextRange, TextSize,
    view::{self as ast, AstNode as _, HasArgList as _, HasName as _},
};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span, SuggestionStyle},
    print,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::TypeMismatch<'db>) -> Vec<Found> {
    let at = d.expr_or_pat.map(|it| it.syntax_node_ptr());
    let node = cx.syntax_view(at);
    let expr = ast::Expr::cast(node.value);
    let mut diag = if let Some(ast::Expr::IfExpr(it)) = expr
        && it.else_branch().is_none()
    {
        no_else(cx, d, node, it)
    } else if let Some(ast::Expr::ReturnExpr(it)) = expr
        && it.expr().is_none()
    {
        return_no_value(cx, d, node, it)
    } else if let Some(ast::Expr::BlockExpr(block)) = expr
        && block.stmt_list().is_some_and(|it| it.tail_expr().is_none())
        && let Some(item) = fn_of_body(block)
        && let Some(ret) = item.ret_type().and_then(|it| it.ty())
    {
        no_tail(cx, d, node, block, item, ret)
    } else {
        // alloc: empty, allocates nothing
        let Some(mut diag) = mismatch(cx, d, node) else { return vec![] };
        if !super::missing_fields::last_field_mismatch(cx, node, &mut diag) {
            // alloc: empty, allocates nothing
            return vec![];
        }
        diag
    };
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// Where the expression's type was checked against what it must be: rustc's cause codes, as
/// the syntax shows them.
enum Context<'db> {
    /// An arm after the first that does not diverge: `MatchExpressionArm`.
    /// `prior` is the arms before it that do not diverge, as `block_span` gives them.
    MatchArm { match_expr: ast::MatchExpr<'db>, prior: Vec<TextRange> },
    /// The `else` of an `if`: `IfExpression`.
    Else { if_expr: ast::IfExpr<'db> },
    /// The initializer of a `let` with a type.
    Let { ty: ast::Type<'db> },
    /// An argument of a call, its index among the written arguments.
    Arg { call: Call<'db>, index: usize },
    /// The value a function returns: its body's tail, or `return`'s value.
    Return { item: ast::Fn<'db> },
    Plain,
}

#[derive(Clone, Copy)]
enum Call<'db> {
    Call(ast::CallExpr<'db>),
    Method(ast::MethodCallExpr<'db>),
}

/// The mismatch's "mismatched types", none for an argument of a call that has more than this
/// error, which the call's `IncorrectArguments` reports.
fn mismatch<'db>(cx: &Cx<'_, 'db>, d: &hir::TypeMismatch<'db>, node: InFile<ast::Node<'db>>) -> Option<Diag> {
    let span = cx.span(node.map(|it| it.text_range()));
    let outer = outer_of(node.value);
    let context = context(cx, outer, d.cause);
    if let Context::Arg { call, .. } = &context {
        let call = match *call {
            Call::Call(it) => ast::Expr::CallExpr(it),
            Call::Method(it) => ast::Expr::MethodCallExpr(it),
        };
        if !super::incorrect_arguments::single_mismatch(cx, node.file_id, call) {
            return None;
        }
    }
    let message = match d.cause {
        hir::ObligationCauseCode::MatchExpressionArm { .. } => "`match` arms have incompatible types",
        hir::ObligationCauseCode::IfExpression => "`if` and `else` have incompatible types",
        _ => "mismatched types",
    };
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0308"), message, span.clone());
    // clone: on the error path, for an error hir found
    diag.span_label(span.clone(), print::sorts_at_error(cx, &d.expected, &d.actual, at_error(d)));
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    match &context {
        Context::MatchArm { match_expr, prior } => {
            let t = ty_at_error(cx, &d.expected, at_error(d).map(|(it, _)| it));
            let mut multiline = is_multiline(cx, &span);
            if prior.len() <= 4 {
                for &range in prior {
                    let at = in_file(range);
                    multiline |= is_multiline(cx, &at);
                    // alloc: the error's text, for an error hir found
                    diag.span_label(at, format!("this is found to be of type `{t}`"));
                }
            } else if let Some(&range) = prior.last() {
                let at = in_file(range);
                multiline |= is_multiline(cx, &at);
                // alloc: the error's text, for an error hir found
                diag.span_label(at, format!("this and all prior arms are found to be of type `{t}`"));
            }
            let whole = in_file(match_expr.syntax().text_range());
            let outer = match match_expr.expr() {
                Some(scrutinee) if multiline || !is_multiline(cx, &whole) => {
                    whole.shrink_to_lo().to(&in_file(scrutinee.syntax().text_range()))
                }
                _ => whole,
            };
            diag.span_label(outer, "`match` arms have incompatible types");
        }
        Context::Else { if_expr } => {
            let whole = in_file(if_expr.syntax().text_range());
            if let Some(ast::ElseBranch::IfExpr(inner)) = if_expr.else_branch()
                && inner.else_branch().is_none()
                && d.actual.is_unit()
            {
                diag.note("`if` expressions without `else` evaluate to `()`");
                diag.note("consider adding an `else` block that evaluates to the expected type");
            }
            let then = if_expr.then_branch().map(|it| in_file(block_span(ast::Expr::BlockExpr(it))));
            let else_ = if_expr.else_branch().map(|it| match it {
                ast::ElseBranch::Block(it) => in_file(block_span(ast::Expr::BlockExpr(it))),
                ast::ElseBranch::IfExpr(it) => in_file(it.syntax().text_range()),
            });
            if let Some(then) = &then {
                // clone: on the error path, for an error hir found
                diag.span_label(then.clone(), "expected because of this");
            }
            if is_multiline(cx, &whole) {
                let ends_with_branch = [&then, &else_].iter().any(|it| it.as_ref().is_some_and(|it| it.hi == whole.hi));
                let outer = match if_expr.condition() {
                    Some(cond) if ends_with_branch => whole.shrink_to_lo().to(&in_file(cond.syntax().text_range())),
                    _ => whole,
                };
                diag.span_label(outer, "`if` and `else` have incompatible types");
            }
        }
        Context::Let { ty } => {
            diag.span_label(in_file(ty.syntax().text_range()), "expected due to this");
        }
        Context::Arg { call, index } => arg_context(cx, node, *call, *index, &mut diag),
        Context::Return { item } => match item.ret_type().and_then(|it| it.ty()) {
            Some(ret) => {
                diag.span_label(
                    in_file(ret.syntax().text_range()),
                    // alloc: the error's text, for an error hir found
                    format!("expected `{}` because of return type", cx.ty(&d.expected)),
                );
            }
            None if d.expected.is_unit() => missing_return_type(cx, node, *item, &d.actual, &mut diag),
            None => {}
        },
        Context::Plain => {}
    }
    note_expected_found(cx, &d.expected, &d.actual, at_error(d), &mut diag);
    if d.cause == hir::ObligationCauseCode::BlockTailExpression {
        stmt_into_expr(cx, d, node, &mut diag);
    }
    if let Some(expr) = ast::Expr::cast(node.value) {
        suggest(cx, node.with_value(expr), &d.expected, &d.actual, &mut diag);
    }
    Some(diag)
}

/// The two types of the mismatch as inference had them at the error, expected then found.
fn at_error<'a>(d: &'a hir::TypeMismatch<'_>) -> Option<(&'a str, &'a str)> {
    d.at_error.as_ref().map(|(expected, found)| (expected.as_str(), found.as_str()))
}

/// `ty` printed, or as inference had it at the error where that holds an undecided literal's
/// type (`{integer}`, `Option<{float}>`), as rustc prints it.
fn ty_at_error<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>, at_error: Option<&str>) -> String {
    match at_error {
        // alloc: the error's text, for an error hir found
        Some(it) if print::holds_literal(it) => it.to_owned(),
        _ => cx.ty(ty),
    }
}

/// rustc's `note_expected_found`: the two types in full, aligned, unless both are simple.
pub(super) fn note_expected_found<'db>(
    cx: &Cx<'_, 'db>,
    expected: &hir::Type<'db>,
    found: &hir::Type<'db>,
    at_error: Option<(&str, &str)>,
    diag: &mut Diag,
) {
    if is_simple_text(cx, expected) && is_simple_text(cx, found) {
        return;
    }
    // alloc: the error's text, for an error hir found
    let e = format!("expected {}", print::prefix_string(cx, expected));
    // alloc: the error's text, for an error hir found
    let f = format!("found {}", print::prefix_string(cx, found));
    let width = e.len().max(f.len());
    // alloc: the error's text, for an error hir found
    diag.note(format!(
        "{:>width$} `{}`\n{:>width$} `{}`",
        e,
        ty_at_error(cx, expected, at_error.map(|(it, _)| it)),
        f,
        ty_at_error(cx, found, at_error.map(|(_, it)| it)),
    ));
}

/// rustc's `is_simple_ty`.
fn is_simple_ty<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> bool {
    if ty.as_builtin().is_some() || ty.is_unit() {
        return true;
    }
    let inner = match (ty.as_reference(), ty.as_slice()) {
        (Some((inner, _)), _) | (None, Some(inner)) => inner,
        (None, None) => match ty.as_array(cx.db) {
            Some((inner, _)) => inner,
            None => return false,
        },
    };
    is_simple_ty(cx, &inner.strip_references())
}

/// rustc's `is_simple_text`: a simple type, an ADT with no type arguments, or a reference to
/// one.
fn is_simple_text<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> bool {
    if ty.as_adt().is_some() {
        return ty.type_arguments().next().is_none();
    }
    if let Some((inner, _)) = ty.as_reference() {
        return is_simple_text(cx, &inner);
    }
    is_simple_ty(cx, ty)
}

/// The expression `node` is the value of, through the blocks it is the tail of.
fn outer_of(mut node: ast::Node<'_>) -> ast::Node<'_> {
    loop {
        let Some(list) = node.parent().and_then(ast::StmtList::cast) else { return node };
        if list.tail_expr().is_none_or(|it| it.syntax() != node) {
            return node;
        }
        let Some(block) = list.syntax().parent().and_then(ast::BlockExpr::cast) else { return node };
        if block.async_token().is_some()
            || block.const_token().is_some()
            || block.try_block_modifier().is_some()
            || block.gen_token().is_some()
        {
            return node;
        }
        node = block.syntax();
    }
}

fn context<'db>(cx: &Cx<'_, 'db>, outer: ast::Node<'db>, cause: hir::ObligationCauseCode) -> Context<'db> {
    let Some(parent) = outer.parent() else { return Context::Plain };
    if matches!(cause, hir::ObligationCauseCode::MatchExpressionArm { .. })
        && let Some(arm) = ast::MatchArm::cast(parent)
        && arm.expr().is_some_and(|it| it.syntax() == outer)
        && let Some(match_expr) = arm.syntax().parent().and_then(|it| it.parent()).and_then(ast::MatchExpr::cast)
    {
        let prior: Vec<TextRange> = match_expr
            .match_arm_list()
            .into_iter()
            .flat_map(|it| it.arms())
            .take_while(|it| *it != arm)
            .filter_map(|it| it.expr())
            .filter(|&expr| !cx.vsema.type_of_expr(expr).is_some_and(|it| it.original.is_never()))
            .map(block_span)
            // alloc: the error's parts, for an error hir found
            .collect();
        return if prior.is_empty() { Context::Plain } else { Context::MatchArm { match_expr, prior } };
    }
    if cause == hir::ObligationCauseCode::IfExpression
        && let Some(if_expr) = ast::IfExpr::cast(parent)
    {
        let is_else = match if_expr.else_branch() {
            Some(ast::ElseBranch::Block(it)) => it.syntax() == outer,
            Some(ast::ElseBranch::IfExpr(it)) => it.syntax() == outer,
            None => false,
        };
        return if is_else { Context::Else { if_expr } } else { Context::Plain };
    }
    if let Some(stmt) = ast::LetStmt::cast(parent)
        && stmt.initializer().is_some_and(|it| it.syntax() == outer)
        && let Some(ty) = stmt.ty()
    {
        return Context::Let { ty };
    }
    if let Some(list) = ast::ArgList::cast(parent)
        && let Some(index) = list.args().position(|it| it.syntax() == outer)
    {
        let call = match list.syntax().parent() {
            Some(it) if ast::CallExpr::can_cast(it.kind()) => ast::CallExpr::cast(it).map(Call::Call),
            Some(it) => ast::MethodCallExpr::cast(it).map(Call::Method),
            None => None,
        };
        return match call {
            Some(call) => Context::Arg { call, index },
            None => Context::Plain,
        };
    }
    if let Some(ret) = ast::ReturnExpr::cast(parent)
        && ret.expr().is_some_and(|it| it.syntax() == outer)
    {
        return match enclosing_fn(ret.syntax()) {
            Some(item) => Context::Return { item },
            None => Context::Plain,
        };
    }
    if let Some(block) = ast::BlockExpr::cast(outer)
        && let Some(item) = fn_of_body(block)
    {
        return Context::Return { item };
    }
    Context::Plain
}

/// The function `block` is the body of.
fn fn_of_body(block: ast::BlockExpr<'_>) -> Option<ast::Fn<'_>> {
    let item = block.syntax().parent().and_then(ast::Fn::cast)?;
    (item.body() == Some(block)).then_some(item)
}

/// The function a `return` returns from: the nearest, unless a closure or an async block is
/// nearer.
fn enclosing_fn(node: ast::Node<'_>) -> Option<ast::Fn<'_>> {
    for it in node.ancestors().skip(1) {
        if ast::ClosureExpr::can_cast(it.kind()) {
            return None;
        }
        if let Some(block) = ast::BlockExpr::cast(it)
            && block.async_token().is_some()
        {
            return None;
        }
        if let Some(item) = ast::Fn::cast(it) {
            return Some(item);
        }
        if ast::Item::can_cast(it.kind()) {
            return None;
        }
    }
    None
}

/// rustc's `find_block_span`: of a block, its tail, else its last statement, else itself.
fn block_span(expr: ast::Expr<'_>) -> TextRange {
    let ast::Expr::BlockExpr(block) = expr else { return expr.syntax().text_range() };
    let Some(list) = block.stmt_list() else { return expr.syntax().text_range() };
    if let Some(tail) = list.tail_expr() {
        return tail.syntax().text_range();
    }
    match list.statements().last() {
        Some(stmt) => stmt.syntax().text_range(),
        None => expr.syntax().text_range(),
    }
}

fn is_multiline(cx: &Cx<'_, '_>, span: &Span) -> bool {
    cx.snippet(span).is_some_and(|it| it.contains('\n'))
}

/// An argument that does not match its parameter: rustc's `report_arg_errors` with one error,
/// "arguments to this function are incorrect" on the callee and the note of where it is
/// defined, the parameter labelled.
fn arg_context<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, call: Call<'db>, index: usize, diag: &mut Diag) {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let (callee_range, callable) = match call {
        Call::Call(call) => {
            let Some(callee) = call.expr() else { return };
            (callee.syntax().text_range(), cx.vsema.resolve_expr_as_callable(callee))
        }
        Call::Method(call) => {
            let Some(name) = call.name_ref() else { return };
            let callable = cx.vsema.resolve_method_call_as_callable(call);
            (name.syntax().text_range(), callable)
        }
    };
    let Some(callable) = callable else { return };
    let (call_name, def_descr) = match callable.kind() {
        hir::CallableKind::Function(f) => {
            let descr = if f.has_self_param(cx.db) {
                "method"
            } else if f.as_assoc_item(cx.db).is_some() {
                "associated function"
            } else {
                "function"
            };
            (if matches!(call, Call::Method(_)) { "method" } else { "function" }, descr)
        }
        hir::CallableKind::TupleStruct(_) => ("struct", "tuple struct"),
        hir::CallableKind::TupleEnumVariant(_) => ("enum variant", "tuple variant"),
        _ => return,
    };
    // alloc: the error's text, for an error hir found
    diag.span_label(in_file(callee_range), format!("arguments to this {call_name} are incorrect"));
    let Some((name, params)) = def_params(cx, &callable) else { return };
    let mut at = MultiSpan::of(name);
    // A method called by its path takes its receiver as the first argument.
    let index = match callable.kind() {
        hir::CallableKind::Function(f) if f.has_self_param(cx.db) && matches!(call, Call::Call(_)) => {
            index.checked_sub(1).map_or(Some(None), |it| Some(Some(it)))
        }
        _ => Some(Some(index)),
    };
    match index {
        Some(Some(index)) => {
            if let Some(param) = params.params.get(index) {
                // alloc: the error's text, for an error hir found
                at.labels.push((param.clone(), String::new()));
            }
        }
        Some(None) => {
            if let Some(param) = &params.self_param {
                // alloc: the error's text, for an error hir found
                at.labels.push((param.clone(), String::new()));
            }
        }
        None => {}
    }
    // alloc: the error's text, for an error hir found
    diag.span_note(at, format!("{def_descr} defined here"));
}

struct Params {
    self_param: Option<Span>,
    params: Vec<Span>,
}

/// The name of what is called, and its parameters (or a tuple struct's or variant's fields).
fn def_params(cx: &Cx<'_, '_>, callable: &hir::Callable<'_>) -> Option<(Span, Params)> {
    match callable.kind() {
        hir::CallableKind::Function(f) => {
            let src = cx.vsema.source(f)?;
            let name = src.value.name()?;
            let list = src.value.param_list()?;
            let span = |range: TextRange| cx.span(src.with_value(range));
            Some((
                span(name.syntax().text_range()),
                Params {
                    self_param: list.self_param().map(|it| span(it.syntax().text_range())),
                    // alloc: the error's parts, for an error hir found
                    params: list.params().map(|it| span(it.syntax().text_range())).collect(),
                },
            ))
        }
        hir::CallableKind::TupleStruct(s) => {
            let src = cx.vsema.source(s)?;
            let name = src.value.name()?;
            let fields = match src.value.field_list()? {
                // alloc: the error's parts, for an error hir found
                ast::FieldList::TupleFieldList(it) => it.fields().map(|f| f.syntax().text_range()).collect(),
                // alloc: empty, allocates nothing
                ast::FieldList::RecordFieldList(_) => Vec::new(),
            };
            let span = |range: TextRange| cx.span(src.with_value(range));
            Some((
                span(name.syntax().text_range()),
                // alloc: the error's parts, for an error hir found
                Params { self_param: None, params: fields.into_iter().map(span).collect() },
            ))
        }
        hir::CallableKind::TupleEnumVariant(v) => {
            let src = cx.vsema.source(v)?;
            let name = src.value.name()?;
            let fields = match src.value.field_list()? {
                // alloc: the error's parts, for an error hir found
                ast::FieldList::TupleFieldList(it) => it.fields().map(|f| f.syntax().text_range()).collect(),
                // alloc: empty, allocates nothing
                ast::FieldList::RecordFieldList(_) => Vec::new(),
            };
            let span = |range: TextRange| cx.span(src.with_value(range));
            Some((
                span(name.syntax().text_range()),
                // alloc: the error's parts, for an error hir found
                Params { self_param: None, params: fields.into_iter().map(span).collect() },
            ))
        }
        _ => None,
    }
}

/// rustc's `suggest_missing_return_type` of a function with no return type whose tail is a
/// value: "try adding a return type", or of `main` and trait methods the label "expected `()`
/// because of default return type".
fn missing_return_type<'db>(
    cx: &Cx<'_, 'db>,
    node: InFile<ast::Node<'_>>,
    item: ast::Fn<'_>,
    found: &hir::Type<'db>,
    diag: &mut Diag,
) {
    let Some(params) = item.param_list() else { return };
    let at = cx.span(node.with_value(TextRange::empty(params.syntax().text_range().end())));
    let is_main = item.name().is_some_and(|it| it.text() == "main")
        && item.syntax().parent().is_some_and(|it| ast::SourceFile::can_cast(it.kind()));
    let in_trait_impl = item
        .syntax()
        .ancestors()
        .find_map(ast::Impl::cast)
        .is_some_and(|it| it.trait_().is_some());
    if is_main || in_trait_impl {
        diag.span_label(at, "expected `()` because of default return type");
    } else if found.contains_unknown() || found.is_closure() || found.as_callable(cx.db).is_some() {
        diag.suggestion(at, "a return type might be missing here", " -> _", Applicability::HasPlaceholders);
    } else {
        diag.suggestion(
            at,
            "try adding a return type",
            // alloc: the error's text, for an error hir found
            format!(" -> {}", cx.ty(found)),
            Applicability::MachineApplicable,
        );
    }
}

/// A function body with no tail where a value is to be returned: rustc points at the return
/// type, labels the function's name, and suggests removing the last statement's semicolon when
/// its value is of the type.
fn no_tail<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::TypeMismatch<'db>,
    node: InFile<ast::Node<'db>>,
    block: ast::BlockExpr<'db>,
    item: ast::Fn<'db>,
    ret: ast::Type<'db>,
) -> Diag {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let span = in_file(ret.syntax().text_range());
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0308"), "mismatched types", span.clone());
    diag.span_label(span, print::sorts_at_error(cx, &d.expected, &d.actual, at_error(d)));
    if let Some(name) = item.name() {
        diag.span_label(
            in_file(name.syntax().text_range()),
            "implicitly returns `()` as its body has no tail or `return` expression",
        );
    }
    remove_semicolon(cx, node.with_value(block), &d.expected, &mut diag);
    note_expected_found(cx, &d.expected, &d.actual, at_error(d), &mut diag);
    diag
}

/// rustc's `consider_removing_semicolon`: "remove this semicolon to return this value" when the
/// last statement of `block` is an expression whose value is of `expected`.
fn remove_semicolon<'db>(
    cx: &Cx<'_, 'db>,
    block: InFile<ast::BlockExpr<'db>>,
    expected: &hir::Type<'db>,
    diag: &mut Diag,
) {
    if let Some(ast::Stmt::ExprStmt(stmt)) = block.value.stmt_list().and_then(|it| it.statements().last())
        && let Some(semi) = stmt.semicolon_token()
        && let Some(expr) = stmt.expr()
        && let Some(ty) = cx.vsema.type_of_expr(expr)
        && ty.original.could_coerce_to(cx.db, expected)
    {
        diag.suggestion_with_style(
            // alloc: the error's text, for an error hir found
            vec![(cx.span(block.with_value(semi.text_range())), String::new())],
            "remove this semicolon to return this value",
            Applicability::MachineApplicable,
            SuggestionStyle::HideCodeInline,
        );
    }
}

/// rustc's `suggest_turning_stmt_into_expr`: a block whose tail of type `()` names a local whose
/// `let` is initialized by an `if`, `match` or block, each of whose blocks may have its last
/// statement's semicolon removed.
fn stmt_into_expr<'db>(cx: &Cx<'_, 'db>, d: &hir::TypeMismatch<'db>, node: InFile<ast::Node<'db>>, diag: &mut Diag) {
    // A `match` arm's cause is of the arm, not of a block.
    let in_block = node.value.parent().is_some_and(|it| ast::StmtList::can_cast(it.kind()));
    let in_arm = outer_of(node.value).parent().is_some_and(|it| ast::MatchArm::can_cast(it.kind()));
    if !d.actual.strip_references().is_unit() || !in_block || in_arm {
        return;
    }
    let Some(ast::Expr::PathExpr(path)) = ast::Expr::cast(node.value) else { return };
    let Some(hir::PathResolution::Local(local)) = path.path().and_then(|it| cx.vsema.resolve_path(it)) else {
        return;
    };
    let source = local.primary_source_view(cx.db);
    cx.root(source.file());
    let Some(init) = source.syntax().parent().and_then(ast::LetStmt::cast).and_then(|it| it.initializer()) else {
        return;
    };
    // alloc: the error's parts, for an error hir found
    let mut blocks = Vec::new();
    collect_blocks(init, &mut blocks);
    let expected = d.expected.strip_references();
    for block in blocks {
        remove_semicolon(cx, InFile::new(source.file(), block), &expected, diag);
    }
}

fn collect_blocks<'db>(expr: ast::Expr<'db>, blocks: &mut Vec<ast::BlockExpr<'db>>) {
    match expr {
        ast::Expr::IfExpr(it) => {
            let Some(else_) = it.else_branch() else { return };
            if let Some(then) = it.then_branch() {
                blocks.push(then);
            }
            match else_ {
                ast::ElseBranch::Block(it) => blocks.push(it),
                ast::ElseBranch::IfExpr(it) => collect_blocks(ast::Expr::IfExpr(it), blocks),
            }
        }
        ast::Expr::MatchExpr(it) => {
            for body in it.match_arm_list().into_iter().flat_map(|it| it.arms()).filter_map(|it| it.expr()) {
                collect_blocks(body, blocks);
            }
        }
        ast::Expr::BlockExpr(it) => blocks.push(it),
        _ => {}
    }
}

/// E0317: an `if` with no `else` where a value of another type than `()` is expected. The
/// front end has already undone rustc's `label_unit_as_expected` swap: `()` is what it found.
fn no_else<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::TypeMismatch<'db>,
    node: InFile<ast::Node<'_>>,
    if_expr: ast::IfExpr<'_>,
) -> Diag {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let span = in_file(if_expr.syntax().text_range());
    let (expected, found) = (&d.expected, &d.actual);
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0317"), "`if` may be missing an `else` clause", span.clone());
    diag.span_label(span, print::sorts_at_error(cx, expected, found, at_error(d)));
    match coercion_reason(cx, node, if_expr.syntax(), expected) {
        Some((at, label)) => {
            diag.span_label(at, label);
        }
        None => {
            if let Some(tail) = if_expr.then_branch().and_then(|it| it.stmt_list()).and_then(|it| it.tail_expr()) {
                diag.span_label(in_file(tail.syntax().text_range()), "found here");
            }
        }
    }
    diag.note("`if` expressions without `else` evaluate to `()`");
    diag.help("consider adding an `else` block that evaluates to the expected type");
    diag
}

/// rustc's `maybe_get_coercion_reason`: the return type of the function `node` is the body's
/// value of, or the `let` it initializes with a type.
fn coercion_reason<'db>(
    cx: &Cx<'_, 'db>,
    node: InFile<ast::Node<'_>>,
    expr: ast::Node<'_>,
    expected: &hir::Type<'db>,
) -> Option<(Span, String)> {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let outer = outer_of(expr);
    if let Some(block) = ast::BlockExpr::cast(outer)
        && let Some(item) = fn_of_body(block)
        && let Some(ret) = item.ret_type().and_then(|it| it.ty())
    {
        return Some((
            in_file(ret.syntax().text_range()),
            // alloc: the error's text, for an error hir found
            format!("expected `{}` because of this return type", cx.ty(expected)),
        ));
    }
    let stmt = outer.parent().and_then(ast::LetStmt::cast)?;
    stmt.ty()?;
    let pat = stmt.pat()?;
    // alloc: the error's text, for an error hir found
    Some((in_file(pat.syntax().text_range()), "expected because of this assignment".to_owned()))
}

/// E0069: `return;` in a function whose return type is not `()`.
fn return_no_value<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::TypeMismatch<'db>,
    node: InFile<ast::Node<'_>>,
    ret: ast::ReturnExpr<'_>,
) -> Diag {
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let span = in_file(ret.syntax().text_range());
    let expected = &d.expected;
    let mut diag =
        // clone: on the error path, for an error hir found
        Diag::error(Some("E0069"), "`return;` in a function whose return type is not `()`", span.clone());
    if let Some(item) = enclosing_fn(ret.syntax())
        && let Some(ty) = item.ret_type().and_then(|it| it.ty())
    {
        diag.span_label(
            in_file(ty.syntax().text_range()),
            // alloc: the error's text, for an error hir found
            format!("expected `{}` because of this return type", cx.ty(expected)),
        );
    }
    if let Some(value) = value_of(cx, expected) {
        diag.suggestion_verbose(
            span.shrink_to_hi(),
            "give the `return` a value of the expected type",
            // alloc: the error's text, for an error hir found
            format!(" {value}"),
            Applicability::HasPlaceholders,
        );
    }
    diag.span_label(span, "return type is not `()`");
    diag
}

/// rustc's `ty_kind_suggestion`: a value of the type to write.
fn value_of(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<String> {
    if ty.is_never() || ty.contains_unknown() {
        return None;
    }
    if ty.is_bool() {
        // alloc: the error's text, for an error hir found
        return Some("false".to_owned());
    }
    if ty.is_char() {
        // alloc: the error's text, for an error hir found
        return Some("'x'".to_owned());
    }
    if ty.is_int_or_uint() {
        // alloc: the error's text, for an error hir found
        return Some("42".to_owned());
    }
    if ty.is_float() {
        // alloc: the error's text, for an error hir found
        return Some("3.14159".to_owned());
    }
    if ty.is_slice() {
        // alloc: the error's text, for an error hir found
        return Some("[]".to_owned());
    }
    if let Some((inner, m)) = ty.as_reference() {
        if inner.is_str() && m == hir::Mutability::Shared {
            // alloc: the error's text, for an error hir found
            return Some("\"\"".to_owned());
        }
        let prefix = if m == hir::Mutability::Mut { "&mut " } else { "&" };
        // alloc: the error's text, for an error hir found
        return value_of(cx, &inner).map(|it| format!("{prefix}{it}"));
    }
    if ty.is_unit() {
        // alloc: the error's text, for an error hir found
        return Some("()".to_owned());
    }
    if let Some(adt) = ty.as_adt() {
        let name = adt.name(cx.db);
        let std = matches!(
            adt.module(cx.db).krate(cx.db).origin(cx.db),
            CrateOrigin::Lang(_)
        );
        let mut args = ty.type_arguments();
        return match (std, name.as_str()) {
            // alloc: the error's text, for an error hir found
            (true, "Vec") => Some("vec![]".to_owned()),
            // alloc: the error's text, for an error hir found
            (true, "String") => Some("String::new()".to_owned()),
            // alloc: the error's text, for an error hir found
            (true, "Box") => args.next().and_then(|it| value_of(cx, &it)).map(|it| format!("Box::new({it})")),
            // alloc: the error's text, for an error hir found
            (true, "Option") => Some("None".to_owned()),
            // alloc: the error's text, for an error hir found
            (true, "Result") => args.next().and_then(|it| value_of(cx, &it)).map(|it| format!("Ok({it})")),
            _ => None,
        };
    }
    None
}

/// rustc's suggestions for an expression of the wrong type: borrow it, dereference it, remove
/// its borrow, convert it with `.to_string()`, or wrap it in the variant of the expected enum
/// that holds its type.
fn suggest<'db>(
    cx: &Cx<'_, 'db>,
    expr: InFile<ast::Expr<'_>>,
    expected: &hir::Type<'db>,
    found: &hir::Type<'db>,
    diag: &mut Diag,
) {
    let span = cx.span(expr.map(|it| it.syntax().text_range()));
    if expr.file_id.is_macro() {
        return;
    }
    if let ast::Expr::Literal(lit) = expr.value
        && let Some(suffix) = numeric_suffix(lit)
        && (expected.is_int_or_uint() && found.is_int_or_uint() || expected.is_float() && found.is_float())
    {
        let token = lit.token().text_range();
        let at = TextRange::new(token.end() - suffix, token.end());
        diag.suggestion_verbose(
            cx.span(expr.with_value(at)),
            // alloc: the error's text, for an error hir found
            format!(
                "change the type of the numeric literal from `{}` to `{}`",
                cx.ty(found),
                cx.ty(expected)
            ),
            cx.ty(expected),
            Applicability::MachineApplicable,
        );
        return;
    }
    if let Some((inner, m)) = expected.as_reference()
        && inner == *found
    {
        let (prefix, how) = if m == hir::Mutability::Mut { ("&mut ", "mutably ") } else { ("&", "") };
        let code = if needs_parens(expr.value) {
            // alloc: the error's text, for an error hir found
            vec![(span.shrink_to_lo(), format!("{prefix}(")), (span.shrink_to_hi(), ")".to_owned())]
        } else {
            // alloc: the error's text, for an error hir found
            vec![(span.shrink_to_lo(), prefix.to_owned())]
        };
        diag.suggestion_with_style(
            code,
            // alloc: the error's text, for an error hir found
            format!("consider {how}borrowing here"),
            Applicability::MachineApplicable,
            SuggestionStyle::ShowCode,
        );
        return;
    }
    if let Some((inner, _)) = found.as_reference()
        && inner == *expected
    {
        if let ast::Expr::RefExpr(r) = expr.value
            && let Some(operand) = r.expr()
        {
            let end = operand.syntax().text_range().start();
            let start = r.syntax().text_range().start();
            diag.suggestion_with_style(
                // alloc: the error's text, for an error hir found
                vec![(cx.span(expr.with_value(TextRange::new(start, end))), String::new())],
                "consider removing the borrow",
                Applicability::MachineApplicable,
                SuggestionStyle::ShowAlways,
            );
            return;
        }
        if inner.is_copy(cx.db) {
            diag.suggestion_with_style(
                // alloc: the error's text, for an error hir found
                vec![(span.shrink_to_lo(), "*".to_owned())],
                "consider dereferencing the borrow",
                Applicability::MachineApplicable,
                SuggestionStyle::ShowCode,
            );
            return;
        }
    }
    if is_string(cx, expected)
        && let Some((inner, _)) = found.as_reference()
        && inner.is_str()
    {
        diag.suggestion_with_style(
            // alloc: the error's text, for an error hir found
            vec![(span.shrink_to_hi(), ".to_string()".to_owned())],
            "try using a conversion method",
            Applicability::MaybeIncorrect,
            SuggestionStyle::ShowCode,
        );
        return;
    }
    wrap_in_variant(cx, &span, expected, found, diag);
}

/// rustc's `suggest_compatible_variants` of one variant: "try wrapping the expression in
/// `Some`".
fn wrap_in_variant<'t>(
    cx: &Cx<'_, '_>,
    span: &Span,
    expected: &hir::Type<'t>,
    found: &hir::Type<'t>,
    diag: &mut Diag,
) {
    let Some(hir::Adt::Enum(e)) = expected.as_adt() else { return };
    // alloc: the error's parts, for an error hir found
    let args: Vec<hir::Type<'_>> = expected.type_arguments().collect();
    let mut compatible = e.variants_iter(cx.db).filter(|v| {
        if matches!(v.kind(cx.db), hir::StructKind::Tuple)
            && let [field] = v.fields(cx.db).as_slice()
        {
            field.ty(cx.db).instantiate_type_params(cx.db, args.iter()) == *found
        } else {
            false
        }
    });
    let (Some(variant), None) = (compatible.next(), compatible.next()) else { return };
    let enum_name = e.name(cx.db);
    let in_prelude = matches!(enum_name.as_str(), "Option" | "Result")
        && matches!(e.module(cx.db).krate(cx.db).origin(cx.db), CrateOrigin::Lang(_));
    let variant_name = variant.name(cx.db);
    let path = if in_prelude {
        // alloc: the error's text, for an error hir found
        variant_name.as_str().to_owned()
    } else {
        // alloc: the error's text, for an error hir found
        format!("{}::{}", enum_name.as_str(), variant_name.as_str())
    };
    diag.multipart_suggestion(
        // alloc: the error's text, for an error hir found
        format!("try wrapping the expression in `{path}`"),
        // alloc: the error's text, for an error hir found
        vec![(span.shrink_to_lo(), format!("{path}(")), (span.shrink_to_hi(), ")".to_owned())],
        Applicability::MaybeIncorrect,
    );
}

/// The length of a numeric literal's type suffix (`f64` of `1f64`).
fn numeric_suffix(lit: ast::Literal<'_>) -> Option<TextSize> {
    let suffix = match lit.kind() {
        ast::LiteralKind::IntNumber(it) => it.suffix().map(str::len),
        ast::LiteralKind::FloatNumber(it) => it.suffix().map(str::len),
        _ => None,
    }?;
    TextSize::try_from(suffix).ok()
}

fn is_string(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    match ty.as_adt() {
        Some(hir::Adt::Struct(it)) => {
            it.name(cx.db).as_str() == "String"
                && it.module(cx.db).krate(cx.db).origin(cx.db)
                    == CrateOrigin::Lang(LangCrateOrigin::Alloc)
        }
        _ => false,
    }
}

/// Whether `&` before the expression would bind tighter than it: rustc's `needs_parens` by
/// precedence, of the expressions that bind looser than a prefix operator.
fn needs_parens(expr: ast::Expr<'_>) -> bool {
    expr.precedence() < ast::prec::ExprPrecedence::Prefix
}