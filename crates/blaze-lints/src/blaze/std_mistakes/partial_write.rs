//! `partial_write`: a call of `std::io::Write::write` whose count of bytes written is thrown
//! away. `write` may write fewer bytes than it was given (a pipe, a socket, a full disk, an
//! interrupt) and says how many it did; code that drops the count loses the rest of the buffer
//! without an error.
//!
//! Found: a method call or a path call resolved to `Write::write` (the trait's method or an
//! impl's of it) whose value is the expression of a statement, the initializer of `let _` or
//! `let _name`, or one of those after `?`, `.unwrap()` or `.expect(..)`. A count that is read
//! (a loop on it, a comparison, a sum) is not a finding: that is the partial-write loop done by
//! hand.
//!
//! The fix, machine-applicable, renames the call `write_all`, which writes every byte or
//! returns the error (`WriteZero` when the writer takes no more): the silent truncation becomes
//! an error the caller sees. The value was thrown away, so its type changing from `usize` to
//! `()` changes nothing that reads it.

use syntax::view::{self as ast, AstNode as _};

use super::{Fix, discarded, in_test, of_trait, report};
use crate::{blaze::Lint, check::Cx, diag::Applicability};

const LINT: Lint = Lint::PartialWrite;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let name = match (ast::MethodCallExpr::cast(node), ast::CallExpr::cast(node)) {
        (Some(call), _) => call.name_ref(),
        (_, Some(call)) => match call.expr() {
            Some(ast::Expr::PathExpr(it)) => {
                it.path().and_then(|it| it.segment()).and_then(|it| it.name_ref())
            }
            _ => None,
        },
        _ => None,
    };
    let Some(name) = name else { return };
    if name.text() != "write" {
        return;
    }
    let Some((f, _, _)) = super::callee(cx, node) else { return };
    if !of_trait(cx, f, &["std", "io", "Write"]) || !discarded(cx, node) || in_test(cx, node) {
        return;
    }
    let (Some(range), Some(at)) = (cx.range(node), cx.range(name.syntax())) else { return };
    let fix = Fix {
        message: "write every byte or return the error",
        // alloc: the fix's one part
        parts: vec![(at, "write_all".to_owned())],
        applicability: Applicability::MachineApplicable,
    };
    report(
        cx,
        LINT,
        node,
        range,
        "the count `Write::write` returns is thrown away, so the bytes it did not write are lost without an error".to_owned(),
        "`write` may write only part of the buffer (a pipe, a socket, a full disk) and says how much in its count",
        Some(fix),
    );
}
