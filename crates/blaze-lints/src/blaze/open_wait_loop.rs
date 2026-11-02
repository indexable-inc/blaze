//! `open_wait_loop`: a loop that waits for progress with no bound (AGENTS.md rule 13: every
//! loop that waits for progress has fuel, a depth or a deadline, and ends in a typed outcome).
//!
//! A `loop` or `while` loop waits when its body, outside the loops and closures inside it,
//! calls `std::thread::sleep`, `yield_now`, `park` or `park_timeout`, `spin_loop` (core's or
//! std's), a `Condvar` wait (std's, `parking_lot`'s), or `blaze_graph`'s `wait` or
//! `wait_until`: it sleeps, spins or waits, and goes round to try again. It is bounded when a
//! condition it ends on compares something: the `while` condition, or the `if` condition or
//! match guard around a `break` of the loop or a `return` in it, holding an ordering comparison
//! (`tries < 8`, `start.elapsed() > timeout`) or an equality of integers (`left == 0`). A loop
//! with no way out is a service loop, not a wait, and is left to `clippy::infinite_loop`; a `for`
//! loop runs over its items and is bounded by them.
//!
//! Calls are resolved, so a renamed import (`use std::thread::sleep as nap`) and a loop a macro
//! writes count. A `?` or an unguarded `break` in a match arm ends the loop on what it matched,
//! not on a bound, and does not make it bounded.

use base_db::Db;
use hir_def::{
    FunctionId,
    expr_store::Body,
    hir::{BinaryOp, CmpOp, Expr, ExprId, LabelId, LoopSource},
    signatures::FunctionSignature,
};
use hir::AsAssocItem as _;

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, Parents},
};
use crate::check::Cx;

const LINT: Lint = Lint::OpenWaitLoop;

/// How many expressions up an exit is followed to its loop.
const UP: usize = 64;

/// The waiting free functions, by their path.
const FREE: &[&[&str]] = &[
    &["std", "thread", "sleep"],
    &["std", "thread", "park"],
    &["std", "thread", "park_timeout"],
    &["core", "hint", "spin_loop"],
    &["std", "hint", "spin_loop"],
];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let db = cx.db();
    let body = analysis.body();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let Expr::Loop { body: inner, label, source } = expr else { continue };
        let kind = match source {
            LoopSource::Loop => "`loop`",
            LoopSource::While => "`while` loop",
            _ => continue,
        };
        let Some((wait, what)) = first_wait(cx, analysis, inner) else { continue };
        let parents = parents.get_or_insert_with(|| Parents::of(body));
        let exits = exits(body, parents, id, inner, label, source);
        if exits.is_empty() || exits.iter().any(|&guard| guard.is_some_and(|it| compares(analysis, it))) {
            continue;
        }
        let Some(range) = analysis.expr_range(wait) else { continue };
        let function = match analysis.def() {
            hir::DefWithBody::Function(f) => format!("`{}`", f.name(db).as_str()),
            _ => "this item".to_owned(), // alloc: a message's part
        };
        let until = exits
            .iter()
            .flatten()
            .find_map(|&guard| analysis.expr_range(guard))
            .map_or_else(|| "for what its `break` matches".to_owned(), |it| format!("for `{}`", cx.snippet(it))); // alloc: a message's part
        let message = format!(
            "this {kind} in {function} waits with {what} {until}, and nothing bounds it: no count, depth or \
             deadline is compared where it ends"
        );
        let note = "a loop that waits for progress gives up after some fuel, depth or deadline (from the \
                    injected clock), and ends in an error naming what it waited for and how far it got";
        facts::push(&mut found, LINT, range, message, Some(note.to_owned())); // alloc: reported at the edge
    }
    found
}

