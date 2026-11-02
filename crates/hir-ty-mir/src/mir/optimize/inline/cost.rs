//! rustc's `CostChecker` (`compiler/rustc_mir_transform/src/cost_checker.rs`) and the leaf
//! check of `cross_crate_inlinable` (`cross_crate_inline.rs`). Two things rustc's MIR has
//! that this MIR has not are made up for here. rustc's MIR building and `LowerIntrinsics`
//! have made most intrinsic calls statements and rvalues before either looks at a body, and
//! this MIR keeps them calls: [`Call`] says what each was made, so it costs what rustc's
//! does. And rustc builds an optional overflow check only where the crate it builds checks
//! overflow, where this MIR has the `Assert` always: one where the body is not written
//! checked is no terminator to rustc.

use smallvec::SmallVec;

use crate::{
    intrinsic::Intrinsic,
    mir::{
        BasicBlock, BasicBlockId, MirBody, MirSpan, OperandKind, StatementKind, StoredPlace,
        Terminator, TerminatorKind,
    },
};

use super::super::block_index;

const INSTR_COST: usize = 5;
const CALL_PENALTY: usize = 25;
const LANDINGPAD_PENALTY: usize = 50;
const RESUME_PENALTY: usize = 45;
const LARGE_SWITCH_PENALTY: usize = 20;
const CONST_SWITCH_BONUS: usize = 10;

/// The most statements a function without calls may have that rustc makes inlinable across
/// crates: `-Zcross-crate-inline-threshold`'s default.
const CROSS_CRATE_THRESHOLD: usize = 100;

/// What the callee of a call terminator is to the cost model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Call {
    /// A function with a body of its own, or one called through a pointer.
    Function,
    /// An intrinsic rustc keeps a call.
    Intrinsic,
    /// `ub_checks`: an assignment of rustc's `RuntimeChecks::UbChecks`.
    UbChecks,
    /// An intrinsic rustc makes `statements` assignments or statements costing `cost`, and
    /// the call a `Goto`.
    Lowered { cost: usize, statements: usize },
    /// `unreachable`, which rustc makes an `Unreachable` terminator.
    Unreachable,
}

/// What rustc's MIR has for a call of `intrinsic` (`None` for any other callee), as
/// `LowerIntrinsics` and MIR building make it.
pub(super) fn classify(intrinsic: Option<Intrinsic>) -> Call {
    let Some(intrinsic) = intrinsic else { return Call::Function };
    match intrinsic {
        Intrinsic::UbChecks => Call::UbChecks,
        Intrinsic::Unreachable => Call::Unreachable,
        Intrinsic::CopyNonoverlapping => Call::Lowered { cost: CALL_PENALTY, statements: 1 },
        // MIR building makes it a store through the box and a copy of the box.
        Intrinsic::WriteBoxViaMove => Call::Lowered { cost: 2 * INSTR_COST, statements: 2 },
        Intrinsic::OverflowChecks
        | Intrinsic::Forget
        | Intrinsic::Assume
        | Intrinsic::WrappingAdd
        | Intrinsic::WrappingSub
        | Intrinsic::WrappingMul
        | Intrinsic::ThreeWayCompare
        | Intrinsic::UncheckedAdd
        | Intrinsic::UncheckedSub
        | Intrinsic::UncheckedMul
        | Intrinsic::UncheckedDiv
        | Intrinsic::UncheckedRem
        | Intrinsic::UncheckedShl
        | Intrinsic::UncheckedShr
        | Intrinsic::AddWithOverflow
        | Intrinsic::SubWithOverflow
        | Intrinsic::MulWithOverflow
        | Intrinsic::ReadViaCopy
        | Intrinsic::WriteViaMove
        | Intrinsic::DiscriminantValue
        | Intrinsic::Offset
        | Intrinsic::SliceGetUnchecked
        | Intrinsic::Transmute
        | Intrinsic::TransmuteUnchecked
        | Intrinsic::AggregateRawPtr
        | Intrinsic::PtrMetadata => Call::Lowered { cost: INSTR_COST, statements: 1 },
        _ => Call::Intrinsic,
    }
}

