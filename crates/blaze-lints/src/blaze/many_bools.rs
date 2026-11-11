//! `many_bools`: a call that passes two or more bare `true` or `false` literals by position,
//! `open(path, true, false)`. Nothing at the call says what each chooses, and two of them swap
//! without a type error. A record parameter names each one at the call and lets a caller leave
//! out the ones it does not care about (`open(path, { create: true, .. })`, blaze's
//! `#![blaze::records]`, docs/v2/plan/design/records.md); a two-variant enum does the same
//! without the extension.
//!
//! It reads the call alone, so a method call and a call of any callee count, the workspace's
//! or not: the finding is at the call, where the reader is. Not counted: a literal inside
//! another argument (`f(g(true), true)` passes one), a call under `#[cfg(test)]` or `#[test]`,
//! and one a macro wrote. There is no fix: the names are the author's, and the callee may be
//! another crate's.

use syntax::view::{self as ast, AstNode as _};

use super::Lint;
use crate::check::Cx;

const LINT: Lint = Lint::ManyBools;

/// How many bare `bool` literals a call passes before it is found.
const AT_LEAST: usize = 2;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>, args: Option<ast::ArgList<'t>>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(args) = args else { return };
    let bare = |arg: &ast::Expr<'t>| {
        matches!(arg, ast::Expr::Literal(_))
            && matches!(&*cx.source_of(arg.syntax()), "true" | "false")
    };
    let bools = args.args().filter(bare).count();
    if bools < AT_LEAST {
        return;
    }
    // What a derive or a macro wrote is not the author's to name.
    if cx.expansion().is_some() || !cx.is_written(node) || cx.is_in_test(node) {
        return;
    }
    let Some(range) = cx.range(args.syntax()) else { return };
    let span = cx.span(range);
    let message = format!(
        "this call passes {bools} bare `bool`s by position: nothing here says what each chooses, and two of them swap without a type error"
    );
    let diagnostic = cx.blaze(LINT, node, span, message);
    // alloc: the finding's help, once per finding
    diagnostic.help(
        "take them as a record parameter with defaults (`opts: { create: bool = false, .. }`, called as `f(.., { create: true, .. })`, `#![blaze::records]`), or as two-variant enums named for the choices"
            .to_owned(),
    );
}
