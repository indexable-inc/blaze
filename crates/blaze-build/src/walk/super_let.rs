//! `super let` (async plan P11, D-A11): a binding that outlives the block it is written in.
//!
//! The library's `pin!` (core/src/pin.rs, the pinned toolchain) is
//! `{ super let mut pinned: PinMacroHelper<_> = PinMacroHelper { value: $value }; .. }`, its
//! comment: "The value is pinned: it is the local above which cannot be named outside this
//! macro", and the block's value is a pointer to that local. Built as a plain `let`, the local
//! is dropped at the block's end and the pointer names a dead slot: wrong code, silently.
//!
//! The rule built here: a `super let` binding is owned by the scope a temporary of its block's
//! value would be owned by. The Reference, "Temporary lifetime extension": "The temporary scopes
//! for expressions in `let` statements are sometimes extended to the scope of the block
//! containing the `let` statement", for an expression that is extending ("Extending based on
//! expressions": the initializer of a `let`, the operand of an extending borrow, the final
//! expression of an extending block, ...); "Temporary scopes": "the temporary scope of an
//! expression is the smallest scope that contains the expression". So:
//! - the block is an extending expression of a `let`'s initializer (`let p = { super let v =
//!   ..; &v };`, the common `let f = pin!(fut);`): the binding is declared in the drop scope of
//!   the block holding that `let`, just before the `let`'s own bindings (where an extended
//!   temporary goes, walk/temps.rs `hold_temp`), dropped at that block's end in reverse order;
//! - otherwise (`f(pin!(fut));`): the binding belongs to the block's temporary scope (the
//!   smallest one holding the block, usually its statement) and is dropped at that scope's end,
//!   after the statement's value, with its temporaries in reverse order of creation.
//!
//! The bindings were owned by `own_pat` in the innermost drop scope (the block's); here they
//! are moved to that scope, in declaration order. Forms not built are refused by name: a block
//! under an extending `if` branch or `match` arm (the binding would need a drop flag, as an
//! extended temporary there does), and `super let .. else`.

use core::alloc::Allocator;

use blaze_typeck::{Expr, ExprId, PatId, Stmt, StmtId};

