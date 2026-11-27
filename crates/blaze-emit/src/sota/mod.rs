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
mod layout;
pub mod memitter;
pub mod mfmt;
pub mod mfunc;
pub mod misel;
pub mod mopt;
pub mod mra;
pub mod msched;
pub mod module;
mod regalloc;
mod release_opts;
pub mod sme2;
mod x86_64;

pub use module::{CompiledModuleFn, ModuleFn, SotaModuleOpts, SotaModuleOut, compile_module_a64};

#[cfg(test)]
mod tests;

use blaze_code::record::{CodeRecord, NO_TARGET, OWN_CODE, OWN_RELRO, Site, SiteKind, Space, TrapRow, TrapSource, Word};
use blaze_lir::{
    Block, BlockKind, Op, Verified,
    amac::{AmacOpts, interleave_amac_loops},
    bce::eliminate_redundant_checks,
    bce_hoist::{BceHoistOpts, hoist_loop_bounds_checks},
    coldsplit::{partition_cold_blocks, reorder_blocks_rpo},
    egraph::{RegBudget, sota_optimize_fixpoint},
    func::{FnAttrs, RefId, RefKind},
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
    verify_in,
};
use blaze_own::Alloc;
use blaze_tier::{Arch, CallClobbers, Emitted, Machine, Names, RelocKind, Tier, TierError, TierName};

use crate::{BULK_INLINE_MOST, Called, Isa, RecordEmitError, Reference, RoutinePlaces, record::emit_record_sourced};

