//! `significant_drop_in_scrutinee`: a lock guard made as a temporary in the scrutinee of a
//! `match`, `if let` or `while let`, which holds the lock through every arm.
//!
//! Follows clippy's `clippy_lints/src/matches/significant_drop_in_scrutinee.rs` (MIT or
//! Apache-2.0), read from indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest
//! checkout to the pinned nightly's (rust-lang/rust 12433c30). As in clippy: a call in the
//! scrutinee written in the file returns a guard that the scrutinee goes on to use in place (a
//! method that borrows or dereferences it, a field or an index through it, `&` or `*` of it),
//! so the guard is a temporary that lives until the whole expression ends, arms included. The
//! fix binds the scrutinee's value to a `let` first, so the guard drops at the end of that
//! statement; it is `MaybeIncorrect` (the arms then run without the lock) and offered only
//! for a `match` or an `if let` that is a statement or a block's value, of a value that can't
//! hold a borrow of the guard. Unlike clippy:
//!
//! - only lock guards, std's and `lock_api`'s ([`guards`](super::guards)), not every type
//!   marked `#[has_significant_drop]`, which the front end does not read;
//! - a guard passed on by value (`m.lock().unwrap()` hands the `LockResult` to `unwrap`) is
//!   moved, not a temporary of the scrutinee, and a scrutinee that is the guard itself is the
//!   value matched on, both left alone;
//! - closures and blocks in the scrutinee have temporaries of their own and are not looked in.

use syntax::{
    SyntaxKind as K, TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{
    Implemented,
    guards::{Guards, Kind},
    utils::indentation,
};
use crate::{check::Cx, diag::Applicability};

const LINT: Implemented = Implemented::SignificantDropInScrutinee;

pub(super) fn check_match<'t>(cx: &mut Cx<'_, 't>, expr: ast::MatchExpr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(scrutinee) = expr.expr() else { return };
    check(cx, expr.syntax(), "match", scrutinee, true);
}

pub(super) fn check_if<'t>(cx: &mut Cx<'_, 't>, expr: ast::IfExpr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(condition) = expr.condition() else { return };
    let whole = matches!(condition, ast::Expr::LetExpr(_));
    for it in lets(condition) {
        let Some(scrutinee) = it.expr() else { continue };
        check(cx, expr.syntax(), "if let", scrutinee, whole);
    }
}

pub(super) fn check_while<'t>(cx: &mut Cx<'_, 't>, expr: ast::WhileExpr<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(condition) = expr.condition() else { return };
    for it in lets(condition) {
        let Some(scrutinee) = it.expr() else { continue };
        check(cx, expr.syntax(), "while let", scrutinee, false);
    }
}