/// What the crate compiling the caller has of the checks the cost model reads: rustc's
/// session's `ub_checks()` and `overflow_checks()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Session {
    pub(super) ub_checks: bool,
    pub(super) overflow_checks: bool,
}

/// Whether rustc's MIR has the terminator at all: an optional overflow check is not one
/// where `written_checked` says the code at its span is not written checked.
fn present(terminator: &Terminator, written_checked: &mut impl FnMut(MirSpan) -> bool) -> bool {
    match &terminator.kind {
        TerminatorKind::Assert { msg, .. } if msg.is_optional_overflow_check() => {
            written_checked(terminator.span)
        }
        _ => true,
    }
}

/// rustc's `is_call_like` of the terminator of a block, an `InlineAsm` one of rustc's being an
/// asm statement here.
pub(super) fn block_is_call_like(
    block: &BasicBlock,
    mut written_checked: impl FnMut(MirSpan) -> bool,
) -> bool {
    block.statements.iter().any(|it| matches!(it.kind, StatementKind::InlineAsm(_)))
        || block
            .terminator
            .as_ref()
            .is_some_and(|it| call_like(it) && present(it, &mut written_checked))
}

/// rustc's `is_call_like`, of a terminator rustc's MIR has.
fn call_like(terminator: &Terminator) -> bool {
    match &terminator.kind {
        TerminatorKind::Drop { .. }
        | TerminatorKind::Assert { .. }
        | TerminatorKind::TailCall { .. } => true,
        TerminatorKind::Call { intrinsic, .. } => {
            matches!(classify(*intrinsic), Call::Function | Call::Intrinsic)
        }
        _ => false,
    }
}

