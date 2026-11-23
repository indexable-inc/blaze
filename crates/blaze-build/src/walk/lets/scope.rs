//! The drop scope of an `if let` / `while let` condition: where its scrutinee temporaries and
//! its bindings live, and where they drop, on the matched path and on every failed link.
//!
//! The Reference, "Destructors":
//! - "Temporary scopes" (`[destructors.scope.temporary]`): "the temporary scope of an
//!   expression is the smallest scope that contains the expression and is one of: ... The
//!   pattern-matching condition(s) and consequent body of `if` ([destructors.scope.temporary.
//!   edition2024])", "The pattern-matching condition and loop body of `while`", "Each operand
//!   of a lazy boolean expression"; and the 2024 edition change ("if let temporary scope"): an
//!   `if let`'s scrutinee temporaries are dropped before the `else` block, where in editions
//!   2021 and earlier they live to the end of the enclosing statement (the `if let` was a
//!   `match`, whose "scrutinee ... is not a temporary scope").
//! - "Place expressions and value expressions": the scrutinee of `if let`, `while let` and
//!   `match` is a place expression context: a PLACE scrutinee (a local, a field, a deref) is
//!   tested where it is, nothing is created and nothing is dropped (walk/lets.rs
//!   `push_scrutinee`: `Task::Place`; walk/matches.rs `tested_place`); a VALUE scrutinee makes
//!   a temporary.
//! - "Drop scopes" (`[destructors.scope.intro]`): "When control flow leaves a drop scope all
//!   variables associated to that scope are dropped in reverse order of declaration (for
//!   variables) or creation (for temporaries)."
//!
//! One mechanism, the one every temporary has: `if_let_open` opens ONE region at the start of
//! the `if let`: a drop scope (`scope_marks`) for the condition's bindings and, in edition 2024,
//! the temporary scope anchored at the `if`'s condition (`temp_marks`/`temp_scopes`) at the same
//! mark, so a scrutinee temporary (walk/moves.rs `own_scrutinee` -> walk/temps.rs `hold_temp`,
//! anchored here by `let_anchor`) and a binding (`bind_tested` -> `own`) sit in it in order of
//! creation. Its end:
//! - matched: after the consequent block, `Task::ScopeEnd(then)` drops the whole region, last
//!   made first (each binding, then what the bindings left of its scrutinee by the scrutinee's
//!   word), then `Task::TempScopeEnd(cond)` closes the (now empty) temporary scope;
//! - a failed link: the test jumps to a block of its own (`link_fail`) that drops exactly what
//!   the region holds at that link (the earlier links' bindings and temporaries, and this
//!   link's whole scrutinee: nothing was moved on this path), with `drop_from`, then goes to
//!   the `else` block. Every link knows statically what exists, so no drop flag is needed;
//!   only a scrutinee a by-value binding moves part of carries its word (`Held::Moved`).
//! - `break`, `continue`, `return` and `?` out of the consequent drop the region with every
//!   scope they leave (`drop_from` over `scope_pats`); a call that may unwind drops it on its
//!   cleanup path the same way (walk/unwind.rs: `drop_from(0)`, every entry of `scope_pats`),
//!   so nothing here is added for them.
//! - `while let` is the lowering's `loop { if let P = e { body } else { break } }`: the region
//!   is the iteration's, dropped at the body's end each time, and before the `break` on the
//!   miss.
//!
//! The edition is the body's (`BodyView::edition`, the crate's edition as the checker read it).
//! Edition 2021 and earlier: the region holds only the bindings; a scrutinee temporary anchors
//! past the `if` (`let_anchor`: `LetAnchor::Above`), to the enclosing statement or the scope
//! the `if` sits in, dropped there on both paths; a `let` chain there is refused by name (chains
//! are edition 2024).

use core::alloc::Allocator;

use blaze_lir::Block;
use blaze_typeck::{BinOp, Edition, Expr, ExprId};

use super::super::{NO_EXPR, Walk, malformed, unsupported};
use crate::BuildError;