/// The `let`s of a condition: itself, or those its `&&` chain joins.
fn lets(condition: ast::Expr<'_>) -> Vec<ast::LetExpr<'_>> {
    // alloc: the `let`s of one condition, a few
    let mut out = Vec::new();
    let mut stack = vec![condition]; // alloc: as `out`
    while let Some(it) = stack.pop() {
        match it {
            ast::Expr::LetExpr(it) => out.push(it),
            ast::Expr::ParenExpr(it) => stack.extend(it.expr()),
            ast::Expr::BinExpr(it) if it.op_kind() == Some(ast::BinaryOp::LogicOp(ast::LogicOp::And)) => {
                stack.extend(it.rhs());
                stack.extend(it.lhs());
            }
            _ => {}
        }
    }
    out
}

/// Lints the guards made as temporaries in `scrutinee`, of `construct` (a `what`); `fixable`
/// when the construct's value is the scrutinee's alone, not one `let` of a chain.
fn check<'t>(cx: &mut Cx<'_, 't>, construct: ast::Node<'t>, what: &str, scrutinee: ast::Expr<'t>, fixable: bool) {
    if !cx.is_written(scrutinee.syntax()) {
        return;
    }
    let guards = Guards::new(cx);
    let db = cx.db();
    // alloc: the guards of one scrutinee, a few
    let mut found: Vec<(TextRange, String)> = Vec::new();
    let mut stack = vec![scrutinee.syntax()]; // alloc: the nodes of one scrutinee
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), K::CLOSURE_EXPR | K::BLOCK_EXPR) {
            continue;
        }
        stack.extend(node.children());
        if !matches!(node.kind(), K::CALL_EXPR | K::METHOD_CALL_EXPR) || node == scrutinee.syntax() {
            continue;
        }
        let Some(call) = ast::Expr::cast(node) else { continue };
        let Some(ty) = cx.ty(call) else { continue };
        if ty.as_adt().and_then(|adt| guards.kind(db, adt)) != Some(Kind::Lock) || !in_place(cx, call, scrutinee) {
            continue;
        }
        let Some(range) = cx.range(node) else { continue };
        let shown = hir::HirDisplay::display(&ty, db, cx.krate().to_display_target(db));
        found.push((range, format!("{shown}")));
    }
    found.sort_by_key(|(range, _)| (range.start(), range.end()));
    let owner = construct
        .ancestors()
        .find_map(ast::Fn::cast)
        .and_then(|it| it.name())
        .map_or_else(|| "this item".to_owned(), |it| format!("`{}`", it.text())); // alloc: a message's part
    let fix = if fixable { fix(cx, construct, scrutinee) } else { None };
    for (range, ty) in found {
        let span = cx.span(range);
        let message = format!(
            "the temporary `{ty}` made by `{}` holds {} until this `{what}` ends, through all its arms, in {owner}",
            cx.snippet(range),
            Kind::Lock.held()
        );
        let diagnostic = cx.lint(LINT, construct, span, message);
        diagnostic.note("a temporary of a scrutinee lives until the end of the whole expression");
        match &fix {
            Some(parts) => {
                diagnostic.note("with the fix the arms run without the lock: check that they don't need it");
                diagnostic.multipart_suggestion(
                    "bind the value first, so the guard drops at the end of that statement",
                    parts.clone(), // clone: one per guard of the scrutinee, a few
                    Applicability::MaybeIncorrect,
                );
            }
            None => {
                diagnostic.help(format!(
                    "bind the value to a `let` before the `{what}`, so the guard drops at the end of that statement"
                ));
            }
        }
    }
}

/// Whether the guard `call` makes is used in place inside `scrutinee`, which makes it a
/// temporary of the scrutinee, rather than moved on.
fn in_place<'t>(cx: &Cx<'_, 't>, call: ast::Expr<'t>, scrutinee: ast::Expr<'t>) -> bool {
    let mut outer = call;
    while let Some(paren) = outer.syntax().parent().and_then(ast::ParenExpr::cast) {
        outer = ast::Expr::ParenExpr(paren);
    }
    if outer.syntax() == scrutinee.syntax() {
        return false;
    }
    let adjusted = cx
        .adjustments(outer)
        .is_some_and(|it| it.iter().any(|it| matches!(it.kind, hir::Adjust::Deref(_) | hir::Adjust::Borrow(_))));
    if adjusted {
        return true;
    }
    let Some(parent) = outer.syntax().parent().and_then(ast::Expr::cast) else { return false };
    let is = |it: Option<ast::Expr<'t>>| it.is_some_and(|it| it.syntax() == outer.syntax());
    match parent {
        ast::Expr::FieldExpr(it) => is(it.expr()),
        ast::Expr::IndexExpr(it) => is(it.base()),
        ast::Expr::RefExpr(it) => is(it.expr()),
        ast::Expr::PrefixExpr(it) => it.op_kind() == Some(ast::UnaryOp::Deref) && is(it.expr()),
        _ => false,
    }
}

/// The fix: `let value = <scrutinee>;` before the statement `construct` is, and `value` for
/// the scrutinee. `None` when the construct is no statement or block value, or the value may
/// hold a borrow of the guard.
fn fix<'t>(cx: &Cx<'_, 't>, construct: ast::Node<'t>, scrutinee: ast::Expr<'t>) -> Option<Vec<(crate::diag::Span, String)>> {
    let ty = cx.ty(scrutinee)?;
    let plain = |it: &hir::Type<'_>| it.is_scalar() || it.is_unit();
    if !(plain(&ty) || ty.is_tuple() && ty.tuple_fields(cx.db()).iter().all(plain)) {
        return None;
    }
    let parent = construct.parent()?;
    let statement = match parent.kind() {
        K::EXPR_STMT => parent,
        K::STMT_LIST => construct,
        K::LET_STMT if ast::LetStmt::cast(parent)?.initializer()?.syntax() == construct => parent,
        _ => return None,
    };
    let at = cx.range(statement)?;
    let range = cx.range(scrutinee.syntax())?;
    cx.range(construct)?;
    let indent = indentation(cx.text(), at);
    // alloc: the fix's two parts
    Some(vec![
        (cx.span(TextRange::empty(at.start())), format!("let value = {};\n{indent}", cx.snippet(range))),
        (cx.span(range), "value".to_owned()), // alloc: as the parts
    ])
}

