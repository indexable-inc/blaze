//! The registers no home takes (docs/v2/plan/design/backend-next.md D100 point 5, "homes and
//! eviction"): the analysis pass gives a value live across blocks a register for its whole
//! interval; the emission pass gives a value defined and last used in one block any register no
//! home holds over its interval, and evicts such a value to its cell when none is free. That
//! eviction only ever finds a register if some registers of each class are NEVER a home: a home
//! holds its register over its whole interval, so where homes cover every register of a class,
//! an instruction in their intervals has nowhere to put an operand loaded from a cell or its
//! result ("no register of the class is free", the Fast tier's refusal in 226 of 512 generated
//! programs before this set existed).
//!
//! How many: the most registers of one class ONE instruction's emission holds at once
//! ([`PER_INSTRUCTION`]): each input of the widest template in its own register (piece 0's
//! operands `0..RESULT`: four) and its result in another. A template's temporaries are the
//! description's scratch pair, which is never allocatable (blaze-tier table.rs, `TEMP0`/`TEMP1`;
//! regs.rs, `allocatable`), so they need none of these. With that many registers of the class
//! outside every home, the emission pass can always place the instruction: an operand or result
//! takes a kept register no local holds, or evicts the local in one whose next use is farthest;
//! only the instruction's own pinned operands are not evictable, and they are fewer than the
//! kept set.
//!
//! Which: caller-saved registers first, highest number first (so the homes, given the lowest
//! free register, keep the low ones); callee-saved ones only where a class has fewer caller-saved
//! registers than [`PER_INSTRUCTION`], and those are then in the analysis's `saved` set (the
//! emission pass gives a local a callee-saved register only when the prologue saves it). On
//! arm64 ([`blaze_tier::ARM64`]) the set is x11 to x15 and v27 to v31: every one caller-saved
//! (AAPCS64: x9 to x15 are temporaries, v16 to v31 are not preserved across a call).

use blaze_tier::table::RESULT;
use blaze_tier::{Reg, RegClass, RegDesc, RegSet};

/// The most registers of one class the emission pass holds for one instruction: every input of
/// the widest template (piece 0's operand numbers `0..RESULT`) and the result.
pub(crate) const PER_INSTRUCTION: u32 = RESULT as u32 + 1;

/// The registers of `class` that no home takes: [`PER_INSTRUCTION`] of `regs.allocatable`,
/// caller-saved first, each group highest first; the whole class when it has fewer.
pub(crate) fn kept(regs: &RegDesc, class: RegClass) -> RegSet {
    let all = regs.allocatable.of(class);
    let caller = all.minus(regs.callee_saved);
    let callee = all.minus(caller);
    let mut kept = RegSet::EMPTY;
    for group in [caller, callee] {
        let mut bits = group.0;
        while bits != 0 && kept.len() < PER_INSTRUCTION {
            // The highest register left in the group: `bits` is not zero, so 63 - leading
            // zeros is in 0..64.
            let number = 63 - bits.leading_zeros();
            bits &= !(1u64 << number);
            kept = kept.with(Reg(number as u8));
        }
    }
    kept
}

/// The callee-saved registers among both classes' kept sets: the analysis adds them to `saved`,
/// so the prologue saves them and the emission pass may give them to a local. Empty on arm64.
pub(crate) fn kept_callee_saved(regs: &RegDesc) -> RegSet {
    let both = kept(regs, RegClass::Gpr).union(kept(regs, RegClass::Fpr));
    both.minus(both.minus(regs.callee_saved))
}
