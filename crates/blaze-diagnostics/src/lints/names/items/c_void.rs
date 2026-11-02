//! `c_void_returns`: `core::ffi::c_void` written as the return type of a function with a body,
//! a closure, a foreign function or a `fn` pointer type. A free function of the Rust ABI, or a
//! `fn` pointer of it, gets "should not be used as a return type"; the others (methods, closures,
//! foreign functions and other ABIs) that it is not C's `void`.
//!
//! Follows `compiler/rustc_lint/src/c_void_returns.rs`, `compiler/rustc_span/src/source_map.rs`
//! (`span_extend_to_prev_char`) and `compiler/rustc_lint/src/lints.rs` (`CVoidReturn`,
//! `ExternCVoidReturn`) (MIT or Apache-2.0).

use hir::{InFile, ModuleDef, PathResolution};
use rustc_abi::ExternAbi;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode},
};

use super::{
    Krate,
    externs::abi_of,
    walk::{Container, Item},
};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// The lints of `item`'s own nodes, in the order rustc's visitor meets them: its `check_fn`,
/// then the closures and `fn` pointer types in it.
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, krate: &Krate) -> Vec<Found> {
    let Some(c_void) = krate.c_void else { return Vec::new() }; // alloc: empty, allocates nothing
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    for &node in item.nodes() {
        let kind = node.value.kind();
        if !matches!(kind, SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR | SyntaxKind::FN_PTR_TYPE) {
            continue;
        }
        let at = node.map(ast::ptr);
        if kind == SyntaxKind::FN
            && let Some(f) = ast::Fn::cast(node.value)
        {
            let extern_ = match item.container {
                Container::Extern(_) => true,
                Container::Module => {
                    if f.body().is_none() {
                        continue;
                    }
                    abi_of(f.abi(), ExternAbi::Rust) != Some(ExternAbi::Rust)
                }
                Container::Impl(_) | Container::Trait(_) => {
                    if f.body().is_none() {
                        continue;
                    }
                    true
                }
            };
            let Some(ret) = f.ret_type() else { continue };
            check(cx, node.with_value(ret), f.param_list().map(|it| it.syntax().text_range().end()), extern_, c_void, at, &mut found);
        } else if kind == SyntaxKind::CLOSURE_EXPR
            && let Some(closure) = ast::ClosureExpr::cast(node.value)
        {
            let Some(ret) = closure.ret_type() else { continue };
            check(cx, node.with_value(ret), None, true, c_void, at, &mut found);
        } else if let Some(ptr) = ast::FnPtrType::cast(node.value) {
            let Some(ret) = ptr.ret_type() else { continue };
            let extern_ = abi_of(ptr.abi(), ExternAbi::Rust) != Some(ExternAbi::Rust);
            let params = ptr.param_list().map(|it| it.syntax().text_range().end());
            check(cx, node.with_value(ret), params, extern_, c_void, at, &mut found);
        }
    }
    found
}

/// rustc's `check_decl` of a return type, at `at`.
fn check<'db>(
    cx: &Cx<'_, 'db>,
    ret: InFile<ast::RetType<'db>>,
    params_end: Option<TextSize>,
    extern_: bool,
    c_void: hir::Enum,
    at: InFile<SyntaxNodePtr>,
    found: &mut Vec<Found>,
) {
    let Some(ast::Type::PathType(ty)) = ret.value.ty() else { return };
    let Some(path) = ty.path() else { return };
    let Some(PathResolution::Def(ModuleDef::Adt(hir::Adt::Enum(it)))) = cx.vsema.resolve_path(path) else { return };
    if it != c_void {
        return;
    }
    let range = ty.syntax().text_range();
    let suggestion = ret.with_value(extend_to_prev_paren(ret.value, range, params_end));
    let span = cx.span(ret.with_value(range));
    let mut diag = match extern_ {
        false => Diag::lint("c_void_returns", "`c_void` should not be used as a return type", span),
        true => Diag::lint(
            "c_void_returns",
            "declarations returning `c_void` are not compatible with C functions returning `void`",
            span,
        ),
    };
    diag.suggestion(cx.span(suggestion), "remove the return type to implicitly return `()`", "", Applicability::MaybeIncorrect);
    diag.help("returning `()` in Rust is equivalent to returning `void` in C");
    if extern_ {
        diag.note("`c_void` is only used through raw pointers for compatibility with `void` pointers");
    }
    crate::lints::code_fact(cx, &mut diag, at);
    found.push(Found::lint(diag, Phase::Late, at));
}

/// `span_extend_to_prev_char(range, ')', true)`: `range` from just after the `)` before it; when
/// the parameters' `)` is right before it (but for white space and `->`), that one, without
/// reading the file.
fn extend_to_prev_paren(ret: ast::RetType<'_>, range: TextRange, params_end: Option<TextSize>) -> TextRange {
    if let Some(end) = params_end {
        return TextRange::new(end, range.end());
    }
    let root = ret.syntax().ancestors().last().unwrap_or(ret.syntax());
    let before = &root.text()[..usize::from(range.start())];
    let after = before.rsplit(')').next().unwrap_or("");
    if after.is_empty() {
        return range;
    }
    TextRange::new(range.start() - TextSize::of(after), range.end())
}
