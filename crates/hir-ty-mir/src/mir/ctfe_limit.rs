//! Which blocks of a body are steps of `long_running_const_eval`: rustc's `CtfeLimit` pass
//! (`rustc_mir_transform/src/ctfe_limit.rs`) puts a `ConstEvalCounter` in each block whose
//! terminator is a call, and in each with a back edge (a successor that dominates it: the
//! block closes a loop), and only those count toward `LINT_TERMINATOR_LIMIT`. So the lint's
//! 2,000,000 steps are about two million loop turns and calls, not two million blocks run: a
//! constant that scans a 20 KB string byte by byte a few dozen times is under it, however many
//! blocks each turn of the scan runs.

use blaze_mem::{Tag, frame};

use super::{
    BasicBlockId, MirBody, TerminatorKind,
    framed::{block_id, block_index},
    optimize::Dominators,
};

/// The blocks of `body` that are steps, as a set of block indices in words (block `b` is bit
/// `b % 64` of word `b / 64`).
pub(super) fn counted_blocks(body: &MirBody<'_>) -> Vec<u64> {
    let count = body.basic_blocks.len();
    // alloc: once per body an evaluation runs, kept by the evaluator for each call of it
    let mut words = vec![0u64; count.div_ceil(64)];
    frame::task(Tag::Mir, |frame| {
        let doms = Dominators::new(body, frame);
        for index in 0..count {
            let block = block_id(index);
            let Some(terminator) = &body.basic_blocks[block].terminator else { continue };
            let call = matches!(
                terminator.kind,
                TerminatorKind::Call { .. } | TerminatorKind::TailCall { .. }
            );
            // `dominates` is false of an unreachable block, as rustc's `is_reachable` check.
            let back_edge =
                || terminator.kind.all_successors().any(|succ| doms.dominates(succ, block));
            if call || back_edge() {
                words[index / 64] |= 1 << (index % 64);
            }
        }
    });
    words
}

/// Whether `block` is in `counted`, a set [`counted_blocks`] made.
pub(super) fn counts(counted: &[u64], block: BasicBlockId) -> bool {
    let index = block_index(block);
    counted[index / 64] >> (index % 64) & 1 == 1
}
