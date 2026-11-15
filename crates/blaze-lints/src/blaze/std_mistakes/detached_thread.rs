//! `detached_thread`: `std::thread::spawn(..)` or `thread::Builder::spawn(..)` whose
//! `JoinHandle` is thrown away (a statement, `let _`, `let _name`) or bound to a local the
//! function never names again. Nobody joins the thread: a panic in it is lost, the process can
//! exit with it half done (its writes torn, its `Drop`s not run), and nothing bounds how many
//! run. The right form: keep the handle and `join` it (its `Result` says whether the thread
//! panicked), or `std::thread::scope`, which joins every thread it spawned before it returns.
//!
//! A handle passed on, returned or stored is followed no further: whoever holds it may join
//! it. Outside test code. No fix: where the join goes is the author's choice.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::{
    body_of, bound, called, discarded, in_test, inherent_of, is_std_fn, parent_expr, report,
    strip, unwrap_of, uses_of,
};
use crate::{blaze::Lint, check::Cx};

const LINT: Lint = Lint::DetachedThread;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let f = match ast::MethodCallExpr::cast(node) {
        Some(call) => cx.method(call),
        None => ast::CallExpr::cast(node).and_then(|it| called(cx, it)),
    };
    let Some(f) = f else { return };
    let builder = match f.name(cx.db()).as_str() {
        "spawn" if is_std_fn(cx, f, &["std", "thread", "spawn"]) => false,
        "spawn" if inherent_of(cx, f, &["std", "thread", "Builder"]) => true,
        _ => return,
    };
    if !unjoined(cx, node) || in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let what = if builder { "`thread::Builder::spawn`" } else { "`thread::spawn`" };
    report(
        cx,
        LINT,
        node,
        range,
        format!("the `JoinHandle` of this {what} is dropped, so nobody joins the thread"),
        "a panic in it is lost and the process can exit with it half done: keep the handle and `join` it, or spawn in `std::thread::scope`, which joins before it returns",
        None,
    );
}

/// Whether the handle `node` makes is thrown away (through `?`, `unwrap`, `expect` for the
/// `Builder`'s `io::Result`) or bound to a local never named again.
fn unjoined<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    if discarded(cx, node) {
        return true;
    }
    // `let h = thread::spawn(..);`, `let h = Builder::new().spawn(..)?;`.
    let mut at = node;
    let stmt = loop {
        let Some(parent) = parent_expr(at) else { return false };
        if let Some(stmt) = ast::LetStmt::cast(parent) {
            break stmt;
        }
        let through = parent.kind() == SyntaxKind::TRY_EXPR
            || ast::MethodCallExpr::cast(parent).is_some_and(|call| {
                call.receiver().map(strip).is_some_and(|it| it.syntax() == at)
                    && unwrap_of(cx, call).is_some()
            });
        if !through {
            return false;
        }
        at = parent;
    };
    let Some(local) = stmt.pat().and_then(|it| bound(cx, it)) else { return false };
    let Some(body) = body_of(node, false) else { return false };
    uses_of(cx, body, local).is_empty()
}
