//! The linear simplify stage (docs/v2/plan/design/backend-next.md, D100 point 1): blaze's own
//! LIR-to-LIR passes in front of the Fast tier, each one walk. After the trivial inliner has
//! spliced `next` or `deref` in, the `Option`, the range and the iterator still live in stack
//! slots; these passes put them in values, propagate copies and constants, and drop dead code,
//! so the Fast tier's code keeps them in registers. Each pass is validated as a tier is:
//! evaluator equality on generated programs, and a planted wrong rewrite caught.
//!
//! - [`sroa`]: scalar replacement of slots whose address does not escape.
//! - `prop`: copy and constant propagation, folding by the evaluator's arithmetic ([`propagate`]).
//! - `dce`: dead code and trivial branches ([`dead`]), and the rewrite every pass makes its output with.
//! - `stage`: [`simplify`], the inliner, `scalar_replace`, `propagate` and `dead`, once each.

use core::fmt;

use crate::{BuildError, VerifyError};

pub mod amac;
pub mod bce;
pub mod bce_hoist;
pub mod coldsplit;
pub mod coro_opt;
mod dce;
pub mod devirt;
pub mod egraph;
pub mod escape;
pub mod gemm;
pub mod ifconv;
pub mod jump_thread;
pub mod loopnest;
pub mod mach_egraph;
pub mod memopt;
pub mod modulo_sched;
pub mod prefetch;
mod prop;
pub mod rvsdg;
pub mod sched;
pub mod semproof;
pub mod simd_idioms;
pub mod slp;
pub mod soa;
pub mod speculate;
pub mod sroa;
mod stage;
pub mod superopt;
pub mod taildup;
pub mod unroll;
pub mod unswitch;
#[cfg(any(test, feature = "eval"))]
pub mod validate;
pub mod vectorize;

