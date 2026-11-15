//! `constant_argument`: a parameter every call in the workspace passes the same constant: a
//! literal (`3`, `"utf-8"`, `true`), a `const`, a unit variant or a unit struct. The parameter
//! is a choice nobody makes: each caller repeats the value, and a reader must check that none
//! differs. The right form is the value inside the function (a `const` or a local), and the
//! parameter gone.
//!
//! Read off the use rows of the whole workspace ([`super::rows`]): each call's arguments after
//! `self`, joined per parameter (unseen, one constant, varies); production code and tests both
//! count, since a test passing another value is a caller that varies it. Found only with two
//! calls or more. Not found: a function named as a value (a callback: its signature is the
//! callee's), a function a serde impl calls (`#[serde(default = "..")]`), a trait's function or
//! an impl's of one, `extern` and test functions, a parameter named `_..`.
//!
//! No fix yet: inlining the parameter edits every call, which may be in other files; the
//! message names the value to inline.

use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint, facts, fn_subject, judged_fn,
    rows::{Arg, Constant, Role, Way},
};
use crate::check::Cx;

const LINT: Lint = Lint::ConstantArgument;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(func) = judged_fn(cx, f) else { return };
    let Some(subject) = fn_subject(cx, func) else { return };
    let seen = facts(cx, subject);
    let calls = seen.get(Role::Plain, Way::Call) + seen.get(Role::Test, Way::Call);
    if calls < 2 || seen.all(Way::Named) > 0 || seen.get(Role::Stored, Way::Call) > 0 {
        return;
    }
    let Some(list) = f.param_list() else { return };
    let name = func.name(cx.db()).as_str().to_owned();
    for (param, arg) in list.params().zip(&seen.args) {
        let Arg::Same(constant) = arg else { continue };
        let Some(pat) = param.pat() else { continue };
        let pat_text = cx.source_of(pat.syntax()).into_owned();
        if pat_text.trim_start_matches("mut ").starts_with('_') {
            continue;
        }
        let Some(range) = cx.range(param.syntax()) else { continue };
        let value = match constant {
            Constant::Literal(it) => format!("`{}`", literal_text(it)),
            Constant::Item(_) => "the same constant".to_owned(),
        };
        let message = format!(
            "every one of the {calls} calls of `fn {name}` passes {value} for `{pat_text}`"
        );
        super::report(
            cx,
            LINT,
            param.syntax(),
            range,
            message,
            "a parameter every caller passes the same value is a choice nobody makes: put the value in the \
             function (a `const` or a local) and drop the parameter",
            None,
        );
    }
}

/// A literal as it would be written.
fn literal_text(lit: &hir_def::hir::Literal) -> String {
    use hir_def::hir::Literal as L;
    match lit {
        L::String(it) => format!("{:?}", it.as_str()),
        L::ByteString(it) => format!("b{:?}", String::from_utf8_lossy(it)),
        L::CString(it) => format!("c{:?}", String::from_utf8_lossy(it)),
        L::Char(it) => format!("{it:?}"),
        L::Bool(it) => it.to_string(),
        L::Int(it, _) => it.to_string(),
        L::Uint(it, _) => it.to_string(),
        L::Float(it, _) => it.to_string(),
    }
}
