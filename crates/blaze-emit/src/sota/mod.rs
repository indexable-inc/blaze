//! Non-LLVM SOTA Optimizing Tier (`TierName::Sota`,
//! `docs/v2/plan/design/sota-tier.md`).
//!
//! Default optimizing tier on AArch64 (`blaze_tier::optimizing_tier`):
//! 1. **Fixed-point LIR mid-end (`blaze_lir::egraph::sota_optimize_fixpoint`)**: SROA +
//!    dominator-scoped Acyclic E-Graph (GVN + algebraic identities + constant folding) +
//!    materialized strength reduction + materialized LICM + DCE.
//! 2. **Loop-Weighted, Coalescing Chordal-SSA Register Allocation (`regalloc`)**: allocates GPRs,
//!    2-word GPR pairs (`{i64, i1}`, 2-reg returns), scalar FPs, and 128-bit NEON vectors with
//!    Belady loop-weighted eviction, phi back-edge & return-value coalescing, `sret` (`X8`), and
//!    frameless leaves.
//! 3. **ISLE-Style Pattern-Fusing AArch64 Emitter (`isel`)**:
//!    - Fused `madd`/`msub`/`fmadd`/`fmsub`, `add/sub lsl/lsr`, `bic`/`orn`/`eon`, `csneg`/`csinc`/`csinv`
//!    - Native 32-bit `W` ALU ops, general 13-bit logical immediates, `tbz`/`tbnz` & `tst` branch fusion
//!    - Scaled register-index loads/stores, adjacent `ldp`/`stp` load/store pair fusion,
//!      Granlund–Montgomery magic-multiply division/modulo, dense `Op::Switch` jump tables
//!    - Full `std` Rust support: dynamic/`>256B` `Memcpy`/`Memmove`/`Memset`/`Memcmp`, `Op::TlsAddr`
//!      (direct TSD on macOS / TLSDesc on Linux), `DataAddr` relocs & `PositionRow`, `Op::Frem`,
//!      stacked arguments + `ParamKind::Byval` + `i128` div/rem, `Op::Invoke`/`InvokeIndirect`/`Resume`
//!      with unwind tables, DirectBl export calls + TCO, and IPRA.
//! 4. **Seamless Fallback (`Sota::with_fallback`)**: delegates exotic ops outside `Sota`'s subset
//!    (e.g. `asm!`, `f128`) already SOTA-optimized to the configured fallback tier.

mod isel;
mod regalloc;
mod release_opts;

#[cfg(test)]
mod tests;

use blaze_code::record::{CodeRecord, NO_TARGET, OWN_CODE, OWN_RELRO, Site, SiteKind, TrapRow, TrapSource, Word};
use blaze_lir::{
    Op, Verified,
    egraph::{RegBudget, sota_optimize_fixpoint},
    simplify::scalar_replace,
    verify,
};
use blaze_own::Alloc;
use blaze_tier::{Arch, CallClobbers, Emitted, Machine, Names, RelocKind, Tier, TierError, TierName};

use crate::{BULK_INLINE_MOST, Called, Isa, RecordEmitError, Reference, RoutinePlaces, record::emit_record_sourced};

/// How `Sota` lowers direct `Op::Call` sites.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallMode {
    /// JIT / in-process slot calls (`adrp x16; ldr x16; blr x16` via `GotPage21`/`GotPageOff12`)
    /// supporting atomic live trampoline patching.
    GotSlot,
    /// Release AOT export calls (`bl #0` via `RelocKind::Branch26`): 1 instruction, zero dead NOPs,
    /// plus direct tail-call optimization (`b #0`).
    DirectBl,
}

/// The Non-LLVM SOTA AArch64 Optimizing Tier (`TierName::Sota`).
pub struct Sota<F = Reference> {
    /// Optional fallback tier (e.g. `Fast` or `Reference`) invoked on the already-SOTA-optimized
    /// LIR when a function uses ops outside `Sota`'s direct fused subset.
    pub fallback: Option<F>,
    /// Maximum fixed-point rounds for the `aegraph` + strength-reduction + LICM + DCE loop.
    pub max_rounds: u32,
    /// Direct call lowering mode (`GotSlot` for JIT, `DirectBl` for AOT release export).
    pub call_mode: CallMode,
}

impl Sota<Reference> {
    /// Standalone SOTA tier (JIT `GotSlot` mode): compiles directly to fused AArch64 machine code
    /// and returns `TierError::Refused` on unsupported exotic ops.
    pub const fn standalone() -> Self {
        Self { fallback: None, max_rounds: 3, call_mode: CallMode::GotSlot }
    }

    /// Release AOT export SOTA tier: enables direct `Branch26` calls and Tail-Call Optimization.
    pub const fn for_export() -> Self {
        Self { fallback: None, max_rounds: 3, call_mode: CallMode::DirectBl }
    }
}

impl<F: Tier> Sota<F> {
    /// Full-coverage SOTA tier: runs `sota_optimize_fixpoint` on every function, emits fused
    /// AArch64 directly whenever possible, and delegates exotic ops (already optimized!) to
    /// `fallback`.
    pub const fn with_fallback(fallback: F) -> Self {
        Self { fallback: Some(fallback), max_rounds: 3, call_mode: CallMode::GotSlot }
    }

    /// Full-coverage release export SOTA tier with fallback for exotic ops.
    pub const fn for_export_with_fallback(fallback: F) -> Self {
        Self { fallback: Some(fallback), max_rounds: 3, call_mode: CallMode::DirectBl }
    }

