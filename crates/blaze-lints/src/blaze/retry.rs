//! `retry`: an operation tried again up to a constant number of times (rule no-fallback: no
//! retry, ever; the first failure is the error, typed and naming what failed). blaze-cas's
//! `FAILURES_MOST = 8` repeated failed IO eight times before it said anything.
//!
//! A finding, outside test code ([`test_code::is_test_code`]), is:
//!
//! - a comparison (`<`, `<=`, `>`, `>=`, `==`, `!=`) one of whose operands is a path, field or
//!   deref whose name has one of the `_`-separated words, any case, `retry`, `retries`,
//!   `retried`, `attempt`, `attempts`, `tries` or `failures`, and whose other operand is a
//!   constant bound: an integer literal other than `0` and `1`, a path to a `const` or
//!   `static`, or an operand named so itself;
//! - a `for` loop over a range whose end is a `const` or `static` named so, or whose pattern
//!   binds a name so named and whose end is a constant bound.
//!
//! The false positives expected are few: a count of failures reported and compared with a
//! threshold that is not a retry (it is kept with `#[expect(blaze::retry, reason = "..")]`
//! saying so); `== 0` and `> 1` style tests of whether anything failed are not findings.
//!
//! There is no fix the lint can write: the loop goes, and the first failure is returned as a
//! typed error naming what failed and where.

use syntax::view::{self as ast, AstNode as _, BinaryOp, HasName as _, RangeItem as _};

use super::{Lint, narrowed_len::has_word, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::Retry;

/// The `_`-separated words of a name that make it a count of tries.
const RETRY_WORDS: &[&str] = &["retry", "retries", "retried", "attempt", "attempts", "tries", "failures"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !matches!(node.kind(), syntax::SyntaxKind::BIN_EXPR | syntax::SyntaxKind::FOR_EXPR) {
        return;
    }
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(node) {
        return;
    }
    let said = match (ast::BinExpr::cast(node), ast::ForExpr::cast(node)) {
        (Some(bin), _) => compared(cx, bin),
        (_, Some(for_)) => looped(cx, for_),
        _ => None,
    };
    let Some(name) = said else { return };
    if test_code::is_test_code(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let message = format!("`{name}` bounds how many times an operation is tried again");
    let span = cx.span(range);
    cx.blaze(LINT, node, span, message)
        .note(
            "a retry repeats a failure, hides its first cause and makes the time an operation takes depend \
             on how often it fails: the first failure is the error",
        )
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// The retry-named operand of the comparison `bin` with a constant bound.
fn compared(cx: &Cx<'_, '_>, bin: ast::BinExpr<'_>) -> Option<String> {
    if !matches!(bin.op_kind()?, BinaryOp::CmpOp(_)) {
        return None;
    }
    let (lhs, rhs) = (bin.lhs()?, bin.rhs()?);
    for (named, other) in [(lhs, rhs), (rhs, lhs)] {
        let Some(name) = retry_name(named) else { continue };
        if retry_name(other).is_some() || bound(cx, other) {
            return Some(name.to_owned());
        }
    }
    None
}

/// The retry-named end or pattern of the `for` loop `for_` over a range with a constant end.
fn looped(cx: &Cx<'_, '_>, for_: ast::ForExpr<'_>) -> Option<String> {
    let ast::Expr::RangeExpr(range) = strip(for_.iterable()?) else { return None };
    let end = strip(range.end()?);
    if let Some(name) = retry_name(end)
        && constant(cx, end)
    {
        return Some(name.to_owned());
    }
    let ast::Pat::IdentPat(pat) = for_.pat()? else { return None };
    let name = pat.name()?;
    let text = name.text();
    (has_word(text, RETRY_WORDS) && bound(cx, end)).then(|| text.to_owned())
}

/// `expr` without the parentheses around it.
fn strip(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    let mut expr = expr;
    while let ast::Expr::ParenExpr(it) = expr {
        let Some(inner) = it.expr() else { break };
        expr = inner;
    }
    expr
}

/// The name of `expr` when it is a path, field or deref whose name has a word of
/// [`RETRY_WORDS`].
fn retry_name(expr: ast::Expr<'_>) -> Option<&str> {
    let name = match strip(expr) {
        ast::Expr::PathExpr(it) => it.path()?.segment()?.name_ref()?.text(),
        ast::Expr::FieldExpr(it) => it.name_ref()?.text(),
        ast::Expr::PrefixExpr(it) => return retry_name(it.expr()?),
        _ => return None,
    };
    has_word(name, RETRY_WORDS).then_some(name)
}

/// Whether `expr` is a constant bound: an integer literal other than `0` and `1`, or a path to
/// a `const` or `static`.
fn bound(cx: &Cx<'_, '_>, expr: ast::Expr<'_>) -> bool {
    match strip(expr) {
        ast::Expr::Literal(it) => {
            let text = it.syntax().text();
            let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '_');
            !digits.is_empty()
                && digits.bytes().all(|it| it.is_ascii_digit() || it == b'_')
                && !matches!(digits.trim_start_matches('0'), "" | "1")
        }
        it => constant(cx, it),
    }
}

/// Whether `expr` is a path to a `const` or `static`.
fn constant(cx: &Cx<'_, '_>, expr: ast::Expr<'_>) -> bool {
    let ast::Expr::PathExpr(path) = expr else { return false };
    let Some(path) = path.path() else { return false };
    matches!(
        cx.path(path),
        Some(hir::PathResolution::Def(hir::ModuleDef::Const(_) | hir::ModuleDef::Static(_)))
    )
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "bounds how many times an operation is tried again";

    fn found(main: &str) -> String {
        let fixture = format!(
            "//- minicore: sized, copy, clone, option, result, fn, iterator, range\n//- /src/lib.rs crate:main\n{}",
            main.trim_start_matches('\n')
        );
        lint(&fixture, "-A clippy::all --warn=blaze::retry")
    }

    /// The bug in miniature: failed IO repeated up to a constant, compared and looped over.
    #[test]
    fn a_retry_up_to_a_constant_is_found() {
        let out = found(
            r#"
const FAILURES_MOST: u32 = 8;
pub struct Io { pub attempts: u32 }
pub fn read(io: &mut Io, mut failures: u32) -> bool {
    while failures < FAILURES_MOST {
        failures += 1;
    }
    if io.attempts >= 3 {
        return false;
    }
    for _ in 0..FAILURES_MOST {}
    for attempt in 0..5 {
        let _ = attempt;
    }
    true
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 4, "{out}");
        assert!(out.contains("`failures` bounds how many times"), "{out}");
    }

    /// Not a finding: a test of whether anything failed, a count compared with no constant,
    /// a name that only holds a retry word's letters, and a test.
    #[test]
    fn whether_anything_failed_other_names_and_tests_are_left_alone() {
        let out = found(
            r#"
pub fn report(failures: u32, seen: u32, contemptuous: u32) -> bool {
    if failures == 0 || failures > 1 {
        return true;
    }
    failures < seen || contemptuous > 8
}
#[cfg(test)]
mod tests {
    pub fn tries_up_to(tries: u32) -> bool {
        tries < 5
    }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// A count that is not a retry, kept by an expectation on its fn, which the finding fulfils.
    #[test]
    fn an_expectation_on_the_fn_is_fulfilled() {
        let out = found(
            r#"
const SAID_MOST: u32 = 16;
#[expect(blaze::retry, reason = "counts the failures reported, to stop listing them; nothing is tried again")]
pub fn listed(failures: u32) -> bool {
    failures < SAID_MOST
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }
}
