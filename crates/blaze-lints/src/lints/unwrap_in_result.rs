//! `unwrap_in_result`: `.unwrap()` or `.expect(..)` of an `Option` or `Result` in a function that
//! returns one: the function has a way to say it failed and panics instead.
//!
//! Follows clippy's `clippy_lints/src/unwrap_in_result.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a call written in the file of `unwrap` or
//! `expect`, as a method or by path (`Option::unwrap(x)`), whose receiver is an `Option` or a
//! `Result`, in the body of a function (free, inherent or trait) returning an `Option` or a
//! `Result`, an `async fn` by what it resolves to; the body of a closure, a constant or an
//! `async` block in it is not the function's. Unlike clippy, which points at the call and
//! says "used in a function that returns `Result`", the message names the function and the
//! receiver, the note is on its return type, the help is the shape that turns the receiver into
//! the function's failure, and an `unwrap` of the same kind as the return is fixed to `?`
//! (maybe incorrect: the error type must convert).
//!
//! What a node of the walk is in ([`owner`], [`returns`]) is shared with
//! [`super::panic_in_result_fn`], [`super::infinite_loop`] and [`super::unused_result_ok`].

use hir::PathResolution;
use syntax::{
    SyntaxKind as K,
    view::{self as ast, AstNode as _, HasArgList as _, HasName as _},
};

use super::{Implemented, utils};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::UnwrapInResult;

/// The body a node of the walk is in.
#[derive(Clone, Copy)]
pub(super) enum Owner<'t> {
    Fn(ast::Fn<'t>),
    Closure(ast::ClosureExpr<'t>),
    /// A constant, a static, a `const` block or an `async` block.
    Other,
}

/// `node`'s ancestors, from `node` up, through to the walk's root when `node` is in an
/// expansion and written in the file (then from where it is written).
pub(super) fn written_ancestors<'t>(
    cx: &Cx<'_, 't>,
    node: ast::Node<'t>,
) -> impl Iterator<Item = ast::Node<'t>> + use<'t> {
    let root = cx.root();
    let outside = match std::ptr::eq(node.file(), root.file()) {
        true => None,
        false => cx
            .range(node)
            .filter(|it| root.text_range().contains_range(*it))
            .map(|it| root.covering_element(it)),
    };
    node.ancestors().chain(outside.into_iter().flat_map(|it| it.ancestors()))
}

/// The innermost body `node` is in, `node` not counted.
pub(super) fn owner<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<Owner<'t>> {
    written_ancestors(cx, node).skip(1).find_map(|it| match it.kind() {
        K::FN => ast::Fn::cast(it).map(Owner::Fn),
        K::CLOSURE_EXPR => ast::ClosureExpr::cast(it).map(Owner::Closure),
        K::CONST | K::STATIC => Some(Owner::Other),
        K::BLOCK_EXPR => ast::BlockExpr::cast(it)
            .filter(|it| it.async_token().is_some() || it.const_token().is_some())
            .map(|_| Owner::Other),
        _ => None,
    })
}

/// What `f`'s body evaluates to: its return type, or for an `async fn` what it resolves to.
pub(super) fn returns<'q>(cx: &Cx<'q, '_>, f: ast::Fn<'_>) -> Option<hir::Type<'q>> {
    let f = cx.fn_def(f)?;
    let db = cx.db();
    match f.is_async(db) {
        true => f.async_ret_type(db),
        false => Some(f.ret_type(db)),
    }
}

