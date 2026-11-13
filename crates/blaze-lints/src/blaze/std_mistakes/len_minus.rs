//! `len_minus`: a length minus something, in an unsigned type, where nothing before it says the
//! length is the larger: `v.len() - 1` on an empty `v`, `buf.len() - header` on a short buffer.
//! A debug build panics there; a release build wraps to a number near `usize::MAX`, which the
//! next index or allocation turns into a crash far from the cause, or worse, a read past the end
//! through `unsafe` code that trusted it.
//!
//! Found, outside test code: a `-` whose type is an unsigned integer and whose left side names
//! a length: a call of a method named `len`, `count` or `capacity`, or a path or field whose
//! name has one of the `_`-separated words `len`, `length`, `size`, `count` or `offset`. The
//! right side is not the literal `0`. Left alone, as the check is there:
//!
//! - a condition the `-` is inside (an `if` it is in the `then` branch of, a `while`, a match
//!   guard), or a statement before it in an enclosing block, that mentions both sides (`if n <=
//!   v.len()`, `assert!(header <= buf.len())`, `let Some(..) = ..checked_sub..`); for a literal
//!   right side, one that mentions the length or its `is_empty()`;
//! - a right side bounded by the left one itself: `len - n.min(len)`.
//!
//! The dominating check is read from the function's text, so it only ever suppresses a finding,
//! never makes one, and a check in a function the code calls is not seen.
//!
//! The fix, where the function returns an `Option` (and the `-` is in no closure or `async`
//! block, whose `?` would be theirs), is `len.checked_sub(n)?`; it is offered, not made
//! (`MaybeIncorrect`), as the `None` it hands on may mean something else to the caller. With no
//! `Option` to hand on there is no fix: `checked_sub` with a typed error naming the two
//! lengths, or the check before, is the author's choice.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, ArithOp, BinaryOp},
};

use super::{Fix, in_test, report, returns_option, strip};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::LenMinus;

/// The `_`-separated words of a name that make it a length's.
const LENGTH_WORDS: &[&str] = &["len", "length", "size", "count", "offset"];

/// The methods whose value is a length.
const LENGTH_CALLS: &[&str] = &["len", "count", "capacity"];

/// How many statements before the `-`, per enclosing block, are read for a check.
const LOOKED_BACK: usize = 32;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(bin) = ast::BinExpr::cast(node) else { return };
    if bin.op_kind() != Some(BinaryOp::ArithOp(ArithOp::Sub)) {
        return;
    }
    let (Some(lhs), Some(rhs)) = (bin.lhs(), bin.rhs()) else { return };
    if !names_length(strip(lhs)) || is_zero(strip(rhs)) {
        return;
    }
    let Some(ty) = cx.ty(lhs) else { return };
    if !ty.as_builtin().is_some_and(|it| it.is_uint()) {
        return;
    }
    let (left, right) = (squeeze(&cx.source_of(lhs.syntax())), squeeze(&cx.source_of(rhs.syntax())));
    if right.contains(".min(") && right.contains(&left) {
        return;
    }
    let literal = matches!(strip(rhs), ast::Expr::Literal(_));
    if dominated(node, &left, &right, literal) || in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let fix = match (returns_option(cx, node), cx.range(lhs.syntax()), cx.range(rhs.syntax())) {
        (true, Some(l), Some(r)) => {
            let receiver = match strip(lhs) {
                ast::Expr::MethodCallExpr(_)
                | ast::Expr::PathExpr(_)
                | ast::Expr::FieldExpr(_)
                | ast::Expr::CallExpr(_)
                | ast::Expr::ParenExpr(_) => cx.snippet(l).to_owned(),
                _ => format!("({})", cx.snippet(l)),
            };
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

/// Whether `expr` names a length: a call of [`LENGTH_CALLS`], or a path or field one of whose
/// name's words is in [`LENGTH_WORDS`].
fn names_length(expr: ast::Expr<'_>) -> bool {
    let has_word = |name: &str| {
        name.split('_').any(|part| LENGTH_WORDS.iter().any(|word| part.eq_ignore_ascii_case(word)))
    };
    match expr {
        ast::Expr::MethodCallExpr(call) => {
            call.name_ref().is_some_and(|it| LENGTH_CALLS.contains(&it.text()))
        }
        ast::Expr::FieldExpr(field) => field.name_ref().is_some_and(|it| has_word(it.text())),
        ast::Expr::PathExpr(path) => path
            .path()
            .and_then(|it| it.segment())
            .and_then(|it| it.name_ref())
            .is_some_and(|it| has_word(it.text())),
        _ => false,
    }
}

/// Whether `expr` is the literal `0`.
fn is_zero(expr: ast::Expr<'_>) -> bool {
    let ast::Expr::Literal(it) = expr else { return false };
    let text = it.syntax().text();
    let digits = text.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    !digits.is_empty() && digits.trim_start_matches(['0', '_']).is_empty()
}

/// `text` without its whitespace.
fn squeeze(text: &str) -> String {
    text.chars().filter(|it| !it.is_whitespace()).collect()
}

/// Whether a condition `node` is under, or a statement before it in a block it is in, mentions
/// both sides (`left` and `right`, squeezed); for a `literal` right side, the length or its
/// emptiness.
fn dominated(node: ast::Node<'_>, left: &str, right: &str, literal: bool) -> bool {
    let empty = left.strip_suffix(".len()").map(|base| format!("{base}.is_empty()"));
    let mentions = |text: &str| {
        let text = squeeze(text);
        match literal {
            true => text.contains(left) || empty.as_ref().is_some_and(|it| text.contains(it.as_str())),
            false => text.contains(left) && text.contains(right),
        }
    };
    let mut child = node;
    for ancestor in node.ancestors().skip(1) {
        match ancestor.kind() {
            SyntaxKind::FN | SyntaxKind::CLOSURE_EXPR | SyntaxKind::CONST | SyntaxKind::STATIC => {
                return false;
            }
            SyntaxKind::IF_EXPR => {
                if let Some(it) = ast::IfExpr::cast(ancestor)
                    && let Some(condition) = it.condition()
                    && condition.syntax() != child
                    && mentions(condition.syntax().text())
                {
                    return true;
                }
            }
            SyntaxKind::WHILE_EXPR | SyntaxKind::MATCH_GUARD => {
                if mentions(ancestor.text()) {
                    return true;
                }
            }
            SyntaxKind::STMT_LIST => {
                let before = ancestor.children().take_while(|it| *it != child).count();
                let skip = before.saturating_sub(LOOKED_BACK);
                if ancestor.children().take(before).skip(skip).any(|it| mentions(it.text())) {
                    return true;
                }
            }
            _ => {}
        }
        child = ancestor;
    }
    false
}
