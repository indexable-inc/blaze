//! `question_mark_converts`: a `?` that turns its operand's error into the function's through a
//! `From` the reader never sees, where the conversion loses what the error was about. Two cases,
//! by the types inference gave (the operand's `Result<_, E>`, the function's or closure's
//! `Result<_, F>`, `E` not `F`):
//!
//! - `E` is one of std's errors that name no subject: `std::io::Error` and `core::fmt::Error`
//!   (each `Error` of `core`, `alloc` or `std`), `ParseIntError`, `ParseFloatError`,
//!   `ParseBoolError`, `ParseCharError`, `Utf8Error`, `FromUtf8Error`, `FromUtf16Error`,
//!   `TryFromIntError`, `TryFromSliceError`, `AddrParseError` and `VarError`. "No such file or
//!   directory" without the path; "invalid digit found in string" without the string. A
//!   `#[from]` variant of the function's error carries the same nothing on.
//! - `F` is an erased error: `Box<dyn Error ..>` (std's `Box` of a `dyn`), `String`,
//!   `anyhow::Error`, `eyre::Report`: `E`'s type is gone and with it every match on what
//!   failed, and a typed `E` never had the subject added on its way out.
//!
//! These are matched by the ADT's crate (`core`, `alloc`, `std`; `anyhow`; `eyre`) and name, not
//! by `std_def`: of them only `std::io::Error`, `core::num::TryFromIntError`, `alloc::boxed::Box`
//! and `alloc::string::String` are in `famous::KNOWN`, and the rest are defined in private
//! modules (`core::num::error`, `core::str::error`) whose paths a table would have to spell.
//!
//! A `?` whose operand is already `F` (after `.map_err(..)`) is the right form and is not
//! found; neither is test code, whose errors are the test's own. No fix: the subject is the
//! caller's to name.
//!
//! The right form: `.map_err(|source| Error::Read { path: path.to_owned(), source })?`, a
//! variant that names the thing the operation was on beside the error it got.

use hir::HirDisplay as _;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::super::{Lint, facts, std_mistakes::report, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::QuestionMarkConverts;

/// std's errors that say what went wrong but not on what.
const SUBJECTLESS: &[&str] = &[
    "Error",
    "ParseIntError",
    "ParseFloatError",
    "ParseBoolError",
    "ParseCharError",
    "Utf8Error",
    "FromUtf8Error",
    "FromUtf16Error",
    "TryFromIntError",
    "TryFromSliceError",
    "AddrParseError",
    "VarError",
];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if node.kind() != SyntaxKind::TRY_EXPR || !cx.on_blaze(LINT) || cx.analysis().is_none() {
        return;
    }
    if test_code::is_test_code(cx, node) {
        return;
    }
    let Some(operand) = ast::TryExpr::cast(node).and_then(|it| it.expr()) else { return };
    let Some(from) = result_error(cx, cx.ty(operand)) else { return };
    let Some(to) = result_error(cx, returns_into(cx, node)) else { return };
    if from == to || from.contains_unknown() || to.contains_unknown() {
        return;
    }
    let db = cx.db();
    let lost = match (subjectless(cx, &from), erased(cx, &to)) {
        (true, _) => "the error names no subject (the path, the text, the key it was about) and the conversion adds none",
        (false, Some(erased)) => erased,
        (false, None) => return,
    };
    let Some(range) = cx.range(node) else { return };
    let target = cx.krate().to_display_target(db);
    let message = format!(
        "this `?` converts `{}` into `{}` through a `From` nobody reads here: {lost}",
        from.display(db, target),
        to.display(db, target)
    );
    let note = "name what failed where it failed: `.map_err(|source| Error::Variant { path, source })?`, a variant \
                that carries the subject beside the error";
    report(cx, LINT, node, range, message, note, None);
}

/// The type a `?` at `node` returns its residual into: the return type of the closure or
/// function it is in, the future's output for an `async fn`. `None` in an `async` or `try`
/// block, a `const` or a `static`, where the `?` acts on something else.
pub(super) fn returns_into<'q, 't>(cx: &Cx<'q, 't>, node: ast::Node<'t>) -> Option<hir::Type<'q>> {
    let db = cx.db();
    for at in node.ancestors().skip(1) {
        match at.kind() {
            SyntaxKind::BLOCK_EXPR => {
                let block = ast::BlockExpr::cast(at)?;
                if block.async_token().is_some()
                    || block.try_block_modifier().is_some()
                    || block.gen_token().is_some()
                {
                    return None;
                }
            }
            SyntaxKind::CLOSURE_EXPR => {
                let closure = ast::Expr::cast(at)?;
                return Some(cx.ty(closure)?.as_callable(db)?.return_type());
            }
            SyntaxKind::FN => {
                let f = cx.fn_def(ast::Fn::cast(at)?)?;
                return match f.is_async(db) {
                    true => f.async_ret_type(db),
                    false => Some(f.ret_type(db)),
                };
            }
            SyntaxKind::CONST | SyntaxKind::STATIC => return None,
            _ => {}
        }
    }
    None
}

/// The error of `ty` when it is a `core::result::Result`.
fn result_error<'q>(cx: &Cx<'q, '_>, ty: Option<hir::Type<'q>>) -> Option<hir::Type<'q>> {
    let ty = ty?;
    let result = cx.famous().core_result_Result().map(hir::Adt::Enum)?;
    if ty.as_adt() != Some(result) {
        return None;
    }
    ty.type_arguments().nth(1)
}

/// Whether `adt` is of `core`, `alloc` or `std`.
fn of_std(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    let db = cx.db();
    let krate = adt.module(db).krate(db);
    ["core", "alloc", "std"].iter().any(|it| facts::crate_is(db, krate, it))
}

/// Whether `ty` is one of [`SUBJECTLESS`].
fn subjectless(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    let Some(adt) = ty.as_adt() else { return false };
    of_std(cx, adt) && SUBJECTLESS.contains(&adt.name(cx.db()).as_str())
}

/// What converting into `ty` loses, when `ty` is an erased error.
fn erased(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<&'static str> {
    let db = cx.db();
    let adt = ty.as_adt()?;
    let name = adt.name(db);
    let erases = match name.as_str() {
        "Box" if of_std(cx, adt) => ty.type_arguments().next().is_some_and(|it| it.as_dyn_trait().is_some()),
        "String" => of_std(cx, adt),
        "Error" => facts::adt_is(db, adt, "anyhow", "Error"),
        "Report" => facts::adt_is(db, adt, "eyre", "Report") || facts::adt_is(db, adt, "color_eyre", "Report"),
        _ => false,
    };
    erases.then_some(
        "the target is an erased error, so the error's type is gone (no caller can match on what failed) and nothing \
         says what it was about",
    )
}