/// What inlining `body` costs, as rustc's `check_callee_mir_body` adds it up: its blocks
/// reachable from the start, the unwind edge of a drop of a value with nothing to drop not
/// followed. `written_checked` is [`super::written_checked_at`] of the body, `needs_drop`
/// says what a drop drops. `None` when one of those blocks ends in a tail call: rustc
/// inlines no function with one ("can't inline functions with tail calls").
pub(super) fn cost(
    body: &MirBody<'_>,
    session: Session,
    mut written_checked: impl FnMut(MirSpan) -> bool,
    mut needs_drop: impl FnMut(&StoredPlace) -> bool,
) -> Option<usize> {
    let mut call_likes = 0usize;
    for (_, block) in body.basic_blocks.iter() {
        call_likes += block
            .statements
            .iter()
            .filter(|it| matches!(it.kind, StatementKind::InlineAsm(_)))
            .count();
        if let Some(terminator) = &block.terminator
            && call_like(terminator)
            && present(terminator, &mut written_checked)
        {
            call_likes += 1;
        }
    }
    let mut penalty = 0usize;
    let mut bonus = 0usize;
    if call_likes == 1 {
        bonus += CALL_PENALTY;
    }
    let landing_pad = |unwind: &Option<BasicBlockId>| {
        if unwind.is_some() { LANDINGPAD_PENALTY } else { 0 }
    };
    let mut visited: SmallVec<[bool; 64]> = SmallVec::from_elem(false, body.basic_blocks.len());
    let mut work: SmallVec<[BasicBlockId; 16]> = SmallVec::new();
    work.push(body.start_block);
    while let Some(id) = work.pop() {
        if std::mem::replace(&mut visited[block_index(id)], true) {
            continue;
        }
        let block = &body.basic_blocks[id];
        for statement in &block.statements {
            match statement.kind {
                StatementKind::Assign(..) | StatementKind::InlineAsm(_) => penalty += INSTR_COST,
                StatementKind::FakeRead(_)
                | StatementKind::Deinit(_)
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        match &terminator.kind {
            TerminatorKind::Drop { place, target, unwind } => {
                work.push(*target);
                if needs_drop(place) {
                    penalty += CALL_PENALTY + landing_pad(unwind);
                    work.extend(*unwind);
                }
                continue;
            }
            TerminatorKind::Call { target, cleanup, intrinsic, .. } => match classify(*intrinsic) {
                Call::Function => penalty += CALL_PENALTY + landing_pad(cleanup),
                Call::Intrinsic => penalty += INSTR_COST + landing_pad(cleanup),
                Call::UbChecks => {
                    penalty += INSTR_COST;
                    if !session.ub_checks {
                        bonus += CALL_PENALTY;
                    }
                    work.extend(*target);
                    continue;
                }
                Call::Lowered { cost, .. } => {
                    penalty += cost;
                    work.extend(*target);
                    continue;
                }
                Call::Unreachable => {
                    bonus += INSTR_COST;
                    continue;
                }
            },
            TerminatorKind::SwitchInt { discr, targets } => {
                if matches!(
                    discr.kind,
                    OperandKind::Constant { .. } | OperandKind::Allocation { .. }
                ) {
                    bonus += CONST_SWITCH_BONUS;
                } else if targets.all_targets().len() > 3 {
                    penalty += LARGE_SWITCH_PENALTY;
                } else {
                    penalty += INSTR_COST;
                }
            }
            TerminatorKind::Assert { msg, cleanup, .. } => {
                if present(terminator, &mut written_checked) {
                    penalty += if msg.is_optional_overflow_check() && !session.overflow_checks {
                        INSTR_COST
                    } else {
                        CALL_PENALTY
                    };
                    penalty += landing_pad(cleanup);
                }
            }
            TerminatorKind::TailCall { .. } => return None,
            TerminatorKind::UnwindResume => penalty += RESUME_PENALTY,
            TerminatorKind::Unreachable => bonus += INSTR_COST,
            TerminatorKind::Goto { .. }
            | TerminatorKind::Return
            | TerminatorKind::Abort
            | TerminatorKind::DropAndReplace { .. }
            | TerminatorKind::Yield { .. }
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => {}
        }
        work.extend(terminator.kind.all_successors());
    }
    Some(penalty.saturating_sub(bonus))
}

/// rustc's leaf check of `cross_crate_inlinable`: `body` calls nothing but intrinsics, has no
/// landing pad, never resumes unwinding and has at most [`CROSS_CRATE_THRESHOLD`] statements
/// and terminators. `trivially_copy` is rustc's `is_trivially_pure_clone_copy` of what a drop
/// drops.
pub(super) fn is_small_leaf(
    body: &MirBody<'_>,
    mut written_checked: impl FnMut(MirSpan) -> bool,
    mut trivially_copy: impl FnMut(&StoredPlace) -> bool,
) -> bool {
    let (mut calls, mut resumes, mut landing_pads, mut statements) = (0usize, 0usize, 0usize, 0);
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            match statement.kind {
                StatementKind::Assign(..)
                | StatementKind::InlineAsm(_)
                | StatementKind::FakeRead(_) => statements += 1,
                StatementKind::Deinit(_)
                | StatementKind::StorageLive(_)
                | StatementKind::StorageDead(_)
                | StatementKind::Nop => {}
            }
        }
        let Some(terminator) = &block.terminator else { continue };
        if !present(terminator, &mut written_checked) {
            continue;
        }
        statements += 1;
        match &terminator.kind {
            TerminatorKind::Drop { place, unwind, .. } => {
                if !trivially_copy(place) {
                    calls += 1;
                    landing_pads += usize::from(unwind.is_some());
                }
            }
            TerminatorKind::Call { cleanup, intrinsic, .. } => match classify(*intrinsic) {
                Call::Function => {
                    calls += 1;
                    landing_pads += usize::from(cleanup.is_some());
                }
                Call::Intrinsic | Call::Unreachable => {}
                // The statements the call is made; the call itself is the `Goto` after them.
                Call::UbChecks => statements += 1,
                Call::Lowered { statements: lowered, .. } => statements += lowered,
            },
            TerminatorKind::Assert { cleanup, .. } => {
                landing_pads += usize::from(cleanup.is_some());
            }
            TerminatorKind::UnwindResume => resumes += 1,
            _ => {}
        }
    }
    calls == 0 && resumes == 0 && landing_pads == 0 && statements <= CROSS_CRATE_THRESHOLD
}
