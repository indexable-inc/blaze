//! `forwarding_fn`: a function whose whole body calls another with its own parameters, in
//! order, unchanged: `fn load(p: &Path) -> Result<Config> { read_config(p) }`. A second name
//! for one function: a reader follows two hops to the body, and the two drift apart when one
//! of them changes.
//!
//! Found from the body's typed tree: one expression (as the tail or a `return`), a call by path
//! or a method call on `self` or the first parameter, whose arguments are the parameters as
//! paths, each to the next parameter's binding, with no adjustment (no auto-borrow, deref or
//! coercion: a wrapper that converts does something), no turbofish, and a callee that is not a
//! trait's (naming an impl of a generic trait is a choice), not generic and not the function
//! itself. Not found: an exported function (an API's name is its own), a trait's function or
//! an impl's of one, a function with an attribute (`#[inline]`, `#[track_caller]`, a `cfg`), an
//! `unsafe`, `async`, `const` or `extern` function, a function named as a value somewhere (a
//! callback whose type the wrapper fits), test code. A method that forwards to a field
//! (`self.inner.f(x)`) is delegation and is left alone.
//!
//! No fix: the callers change with it, in other files.

use syntax::view::{self as ast, AstNode as _, HasArgList as _, HasGenericArgs as _, HasName as _};

use super::{
    Lint, facts, fn_subject, has_attrs, judged_fn,
    rows::Way,
};
use crate::check::Cx;

const LINT: Lint = Lint::ForwardingFn;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) || has_attrs(f) || cx.is_exported(f.syntax()) {
        return;
    }
    if f.unsafe_token().is_some() || f.async_token().is_some() || f.const_token().is_some() {
        return;
    }
    let Some(func) = judged_fn(cx, f) else { return };
    let Some(list) = f.body().and_then(|it| it.stmt_list()) else { return };
    if list.statements().next().is_some() {
        return;
    }
    let Some(mut tail) = list.tail_expr().map(|it| it.peel_parens()) else { return };
    if let ast::Expr::ReturnExpr(ret) = tail {
        let Some(inner) = ret.expr() else { return };
        tail = inner.peel_parens();
    }
    let Some(params) = f.param_list() else { return };
    // alloc: the function's few parameter names, `self` first
    let mut expected: Vec<String> = Vec::new();
    if params.self_param().is_some() {
        expected.push("self".to_owned());
    }
    for param in params.params() {
        let Some(ast::Pat::IdentPat(pat)) = param.pat() else { return };
        if pat.ref_token().is_some() || pat.pat().is_some() {
            return;
        }
        let Some(name) = pat.name() else { return };
        expected.push(name.text().to_owned());
    }
    let (callee, passed) = match tail {
        ast::Expr::MethodCallExpr(call) => {
            if call.generic_arg_list().is_some() {
                return;
            }
            let Some(callee) = cx.method(call) else { return };
            let Some(receiver) = call.receiver() else { return };
            let Some(args) = call.arg_list() else { return };
            // alloc: the call's few arguments, its receiver first
            let passed: Vec<_> = std::iter::once(receiver).chain(args.args()).collect();
            (callee, passed)
        }
        ast::Expr::CallExpr(call) => {
            let Some(ast::Expr::PathExpr(path)) = call.expr().map(|it| it.peel_parens()) else { return };
            let Some(path) = path.path() else { return };
            if path.segment().and_then(|it| it.generic_arg_list()).is_some() {
                return;
            }
            let Some(hir::PathResolution::Def(hir::ModuleDef::Function(callee))) = cx.path(path) else {
                return;
            };
            let Some(args) = call.arg_list() else { return };
            // alloc: the call's few arguments
            let passed: Vec<_> = args.args().collect();
            (callee, passed)
        }
        _ => return,
    };
    let db = cx.db();
    if callee == func || passed.len() != expected.len() {
        return;
    }
    if hir::AsAssocItem::as_assoc_item(callee, db).is_some_and(|it| it.container_or_implemented_trait(db).is_some()) {
        return;
    }
    if !hir::GenericDef::Function(callee).type_or_const_params(db).is_empty() {
        return;
    }
    let by_method = matches!(tail, ast::Expr::MethodCallExpr(_));
    for (place, (arg, name)) in passed.iter().zip(&expected).enumerate() {
        let ast::Expr::PathExpr(path) = arg.peel_parens() else { return };
        let Some(written) = path.path() else { return };
        if written.qualifier().is_some() || written.syntax().text() != name.as_str() {
            return;
        }
        if name != "self" && !matches!(cx.path(written), Some(hir::PathResolution::Local(_))) {
            return;
        }
        // A reborrow keeps the type; an auto-borrow of a method's receiver is the call's own.
        if !(by_method && place == 0) && cx.ty(*arg) != cx.adjusted_ty(*arg) {
            return;
        }
    }
    if cx.ty(tail) != cx.adjusted_ty(tail) {
        return;
    }
    if let Some(subject) = fn_subject(cx, func)
        && facts(cx, subject).all(Way::Named) > 0
    {
        return;
    }
    let Some(name) = f.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let callee_name = callee.name(db).as_str().to_owned();
    let message = format!(
        "`fn {}` only forwards its parameters to `{callee_name}`: call `{callee_name}` directly",
        name.text()
    );
    super::report(
        cx,
        LINT,
        f.syntax(),
        range,
        message,
        "a function that only forwards is a second name for one body: readers follow two hops and the two drift apart",
        None,
    );
}
