//! `map_err_ignore`: `result.map_err(|_| ..)`, which throws the original error away, so the new
//! one can't say what failed: the silent fallback AGENTS.md rule 3 bans, and an error that names
//! nothing (rule 14).
//!
//! Follows clippy's `clippy_lints/src/methods/map_err_ignore.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a `map_err` of a `Result` (clippy asks
//! that the method's impl is of `Result`; here the receiver is one, the same calls) given a
//! closure that is not `move` and whose one parameter is the pattern `_`; reported at the
//! closure's parameters. A named binding (`|_e|`) is left alone, as in clippy. Unlike clippy's,
//! the message names the error type dropped, and the help points at keeping it as the source
//! or an `#[expect]` with a reason rather than renaming the binding.

use hir::HirDisplay as _;
use syntax::view::{self as ast, AstNode as _};

use super::{Implemented, utils};
use crate::check::Cx;

const LINT: Implemented = Implemented::MapErrIgnore;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(call) = utils::method_call(cx, expr) else { return };
    let [ast::Expr::ClosureExpr(closure)] = call.args() else { return };
    if call.name != "map_err" || closure.move_token().is_some() {
        return;
    }
    let Some(params) = closure.param_list() else { return };
    let mut each = params.params();
    let (Some(param), None) = (each.next(), each.next()) else { return };
    if !matches!(param.pat(), Some(ast::Pat::WildcardPat(_))) {
        return;
    }
    let Some(ty) = cx.ty(call.receiver) else { return };
    let ty = ty.strip_references();
    if utils::option_or_result(cx, &ty) != Some("Result") {
        return;
    }
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    let error = match ty.type_arguments().nth(1) {
        Some(it) => format!("`{}`", it.display(db, target)),
        None => "error".to_owned(),
    };
    let Some(range) = cx.range(params.syntax()) else { return };
    let span = cx.span(range);
    cx.lint(
        LINT,
        expr.syntax(),
        span,
        format!("`map_err(|_| ..)` throws away the {error} of `{}`: the new error can't say what failed", cx.source_of(call.receiver.syntax())),
    )
    .help("keep the original as the new error's source (`|e| Error::Read { path, source: e }`)")
    .help("if dropping it is right here, say why: `#[expect(clippy::map_err_ignore, reason = \"..\")]`");
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "map_err_ignore";
    const FOUND: &str = "throws away the";

    #[test]
    fn a_wildcard_closure_is_found_and_a_named_or_move_one_is_not() {
        let found = lint_only(
            LINT,
            r#"
pub struct ParseError;
pub struct Mine;
pub fn parse() -> Result<u8, ParseError> { Err(ParseError) }
pub fn f() -> Result<u8, Mine> {
    let a = parse().map_err(|_| Mine);
    let b = parse().map_err(|_e| Mine);
    let c = parse().map_err(move |_| Mine);
    let d = parse().map_err(|e| { let _ = e; Mine });
    let _ = (b, c, d);
    a
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("throws away the `ParseError` of `parse()`"), "{found}");
    }

    #[test]
    fn a_map_err_not_of_a_result_is_not_found() {
        let found = lint_only(
            LINT,
            r#"
pub struct Other;
impl Other {
    pub fn map_err<F: FnOnce(u8) -> u8>(self, f: F) -> u8 { f(0) }
}
pub fn f() -> u8 {
    Other.map_err(|_| 1)
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn a_result_through_a_renamed_alias_and_a_call_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod errors {
    pub struct ParseError;
    pub type Parsed = Result<u8, ParseError>;
}
use errors::Parsed as Out;
macro_rules! keep { ($e:expr) => { $e }; }
pub fn parse() -> Out { Err(errors::ParseError) }
pub fn f() -> Result<u8, ()> {
    let a = parse().map_err(|_| ());
    keep!(parse().map_err(|_| ()));
    a
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
    }
}