/// The first wait of the loop body `inner`, outside the loops and closures in it, with what it
/// is as the message names it.
fn first_wait(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, inner: ExprId) -> Option<(ExprId, String)> {
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let mut stack = vec![inner]; // alloc: the expressions of one loop body
    // alloc: the waits of one loop body, a few
    let mut waits: Vec<(ExprId, String)> = Vec::new();
    while let Some(id) = stack.pop() {
        match body.expr(id) {
            Expr::Loop { .. } | Expr::Closure { .. } => continue,
            Expr::Call { .. } | Expr::MethodCall { .. } => {
                if let Some((f, _)) = written_callee(infer, body, id)
                    && let Some(what) = waits_with(cx, db, f)
                {
                    waits.push((id, what));
                }
            }
            _ => {}
        }
        body.walk_child_exprs(id, |child| stack.push(child));
    }
    waits.into_iter().min_by_key(|(id, _)| analysis.expr_range(*id).map(|it| it.start()))
}

/// Whether a call of `f` sleeps, spins or waits: what it is, as the message names it.
fn waits_with(cx: &Cx<'_, '_>, db: &Db, f: FunctionId) -> Option<String> {
    let function = hir::Function::from(f);
    let name = FunctionSignature::of(db, f).name.as_str();
    let krate = function.module(db).krate(db);
    if let Some(path) = FREE.iter().find(|path| cx.std_def(path) == Some(hir::ModuleDef::Function(function))) {
        return Some(format!("`{}`", path.join("::")));
    }
    if function.as_assoc_item(db).is_none() {
        if name == "yield_now" && facts::crate_is(db, krate, "std") {
            return Some("`std::thread::yield_now`".to_owned()); // alloc: a message's part
        }
        if matches!(name, "wait" | "wait_until") && facts::crate_is(db, krate, "blaze_graph") {
            return Some(format!("`blaze_graph`'s `{name}`"));
        }
        return None;
    }
    let adt = facts::impl_adt(db, f)?;
    let condvar = cx.std_def(&["std", "sync", "Condvar"]) == Some(hir::ModuleDef::Adt(adt))
        || facts::adt_is(db, adt, "parking_lot", "Condvar");
    (condvar && name.starts_with("wait")).then(|| format!("`Condvar::{name}`"))
}

/// The ways out of the loop `id` (its body `inner`), each with the condition it is taken on:
/// the `while` condition, and for each `break` of the loop and `return` in it the nearest `if`
/// condition or match guard around it inside the loop, `None` when there is none.
fn exits(
    body: &Body,
    parents: &Parents,
    id: ExprId,
    inner: ExprId,
    label: Option<LabelId>,
    source: LoopSource,
) -> Vec<Option<ExprId>> {
    // alloc: the exits of one loop, a few
    let mut out = Vec::new();
    if source == LoopSource::While
        && let Expr::If { condition, .. } = body.expr(inner)
    {
        out.push(Some(condition));
    }
    // Each expression with whether it is inside a loop nested in this one.
    let mut stack = vec![(inner, false)]; // alloc: the expressions of one loop body
    while let Some((at, nested)) = stack.pop() {
        let exit = match body.expr(at) {
            Expr::Closure { .. } => continue,
            Expr::Break { label: target, .. } => match target {
                Some(it) => Some(it) == label,
                None => !nested,
            },
            Expr::Return { .. } => true,
            _ => false,
        };
        if exit {
            out.push(guard_of(body, parents, at, id));
        }
        let nested = nested || matches!(body.expr(at), Expr::Loop { .. });
        body.walk_child_exprs(at, |child| stack.push((child, nested)));
    }
    out
}

/// The nearest `if` condition or match guard that `exit` is taken on, inside the loop `id`.
fn guard_of(body: &Body, parents: &Parents, exit: ExprId, id: ExprId) -> Option<ExprId> {
    let mut child = exit;
    for parent in parents.up(exit, UP) {
        if parent == id {
            return None;
        }
        match body.expr(parent) {
            Expr::If { condition, .. } if condition != child => return Some(condition),
            Expr::Match { arms, .. } => {
                if let Some(guard) = arms.iter().find(|arm| arm.expr == child).and_then(|arm| arm.guard) {
                    return Some(guard);
                }
            }
            _ => {}
        }
        child = parent;
    }
    None
}

