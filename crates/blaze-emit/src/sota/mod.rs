//! Experimental Non-LLVM SOTA Optimizing Tier (`TierName::Sota`,
//! `docs/v2/plan/design/sota-tier.md`).
//!
//! Default optimizing tier on AArch64 (`blaze_tier::optimizing_tier`):
//! 1. **Fixed-point LIR mid-end (`blaze_lir::egraph::sota_optimize_fixpoint`)**: SROA +
//!    dominator-scoped Acyclic E-Graph (GVN + local store-to-load forwarding + algebraic
//!    identities + constant folding) + associative constant reassociation + materialized strength
//!    reduction + materialized LICM + DCE + optional bounded AArch64 e-graph superoptimization
//!    (`blaze_lir::superopt::superopt_a64`) for hot loops / release export.
//! 2. **Loop-Weighted, Coalescing Chordal-SSA Register Allocation (`regalloc`)**: allocates GPRs,
//!    2-word GPR pairs (`{i64, i1}`, 2-reg returns), and scalar FPs with Belady loop-weighted
//!    eviction, phi back-edge & return-value coalescing, `sret` (`X8`), and frameless leaves.
//! 3. **ISLE-Style Pattern-Fusing AArch64 Emitter (`isel`)**:
//!    - Fused `madd`/`msub`/`fmadd`/`fmsub`, `add/sub lsl/lsr`, `bic`/`orn`/`eon`, `csneg`/`csinc`/`csinv`
//!    - Native 32-bit `W` ALU ops (zero redundant `zext`), general 13-bit logical immediates
//!    - `tbz`/`tbnz` & `tst` branch fusion, scaled register-index `[base, idx, lsl #size]` loads/stores,
//!      adjacent `ldp`/`stp` load/store pair fusion
//!    - Granlund–Montgomery magic-multiply constant division/modulo (`umulh` + shift)
//!    - Dense `Op::Switch` O(1) PC-relative jump tables, Tail-Call Optimization (`b` / `br`),
//!      direct `Branch26` (`bl`) export mode (zero call-site NOPs), cold-block sinking & 16B loop alignment
//!    - Full scalar coverage: bitops (`clz`/`ctz`/`popcnt`/`bswap`/`bitrev`/`rotl`/`rotr`),
//!      `umulhi`/`smulhi`, `umin`/`umax`/`smin`/`smax`, saturating arithmetic, overflow pairs,
//!      LSE atomics (`swp`/`ldadd`/`ldset`/`ldeor`/`ldclr`/`ldsmax`/`ldsmin`/`ldumax`/`ldumin`) & fences,
//!      and 16-byte `ldp`/`stp` inline `memcpy`/`memset`.
//! 4. **Seamless Fallback (`Sota::with_fallback`)**: delegates exotic ops outside `Sota`'s subset
//!    (e.g. `asm!`, `f128`) already SOTA-optimized to the configured fallback tier.

mod enc_x64;
mod isel;
pub mod module;
mod regalloc;
mod release_opts;
mod x86_64;

pub use module::{CompiledModuleFn, ModuleFn, SotaModuleOpts, SotaModuleOut, compile_module_a64};

#[cfg(test)]
mod tests;

use blaze_code::record::{CodeRecord, NO_TARGET, OWN_CODE, OWN_RELRO, Site, SiteKind, TrapRow, TrapSource, Word};
use blaze_lir::{
    Op, Verified,
    bce::eliminate_redundant_checks,
    coldsplit::partition_cold_blocks,
    egraph::{RegBudget, sota_optimize_fixpoint},
    ifconv::if_convert,
    loopnest::{fuse_adjacent_loops, interchange_and_tile_loops},
    memopt::optimize_memory,
    sched::schedule_dag,
    simplify::scalar_replace,
    slp::{slp_vectorize, vectorize_memchr_loops},
    superopt::{SuperoptOpts, superopt_a64},
    taildup::tail_duplicate_dispatch,
    unroll::{rotate_loops, unroll_scalar_loops},
    unswitch::unswitch_loops,
    vectorize::{auto_vectorize_loops_with_width, vectorize_iv_div_loops},
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
    /// Optional bounded e-graph superoptimizer (enabled for release/export & hot PGO JIT functions).
    pub superopt: Option<SuperoptOpts>,
    /// Whether to run 2x-unrolled 128-bit SIMD loop auto-vectorization (`NEON`/`SSE2`).
    pub auto_vectorize: bool,
}

