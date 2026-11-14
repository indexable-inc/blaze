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
//! method or an impl of one (the signature is the trait's), a function with an ABI, a function
//! under `#[cfg(..)]` (a twin of another target's with the same signature), a body that diverges
//! (its tail or last statement is `todo!()`, `unimplemented!()`, `loop {}`: `placeholder_body`
//! and `todo` say those), and a signature a macro wrote.
//!
//! A callback (a function passed as a `fn` pointer or an `impl Fn`, whose signature is its
//! user's) is reported: whether a function is one is a fact of its uses, in other bodies, and a
//! body's lint reads only its own body, so its result is keyed by that body alone. A signature
//! fixed for a reason this cannot see, a callback's among them, says the reason:
//! `#[expect(blaze::unused_parameter, reason = "a callback of `apply`")]`.
//!
//! No fix: dropping the parameter changes every caller, which is a change of the signature to
//! make in one batch with them.

use std::alloc::Global;

use hir::AsAssocItem as _;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{BindingId, Expr, ExprId, Statement},
};
use hir_ty::InferenceResult;
use syntax::view::{self as ast, AstNode as _, HasAttrs as _};

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
    if diverges(body, analysis.infer(), body.root_expr()) {
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
    if unread.is_empty() {
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

/// Whether the body `root` diverges: its own type is `!`, or it is a block whose tail, or last
/// statement when it has no tail, is of type `!` (`{ loop {} }` is typed as the return type it
/// coerces to, so its own type does not say it).
fn diverges(body: &Body, infer: &InferenceResult<'_>, root: ExprId) -> bool {
    if infer.expr_ty(root).is_never() {
        return true;
    }
    let Expr::Block { statements, tail, .. } = body.expr(root) else { return false };
    let last = match (tail, statements.last()) {
        (Some(tail), _) => tail,
        (None, Some(Statement::Expr { expr, .. })) => expr,
        _ => return false,
    };
    infer.expr_ty(last).is_never()
}
