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

use blaze_code::record::{CodeRecord, NO_TARGET, OWN_RELRO, Site, SiteKind, TrapSource};
use blaze_lir::{
    Op, Verified,
    egraph::{RegBudget, sota_optimize_fixpoint},
    simplify::scalar_replace,
    verify,
};
use blaze_own::Alloc;
use blaze_tier::{Arch, Emitted, Machine, Names, RelocKind, Tier, TierError, TierName};

use crate::{BULK_INLINE_MOST, Called, Isa, RecordEmitError, Reference, RoutinePlaces, record::emit_record_sourced};

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

/// Converts a `Sota`-emitted AArch64 [`Emitted`] record into `blaze-code`'s [`CodeRecord`] (`bzcode07`)
/// for direct use by `blaze-ladder` (`blaze run` / `blaze test`) on AArch64.
///
/// Only accepts relocation & metadata patterns `CodeRecord` can encode cleanly; returns `None`
/// otherwise so `emit_sota_record_measured` falls back to `emit_record_sourced`.
fn emitted_to_code_record<'a>(emitted: Emitted<'a>, alloc: Alloc<'a>) -> Option<(CodeRecord<'a>, u32)> {
    if !emitted.words.is_empty() || !emitted.traps.is_empty() || !emitted.position_slots.is_empty() {
        return None;
    }
    blaze_code::unwind::Frame::of_word(emitted.frame).ok()?;

    let mut rec = CodeRecord::new_in(alloc);
    let insts = u32::try_from(emitted.code.len()).ok()?;
    rec.code = emitted.code;
    rec.relro = emitted.relro;
    rec.frame = emitted.frame;
    rec.pads = emitted.pads;
    rec.code_align = emitted.code_align;
    rec.relro_align = emitted.relro_align;

    // Remap targets and C-name entries onto `CodeRecord`.
    let mut target_remap: blaze_own::Vec<u32, Alloc<'a>> =
        blaze_own::Vec::with_capacity_in(emitted.targets.len(), alloc);
    for t in emitted.targets.iter() {
        if let Some(&(_, start, len)) = emitted.names.iter().find(|(sym, _, _)| *sym == t.symbol) {
            let s = core::str::from_utf8(&emitted.text[start as usize..(start + len) as usize]).ok()?;
            target_remap.push(rec.outside(s));
        } else {
            let idx = rec.targets.len() as u32;
            rec.targets.push(*t);
            target_remap.push(idx);
        }
    }

    // Pair `GotPage21` + `GotPageOff12` into `SiteKind::Call` / `SiteKind::Load`, and
    // `Page21` + `PageOff12 { scale: 0 }` (targeting `OWN_RELRO`) into `SiteKind::Relro`.
    let mut i = 0;
    while i < emitted.relocs.len() {
        let r = emitted.relocs[i];
        if !r.in_relro
            && r.kind == RelocKind::GotPage21
            && r.addend == 0
            && i + 1 < emitted.relocs.len()
            && !emitted.relocs[i + 1].in_relro
            && emitted.relocs[i + 1].kind == RelocKind::GotPageOff12
            && emitted.relocs[i + 1].at == r.at + 4
            && emitted.relocs[i + 1].target == r.target
            && emitted.relocs[i + 1].addend == 0
            && (r.at as usize) + 8 <= rec.code.len()
        {
            let target = *target_remap.get(r.target as usize)?;
            let ldr_at = r.at as usize + 4;
            let ldr_word = u32::from_le_bytes([
                rec.code[ldr_at],
                rec.code[ldr_at + 1],
                rec.code[ldr_at + 2],
                rec.code[ldr_at + 3],
            ]);
            let ldr_rt = ldr_word & 0x1f;
            let is_call = ldr_rt == 16 && blaze_tier::record::calls(&rec.code, r.at + 4);
            let kind = if is_call { SiteKind::Call } else { SiteKind::Load };
            rec.sites.push(Site { at: r.at, kind, target, addend: 0 });
            i += 2;
        } else if !r.in_relro
            && r.kind == RelocKind::Page21
            && r.target == OWN_RELRO
            && r.addend >= 0
            && i + 1 < emitted.relocs.len()
            && !emitted.relocs[i + 1].in_relro
            && emitted.relocs[i + 1].kind == (RelocKind::PageOff12 { scale: 0 })
            && emitted.relocs[i + 1].at == r.at + 4
            && emitted.relocs[i + 1].target == OWN_RELRO
            && emitted.relocs[i + 1].addend == r.addend
            && (r.at as usize) + 8 <= rec.code.len()
            && (r.addend as u64) < (rec.relro.len() as u64)
        {
            rec.sites.push(Site { at: r.at, kind: SiteKind::Relro, target: NO_TARGET, addend: r.addend as u32 });
            i += 2;
        } else {
            return None;
        }
    }
    Some((rec, insts))
}

/// Default AArch64 optimizing record emitter for `blaze-ladder` (`blaze run` / `blaze test`):
/// runs SROA + fixed-point Aegraph/Strength-Reduction/LICM/DCE, emits native frameless fused
/// AArch64 `Sota` code directly whenever the function fits `Sota`'s subset, and delegates exotic
/// ops (already SOTA-optimized!) cleanly to `emit_record_sourced` while preserving trap-row
/// ordinal correspondence.
pub fn emit_sota_record_measured<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    isa: Isa,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    // Preserve exact `Op::Unreachable` count when `traps` are attached so trap-source ordinals stay
    // 1:1 with `traps`.
    let orig_unreach = if traps.is_empty() {
        0
    } else {
        func.view().ops.iter().filter(|&&op| op == Op::Unreachable as u8).count()
    };

    let sroa_func = scalar_replace(func.view(), alloc).ok();
    let mid_in = match &sroa_func {
        Some(f) => f.view(),
        None => func.view(),
    };
    let sota_opt = sota_optimize_fixpoint(mid_in, RegBudget::AARCH64, 3, |_| 1, alloc).ok();
    let chosen_opt = sota_opt.as_ref().and_then(|res| {
        let v = verify(res.func.view()).ok()?;
        if !traps.is_empty() {
            let after_unreach = v.view().ops.iter().filter(|&&op| op == Op::Unreachable as u8).count();
            if after_unreach != orig_unreach {
                return None;
            }
        }
        Some(v)
    });

    if let Some(opt_v) = chosen_opt {
        if traps.is_empty() {
            if let Ok(emitted) = isel::emit_sota_a64(opt_v.view(), names, alloc) {
                if let Some((rec, insts)) = emitted_to_code_record(emitted, alloc) {
                    return Ok((rec, insts, Called::NONE));
                }
            }
        }
        return emit_record_sourced(opt_v, names, routines, isa, BULK_INLINE_MOST, traps, alloc);
    }

    emit_record_sourced(func, names, routines, isa, BULK_INLINE_MOST, traps, alloc)
}
