//! `todo`: a call of std's `todo` macro.
//!
//! Follows the `todo` part of clippy's `clippy_lints/src/panic_unimplemented.rs` (MIT or
//! Apache-2.0), ported from what is known of its source at rust-lang/rust's nightly of 2026-10:
//! no reference checkout of clippy was at hand. clippy lints the call in a test as anywhere
//! else (`allow-panic-in-tests` is `panic`'s alone), so this does too.

use syntax::view::{self as ast, AstNode as _};

use crate::check::Cx;

const LINT: super::Implemented = super::Implemented::Todo;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(call) = ast::MacroCall::cast(node) else { return };
    if !super::print_stdout::named(call, &["todo"])
        || cx.std_macro(call).is_none_or(|it| it.as_str() != "todo")
    {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let span = cx.span(range);
    cx.lint(LINT, node, span, "`todo` should not be present in production code");
}