impl Sota<Reference> {
    /// Standalone SOTA tier (JIT `GotSlot` mode): compiles directly to fused AArch64/x86-64
    /// machine code with Memory LICM/DSE + 128-bit SIMD auto-vectorization.
    pub const fn standalone() -> Self {
        Self {
            fallback: None,
            max_rounds: 4,
            call_mode: CallMode::GotSlot,
            superopt: None,
            auto_vectorize: true,
        }
    }

    /// Hot PGO JIT tier: enables bounded e-graph superoptimization + 2x-unrolled 128-bit SIMD
    /// auto-vectorization + Memory LICM/DSE while keeping `GotSlot` live-patchable calls.
    pub const fn for_jit_hot() -> Self {
        Self {
            fallback: None,
            max_rounds: 3,
            call_mode: CallMode::GotSlot,
            superopt: Some(SuperoptOpts::apple_silicon()),
            auto_vectorize: true,
        }
    }

    /// Release AOT export SOTA tier: enables bounded e-graph superoptimization + 2x-unrolled
    /// 128-bit SIMD auto-vectorization + direct `Branch26`/`Pc32` calls + Tail-Call Optimization.
    pub const fn for_export() -> Self {
        Self {
            fallback: None,
            max_rounds: 3,
            call_mode: CallMode::DirectBl,
            superopt: Some(SuperoptOpts::apple_silicon()),
            auto_vectorize: true,
        }
    }
}

impl<F: Tier> Sota<F> {
    /// Full-coverage SOTA tier: runs `sota_optimize_fixpoint` + Memory LICM/DSE + SIMD vectorization
    /// on every function, emits fused AArch64/x86-64 directly whenever possible, and delegates exotic
    /// ops (already optimized!) to `fallback`.
    pub const fn with_fallback(fallback: F) -> Self {
        Self {
            fallback: Some(fallback),
            max_rounds: 3,
            call_mode: CallMode::GotSlot,
            superopt: None,
            auto_vectorize: true,
        }
    }

    /// Full-coverage hot PGO JIT SOTA tier with fallback for exotic ops.
    pub const fn for_jit_hot_with_fallback(fallback: F) -> Self {
        Self {
            fallback: Some(fallback),
            max_rounds: 3,
            call_mode: CallMode::GotSlot,
            superopt: Some(SuperoptOpts::apple_silicon()),
            auto_vectorize: true,
        }
    }

    /// Full-coverage release export SOTA tier with fallback for exotic ops.
    pub const fn for_export_with_fallback(fallback: F) -> Self {
        Self {
            fallback: Some(fallback),
            max_rounds: 3,
            call_mode: CallMode::DirectBl,
            superopt: Some(SuperoptOpts::apple_silicon()),
            auto_vectorize: true,
        }
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

/// Canonical SOTA mid-end optimization stack shared by single-function `Sota::emit`,
/// `emit_sota_record_measured`, and whole-program `compile_module_a64`.
pub(crate) fn optimize_sota_midend<'a>(
    input: blaze_lir::View<'_>,
    machine: Machine,
    max_rounds: u32,
    auto_vectorize: bool,
    superopt: Option<SuperoptOpts>,
    alloc: Alloc<'a>,
) -> Result<blaze_lir::Func<Alloc<'a>>, TierError> {
    let (reg_budget, default_super_arch) = match machine.arch {
        Arch::Arm64 => (RegBudget::AARCH64, blaze_lir::superopt::AArch64MicroArch::AppleSilicon),
        Arch::X86_64 => (
            RegBudget { hardware_fma: (machine.features & Machine::FMA) != 0, ..RegBudget::X86_64 },
            blaze_lir::superopt::AArch64MicroArch::X86_64Zen5,
        ),
    };

