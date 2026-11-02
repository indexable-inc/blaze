//! `dbg_macro`: a call of std's `dbg!`.
//!
//! Follows clippy's `clippy_lints/src/dbg_macro.rs` (MIT or Apache-2.0), ported from what is
//! known of its source at rust-lang/rust's nightly of 2026-10: no reference checkout of clippy
//! was at hand. `allow-dbg-in-tests` leaves alone what clippy's `is_in_test` finds in a test.

use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::DbgMacro;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(call) = ast::MacroCall::cast(node) else { return };
    if !super::print_stdout::named(call, &["dbg"]) || cx.std_macro(call).is_none_or(|it| it.as_str() != "dbg") {
        return;
    }
    if cx.conf().bool("allow-dbg-in-tests", false) && cx.is_in_test(node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let mut args = Vec::new();
    cx.macro_args(call, &mut args);
    let (at, code) = match args.as_slice() {
        // `dbg!()`: a statement of its own goes with its `;`, anything else becomes `()`.
        [] => match statement_semicolon(cx, call) {
            Some(semicolon) => (TextRange::new(range.start(), semicolon.end()), String::new()),
            None => (range, "()".to_owned()),
        },
        [(arg, _)] => (range, cx.snippet(*arg).to_owned()),
        [(first, _), .., (last, _)] => {
            (range, format!("({})", cx.snippet(TextRange::new(first.start(), last.end()))))
        }
    };
    let span = cx.span(range);
    let at = cx.span(at);
    cx.lint(LINT, node, span, "the `dbg!` macro is intended as a debugging tool").suggestion(
        at,
        "remove the invocation before committing it to a version control system",
        code,
        Applicability::MachineApplicable,
    );
}

/// rustc's `mac_call_stmt_semi_span` for a call that is a statement (its HIR parent a `Stmt`):
/// where the `;` after it is, the statement's last token.
fn statement_semicolon<'t>(cx: &Cx<'_, 't>, call: ast::MacroCall<'t>) -> Option<TextRange> {
    let expr = ast::MacroExpr::cast(call.syntax().parent()?)?;
    let stmt = ast::ExprStmt::cast(expr.syntax().parent()?)?;
    stmt.semicolon_token()?;
    let range = cx.range(stmt.syntax())?;
    Some(TextRange::new(range.end() - syntax::TextSize::from(1), range.end()))
}
