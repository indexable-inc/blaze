//! `#[loop_match]` loops and their `#[const_continue]` jumps (feature `loop_match`). rustc's
//! `builder/expr/into.rs`, `ExprKind::LoopMatch`, and `builder/scope.rs`,
//! `break_const_continuable_scope` (MIT or Apache-2.0).
//!
//! The loop lowers as a plain one, its head testing the arms in order. A `#[const_continue]
//! break 'blk C` does what `break 'blk C` and the loop's `state = ..` do, in that order (the
//! block's value is `C`, the arm's scopes are left, the old state is dropped and `C` written),
//! and jumps to the entry of the arm [`loop_match_check`] picked for `C`: a block that tests
//! only that arm's pattern and binds it, then joins the arm's body. The head's chain of tests
//! is not run.
//!
//! The entry's no-match edge goes to the head as a `continue` would. The check proved it is
//! never taken; it is there so that a wrong pick runs the arm the head would, not a wrong one.

use crate::mir::loop_match::{ConstContinueJump, loop_match_check};

use super::*;

/// One `#[loop_match]` loop being lowered, with jumps to it.
pub(super) struct LoopMatchFrame<'f> {
    loop_expr: ExprId,
    /// The `match` on the state.
    match_expr: ExprId,
    /// The loop's head and the drop scope a `continue` of it leaves to, once the loop has
    /// them.
    head: Option<(BasicBlockId, usize)>,
    /// The scrutinee's place, once the `match` is lowered.
    state: Option<StoredPlace>,
    /// The entry of each arm some `#[const_continue]` jumps to, by arm index, and whether the
    /// arm's lowering has filled it.
    entries: Vec<(u32, BasicBlockId, bool), &'f Frame>,
}

