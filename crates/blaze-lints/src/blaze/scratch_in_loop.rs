//! `scratch_in_loop`: a `Vec`, `String` or hash map made inside a loop's body, grown there,
//! and dropped at the end of each iteration. Every round allocates a fresh buffer, grows it
//! from nothing (reallocating as it fills) and frees it. Made once before the loop and
//! `.clear()`ed each round, it keeps its capacity: after the first rounds no round allocates.
//! Goose keeps such scratch on a data stack that each round resets.
//!
//! Found: a plain `let` in a block whose innermost enclosing loop is a `loop`, `while` or `for`
//! (no closure between them), bound to `new()`, `default()`, `with_capacity(..)` (or
//! `vec![]`) of std's `Vec`, `String`, `HashMap` or `HashSet` (or hashbrown's), when every use
//! of the local only borrows it (a method through an autoref, `&x`, `&mut x`, `x[i]`) and one
//! grows it (`push`, `insert`, `extend`, ...). Nothing moves it out, so it dies at the end of
//! the round. A collection whose type holds a reference is left alone: hoisted, it would have
//! to outlive what each round borrows.
//!
//! Not found: a buffer built in an iterator closure (`map(|x| ..)`), whose hoisting needs a
//! `RefCell` or a `fold`. No fix: where the buffer goes and when it is cleared is the author's
//! choice.

use hir_def::{DefWithBodyId, hir::Expr};

use super::{Finding, Lint, exempt, facts, goose_locals};
use crate::check::Cx;

const LINT: Lint = Lint::ScratchInLoop;

/// How many expressions up a block is followed to its loop.
const UP: usize = 64;

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
    let mut parents = None;
    for (block, expr) in body.exprs() {
        if !matches!(expr, Expr::Block { .. }) {
            continue;
        }
        let lets = goose_locals::lets(body, block);
        if lets.is_empty() {
            continue;
        }
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        let in_loop = parents
            .up(block, UP)
            .find(|it| matches!(body.expr(*it), Expr::Loop { .. } | Expr::Closure { .. }))
            .is_some_and(|it| matches!(body.expr(it), Expr::Loop { .. }));
        if !in_loop {
            continue;
        }
        for made in lets {
            let Some((coll, _)) = goose_locals::made(cx, analysis, made.init) else { continue };
            let ty = analysis.ty(analysis.infer().expr_ty(made.init));
            if ty.contains_reference(db) {
                continue;
            }
            let uses = goose_locals::uses(db, def, analysis, parents, made.local);
            if !uses.only_borrowed || !uses.grown {
                continue;
            }
            let Some(at) = analysis.expr_range(made.init) else { continue };
            if exempt::exempt(cx.text(), at, LINT) {
                continue;
            }
            let name = body[made.local].name.as_str();
            let message = format!(
                "`{name}` is a `{}` made, grown and dropped in every iteration of the loop: each round allocates it again",
                coll.say()
            );
            let note = format!(
                "make `{name}` once before the loop and call `{name}.clear()` at the start of each round: it keeps its \
                 capacity, so after the first rounds nothing allocates (Goose's scratch on the data stack)"
            );
            facts::push(&mut found, LINT, at, message, Some(note));
        }
    }
    found
}