#[cfg(test)]
mod tests {
    use super::super::drops_tests::lint_only;

    const LINT: &[&str] = &["significant_drop_in_scrutinee"];
    const FOUND: &str = "through all its arms";

    #[test]
    fn a_guard_made_in_a_match_if_let_or_while_let_scrutinee_is_found() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::Mutex;
pub fn work(n: usize) {}
pub fn matched(m: &Mutex<Vec<u32>>) {
    match m.lock().unwrap().len() {
        0 => work(0),
        n => work(n),
    }
}
pub fn if_let(m: &Mutex<Vec<u32>>) -> u32 {
    if let Some(x) = m.lock().unwrap().first() { *x } else { 0 }
}
pub fn while_let(m: &Mutex<Vec<u32>>) {
    while let Some(x) = m.lock().unwrap().pop() {
        work(x as usize);
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains("the temporary `MutexGuard<"), "{found}");
        assert!(
            found.contains("made by `m.lock().unwrap()` holds the lock until this `match` ends, through all its arms, in `matched`"),
            "{found}"
        );
        assert!(found.contains("until this `if let` ends, through all its arms, in `if_let`"), "{found}");
        assert!(found.contains("until this `while let` ends, through all its arms, in `while_let`"), "{found}");
        assert!(found.contains("let value = m.lock().unwrap().len();"), "the `match` of a count is fixed:\n{found}");
        assert_eq!(found.matches("let value =").count(), 1, "a value borrowing the guard is not fixed:\n{found}");
    }

    /// Bound to a `let` first, matched on itself, made in a closure, or handed on by value:
    /// no guard is a temporary of the scrutinee.
    #[test]
    fn a_guard_bound_first_matched_itself_in_a_closure_or_moved_is_left_alone() {
        let found = lint_only(
            LINT,
            r#"
use std::sync::{Mutex, MutexGuard};
pub fn work(n: usize) {}
pub fn take(g: MutexGuard<'_, Vec<u32>>) -> usize { g.len() }
pub fn bound(m: &Mutex<Vec<u32>>) {
    let n = m.lock().unwrap().len();
    match n { 0 => work(0), n => work(n) }
}
pub fn itself(m: &Mutex<Vec<u32>>) {
    match m.lock().unwrap() { g => work(g.len()) }
}
pub fn in_closure(m: &Mutex<Vec<u32>>) {
    match (|| m.lock().unwrap().len())() { n => work(n) }
}
pub fn moved(m: &Mutex<Vec<u32>>) {
    match take(m.lock().unwrap()) { n => work(n) }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn a_renamed_lock_apis_guard_and_a_scrutinee_written_in_a_macro_call_are_found() {
        let found = lint_only(
            LINT,
            r#"
mod locks { pub use parking_lot::Mutex as Lock; }
use locks::Lock;
use std::sync::Mutex;
pub fn work(n: usize) {}
macro_rules! item { ($($t:tt)*) => { $($t)* }; }
pub fn parked(m: &Lock<Vec<u32>>) {
    match m.lock().len() { n => work(n) }
}
item! {
    pub fn handed(m: &Mutex<Vec<u32>>) {
        match m.lock().unwrap().len() { n => work(n) }
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("made by `m.lock()` holds the lock until this `match` ends"), "{found}");
        assert!(found.contains("in `parked`"), "{found}");
        assert!(found.contains("in `handed`"), "{found}");
    }

    /// The fix applied as it reads lints clean: the guard drops at the end of the `let`.
    #[test]
    fn the_fix_applied_lints_clean() {
        let before = r#"
use std::sync::Mutex;
pub fn work(n: usize) {}
pub fn matched(m: &Mutex<Vec<u32>>) {
    match m.lock().unwrap().len() {
        0 => work(0),
        n => work(n),
    }
}
"#;
        let found = lint_only(LINT, before);
        assert!(found.contains("let value = m.lock().unwrap().len();"), "{found}");
        let after = before.replace(
            "    match m.lock().unwrap().len() {",
            "    let value = m.lock().unwrap().len();\n    match value {",
        );
        assert_ne!(after, before);
        let left = lint_only(LINT, &after);
        assert!(left.is_empty(), "the fixed file lints clean:\n{after}\n{left}");
    }
}
