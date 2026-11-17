//! The `Fast` tier (WAVE-BACKEND.md piece K2; backend-next.md D97 point 2): the composer over a
//! template table, behind piece 0's [`Tier`] seam. The analysis pass (analysis.rs) decides the
//! order, loops and homes; the emission pass (emit.rs) copies templates. Nothing here encodes an
//! instruction.

use blaze_lir::View;
use blaze_own::Alloc;
use blaze_tier::{Arch, Emitted, Machine, Names, RegDesc, Table, Tier, TierError, TierName};

use super::{analysis, emit};

/// The Fast tier: every instruction a template of `table`, registers as `regs` describes them.
pub struct Fast<'t> {
    pub table: &'t dyn Table,
    pub regs: &'t RegDesc,
}

impl Tier for Fast<'_> {
    const NAME: TierName = TierName::Fast;

    fn emit<'a>(&self, func: View<'_>, names: &dyn Names, machine: Machine, alloc: Alloc<'a>) -> Result<Emitted<'a>, TierError> {
        // One machine throughout: the table's, the description's and the caller's. arm64 only:
        // the composer's frame, poll and glue patterns are arm64's (emit.rs, `glue`).
        if self.table.head().machine != machine || self.regs.arch != machine.arch || machine.arch != Arch::Arm64 {
            return Err(TierError::Machine { tier: Self::NAME, machine });
        }
        let an = analysis::analyse(func, self.regs, alloc)?;
        emit::emit(func, &an, self.table, self.regs, names, alloc)
    }
}
