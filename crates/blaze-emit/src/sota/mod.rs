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

pub mod abi;
pub mod bolt;
mod enc_x64;
mod isel;
pub mod module;
mod regalloc;
mod release_opts;
pub mod sme2;
mod x86_64;

pub use module::{CompiledModuleFn, ModuleFn, SotaModuleOpts, SotaModuleOut, compile_module_a64};

#[cfg(test)]
mod tests;

use blaze_code::record::{CodeRecord, NO_TARGET, OWN_CODE, OWN_RELRO, Site, SiteKind, TrapRow, TrapSource, Word};
use blaze_lir::{
    Op, Verified,
    amac::{AmacOpts, interleave_amac_loops},
    bce::eliminate_redundant_checks,
    bce_hoist::{BceHoistOpts, hoist_loop_bounds_checks},
    coldsplit::{partition_cold_blocks, reorder_blocks_rpo},
    egraph::{RegBudget, sota_optimize_fixpoint},
    gemm::{GemmOpts, optimize_gemm_loops},
    ifconv::if_convert,
    loopnest::{fuse_adjacent_loops, interchange_and_tile_loops},
    mach_egraph::{MachEGraphOpts, optimize_mach_egraph},
    memopt::{optimize_memory, scalarize_small_memops},
    modulo_sched::{ModuloSchedOpts, swing_modulo_schedule},
    prefetch::insert_software_prefetch,
    sched::schedule_dag,
    simd_idioms::{SimdIdiomOpts, lower_simd_idioms},
    simplify::{coalesce_cfg, dead, propagate, scalar_replace},
    slp::{slp_vectorize, vectorize_memchr_loops},
    superopt::{SuperoptOpts, superopt_a64},
    taildup::tail_duplicate_dispatch,
    thread_jumps,
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

/// Runs `scalar_replace -> propagate -> thread_jumps -> propagate -> dead -> coalesce_cfg`
/// (best-effort per sub-pass, verified before returning) so local stack slots (`Option<usize>`,
/// `Option<&T>`, `Range<usize>`) promote into SSA block parameters, phi-discriminant
/// `Switch`/`Br` chains thread away, and single-predecessor jump chains coalesce into canonical
/// loop shapes before downstream loop/vector/GEMM passes inspect the CFG.
fn run_sroa_thread_cleanup<'a>(
    input: blaze_lir::View<'_>,
    alloc: Alloc<'a>,
) -> Option<blaze_lir::Func<Alloc<'a>>> {
    let mut cur: Option<blaze_lir::Func<Alloc<'a>>> = scalar_replace(input, alloc).ok();
    let v = cur.as_ref().map_or(input, |f| f.view());
    if let Ok(f) = propagate(v, alloc) {
        cur = Some(f);
    }
    let v = cur.as_ref().map_or(input, |f| f.view());
    if let Ok(f) = thread_jumps(v, alloc) {
        cur = Some(f);
    }
    let v = cur.as_ref().map_or(input, |f| f.view());
    if let Ok(f) = propagate(v, alloc) {
        cur = Some(f);
    }
    let v = cur.as_ref().map_or(input, |f| f.view());
    if let Ok(f) = dead(v, alloc) {
        cur = Some(f);
    }
    let v = cur.as_ref().map_or(input, |f| f.view());
    if let Ok(f) = coalesce_cfg(v, alloc) {
        cur = Some(f);
    }
    let v = cur.as_ref().map_or(input, |f| f.view());
    if let Ok(Some(rpo)) = reorder_blocks_rpo(v, alloc) {
        cur = Some(rpo.func);
    }
    cur.filter(|f| verify(f.view()).is_ok())
}

