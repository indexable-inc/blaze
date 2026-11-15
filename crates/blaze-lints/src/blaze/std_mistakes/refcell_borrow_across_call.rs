//! `refcell_borrow_across_call`: a `RefCell`'s `borrow()` or `borrow_mut()` guard held while
//! the same function borrows the cell again or calls something that can reach it. A second
//! `borrow_mut` while any guard lives, or a `borrow` while a `borrow_mut` guard lives, panics
//! (`BorrowMutError`); a call handed the cell, or the value that holds it (`self` for
//! `self.cell`, the `Rc` it is in), can do the same out of sight, and does once someone adds
//! the read there.
//!
//! What is held: a guard bound by `let g = cell.borrow_mut();`, from the `let` to the end of
//! its block or a `drop(g)`, and a temporary guard for the rest of its expression
//! (`cell.borrow_mut().push(f(&cell))`). What is found while it is held, outside the guard's
//! own uses:
//!
//! - the same cell, written the same way, borrowed again where either borrow is `borrow_mut`;
//! - a call that is not std's, handed the cell or its root (the local or `self` the cell is a
//!   field of), as an argument or a receiver.
//!
//! Intraprocedural, and by the cell's written place: a call that reaches the cell through a
//! global, a thread-local or a clone of the `Rc` under another name is not seen, and a
//! `borrow` behind a branch that never runs with the guard held is still found. The right
//! form: end the guard before the call (`drop(g)`, or a block around it), or copy out what the
//! call needs. Outside test code. No fix.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasArgList as _},
};

use super::{
    bound, called, in_test, inherent_adt, parent_expr, receiver_of, report, strip, uses_of,
};
use crate::{
    blaze::{Lint, facts},
    check::Cx,
};

const LINT: Lint = Lint::RefcellBorrowAcrossCall;

/// How many statements after a guard's `let` are looked through.
const STATEMENTS: usize = 64;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(call) = ast::MethodCallExpr::cast(node) else { return };
    let Some(f) = cx.method(call) else { return };
    let held_mut = match f.name(cx.db()).as_str() {
        "borrow" => false,
        "borrow_mut" => true,
        _ => return,
    };
    if !of_refcell(cx, f) {
        return;
    }
    let Some(receiver) = call.receiver().map(strip) else { return };
    let cell = squeeze(&cx.source_of(receiver.syntax()));
    let root = root_of(cx, receiver);
    // alloc: the expressions the guard is held across, a few statements'
    let mut held: Vec<ast::Node<'t>> = Vec::new();
    let mut guard = None;
    if let Some(stmt) = parent_expr(node).and_then(ast::LetStmt::cast) {
        let Some(local) = stmt.pat().and_then(|it| bound(cx, it)) else { return };
        guard = Some(local);
        let mut next = stmt.syntax().next_sibling();
        for _ in 0..STATEMENTS {
            let Some(at) = next else { break };
            next = at.next_sibling();
            if dropped(cx, at, local) {
                break;
            }
            held.push(at);
        }
    } else if let Some(outer) = receiver_of(node)
        && let Some(args) = outer.arg_list()
    {
        held.push(args.syntax());
    } else {
        return;
    }
    if in_test(cx, node) {
        return;
    }
    let guard_uses: Vec<ast::Node<'t>> = match guard {
        Some(local) => held
            .iter()
            .flat_map(|at| uses_of(cx, *at, local))
            .map(|it| it.syntax())
            .collect(), // alloc: the guard's uses, a few
        None => Vec::new(), // alloc: none
    };
    for at in held {
        for inner in at.descendants() {
            let Some(conflict) = conflict(cx, inner, &cell, root, held_mut, &guard_uses) else {
                continue;
            };
            let Some(range) = cx.range(inner) else { continue };
            let how = if held_mut { "borrow_mut" } else { "borrow" };
            report(
                cx,
                LINT,
                inner,
                range,
                format!("{conflict} while the `{how}()` guard of `{cell}` is held"),
                "a `borrow_mut` while any guard of the cell lives, or a `borrow` while a `borrow_mut` guard lives, panics: end the guard first (`drop(g)` or a block around it), or copy out what the call needs",
                None,
            );
            return;
        }
    }
}

