//! `target_intrinsic` in the reference evaluator (THREADS.md D1).
//!
//! The four target intrinsics blaze builds (blaze-build `intrinsics::target::Target`, the one
//! home of the set; blaze-lir sits below blaze-build and cannot name it, so the names are
//! listed here and blaze-build's test runs this evaluator on what it builds) are the aarch64
//! barriers and hints: `isb`, `dmb`, `dsb` and `hint`. They order or pace the processor and
//! have no meaning in the evaluator's single-threaded abstract machine: each tier must equal
//! the evaluator in observable results, and none of the four changes a value or a byte of
//! memory. So each runs as no effect. Any other name is refused, never guessed.

use core::alloc::Allocator;

use super::{EvalError, Outcome, Run, Val};
use crate::Op;

#[cfg(test)]
mod tests;

/// The link names the evaluator runs as no effect: blaze-build `Target::link_name` of each of
/// `ALL`.
pub(super) const NO_EFFECT: [&[u8]; 4] = [b"llvm.aarch64.isb", b"llvm.aarch64.dmb", b"llvm.aarch64.dsb", b"llvm.aarch64.hint"];

impl<A: Allocator + Clone> Run<'_, A> {
    /// Target intrinsic `inst`: one of [`NO_EFFECT`] does nothing and gives no value; any
    /// other name is refused.
    pub(super) fn target_intrinsic(&mut self, inst: usize) -> Result<Result<Option<Val>, Outcome<A>>, EvalError> {
        let mut o = self.view.operands(inst);
        let name = self.view.intrinsic_name(o.intrinsic());
        if !NO_EFFECT.contains(&name) {
            return Err(EvalError::Unsupported {
                inst,
                op: Op::TargetIntrinsic,
                what: "a target intrinsic other than `llvm.aarch64.{isb,dmb,dsb,hint}` (the instruction's row names it)",
            });
        }
        // Its operand (the immediate) is read so an unset value is still the verifier's error
        // it always is.
        for value in o.values().iter() {
            self.get(inst, value)?;
        }
        Ok(Ok(None))
    }
}
