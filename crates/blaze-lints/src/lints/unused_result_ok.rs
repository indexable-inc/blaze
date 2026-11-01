//! `unused_result_ok`: the statement `result.ok();`, which reads like a check but only silences
//! `Result`'s `#[must_use]`: the error is dropped, the silent fallback AGENTS.md rule 3 bans.
//!
//! Follows clippy's `clippy_lints/src/unused_result_ok.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: an expression statement ending in `;`
//! whose expression is `.ok()` of a `Result`, written in the file. Unlike clippy, whose
//! suggestion is `let _ = ..` (the same drop, spelled differently), the suggestion is `?` where
//! the function returns a `Result`, maybe incorrect as the error type must convert; elsewhere
//! the help says how to handle it or why it may be dropped.

use syntax::view::{self as ast, AstNode as _};

use super::{Implemented, unwrap_in_result, utils};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::UnusedResultOk;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(call) = utils::method_call(cx, expr) else { return };
    if call.name != "ok" || !call.args().is_empty() {
        return;
    }
    let Some(stmt) = expr.syntax().parent().and_then(ast::ExprStmt::cast) else { return };
    if stmt.semicolon_token().is_none() {
        return;
    }
    let Some(ty) = cx.ty(call.receiver) else { return };
    if utils::option_or_result(cx, &ty) != Some("Result") || ty.is_reference() {
        return;
    }
    let Some(range) = cx.range(expr.syntax()) else { return };
    let receiver = cx.source_of(call.receiver.syntax());
    let in_result_fn = match unwrap_in_result::owner(cx, expr.syntax()) {
        Some(unwrap_in_result::Owner::Fn(f)) => {
            unwrap_in_result::returns(cx, f).is_some_and(|it| utils::option_or_result(cx, &it) == Some("Result"))
        }
        _ => false,
    };
    let span = cx.span(range);
    let message = format!("`{receiver}.ok();` drops the error of `{receiver}`: `.ok()` only silences `#[must_use]`");
    let diagnostic = cx.lint(LINT, expr.syntax(), span.clone(), message);
    if in_result_fn {
        diagnostic
            .suggestion(span, "return the error instead", format!("{receiver}?"), Applicability::MaybeIncorrect)
            .note("the function's error type must convert from this one");
    } else {
        diagnostic.help("handle the `Err` (a `match`, `?`), or say why it may be dropped: `#[expect(clippy::unused_result_ok, reason = \"..\")]`");
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "unused_result_ok";
    const FOUND: &str = "only silences `#[must_use]`";

    #[test]
    fn a_statement_ok_of_a_result_is_found_and_other_ok_calls_are_not() {
        let found = lint_only(
            LINT,
            r#"
pub struct E;
pub fn write() -> Result<(), E> { Ok(()) }
pub fn f() -> Option<()> {
    write().ok();
    let _ = write().ok();
    let kept = write().ok();
    kept
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("`write().ok();` drops the error of `write()`"), "{found}");
        assert!(found.contains("handle the `Err`"), "{found}");
    }

    #[test]
    fn in_a_function_returning_a_result_it_suggests_the_question_mark() {
        let found = lint_only(
            LINT,
            r#"
pub struct E;
pub fn write() -> Result<(), E> { Ok(()) }
pub fn f() -> Result<(), E> {
    write().ok();
    Ok(())
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("write()?"), "{found}");
    }

    #[test]
    fn an_ok_of_something_else_is_not_found() {
        let found = lint_only(
            LINT,
            r#"
pub struct Other;
impl Other { pub fn ok(self) -> u8 { 0 } }
pub fn f() {
    Other.ok();
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn a_result_through_a_renamed_alias_and_a_statement_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod io {
    pub struct Error;
    pub type Written = Result<(), Error>;
}
use io::Written as W;
macro_rules! run { ($($t:tt)*) => { $($t)* }; }
pub fn write() -> W { Ok(()) }
pub fn f() {
    write().ok();
    run! { write().ok(); }
}
"#,
        );
        assert!(found.matches(FOUND).count() >= 1, "{found}");
        assert!(found.contains("`write().ok();` drops"), "{found}");
    }
}
