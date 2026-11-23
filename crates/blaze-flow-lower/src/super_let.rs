//! `super let` in the flow (async plan P11, D-A11): a binding's `StorageDead` (and its drop)
//! goes to the scope the builder drops it in (blaze-build walk/super_let.rs), so the flow
//! checks borrows of it against the scope it really lives in.
//!
//! The library's `pin!` (core/src/pin.rs) is `{ super let mut pinned: PinMacroHelper<_> =
//! PinMacroHelper { value: $value }; .. }`, whose value borrows `pinned` past the block. The
//! rule, as for a temporary of the block's value (the Reference, "Temporary lifetime
//! extension": "The temporary scopes for expressions in `let` statements are sometimes extended
//! to the scope of the block containing the `let` statement"; "Temporary scopes": "the temporary
//! scope of an expression is the smallest scope that contains the expression"):
//! - the block is an extending expression of a `let` initializer (`mark_extended` reached it):
//!   the binding joins the variables of the block holding that `let`, before that `let`'s own
//!   bindings, and dies at that block's end;
//! - otherwise: it joins the temporaries of the innermost temporary scope holding the block (its
//!   statement's, or an edition-2024 tail's) and dies at that scope's end.
//!
//! `super let .. else`, and a `super let` with no temporary scope around its block, are refused
//! by name ([`Form::SuperLet`]).

use core::alloc::Allocator;

use blaze_typeck::{Expr, ExprId, StmtId};

use crate::{Facts, Form, Lower, Refusal};

impl<F: Facts, A: Allocator + Copy, R: crate::one_call::CallRule> Lower<'_, '_, F, A, R> {
    /// `mark_extended` met block `e` as an extending expression of the `let` being lowered:
    /// the drop scope its `super let` bindings join is the one opened after the `let`'s block
    /// (`scope_marks[d]` with `d` the count open now, while that block is the innermost).
    pub(crate) fn note_extending_block(&mut self, e: ExprId) {
        self.super_ext.push((e.0, self.scope_marks.len() as u32));
    }

    /// Whether statement `s` was written `super let` (`BodyView::super_lets`, sorted).
    fn is_super_let(&self, s: StmtId) -> bool {
        self.body.super_lets.binary_search_by(|q| q.0.cmp(&s.0)).is_ok()
    }

    /// The block expression whose statement list holds statement `s`.
    fn block_of_stmt(&self, s: StmtId) -> Result<ExprId, Refusal> {
        for (i, e) in self.body.exprs.iter().enumerate() {
            if let Expr::Block { stmts, .. } = e
                && self.body.stmt_lists.get(stmts.range()).is_some_and(|ss| ss.iter().any(|x| x.0 == s.0))
            {
                return Ok(ExprId(i as u32));
            }
        }
        self.refuse(Form::Malformed, self.body.root)
    }

    /// A `let`-`else` statement `s` (its `else` block `x`): refused when written `super let`.
    pub(crate) fn super_let_else(&self, s: StmtId, x: ExprId) -> Result<(), Refusal> {
        if self.is_super_let(s) {
            return self.refuse(Form::SuperLet, x);
        }
        Ok(())
    }

    /// After `let` statement `s` bound its pattern (its bindings are `vars[from..]`): when it is
    /// a `super let`, moves them to the scope the module's rule names; `e` is where a refusal
    /// points.
    pub(crate) fn place_super_let(&mut self, s: StmtId, from: usize, e: ExprId) -> Result<(), Refusal> {
        if !self.is_super_let(s) {
            return Ok(());
        }
        if from > self.vars.len() {
            return self.refuse(Form::Malformed, e);
        }
        let n = self.vars.len() - from;
        if n == 0 {
            return Ok(());
        }
        let block = self.block_of_stmt(s)?;
        let ext = self.super_ext.iter().rev().find(|&&(b, _)| b == block.0).map(|&(_, d)| d as usize);
        match ext {
            Some(d) => {
                let Some(&m) = self.scope_marks.get(d) else { return self.refuse(Form::Malformed, block) };
                let m = m as usize;
                if m > from {
                    return self.refuse(Form::Malformed, block);
                }
                // vars[from..from + n] to vars[m..m + n]; every scope opened from `d` on starts
                // `n` later.
                for k in 0..n {
                    let l = self.vars.remove(from + k);
                    self.vars.insert(m + k, l);
                }
                for x in self.scope_marks.iter_mut().skip(d) {
                    *x += n as u32;
                }
            }
            None => {
                // The innermost temporary scope is the `super let` statement's own (its
                // `CloseTemps` follows `LetInit`); the one under it holds the block.
                let k = self.temp_marks.len();
                if k < 2 {
                    return self.refuse(Form::SuperLet, block);
                }
                let m = self.temp_marks[k - 1] as usize;
                if m > self.temps.len() {
                    return self.refuse(Form::Malformed, block);
                }
                for j in 0..n {
                    let l = self.vars.remove(from);
                    self.temps.insert(m + j, l);
                }
                self.temp_marks[k - 1] += n as u32;
                // A variable scope opened inside temporary scope `k - 2` (the block's, so after
                // its temporaries so far) has these `n` around it too (`Lower::scope_temps`).
                for x in self.scope_temps.iter_mut() {
                    if x.1 as usize > k - 2 {
                        x.0 += n as u32;
                    }
                }
            }
        }
        Ok(())
    }
}