/// Whether `condition` compares something: an ordering comparison, or an equality of
/// integers, outside the closures in it.
fn compares(analysis: &hir::BodyAnalysis<'_>, condition: ExprId) -> bool {
    let body = analysis.body();
    let infer = analysis.infer();
    let mut stack = vec![condition]; // alloc: the expressions of one condition
    while let Some(id) = stack.pop() {
        match body.expr(id) {
            Expr::Closure { .. } => continue,
            Expr::BinaryOp { op: Some(BinaryOp::CmpOp(CmpOp::Ord { .. })), .. } => return true,
            Expr::BinaryOp { lhs, op: Some(BinaryOp::CmpOp(CmpOp::Eq { .. })), .. }
                if analysis.ty(infer.expr_ty(lhs)).strip_references().is_int_or_uint() =>
            {
                return true;
            }
            _ => {}
        }
        body.walk_child_exprs(id, |child| stack.push(child));
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::lints::drops_tests::blaze_only;

    const LINT: &str = "open_wait_loop";
    const FOUND: &str = "and nothing bounds it";

    #[test]
    fn a_loop_that_sleeps_spins_or_waits_until_success_is_found() {
        let found = blaze_only(
            LINT,
            r#"
use std::{hint::spin_loop, thread, time::Duration};
pub fn ready() -> bool { true }
pub fn connect() -> Result<u32, u32> { Ok(1) }
pub fn poll() {
    while !ready() {
        thread::sleep(Duration::from_millis(10));
    }
}
pub fn spin() {
    loop {
        if ready() {
            break;
        }
        spin_loop();
    }
}
pub fn retry() -> u32 {
    loop {
        match connect() {
            Ok(it) => return it,
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(
            found.contains("this `while` loop in `poll` waits with `std::thread::sleep` for `!ready()`, and nothing bounds it"),
            "{found}"
        );
        assert!(found.contains("this `loop` in `spin` waits with `std::hint::spin_loop` for `ready()`"), "{found}");
        assert!(found.contains("this `loop` in `retry` waits with `std::thread::sleep` for what its `break` matches"), "{found}");
    }

    /// Bounded by a count, a deadline or an integer reaching zero, a service loop with no
    /// way out, a `for` loop, a loop that waits only in a closure or an inner loop, and a
    /// loop that never waits: none is an open wait.
    #[test]
    fn a_bounded_service_for_or_non_waiting_loop_is_left_alone() {
        let found = blaze_only(
            LINT,
            r#"
use std::{thread, time::{Duration, Instant}};
pub fn ready() -> bool { true }
pub fn counted() -> bool {
    let mut tries = 0;
    while !ready() {
        if tries > 8 { return false; }
        tries += 1;
        thread::sleep(Duration::from_millis(10));
    }
    true
}
pub fn deadline(timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if ready() { return true; }
        if start.elapsed() > timeout { return false; }
        thread::sleep(Duration::from_millis(1));
    }
}
pub fn fuel(mut left: u32) {
    while left != 0 && !ready() {
        left -= 1;
        thread::yield_now();
    }
}
pub fn service() -> ! {
    loop {
        thread::sleep(Duration::from_millis(100));
    }
}
pub fn each(items: &[u32]) {
    for _ in items {
        thread::sleep(Duration::from_millis(1));
    }
}
pub fn later() {
    while !ready() {
        let f = || thread::sleep(Duration::from_millis(1));
    }
}
pub fn busy() {
    let mut n = 0u32;
    while !ready() {
        n += 1;
    }
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn a_renamed_wait_and_a_macro_written_loop_are_found() {
        let found = blaze_only(
            LINT,
            r#"
use std::thread::sleep as nap;
use std::time::Duration;
pub fn ready() -> bool { true }
macro_rules! wait_for { ($c:expr) => { while !$c { std::thread::sleep(Duration::from_millis(1)); } }; }
pub fn renamed() {
    while !ready() {
        nap(Duration::from_millis(1));
    }
}
pub fn handed() {
    wait_for!(ready());
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("in `renamed` waits with `std::thread::sleep`"), "{found}");
        assert!(found.contains("in `handed` waits with `std::thread::sleep`"), "{found}");
    }
}