    // Stage 0: Scalar Replacement of Aggregates (SROA) promotes non-escaping local stack slots
    // straight into SSA registers before Aegraph + LICM + Strength Reduction.
    let sroa_func = scalar_replace(input, alloc).ok();
    let mid_in = match &sroa_func {
        Some(f) => f.view(),
        None => input,
    };

    // Stage 1: Fixed-point Aegraph + Reassociation + Strength Reduction + LICM + DCE.
    let optimized = sota_optimize_fixpoint(mid_in, reg_budget, max_rounds, |_| 1, alloc)
        .map_err(|_| TierError::Refused {
            tier: TierName::Sota,
            inst: None,
            what: "SOTA mid-end optimization refused function",
        })?;

    // Stage 1b: Dominating Bounds-Check Elimination + Loop Unswitching + If-Conversion.
    let bce_out = eliminate_redundant_checks(optimized.func.view(), alloc).ok();
    let post_bce = match &bce_out {
        Some(o) => o.func.view(),
        None => optimized.func.view(),
    };
    let unsw_out = unswitch_loops(post_bce, 2, alloc).ok();
    let post_unsw = match &unsw_out {
        Some(o) => o.func.view(),
        None => post_bce,
    };
    let ifc_out = if_convert(post_unsw, 8, alloc).ok();
    let post_ifc = match &ifc_out {
        Some(o) => o.func.view(),
        None => post_unsw,
    };
    let post_clean_func = if bce_out.as_ref().is_some_and(|o| o.stats.checks_eliminated > 0)
        || unsw_out.as_ref().is_some_and(|o| o.stats.loops_unswitched > 0)
        || ifc_out.as_ref().is_some_and(|o| o.stats.branches_converted > 0)
    {
        sota_optimize_fixpoint(post_ifc, reg_budget, 2, |_| 1, alloc).ok().map(|o| o.func)
    } else {
        None
    };
    let post_clean = match &post_clean_func {
        Some(f) => f.view(),
        None => post_ifc,
    };

    // Stage 1c: Alias-aware Memory LICM + Dead Store Elimination + Loop Fusion & 2D Interchange.
    let mem_func = optimize_memory(post_clean, alloc).ok().map(|o| o.func);
    let post_mem = match &mem_func {
        Some(f) => f.view(),
        None => post_clean,
    };
    let fuse_func = if auto_vectorize {
        fuse_adjacent_loops(post_mem, alloc).ok().filter(|o| o.stats.loops_fused > 0).map(|o| o.func)
    } else {
        None
    };
    let post_fuse = match &fuse_func {
        Some(f) => f.view(),
        None => post_mem,
    };
    let inter_func = if auto_vectorize {
        interchange_and_tile_loops(post_fuse, 0, alloc)
            .ok()
            .filter(|o| o.stats.loops_interchanged > 0)
            .map(|o| o.func)
    } else {
        None
    };
    let post_nest = match &inter_func {
        Some(f) => f.view(),
        None => post_fuse,
    };

