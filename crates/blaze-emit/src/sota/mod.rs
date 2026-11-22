//! Experimental Non-LLVM SOTA Optimizing Tier (`TierName::Sota`,
//! `docs/v2/plan/design/sota-tier.md`).
//!
//! Default optimizing tier on AArch64 (`blaze_tier::optimizing_tier`):
//! 1. **Fixed-point LIR mid-end (`blaze_lir::egraph::sota_optimize_fixpoint`)**: dominator-scoped
//!    Acyclic E-Graph (GVN + commutative canonicalization + algebraic identities + exact constant
//!    folding) + materialized strength reduction (`mul`/`udiv`/`urem` by `2^k` -> `shl`/`lshr`/`and`)
//!    + materialized LICM (hoisting loop-invariant math into preheaders) + DCE + PGO/loop/pressure
//!    feedback.
//! 2. **Chordal-SSA Register Allocation (`regalloc`)**: allocates live SSA values directly into
//!    physical AArch64 GPRs (`X0..X15` + `X19..X28`), keeping leaf functions completely frameless.
//! 3. **ISLE-Style Pattern-Fusing AArch64 Emitter (`isel`)**: fuses `madd`, `msub`, `add lsl`,
//!    `add lsr`, `sub lsl`, `ubfm` bitmasks, `add_imm`/`sub_imm`, `cmp`+`b.cond`/`cbz`/`cbnz`,
//!    `cmp`+`csel`, and eliminates fallthrough jumps.
//! 4. **Seamless Fallback (`Sota::with_fallback`)**: for functions using ops outside `Sota`'s direct
//!    fused scalar subset (e.g. `asm!`, `f128`, atomics, stack slots), delegates the
//!    *already-SOTA-optimized* LIR to the configured fallback tier (`Fast` or `Reference`).

mod isel;
mod regalloc;

#[cfg(test)]
mod tests;

use blaze_lir::{
    Verified,
    egraph::{RegBudget, sota_optimize_fixpoint},
    simplify::scalar_replace,
    verify,
};
use blaze_own::Alloc;
use blaze_tier::{Arch, Emitted, Machine, Names, Tier, TierError, TierName};

use crate::Reference;

/// The Experimental Non-LLVM SOTA Optimizing Tier (`TierName::Sota`).
pub struct Sota<F = Reference> {
    /// Optional fallback tier (e.g. `Fast` or `Reference`) invoked on the already-SOTA-optimized
    /// LIR when a function uses ops outside `Sota`'s direct fused scalar subset.
    pub fallback: Option<F>,
    /// Maximum fixed-point rounds for the `aegraph` + strength-reduction + LICM + DCE loop.
    pub max_rounds: u32,
}

impl Sota<Reference> {
    /// Standalone SOTA tier: compiles directly to fused AArch64 machine code and returns
    /// `TierError::Refused` on unsupported exotic ops.
    pub const fn standalone() -> Self {
        Self { fallback: None, max_rounds: 3 }
    }
}

impl<F: Tier> Sota<F> {
    /// Full-coverage SOTA tier: runs `sota_optimize_fixpoint` on every function, emits fused
    /// frameless AArch64 directly whenever possible, and delegates exotic ops (already optimized!)
    /// to `fallback`.
    pub const fn with_fallback(fallback: F) -> Self {
        Self { fallback: Some(fallback), max_rounds: 3 }
    }
}

impl<F: Tier> Tier for Sota<F> {
    const NAME: TierName = TierName::Sota;

    fn emit<'a>(
        &self,
        func: Verified<'_>,
        names: &dyn Names,
        machine: Machine,
        alloc: Alloc<'a>,
    ) -> Result<Emitted<'a>, TierError> {
        if machine.arch != Arch::Arm64 {
            return Err(TierError::Machine { tier: Self::NAME, machine });
        }

        // Stage 0: Scalar Replacement of Aggregates (SROA) promotes non-escaping local stack slots
        // straight into SSA registers before Aegraph + LICM + Strength Reduction!
        let sroa_func = scalar_replace(func.view(), alloc).ok();
        let mid_in = match &sroa_func {
            Some(f) => f.view(),
            None => func.view(),
        };

        // Stage 1: Fixed-point Aegraph + Materialized Strength Reduction + Materialized LICM + DCE.
        let optimized = sota_optimize_fixpoint(mid_in, RegBudget::AARCH64, self.max_rounds, |_| 1, alloc)
            .map_err(|_| TierError::Refused {
                tier: Self::NAME,
                inst: None,
                what: "SOTA mid-end optimization refused function",
            })?;
        let opt_verified = verify(optimized.func.view()).map_err(|_| TierError::Refused {
            tier: Self::NAME,
            inst: None,
            what: "SOTA optimized LIR failed verify",
        })?;

        // Stage 2 + 3: Chordal-SSA regalloc + ISLE-style fused AArch64 emission.
        match isel::emit_sota_a64(opt_verified.view(), names, alloc) {
            Ok(emitted) => Ok(emitted),
            Err(err) => {
                if let Some(ref fb) = self.fallback {
                    fb.emit(opt_verified, names, machine, alloc)
                } else {
                    Err(err)
                }
            }
        }
    }
}
