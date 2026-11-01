//! `panic_in_result_fn`: a `panic!` or an assertion in a function that returns a `Result`: the
//! function has a way to say it failed and panics instead.
//!
//! Follows clippy's `clippy_lints/src/panic_in_result_fn.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a function (free, inherent or trait, not a
//! closure) returning a `Result`, an `async fn` by what it resolves to, whose body (not the
//! bodies of the closures, constants and items in it) calls `core`'s or `std`'s `panic!`,
//! `assert!`, `assert_eq!` or `assert_ne!`; `todo!`, `unimplemented!` and `unreachable!` say
//! the code is unfinished or can't be reached, and are not counted. Unlike clippy, which takes
//! a panic only where it is the outermost macro call, a panic a macro of the crate expands to
//! is counted too (pointed at where that macro is called), and the message names the function
//! and how many panics it has.

use syntax::{
    SyntaxKind as K, TextRange, WalkEvent,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{Implemented, unwrap_in_result, utils};
use crate::{check::Cx, diag::MultiSpan};

const LINT: Implemented = Implemented::PanicInResultFn;

/// The macros counted as panics.
const PANICS: &[&str] = &["panic", "assert", "assert_eq", "assert_ne"];

/// How deep the walk goes into the expansions of the crate's own macros.
const DEEPEST: u32 = 8;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on(LINT) || !cx.is_written(f.syntax()) {
        return;
    }
    let Some(body) = f.body() else { return };
    let Some(ty) = unwrap_in_result::returns(cx, f) else { return };
    if utils::option_or_result(cx, &ty) != Some("Result") || ty.is_reference() {
        return;
    }
    let mut panics = Vec::new(); // alloc: the panics of one function, reported then dropped
    walk(cx, body.syntax(), None, 0, &mut panics);
    if panics.is_empty() {
        return;
    }
    let Some(span) = cx.item_span(f.syntax()) else { return };
    let name = f.name().map_or("this function", |it| it.text());
    let message = match panics.len() {
        1 => format!("`{name}` returns a `Result` and has a panic: a caller can't handle a panic as an error"),
        n => format!("`{name}` returns a `Result` and has {n} panics: a caller can't handle a panic as an error"),
    };
    let mut at = MultiSpan::default();
    at.primary = panics.iter().map(|it| cx.span(*it)).collect(); // alloc: as `panics`
    cx.lint(LINT, f.syntax(), span, message)
        .span_note(at, "here")
        .help("return an `Err` saying what went wrong; a check that can only fail on a bug is `debug_assert!`");
}

/// Adds to `found` where each panic of `node` is written: its own call, or the call of the
/// macro of the crate it is in the expansion of (`outer`).
fn walk<'t>(
    cx: &Cx<'_, 't>,
    node: ast::Node<'t>,
    outer: Option<TextRange>,
    depth: u32,
    found: &mut Vec<TextRange>,
) {
    let mut events = node.preorder();
    while let Some(event) = events.next() {
        let WalkEvent::Enter(it) = event else { continue };
        if skips(it) {
            events.skip_subtree();
            continue;
        }
        let Some(call) = ast::MacroCall::cast(it) else { continue };
        events.skip_subtree();
        let at = outer.or_else(|| cx.range(it));
        match cx.std_macro(call) {
            Some(name) => {
                if PANICS.contains(&name.as_str())
                    && let Some(at) = at
                    && !found.contains(&at)
                {
                    found.push(at);
                }
            }
            None => {
                if depth < DEEPEST
                    && let Some(expansion) = cx.expand(call)
                {
                    walk(cx, expansion, at, depth + 1, found);
                }
            }
        }
    }
}

/// Whether `node` is a body of its own or an item: what it runs is not the function's.
fn skips(node: ast::Node<'_>) -> bool {
    match node.kind() {
        K::CLOSURE_EXPR | K::FN | K::CONST | K::STATIC | K::IMPL | K::TRAIT | K::MODULE => true,
        K::BLOCK_EXPR => ast::BlockExpr::cast(node).is_some_and(|it| it.const_token().is_some()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "panic_in_result_fn";
    const FOUND: &str = "a caller can't handle a panic as an error";

    #[test]
    fn a_panic_or_assertion_in_a_function_returning_a_result_is_found() {
        let found = lint_only(
            LINT,
            r#"
pub struct E;
pub fn f(x: u8) -> Result<u8, E> {
    assert!(x > 0);
    assert_eq!(x, 1, "one");
    if x == 2 { panic!("two") }
    Ok(x)
}
pub fn g(x: u8) -> Result<u8, E> {
    if x == 0 { panic!() }
    Ok(x)
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("`f` returns a `Result` and has 3 panics"), "{found}");
        assert!(found.contains("`g` returns a `Result` and has a panic"), "{found}");
    }

    #[test]
    fn a_panic_elsewhere_or_in_a_closure_or_an_unfinished_body_is_not_found() {
        let found = lint_only(
            LINT,
            r#"
pub struct E;
pub fn plain(x: u8) -> u8 { assert!(x > 0); x }
pub fn find(x: u8) -> Option<u8> { assert!(x > 0); Some(x) }
pub fn f(x: u8) -> Result<u8, E> {
    let check = |y: u8| assert!(y > 0);
    check(x);
    fn inner() { panic!() }
    Ok(x)
}
pub fn g() -> Result<u8, E> { todo!() }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn a_result_through_a_renamed_alias_and_a_panic_a_macro_writes_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod io {
    pub struct Error;
    pub type Done = Result<u8, Error>;
}
use io::Done as Out;
macro_rules! check { ($c:expr) => { assert!($c) }; }
pub fn f(x: u8) -> Out {
    check!(x > 0);
    Ok(x)
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(found.contains("`f` returns a `Result` and has a panic"), "{found}");
    }
}
