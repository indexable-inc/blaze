//! `unused_parameter`: a parameter of a function of the workspace that its body never reads. A
//! caller works out a value and passes it, and nothing it passes changes what the function does:
//! either the function lost the use it had (an edit dropped it, and the bug is that it no longer
//! honours the argument) or the parameter is left over and every caller pays for it. A name
//! starting with `_` does not excuse it: the underscore only quiets rustc's `unused_variables`,
//! and says nothing of why the value is taken.
//!
//! Read from the HIR of the body, macro expansions included (a parameter named only in a
//! `format!` or an `assert!` is read): a parameter is read when a path expression anywhere in the
//! body, closures included, resolves to one of its bindings. A write (`x = ..`) is not a read; a
//! compound assignment (`x += ..`) is.
//!
//! Not checked: `self` (whether a method needs its receiver is a different question), a trait's
//! method or an impl of one (the signature is the trait's), a function with an ABI, a callback
//! (a function named as a value in its own file, not called: its signature is the one its user
//! needs), a function under `#[cfg(..)]` (a twin of another target's with the same signature),
//! a body whose value diverges (`todo!()`, `unimplemented!()`, `loop {}`: `placeholder_body` and
//! `todo` say those), and a signature a macro wrote. A function whose signature is fixed for a
//! reason this cannot see says the reason: `#[expect(blaze::unused_parameter, reason = "..")]`.
//!
//! No fix: dropping the parameter changes every caller, which is a change of the signature to
//! make in one batch with them.

use std::alloc::Global;

use hir::AsAssocItem as _;
use hir_def::{DefWithBodyId, hir::BindingId};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::super::{Lint, hash_order_output::local_of, std_mistakes::report};
use crate::check::Cx;

const LINT: Lint = Lint::UnusedParameter;

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || !cx.in_file() {
        return;
    }
    let Some(f) = ast::Fn::cast(cx.root()) else { return };
    if f.abi().is_some() || f.body().is_none() || !cx.is_written(f.syntax()) {
        return;
    }
    if f.attrs().any(|it| it.simple_name() == Some("cfg")) {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    if func.as_assoc_item(db).is_some_and(|it| it.container_or_implemented_trait(db).is_some()) {
        return;
    }
    let Some(list) = f.param_list() else { return };
    let Some(analysis) = cx.analysis() else { return };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return };
    let body = analysis.body();
    if analysis.infer().expr_ty(body.root_expr()).is_never() {
        return;
    }
    let mut read: Vec<BindingId, Global> = Vec::new_in(Global);
    for (id, _) in body.exprs() {
        if let Some(binding) = local_of(db, def, body, id) {
            read.push(binding);
        }
    }
    read.sort_unstable_by_key(|it| it.into_raw().into_u32());
    read.dedup();
    let is_read = |it: &BindingId| {
        read.binary_search_by_key(&it.into_raw().into_u32(), |b| b.into_raw().into_u32()).is_ok()
    };
    // The parameters as written, in the order of the body's own: `self` is neither's.
    let mut unread: Vec<(ast::Param<'_>, String), Global> = Vec::new_in(Global);
    for (param, pat) in list.params().zip(body.params.iter()) {
        let mut bindings: Vec<BindingId, Global> = Vec::new_in(Global);
        body.walk_bindings_in_pat(pat.user_written, |it| bindings.push(it));
        // A parameter that binds nothing (`_`, `(_, _)`) is never read; one that binds is read
        // when any of its bindings is.
        if bindings.iter().any(is_read) {
            continue;
        }
        let Some(written) = param.pat() else { continue };
        unread.push((param, cx.source_of(written.syntax()).into_owned()));
    }
    if unread.is_empty() || is_callback(cx, f, func) {
        return;
    }
    let name = func.name(db);
    let name = name.as_str();
    for (param, pat) in unread {
        let Some(range) = cx.range(param.syntax()) else { continue };
        let message = format!(
            "`fn {name}` never reads its parameter `{pat}`: what a caller passes changes nothing it does"
        );
        report(
            cx,
            LINT,
            param.syntax(),
            range,
            message,
            "either the body lost the use it had (read it again) or the parameter is left over \
             (remove it with its argument at every call, in one change); a signature fixed for a \
             reason this cannot see says it with `#[expect(blaze::unused_parameter, reason = \"..\")]`",
            None,
        );
    }
}

/// Whether `func`, written as `f`, is named as a value in its own file rather than called: a
/// callback, whose signature is its user's. A path that names it and does not resolve counts
/// too, so that what cannot be seen is not reported.
fn is_callback<'t>(cx: &Cx<'_, 't>, f: ast::Fn<'t>, func: hir::Function) -> bool {
    let Some(name) = f.name() else { return true };
    let Some(file) = f.syntax().ancestors().last() else { return true };
    for node in file.descendants() {
        let Some(path_expr) = ast::PathExpr::cast(node) else { continue };
        let Some(path) = path_expr.path() else { continue };
        let Some(segment) = path.segment() else { continue };
        if segment.name_ref().is_none_or(|it| it.text() != name.text()) {
            continue;
        }
        let called = node.parent().is_some_and(|parent| {
            parent.kind() == SyntaxKind::CALL_EXPR
                && ast::CallExpr::cast(parent)
                    .and_then(|call| call.expr())
                    .is_some_and(|callee| callee.syntax() == node)
        });
        if called {
            continue;
        }
        match cx.path(path) {
            Some(hir::PathResolution::Def(hir::ModuleDef::Function(it))) if it == func => {
                return true;
            }
            Some(_) => {}
            None => return true,
        }
    }
    false
}
