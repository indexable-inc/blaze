//! `let_underscore_future`: `let _ = future;`, which drops a future without running it: the
//! work it stands for never happens, and nothing says so.
//!
//! Follows clippy's `clippy_lints/src/let_underscore.rs` (`LET_UNDERSCORE_FUTURE`, MIT or
//! Apache-2.0), read from indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest
//! checkout to the pinned nightly's (rust-lang/rust 12433c30). As in clippy: a `let` whose
//! pattern is `_`, written in the file, whose initializer's type implements `Future`; a future
//! is not then also reported by [`super::let_underscore_must_use`]. clippy's first branch,
//! `let_underscore_lock` (a `parking_lot` guard), is not ported: such a guard is `#[must_use]`
//! and `let_underscore_must_use` reports it. Unlike clippy's, the message names the
//! expression.

use syntax::view::{self as ast, AstNode as _};

use super::Implemented;
use crate::check::Cx;

const LINT: Implemented = Implemented::LetUnderscoreFuture;

/// The initializer of `let _ = <init>;`, when `stmt` is one.
pub(super) fn wildcard_init<'t>(stmt: ast::LetStmt<'t>) -> Option<ast::Expr<'t>> {
    if !matches!(stmt.pat()?, ast::Pat::WildcardPat(_)) || stmt.let_else().is_some() {
        return None;
    }
    stmt.initializer()
}

/// Whether `ty` implements `Future`.
pub(super) fn is_future(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    cx.lang_trait(hir::LangItem::Future).is_some_and(|future| ty.impls_trait(cx.db(), future, &[]))
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, stmt: ast::LetStmt<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(init) = wildcard_init(stmt) else { return };
    let Some(ty) = cx.ty(init) else { return };
    if !is_future(cx, &ty) {
        return;
    }
    let Some(range) = cx.range(stmt.syntax()) else { return };
    let span = cx.span(range);
    let message =
        format!("`let _ =` drops the future of `{}` without running it", cx.source_of(init.syntax()));
    cx.lint(LINT, stmt.syntax(), span, message)
        .help("`.await` it or hand it to the scheduler; if never running it is the point, `drop(..)` it by name and say why");
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "let_underscore_future";
    const FOUND: &str = "without running it";

    #[test]
    fn a_dropped_future_is_found_and_a_bound_or_awaited_one_is_not() {
        let found = lint_only(
            LINT,
            r#"
use core::{future::Future, pin::Pin, task::{Context, Poll}};
pub struct Ready;
impl Future for Ready {
    type Output = u8;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u8> { Poll::Ready(0) }
}
pub async fn work() -> u8 { 0 }
pub async fn f() {
    let _ = work();
    let _ = Ready;
    let kept = work();
    let _ = kept.await;
    let _ = 1u8;
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("drops the future of `work()`"), "{found}");
        assert!(found.contains("drops the future of `Ready`"), "{found}");
    }

    #[test]
    fn a_future_through_a_renamed_import_and_one_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod jobs { pub async fn work() -> u8 { 0 } }
use jobs::work as run;
macro_rules! stmt { ($($t:tt)*) => { $($t)* }; }
pub async fn f() {
    let _ = run();
    stmt! { let _ = run(); }
}
"#,
        );
        assert!(found.matches(FOUND).count() >= 1, "{found}");
        assert!(found.contains("drops the future of `run()`"), "{found}");
    }
}