/// Explicit compilation effort profile for AArch64 `Sota` / `misel` emission (Rule 3: never a
/// silent fallback; callers and PGO select the tier explicitly).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmitProfile {
    /// **Fast-tier (default for cold / unprofiled functions, `docs/v2/plan/design/bottom.md`):**
    /// Skips all expensive LIR mid-end optimization passes (SROA/jump-threading, fixed-point
    /// `aegraph` GVN/LICM/strength-reduction, auto-vectorization, GEMM/AMAC/SLP, unrolling,
    /// modulo scheduling, mach-egraph, superopt, DAG scheduling, cold splitting), uses `O(V + E)`
    /// RPO block layout (skipping Ext-TSP), skips pre-RA & post-RA `mopt` and `msched`, and runs
    /// single-pass bitmask linear-scan `mra`.
    ///
    /// Preserves **100% identical runtime semantics**, AAPCS64 / `InternalFast` /
    /// `ColdPreserveAll` ABI, IPRA `CallClobbers`, unwind frames, call/import sites, trap rows,
    /// and source positions.
    Fast,
    /// **SOTA optimizing tier (for PGO-hot / loop-bearing / release-export hot functions):**
    /// Runs the full LIR mid-end (`sota_optimize_fixpoint` + loop/vector/GEMM/SIMD/AMAC/SLP/
    /// mach-egraph/superopt/DAG-sched/cold-split), Ext-TSP block layout, pre-RA & post-RA
    /// `mopt` + `msched`, and full coalescing/remat/cold-exit-splitting `mra`.
    Sota,
}

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
    /// Fast cold-tier standalone configuration (`EmitProfile::Fast`): skips mid-end optimization
    /// and uses fast linear-scan codegen while keeping exact runtime semantics and ABI.
    pub const fn fast() -> Self {
        Self {
            fallback: None,
            max_rounds: 0,
            call_mode: CallMode::GotSlot,
            superopt: None,
            auto_vectorize: false,
        }
    }

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
    /// Full-coverage fast cold-tier configuration (`EmitProfile::Fast`) with explicit fallback
    /// for exotic ops outside the direct emitter subset.
    pub const fn fast_with_fallback(fallback: F) -> Self {
        Self {
            fallback: Some(fallback),
            max_rounds: 0,
            call_mode: CallMode::GotSlot,
            superopt: None,
            auto_vectorize: false,
        }
    }

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
    cur.filter(|f| verify_in(f.view(), alloc).is_ok())
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

    // Stage 1f: Pre-RA intra-block critical-path latency-aware DAG list scheduler.
    // Provides register-pressure-bounded SSA ILP overlap before RA (while `mach_a64::mach_sched_m5`
    // performs post-RA Apple-M5 microarchitectural scheduling on AArch64). Runs even when a cold
    // inner loop was unrolled; skipped only after GEMM, AMAC, SIMD idioms, modulo pipelining, or
    // dispatch tail-dup.
    let sched_func = if !did_taildup
        && !did_gemm
        && !did_amac
        && !did_simd_idiom
        && !did_modulo
    {
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
        let fast_mode = self.max_rounds == 0 && !self.auto_vectorize && self.superopt.is_none();
        let opt_owned;
        let opt_verified = if fast_mode {
            func
        } else {
            opt_owned = optimize_sota_midend(
                func.view(),
                machine,
                self.max_rounds,
                self.auto_vectorize,
                self.superopt,
                alloc,
            )?;
            verify_in(opt_owned.view(), alloc).map_err(|_| TierError::Refused {
                tier: Self::NAME,
                inst: None,
                what: "SOTA optimized LIR failed verify",
            })?
        };

        // Stage 2 + 3: Register allocation + AArch64 or x86-64 emission using all enabled
        // target-CPU hardware intrinsics (`LSE`/`CSSC` on AArch64; `BMI2`/`FMA`/`AVX2` on x86-64).
        let emit_res = match machine.arch {
            Arch::Arm64 => {
                let is_macos = machine.os == blaze_tier::Os::MacOs;
                if fast_mode {
                    isel::emit_sota_a64_with_abi_profile(
                        opt_verified.view(),
                        names,
                        self.call_mode,
                        machine.features,
                        is_macos,
                        |_| CallClobbers::C_ABI_ARM64,
                        None,
                        false,
                        true,
                        alloc,
                    )
                    .map(|(emitted, _)| emitted)
                } else {
                    isel::emit_sota_a64(opt_verified.view(), names, self.call_mode, machine.features, is_macos, alloc)
                }
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
        if t.space.is_outside() {
            let &(_, start, len) = emitted.names.iter().find(|(sym, _, _)| *sym == t.symbol)?;
            let start_idx = usize::try_from(start).ok()?;
            let end_idx = usize::try_from(start.checked_add(len)?).ok()?;
            let s = core::str::from_utf8(emitted.text.get(start_idx..end_idx)?).ok()?;
            let idx = if t.space == Space::WeakOutside { rec.weak_outside(s) } else { rec.outside(s) };
            target_remap.push(idx);
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

/// Preferred caller-saved scratch allocation order for interprocedural register-avoidance
/// coloring (from [`abi::CustomCallPlan::scratch_gpr_order`] and
/// [`abi::CustomCallPlan::scratch_fp_order`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScratchOrder {
    pub gpr: [u8; 16],
    pub fp: [u8; 24],
}

impl ScratchOrder {
    /// Default caller-saved allocation order matching AArch64 caller-saved pools.
    pub const DEFAULT: Self = Self {
        gpr: regalloc::ScratchOrder::DEFAULT.gpr,
        fp: regalloc::ScratchOrder::DEFAULT.fp,
    };

    #[inline]
    pub(crate) const fn to_regalloc(self) -> regalloc::ScratchOrder {
        regalloc::ScratchOrder { gpr: self.gpr, fp: self.fp }
    }
}

/// Configuration for [`emit_sota_record_for_machine_with_abi`] (Rule 83: one options struct).
#[derive(Clone, Copy, Debug)]
pub struct SotaRecordAbiOpts<'a> {
    /// Target host machine architecture, OS, and ISA feature flags.
    pub machine: Machine,
    /// 2-tier hot/cold classification:
    /// - `true` ([`EmitProfile::Sota`]): runs the full LIR mid-end (`sota_optimize_fixpoint` +
    ///   loop/vector/GEMM/AMAC/SLP/mach-egraph/superopt/DAG-sched/cold-split), Ext-TSP block
    ///   layout, pre/post-RA `mopt` + `msched`, and full coalescing `mra`.
    /// - `false` ([`EmitProfile::Fast`]): skips all LIR mid-end optimization passes, uses
    ///   `O(V + E)` RPO block layout, skips `mopt` + `msched`, and runs single-pass bitmask
    ///   linear-scan `mra` while keeping identical runtime semantics, ABI, unwind frames, traps,
    ///   and IPRA `CallClobbers`.
    pub is_hot: bool,
    /// Optional interprocedural caller-avoidance register allocation order.
    pub scratch_order: Option<ScratchOrder>,
    /// When `true`, saves and restores modified caller-saved GPR/FP registers outside return
    /// slots (`CallConvKind::ColdPreserveAll`) so hot callers keep loop values live across cold
    /// calls with zero spills. Automatically suppressed if the body can unwind or tail-call.
    pub preserve_all_cold: bool,
    /// Sorted `(Outside::Func(place), CallClobbers)` table of already-emitted direct internal
    /// callees for bottom-up Interprocedural Register Allocation (IPRA). Keyed by `RefRec::place`,
    /// which stays invariant across intra-function LIR optimization passes.
    pub callee_clobbers: &'a [(u32, CallClobbers)],
    /// When `true`, this function's own signature uses `CallConvKind::InternalFast`
    /// (`X0..X7, X9..X15` GPR args and `V0..V7, V16..V23` FP args, keeping `X8` for `Sret`).
    pub internal_fast: bool,
    /// Sorted slice of `RefRec::place` indices (`RefKind::Func`) whose callee uses
    /// `CallConvKind::InternalFast`.
    pub internal_fast_callees: &'a [u32],
}

impl SotaRecordAbiOpts<'static> {
    /// Full optimizing SOTA defaults matching [`emit_sota_record_for_machine`]: C ABI clobbers,
    /// default scratch order, no `ColdPreserveAll`, full hot SOTA tier (`EmitProfile::Sota`).
    pub const fn for_machine(machine: Machine) -> Self {
        Self {
            machine,
            is_hot: true,
            scratch_order: None,
            preserve_all_cold: false,
            callee_clobbers: &[],
            internal_fast: false,
            internal_fast_callees: &[],
        }
    }

    /// Fast cold/unprofiled defaults (`EmitProfile::Fast`): C ABI clobbers, default scratch order,
    /// no `ColdPreserveAll`, skipping mid-end SOTA passes + `mopt`/`msched` + Ext-TSP.
    pub const fn for_machine_fast(machine: Machine) -> Self {
        Self {
            machine,
            is_hot: false,
            scratch_order: None,
            preserve_all_cold: false,
            callee_clobbers: &[],
            internal_fast: false,
            internal_fast_callees: &[],
        }
    }
}

impl SotaRecordAbiOpts<'_> {
    /// Returns the explicit [`EmitProfile`] selected by `self.is_hot`.
    #[inline]
    pub const fn emit_profile(&self) -> EmitProfile {
        if self.is_hot { EmitProfile::Sota } else { EmitProfile::Fast }
    }
}