    // Stage 1d: 4x-unrolled SIMD Loop Auto-Vectorization, SIMD `memchr`, SLP, IV-div NEON,
    // 4x Scalar Multi-Accumulator Unrolling, and Loop Rotation.
    let vec_bytes = if machine.arch == Arch::X86_64 && (machine.features & Machine::AVX512 != 0) {
        64
    } else if machine.arch == Arch::X86_64 && (machine.features & Machine::AVX2 != 0) {
        32
    } else {
        16
    };
    let mut loop_cur: Option<blaze_lir::Func<Alloc<'a>>> = None;
    let mut did_vectorize = false;
    let mut did_unroll = false;
    if auto_vectorize {
        for _ in 0..8 {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            match auto_vectorize_loops_with_width(v, vec_bytes, alloc) {
                Ok(o) if o.stats.loops_vectorized > 0 => {
                    did_vectorize = true;
                    loop_cur = Some(o.func);
                }
                _ => break,
            }
        }
        for _ in 0..8 {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            match vectorize_memchr_loops(v, alloc) {
                Ok(o) if o.stats.loops_vectorized > 0 => {
                    loop_cur = Some(o.func);
                }
                _ => break,
            }
        }
        {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            if let Ok(o) = slp_vectorize(v, alloc)
                && o.stats.trees_vectorized > 0
            {
                loop_cur = Some(o.func);
            }
        }
        if machine.arch == Arch::Arm64 {
            for _ in 0..8 {
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                match vectorize_iv_div_loops(v, alloc) {
                    Ok(o) if o.stats.loops_vectorized > 0 => {
                        did_vectorize = true;
                        loop_cur = Some(o.func);
                    }
                    _ => break,
                }
            }
        }
        for _ in 0..8 {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            match unroll_scalar_loops(v, alloc) {
                Ok(o) if o.stats.loops_unrolled > 0 => {
                    did_unroll = true;
                    loop_cur = Some(o.func);
                }
                _ => break,
            }
        }
        for _ in 0..8 {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            match rotate_loops(v, alloc) {
                Ok(o) if o.stats.loops_rotated > 0 => {
                    loop_cur = Some(o.func);
                }
                _ => break,
            }
        }
    }
    let post_loop = loop_cur.as_ref().map_or(post_nest, |f| f.view());

    // Stage 1d-ii: Interpreter / State-Machine `Switch` Tail Duplication.
    let taildup_func = if auto_vectorize {
        tail_duplicate_dispatch(post_loop, alloc).ok().filter(|o| o.stats.arms_duplicated > 0).map(|o| o.func)
    } else {
        None
    };
    let post_td = match &taildup_func {
        Some(f) => f.view(),
        None => post_loop,
    };

    // Stage 1e: Bounded multi-round e-graph superoptimization.
    let super_func = if let Some(mut sopts) = superopt {
        if machine.arch == Arch::X86_64 {
            sopts.microarch = default_super_arch;
        }
        superopt_a64(post_td, sopts, |_| true, alloc).ok().map(|o| o.func)
    } else {
        None
    };
    let post_super = match &super_func {
        Some(f) => f.view(),
        None => post_td,
    };

    // Stage 1f: Intra-block critical-path latency-aware DAG list scheduler.
    let sched_func = if !did_vectorize && !did_unroll {
        schedule_dag(post_super, alloc).ok().filter(|o| o.stats.insts_reordered > 0).map(|o| o.func)
    } else {
        None
    };
    let post_sched = match &sched_func {
        Some(f) => f.view(),
        None => post_super,
    };

    // Stage 1g: Hot/Cold block partitioning (sinks trap/panic/cleanup blocks to tail for L1i density).
    if let Ok(o) = partition_cold_blocks(post_sched, |_| false, alloc)
        && o.stats.blocks_sunk > 0
    {
        return Ok(o.func);
    }
    Ok(post_sched.to_func_in(alloc))
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
        let final_func = optimize_sota_midend(
            func.view(),
            machine,
            self.max_rounds,
            self.auto_vectorize,
            self.superopt,
            alloc,
        )?;
        let opt_verified = verify(final_func.view()).map_err(|_| TierError::Refused {
            tier: Self::NAME,
            inst: None,
            what: "SOTA optimized LIR failed verify",
        })?;

