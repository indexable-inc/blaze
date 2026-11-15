//! `len_minus`: a length minus something, in an unsigned type, where nothing before it says the
//! length is the larger: `v.len() - 1` on an empty `v`, `buf.len() - header` on a short buffer.
//! A debug build panics there; a release build wraps to a number near `usize::MAX`, which the
//! next index or allocation turns into a crash far from the cause, or worse, a read past the end
//! through `unsafe` code that trusted it.
//!
//! Found, outside test code: a `-` whose type is an unsigned integer and whose left side is a
//! length: a call that resolves to std's `len` (of a slice, a `str`, a std collection,
//! `ExactSizeIterator`), or a local bound, without `mut`, to one. The right side is not the
//! literal `0`. Left alone, as the check is there, read from the typed tree, never the text:
//!
//! - a comparison that dominates the `-` (in the condition of an enclosing `if` or `while`, in
//!   the guard of its match arm, or in a statement before it in an enclosing block, a macro
//!   call's arguments included: `assert!(n <= v.len())`) of the same length (the same local, or
//!   `len` of the same place) with the same right side, by resolution; for a literal right side,
//!   any comparison of the length, or `is_empty()` of the same sequence;
//! - a dominating `checked_sub` of the same two;
//! - a right side bounded by the length itself: `len - n.min(len)`, `len - min(n, len)`.
//!
//! Each of these only ever suppresses a finding, never makes one; a check in a function the
//! code calls is not seen. [`reported`] is the one home of the decision: `implicit_overflow`
//! leaves the subtractions it holds to this lint.
//!
//! The fix, where the function returns an `Option` (and the `-` is in no closure or `async`
//! block, whose `?` would be theirs), is `len.checked_sub(n)?`; it is offered, not made
//! (`MaybeIncorrect`), as the `None` it hands on may mean something else to the caller. With no
//! `Option` to hand on there is no fix: `checked_sub` with a typed error naming the two
//! lengths, or the check before, is the author's choice.

use syntax::view::{self as ast, AstNode as _, ArithOp, BinaryOp, HasArgList as _};

use super::{
    Fix, body_of, called, dominated, in_test, is_std_fn, receiver_text, report, returns_option,
    same_place, std_method, strip,
};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::LenMinus;

/// A length the lint judges: the expression written, and the sequence it is the length of
/// when that is known (the receiver of `len`).
#[derive(Clone, Copy)]
struct Length<'t> {
    written: ast::Expr<'t>,
    of: Option<ast::Expr<'t>>,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some((lhs, rhs)) = finding(cx, node) else { return };
    let Some(range) = cx.range(node) else { return };
    let fix = match (returns_option(cx, node), cx.range(lhs.syntax()), cx.range(rhs.syntax())) {
        (true, Some(l), Some(r)) => {
            let receiver = receiver_text(cx, lhs, l);
            Some(Fix {
                message: "hand a `None` on where the length is the smaller",
                // alloc: the fix's one part
                parts: vec![(range, format!("{receiver}.checked_sub({})?", cx.snippet(r)))],
                applicability: Applicability::MaybeIncorrect,
            })
        }
        _ => None,
    };
    let message = format!(
        "`{}` underflows when `{}` is the larger, and nothing before it says it is not",
        cx.source_of(node),
        cx.source_of(rhs.syntax())
    );
    report(
        cx,
        LINT,
        node,
        range,
        message,
        "a debug build panics there and a release build wraps to near `usize::MAX`, which the next index or allocation turns into a crash far from the cause",
        fix,
    );
}

/// Whether `node`, a `-`, is what this lint reports when it is on: the one decision, which
/// `implicit_overflow` asks so a length's subtraction is found once.
pub(crate) fn reported<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    finding(cx, node).is_some()
}

/// The two sides of `node` when it is a finding.
fn finding<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<(ast::Expr<'t>, ast::Expr<'t>)> {
    let bin = ast::BinExpr::cast(node)?;
    if bin.op_kind() != Some(BinaryOp::ArithOp(ArithOp::Sub)) {
        return None;
    }
    let (lhs, rhs) = (bin.lhs()?, bin.rhs()?);
    if is_zero(strip(rhs)) || !cx.ty(lhs)?.as_builtin().is_some_and(|it| it.is_uint()) {
        return None;
    }
    let length = Length { written: lhs, of: length_of(cx, lhs)? };
    if bounded_by(cx, rhs, length) {
        return None;
    }
    let literal = matches!(strip(rhs), ast::Expr::Literal(_));
    let guard = |at: ast::Node<'t>| guards(cx, at, length, rhs, literal);
    if dominated(cx, node, &guard) || in_test(cx, node) {
        return None;
    }
    Some((lhs, rhs))
}