/// Role a function plays in `CallConvKind::InternalFast` eligibility checking (Rule 85).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InternalFastRole {
    /// Function may call `InternalFast` callees via `Op::Call` / `Op::Invoke` / `Op::TailCall`.
    Caller,
    /// Function's own entry signature uses `InternalFast` (`X0..X7, X9..X15`, `V0..V7, V16..V23`).
    Callee,
}

/// Returns `true` iff `view` targets AArch64 and passes `misel`'s strict supported-subset check
/// for `role` so it can soundly participate in `CallConvKind::InternalFast` without ever falling
/// back to C-ABI emission.
pub fn can_use_internal_fast_a64(
    view: blaze_lir::View<'_>,
    names: &dyn Names,
    machine: Machine,
    role: InternalFastRole,
) -> bool {
    if machine.arch != Arch::Arm64 {
        return false;
    }
    misel::check_misel_subset(
        &view,
        names,
        misel::MIseOpts {
            machine,
            call_mode: CallMode::GotSlot,
            scratch_gpr_order: None,
            preserve_all_cold: false,
            call_clobbers: &[],
            internal_fast: role == InternalFastRole::Callee,
            internal_fast_calls: &[],
            fast_mode: false,
        },
    )
    .is_ok()
}

/// Returns `true` iff `view` can soundly use `CallConvKind::ColdPreserveAll` without breaking
/// compact/DWARF unwinding or tail-call lowering:
/// - no `Op::Invoke` / `Op::InvokeIndirect` / `Op::Resume` / non-normal landing-pad blocks,
/// - no `Op::TailCall` / `Op::TailCallIndirect` (refused by `isel` under `preserve_all_cold`),
/// - no inline `Op::Asm` / `Op::AsmBr`, and
/// - either explicitly `FnAttrs::NOUNWIND` or a strict zero-call leaf that cannot unwind.
fn is_safe_for_cold_preserve_all(view: blaze_lir::View<'_>) -> bool {
    for b in 0..view.blocks() {
        let Ok(b_u32) = u32::try_from(b) else {
            return false;
        };
        if view.block_kind(Block(b_u32)) != BlockKind::Normal {
            return false;
        }
    }
    let mut has_call = false;
    for inst in 0..view.len() {
        match view.op(inst) {
            Op::Invoke
            | Op::InvokeIndirect
            | Op::Resume
            | Op::TailCall
            | Op::TailCallIndirect
            | Op::Asm
            | Op::AsmBr => return false,
            Op::Call | Op::CallIndirect => {
                has_call = true;
            }
            _ => {}
        }
    }
    !has_call || (view.head.attrs & FnAttrs::NOUNWIND) != 0
}

