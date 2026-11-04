//! `push_without_reserve`: a `Vec` or `String` made empty (`new()`, `default()`, `vec![]`) and
//! then pushed to once per iteration of a `for` loop whose trip count is known before the loop
//! starts (a range of integers, or an iterator that is `ExactSizeIterator`). Each time the
//! buffer fills it is reallocated and copied, about log2(n) times; `with_capacity(n)` allocates
//! once. Goose grows a bump region in place for the same reason.
//!
//! Found: `let mut v = Vec::new();` (or `String`) in a block, the same block's next statement
//! that names `v` is the `for` loop, and the loop's own body pushes to `v` (`push`, a `String`'s
//! `push`/`push_str`) exactly once as one of its statements, not under an `if`, a `match` or an
//! inner loop, with no `break`, `continue` or `return` in the body (so it runs the full count).
//! A `let` inside another loop is left to `scratch_in_loop` when that is on (hoisting keeps the
//! capacity across rounds, which beats reserving each round). clippy's `vec_init_then_push`
//! finds pushes right after the `let`, never a loop, so the two don't meet.
//!
//! For a `String`, `with_capacity(n)` reserves `n` bytes: a lower bound when each push writes
//! one character or more. No fix: the count's expression is the loop's, which may borrow what
//! the `with_capacity` can't see yet.

use hir_def::{
    DefWithBodyId,
    hir::{Expr, ExprId},
    signatures::FunctionSignature,
};

use super::{
    Finding, Lint,
    alloc::written_callee,
    exempt, facts,
    goose_locals::{self, Coll},
    hash_order_output::local_of,
};
use crate::check::Cx;

const LINT: Lint = Lint::PushWithoutReserve;

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
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let exact = cx.std_trait(&["core", "iter", "ExactSizeIterator"]);
    let scratch_on = cx.on_blaze(Lint::ScratchInLoop);
    let mut parents = None;
    for (block, expr) in body.exprs() {
        let Expr::Block { statements, tail, .. } = expr else { continue };
        let lets = goose_locals::lets(body, block);
        if lets.is_empty() {
            continue;
        }
        // alloc: the statements' expressions of one block, then its tail
        let rest: Vec<(usize, ExprId)> = statements
            .iter()
            .enumerate()
            .filter_map(|(at, it)| match it {
                hir_def::hir::Statement::Expr { expr, .. } => Some((at, expr)),
                hir_def::hir::Statement::Let { initializer: Some(init), .. } => Some((at, init)),
                _ => None,
            })
            .chain(tail.map(|it| (usize::MAX, it)))
            .collect();
        for made in lets {
            let Some((coll @ (Coll::Vec | Coll::String), "new" | "default")) =
                goose_locals::made(cx, analysis, made.init)
            else {
                continue;
            };
            let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
            if scratch_on
                && parents.up(block, 64).any(|it| matches!(body.expr(it), Expr::Loop { .. }))
            {
                continue;
            }
            // The next statement after the `let` that names the local.
            let Some(&(_, next)) = rest
                .iter()
                .filter(|(at, _)| *at > made.at)
                .find(|(_, it)| goose_locals::mentions(db, def, body, *it, made.local))
            else {
                continue;
            };
            let Some(for_) = goose_locals::for_loop(body, next) else { continue };
            let iterator = analysis.ty(analysis.infer().expr_ty(for_.iterator));
            let range = iterator.as_adt().is_some_and(|it| facts::adt_is(db, it, "core", "Range"));
            let sized = exact.is_some_and(|t| iterator.impls_trait(db, t, &[]));
            if !(range || sized) {
                continue;
            }
            if pushes_once(cx, def, analysis, for_.body, made.local, coll) != Some(true) {
                continue;
            }
            let Some(at) = analysis.expr_range(made.init) else { continue };
            if exempt::exempt(cx.text(), at, LINT) {
                continue;
            }
            let count = match (range, analysis.expr_range(for_.head)) {
                (true, Some(head)) => match cx.snippet(head).trim().split_once("..") {
                    Some(("0" | "", end)) => end.trim().to_owned(), // alloc: a message's part
                    Some((start, end)) => format!("{} - {}", end.trim(), start.trim()),
                    None => "n".to_owned(), // alloc: a message's part
                },
                (false, Some(head)) => format!("{}.len()", cx.snippet(head).trim()),
                _ => "n".to_owned(), // alloc: a message's part
            };
            let name = analysis.body()[made.local].name.as_str().to_owned(); // alloc: a message's part
            let message = format!(
                "`{name}` is made empty, then pushed to once per iteration of a loop that runs `{count}` times, known \
                 before it starts: it reallocates as it grows"
            );
            let note = format!(
                "`{}::with_capacity({count})` allocates once instead of about log2({count}) times, each copying what was \
                 pushed so far{}",
                coll.say(),
                match coll {
                    Coll::String => " (for a `String` the capacity is in bytes: a lower bound)",
                    _ => "",
                }
            );
            facts::push(&mut found, LINT, at, message, Some(note));
        }
    }
    found
}

/// Whether the loop body `body_expr` pushes to `local` exactly once per iteration, as one of
/// its own statements, with no `break`, `continue` or `return`. `None` when it does not.
fn pushes_once(
    cx: &Cx<'_, '_>,
    def: DefWithBodyId,
    analysis: &hir::BodyAnalysis<'_>,
    body_expr: ExprId,
    local: hir_def::hir::BindingId,
    coll: Coll,
) -> Option<bool> {
    let db = cx.db();
    let body = analysis.body();
    let Expr::Block { statements, tail, .. } = body.expr(body_expr) else { return None };
    let mut leaves = false;
    let walked = super::general_facts::walk(body, body_expr, |it| {
        leaves |= matches!(
            body.expr(it),
            Expr::Break { .. } | Expr::Continue { .. } | Expr::Return { .. }
        );
    });
    if leaves || walked != super::general_facts::Walked::Done {
        return None;
    }
    let is_push = |it: ExprId| {
        let Expr::MethodCall { receiver, .. } = body.expr(it) else { return false };
        let Some((f, _)) = written_callee(analysis.infer(), body, it) else { return false };
        let name = FunctionSignature::of(db, f).name.as_str();
        let ok = match coll {
            Coll::Vec => name == "push",
            _ => matches!(name, "push" | "push_str"),
        };
        ok && local_of(db, def, body, receiver) == Some(local)
    };
    let top = statements
        .iter()
        .filter_map(|it| match it {
            hir_def::hir::Statement::Expr { expr, .. } => Some(expr),
            _ => None,
        })
        .chain(tail)
        .filter(|it| is_push(*it))
        .count();
    let mut all = 0;
    super::general_facts::walk(body, body_expr, |it| all += usize::from(is_push(it)));
    Some(top == 1 && all == 1)
}