/// What `node` does that can meet the guard of `cell` (rooted at `root`) held: `None` when it
/// is nothing of the kind.
fn conflict<'q, 't>(
    cx: &Cx<'q, 't>,
    node: ast::Node<'t>,
    cell: &str,
    root: Option<Root<'q>>,
    held_mut: bool,
    guard_uses: &[ast::Node<'t>],
) -> Option<&'static str> {
    if let Some(call) = ast::MethodCallExpr::cast(node) {
        let f = cx.method(call)?;
        let receiver = strip(call.receiver()?);
        if guard_uses.contains(&receiver.syntax()) {
            return None;
        }
        let name = f.name(cx.db());
        if matches!(name.as_str(), "borrow" | "borrow_mut")
            && of_refcell(cx, f)
            && squeeze(&cx.source_of(receiver.syntax())) == cell
        {
            let again = held_mut || name.as_str() == "borrow_mut";
            return again.then_some("this borrows the cell again");
        }
        if std_fn(cx, f) {
            return None;
        }
        let args = call.arg_list().into_iter().flat_map(|it| it.args());
        if std::iter::once(receiver).chain(args).any(|it| reaches(cx, it, cell, root)) {
            return Some("this call is handed the cell or what holds it");
        }
        return None;
    }
    let call = ast::CallExpr::cast(node)?;
    let f = called(cx, call)?;
    if std_fn(cx, f) {
        return None;
    }
    let mut args = call.arg_list()?.args();
    let handed = args.any(|it| reaches(cx, it, cell, root));
    handed.then_some("this call is handed the cell or what holds it")
}

/// Whether `f` is a method of `RefCell`'s inherent impl: the ADT std names
/// `#[rustc_diagnostic_item = "RefCell"]`.
fn of_refcell(cx: &Cx<'_, '_>, f: hir::Function) -> bool {
    let db = cx.db();
    let refcell = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("RefCell"));
    refcell.is_some() && inherent_adt(cx, f) == refcell
}

/// The local or `self` a cell's place is rooted at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Root<'q> {
    Local(hir::Local<'q>),
    SelfParam,
}

/// The root of the place `expr`: the local or `self` its fields start from.
fn root_of<'q, 't>(cx: &Cx<'q, 't>, expr: ast::Expr<'t>) -> Option<Root<'q>> {
    let mut at = strip(expr);
    for _ in 0..16 {
        match at {
            ast::Expr::FieldExpr(field) => at = strip(field.expr()?),
            ast::Expr::PathExpr(path) => {
                let path = path.path()?;
                if path.qualifier().is_none()
                    && path.segment()?.name_ref().is_some_and(|it| it.self_token().is_some())
                {
                    return Some(Root::SelfParam);
                }
                return match cx.path(path)? {
                    hir::PathResolution::Local(it) => Some(Root::Local(it)),
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// Whether the argument `expr` (past `&`, `&mut`) is the cell or its root.
fn reaches<'q, 't>(
    cx: &Cx<'q, 't>,
    expr: ast::Expr<'t>,
    cell: &str,
    root: Option<Root<'q>>,
) -> bool {
    let mut at = strip(expr);
    if let ast::Expr::RefExpr(reference) = at
        && let Some(inner) = reference.expr()
    {
        at = strip(inner);
    }
    if squeeze(&cx.source_of(at.syntax())) == cell {
        return true;
    }
    let ast::Expr::PathExpr(_) = at else { return false };
    root.is_some() && root_of(cx, at) == root
}

/// Whether `f` is std's (`core`, `alloc`, `std`), which reaches no user's cell but through a
/// closure it is handed.
fn std_fn(cx: &Cx<'_, '_>, f: hir::Function) -> bool {
    let db = cx.db();
    let krate = f.module(db).krate(db);
    ["core", "alloc", "std"].iter().any(|it| facts::crate_is(db, krate, it))
}

/// Whether the statement `stmt` is `drop(g)` of the guard `local`.
fn dropped<'q, 't>(cx: &Cx<'q, 't>, stmt: ast::Node<'t>, local: hir::Local<'q>) -> bool {
    if stmt.kind() != SyntaxKind::EXPR_STMT {
        return false;
    }
    let Some(ast::Expr::CallExpr(call)) = stmt.children().find_map(ast::Expr::cast).map(strip)
    else {
        return false;
    };
    let Some(f) = called(cx, call) else { return false };
    if f.name(cx.db()).as_str() != "drop" || !std_fn(cx, f) {
        return false;
    }
    !uses_of(cx, call.syntax(), local).is_empty()
}

/// `text` without its whitespace.
fn squeeze(text: &str) -> String {
    text.chars().filter(|it| !it.is_whitespace()).collect()
}