/// Looks up a direct `Op::Call` / `Op::Invoke` / `Op::TailCall` instruction's IPRA callee
/// [`CallClobbers`] by its invariant `Outside::Func(place)` index in `callee_clobbers`.
fn lookup_direct_callee_clobbers(
    view: blaze_lir::View<'_>,
    inst: usize,
    callee_clobbers: &[(u32, CallClobbers)],
) -> Option<CallClobbers> {
    if callee_clobbers.is_empty() {
        return None;
    }
    if !matches!(view.op(inst), Op::Call | Op::Invoke | Op::TailCall) {
        return None;
    }
    let &data_at = view.data.get(inst)?;
    let &ref_word = usize::try_from(data_at).ok().and_then(|at| view.extra.get(at))?;
    let r = view.refs.get(RefId(ref_word).index())?;
    if RefKind::from_word(r.kind) != Some(RefKind::Func) {
        return None;
    }
    let pos = callee_clobbers.binary_search_by_key(&r.place, |&(p, _)| p).ok()?;
    callee_clobbers.get(pos).map(|&(_, clob)| clob)
}

/// Collects sorted LIR instruction indices of direct `Op::Call` / `Op::Invoke` / `Op::TailCall`
/// sites targeting any `RefKind::Func` whose invariant `RefRec::place` appears in sorted
/// `internal_fast_callees`.
fn collect_misel_fast_calls<'a>(
    view: blaze_lir::View<'_>,
    internal_fast_callees: &[u32],
    alloc: Alloc<'a>,
) -> blaze_own::Vec<u32, Alloc<'a>> {
    let mut out: blaze_own::Vec<u32, Alloc<'a>> = blaze_own::Vec::new_in(alloc);
    if internal_fast_callees.is_empty() {
        return out;
    }
    for inst in 0..view.len() {
        if !matches!(view.op(inst), Op::Call | Op::Invoke | Op::TailCall) {
            continue;
        }
        let Some(&data_at) = view.data.get(inst) else {
            continue;
        };
        let Some(&ref_word) = usize::try_from(data_at).ok().and_then(|at| view.extra.get(at)) else {
            continue;
        };
        let Some(r) = view.refs.get(RefId(ref_word).index()) else {
            continue;
        };
        if RefKind::from_word(r.kind) == Some(RefKind::Func)
            && internal_fast_callees.binary_search(&r.place).is_ok()
            && let Ok(inst_u32) = u32::try_from(inst)
        {
            out.push(inst_u32);
        }
    }
    out
}

/// Checks whether `view` fits `misel`'s strict subset under `opts` and `fast_calls`.
fn misel_subset_ok(
    view: blaze_lir::View<'_>,
    names: &dyn Names,
    opts: SotaRecordAbiOpts<'_>,
    fast_calls: &[u32],
) -> bool {
    let effective_preserve_all_cold =
        opts.preserve_all_cold && !opts.internal_fast && is_safe_for_cold_preserve_all(view);
    misel::check_misel_subset(
        &view,
        names,
        misel::MIseOpts {
            machine: opts.machine,
            call_mode: CallMode::GotSlot,
            scratch_gpr_order: opts.scratch_order.map(|o| o.gpr),
            preserve_all_cold: effective_preserve_all_cold,
            call_clobbers: &[],
            internal_fast: opts.internal_fast,
            internal_fast_calls: fast_calls,
            fast_mode: false,
        },
    )
    .is_ok()
}