pub use amac::{AmacOpts, AmacOutput, AmacStats, interleave_amac_loops};
pub use bce::{BceOutput, BceStats, eliminate_redundant_checks};
pub use bce_hoist::{BceHoistOpts, BceHoistOutput, BceHoistStats, hoist_loop_bounds_checks};
pub use coldsplit::{ColdSplitOutput, ColdSplitStats, RpoOrderOutput, partition_cold_blocks, reorder_blocks_rpo};
pub use coro_opt::{
    CoroFramePlan, CoroFrameSlot, CoroFrameStats, CoroFuseOutput, CoroFuseStats, fuse_nested_coroutines,
    pack_coroutine_frame_colored,
};
pub use dce::{coalesce_cfg, dead};
pub use devirt::{DevirtOutput, DevirtStats, IndirectTarget, speculative_devirt};
pub use egraph::{
    AegraphOutput, AegraphStats, LoopSummary, OpportunisticInlineSite, RegBudget, SotaFeedbackOutput, SsaRegPlan,
    aegraph_optimize, hoist_loop_invariants, materialize_strength_reduction, plan_opportunistic_inlining,
    plan_ssa_regalloc, sota_optimize_fixpoint,
};
pub use escape::{AllocRole, EscapeOutput, EscapeStats, EscapeSummary, promote_heap_to_stack, summarize_escape};
pub use gemm::{
    GemmOpts, GemmOutput, GemmStats, SME2_GEMM_F32_INTRINSIC, SME2_GEMM_F64_INTRINSIC,
    SME2_GEMM_I8_I32_INTRINSIC, SME2_GEMM_U8_U32_INTRINSIC, optimize_gemm_loops,
};
pub use ifconv::{IfConvOutput, IfConvStats, if_convert};
pub use jump_thread::thread_jumps;
pub use loopnest::{LoopNestOutput, LoopNestStats, fuse_adjacent_loops, interchange_and_tile_loops};
pub use mach_egraph::{
    AppleM5PortModel, CacheDecodeError, CachedRewrite, MachEGraphOpts, MachEGraphOutput, MachEGraphStats,
    SuperoptCache, optimize_mach_egraph,
};
pub use memopt::{MemOptOutput, MemOptStats, optimize_memory};
pub use modulo_sched::{ModuloSchedOpts, ModuloSchedOutput, ModuloSchedStats, swing_modulo_schedule};
pub use prefetch::{
    PREFETCH_READ_L1_INTRINSIC, PrefetchOutput, PrefetchStats, build_fast_utoa_u64, build_swisstable_probe_16b,
    insert_software_prefetch,
};
pub use prop::{Plant, propagate, propagate_planted};
pub use rvsdg::{PortCostModel, RegionCost, RvsdgOutput, RvsdgStats, co_optimize_regions, estimate_function_cost};
pub use sched::{SchedOutput, SchedStats, schedule_dag};
pub use semproof::{ParamProof, SemProofOutput, SemProofStats, apply_semantic_proofs};
pub use simd_idioms::{
    A64_CRC32B_INTRINSIC, A64_CRC32CB_INTRINSIC, A64_CRC32CX_INTRINSIC, A64_CRC32X_INTRINSIC, NEON_SDOT_INTRINSIC,
    NEON_TBL16_INTRINSIC, NEON_TBL64_INTRINSIC, NEON_UDOT_INTRINSIC, SimdIdiomOpts, SimdIdiomOutput, SimdIdiomStats,
    emit_sort4_network, emit_sort4_values, emit_sort8_network, emit_sort8_values, lower_byte_lut_loops,
    lower_crc32_table_loops, lower_dotprod_loops, lower_simd_idioms, lower_sorting_networks,
};
pub use slp::{MemchrOutput, MemchrStats, SlpOutput, SlpStats, slp_vectorize, vectorize_memchr_loops};
pub use soa::{SoaAnalysis, SoaOutput, SoaStats, StructField, analyze_aos_loop, rewrite_aos_to_soa};
pub use speculate::{
    OsrPlan, SpeculateOutput, SpeculateStats, ValueSpecHint, guard_alias_for_simd, specialize_guarded_value,
    synthesize_osr_entry,
};
pub use sroa::scalar_replace;
pub use stage::simplify;
pub use superopt::{AArch64MicroArch, SuperoptOpts, SuperoptOutput, SuperoptStats, superopt_a64};
pub use taildup::{TailDupOutput, TailDupStats, tail_duplicate_dispatch};
pub use unroll::{RotateOutput, RotateStats, UnrollOutput, UnrollStats, rotate_loops, unroll_scalar_loops};
pub use unswitch::{UnswitchOutput, UnswitchStats, unswitch_loops};
#[cfg(any(test, feature = "eval"))]
pub use validate::{
    Counterexample, OutcomeSummary, ValidationOpts, ValidationReport, prove_i8_rewrite_exact, validate_equivalence,
};
pub use vectorize::{VectorizeOutput, VectorizeStats, auto_vectorize_loops, auto_vectorize_loops_with_width, vectorize_iv_div_loops};

#[cfg(test)]
mod prop_tests;
#[cfg(test)]
mod sroa_tests;

/// Why a simplify pass made no function.
#[derive(Debug)]
pub enum SimplifyError {
    /// A builder the pass drove refused what it was given.
    Build(BuildError),
    /// The pass's output is not LIR: a bug of the pass, named by the verifier's rule.
    Verify(VerifyError),
    /// Instruction `inst` of the input is a form the pass does not take, or the output would
    /// not fit LIR's columns: `what` says which.
    Refused { inst: usize, what: &'static str },
}

impl fmt::Display for SimplifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimplifyError::Build(error) => write!(f, "the simplified function: {error}"),
            SimplifyError::Verify(error) => write!(f, "the simplified function is not LIR: {error}"),
            SimplifyError::Refused { inst, what } => {
                write!(f, "instruction {inst}: the simplify stage refuses {what}")
            }
        }
    }
}

impl core::error::Error for SimplifyError {}