    /// IPRA-enabled AArch64 direct emission returning `(Emitted, CallClobbers)` so bottom-up
    /// module compilation preserves caller-saved registers across internal calls with zero spills.
    pub fn emit_a64_with_ipra<'a>(
        &self,
        func: Verified<'_>,
        names: &dyn Names,
        features: u64,
        call_clobbers: impl Fn(usize) -> CallClobbers,
        alloc: Alloc<'a>,
    ) -> Result<(Emitted<'a>, CallClobbers), TierError> {
        isel::emit_sota_a64_with_ipra(func.view(), names, self.call_mode, features, cfg!(target_os = "macos"), call_clobbers, alloc)
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
        // straight into SSA registers before Aegraph + LICM + Strength Reduction.
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

        // Stage 2 + 3: Loop-weighted Chordal-SSA regalloc + ISLE-style fused AArch64 emission.
        let is_macos = machine.os == blaze_tier::Os::MacOs;
        match isel::emit_sota_a64(opt_verified.view(), names, self.call_mode, machine.features, is_macos, alloc) {
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
/// for direct use by `blaze-ladder` (`blaze run` / `blaze test`) on AArch64, wiring `TrapSource`
/// rows to the exact emitted `brk #BRK_UNREACHABLE` offsets.
fn emitted_to_code_record<'a>(
    emitted: Emitted<'a>,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Option<(CodeRecord<'a>, u32)> {
    blaze_code::unwind::Frame::of_word(emitted.frame).ok()?;

    let mut rec = CodeRecord::new_in(alloc);
    let insts = u32::try_from(emitted.code.len()).ok()?;
    rec.code = emitted.code;
    rec.relro = emitted.relro;
    rec.frame = emitted.frame;
    rec.pads = emitted.pads;
    rec.code_align = emitted.code_align;
    rec.relro_align = emitted.relro_align;
    rec.positions = emitted.position_slots;

    for source in traps {
        let row = emitted.traps.iter().find(|t| t.site == Some((0, source.ordinal)))?;
        rec.traps.push(TrapRow {
            at: row.at,
            kind: source.kind,
            site: source.site,
            held: row.held,
            subject: source.subject,
        });
    }

    let mut target_remap: blaze_own::Vec<u32, Alloc<'a>> =
        blaze_own::Vec::with_capacity_in(emitted.targets.len(), alloc);
    for t in emitted.targets.iter() {
        if let Some(&(_, start, len)) = emitted.names.iter().find(|(sym, _, _)| *sym == t.symbol) {
            let start_i = usize::try_from(start).ok()?;
            let end_i = start_i.checked_add(usize::try_from(len).ok()?)?;
            let slice = emitted.text.get(start_i..end_i)?;
            let s = core::str::from_utf8(slice).ok()?;
            target_remap.push(rec.outside(s));
        } else {
            let idx = u32::try_from(rec.targets.len()).ok()?;
            rec.targets.push(*t);
            target_remap.push(idx);
        }
    }

    for w in emitted.words.iter() {
        let target = if w.target == OWN_CODE || w.target == OWN_RELRO {
            w.target
        } else {
            *target_remap.get(usize::try_from(w.target).ok()?)?
        };
        rec.words.push(Word { at: w.at, target, addend: w.addend });
    }

    let mut i = 0;
    while let Some(&r) = emitted.relocs.get(i) {
        let next = emitted.relocs.get(i + 1).copied();
        if !r.in_relro
            && r.kind == RelocKind::GotPage21
            && r.addend == 0
            && let Some(r2) = next
            && !r2.in_relro
            && r2.kind == RelocKind::GotPageOff12
            && r2.at == r.at.checked_add(4)?
            && r2.target == r.target
            && r2.addend == 0
            && let Some(ldr_at) = usize::try_from(r.at).ok()?.checked_add(4)
            && let Some(word_bytes) = rec.code.get(ldr_at..ldr_at + 4)
        {
            let target = *target_remap.get(usize::try_from(r.target).ok()?)?;
            let ldr_word = u32::from_le_bytes([
                *word_bytes.first()?,
                *word_bytes.get(1)?,
                *word_bytes.get(2)?,
                *word_bytes.get(3)?,
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
            && let Some(r2) = next
            && !r2.in_relro
            && r2.kind == (RelocKind::PageOff12 { scale: 0 })
            && r2.at == r.at.checked_add(4)?
            && r2.target == OWN_RELRO
            && r2.addend == r.addend
            && usize::try_from(r.at).ok()?.checked_add(8).is_some_and(|e| e <= rec.code.len())
            && let Ok(addend_u32) = u32::try_from(r.addend)
            && u64::from(addend_u32) < u64::try_from(rec.relro.len()).ok()?
        {
            rec.sites.push(Site { at: r.at, kind: SiteKind::Relro, target: NO_TARGET, addend: addend_u32 });
            i += 2;
        } else {
            return None;
        }
    }
    Some((rec, insts))
}

/// Default AArch64 optimizing record emitter for `blaze-ladder` (`blaze run` / `blaze test`):
/// runs SROA + fixed-point Aegraph/Strength-Reduction/LICM/DCE, emits native fused
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
        let features = if isa.lse { Machine::LSE } else { 0 };
        let is_macos = isa.os == crate::Os::MacOs;
        if let Ok(emitted) = isel::emit_sota_a64(opt_v.view(), names, CallMode::GotSlot, features, is_macos, alloc) {
            if let Some((rec, insts)) = emitted_to_code_record(emitted, traps, alloc) {
                return Ok((rec, insts, Called::NONE));
            }
        }
        return emit_record_sourced(opt_v, names, routines, isa, BULK_INLINE_MOST, traps, alloc);
    }

    emit_record_sourced(func, names, routines, isa, BULK_INLINE_MOST, traps, alloc)
}