/// Converts a `misel` [`TierError`] into a typed [`RecordEmitError`] when `InternalFast` ABI is
/// active so we never silently fall back to C-ABI emission (Rule 3).
fn map_internal_fast_tier_err(err: TierError) -> RecordEmitError {
    match err {
        TierError::NoName { of, .. } => RecordEmitError::NoSymbol { of },
        TierError::TooLarge { what, .. } => RecordEmitError::Emit(crate::EmitError::TooLarge { what }),
        TierError::Refused { what, .. } => RecordEmitError::Emit(crate::EmitError::Record { why: what }),
        TierError::Machine { .. } => {
            RecordEmitError::Emit(crate::EmitError::Record { why: "InternalFast requires AArch64 misel" })
        }
        TierError::Table { .. } => {
            RecordEmitError::Emit(crate::EmitError::Record { why: "misel table error under InternalFast" })
        }
        TierError::Record { .. } => {
            RecordEmitError::Emit(crate::EmitError::Record { why: "misel record error under InternalFast" })
        }
    }
}

/// Explicit-`Machine` + Zero-ABI / IPRA / 2-tier hot-cold AArch64 record emitter:
/// - Under [`EmitProfile::Fast`] (`!opts.is_hot`, or mega-functions past the mid-end bound):
///   skips all LIR mid-end SOTA passes (SROA/jump-threading, `aegraph` GVN/LICM/strength
///   reduction, auto-vectorization, GEMM/AMAC/SLP, unrolling, modulo scheduling, mach-egraph,
///   superopt, DAG scheduling, cold splitting) and emits directly through `misel` / `isel` in
///   fast mode (`O(V + E)` RPO layout, zero `mopt`/`msched`, single-pass bitmask `mra`).
/// - Under [`EmitProfile::Sota`] (`opts.is_hot` below the mega-function bound): runs the full
///   SOTA LIR mid-end + full `misel` / `isel` backend (`mopt`, `msched`, coalescing `mra`).
///
/// Both profiles preserve 100% identical runtime semantics, AAPCS64 / `InternalFast` /
/// `ColdPreserveAll` ABI, IPRA `CallClobbers`, unwind frames, import sites, and trap rows.
pub fn emit_sota_record_for_machine_with_abi<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    opts: SotaRecordAbiOpts<'_>,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called, CallClobbers), RecordEmitError> {
    let machine = opts.machine;
    let isa = Isa {
        lse: (machine.features & Machine::LSE) != 0,
        os: match machine.os {
            blaze_tier::Os::MacOs => crate::Os::MacOs,
            blaze_tier::Os::Linux => crate::Os::Linux,
        },
    };

    // Mega-function mid-end bound (rule 18, matching `value_info::MAX_BLOCKS` and
    // `layout::EXACT_LOOP_MAX_BLOCKS`): above this size (e.g. generated `cranelift-isle` decision
    // trees or huge functions with 10k+ blocks), use `EmitProfile::Fast` so compilation stays
    // strictly linear end-to-end.
    const SOTA_MIDEND_MAX_BLOCKS: usize = 4096;
    const SOTA_MIDEND_MAX_INSTS: usize = 65_536;
    let is_mega_fn =
        func.view().blocks() > SOTA_MIDEND_MAX_BLOCKS || func.view().len() > SOTA_MIDEND_MAX_INSTS;
    let effective_profile = if is_mega_fn { EmitProfile::Fast } else { opts.emit_profile() };
    let fast_mode = effective_profile == EmitProfile::Fast;

    let run_midend = !fast_mode;
    let max_rounds = 3;
    let superopt = Some(SuperoptOpts::apple_silicon());

    let scalarized_mem =
        run_midend.then(|| scalarize_small_memops(func.view(), alloc).ok().flatten()).flatten();
    let stage0_in = scalarized_mem.as_ref().map_or(func.view(), |f| f.view());
    let pre_clean = run_midend.then(|| run_sroa_thread_cleanup(stage0_in, alloc)).flatten();
    let mid_in = match &pre_clean {
        Some(f) => f.view(),
        None => stage0_in,
    };
    let sota_opt = run_midend
        .then(|| sota_optimize_fixpoint(mid_in, RegBudget::AARCH64, max_rounds, |_| 1, alloc).ok())
        .flatten();
    let chosen_opt = sota_opt.as_ref().and_then(|res| verify_in(res.func.view(), alloc).ok());
    let has_fast_abi_req = opts.internal_fast
        || !collect_misel_fast_calls(func.view(), opts.internal_fast_callees, alloc).is_empty();

    if let Some(opt_v) = chosen_opt.or_else(|| (has_fast_abi_req || fast_mode).then_some(func)) {
        let late_func = run_midend.then(|| {
            run_sota_loop_and_late_passes(
                opt_v.view(),
                machine,
                RegBudget::AARCH64,
                true,
                superopt,
                alloc,
            )
        });
        let full_v = match late_func.as_ref().map(|f| verify_in(f.view(), alloc)) {
            Some(Ok(v)) => v,
            _ => opt_v,
        };
        let is_macos = machine.os == blaze_tier::Os::MacOs;
        let initial_fast_calls = collect_misel_fast_calls(full_v.view(), opts.internal_fast_callees, alloc);
        let need_misel_retry = run_midend && (opts.internal_fast || !initial_fast_calls.is_empty());
        let late_no_vec: Option<blaze_lir::Func<Alloc<'a>>>;
        let emit_v: Verified<'_> = if need_misel_retry
            && !misel_subset_ok(full_v.view(), names, opts, initial_fast_calls.as_slice())
        {
            // Late auto-vectorization / GEMM / SIMD passes introduced an op outside `misel`'s
            // subset while `InternalFast` ABI is active: re-run late scalar passes with
            // `auto_vectorize = false` so BCE, unswitch, if-conv, and superopt still apply,
            // falling back to `opt_v` or original `func` if needed — never to C-ABI `isel`.
            late_no_vec = Some(run_sota_loop_and_late_passes(
                opt_v.view(),
                machine,
                RegBudget::AARCH64,
                false,
                superopt,
                alloc,
            ));
            if let Some(nv) = late_no_vec.as_ref().and_then(|f| verify_in(f.view(), alloc).ok())
                && {
                    let nv_fast = collect_misel_fast_calls(nv.view(), opts.internal_fast_callees, alloc);
                    misel_subset_ok(nv.view(), names, opts, nv_fast.as_slice())
                }
            {
                nv
            } else {
                let opt_fast = collect_misel_fast_calls(opt_v.view(), opts.internal_fast_callees, alloc);
                if misel_subset_ok(opt_v.view(), names, opts, opt_fast.as_slice()) {
                    opt_v
                } else {
                    func
                }
            }
        } else {
            full_v
        };

        let full_view = emit_v.view();
        let effective_preserve_all_cold =
            opts.preserve_all_cold && !opts.internal_fast && is_safe_for_cold_preserve_all(full_view);
        let ra_order = opts.scratch_order.map(ScratchOrder::to_regalloc);
        let lookup_clobbers = |inst: usize| -> Option<CallClobbers> {
            lookup_direct_callee_clobbers(full_view, inst, opts.callee_clobbers)
        };
        // Primary V3 AArch64 pure-SSA `MFunc` pipeline (`fast_mode` skips Ext-TSP, `mopt`,
        // `msched`, and multi-pass coalescing; SOTA runs the full backend optimization stack).
        let mut misel_call_clobs: blaze_own::Vec<(u32, CallClobbers), Alloc<'a>> =
            blaze_own::Vec::new_in(alloc);
        if !opts.callee_clobbers.is_empty() {
            for inst in 0..full_view.len() {
                if let Some(clob) = lookup_clobbers(inst)
                    && let Ok(inst_u32) = u32::try_from(inst)
                {
                    misel_call_clobs.push((inst_u32, clob));
                }
            }
        }
        let misel_fast_calls = collect_misel_fast_calls(full_view, opts.internal_fast_callees, alloc);
        let must_use_misel = opts.internal_fast || !misel_fast_calls.is_empty();
        let misel_result = misel::emit_mfunc_ssa_a64(
            emit_v,
            names,
            misel::MIseOpts {
                machine,
                call_mode: CallMode::GotSlot,
                scratch_gpr_order: opts.scratch_order.map(|o| o.gpr),
                preserve_all_cold: effective_preserve_all_cold,
                call_clobbers: misel_call_clobs.as_slice(),
                internal_fast: opts.internal_fast,
                internal_fast_calls: misel_fast_calls.as_slice(),
                fast_mode,
            },
            alloc,
        )
        .map(|(emitted, clobbers, _stats)| (emitted, clobbers));
        let emit_result = match misel_result {
            Ok(pair) => Ok(pair),
            Err(err) if must_use_misel => return Err(map_internal_fast_tier_err(err)),
            Err(_) => isel::emit_sota_a64_with_abi_profile(
                full_view,
                names,
                CallMode::GotSlot,
                machine.features,
                is_macos,
                |inst| lookup_clobbers(inst).unwrap_or(CallClobbers::C_ABI_ARM64),
                ra_order.as_ref(),
                effective_preserve_all_cold,
                fast_mode,
                alloc,
            ),
        };
        if let Ok((emitted, mut clobbers)) = emit_result {
            if let Some((rec, insts)) = emitted_to_code_record(emitted, traps, alloc) {
                // Soundly union callee clobbers for tail-called `Op::Call`/`Op::CallIndirect` sites
                // (which `regalloc` excludes from `call_positions` since they do not clobber locals).
                // `effective_preserve_all_cold` disables TCO and saves/restores modified scratch regs.
                if !effective_preserve_all_cold {
                    for inst in 0..full_view.len() {
                        match full_view.op(inst) {
                            Op::Call | Op::Invoke | Op::TailCall => {
                                let callee_clob = lookup_clobbers(inst).unwrap_or(CallClobbers::C_ABI_ARM64);
                                clobbers.clobbered = clobbers.clobbered.union(callee_clob.clobbered);
                            }
                            Op::CallIndirect | Op::InvokeIndirect | Op::TailCallIndirect => {
                                clobbers.clobbered = clobbers.clobbered.union(CallClobbers::C_ABI_ARM64.clobbered);
                            }
                            _ => {}
                        }
                    }
                }
                return Ok((rec, insts, Called::NONE, clobbers));
            }
            if must_use_misel {
                return Err(RecordEmitError::Emit(crate::EmitError::Record {
                    why: "misel InternalFast emitted record conversion failed",
                }));
            }
        } else if must_use_misel {
            return Err(RecordEmitError::Emit(crate::EmitError::Record {
                why: "misel InternalFast emission refused",
            }));
        }
        // `emit_record_sourced` indexes `form.unreachables` by `TrapSource::ordinal`; if `opt_v`
        // changed the `Op::Unreachable` count while traps are present, delegate `func` so ordinals
        // stay intact on the baseline path.
        let fallback_v = if fast_mode
            || traps.is_empty()
            || opt_v.view().ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count()
                == func.view().ops.iter().filter(|&&op| Op::from_byte(op) == Some(Op::Unreachable)).count()
        {
            opt_v
        } else {
            func
        };
        let (rec, insts, called) =
            emit_record_sourced(fallback_v, names, routines, isa, BULK_INLINE_MOST, traps, alloc)?;
        return Ok((rec, insts, called, CallClobbers::C_ABI_ARM64));
    }

    let (rec, insts, called) = emit_record_sourced(func, names, routines, isa, BULK_INLINE_MOST, traps, alloc)?;
    Ok((rec, insts, called, CallClobbers::C_ABI_ARM64))
}