/// Shared Stage 1a–1g loop, vectorization, dispatch tail-duplication, superoptimization,
/// scheduling, and hot/cold partitioning stack used by both [`optimize_sota_midend`] and
/// [`emit_sota_record_for_machine`].
fn run_sota_loop_and_late_passes<'a>(
    input: blaze_lir::View<'_>,
    machine: Machine,
    reg_budget: RegBudget,
    auto_vectorize: bool,
    superopt: Option<SuperoptOpts>,
    alloc: Alloc<'a>,
) -> blaze_lir::Func<Alloc<'a>> {
    let default_super_arch = match machine.arch {
        Arch::Arm64 => blaze_lir::superopt::AArch64MicroArch::AppleSilicon,
        Arch::X86_64 => blaze_lir::superopt::AArch64MicroArch::X86_64Zen5,
    };

    // Stage 1a: Small-constant `Memcpy`/`Memmove` scalarization (`1, 2, 4, 8` bytes ->
    // `Load` + `Store`) + post-fixpoint SROA + copy/const propagation + phi-discriminant jump
    // threading + DCE + CFG coalescing so swap temporaries and `Option`/iterator `Switch`/`Br`
    // discriminants exposed by `sota_optimize_fixpoint` collapse into canonical SSA loops before
    // BCE hoisting, GEMM, SIMD idioms, vectorization, and AMAC inspect the CFG.
    let scalarized_mem = scalarize_small_memops(input, alloc).ok().flatten();
    let sroa_in = scalarized_mem.as_ref().map_or(input, |f| f.view());
    let pre_loop_clean = run_sroa_thread_cleanup(sroa_in, alloc);
    let loop_in = match &pre_loop_clean {
        Some(f) => f.view(),
        None => sroa_in,
    };

    // Stage 1b-0: Dominating BCE + Safe-Rust Loop-Versioning Bounds-Check Hoisting + Triple-Nested
    // GEMM recognition (Auto-SME2 FMOPA / 4x16 NEON register blocking) run BEFORE `unswitch_loops`
    // so safe Rust slice loops hoist their panic checks and expose clean 1D/3D fast-path loops!
    let bce0_out = eliminate_redundant_checks(loop_in, alloc).ok();
    let post_bce0 = match &bce0_out {
        Some(o) => o.func.view(),
        None => loop_in,
    };
    let hoist_out = if auto_vectorize {
        hoist_loop_bounds_checks(post_bce0, BceHoistOpts::default_sota(), alloc)
            .ok()
            .filter(|o| o.stats.checks_hoisted > 0)
    } else {
        None
    };
    let post_hoist = match &hoist_out {
        Some(o) => o.func.view(),
        None => post_bce0,
    };
    let mut did_gemm = false;
    let gemm_func = if auto_vectorize && machine.arch == Arch::Arm64 {
        let gemm_opts = if (machine.features & Machine::SME2) != 0 {
            GemmOpts::sme2()
        } else {
            GemmOpts::neon()
        };
        optimize_gemm_loops(post_hoist, gemm_opts, alloc)
            .ok()
            .filter(|o| o.stats.sme2_lowered > 0 || o.stats.neon_blocked > 0)
            .map(|o| {
                did_gemm = true;
                o.func
            })
    } else {
        None
    };
    let post_gemm_in = match &gemm_func {
        Some(f) => f.view(),
        None => post_hoist,
    };

    // Stage 1b: Dominating Bounds-Check Elimination + Loop Unswitching + If-Conversion.
    let bce_out = eliminate_redundant_checks(post_gemm_in, alloc).ok();
    let post_bce = match &bce_out {
        Some(o) => o.func.view(),
        None => post_gemm_in,
    };
    let unsw_out = unswitch_loops(post_bce, 2, alloc).ok();
    let post_unsw = match &unsw_out {
        Some(o) => o.func.view(),
        None => post_bce,
    };
    let ifc_out = if_convert(post_unsw, 32, alloc).ok();
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

    // Stage 1c: Alias-aware Memory LICM + Dead Store Elimination + post-memopt SROA/jump-thread
    // cleanup + Loop Fusion & 2D Interchange.
    let mem_out = optimize_memory(post_clean, alloc).ok();
    let mem_changed = mem_out
        .as_ref()
        .is_some_and(|o| o.stats.loads_hoisted > 0 || o.stats.dead_stores_eliminated > 0);
    let mem_clean = if mem_changed {
        mem_out.as_ref().and_then(|o| run_sroa_thread_cleanup(o.func.view(), alloc))
    } else {
        None
    };
    let post_mem = match (&mem_clean, &mem_out) {
        (Some(f), _) => f.view(),
        (None, Some(o)) => o.func.view(),
        (None, None) => post_clean,
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

    // Stage 1d: Crazy-SOTA GEMM (Auto-SME2 FMOPA / 4x16 NEON register blocking), SIMD Idioms
    // (NEON TBL 16B/64B byte-LUTs, SDOT/UDOT, branchless sorting networks), AMAC 4-way
    // interleaved pointer-chasing coroutine prefetching, 4x-unrolled SIMD Auto-Vectorization,
    // SIMD `memchr`, SLP, IV-div NEON, 4x Scalar Unrolling, Loop Rotation, Indirect Prefetch,
    // and Swing Modulo Scheduling.
    let vec_bytes = if machine.arch == Arch::X86_64 && (machine.features & Machine::AVX512 != 0) {
        64
    } else if machine.arch == Arch::X86_64 && (machine.features & Machine::AVX2 != 0) {
        32
    } else {
        16
    };
    let mut loop_cur: Option<blaze_lir::Func<Alloc<'a>>> = None;
    let mut did_simd_idiom = false;
    let mut did_amac = false;
    let mut did_vectorize = false;
    let mut did_unroll = false;
    let mut did_modulo = false;
    if auto_vectorize {
        if machine.arch == Arch::Arm64 {
            if !did_gemm {
                let gemm_opts = if (machine.features & Machine::SME2) != 0 {
                    GemmOpts::sme2()
                } else {
                    GemmOpts::neon()
                };
                let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
                if let Ok(o) = optimize_gemm_loops(v, gemm_opts, alloc)
                    && (o.stats.sme2_lowered > 0 || o.stats.neon_blocked > 0)
                {
                    did_gemm = true;
                    loop_cur = Some(o.func);
                }
            }

            let idiom_opts = SimdIdiomOpts {
                enable_tbl: true,
                enable_dotprod: (machine.features & Machine::DOTPROD) != 0,
                enable_sort_net: true,
                enable_crc32: (machine.features & Machine::CRC32) != 0,
            };
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            if let Ok(o) = lower_simd_idioms(v, idiom_opts, alloc)
                && (o.stats.tbl_loops > 0
                    || o.stats.dotprod_loops > 0
                    || o.stats.crc32_loops > 0
                    || o.stats.sort_swaps_canonicalized > 0)
            {
                did_simd_idiom = o.stats.tbl_loops > 0 || o.stats.dotprod_loops > 0 || o.stats.crc32_loops > 0;
                loop_cur = Some(o.func);
            }

            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            if let Ok(o) = interleave_amac_loops(v, AmacOpts::apple_silicon(), alloc)
                && o.stats.nests_interleaved > 0
            {
                did_amac = true;
                loop_cur = Some(o.func);
            }
        }
        if !did_gemm {
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
        }
        if did_vectorize || did_gemm || did_simd_idiom {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            if let Ok(o) = sota_optimize_fixpoint(v, reg_budget, 2, |_| 1, alloc) {
                loop_cur = Some(o.func);
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
        if !did_amac && !did_gemm {
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
        if machine.arch == Arch::Arm64 && !did_amac {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            if let Ok(o) = insert_software_prefetch(v, 8, alloc)
                && o.stats.prefetches_inserted > 0
            {
                loop_cur = Some(o.func);
            }
        }
        if machine.arch == Arch::Arm64
            && !did_gemm
            && !did_simd_idiom
            && !did_amac
            && !did_vectorize
            && !did_unroll
        {
            let v = loop_cur.as_ref().map_or(post_nest, |f| f.view());
            if let Ok(o) = swing_modulo_schedule(v, ModuloSchedOpts::apple_m5(), alloc)
                && o.stats.loops_pipelined > 0
            {
                did_modulo = true;
                loop_cur = Some(o.func);
            }
        }
    }
    let post_loop = loop_cur.as_ref().map_or(post_nest, |f| f.view());

    // Stage 1d-ii: Interpreter / State-Machine `Switch` Tail Duplication (both 1-block direct and
    // 2-block bounded `dispatch -> decode(Switch)` loops).
    let taildup_func = if auto_vectorize {
        tail_duplicate_dispatch(post_loop, alloc).ok().filter(|o| o.stats.arms_duplicated > 0).map(|o| o.func)
    } else {
        None
    };
    let did_taildup = taildup_func.is_some();
    let post_td = match &taildup_func {
        Some(f) => f.view(),
        None => post_loop,
    };

    // Stage 1e: Joint Apple-M5 Machine-Op E-Graph + Bounded Multi-Round Superoptimization.
    let mach_func = if machine.arch == Arch::Arm64 {
        let mut mopts = MachEGraphOpts::apple_m5();
        mopts.enable_synth = false; // Fast deterministic saturation in hot path; synth available via explicit cache.
        optimize_mach_egraph(post_td, mopts, None, alloc)
            .ok()
            .filter(|o| o.stats.rewrites_applied > 0)
            .map(|o| o.func)
    } else {
        None
    };
    let post_mach = match &mach_func {
        Some(f) => f.view(),
        None => post_td,
    };
    let super_func = if let Some(mut sopts) = superopt {
        if machine.arch == Arch::X86_64 {
            sopts.microarch = default_super_arch;
        }
        superopt_a64(post_mach, sopts, |_| true, alloc).ok().map(|o| o.func)
    } else {
        None
    };
    let post_super = match &super_func {
        Some(f) => f.view(),
        None => post_mach,
    };

    // Stage 1f: Intra-block critical-path latency-aware DAG list scheduler (skipped after
    // vectorization, unrolling, GEMM, AMAC, SIMD idioms, modulo pipelining, or dispatch tail-dup).
    let sched_func = if !did_vectorize && !did_unroll && !did_taildup && !did_gemm && !did_amac && !did_simd_idiom && !did_modulo {
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
        return o.func;
    }
    post_sched.to_func_in(alloc)
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
    let reg_budget = match machine.arch {
        Arch::Arm64 => RegBudget::AARCH64,
        Arch::X86_64 => RegBudget { hardware_fma: (machine.features & Machine::FMA) != 0, ..RegBudget::X86_64 },
    };

    // Stage 0: Small-constant `Memcpy`/`Memmove` scalarization + SROA + copy/const propagation +
    // phi-discriminant jump threading + DCE + CFG coalescing promotes swap temporaries and
    // non-escaping local stack slots into SSA registers and collapses `Option`/iterator
    // discriminants before Aegraph + LICM + Strength Reduction.
    let scalarized_mem = scalarize_small_memops(input, alloc).ok().flatten();
    let stage0_in = scalarized_mem.as_ref().map_or(input, |f| f.view());
    let pre_clean = run_sroa_thread_cleanup(stage0_in, alloc);
    let mid_in = match &pre_clean {
        Some(f) => f.view(),
        None => stage0_in,
    };

    // Stage 1: Fixed-point Aegraph + Reassociation + Strength Reduction + LICM + DCE.
    let optimized = sota_optimize_fixpoint(mid_in, reg_budget, max_rounds, |_| 1, alloc)
        .map_err(|_| TierError::Refused {
            tier: TierName::Sota,
            inst: None,
            what: "SOTA mid-end optimization refused function",
        })?;

    Ok(run_sota_loop_and_late_passes(
        optimized.func.view(),
        machine,
        reg_budget,
        auto_vectorize,
        superopt,
        alloc,
    ))
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
        for row in emitted.traps.iter().filter(|t| t.site == Some((0, source.ordinal))) {
            rec.traps.push(TrapRow {
                at: row.at,
                kind: source.kind,
                site: source.site,
                held: row.held,
                subject: source.subject,
            });
        }
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

/// Explicit-`Machine` AArch64 optimizing record emitter for `blaze run`, `blaze test`, and
/// `blaze export`: runs SROA + fixed-point Aegraph/Reassociation/Strength-Reduction/LICM/DCE +
/// full SOTA loop, vectorization, GEMM, Apple-M5 machine-op e-graph, and superoptimization
/// passes, emits native fused AArch64 `Sota` code directly whenever the function fits `Sota`'s
/// subset (preserving 0..=N surviving/cloned trap rows via [`emitted_to_code_record`]), and
/// delegates exotic ops cleanly to `emit_record_sourced`.
///
/// Callers targeting Apple M4/M5 can pass explicit hardware feature flags
/// (`Machine::LSE | Machine::DOTPROD | Machine::CSSC | Machine::SME | Machine::SME2`)
/// deterministically.
pub fn emit_sota_record_for_machine<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    machine: Machine,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    let isa = Isa {
        lse: (machine.features & Machine::LSE) != 0,
        os: match machine.os {
            blaze_tier::Os::MacOs => crate::Os::MacOs,
            blaze_tier::Os::Linux => crate::Os::Linux,
        },
    };

    let scalarized_mem = scalarize_small_memops(func.view(), alloc).ok().flatten();
    let stage0_in = scalarized_mem.as_ref().map_or(func.view(), |f| f.view());
    let pre_clean = run_sroa_thread_cleanup(stage0_in, alloc);
    let mid_in = match &pre_clean {
        Some(f) => f.view(),
        None => stage0_in,
    };
    let sota_opt = sota_optimize_fixpoint(mid_in, RegBudget::AARCH64, 3, |_| 1, alloc).ok();
    let chosen_opt = sota_opt.as_ref().and_then(|res| verify(res.func.view()).ok());

    if let Some(opt_v) = chosen_opt {
        let late_func = run_sota_loop_and_late_passes(
            opt_v.view(),
            machine,
            RegBudget::AARCH64,
            true,
            Some(SuperoptOpts::apple_silicon()),
            alloc,
        );
        let full_v = match verify(late_func.view()) {
            Ok(v) => v,
            Err(_) => opt_v,
        };
        let is_macos = machine.os == blaze_tier::Os::MacOs;
        if let Ok(emitted) =
            isel::emit_sota_a64(full_v.view(), names, CallMode::GotSlot, machine.features, is_macos, alloc)
        {
            if let Some((rec, insts)) = emitted_to_code_record(emitted, traps, alloc) {
                return Ok((rec, insts, Called::NONE));
            }
        }
        // `emit_record_sourced` indexes `form.unreachables` by `TrapSource::ordinal`; if `opt_v`
        // changed the `Op::Unreachable` count while traps are present, delegate `func` so ordinals
        // stay intact on the baseline path.
        let fallback_v = if traps.is_empty()
            || opt_v.view().ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count()
                == func.view().ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count()
        {
            opt_v
        } else {
            func
        };
        return emit_record_sourced(fallback_v, names, routines, isa, BULK_INLINE_MOST, traps, alloc);
    }

    emit_record_sourced(func, names, routines, isa, BULK_INLINE_MOST, traps, alloc)
}

/// Default AArch64 optimizing record emitter for `blaze-ladder` (`blaze run` / `blaze test`):
/// derives the target [`Machine`] from `isa` (enabling `Machine::LSE` when advertised and
/// `Machine::DOTPROD | Machine::CRC32` on Apple Silicon macOS) and delegates to
/// [`emit_sota_record_for_machine`].
pub fn emit_sota_record_measured<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    isa: Isa,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    let features = (if isa.lse { Machine::LSE } else { 0 })
        | (if isa.os == crate::Os::MacOs { Machine::DOTPROD | Machine::CRC32 } else { 0 });
    let os = match isa.os {
        crate::Os::MacOs => blaze_tier::Os::MacOs,
        crate::Os::Linux => blaze_tier::Os::Linux,
    };
    let machine = Machine { arch: Arch::Arm64, os, features };
    emit_sota_record_for_machine(func, names, routines, machine, traps, alloc)
}