        // Stage 2 + 3: Loop-weighted Chordal-SSA regalloc + ISLE-style fused AArch64 or x86-64 emission
        // using all enabled target-CPU hardware intrinsics (`LSE`/`CSSC` on AArch64; `BMI2`/`FMA`/`AVX2` on x86-64).
        let emit_res = match machine.arch {
            Arch::Arm64 => {
                let is_macos = machine.os == blaze_tier::Os::MacOs;
                isel::emit_sota_a64(opt_verified.view(), names, self.call_mode, machine.features, is_macos, alloc)
            }
            Arch::X86_64 => {
                x86_64::emit_sota_x64(opt_verified.view(), names, self.call_mode, machine.features, alloc)
            }
        };
        match emit_res {
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
            let start_idx = usize::try_from(start).ok()?;
            let end_idx = usize::try_from(start.checked_add(len)?).ok()?;
            let s = core::str::from_utf8(emitted.text.get(start_idx..end_idx)?).ok()?;
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
        let r_at = usize::try_from(r.at).ok()?;
        if !r.in_relro
            && r.kind == RelocKind::GotPage21
            && r.addend == 0
            && let Some(n) = next
            && !n.in_relro
            && n.kind == RelocKind::GotPageOff12
            && n.at == r.at + 4
            && n.target == r.target
            && n.addend == 0
            && r_at + 8 <= rec.code.len()
        {
            let target = *target_remap.get(usize::try_from(r.target).ok()?)?;
            let ldr_at = r_at + 4;
            let ldr_bytes = rec.code.get(ldr_at..ldr_at + 4)?;
            let ldr_word = u32::from_le_bytes(ldr_bytes.try_into().ok()?);
            let ldr_rt = ldr_word & 0x1f;
            let is_call = ldr_rt == 16 && blaze_tier::record::calls(&rec.code, r.at + 4);
            let kind = if is_call { SiteKind::Call } else { SiteKind::Load };
            rec.sites.push(Site { at: r.at, kind, target, addend: 0 });
            i += 2;
        } else if !r.in_relro
            && r.kind == RelocKind::Page21
            && r.target == OWN_RELRO
            && let Ok(addend_u32) = u32::try_from(r.addend)
            && let Ok(addend_usize) = usize::try_from(addend_u32)
            && let Some(n) = next
            && !n.in_relro
            && n.kind == (RelocKind::PageOff12 { scale: 0 })
            && n.at == r.at + 4
            && n.target == OWN_RELRO
            && n.addend == r.addend
            && r_at + 8 <= rec.code.len()
            && addend_usize < rec.relro.len()
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
/// runs SROA + fixed-point Aegraph/Reassociation/Strength-Reduction/LICM/DCE, emits native fused
/// AArch64 `Sota` code directly whenever the function fits `Sota`'s subset, and delegates exotic
/// ops (already SOTA-optimized!) cleanly to `emit_record_sourced`.
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
        func.view().ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count()
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
            let after_unreach = v.view().ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count();
            if after_unreach != orig_unreach {
                return None;
            }
        }
        Some(v)
    });

    if let Some(opt_v) = chosen_opt {
        {
            // Run the full SOTA loop & straight-line optimization stack for direct native AArch64 `Sota`
            // emission (`BCE -> Unswitch -> IfConv -> MemOpt -> 4x NEON / memchr / iv-div NEON / SLP / 4x Unroll / Rotate -> TailDup -> Sched`).
            let bce_out = eliminate_redundant_checks(opt_v.view(), alloc).ok();
            let post_bce = bce_out.as_ref().map_or(opt_v.view(), |o| o.func.view());
            let unsw_out = unswitch_loops(post_bce, 2, alloc).ok();
            let post_unsw = unsw_out.as_ref().map_or(post_bce, |o| o.func.view());
            let ifc_out = if_convert(post_unsw, 8, alloc).ok();
            let post_ifc = ifc_out.as_ref().map_or(post_unsw, |o| o.func.view());
            let clean_func = if bce_out.as_ref().is_some_and(|o| o.stats.checks_eliminated > 0)
                || unsw_out.as_ref().is_some_and(|o| o.stats.loops_unswitched > 0)
                || ifc_out.as_ref().is_some_and(|o| o.stats.branches_converted > 0)
            {
                sota_optimize_fixpoint(post_ifc, RegBudget::AARCH64, 2, |_| 1, alloc).ok().map(|o| o.func)
            } else {
                None
            };
            let post_clean = clean_func.as_ref().map_or(post_ifc, |f| f.view());
            let mem_func = optimize_memory(post_clean, alloc).ok().map(|o| o.func);
            let post_mem = mem_func.as_ref().map_or(post_clean, |f| f.view());
            let fuse_f = fuse_adjacent_loops(post_mem, alloc).ok().filter(|o| o.stats.loops_fused > 0).map(|o| o.func);
            let post_fuse = fuse_f.as_ref().map_or(post_mem, |f| f.view());
            let inter_f = interchange_and_tile_loops(post_fuse, 0, alloc)
                .ok()
                .filter(|o| o.stats.loops_interchanged > 0)
                .map(|o| o.func);
            let post_nest = inter_f.as_ref().map_or(post_fuse, |f| f.view());
            let mut loop_cur: Option<blaze_lir::Func<Alloc<'a>>> = None;
            let mut did_vec = false;
            let mut did_unroll = false;
            for _ in 0..8 {
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                match auto_vectorize_loops_with_width(v, 16, alloc) {
                    Ok(o) if o.stats.loops_vectorized > 0 => {
                        did_vec = true;
                        loop_cur = Some(o.func);
                    }
                    _ => break,
                }
            }
            for _ in 0..8 {
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                match vectorize_memchr_loops(v, alloc) {
                    Ok(o) if o.stats.loops_vectorized > 0 => {
                        loop_cur = Some(o.func);
                    }
                    _ => break,
                }
            }
            {
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                if let Ok(o) = slp_vectorize(v, alloc)
                    && o.stats.trees_vectorized > 0
                {
                    loop_cur = Some(o.func);
                }
            }
            for _ in 0..8 {
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                match vectorize_iv_div_loops(v, alloc) {
                    Ok(o) if o.stats.loops_vectorized > 0 => {
                        did_vec = true;
                        loop_cur = Some(o.func);
                    }
                    _ => break,
                }
            }
            for _ in 0..8 {
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                match unroll_scalar_loops(v, alloc) {
                    Ok(o) if o.stats.loops_unrolled > 0 => {
                        did_unroll = true;
                        loop_cur = Some(o.func);
                    }
                    _ => break,
                }
            }
            for _ in 0..8 {
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                match rotate_loops(v, alloc) {
                    Ok(o) if o.stats.loops_rotated > 0 => {
                        loop_cur = Some(o.func);
                    }
                    _ => break,
                }
            }
            let post_loop = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            let td_f = tail_duplicate_dispatch(post_loop, alloc).ok().filter(|o| o.stats.arms_duplicated > 0).map(|o| o.func);
            let post_td = td_f.as_ref().map_or(post_loop, |f| f.view());
            let sched_f = if !did_vec && !did_unroll {
                schedule_dag(post_td, alloc).ok().filter(|o| o.stats.insts_reordered > 0).map(|o| o.func)
            } else {
                None
            };
            let post_sched = sched_f.as_ref().map_or(post_td, |f| f.view());
            let cold_f = partition_cold_blocks(post_sched, |_| false, alloc)
                .ok()
                .filter(|o| o.stats.blocks_sunk > 0)
                .map(|o| o.func);
            let final_v = cold_f.as_ref().map_or(post_sched, |f| f.view());

            let traps_ok = traps.is_empty()
                || final_v.ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count() == orig_unreach;
            let emit_v = if traps_ok { final_v } else { opt_v.view() };
            if let Ok(full_v) = verify(emit_v) {
                let features = if isa.lse { Machine::LSE } else { 0 };
                let is_macos = isa.os == crate::Os::MacOs;
                if let Ok(emitted) = isel::emit_sota_a64(full_v.view(), names, CallMode::GotSlot, features, is_macos, alloc) {
                    if let Some((rec, insts)) = emitted_to_code_record(emitted, traps, alloc) {
                        return Ok((rec, insts, Called::NONE));
                    }
                }
            }
        }
        return emit_record_sourced(opt_v, names, routines, isa, BULK_INLINE_MOST, traps, alloc);
    }

    emit_record_sourced(func, names, routines, isa, BULK_INLINE_MOST, traps, alloc)
}
