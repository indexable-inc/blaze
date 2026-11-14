//! `#[loop_match]` loops and their `#[const_continue]` jumps (feature `loop_match`). rustc's
//! `builder/expr/into.rs`, `ExprKind::LoopMatch`, and `builder/scope.rs`,
//! `break_const_continuable_scope` (MIT or Apache-2.0).
//!
//! The loop lowers as a plain one, its head testing the arms in order. A `#[const_continue]
//! break 'blk C` assigns `C` to the state, leaves what a `break 'blk C` leaves, and jumps to
//! the entry of the arm [`loop_match_check`] picked for `C`: a block that tests only that arm's
//! pattern and binds it, then joins the arm's body. The head's chain of tests is not run.
//!
//! The entry's no-match edge goes to the head as a `continue` would. The check proved it is
//! never taken; it is there so that a wrong pick runs the arm the head would, not a wrong one.

use crate::mir::loop_match::{ConstContinueJump, loop_match_check};

use super::*;

/// One `#[loop_match]` loop being lowered, with jumps to it.
pub(super) struct LoopMatchFrame<'f> {
    loop_expr: ExprId,
    /// `state = 'blk: { .. }`, the loop's body.
    assignment: ExprId,
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

    /// Starts `loop_expr`'s frame when it is a `#[loop_match]` loop the check accepted. Every
    /// other loop lowers as before: one the check refused never gets here (the body is not
    /// lowered). A loop no `#[const_continue]` jumps to still gets a frame, for its update of
    /// the state (`lower_loop_match_update`).
    pub(super) fn enter_loop_match(&mut self, loop_expr: ExprId) -> Result<'db, ()> {
        if !self.store.is_loop_match(loop_expr) {
            return Ok(());
        }
        let def = self.loop_match_owner(loop_expr)?;
        let Some(accepted) = loop_match_check(self.db, def).loop_of(loop_expr) else {
            return Ok(());
        };
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
            assignment: accepted.assignment,
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

    /// Whether `assignment` is the `state = 'blk: { .. }` of a loop being lowered, with a
    /// state an ordinary assignment would drop.
    pub(super) fn is_loop_match_update(&mut self, assignment: ExprId, rhs: ExprId) -> bool {
        if !self.loop_matches.iter().any(|it| it.assignment == assignment) {
            return false;
        }
        let ty = self.expr_ty_without_adjust(rhs);
        !self.is_copy(ty)
    }

    /// `state = 'blk: { .. }` in a `#[loop_match]` loop: the block's value is moved into the
    /// state, and the old state is not dropped. rustc's `ExprKind::LoopMatch` writes it with a
    /// plain assign, as `break_const_continuable_scope` writes a jump's value, so a state
    /// with a `Drop` impl is never dropped by the loop, on either path. Without the attribute
    /// the same source drops it, as the ordinary assignment does.
    pub(super) fn lower_loop_match_update(
        &mut self,
        assignment: ExprId,
        rhs: ExprId,
        place: Place<'db>,
        current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let span = MirSpan::from(assignment);
        let Some((value, current)) = self.lower_expr_as_place(current, rhs, true)? else {
            return Ok(None);
        };
        self.push_fake_read(current, value, span);
        // The `match` in `rhs` set the state's place as it lowered.
        let state = self.loop_matches.iter().rev().find(|it| it.assignment == assignment);
        let Some(state) = state.and_then(|it| it.state.as_ref()).map(StoredPlace::as_ref) else {
            implementation_error!("`#[loop_match]` update {assignment:?}: the state has no place");
        };
        let moved = Operand { kind: OperandKind::Move(value.store()), span: Some(rhs.into()) };
        self.push_assignment(current, state, moved.into(), span);
        self.push_assign_unit(current, place, span);
        Ok(Some(current))
    }

    /// `#[const_continue] break 'blk value`: `state = value`, then what a `break 'blk` leaves
    /// is left, and the jump goes to the entry of the arm the check picked.
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
        let Some(current) = self.lower_expr_to_place(value, state, current)? else {
            return Ok(None);
        };
        // What a `break 'blk` leaves: every scope inside the labeled block. The arm's entry is
        // inside the block, so what is live there stays live, as on the head's path.
        let scope = self
            .labeled_loop_blocks
            .get(&label)
            .ok_or(MirLowerError::UnresolvedLabel)?
            .drop_scope_index;
        self.exit_to(scope, current, entry, break_expr.into());
        Ok(None)
    }
}