/// Where a temporary in a `let` condition's scrutinee is held (walk/temps.rs `temp_anchor`).
pub(in crate::walk) enum LetAnchor {
    /// Edition 2024: the temporary scope of the `if`'s condition, opened by `if_let_open`.
    Cond(u32),
    /// Edition 2021 and earlier: the `if` expression itself; the search for the temporary's
    /// scope goes on from it, as from any expression.
    Above(ExprId),
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether this body follows the edition-2024 rule for `if let` temporaries.
    pub(in crate::walk) fn if_let_2024(&self) -> bool {
        self.body.edition >= Edition::E2024
    }

    /// The start of `if let` expression `e` (condition `cond`, `links` links): opens its region
    /// (the module's doc). `true` when the temporary scope was opened too (edition 2024): the
    /// caller then queues `Task::TempScopeEnd(cond)` after the region's `Task::ScopeEnd`.
    pub(in crate::walk) fn if_let_open(&mut self, e: ExprId, cond: ExprId, links: usize) -> Result<bool, BuildError> {
        let edition_2024 = self.if_let_2024();
        if links > 1 && !edition_2024 {
            return Err(unsupported(e, "a `let` chain before edition 2024 (chains are an edition-2024 form)"));
        }
        let at = self.scope_pats.len() as u32;
        self.scope_marks.push(at);
        if edition_2024 {
            // The temporary scope "The pattern-matching condition(s) and consequent body of
            // `if`", at the same mark, nested in the drop scope just opened: a temporary
            // held in it goes before any scope opened later (walk/temps.rs `hold_temp`).
            self.temp_marks.push(at);
            self.temp_scopes.push((cond.0, self.scope_marks.len() as u32));
        }
        Ok(edition_2024)
    }

    /// The anchor of a temporary in the scrutinee of `let` expression `l` (walk/temps.rs
    /// `temp_anchor`, reaching `l` from its scrutinee): up through the `&&` chain `l` is a link
    /// of, to the `if` whose condition it is.
    pub(in crate::walk) fn let_anchor(&self, l: ExprId) -> Result<LetAnchor, BuildError> {
        let mut c = l.0;
        loop {
            let (up, _) = *self.expr_parent.get(c as usize).ok_or(malformed("an expression past `exprs`"))?;
            if up == NO_EXPR {
                return Err(malformed("a `let` condition outside an `if`"));
            }
            match self.expr(ExprId(up))? {
                Expr::Binary { op: BinOp::And, .. } | Expr::Paren(_) => c = up,
                Expr::If { cond, .. } if cond.0 == c => {
                    return Ok(if self.if_let_2024() { LetAnchor::Cond(cond.0) } else { LetAnchor::Above(ExprId(up)) });
                }
                _ => return Err(unsupported(l, "a `let` expression that is not a link of an `if` or `while` condition")),
            }
        }
    }

    /// The block a failed test of an `if let` link goes to, given the `else` block `fail`:
    /// `fail` itself when the region holds nothing that may need a drop here, else a new block
    /// `link_fail_end` fills once the test is built.
    pub(in crate::walk) fn link_fail(&mut self, fail: Block) -> Result<(Block, bool), BuildError> {
        let mark = *self.scope_marks.last().ok_or(malformed("an `if let` link outside its region"))? as usize;
        if !self.pending_from(mark) {
            return Ok((fail, false));
        }
        Ok((self.create(), true))
    }

    /// Fills block `undo` (from `link_fail`): the region's entries dropped, last made first,
    /// each by its static state or its word (`drop_from`, leaving the walk's state as it is:
    /// the matched path still holds them), then on to the `else` block `fail`. The current
    /// block when this is called is left as it was: the caller begins its own next block.
    pub(in crate::walk) fn link_fail_end(&mut self, undo: Block, fail: Block) -> Result<(), BuildError> {
        let mark = *self.scope_marks.last().ok_or(malformed("an `if let` link outside its region"))? as usize;
        self.begin(undo);
        self.drop_from(mark)?;
        self.jump(fail, &[]);
        Ok(())
    }
}