/// Explicit-`Machine` + explicit-[`EmitProfile`] AArch64 code record emitter.
pub fn emit_sota_record_with_profile<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    machine: Machine,
    profile: EmitProfile,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    let opts = match profile {
        EmitProfile::Fast => SotaRecordAbiOpts::for_machine_fast(machine),
        EmitProfile::Sota => SotaRecordAbiOpts::for_machine(machine),
    };
    let (rec, insts, called, _clobbers) = emit_sota_record_for_machine_with_abi(
        func,
        names,
        routines,
        opts,
        traps,
        alloc,
    )?;
    Ok((rec, insts, called))
}

/// Explicit-`Machine` AArch64 optimizing record emitter for `blaze export`: delegates to
/// [`emit_sota_record_with_profile`] with [`EmitProfile::Sota`].
pub fn emit_sota_record_for_machine<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    machine: Machine,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    emit_sota_record_with_profile(func, names, routines, machine, EmitProfile::Sota, traps, alloc)
}

/// Default AArch64 cold/unprofiled record emitter for `blaze-ladder` (`blaze build` / `blaze run` /
/// `blaze test`): derives the target [`Machine`] from `isa` and emits via [`EmitProfile::Fast`]
/// (per `docs/v2/plan/design/bottom.md`, "Tiers": first code is fast-tier; PGO-hot and release
/// export functions explicitly opt into [`EmitProfile::Sota`]).
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
    emit_sota_record_with_profile(func, names, routines, machine, EmitProfile::Fast, traps, alloc)
}
