//! `MapUnitFn`: `map_unit_fn`, a statement `slice.iter_mut().map(f);` whose `f` returns `()`
//! or `!`, which maps lazily and is dropped unused.
//!
//! Follows rustc's `compiler/rustc_lint/src/map_unit_fn.rs`, `compiler/rustc_lint/src/lints.rs`
//! (`MappingToUnit`) and `compiler/rustc_middle/src/ty/mod.rs` (`span_of_impl`: a local
//! item's `def_span`, else nothing) (MIT or Apache-2.0).

use base_db::Db;
use hir::InFile;
use hir_def::{
    CallableDefId, FunctionId, HasModule as _, ItemContainerId, Lookup as _,
    expr_store::ExpressionStore,
    hir::{Expr, ExprId, Statement},
    src::HasSource as _,
};
use hir_ty::{
    InferBodyId,
    db::{HirDatabase, InternedClosure},
    next_solver::{CallableIdWrapper, Ty, TyKind},
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::signature_range_view,
    lints::bodies::{
        BodyCx, Hooks, LatePass, LintSet, lint_set,
        types::util::{self, fn_item, from_expansion, method_fn},
    },
    lower::{Found, Phase, break_outside_of_loop::up},
    model::{Applicability, Diag, Span},
};

#[derive(Default)]
pub(crate) struct MapUnitFn;

impl LatePass for MapUnitFn {
    const LINTS: LintSet = lint_set(["map_unit_fn"]);
    const HOOKS: Hooks = Hooks::STMT;

    fn check_stmt(&mut self, b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize, out: &mut Vec<Found>) {
        if let Some(found) = map_unit_fn(b, block, index) {
            out.push(found);
        }
    }
}

fn map_unit_fn(b: &BodyCx<'_, '_, '_>, block: ExprId, index: usize) -> Option<Found> {
    let Expr::Block { statements, .. } = b.store().expr(block) else { return None };
    let Statement::Expr { expr, has_semi: true } = statements.get(index)? else { return None };
    let Expr::MethodCall { receiver, method_name, args: &[arg], .. } = b.store().expr(expr) else { return None };
    if method_name.as_str() != "map"
        || !method_fn(b, expr).and_then(|it| fn_item(b.db(), it)).is_some_and(|it| it.as_str() == "IteratorMap")
    {
        return None;
    }
    let node = util::syntax(b, expr)?;
    let stmt = up(b.cx, node).find(|it| it.value.kind() == SyntaxKind::EXPR_STMT)?;
    if from_expansion(&b.cx.span(stmt.as_ref().map(|it| it.text_range())))
        || from_expansion(&b.expr_span(receiver)?)
        || from_expansion(&b.expr_span(arg)?)
        || !is_impl_slice(b, receiver)
    {
        return None;
    }
    let arg_ty = b.infer.expr_ty(arg);
    let ret_ty = match arg_ty.kind() {
        TyKind::Closure(_, args) => args.as_closure().sig().skip_binder().output(),
        TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(function)), _) => {
            let sig = b.db().callable_item_signature(CallableDefId::FunctionId(function));
            sig.skip_binder().skip_binder().output()
        }
        _ => return None,
    };
    if !returns_nothing(ret_ty) {
        return None;
    }
    let function_label = match arg_ty.kind() {
        TyKind::Closure(closure, _) => closure_span(b, closure.0.loc(b.db())),
        TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(function)), _) => fn_span(b, function),
        _ => None,
    };

    // clone: a node handle or span, once the lint fires
    let call = ast::MethodCallExpr::cast(node.value)?;
    let name = call.name_ref()?;
    let name_span = b.cx.span(node.with_value(name.syntax().text_range()));
    let span = b.cx.span(node.with_value(TextRange::new(name.syntax().text_range().start(), node.value.text_range().end())));
    let arg_span = util::span(b, arg)?;
    // clone: a node handle or span, once the lint fires
    let mut diag = Diag::lint("map_unit_fn", "`Iterator::map` call that discard the iterator's values", span.clone());
    diag.note("`Iterator::map`, like many of the methods on `Iterator`, gets executed lazily, meaning that its effects won't be visible until it is iterated");
    diag.span_label(
        // clone: a node handle or span, once the lint fires
        function_label.unwrap_or_else(|| arg_span.clone()),
        "this function returns `()`, which is likely not what you wanted",
    );
    diag.span_label(arg_span, "called `Iterator::map` with callable that returns `()`");
    diag.span_label(span, "after this call to map, the resulting iterator is `impl Iterator<Item = ()>`, which means the only information carried by the iterator is the number of items");
    diag.suggestion_verbose(name_span, "you might have meant to use `Iterator::for_each`", "for_each", Applicability::MaybeIncorrect);
    if let Some(written) = util::syntax(b, receiver).and_then(|it| ast::Expr::cast(it.value))
        && let Some(receiver_ty) = b.cx.vsema.type_of_expr(written)
    {
        let printed = b.cx.ty(&receiver_ty.original);
        // alloc: the lint's diagnostic
        diag.fact_opt(b.cx.method_fact(&receiver_ty.original, "map"), format!("what `map` is on `{printed}`"));
    }
    Some(b.lint(diag, Phase::Late, stmt.map(ast::ptr)))
}

