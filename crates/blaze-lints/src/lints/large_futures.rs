//! `large_futures`: `fut.await` of a future whose size is at least the threshold, which a
//! fiber's small stack may not hold.
//!
//! Follows clippy's `clippy_lints/src/large_futures.rs` (MIT or Apache-2.0). As in clippy: the
//! operand of an `.await` written in the file, whose type implements `Future` and whose layout
//! is at least `future-size-threshold` bytes (`clippy.toml`, 16384 by default); the lint is on
//! the operand, and the suggestion, `Box::pin(..)` of it, is `Unspecified` as clippy's is.
//!
//! Unlike clippy, which skips a future whose layout fails, a layout the front end can't compute
//! is reported on the operand with the reason, never taken as small (rule `no-fallback`). The
//! one layout left alone is one that depends on a type parameter (`HasPlaceholder`): its size
//! is known only per instance, and rustc's `layout_of` refuses it too.

use syntax::view::{self as ast, AstNode as _};

use super::{Implemented, let_underscore_future::is_future};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::LargeFutures;

/// clippy's `future-size-threshold` when `clippy.toml` sets none.
pub(super) const DEFAULT_THRESHOLD: u64 = 16 * 1024;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let ast::Expr::AwaitExpr(await_expr) = expr else { return };
    let Some(arg) = await_expr.expr() else { return };
    if !cx.is_written(expr.syntax()) || !cx.is_written(arg.syntax()) {
        return;
    }
    let Some(ty) = cx.ty(arg) else { return };
    if !is_future(cx, &ty) {
        return;
    }
    let Some(range) = cx.range(arg.syntax()) else { return };
    let span = cx.span(range);
    let size = match ty.layout(cx.db()) {
        Ok(layout) => layout.size(),
        Err(hir::LayoutError::HasPlaceholder) => return,
        Err(error) => {
            let message = format!(
                "the size of the future `{}` is not known, so `large_futures` can't check it: \
                 its layout failed ({error})",
                cx.snippet(range)
            );
            cx.lint(LINT, arg.syntax(), span, message);
            return;
        }
    };
    let threshold =
        u64::try_from(cx.conf().int("future-size-threshold", -1)).unwrap_or(DEFAULT_THRESHOLD);
    if size < threshold {
        return;
    }
    let code = format!("Box::pin({})", cx.snippet(range));
    cx.lint(LINT, arg.syntax(), span.clone(), format!("large future with a size of {size} bytes"))
        .suggestion(span, "consider `Box::pin` on it", code, Applicability::Unspecified);
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "large_futures";
    const FOUND: &str = "large future with a size of";

    #[test]
    fn a_large_awaited_future_is_found_and_a_small_one_is_not() {
        let found = lint_only(
            LINT,
            r#"
fn keep(_: &[u8; 16384]) {}
pub async fn big() {
    let buf = [0u8; 16384];
    small().await;
    keep(&buf);
}
pub async fn small() {}
pub async fn caller() {
    big().await;
    small().await;
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("Box::pin(big())"), "{found}");
    }

    /// A future whose layout fails is reported with the reason, never taken as small: here
    /// `big`'s body indexes an array in a `core` without `Index` (minicore's, no `index` flag),
    /// so its MIR, and with it what the future keeps across the `.await`, does not build.
    #[test]
    fn a_future_whose_size_is_unknown_is_reported_not_passed() {
        let found = lint_only(
            LINT,
            r#"
pub async fn big() {
    let buf = [0u8; 16384];
    small().await;
    let _ = buf[0];
}
pub async fn small() {}
pub async fn caller() {
    big().await;
}
"#,
        );
        assert!(
            found.contains("the size of the future `big()` is not known")
                && found.contains("the coroutine's body did not build to MIR"),
            "{found}"
        );
        assert!(!found.contains(FOUND), "{found}");
    }
}
