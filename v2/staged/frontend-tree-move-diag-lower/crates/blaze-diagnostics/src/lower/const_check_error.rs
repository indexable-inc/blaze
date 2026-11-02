//! What rustc's const checker refuses in a `const fn`, a `const` or a `static` (#1440): E0015
//! for a call of a function that is not `const` or for inline assembly, and the errors it
//! gives no code (a function pointer call, a non-const intrinsic, a pointer cast to an integer
//! or compared).
//!
//! Follows rustc's `compiler/rustc_const_eval/src/check_consts/ops.rs` (`FnCallNonConst`,
//! `build_error_for_const_call`, `FnCallIndirect`, `IntrinsicNonConst`, `InlineAsm`,
//! `RawPtrComparison`, `RawPtrToIntCast`) and `compiler/rustc_const_eval/src/diagnostics.rs`
//! (MIT or Apache-2.0). hir gives the message as `hir-ty-mir`'s const checker
//! (`crates/hir-ty-mir/src/mir/const_check.rs`) writes it; the notes are rustc's for it.

use blaze_items::{Fact, ItemId};
use hir::{AsAssocItem, InFile, ModuleDef, PathResolution};
use syntax::{
    SyntaxKind, SyntaxNodePtr,
    view::{self as ast, AstNode as _, HasGenericParams as _},
};

use crate::{
    cx::{Cx, signature_range_view as signature_range},
    lints::code_fact,
    lower::{Found, Phase, break_outside_of_loop::up},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ConstCheckError) -> Vec<Found> {
    let code = (!d.code.is_empty()).then_some(d.code);
    // clone: the message hir owns, once per diagnostic (the edge).
    let mut diag = Diag::error(code, d.message.clone(), cx.span_of(d.node));
    let message = d.message.as_str();
    if message.starts_with("cannot call non-const ") {
        non_const_call(cx, &mut diag, d.node, message);
    } else if message == "pointers cannot be cast to integers during const eval" {
        diag.note("at compile-time, pointers do not have an integer value");
        diag.note(
            "avoiding this restriction via `transmute`, `union`, or raw pointers leads to compile-time undefined behavior",
        );
    } else if message == "pointers cannot be reliably compared during const eval" {
        diag.note("see issue #53020 <https://github.com/rust-lang/rust/issues/53020> for more information");
    }
    code_fact(cx, &mut diag, d.node);
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::new(diag, phase(cx, d.node))]
}

/// When rustc checks the MIR of the body `at` is in: a free `const` or `static` as it
/// evaluates it, in its type checking of the crate's bodies (`rustc_hir_analysis::check_crate`),
/// an enum's discriminant in its well-formedness check, and the rest (a `const fn`, an
/// associated or generic `const`) in its borrow checking of each body.
pub(crate) fn phase(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>) -> Phase {
    for node in up(cx, cx.syntax_view(at)) {
        let node = node.value;
        if ast::Fn::can_cast(node.kind()) {
            return Phase::Mir;
        }
        if ast::Const::can_cast(node.kind()) {
            let associated = node.parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST);
            let generic = ast::Const::cast(node).is_some_and(|it| it.generic_param_list().is_some());
            return match associated || generic {
                true => Phase::Mir,
                false => Phase::Bodies,
            };
        }
        if ast::Static::can_cast(node.kind()) {
            return Phase::Bodies;
        }
        if ast::Variant::can_cast(node.kind()) {
            return Phase::Items;
        }
    }
    Phase::Mir
}

/// rustc's `FnCallNonConst`: where the function called is not `const` when it is a free
/// function (a method of an inherent `impl` gets no note), the limit on calls, and for a
/// `static` the suggestion of `LazyLock`.
fn non_const_call(cx: &Cx<'_, '_>, diag: &mut Diag, node: InFile<SyntaxNodePtr>, message: &str) {
    if message.starts_with("cannot call non-const intrinsic ") {
        return;
    }
    let callee = ast::Expr::cast(cx.syntax_view(node).value).and_then(|expr| callee(cx, expr));
    if let Some(callee) = callee
        && let Ok(id) = hir_def::FunctionId::try_from(callee)
    {
        // alloc: the callee's name for the notes, once per diagnostic (the edge).
        let name = callee.name(cx.db).display(cx.db, cx.edition).to_string();
        let key = cx.key(ItemId::Function(id));
        // alloc: the fact's text, once per diagnostic (the edge).
        diag.fact(Fact::Signature(key), format!("the signature of `{name}`"));
        if callee.as_assoc_item(cx.db).is_none()
            && let Some(source) = cx.vsema.source(callee)
        {
            let at = source.with_value(signature_range(source.value));
            // alloc: the note's text, once per diagnostic (the edge).
            diag.span_note(cx.span(at), format!("function `{name}` is not const"));
        }
    }
    let Some((_, kind)) = message.rsplit_once(" in ") else { return };
    // alloc: the note's text, once per diagnostic (the edge).
    diag.note(format!("calls in {kind} are limited to constant functions, tuple structs and tuple variants"));
    if kind == "statics" {
        diag.note("consider wrapping this expression in `std::sync::LazyLock::new(|| ...)`");
    }
}

/// The function a call or a method call calls.
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