/// rustc's `ret_ty.is_unit() || ret_ty.is_never()`.
fn returns_nothing(ty: Ty<'_>) -> bool {
    ty.is_unit() || ty.is_never()
}

/// rustc's `is_impl_slice`: `receiver` is a method call of a method of an inherent impl on a
/// slice (`<[T]>::iter_mut`).
fn is_impl_slice(b: &BodyCx<'_, '_, '_>, receiver: ExprId) -> bool {
    let db = b.db();
    let Some(method) = method_fn(b, receiver) else { return false };
    let ItemContainerId::ImplId(impl_id) = method.lookup(db).container else { return false };
    db.impl_self_ty(impl_id).skip_binder().is_slice()
}

/// rustc's `span_of_impl` of a function: its `def_span` (its signature) when it is of this
/// crate.
fn fn_span(b: &BodyCx<'_, '_, '_>, function: FunctionId) -> Option<Span> {
    let db = b.db();
    if hir::Crate::from(function.krate(db)) != b.cx.krate {
        return None;
    }
    let source = function.lookup(db).source_view(db);
    Some(b.cx.span(source.map(signature_range_view)))
}

/// rustc's `span_of_impl` of a closure (always of this crate): its `def_span`, the closure's
/// header from `move` (or its first token after its attributes) to its parameters' `|` or its
/// return type.
fn closure_span(b: &BodyCx<'_, '_, '_>, closure: InternedClosure<'_>) -> Option<Span> {
    let db = b.db();
    let ptr = match closure.owner {
        InferBodyId::DefWithBodyId(owner) if owner == b.owner => b.expr_node(closure.expr)?,
        owner => expr_node(db, owner, closure.expr)?,
    };
    let node = b.cx.syntax_view(ptr);
    let written = ast::ClosureExpr::cast(node.value)?;
    let start = node
        .value
        .children_with_tokens()
        .find(|it| !matches!(it.kind(), SyntaxKind::ATTR | SyntaxKind::WHITESPACE | SyntaxKind::COMMENT))?
        .text_range()
        .start();
    let end = match written.ret_type() {
        Some(ret) => ret.syntax().text_range().end(),
        None => written.param_list()?.syntax().text_range().end(),
    };
    Some(b.cx.span(node.with_value(TextRange::new(start, end))))
}

/// The node an expression of another body is written as.
fn expr_node(db: &Db, owner: InferBodyId<'_>, expr: ExprId) -> Option<InFile<SyntaxNodePtr>> {
    let (_, source_map) = ExpressionStore::with_source_map(db, owner.expression_store_owner(db));
    let source = source_map.expr_syntax(expr).ok()?;
    Some(source.map(|it| it.syntax_node_ptr()))
}