/// When `expr` is a length, the sequence it is the length of (`None` when that is not
/// written): a call resolved to std's `len`, or a local bound, without `mut`, to one.
fn length_of<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> Option<Option<ast::Expr<'t>>> {
    match strip(expr) {
        ast::Expr::MethodCallExpr(call) if std_method(cx, call, "len") => Some(call.receiver()),
        ast::Expr::PathExpr(path) => {
            let Some(hir::PathResolution::Local(local)) = path.path().and_then(|it| cx.path(it))
            else {
                return None;
            };
            let body = body_of(path.syntax(), false)?;
            body.descendants().filter_map(ast::LetStmt::cast).find_map(|stmt| {
                let ast::Pat::IdentPat(pat) = stmt.pat()? else { return None };
                if pat.mut_token().is_some() || cx.local(pat) != Some(local) {
                    return None;
                }
                match strip(stmt.initializer()?) {
                    ast::Expr::MethodCallExpr(call) if std_method(cx, call, "len") => {
                        Some(call.receiver())
                    }
                    _ => None,
                }
            })
        }
        _ => None,
    }
}

/// Whether `expr` is `length`: the same place, or `len` of the same sequence.
fn is_length<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>, length: Length<'t>) -> bool {
    if same_place(cx, expr, length.written) {
        return true;
    }
    match (strip(expr), length.of) {
        (ast::Expr::MethodCallExpr(call), Some(of)) => {
            std_method(cx, call, "len") && call.receiver().is_some_and(|it| same_place(cx, it, of))
        }
        _ => false,
    }
}

/// Whether `rhs` is bounded by the length itself: `n.min(len)`, `len.min(n)`, `min(n, len)`.
fn bounded_by<'t>(cx: &Cx<'_, 't>, rhs: ast::Expr<'t>, length: Length<'t>) -> bool {
    let operands: Option<(ast::Expr<'t>, ast::Expr<'t>)> = match strip(rhs) {
        ast::Expr::MethodCallExpr(call) if std_method(cx, call, "min") => {
            let mut args = call.arg_list().into_iter().flat_map(|it| it.args());
            call.receiver().zip(args.next())
        }
        ast::Expr::CallExpr(call)
            if called(cx, call).is_some_and(|f| is_std_fn(cx, f, &["core", "cmp", "min"])) =>
        {
            let mut args = call.arg_list().into_iter().flat_map(|it| it.args());
            args.next().zip(args.next())
        }
        _ => None,
    };
    operands.is_some_and(|(a, b)| is_length(cx, a, length) || is_length(cx, b, length))
}

/// Whether `at`, an expression of a dominating check, says `length` is at least `rhs`: a
/// comparison of the two (of the length alone for a `literal` right side), `is_empty()` of the
/// sequence for a literal, or a `checked_sub` of the two.
fn guards<'t>(
    cx: &Cx<'_, 't>,
    at: ast::Node<'t>,
    length: Length<'t>,
    rhs: ast::Expr<'t>,
    literal: bool,
) -> bool {
    if let Some(cmp) = ast::BinExpr::cast(at)
        && matches!(cmp.op_kind(), Some(BinaryOp::CmpOp(_)))
        && let (Some(a), Some(b)) = (cmp.lhs(), cmp.rhs())
    {
        return match literal {
            true => is_length(cx, a, length) || is_length(cx, b, length),
            false => {
                (is_length(cx, a, length) && same_place(cx, b, rhs))
                    || (is_length(cx, b, length) && same_place(cx, a, rhs))
            }
        };
    }
    let Some(call) = ast::MethodCallExpr::cast(at) else { return false };
    let Some(receiver) = call.receiver() else { return false };
    if literal && std_method(cx, call, "is_empty") {
        return length.of.is_some_and(|of| same_place(cx, receiver, of));
    }
    if std_method(cx, call, "checked_sub") && is_length(cx, receiver, length) {
        let mut args = call.arg_list().into_iter().flat_map(|it| it.args());
        return args.next().is_some_and(|it| same_place(cx, it, rhs));
    }
    false
}

/// Whether `expr` is the literal `0`.
fn is_zero(expr: ast::Expr<'_>) -> bool {
    let ast::Expr::Literal(it) = expr else { return false };
    let text = it.syntax().text();
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    !digits.is_empty() && digits.trim_start_matches(['0', '_']).is_empty()
}