use super::{Held, Walk, malformed, unsupported};
use crate::BuildError;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The `super let` statement whose pattern is `pat` (`BodyView::super_lets`), if any.
    fn super_let_of(&self, pat: PatId) -> Result<Option<StmtId>, BuildError> {
        for &s in self.body.super_lets {
            match self.body.stmts.get(s.ix()).copied().ok_or(malformed("a `super let` past `stmts`"))? {
                Stmt::Let { pat: p, .. } if p.0 == pat.0 => return Ok(Some(s)),
                Stmt::Let { .. } => {}
                _ => return Err(malformed("a `super let` entry that is not a `let` statement")),
            }
        }
        Ok(None)
    }

    /// The block expression whose statement list holds statement `s`.
    fn block_of_stmt(&self, s: StmtId) -> Result<ExprId, BuildError> {
        for (i, e) in self.body.exprs.iter().enumerate() {
            if let Expr::Block { stmts, .. } = e {
                let ss = self.body.stmt_lists.get(stmts.range()).ok_or(malformed("a block's statements past `stmt_lists`"))?;
                if ss.iter().any(|x| x.0 == s.0) {
                    return Ok(ExprId(i as u32));
                }
            }
        }
        Err(malformed("a `super let` in no block"))
    }

    /// The start of statement `s` (`stmt`): a `super let` with an `else` is refused by name.
    pub(super) fn super_let_form(&self, s: StmtId, stmt: Stmt) -> Result<(), BuildError> {
        if let Stmt::Let { else_: Some(x), .. } = stmt
            && self.body.super_lets.binary_search_by(|q| q.0.cmp(&s.0)).is_ok()
        {
            return Err(unsupported(x, "a `super let` with an `else` block"));
        }
        Ok(())
    }

    /// After the `let` whose pattern is `pat` owned its bindings (the entries `from..` of
    /// `scope_pats`, all `Held::Pat`): when it is a `super let`, moves them to the scope a
    /// temporary of its block's value would be owned by (the module's rule).
    pub(super) fn place_super_let(&mut self, pat: PatId, from: usize) -> Result<(), BuildError> {
        let Some(s) = self.super_let_of(pat)? else { return Ok(()) };
        if from > self.scope_pats.len() {
            return Err(malformed("a `super let`'s bindings start past `scope_pats`"));
        }
        let n = self.scope_pats.len() - from;
        if n == 0 {
            // No binding of it needs drop: nothing is held, nothing to move.
            return Ok(());
        }
        let block = self.block_of_stmt(s)?;
        match self.extending_init(block.0)? {
            Some((_, true)) => Err(unsupported(
                block,
                "a `super let` in a block under an extending `if` branch or `match` arm (its binding would need a drop flag)",
            )),
            Some((init, false)) => {
                // Declared just before the `let`'s own bindings, in the drop scope of the block
                // holding that `let` (as `hold_temp` places an extended temporary).
                let t = self.temp_scope_index(init).ok_or(malformed("a `super let`'s extending `let` is not open"))?;
                let depth = self.temp_scopes.get(t).ok_or(malformed("a temporary scope past `temp_scopes`"))?.1 as usize;
                let mut at = from;
                for _ in 0..n {
                    let h = self.scope_pats.remove(at);
                    if !matches!(h, Held::Pat(_)) {
                        return Err(malformed("a `super let`'s entry that is not a binding"));
                    }
                    let pos = *self.temp_marks.get(t).ok_or(malformed("a temporary scope past `temp_marks`"))? as usize;
                    if pos > at {
                        return Err(malformed("a `super let`'s extending `let` opened after its binding"));
                    }
                    // `insert_held` moves `temp_marks[t]` past it: the next binding goes after
                    // this one (declaration order); the entries left move up by one.
                    self.insert_held(pos, t, depth, h);
                    at += 1;
                }
                Ok(())
            }
            None => {
                // The block's temporary scope: dropped at its end (`temp_end` takes the
                // bindings named in `super_temps`).
                let anchor = self.temp_anchor(block)?;
                let t = self.temp_scope_index(anchor).ok_or(malformed("a `super let`'s temporary scope is not open"))?;
                let depth = self.temp_scopes.get(t).ok_or(malformed("a temporary scope past `temp_scopes`"))?.1 as usize;
                let mut at = from;
                for _ in 0..n {
                    let h = self.scope_pats.remove(at);
                    let Held::Pat(p) = h else { return Err(malformed("a `super let`'s entry that is not a binding")) };
                    // The end of scope `t`'s own entries: before the first scope opened inside
                    // it (as `hold_temp` places a temporary).
                    let mut pos = self.scope_pats.len();
                    if let Some(&m) = self.scope_marks.get(depth) {
                        pos = pos.min(m as usize);
                    }
                    if let Some(&m) = self.temp_marks.get(t + 1) {
                        pos = pos.min(m as usize);
                    }
                    if pos > at {
                        return Err(malformed("a `super let`'s temporary scope opened after its binding"));
                    }
                    self.insert_held(pos, t + 1, depth, h);
                    self.super_temps.push(p);
                    at += 1;
                }
                Ok(())
            }
        }
    }

    /// `temp_end`'s question for entry `h` past its scope's mark: whether it is a `super let`
    /// binding owned by this temporary scope; if so it is dropped here (and is no longer owned).
    pub(super) fn super_let_ends_here(&mut self, h: Held) -> Result<bool, BuildError> {
        let Held::Pat(p) = h else { return Ok(false) };
        let Some(i) = self.super_temps.iter().position(|q| q.0 == p.0) else { return Ok(false) };
        self.super_temps.remove(i);
        self.drop_held(h, true)?;
        self.owned[p.ix()] = super::LocalOwn::None;
        Ok(true)
    }
}