/// The receiver of `expr` when it is a call of `unwrap` or `expect`, by method or by path, and
/// the method's name.
fn unwrap_call<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> Option<(&'t str, ast::Expr<'t>)> {
    match expr {
        ast::Expr::MethodCallExpr(_) => {
            let call = utils::method_call(cx, expr)?;
            matches!(call.name, "unwrap" | "expect").then_some((call.name, call.receiver))
        }
        ast::Expr::CallExpr(call) => {
            let ast::Expr::PathExpr(path) = call.expr()? else { return None };
            let path = path.path()?;
            let name = path.segment()?.name_ref()?;
            if !matches!(name.text(), "unwrap" | "expect") {
                return None;
            }
            let PathResolution::Def(hir::ModuleDef::Function(f)) = cx.path(path)? else { return None };
            let db = cx.db();
            if !f.module(db).krate(db).origin(db).is_lang() {
                return None;
            }
            let receiver = call.arg_list()?.args().next()?;
            cx.is_written(receiver.syntax()).then_some((name.text(), receiver))
        }
        _ => None,
    }
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || !cx.is_written(expr.syntax()) {
        return;
    }
    let Some((method, receiver)) = unwrap_call(cx, expr) else { return };
    let Some(have) = cx.ty(receiver).and_then(|it| utils::option_or_result(cx, &it)) else { return };
    let Some(Owner::Fn(f)) = owner(cx, expr.syntax()) else { return };
    let Some(want) = returns(cx, f).and_then(|it| utils::option_or_result(cx, &it)) else { return };
    let Some(range) = cx.range(expr.syntax()) else { return };
    let name = f.name().map_or("this function", |it| it.text());
    let written = cx.source_of(receiver.syntax());
    let note = f.ret_type().and_then(|it| cx.range(it.syntax())).map(|it| cx.span(it));
    let span = cx.span(range);
    let message = format!(
        "`{method}` of the `{have}` `{written}` in `{name}`, which returns a `{want}`: it panics where it could return"
    );
    let diagnostic = cx.lint(LINT, expr.syntax(), span.clone(), message);
    if let Some(note) = note {
        diagnostic.span_note(note, format!("`{name}` returns a `{want}` here"));
    }
    match (have, want) {
        ("Option", "Result") => {
            diagnostic.help(format!("`{written}.ok_or(..)?` returns an error saying what was missing"));
        }
        ("Result", "Option") => {
            diagnostic.help(format!("`{written}.ok()?` returns `None`; the error is dropped, so say why in a comment"));
        }
        _ if method == "unwrap" => {
            diagnostic.suggestion(
                span,
                "return the failure instead",
                format!("{written}?"),
                Applicability::MaybeIncorrect,
            );
        }
        _ => {
            diagnostic.help(format!("`{written}?` returns the failure; put what `expect` says into the error"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "unwrap_in_result";
    const FOUND: &str = "it panics where it could return";

    #[test]
    fn an_unwrap_in_a_function_returning_a_result_or_option_is_found() {
        let found = lint_only(
            LINT,
            r#"
pub struct E;
pub fn parse() -> Result<u8, E> { Ok(0) }
pub fn find() -> Option<u8> { None }
pub fn f() -> Result<u8, E> {
    let a = parse().unwrap();
    let b = find().expect("there");
    Ok(a + b)
}
pub fn g() -> Option<u8> {
    Some(find().unwrap() + Option::unwrap(find()))
}
pub async fn h() -> Option<u8> {
    Some(find().unwrap())
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 5, "{found}");
        assert!(found.contains("`unwrap` of the `Result` `parse()` in `f`, which returns a `Result`"), "{found}");
        assert!(found.contains("`find().ok_or(..)?`"), "{found}");
        assert!(found.contains("find()?"), "{found}");
    }

    #[test]
    fn an_unwrap_elsewhere_or_in_a_closure_or_constant_is_not_found() {
        let found = lint_only(
            LINT,
            r#"
pub fn find() -> Option<u8> { None }
pub fn plain() -> u8 { find().unwrap() }
pub fn f() -> Option<u8> {
    let c = || find().unwrap();
    const N: u8 = { let x: Option<u8> = Some(1); x.unwrap() };
    Some(c() + N)
}
pub struct Other;
impl Other { pub fn unwrap(self) -> u8 { 0 } }
pub fn g() -> Option<u8> { Some(Other.unwrap()) }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn a_result_through_a_renamed_alias_and_an_unwrap_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod io {
    pub struct Error;
    pub type Done = Result<u8, Error>;
}
use io::Done as Out;
macro_rules! id { ($e:expr) => { $e }; }
pub fn read() -> Out { Ok(0) }
pub fn f() -> Out {
    let a = read().unwrap();
    let b = id!(read().unwrap());
    Ok(a + b)
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("`read()` in `f`"), "{found}");
    }
}
