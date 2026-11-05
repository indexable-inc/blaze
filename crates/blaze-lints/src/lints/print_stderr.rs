//! `print_stderr`: a call of std's `eprint!` or `eprintln!`.
//!
//! Follows the `print_stderr` part of clippy's `clippy_lints/src/write.rs` (`Write::check_expr`)
//! (MIT or Apache-2.0), ported from what is known of its source at rust-lang/rust's nightly of
//! 2026-10: no reference checkout of clippy was at hand. `allow-print-in-tests` leaves tests
//! alone; unlike `print_stdout`, a build script is linted.

use syntax::view::{self as ast, AstNode as _};

use crate::check::Cx;

const LINT: super::Implemented = super::Implemented::PrintStderr;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(call) = ast::MacroCall::cast(node) else { return };
    super::print_stdout::check_print(cx, call, LINT, &["eprint", "eprintln"]);
}
