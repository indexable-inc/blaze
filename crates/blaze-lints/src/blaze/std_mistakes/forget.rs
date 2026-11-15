//! `forget`: a value kept from being dropped with nothing to drop it later. `std::mem::forget`
//! itself is banned by `.clippy.toml` (`disallowed_methods`, replaced by `ManuallyDrop::new`,
//! which says what it keeps); this lint finds what that ban moves the leak to:
//!
//! - `ManuallyDrop::new(x)` whose value the function throws away (a statement, `let _`), or binds
//!   to a local that it neither hands on (moves: returns it, passes it, stores it) nor ends with
//!   `ManuallyDrop::drop`, `ManuallyDrop::into_inner` or `ManuallyDrop::take`. What `x` owns
//!   (memory, a file, a lock) is never given back, and a `Drop` that flushes never runs.
//! - `Box::leak(..)` outside the initializer of a `static`, of a value whose type is not
//!   `#[blaze::program_lifetime]`: memory given up for the rest of the process, once per call.
//!   A `static`'s initializer runs once; a program-lifetime type says every instance lives that
//!   long, and `blaze::program_lifetime` checks the claim.
//!
//! Outside test code. Narrowed: a `ManuallyDrop` handed on is not followed (the place it went
//! may drop it), and a `Box::leak` run once by a guard the lint can't see (a `Once`, a flag) is
//! still found: say so with `#[expect(blaze::forget, reason = "..")]`. No fix: where the value
//! should be dropped is the author's choice.

use hir_def::AdtId;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::{body_of, bound, discarded, in_test, inherent_of, parent_expr, report, uses_of};
use crate::{blaze::Lint, check::Cx};

const LINT: Lint = Lint::Forget;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(call) = ast::CallExpr::cast(node) else { return };
    let Some(f) = super::called(cx, call) else { return };
    let name = f.name(cx.db());
    let (message, note) = match name.as_str() {
        "new" if inherent_of(cx, f, &["core", "mem", "ManuallyDrop"]) => {
            if !never_dropped(cx, node) {
                return;
            }
            (
                "this `ManuallyDrop` is never dropped, taken apart or handed on, so what it holds leaks",
                "end it in this function with `ManuallyDrop::drop`, `into_inner` or `take`, or hand it to the owner that does",
            )
        }
        "leak" if inherent_of(cx, f, &["alloc", "boxed", "Box"]) => {
            if in_static(node) || program_lifetime(cx, call) {
                return;
            }
            (
                "`Box::leak` gives this memory up for the rest of the process, once per call",
                "leak only in a `static`'s initializer or for a `#[blaze::program_lifetime]` type; else keep the `Box` with the owner that drops it",
            )
        }
        _ => return,
    };
    if in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    report(cx, LINT, node, range, message.to_owned(), note, None);
}

/// Whether the `ManuallyDrop` `node` makes is thrown away, or bound to a local the function
/// neither moves nor ends.
fn never_dropped<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    if discarded(cx, node) {
        return true;
    }
    let Some(stmt) = parent_expr(node).and_then(ast::LetStmt::cast) else { return false };
    let Some(local) = stmt.pat().and_then(|it| bound(cx, it)) else { return false };
    let Some(body) = body_of(node, false) else { return false };
    let ends = ["drop", "into_inner", "take"];
    for path in uses_of(cx, body, local) {
        let mut at = path.syntax();
        // `ManuallyDrop::drop(&mut m)`: the use under its `&mut`.
        if let Some(parent) = parent_expr(at)
            && parent.kind() == SyntaxKind::REF_EXPR
        {
            at = parent;
        }
        let Some(parent) = parent_expr(at) else { return false };
        match parent.kind() {
            SyntaxKind::ARG_LIST => {
                let call = parent.parent().and_then(ast::CallExpr::cast);
                let ended = call.and_then(|it| super::called(cx, it)).is_some_and(|f| {
                    ends.contains(&f.name(cx.db()).as_str())
                        && inherent_of(cx, f, &["core", "mem", "ManuallyDrop"])
                });
                // Handed to any other call by value: moved on. By reference: only lent.
                if ended || at == path.syntax() {
                    return false;
                }
            }
            SyntaxKind::METHOD_CALL_EXPR | SyntaxKind::FIELD_EXPR | SyntaxKind::INDEX_EXPR => {}
            SyntaxKind::PREFIX_EXPR | SyntaxKind::REF_EXPR => {}
            // Returned, stored, assigned, put in a tuple or a struct: moved on.
            _ => return false,
        }
    }
    true
}

/// Whether `node` is in the initializer of a `static` (a `LazyLock`'s closure there included).
fn in_static(node: ast::Node<'_>) -> bool {
    node.ancestors().any(|it| it.kind() == SyntaxKind::STATIC)
}

/// Whether `call`, `Box::leak(..)`, leaks a value whose type is `#[blaze::program_lifetime]`.
fn program_lifetime<'t>(cx: &Cx<'_, 't>, call: ast::CallExpr<'t>) -> bool {
    let Some(ty) = cx.ty(ast::Expr::CallExpr(call)) else { return false };
    let Some(adt) = ty.strip_references().as_adt() else { return false };
    hir_ty_pure::program_lifetime::declared(cx.db(), AdtId::from(adt))
}