impl<'a, 'f, 'db> MirLowerCtx<'a, 'f, 'db> {
    /// The body's own [`DefWithBodyId`], whose check holds the loop's jumps.
    fn loop_match_owner(&self, expr: ExprId) -> Result<'db, DefWithBodyId> {
        match self.owner {
            InferBodyId::DefWithBodyId(def) => Ok(def),
            // `loop_match_check` reads item bodies; an anonymous const has none it covers.
            InferBodyId::AnonConstId(_) => {
                not_supported!("`#[loop_match]` loop or `#[const_continue]` {expr:?} in an anonymous const")
            }
        }
    }

    /// Starts `loop_expr`'s frame when it is a `#[loop_match]` loop the check accepted and some
    /// `#[const_continue]` jumps to. Every other loop lowers as before: one the check refused
    /// never gets here (the body is not lowered), and without jumps nothing differs.
    pub(super) fn enter_loop_match(&mut self, loop_expr: ExprId) -> Result<'db, ()> {
        if !self.store.is_loop_match(loop_expr) {
            return Ok(());
        }
        let def = self.loop_match_owner(loop_expr)?;
        let Some(accepted) = loop_match_check(self.db, def).loop_of(loop_expr) else {
            return Ok(());
        };
        if accepted.targets.is_empty() {
            return Ok(());
        }
        // alloc: in the lowering's frame, one slot per arm a jump targets
        let mut entries = Vec::with_capacity_in(accepted.targets.len(), self.frame);
        for &arm in &accepted.targets {
            // Made now, before the arms, since a jump may be lowered in an arm before the arm
            // it jumps to.
            let entry = self.new_basic_block();
            entries.push((arm, entry, false));
        }
        // alloc: in the lowering's frame, one per `#[loop_match]` loop being lowered
        self.loop_matches.push(LoopMatchFrame {
            loop_expr,
            match_expr: accepted.match_expr,
            head: None,
            state: None,
            entries,
        });
        Ok(())
    }

    /// Records the head of `loop_expr`, called with the loop's blocks current.
    pub(super) fn loop_match_begins(&mut self, loop_expr: ExprId) {
        let Some(blocks) = self.current_loop_blocks.as_ref() else { return };
        let head = (blocks.begin, blocks.drop_scope_index);
        if let Some(frame) = self.loop_matches.iter_mut().rev().find(|it| it.loop_expr == loop_expr)
        {
            frame.head = Some(head);
        }
    }

    /// Ends `loop_expr`'s frame. An entry left empty would be a block with no terminator, so
    /// it is refused here, naming the arm.
    pub(super) fn leave_loop_match(&mut self, loop_expr: ExprId) -> Result<'db, ()> {
        if self.loop_matches.last().is_none_or(|it| it.loop_expr != loop_expr) {
            return Ok(());
        }
        let Some(frame) = self.loop_matches.pop() else { return Ok(()) };
        if let Some(&(arm, _, _)) = frame.entries.iter().find(|it| !it.2) {
            implementation_error!(
                "`#[loop_match]` loop {loop_expr:?}: arm {arm} is a jump target but was never lowered"
            );
        }
        Ok(())
    }

    /// Records the state's place when `match_expr` is the `match` of a loop being lowered.
    pub(super) fn loop_match_dispatch(&mut self, match_expr: ExprId, state: Place<'db>) {
        if let Some(frame) =
            self.loop_matches.iter_mut().rev().find(|it| it.match_expr == match_expr)
        {
            frame.state = Some(state.store());
        }
    }

    /// The block arm `index` of `match_expr` starts at: `then`, where its pattern matched at
    /// the head, unless the arm is a jump target. Then a new block, which both `then` and the
    /// arm's entry go to; the entry tests the arm's pattern against the state and binds it.
    pub(super) fn loop_match_arm_entry(
        &mut self,
        match_expr: ExprId,
        index: usize,
        pat: PatId,
        then: BasicBlockId,
    ) -> Result<'db, BasicBlockId> {
        let Some(frame) = self.loop_matches.iter().rposition(|it| it.match_expr == match_expr)
        else {
            return Ok(then);
        };
        let Some(slot) = self.loop_matches[frame].entries.iter().position(|it| it.0 as usize == index)
        else {
            return Ok(then);
        };
        let it = &self.loop_matches[frame];
        let (Some((head, scope)), Some(state)) = (it.head, it.state.as_ref().map(StoredPlace::as_ref))
        else {
            implementation_error!("`#[loop_match]` `match` {match_expr:?} lowered outside its loop");
        };
        let entry = it.entries[slot].1;
        let span = MirSpan::from(match_expr);
        let body = self.new_basic_block();
        self.set_goto(then, body, span);
        // The entry is the arm's alone: no guard runs (the check refuses guards), and the arm
        // is reached whatever the head's arms before it are.
        let outer = self.guarded_moves.take();
        let unreached = mem::replace(&mut self.arm_unreached, false);
        let matched = self.pattern_match(entry, None, state, pat);
        self.arm_unreached = unreached;
        self.guarded_moves = outer;
        let (bound, otherwise) = matched?;
        self.set_goto(bound, body, span);
        if let Some(otherwise) = otherwise {
            self.exit_to(scope, otherwise, head, span);
        }
        self.loop_matches[frame].entries[slot].2 = true;
        Ok(body)
    }

    /// `#[const_continue] break 'blk value`, in the plain loop's order: the value goes where a
    /// `break 'blk value` puts it, what that break leaves is left, then `state = ..` drops the
    /// old state and writes the new one as the ordinary assignment does, and the jump goes to
    /// the entry of the arm the check picked.
    ///
    /// rustc (nightly-2026-10-27) writes the value into the state before the scopes are left
    /// and never drops the old state, so a state with `Drop` prints otherwise there. blaze
    /// keeps the plain loop's behaviour: the attributes change speed, not effects. A `Copy`
    /// state goes the same way, as an arm's local whose destructor reads the state sees the
    /// old value in the plain loop; the drop is skipped for it (`drop_assigned`).
    pub(super) fn lower_const_continue(
        &mut self,
        break_expr: ExprId,
        value: Option<ExprId>,
        label: Option<LabelId>,
        current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let def = self.loop_match_owner(break_expr)?;
        let Some(jump) = loop_match_check(self.db, def).jump_of(break_expr) else {
            // The check runs on no body with a type error, as rustc builds no THIR for one, so
            // it decided no jump; the error is inference's, reported with the body.
            if self.infer.has_errors {
                return Err(MirLowerError::HasErrors);
            }
            // A refused `#[const_continue]` keeps its body from lowering, so one that gets here
            // was accepted, and every accepted one has a jump.
            implementation_error!("`#[const_continue]` {break_expr:?} has no jump and no refusal");
        };
        let (Some(value), Some(label)) = (value, label) else {
            implementation_error!("`#[const_continue]` {break_expr:?} accepted without a label and a value");
        };
        let ConstContinueJump { loop_expr, arm, .. } = jump;
        let Some(frame) = self.loop_matches.iter().rev().find(|it| it.loop_expr == loop_expr)
        else {
            implementation_error!(
                "`#[const_continue]` {break_expr:?} lowered outside its loop {loop_expr:?}"
            );
        };
        let entry = frame.entries.iter().find(|it| it.0 == arm).map(|it| it.1);
        let (Some(entry), Some(state)) = (entry, frame.state.as_ref().map(StoredPlace::as_ref))
        else {
            implementation_error!(
                "`#[const_continue]` {break_expr:?}: arm {arm} has no entry or the state no place"
            );
        };
        let block = self.labeled_loop_blocks.get(&label).ok_or(MirLowerError::UnresolvedLabel)?;
        // The labeled block's value, the temporary `state = 'blk: { .. }` moves from. It is
        // outside the block's scopes, so it outlives what the break leaves.
        let (value_place, scope) = (block.place.as_ref(), block.drop_scope_index);
        let Some(current) = self.lower_expr_to_place(value, value_place, current)? else {
            return Ok(None);
        };
        let span = MirSpan::from(break_expr);
        // What a `break 'blk` leaves: every scope inside the labeled block, an arm's locals
        // first. The arm's entry is inside the block, so what is live there stays live, as on
        // the head's path.
        let assign = self.new_basic_block();
        self.exit_to(scope, current, assign, span);
        let ty = self.place_ty(&state);
        let moved: Rvalue = Operand { kind: OperandKind::Move(value_place.store()), span: None }.into();
        // clone: kept in the body, once on the way out of the drop if it unwinds
        let assigned = self.drop_assigned(state, ty, assign, span, Some(moved.clone()));
        self.push_assignment(assigned, state, moved, span);
        self.set_goto(assigned, entry, span);
        Ok(None)
    }
}
