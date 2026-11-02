//! `print_stdout`: a call of std's `print!` or `println!`.
//!
//! Follows the `print_stdout` part of clippy's `clippy_lints/src/write.rs` (`Write::check_expr`)
//! (MIT or Apache-2.0), ported from what is known of its source at rust-lang/rust's nightly of
//! 2026-10: no reference checkout of clippy was at hand. A build script (the crate cargo names
//! `build_script_build`) may print, and `allow-print-in-tests` leaves tests alone.

use syntax::view::{self as ast, AstNode as _};

use crate::check::Cx;

const LINT: super::Implemented = super::Implemented::PrintStdout;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(call) = ast::MacroCall::cast(node) else { return };
    check_print(cx, call, LINT, &["print", "println"]);
}

/// clippy's `Write::check_expr` for the std macros `names`, linted as `lint`.
pub(super) fn check_print<'t>(
    cx: &mut Cx<'_, 't>,
    call: ast::MacroCall<'t>,
    lint: super::Implemented,
    names: &[&str],
) {
    if !named(call, names) {
        return;
    }
    let Some(name) = cx.std_macro(call) else { return };
    let Some(&name) = names.iter().find(|it| **it == name.as_str()) else { return };
    if cx.conf().bool("allow-print-in-tests", false) && cx.is_in_test(call.syntax()) {
        return;
    }
    if lint == LINT && is_build_script(cx) {
        return;
    }
    let Some(range) = cx.range(call.syntax()) else { return };
    let span = cx.span(range);
    cx.lint(lint, call.syntax(), span, format!("use of `{name}!`"));
}

/// The syntactic filter of the macro lints: the last segment of `call`'s path is one of
/// `names`. A std macro is called by its own name (`println!`, `std::println!`); a renaming
/// `use` is not looked through.
pub(super) fn named(call: ast::MacroCall<'_>, names: &[&str]) -> bool {
    call.path()
        .and_then(|it| it.segment())
        .and_then(|it| it.name_ref())
        .is_some_and(|it| names.contains(&it.text()))
}

/// clippy's `is_build_script`: the crate's name is `build_script_build`, as cargo names a
/// package's build script.
fn is_build_script(cx: &Cx<'_, '_>) -> bool {
    cx.krate()
        .display_name(cx.db())
        .is_some_and(|it| it.crate_name().as_str() == "build_script_build")
}
