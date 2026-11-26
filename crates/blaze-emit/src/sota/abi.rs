//! Zero-ABI Whole-Program Custom Calling Convention, Interprocedural Register-Avoidance
//! Coloring, Global SCC Register Pinning & `preserve_all` Cold-Call ABI for AArch64.
//!
//! Pure sans-IO module analysis over verified `blaze_lir::View`s:
//! 1. **ABI Safety Proof**: exported and address-taken functions stay strictly on AAPCS64
//!    ([`CallConvKind::CAbi`]); only strictly module-internal, non-address-taken functions are
//!    upgraded.
//! 2. **Cold `preserve_all`** ([`CallConvKind::ColdPreserveAll`]): internal cold / panic-trap
//!    callees preserve all 28 GPRs (`x0..x17`, `x19..x28`) and all 32 NEON/FP registers
//!    (`v0..v31`) outside their return slots so hot callers incur zero spills across cold edges,
//!    while [`cold_preserve_save_set`] lets the cold callee save only the registers it actually
//!    touches.
//! 3. **Wide Internal Fast ABI** ([`CallConvKind::InternalFast`]): packs up to 15 GPR arguments
//!    (`x0..x7`, `x9..x15`, keeping `x8` dedicated to `sret`), 16 FP/NEON arguments
//!    (`v0..v7`, `v16..v23`, excluding callee-saved `v8..v15` and scratch `v29..v31`), and multi-reg
//!    returns (`x0..x7` / `v0..v7`), eliminating stack argument traffic and `x8` `sret` memory
//!    round-trips.
//! 4. **Interprocedural Register-Avoidance Coloring**: aggregates caller live-register pressure
//!    across all direct callers of each callee and reorders `scratch_gpr_order` /
//!    `scratch_fp_order` so callees allocate caller-untouched registers first and caller-hot
//!    registers last.
//! 5. **Global SCC Register Pinning** ([`CallConvKind::PinnedScc`]): detects invariant `ptr`
//!    parameters passed unchanged across internal SCCs and call chains and pins them to dedicated
//!    callee-preserved registers (`x28`, `x27`, `x26`, `x25`) with zero argument moves or reloads.

use core::alloc::Allocator;

use blaze_lir::{
    Op, Ty, View,
    func::{Conv, FnAttrs, ParamFactsRec, ParamKind, ParamRec, RefKind, SigRec},
    lto::tarjan_sccs,
    ty::Form,
};
use blaze_own::Vec;
use blaze_tier::{CallClobbers, Reg, RegSet};

/// AAPCS64 C-ABI argument GPR cap (`x0..x7`).
const C_ABI_MAX_GPR_ARGS: u8 = 8;
/// AAPCS64 C-ABI argument FP/NEON cap (`v0..v7`).
const C_ABI_MAX_FP_ARGS: u8 = 8;
/// AAPCS64 C-ABI direct GPR return cap (`x0..x1`).
const C_ABI_MAX_GPR_RETS: u8 = 2;
/// AAPCS64 C-ABI direct FP/NEON return cap (`v0..v3`).
const C_ABI_MAX_FP_RETS: u8 = 4;
/// AAPCS64 indirect result pointer register (`x8`).
const A64_SRET_REG: u8 = 8;

/// Canonical `InternalFast` GPR argument register pool (`x0..x7`, `x9..x15`):
/// - Indices `0..8` match AAPCS64 (`x0..x7`) so 1..=8-arg calls remain zero-cost compatible.
/// - Strictly excludes `x8` (dedicated `Sret` pointer), `x16`/`x17` (linker / `GotSlot` scratch),
///   `x18` (macOS platform register), and `x19..x28` (callee-saved).
pub const INTERNAL_FAST_GPR_ARGS: [u8; 15] = [0, 1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15];

/// Canonical `InternalFast` FP/NEON argument register pool (`v0..v7`, `v16..v23`):
/// - Indices `0..8` match AAPCS64 (`v0..v7`).
/// - Strictly excludes `v8..v15` (AAPCS64 callee-saved) and `v29..v31` (inline NEON / RA spill
///   scratch).
pub const INTERNAL_FAST_FP_ARGS: [u8; 16] =
    [0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23];

/// Direct multi-word GPR return register pool (`x0..x3`) for `InternalFast` struct return lowering.
pub const INTERNAL_FAST_MAX_GPR_RETS: usize = 4;

/// Hardware cap on `InternalFast` argument GPRs (`INTERNAL_FAST_GPR_ARGS.len()`).
const MAX_HW_FAST_GPR_ARGS: u8 = 15;
/// Hardware cap on `InternalFast` argument FP/NEON registers (`INTERNAL_FAST_FP_ARGS.len()`).
const MAX_HW_FAST_FP_ARGS: u8 = 16;
/// Hardware cap on `InternalFast` return registers per class (`x0..x7` / `v0..v7`).
const MAX_HW_FAST_RETS: u8 = 8;

/// Standard AAPCS64 callee-preserved register mask: `x19..=x28` and `v8..=v15`.
const C_ABI_PRESERVED: RegSet = RegSet(0x1ff8_0000 | (0x0000_ff00u64 << 32));

/// Full `ColdPreserveAll` preserved register mask before removing return registers:
/// all 28 GPRs (`x0..=x17`, `x19..=x28`; `x18` is macOS platform-reserved) and all 32 NEON
/// registers (`v0..=v31`).
const COLD_PRESERVE_ALL_BASE: RegSet = RegSet(0x1ffb_ffff | (0xffff_ffffu64 << 32));

/// Default scratch GPR allocation order matching `sota::regalloc`'s caller-saved GPR pool
/// (`x9..x15`, `x8`, `x0..x7`).
const DEFAULT_SCRATCH_GPR_ORDER: [u8; 16] =
    [9, 10, 11, 12, 13, 14, 15, 8, 0, 1, 2, 3, 4, 5, 6, 7];

/// Default scratch FP/NEON allocation order covering all 24 caller-saved AAPCS64 vector registers
/// (`v16..v28`, `v0..v7`, `v29..v31`; `v8..v15` are callee-saved).
const DEFAULT_SCRATCH_FP_ORDER: [u8; 24] = [
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 0, 1, 2, 3, 4, 5, 6, 7, 29, 30, 31,
];

/// Callee-preserved GPRs allocated top-down for global SCC / call-chain pointer pinning.
const PINNED_SCC_GPRS: [u8; 4] = [28, 27, 26, 25];

/// Calling convention selected for one module function.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CallConvKind {
    /// Standard AAPCS64 C ABI (`x0..x7` / `v0..v7` args, `x0..x1` / `v0..v3` returns, `x8` sret).
    /// Mandatory for exported and address-taken functions.
    CAbi,
    /// Module-internal wide register ABI: up to 15 GPR args (`x0..x7`, `x9..x15`, keeping `x8`
    /// dedicated to `sret`), 16 FP/NEON args (`v0..v7`, `v16..v23`), and multi-reg returns
    /// (`x0..x7` / `v0..v7`).
    InternalFast,
    /// Module-internal cold ABI preserving all 28 GPRs (`x0..x17`, `x19..x28`) and all 32 NEON
    /// registers (`v0..v31`) outside return slots so hot callers suffer zero spills.
    ColdPreserveAll,
    /// Module-internal fast ABI with one or more invariant pointer parameters permanently pinned
    /// to dedicated callee-preserved registers (`x28`, `x27`, `x26`, `x25`) across an SCC or chain.
    PinnedScc,
}

/// Physical location assigned to a single parameter or return value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ArgSlot {
    /// AArch64 general-purpose register (`x0..x28`).
    Gpr(u8),
    /// AArch64 FP/NEON register (`v0..v31`).
    Fp(u8),
    /// Outgoing/incoming stack argument area byte offset (8-byte or 16-byte aligned).
    StackOff(u32),
}

/// One parameter pinned to a fixed callee-preserved GPR across an entire SCC or call chain.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PinnedReg {
    pub param_index: u16,
    pub reg: u8,
}

/// Per-function calling convention and register-avoidance plan emitted by [`plan_module_abi`].
#[derive(Debug)]
pub struct CustomCallPlan<A: Allocator> {
    pub conv: CallConvKind,
    pub param_slots: Vec<ArgSlot, A>,
    pub return_slots: Vec<ArgSlot, A>,
    pub scratch_gpr_order: [u8; 16],
    pub scratch_fp_order: [u8; 24],
    pub preserved_regs: RegSet,
    pub pinned_globals: Vec<PinnedReg, A>,
}

/// Aggregate telemetry produced by [`plan_module_abi`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct ZeroAbiStats {
    pub internal_fast_fns: usize,
    pub wide_arg_fast_fns: usize,
    pub cold_preserve_all_fns: usize,
    pub stack_args_eliminated: usize,
    pub sret_eliminated: usize,
    pub pinned_scc_params: usize,
    pub avoided_clobber_regs: usize,
}

/// Whole-module Zero-ABI plan indexed 1:1 with the input function slice.
#[derive(Debug)]
pub struct ModuleAbiPlan<A: Allocator> {
    pub plans: Vec<CustomCallPlan<A>, A>,
    pub stats: ZeroAbiStats,
}

/// Configuration for [`plan_module_abi`] (Rule 83: one options struct).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ZeroAbiOpts {
    pub max_fast_gpr_args: u8,
    pub max_fast_fp_args: u8,
    pub max_fast_returns: u8,
    pub enable_cold_preserve_all: bool,
    pub enable_scc_pinning: bool,
}

impl ZeroAbiOpts {
    /// Release AArch64 export defaults: 15 GPR args (`x0..x7`, `x9..x15`), 16 FP/NEON args
    /// (`v0..v7`, `v16..v23`), 8 register returns (`x0..x7` / `v0..v7`), cold `preserve_all`, and
    /// global SCC pinning.
    pub const fn arm64_export() -> Self {
        Self {
            max_fast_gpr_args: MAX_HW_FAST_GPR_ARGS,
            max_fast_fp_args: MAX_HW_FAST_FP_ARGS,
            max_fast_returns: MAX_HW_FAST_RETS,
            enable_cold_preserve_all: true,
            enable_scc_pinning: true,
        }
    }
}

/// One module function's inputs to [`plan_module_abi`].
///
/// Register sets follow `blaze_tier::Reg` numbering: GPRs are `Reg(0..32)` and FP/NEON registers
/// are `Reg(32..64)` (`Reg(32 + v_num)`).
#[derive(Clone, Copy, Debug)]
pub struct AbiFnInput<'a> {
    pub view: View<'a>,
    pub is_exported: bool,
    pub address_taken: bool,
    pub is_cold: bool,
    pub direct_callees: &'a [usize],
    pub caller_live_pressure_gprs: RegSet,
    pub caller_live_pressure_fps: RegSet,
}

/// Computes the exact caller-visible [`CallClobbers`] mask for a callee governed by `plan`,
/// given the registers `actual_touched` that the callee body actually writes.
#[inline]
pub fn plan_call_clobbers<A: Allocator>(
    plan: &CustomCallPlan<A>,
    actual_touched: RegSet,
) -> CallClobbers {
    CallClobbers { clobbered: actual_touched.minus(plan.preserved_regs) }
}

/// Computes the minimal set of registers a callee's prologue/epilogue must save and restore to
/// uphold `plan.preserved_regs` (specifically minimizing spill overhead in `ColdPreserveAll`).
#[inline]
pub fn cold_preserve_save_set<A: Allocator>(
    plan: &CustomCallPlan<A>,
    actual_touched: RegSet,
) -> RegSet {
    RegSet(actual_touched.0 & plan.preserved_regs.0)
}

/// Counts scalar/pointer GPR arguments and FP/NEON arguments in `view`'s own signature
/// (excluding `ParamKind::Sret`, which occupies dedicated `x8`).
pub fn count_sig_arg_classes(view: View<'_>) -> (u8, u8) {
    let (params, _) = sig_slices(view, view.head.sig);
    let mut gpr = 0u8;
    let mut fp = 0u8;
    for param in params {
        let kind = ParamKind::from_byte(param.kind).unwrap_or(ParamKind::Value);
        if kind == ParamKind::Sret {
            continue;
        }
        let ty = Ty(param.ty);
        if kind == ParamKind::Value && is_fp_or_vec(ty) {
            fp = fp.saturating_add(1);
        } else {
            gpr = gpr.saturating_add(1);
        }
    }
    (gpr, fp)
}

/// Returns `true` when `input` is eligible for wide-register [`CallConvKind::InternalFast`]
/// argument promotion (`> 8` GPR or `> 8` FP/NEON scalar args passed via
/// [`INTERNAL_FAST_GPR_ARGS`] / [`INTERNAL_FAST_FP_ARGS`]).
///
/// Strictly refuses exported, address-taken, cold (`ColdPreserveAll`), variadic, foreign-ABI
/// (`Conv != Conv::C`), and `ParamKind::Byval` functions.
pub fn is_internal_fast_eligible(input: &AbiFnInput<'_>, gpr_args: u8, fp_args: u8) -> bool {
    if input.is_exported || input.address_taken {
        return false;
    }
    if is_cold_candidate(input) {
        return false;
    }
    if requires_fixed_c_abi(input.view) {
        return false;
    }
    gpr_args > C_ABI_MAX_GPR_ARGS || fp_args > C_ABI_MAX_FP_ARGS
}

/// Plans whole-module AArch64 calling conventions, interprocedural register-avoidance ordering,
/// global SCC pointer pinning, and cold `preserve_all` preservation sets.
pub fn plan_module_abi<A: Allocator + Copy>(
    fns: &[AbiFnInput<'_>],
    opts: ZeroAbiOpts,
    alloc: A,
) -> ModuleAbiPlan<A> {
    let n = fns.len();
    if n == 0 {
        return ModuleAbiPlan {
            plans: Vec::new_in(alloc),
            stats: ZeroAbiStats::default(),
        };
    }

    let max_fast_gpr = opts.max_fast_gpr_args.min(MAX_HW_FAST_GPR_ARGS);
    let max_fast_fp = opts.max_fast_fp_args.min(MAX_HW_FAST_FP_ARGS);
    let max_fast_ret = opts.max_fast_returns.min(MAX_HW_FAST_RETS);

    // Phase 1: Classify baseline convention per function (Rule 1 ABI safety + Rule 2 ColdPreserveAll).
    let mut base_convs: Vec<CallConvKind, A> = Vec::with_capacity_in(n, alloc);
    for input in fns {
        let conv = if input.is_exported
            || input.address_taken
            || requires_fixed_c_abi(input.view)
        {
            CallConvKind::CAbi
        } else if opts.enable_cold_preserve_all && is_cold_candidate(input) {
            CallConvKind::ColdPreserveAll
        } else {
            CallConvKind::InternalFast
        };
        base_convs.push(conv);
    }

    // Phase 2: Build deterministic CSR forward call graph, reverse caller lists, and Tarjan SCCs.
    let mut adj_offsets: Vec<u32, A> = Vec::with_capacity_in(n + 1, alloc);
    let mut adj: Vec<u32, A> = Vec::with_capacity_in(n.saturating_mul(2), alloc);
    let mut caller_counts: Vec<usize, A> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        caller_counts.push(0);
    }
    adj_offsets.push(0);

    for input in fns {
        let edge_start = adj.len();
        for &callee in input.direct_callees {
            if callee < n
                && let Ok(callee_u32) = u32::try_from(callee)
            {
                adj.push(callee_u32);
            }
        }
        dedup_sorted_tail(&mut adj, edge_start);
        for &callee_u32 in adj.get(edge_start..).unwrap_or(&[]) {
            if let Ok(cidx) = usize::try_from(callee_u32)
                && let Some(cnt) = caller_counts.get_mut(cidx)
            {
                *cnt += 1;
            }
        }
        let next_off = u32::try_from(adj.len()).unwrap_or(u32::MAX);
        adj_offsets.push(next_off);
    }

    // Reverse adjacency (callers of each callee) in CSR form.
    let mut rev_offsets: Vec<usize, A> = Vec::with_capacity_in(n + 1, alloc);
    let mut total_edges = 0usize;
    rev_offsets.push(0);
    for &cnt in caller_counts.iter() {
        total_edges += cnt;
        rev_offsets.push(total_edges);
    }
    let mut rev_adj: Vec<usize, A> = Vec::with_capacity_in(total_edges, alloc);
    for _ in 0..total_edges {
        rev_adj.push(0);
    }
    let mut rev_cursor: Vec<usize, A> = Vec::with_capacity_in(n, alloc);
    for i in 0..n {
        rev_cursor.push(rev_offsets.get(i).copied().unwrap_or(0));
    }
    for caller in 0..n {
        let start = adj_offsets.get(caller).copied().and_then(|v| usize::try_from(v).ok()).unwrap_or(0);
        let end = adj_offsets.get(caller + 1).copied().and_then(|v| usize::try_from(v).ok()).unwrap_or(start);
        for &callee_u32 in adj.get(start..end).unwrap_or(&[]) {
            if let Ok(callee) = usize::try_from(callee_u32)
                && let Some(pos) = rev_cursor.get_mut(callee)
            {
                let write_at = *pos;
                *pos += 1;
                if let Some(slot) = rev_adj.get_mut(write_at) {
                    *slot = caller;
                }
            }
        }
    }

    // Compute Tarjan SCC ID per function (deterministic reverse topological order).
    let sccs = tarjan_sccs(n, &adj_offsets, &adj, alloc);
    let mut fn_scc: Vec<usize, A> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        fn_scc.push(0);
    }
    let scc_count = sccs.offsets.len().saturating_sub(1);
    for scc_id in 0..scc_count {
        let start = sccs.offsets.get(scc_id).copied().and_then(|v| usize::try_from(v).ok()).unwrap_or(0);
        let end = sccs.offsets.get(scc_id + 1).copied().and_then(|v| usize::try_from(v).ok()).unwrap_or(start);
        for &node_u32 in sccs.nodes.get(start..end).unwrap_or(&[]) {
            if let Ok(node) = usize::try_from(node_u32)
                && let Some(dst) = fn_scc.get_mut(node)
            {
                *dst = scc_id;
            }
        }
    }

    // Phase 3: Rule 5 — Global SCC / call-chain pass-through pointer pinning.
    let mut pinned_per_fn: Vec<Vec<PinnedReg, A>, A> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        pinned_per_fn.push(Vec::new_in(alloc));
    }
    let mut stats = ZeroAbiStats::default();
    if opts.enable_scc_pinning {
        stats.pinned_scc_params = compute_scc_pinned_params(
            fns,
            &base_convs,
            &rev_offsets,
            &rev_adj,
            &fn_scc,
            &mut pinned_per_fn,
            alloc,
        );
    }

    // Phase 4: Rule 4 — Interprocedural register-avoidance ordering per callee.
    let mut scratch_gprs: Vec<[u8; 16], A> = Vec::with_capacity_in(n, alloc);
    let mut scratch_fps: Vec<[u8; 24], A> = Vec::with_capacity_in(n, alloc);
    for callee in 0..n {
        let rev_start = rev_offsets.get(callee).copied().unwrap_or(0);
        let rev_end = rev_offsets.get(callee + 1).copied().unwrap_or(rev_start);
        let callers = rev_adj.get(rev_start..rev_end).unwrap_or(&[]);
        let (gpr_order, avoided_gpr) = compute_gpr_avoidance_order(fns, callers);
        let (fp_order, avoided_fp) = compute_fp_avoidance_order(fns, callers);
        stats.avoided_clobber_regs += avoided_gpr + avoided_fp;
        scratch_gprs.push(gpr_order);
        scratch_fps.push(fp_order);
    }

    // Phase 5: Rule 1/2/3 — Assemble parameter slots, return slots, preserved sets, and stats.
    let mut plans: Vec<CustomCallPlan<A>, A> = Vec::with_capacity_in(n, alloc);
    for idx in 0..n {
        let Some(input) = fns.get(idx) else {
            break;
        };
        let base_conv = base_convs.get(idx).copied().unwrap_or(CallConvKind::CAbi);
        let pinned_list = if let Some(slot) = pinned_per_fn.get_mut(idx) {
            core::mem::replace(slot, Vec::new_in(alloc))
        } else {
            Vec::new_in(alloc)
        };
        let conv = if base_conv == CallConvKind::InternalFast && !pinned_list.is_empty() {
            CallConvKind::PinnedScc
        } else {
            base_conv
        };

        let (gpr_args, fp_args) = count_sig_arg_classes(input.view);
        let has_internal_caller = caller_counts.get(idx).copied().unwrap_or(0) > 0;
        match conv {
            CallConvKind::CAbi => {}
            CallConvKind::InternalFast | CallConvKind::PinnedScc => {
                stats.internal_fast_fns += 1;
                if has_internal_caller && is_internal_fast_eligible(input, gpr_args, fp_args) {
                    stats.wide_arg_fast_fns += 1;
                }
            }
            CallConvKind::ColdPreserveAll => {
                stats.cold_preserve_all_fns += 1;
            }
        }

        let (param_slots, return_slots, stack_saved, sret_saved) = assign_slots(
            input.view,
            conv,
            &pinned_list,
            max_fast_gpr,
            max_fast_fp,
            max_fast_ret,
            alloc,
        );
        stats.stack_args_eliminated += stack_saved;
        stats.sret_eliminated += sret_saved;

        let preserved_regs = compute_preserved_regs(conv, &return_slots);
        let scratch_gpr_order = scratch_gprs.get(idx).copied().unwrap_or(DEFAULT_SCRATCH_GPR_ORDER);
        let scratch_fp_order = scratch_fps.get(idx).copied().unwrap_or(DEFAULT_SCRATCH_FP_ORDER);

        plans.push(CustomCallPlan {
            conv,
            param_slots,
            return_slots,
            scratch_gpr_order,
            scratch_fp_order,
            preserved_regs,
            pinned_globals: pinned_list,
        });
    }

    ModuleAbiPlan { plans, stats }
}

/// Deduplicates `adj[edge_start..]` in ascending order in place.
fn dedup_sorted_tail<A: Allocator>(adj: &mut Vec<u32, A>, edge_start: usize) {
    if let Some(slice) = adj.get_mut(edge_start..) {
        slice.sort_unstable();
    }
    let mut write = edge_start;
    let mut prev: Option<u32> = None;
    for read in edge_start..adj.len() {
        let Some(&val) = adj.get(read) else {
            break;
        };
        if prev != Some(val) {
            if let Some(dst) = adj.get_mut(write) {
                *dst = val;
            }
            write += 1;
            prev = Some(val);
        }
    }
    adj.truncate(write);
}

/// Returns `true` when an internal function qualifies for [`CallConvKind::ColdPreserveAll`].
fn is_cold_candidate(input: &AbiFnInput<'_>) -> bool {
    if input.is_cold {
        return true;
    }
    let attrs = input.view.head.attrs;
    if (attrs & FnAttrs::COLD) != 0 {
        return true;
    }
    if (attrs & FnAttrs::NORETURN) != 0 && body_ends_in_trap_or_unwind(input.view) {
        return true;
    }
    false
}

/// Returns `true` if `view` has no normal return path and terminates at least one block with
/// `Op::Trap` or `Op::Resume` (panic / abort / unwind sink).
fn body_ends_in_trap_or_unwind(view: View<'_>) -> bool {
    let block_count = view.block_starts.len();
    if block_count == 0 || view.ops.is_empty() {
        return false;
    }
    let mut saw_trap_or_unwind = false;
    for b in 0..block_count {
        let start = view
            .block_starts
            .get(b)
            .copied()
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(view.ops.len());
        let end = view
            .block_starts
            .get(b + 1)
            .copied()
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(view.ops.len());
        if end <= start || end > view.ops.len() {
            continue;
        }
        let last = end - 1;
        let Some(op) = view.ops.get(last).copied().and_then(Op::from_byte) else {
            continue;
        };
        if op == Op::Return || op == Op::TailCall || op == Op::TailCallIndirect {
            return false;
        }
        if op == Op::Trap || op == Op::Resume {
            saw_trap_or_unwind = true;
        }
    }
    saw_trap_or_unwind
}

/// Safely reads the [`SigRec`] for `sig_word` in `view`.
fn sig_rec<'a>(view: View<'a>, sig_word: u32) -> Option<&'a SigRec> {
    let sig_idx = usize::try_from(sig_word).ok()?;
    view.sigs.get(sig_idx)
}

/// Returns `true` when `view`'s own signature requires strict C/foreign ABI handling:
/// variadic `...`, explicit non-default calling convention (`Conv != Conv::C`), or
/// `ParamKind::Byval` stack-copied C aggregates.
fn requires_fixed_c_abi(view: View<'_>) -> bool {
    let Some(rec) = sig_rec(view, view.head.sig) else {
        return true;
    };
    if (rec.bits & SigRec::VARIADIC) != 0 {
        return true;
    }
    if Conv::from_byte(rec.conv) != Some(Conv::C) {
        return true;
    }
    let (params, _) = sig_slices(view, view.head.sig);
    for param in params {
        if ParamKind::from_byte(param.kind) == Some(ParamKind::Byval) {
            return true;
        }
    }
    false
}

/// Safely reads `(params, rets)` slices for signature `sig_word` in `view`.
fn sig_slices<'a>(view: View<'a>, sig_word: u32) -> (&'a [ParamRec], &'a [ParamRec]) {
    let Some(rec) = sig_rec(view, sig_word) else {
        return (&[], &[]);
    };
    let Some(first) = usize::try_from(rec.first).ok() else {
        return (&[], &[]);
    };
    let n_params = usize::from(rec.params);
    let n_rets = usize::from(rec.rets);
    let Some(params_end) = first.checked_add(n_params) else {
        return (&[], &[]);
    };
    let Some(rets_end) = params_end.checked_add(n_rets) else {
        return (&[], &[]);
    };
    let params = view.sig_params.get(first..params_end).unwrap_or(&[]);
    let rets = view.sig_params.get(params_end..rets_end).unwrap_or(&[]);
    (params, rets)
}

/// Safely reads the `ParamFactsRec` attached to `param`, if present.
fn param_facts<'a>(view: View<'a>, param: &ParamRec) -> Option<&'a ParamFactsRec> {
    let row = param.facts.checked_sub(1)?;
    let idx = usize::try_from(row).ok()?;
    view.param_facts.get(idx)
}

/// Safely reads compound type member `u16` types for `ty` in `view`.
fn compound_members<'a>(view: View<'a>, ty: Ty) -> &'a [u16] {
    let Some(row) = ty.compound_row() else {
        return &[];
    };
    let Some(row_idx) = usize::try_from(row).ok() else {
        return &[];
    };
    let Some(rec) = view.compounds.get(row_idx) else {
        return &[];
    };
    let Some(first) = usize::try_from(rec.first).ok() else {
        return &[];
    };
    let Some(len) = usize::try_from(rec.len).ok() else {
        return &[];
    };
    let Some(end) = first.checked_add(len) else {
        return &[];
    };
    view.compound_tys.get(first..end).unwrap_or(&[])
}

/// Returns `true` if `ty` lives in the AArch64 FP/NEON register bank (`v0..v31`).
#[inline]
const fn is_fp_or_vec(ty: Ty) -> bool {
    ty.is_float() || ty.is_vector()
}

/// Returns the byte size and alignment step (8 or 16) for a spilled argument of type `ty`.
#[inline]
const fn stack_arg_step(ty: Ty) -> u32 {
    if ty.is_vector() || ty.bits(64) > 64 {
        16
    } else {
        8
    }
}

/// Aligns `off` upward to a power-of-two `align` without overflowing.
#[inline]
const fn align_up(off: u32, align: u32) -> u32 {
    if align <= 1 {
        return off;
    }
    let mask = align - 1;
    off.saturating_add(mask) & !mask
}

/// Looks up whether `param_index` is pinned to a fixed GPR.
fn find_pinned_reg(pinned: &[PinnedReg], param_index: u16) -> Option<u8> {
    for entry in pinned {
        if entry.param_index == param_index {
            return Some(entry.reg);
        }
    }
    None
}

/// Assigns parameter and return [`ArgSlot`]s for `view` under `conv`, returning
/// `(param_slots, return_slots, stack_args_eliminated, sret_eliminated)`.
fn assign_slots<A: Allocator + Copy>(
    view: View<'_>,
    conv: CallConvKind,
    pinned: &[PinnedReg],
    max_fast_gpr: u8,
    max_fast_fp: u8,
    max_fast_ret: u8,
    alloc: A,
) -> (Vec<ArgSlot, A>, Vec<ArgSlot, A>, usize, usize) {
    let (params, rets) = sig_slices(view, view.head.sig);

    let (gpr_cap, fp_cap, ret_gpr_cap, ret_fp_cap) = match conv {
        CallConvKind::CAbi => (
            C_ABI_MAX_GPR_ARGS,
            C_ABI_MAX_FP_ARGS,
            C_ABI_MAX_GPR_RETS,
            C_ABI_MAX_FP_RETS,
        ),
        CallConvKind::InternalFast | CallConvKind::ColdPreserveAll | CallConvKind::PinnedScc => {
            (max_fast_gpr, max_fast_fp, max_fast_ret, max_fast_ret)
        }
    };

    // Baseline AAPCS64 stack-arg count for exact `stack_args_eliminated` accounting.
    let c_abi_stack_args = count_c_abi_stack_args(params);

    // Check if an explicit `ParamKind::Sret` parameter can be promoted to multi-register returns.
    let promoted_sret_words = if conv != CallConvKind::CAbi && rets.is_empty() {
        find_promotable_sret_words(view, params, ret_gpr_cap)
    } else {
        None
    };

    let mut param_slots: Vec<ArgSlot, A> = Vec::with_capacity_in(params.len(), alloc);
    let mut next_gpr = 0u8;
    let mut next_fp = 0u8;
    let mut stack_off = 0u32;
    let mut custom_stack_args = 0usize;

    for (p_idx, param) in params.iter().enumerate() {
        let kind = ParamKind::from_byte(param.kind).unwrap_or(ParamKind::Value);
        if kind == ParamKind::Sret {
            if promoted_sret_words.is_some() {
                // Promoted to direct return registers; keep slot marker out of arg GPRs.
                continue;
            }
            param_slots.push(ArgSlot::Gpr(A64_SRET_REG));
            continue;
        }

        if let Ok(p_u16) = u16::try_from(p_idx)
            && let Some(pinned_reg) = find_pinned_reg(pinned, p_u16)
        {
            param_slots.push(ArgSlot::Gpr(pinned_reg));
            continue;
        }

        let ty = Ty(param.ty);
        let use_fp = kind == ParamKind::Value && is_fp_or_vec(ty);
        if use_fp {
            let reg = if next_fp < fp_cap {
                INTERNAL_FAST_FP_ARGS.get(usize::from(next_fp)).copied()
            } else {
                None
            };
            if let Some(fp_reg) = reg {
                param_slots.push(ArgSlot::Fp(fp_reg));
                next_fp += 1;
            } else {
                let step = stack_arg_step(ty);
                stack_off = align_up(stack_off, step);
                param_slots.push(ArgSlot::StackOff(stack_off));
                stack_off = stack_off.saturating_add(step);
                custom_stack_args += 1;
            }
        } else {
            let reg = if next_gpr < gpr_cap {
                INTERNAL_FAST_GPR_ARGS.get(usize::from(next_gpr)).copied()
            } else {
                None
            };
            if let Some(gpr_reg) = reg {
                param_slots.push(ArgSlot::Gpr(gpr_reg));
                next_gpr += 1;
            } else {
                let step = stack_arg_step(ty);
                stack_off = align_up(stack_off, step);
                param_slots.push(ArgSlot::StackOff(stack_off));
                stack_off = stack_off.saturating_add(step);
                custom_stack_args += 1;
            }
        }
    }

    let stack_saved = if conv == CallConvKind::CAbi {
        0
    } else {
        c_abi_stack_args.saturating_sub(custom_stack_args)
    };

    // Assign return slots (expanding compound return types into their scalar/vector members).
    let mut return_slots: Vec<ArgSlot, A> = Vec::with_capacity_in(rets.len().max(4), alloc);
    let mut sret_eliminated = 0usize;

    if let Some(words) = promoted_sret_words {
        for r in 0..words {
            return_slots.push(ArgSlot::Gpr(r));
        }
        sret_eliminated = 1;
    } else {
        let mut ret_gpr = 0u8;
        let mut ret_fp = 0u8;
        let mut ret_stack = 0u32;
        let mut total_ret_gprs = 0u8;
        let mut total_ret_fps = 0u8;
        let mut custom_ret_spilled = false;

        for ret in rets {
            let ty = Ty(ret.ty);
            if ty.form() == Form::Compound {
                for &member_bits in compound_members(view, ty) {
                    push_return_slot(
                        Ty(member_bits),
                        ret_gpr_cap,
                        ret_fp_cap,
                        &mut ret_gpr,
                        &mut ret_fp,
                        &mut ret_stack,
                        &mut total_ret_gprs,
                        &mut total_ret_fps,
                        &mut custom_ret_spilled,
                        &mut return_slots,
                    );
                }
            } else {
                push_return_slot(
                    ty,
                    ret_gpr_cap,
                    ret_fp_cap,
                    &mut ret_gpr,
                    &mut ret_fp,
                    &mut ret_stack,
                    &mut total_ret_gprs,
                    &mut total_ret_fps,
                    &mut custom_ret_spilled,
                    &mut return_slots,
                );
            }
        }

        let c_abi_needed_sret =
            total_ret_gprs > C_ABI_MAX_GPR_RETS || total_ret_fps > C_ABI_MAX_FP_RETS;
        if conv != CallConvKind::CAbi && c_abi_needed_sret && !custom_ret_spilled {
            sret_eliminated = 1;
        }
    }

    (param_slots, return_slots, stack_saved, sret_eliminated)
}

/// Counts how many parameters in `params` spill to the stack under AAPCS64 C ABI.
fn count_c_abi_stack_args(params: &[ParamRec]) -> usize {
    let mut gpr = 0u8;
    let mut fp = 0u8;
    let mut spilled = 0usize;
    for param in params {
        let kind = ParamKind::from_byte(param.kind).unwrap_or(ParamKind::Value);
        if kind == ParamKind::Sret {
            continue;
        }
        let ty = Ty(param.ty);
        if kind == ParamKind::Value && is_fp_or_vec(ty) {
            if fp < C_ABI_MAX_FP_ARGS {
                fp += 1;
            } else {
                spilled += 1;
            }
        } else if gpr < C_ABI_MAX_GPR_ARGS {
            gpr += 1;
        } else {
            spilled += 1;
        }
    }
    spilled
}

/// If `params` contains an `Sret` parameter whose pointee size is known and fits within
/// `ret_gpr_cap` 8-byte GPR return registers, returns the number of return registers.
fn find_promotable_sret_words(view: View<'_>, params: &[ParamRec], ret_gpr_cap: u8) -> Option<u8> {
    for param in params {
        if ParamKind::from_byte(param.kind) != Some(ParamKind::Sret) {
            continue;
        }
        let facts = param_facts(view, param)?;
        if facts.size == 0 {
            return None;
        }
        let words = facts.size.div_ceil(8);
        let words_u8 = u8::try_from(words).ok()?;
        if words_u8 > C_ABI_MAX_GPR_RETS && words_u8 <= ret_gpr_cap {
            return Some(words_u8);
        }
        return None;
    }
    None
}

/// Appends one scalar/vector return value's [`ArgSlot`] and updates return counters.
fn push_return_slot<A: Allocator>(
    ty: Ty,
    ret_gpr_cap: u8,
    ret_fp_cap: u8,
    ret_gpr: &mut u8,
    ret_fp: &mut u8,
    ret_stack: &mut u32,
    total_ret_gprs: &mut u8,
    total_ret_fps: &mut u8,
    spilled: &mut bool,
    out: &mut Vec<ArgSlot, A>,
) {
    if is_fp_or_vec(ty) {
        *total_ret_fps = total_ret_fps.saturating_add(1);
        if *ret_fp < ret_fp_cap {
            out.push(ArgSlot::Fp(*ret_fp));
            *ret_fp += 1;
        } else {
            let step = stack_arg_step(ty);
            *ret_stack = align_up(*ret_stack, step);
            out.push(ArgSlot::StackOff(*ret_stack));
            *ret_stack = ret_stack.saturating_add(step);
            *spilled = true;
        }
    } else {
        *total_ret_gprs = total_ret_gprs.saturating_add(1);
        if *ret_gpr < ret_gpr_cap {
            out.push(ArgSlot::Gpr(*ret_gpr));
            *ret_gpr += 1;
        } else {
            let step = stack_arg_step(ty);
            *ret_stack = align_up(*ret_stack, step);
            out.push(ArgSlot::StackOff(*ret_stack));
            *ret_stack = ret_stack.saturating_add(step);
            *spilled = true;
        }
    }
}

/// Computes `preserved_regs` for `conv`, stripping any register carrying a return value.
fn compute_preserved_regs(conv: CallConvKind, return_slots: &[ArgSlot]) -> RegSet {
    let mut preserved = match conv {
        CallConvKind::ColdPreserveAll => COLD_PRESERVE_ALL_BASE,
        CallConvKind::CAbi | CallConvKind::InternalFast | CallConvKind::PinnedScc => {
            C_ABI_PRESERVED
        }
    };
    for &slot in return_slots {
        match slot {
            ArgSlot::Gpr(r) if r < 32 => {
                preserved = preserved.without(Reg(r));
            }
            ArgSlot::Fp(r) if r < 32 => {
                preserved = preserved.without(Reg(32 + r));
            }
            ArgSlot::Gpr(_) | ArgSlot::Fp(_) | ArgSlot::StackOff(_) => {}
        }
    }
    preserved
}

/// Aggregates `caller_live_pressure_gprs` across `callers` and returns a stably sorted
/// `scratch_gpr_order` with caller-untouched registers first and caller-hot registers last,
/// along with the count of hot GPRs demoted behind untouched registers.
fn compute_gpr_avoidance_order(fns: &[AbiFnInput<'_>], callers: &[usize]) -> ([u8; 16], usize) {
    let mut pressure = [0u32; 16];
    for &caller_idx in callers {
        let Some(caller) = fns.get(caller_idx) else {
            continue;
        };
        let live = caller.caller_live_pressure_gprs;
        if live.is_empty() {
            continue;
        }
        for (slot_idx, &reg) in DEFAULT_SCRATCH_GPR_ORDER.iter().enumerate() {
            if live.contains(Reg(reg))
                && let Some(cnt) = pressure.get_mut(slot_idx)
            {
                *cnt = cnt.saturating_add(1);
            }
        }
    }

    let mut indices: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    // Deterministic stable insertion sort by ascending caller live pressure.
    for i in 1..16 {
        let key = indices.get(i).copied().unwrap_or(0);
        let key_p = pressure.get(key).copied().unwrap_or(0);
        let mut j = i;
        while j > 0 {
            let prev = indices.get(j - 1).copied().unwrap_or(0);
            let prev_p = pressure.get(prev).copied().unwrap_or(0);
            if prev_p > key_p {
                if let Some(dst) = indices.get_mut(j) {
                    *dst = prev;
                }
                j -= 1;
            } else {
                break;
            }
        }
        if let Some(dst) = indices.get_mut(j) {
            *dst = key;
        }
    }

    let mut order = DEFAULT_SCRATCH_GPR_ORDER;
    let mut has_untouched = false;
    let mut hot_count = 0usize;
    for (out_idx, &src_idx) in indices.iter().enumerate() {
        let reg = DEFAULT_SCRATCH_GPR_ORDER.get(src_idx).copied().unwrap_or(0);
        if let Some(dst) = order.get_mut(out_idx) {
            *dst = reg;
        }
        let p = pressure.get(src_idx).copied().unwrap_or(0);
        if p == 0 {
            has_untouched = true;
        } else {
            hot_count += 1;
        }
    }
    let avoided = if has_untouched { hot_count } else { 0 };
    (order, avoided)
}

/// Aggregates `caller_live_pressure_fps` across `callers` and returns a stably sorted
/// `scratch_fp_order` with caller-untouched NEON registers first and caller-hot registers last,
/// along with the count of hot FP registers demoted behind untouched registers.
fn compute_fp_avoidance_order(fns: &[AbiFnInput<'_>], callers: &[usize]) -> ([u8; 24], usize) {
    let mut pressure = [0u32; 24];
    for &caller_idx in callers {
        let Some(caller) = fns.get(caller_idx) else {
            continue;
        };
        let live = caller.caller_live_pressure_fps;
        if live.is_empty() {
            continue;
        }
        for (slot_idx, &reg) in DEFAULT_SCRATCH_FP_ORDER.iter().enumerate() {
            if live.contains(Reg(32 + reg))
                && let Some(cnt) = pressure.get_mut(slot_idx)
            {
                *cnt = cnt.saturating_add(1);
            }
        }
    }

    let mut indices: [usize; 24] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
    ];
    for i in 1..24 {
        let key = indices.get(i).copied().unwrap_or(0);
        let key_p = pressure.get(key).copied().unwrap_or(0);
        let mut j = i;
        while j > 0 {
            let prev = indices.get(j - 1).copied().unwrap_or(0);
            let prev_p = pressure.get(prev).copied().unwrap_or(0);
            if prev_p > key_p {
                if let Some(dst) = indices.get_mut(j) {
                    *dst = prev;
                }
                j -= 1;
            } else {
                break;
            }
        }
        if let Some(dst) = indices.get_mut(j) {
            *dst = key;
        }
    }

    let mut order = DEFAULT_SCRATCH_FP_ORDER;
    let mut has_untouched = false;
    let mut hot_count = 0usize;
    for (out_idx, &src_idx) in indices.iter().enumerate() {
        let reg = DEFAULT_SCRATCH_FP_ORDER.get(src_idx).copied().unwrap_or(0);
        if let Some(dst) = order.get_mut(out_idx) {
            *dst = reg;
        }
        let p = pressure.get(src_idx).copied().unwrap_or(0);
        if p == 0 {
            has_untouched = true;
        } else {
            hot_count += 1;
        }
    }
    let avoided = if has_untouched { hot_count } else { 0 };
    (order, avoided)
}

/// Decoded direct call site within a caller's LIR `View`.
#[derive(Clone, Copy)]
struct DirectCallSite<'a> {
    place: u32,
    args: &'a [u32],
}

/// Extracts `(entry_param_start, entry_param_end)` instruction index range of `Block(0)`.
fn entry_param_range(view: View<'_>) -> (usize, usize) {
    let Some(&start_u32) = view.block_starts.first() else {
        return (0, 0);
    };
    let Ok(start) = usize::try_from(start_u32) else {
        return (0, 0);
    };
    let end = view
        .block_starts
        .get(1)
        .copied()
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(view.ops.len())
        .min(view.ops.len());
    let mut cur = start;
    while cur < end {
        if view.ops.get(cur).copied().and_then(Op::from_byte) == Some(Op::Param) {
            cur += 1;
        } else {
            break;
        }
    }
    (start, cur)
}

/// Extracts a direct `Op::Call`, `Op::Invoke`, or `Op::TailCall` instruction's target `place`
/// and argument value slice without unchecked indexing.
fn decode_direct_call<'a>(view: View<'a>, inst: usize) -> Option<DirectCallSite<'a>> {
    let op = view.ops.get(inst).copied().and_then(Op::from_byte)?;
    let args_off = if op == Op::Call || op == Op::TailCall {
        2usize
    } else if op == Op::Invoke {
        4usize
    } else {
        return None;
    };
    let base = view.data.get(inst).copied().and_then(|v| usize::try_from(v).ok())?;
    let ref_word = view.extra.get(base).copied()?;
    let ref_idx = usize::try_from(ref_word).ok()?;
    let ref_rec = view.refs.get(ref_idx)?;
    if RefKind::from_word(ref_rec.kind) != Some(RefKind::Func) {
        return None;
    }
    let argc_pos = base.checked_add(args_off)?;
    let argc_u32 = view.extra.get(argc_pos).copied()?;
    let argc = usize::try_from(argc_u32).ok()?;
    let args_start = argc_pos.checked_add(1)?;
    let args_end = args_start.checked_add(argc)?;
    let args = view.extra.get(args_start..args_end)?;
    Some(DirectCallSite { place: ref_rec.place, args })
}

/// Checks if `param` is a plain `Ty::PTR` value parameter eligible for SCC register pinning.
#[inline]
fn is_pinnable_ptr_param(param: &ParamRec) -> bool {
    ParamKind::from_byte(param.kind) == Some(ParamKind::Value) && Ty(param.ty) == Ty::PTR
}

/// Checks whether a call site's argument types match `callee_params` (used when `RefRec.place`
/// does not directly encode the module function index).
fn call_matches_callee_sig(caller_view: View<'_>, args: &[u32], callee_params: &[ParamRec]) -> bool {
    if args.len() != callee_params.len() {
        return false;
    }
    for (&arg_word, expected) in args.iter().zip(callee_params.iter()) {
        let Some(arg_idx) = usize::try_from(arg_word).ok() else {
            return false;
        };
        let Some(&arg_ty) = caller_view.tys.get(arg_idx) else {
            return false;
        };
        if arg_ty != expected.ty {
            return false;
        }
    }
    true
}

/// If EVERY direct call site from `caller` to `callee` passes the SAME entry `Op::Param(q)` (of
/// type `Ty::PTR`) of `caller` as argument `callee_param_idx`, returns `Some(q)`.
fn find_passthrough_source_param(
    caller_input: &AbiFnInput<'_>,
    callee_idx: usize,
    callee_params: &[ParamRec],
    callee_param_idx: usize,
) -> Option<u16> {
    let caller_view = caller_input.view;
    let (caller_params, _) = sig_slices(caller_view, caller_view.head.sig);
    let (ep_start, ep_end) = entry_param_range(caller_view);
    let callee_place = u32::try_from(callee_idx).ok();

    // First determine whether all direct call sites in `caller_view` use `place` directly as a
    // module function index present in `caller_input.direct_callees`.
    let mut saw_any_direct = false;
    let mut all_places_match_callees = true;
    for inst in 0..caller_view.ops.len() {
        let Some(site) = decode_direct_call(caller_view, inst) else {
            continue;
        };
        saw_any_direct = true;
        let place_in_callees = usize::try_from(site.place)
            .ok()
            .is_some_and(|p| caller_input.direct_callees.contains(&p));
        if !place_in_callees {
            all_places_match_callees = false;
            break;
        }
    }
    if !saw_any_direct {
        return None;
    }

    let mut matched_sites = 0usize;
    let mut source_param: Option<u16> = None;
    for inst in 0..caller_view.ops.len() {
        let Some(site) = decode_direct_call(caller_view, inst) else {
            continue;
        };
        if all_places_match_callees {
            if Some(site.place) != callee_place {
                continue;
            }
        } else if !call_matches_callee_sig(caller_view, site.args, callee_params) {
            continue;
        }
        matched_sites += 1;
        let arg_word = site.args.get(callee_param_idx).copied()?;
        let arg_idx = usize::try_from(arg_word).ok()?;
        if arg_idx < ep_start || arg_idx >= ep_end {
            return None;
        }
        let caller_param_idx = arg_idx - ep_start;
        let caller_param = caller_params.get(caller_param_idx)?;
        if !is_pinnable_ptr_param(caller_param) {
            return None;
        }
        let caller_param_u16 = u16::try_from(caller_param_idx).ok()?;
        if let Some(prev) = source_param {
            if prev != caller_param_u16 {
                return None;
            }
        } else {
            source_param = Some(caller_param_u16);
        }
    }

    if matched_sites > 0 {
        source_param
    } else {
        None
    }
}

/// One indexed `(fn_idx, param_index)` candidate node for SCC register pinning.
#[derive(Clone, Copy)]
struct ParamNode {
    fn_idx: usize,
    param_index: u16,
}

/// Iterative union-find over parameter candidate nodes (zero recursion).
fn uf_find(parent: &mut [usize], mut x: usize) -> usize {
    let mut root = x;
    for _ in 0..parent.len() {
        let p = parent.get(root).copied().unwrap_or(root);
        if p == root {
            break;
        }
        root = p;
    }
    for _ in 0..parent.len() {
        let p = parent.get(x).copied().unwrap_or(root);
        if p == root {
            break;
        }
        if let Some(slot) = parent.get_mut(x) {
            *slot = root;
        }
        x = p;
    }
    root
}

/// Merges two union-find sets deterministically by smaller root index.
fn uf_union(parent: &mut [usize], a: usize, b: usize) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra == rb {
        return;
    }
    if ra < rb {
        if let Some(slot) = parent.get_mut(rb) {
            *slot = ra;
        }
    } else if let Some(slot) = parent.get_mut(ra) {
        *slot = rb;
    }
}

/// Rule 5: Detects invariant `Ty::PTR` parameters passed unchanged across internal SCCs and call
/// chains and assigns dedicated callee-preserved registers (`x28`, `x27`, `x26`, `x25`).
///
/// Returns the total number of `(function, param)` pairs pinned.
fn compute_scc_pinned_params<A: Allocator + Copy>(
    fns: &[AbiFnInput<'_>],
    base_convs: &[CallConvKind],
    rev_offsets: &[usize],
    rev_adj: &[usize],
    fn_scc: &[usize],
    pinned_per_fn: &mut [Vec<PinnedReg, A>],
    alloc: A,
) -> usize {
    let n = fns.len();
    let mut fn_node_base: Vec<usize, A> = Vec::with_capacity_in(n + 1, alloc);
    let mut nodes: Vec<ParamNode, A> = Vec::with_capacity_in(n * 2, alloc);
    fn_node_base.push(0);

    for (fn_idx, input) in fns.iter().enumerate() {
        let eligible = base_convs.get(fn_idx).copied() == Some(CallConvKind::InternalFast);
        if eligible {
            let (params, _) = sig_slices(input.view, input.view.head.sig);
            for (p_idx, param) in params.iter().enumerate() {
                if is_pinnable_ptr_param(param)
                    && let Ok(p_u16) = u16::try_from(p_idx)
                {
                    nodes.push(ParamNode { fn_idx, param_index: p_u16 });
                }
            }
        }
        fn_node_base.push(nodes.len());
    }

    let node_count = nodes.len();
    if node_count == 0 {
        return 0;
    }

    // Lookup `(fn_idx, param_index)` -> dense node ID.
    let find_node = |fn_idx: usize, param_index: u16| -> Option<usize> {
        let start = fn_node_base.get(fn_idx).copied()?;
        let end = fn_node_base.get(fn_idx + 1).copied()?;
        for id in start..end {
            if nodes.get(id).is_some_and(|nd| nd.param_index == param_index) {
                return Some(id);
            }
        }
        None
    };

    // Validate each candidate node: EVERY caller of `fn_idx` must pass a consistent entry
    // `Op::Param` of type `Ty::PTR` into `param_index`. Collect verified internal edges.
    let mut valid: Vec<bool, A> = Vec::with_capacity_in(node_count, alloc);
    let mut has_scc_or_chain_edge: Vec<bool, A> = Vec::with_capacity_in(node_count, alloc);
    for _ in 0..node_count {
        valid.push(true);
        has_scc_or_chain_edge.push(false);
    }

    let mut edges: Vec<(usize, usize, bool), A> = Vec::with_capacity_in(node_count * 2, alloc);

    for node_id in 0..node_count {
        let Some(node) = nodes.get(node_id).copied() else {
            break;
        };
        let Some(callee_input) = fns.get(node.fn_idx) else {
            continue;
        };
        let (callee_params, _) = sig_slices(callee_input.view, callee_input.view.head.sig);
        let rev_start = rev_offsets.get(node.fn_idx).copied().unwrap_or(0);
        let rev_end = rev_offsets.get(node.fn_idx + 1).copied().unwrap_or(rev_start);
        let callers = rev_adj.get(rev_start..rev_end).unwrap_or(&[]);

        let mut all_callers_ok = true;
        let edge_mark = edges.len();
        for &caller_idx in callers {
            let Some(caller_input) = fns.get(caller_idx) else {
                all_callers_ok = false;
                break;
            };
            let Some(src_param) = find_passthrough_source_param(
                caller_input,
                node.fn_idx,
                callee_params,
                usize::from(node.param_index),
            ) else {
                all_callers_ok = false;
                break;
            };
            if let Some(src_node) = find_node(caller_idx, src_param) {
                let same_scc = fn_scc.get(caller_idx) == fn_scc.get(node.fn_idx);
                edges.push((src_node, node_id, same_scc));
            }
        }

        if !all_callers_ok {
            edges.truncate(edge_mark);
            if let Some(v) = valid.get_mut(node_id) {
                *v = false;
            }
        }
    }

    // Union all valid internal pass-through edges into equivalence classes.
    let mut parent: Vec<usize, A> = Vec::with_capacity_in(node_count, alloc);
    let mut node_in_cycle: Vec<bool, A> = Vec::with_capacity_in(node_count, alloc);
    for i in 0..node_count {
        parent.push(i);
        node_in_cycle.push(false);
    }
    for &(src_node, dst_node, same_scc) in edges.iter() {
        let src_ok = valid.get(src_node).copied().unwrap_or(false);
        let dst_ok = valid.get(dst_node).copied().unwrap_or(false);
        if src_ok && dst_ok {
            uf_union(&mut parent, src_node, dst_node);
            if let Some(flag) = has_scc_or_chain_edge.get_mut(src_node) {
                *flag = true;
            }
            if let Some(flag) = has_scc_or_chain_edge.get_mut(dst_node) {
                *flag = true;
            }
            if same_scc {
                if let Some(flag) = node_in_cycle.get_mut(src_node) {
                    *flag = true;
                }
                if let Some(flag) = node_in_cycle.get_mut(dst_node) {
                    *flag = true;
                }
            }
        }
    }

    // Normalize component roots and check per-component eligibility (must have >= 1 internal
    // pass-through edge and must not map two different parameters of the same function to one reg).
    let mut comp_has_edge: Vec<bool, A> = Vec::with_capacity_in(node_count, alloc);
    let mut comp_in_cycle: Vec<bool, A> = Vec::with_capacity_in(node_count, alloc);
    let mut comp_conflict: Vec<bool, A> = Vec::with_capacity_in(node_count, alloc);
    let mut comp_roots: Vec<usize, A> = Vec::with_capacity_in(node_count, alloc);
    for _ in 0..node_count {
        comp_has_edge.push(false);
        comp_in_cycle.push(false);
        comp_conflict.push(false);
        comp_roots.push(0);
    }
    for node_id in 0..node_count {
        let root = uf_find(&mut parent, node_id);
        if let Some(dst) = comp_roots.get_mut(node_id) {
            *dst = root;
        }
        if has_scc_or_chain_edge.get(node_id).copied().unwrap_or(false)
            && let Some(flag) = comp_has_edge.get_mut(root)
        {
            *flag = true;
        }
        if node_in_cycle.get(node_id).copied().unwrap_or(false)
            && let Some(flag) = comp_in_cycle.get_mut(root)
        {
            *flag = true;
        }
    }

    // Detect if two distinct parameters of the same function land in the same component.
    for fn_idx in 0..n {
        let start = fn_node_base.get(fn_idx).copied().unwrap_or(0);
        let end = fn_node_base.get(fn_idx + 1).copied().unwrap_or(start);
        for a in start..end {
            let ra = comp_roots.get(a).copied().unwrap_or(a);
            for b in (a + 1)..end {
                let rb = comp_roots.get(b).copied().unwrap_or(b);
                if ra == rb
                    && let Some(c) = comp_conflict.get_mut(ra)
                {
                    *c = true;
                }
            }
        }
    }

    // Assign pinned registers (`x28`, `x27`, `x26`, `x25`) deterministically, prioritizing cyclic
    // SCC components first and then acyclic pass-through call chains.
    let mut fn_used_mask: Vec<u8, A> = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        fn_used_mask.push(0);
    }

    let mut total_pinned = 0usize;
    for want_cycle in [true, false] {
        for root in 0..node_count {
            if comp_roots.get(root).copied() != Some(root) {
                continue;
            }
            if !comp_has_edge.get(root).copied().unwrap_or(false)
                || comp_conflict.get(root).copied().unwrap_or(false)
                || comp_in_cycle.get(root).copied().unwrap_or(false) != want_cycle
            {
                continue;
            }

            // Union of pinned-slot masks already consumed by any function in this component.
            let mut blocked_mask = 0u8;
            for node_id in 0..node_count {
                if comp_roots.get(node_id).copied() == Some(root)
                    && let Some(node) = nodes.get(node_id)
                    && let Some(&used) = fn_used_mask.get(node.fn_idx)
                {
                    blocked_mask |= used;
                }
            }

            // Choose first available register in `PINNED_SCC_GPRS`.
            let mut chosen: Option<(u8, u8)> = None;
            for (slot_bit, &reg) in PINNED_SCC_GPRS.iter().enumerate() {
                let bit = 1u8.checked_shl(u32::try_from(slot_bit).unwrap_or(8)).unwrap_or(0);
                if bit != 0 && (blocked_mask & bit) == 0 {
                    chosen = Some((bit, reg));
                    break;
                }
            }
            let Some((bit, reg)) = chosen else {
                continue;
            };

            for node_id in 0..node_count {
                if comp_roots.get(node_id).copied() == Some(root)
                    && let Some(node) = nodes.get(node_id)
                {
                    if let Some(used) = fn_used_mask.get_mut(node.fn_idx) {
                        *used |= bit;
                    }
                    if let Some(out) = pinned_per_fn.get_mut(node.fn_idx) {
                        out.push(PinnedReg { param_index: node.param_index, reg });
                        total_pinned += 1;
                    }
                }
            }
        }
    }

    for list in pinned_per_fn.iter_mut() {
        list.sort_unstable_by_key(|p| p.param_index);
    }

    total_pinned
}

#[cfg(test)]
mod tests {
    use blaze_lir::{
        Flags, Ty,
        builder::{Builder, Facts, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv, Ext, FnAttrs, Func, ParamKind, RefKind},
    };
    use blaze_own::{Alloc, Vec};
    use blaze_tier::{CallClobbers, Reg, RegSet};

    use super::{
        A64_SRET_REG, AbiFnInput, ArgSlot, C_ABI_PRESERVED, CallConvKind, INTERNAL_FAST_FP_ARGS,
        INTERNAL_FAST_GPR_ARGS, INTERNAL_FAST_MAX_GPR_RETS, PinnedReg, ZeroAbiOpts,
        cold_preserve_save_set, count_sig_arg_classes, is_internal_fast_eligible,
        plan_call_clobbers, plan_module_abi,
    };

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn build_i64_fn_with_sig(
        n_params: usize,
        n_rets: usize,
        conv: Conv,
        variadic: Variadic,
        attrs: FnAttrs,
    ) -> Func<Alloc<'static>> {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let mut params = Vec::with_capacity_in(n_params, alloc);
        for _ in 0..n_params {
            params.push(Param::of(Ty::I64));
        }
        let mut rets = Vec::with_capacity_in(n_rets, alloc);
        for _ in 0..n_rets {
            rets.push(Param::of(Ty::I64));
        }
        let sig = b.sig(conv, variadic, &params, &rets);
        b.set_signature(sig);
        b.set_attrs(attrs);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let mut vals = Vec::with_capacity_in(n_params, alloc);
        for _ in 0..n_params {
            vals.push(b.param(Ty::I64));
        }
        let mut ret_vals = Vec::with_capacity_in(n_rets, alloc);
        for i in 0..n_rets {
            if let Some(&v) = vals.get(i) {
                ret_vals.push(v);
            } else {
                ret_vals.push(b.iconst(Ty::I64, u128::try_from(i).unwrap_or(0)));
            }
        }
        b.ret(&ret_vals);
        b.finish_checked().unwrap()
    }

    fn build_i64_fn(n_params: usize, n_rets: usize, attrs: FnAttrs) -> Func<Alloc<'static>> {
        build_i64_fn_with_sig(n_params, n_rets, Conv::C, Variadic::No, attrs)
    }

    fn build_f64_fn(n_params: usize) -> Func<Alloc<'static>> {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let mut params = Vec::with_capacity_in(n_params, alloc);
        for _ in 0..n_params {
            params.push(Param::of(Ty::F64));
        }
        let sig = b.sig(Conv::C, Variadic::No, &params, &[Param::of(Ty::F64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let mut first_val = None;
        for _ in 0..n_params {
            let v = b.param(Ty::F64);
            if first_val.is_none() {
                first_val = Some(v);
            }
        }
        let ret_v = first_val.unwrap();
        b.ret(&[ret_v]);
        b.finish_checked().unwrap()
    }

    fn build_sret_and_i64_fn(size_bytes: u32, n_i64_params: usize) -> Func<Alloc<'static>> {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sret = Param {
            ty: Ty::PTR,
            ext: Ext::None,
            kind: ParamKind::Sret,
            facts: Facts { size: size_bytes, align: 3, ..Facts::default() },
        };
        let mut params = Vec::with_capacity_in(1 + n_i64_params, alloc);
        params.push(sret);
        for _ in 0..n_i64_params {
            params.push(Param::of(Ty::I64));
        }
        let sig = b.sig(Conv::C, Variadic::No, &params, &[]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let out_ptr = b.param(Ty::PTR);
        let mut first_x = None;
        for _ in 0..n_i64_params {
            let x = b.param(Ty::I64);
            if first_x.is_none() {
                first_x = Some(x);
            }
        }
        if let Some(x) = first_x {
            b.store(Flags::NONE, x, out_ptr, 0, MemAttrs::aligned(3));
        }
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    fn build_cold_trap_fn(attrs: FnAttrs) -> Func<Alloc<'static>> {
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, test_alloc());
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[]);
        b.set_signature(sig);
        b.set_attrs(attrs);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let _code = b.param(Ty::I64);
        b.trap(1);
        b.finish_checked().unwrap()
    }

    fn build_chain_caller_fn(callee_place: u32) -> Func<Alloc<'static>> {
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, test_alloc());
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let callee_ref = b.reference(RefKind::Func, callee_place);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let ctx = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        let res = b.call(Flags::NONE, callee_ref, sig, &[ctx, x]);
        b.ret(&[res]);
        b.finish_checked().unwrap()
    }

    fn build_chain_leaf_fn() -> Func<Alloc<'static>> {
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, test_alloc());
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let ctx = b.param(Ty::PTR);
        let _x = b.param(Ty::I64);
        let loaded = b.load(Ty::I64, Flags::NONE, ctx, 0, MemAttrs::aligned(3));
        b.ret(&[loaded]);
        b.finish_checked().unwrap()
    }

    fn build_sret_fn(size_bytes: u32) -> Func<Alloc<'static>> {
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, test_alloc());
        let sret = Param {
            ty: Ty::PTR,
            ext: Ext::None,
            kind: ParamKind::Sret,
            facts: Facts { size: size_bytes, align: 3, ..Facts::default() },
        };
        let sig = b.sig(Conv::C, Variadic::No, &[sret, Param::of(Ty::I64)], &[]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let out_ptr = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        b.store(Flags::NONE, x, out_ptr, 0, MemAttrs::aligned(3));
        b.ret(&[]);
        b.finish_checked().unwrap()
    }

    #[test]
    fn exported_and_address_taken_strictly_keep_c_abi() {
        let f_exp = build_i64_fn(12, 4, FnAttrs::default());
        let f_addr = build_i64_fn(12, 4, FnAttrs::default());
        let inputs = [
            AbiFnInput {
                view: f_exp.view(),
                is_exported: true,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_addr.view(),
                is_exported: false,
                address_taken: true,
                is_cold: true,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        assert_eq!(out.plans.len(), 2);
        for plan in out.plans.iter() {
            assert_eq!(plan.conv, CallConvKind::CAbi);
            assert_eq!(plan.param_slots.len(), 12);
            assert_eq!(plan.param_slots[7], ArgSlot::Gpr(7));
            assert_eq!(plan.param_slots[8], ArgSlot::StackOff(0));
            assert_eq!(plan.param_slots[11], ArgSlot::StackOff(24));
            assert_eq!(plan.return_slots[0], ArgSlot::Gpr(0));
            assert_eq!(plan.return_slots[1], ArgSlot::Gpr(1));
            assert_eq!(plan.return_slots[2], ArgSlot::StackOff(0));
            assert!(plan.pinned_globals.is_empty());
        }
        assert_eq!(out.stats.internal_fast_fns, 0);
        assert_eq!(out.stats.cold_preserve_all_fns, 0);
        assert_eq!(out.stats.stack_args_eliminated, 0);
        assert_eq!(out.stats.sret_eliminated, 0);
    }

    #[test]
    fn twelve_arg_internal_fn_eliminates_all_four_stack_args() {
        let f = build_i64_fn(12, 1, FnAttrs::default());
        let inputs = [AbiFnInput {
            view: f.view(),
            is_exported: false,
            address_taken: false,
            is_cold: false,
            direct_callees: &[],
            caller_live_pressure_gprs: RegSet::EMPTY,
            caller_live_pressure_fps: RegSet::EMPTY,
        }];

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        let plan = &out.plans[0];
        assert_eq!(plan.conv, CallConvKind::InternalFast);
        assert_eq!(plan.param_slots.len(), 12);
        for (idx, &expected_reg) in INTERNAL_FAST_GPR_ARGS[..12].iter().enumerate() {
            assert_eq!(plan.param_slots[idx], ArgSlot::Gpr(expected_reg));
        }
        assert_eq!(plan.param_slots[8], ArgSlot::Gpr(9));
        assert_eq!(out.stats.internal_fast_fns, 1);
        assert_eq!(out.stats.stack_args_eliminated, 4);
    }

    #[test]
    fn four_word_return_internal_fn_eliminates_sret() {
        let f_multi = build_i64_fn(4, 4, FnAttrs::default());
        let f_sret = build_sret_fn(32);
        let inputs = [
            AbiFnInput {
                view: f_multi.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_sret.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        for plan in out.plans.iter() {
            assert_eq!(plan.conv, CallConvKind::InternalFast);
            assert_eq!(
                &plan.return_slots[..],
                &[ArgSlot::Gpr(0), ArgSlot::Gpr(1), ArgSlot::Gpr(2), ArgSlot::Gpr(3)]
            );
        }
        assert_eq!(out.stats.sret_eliminated, 2);
    }

    #[test]
    fn cold_preserve_all_clobbers_zero_caller_regs_and_computes_exact_save_set() {
        let f_cold = build_cold_trap_fn(FnAttrs(FnAttrs::COLD | FnAttrs::NORETURN));
        let f_cold_ret = build_i64_fn(1, 1, FnAttrs::default());
        let inputs = [
            AbiFnInput {
                view: f_cold.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_cold_ret.view(),
                is_exported: false,
                address_taken: false,
                is_cold: true,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        assert_eq!(out.stats.cold_preserve_all_fns, 2);

        let touched_x9_x12 = RegSet::EMPTY
            .with(Reg(9))
            .with(Reg(10))
            .with(Reg(11))
            .with(Reg(12));

        let trap_plan = &out.plans[0];
        assert_eq!(trap_plan.conv, CallConvKind::ColdPreserveAll);
        assert_eq!(trap_plan.preserved_regs.len(), 28 + 32);
        assert_eq!(plan_call_clobbers(trap_plan, touched_x9_x12), CallClobbers::EMPTY);
        assert_eq!(cold_preserve_save_set(trap_plan, touched_x9_x12), touched_x9_x12);

        // Cold callee returning `i64` in `x0`: `x0` is excluded from `preserved_regs`,
        // while `x9..x12` are preserved and saved.
        let ret_plan = &out.plans[1];
        assert_eq!(ret_plan.conv, CallConvKind::ColdPreserveAll);
        assert!(!ret_plan.preserved_regs.contains(Reg(0)));
        let touched_with_ret = touched_x9_x12.with(Reg(0));
        assert_eq!(
            plan_call_clobbers(ret_plan, touched_with_ret),
            CallClobbers { clobbered: RegSet::EMPTY.with(Reg(0)) }
        );
        assert_eq!(cold_preserve_save_set(ret_plan, touched_with_ret), touched_x9_x12);
    }

    #[test]
    fn interprocedural_register_avoidance_pushes_caller_live_x9_x12_to_end() {
        let caller = build_i64_fn(2, 1, FnAttrs::default());
        let callee = build_i64_fn(2, 1, FnAttrs::default());
        let hot_gprs = RegSet::EMPTY
            .with(Reg(9))
            .with(Reg(10))
            .with(Reg(11))
            .with(Reg(12));
        let hot_fps = RegSet::EMPTY.with(Reg(32 + 16)).with(Reg(32 + 17));

        let inputs = [
            AbiFnInput {
                view: caller.view(),
                is_exported: true,
                address_taken: false,
                is_cold: false,
                direct_callees: &[1],
                caller_live_pressure_gprs: hot_gprs,
                caller_live_pressure_fps: hot_fps,
            },
            AbiFnInput {
                view: callee.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        let callee_plan = &out.plans[1];
        assert_eq!(&callee_plan.scratch_gpr_order[12..16], &[9, 10, 11, 12]);
        assert_eq!(&callee_plan.scratch_gpr_order[0..3], &[13, 14, 15]);
        assert_eq!(&callee_plan.scratch_fp_order[22..24], &[16, 17]);
        assert_eq!(callee_plan.scratch_fp_order[0], 18);
        assert_eq!(out.stats.avoided_clobber_regs, 6);
    }

    #[test]
    fn scc_passthrough_pointer_pinning_assigns_x28_across_three_fn_chain() {
        let f0 = build_chain_caller_fn(1);
        let f1 = build_chain_caller_fn(2);
        let f2 = build_chain_leaf_fn();

        let inputs = [
            AbiFnInput {
                view: f0.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[1],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f1.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[2],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f2.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        assert_eq!(out.stats.pinned_scc_params, 3);
        for plan in out.plans.iter() {
            assert_eq!(plan.conv, CallConvKind::PinnedScc);
            assert_eq!(&plan.pinned_globals[..], &[PinnedReg { param_index: 0, reg: 28 }]);
            assert_eq!(&plan.param_slots[..], &[ArgSlot::Gpr(28), ArgSlot::Gpr(0)]);
        }
    }

    #[test]
    fn non_passthrough_caller_disqualifies_pinning() {
        let f_caller = build_chain_caller_fn(2);
        // Second caller passes `null` instead of its own `Op::Param(0)`.
        let f_bad_caller = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, test_alloc());
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let callee_ref = b.reference(RefKind::Func, 2);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let _ctx = b.param(Ty::PTR);
            let x = b.param(Ty::I64);
            let nul = b.null();
            let res = b.call(Flags::NONE, callee_ref, sig, &[nul, x]);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };
        let f_leaf = build_chain_leaf_fn();

        let inputs = [
            AbiFnInput {
                view: f_caller.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[2],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_bad_caller.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[2],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_leaf.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        assert_eq!(out.stats.pinned_scc_params, 0);
        for plan in out.plans.iter() {
            assert_eq!(plan.conv, CallConvKind::InternalFast);
            assert!(plan.pinned_globals.is_empty());
        }
    }

    #[test]
    fn internal_fast_gpr_pool_invariants() {
        assert_eq!(INTERNAL_FAST_GPR_ARGS.len(), 15);
        assert_eq!(INTERNAL_FAST_MAX_GPR_RETS, 4);
        assert_eq!(&INTERNAL_FAST_GPR_ARGS[..8], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let mut seen = 0u32;
        for &reg in &INTERNAL_FAST_GPR_ARGS {
            assert!(reg < 32, "GPR index out of range: {reg}");
            assert_ne!(reg, A64_SRET_REG, "X8 must stay dedicated to Sret");
            assert_ne!(reg, 16, "X16 is linker/GotSlot scratch");
            assert_ne!(reg, 17, "X17 is linker/GotSlot scratch");
            assert_ne!(reg, 18, "X18 is macOS platform register");
            assert!(reg < 19, "X19..=X31 must never appear in InternalFast GPR arg pool: {reg}");
            assert!(!C_ABI_PRESERVED.contains(Reg(reg)), "GPR arg must not be callee-saved: {reg}");

            let bit = 1u32 << u32::from(reg);
            assert_eq!(seen & bit, 0, "duplicate GPR in INTERNAL_FAST_GPR_ARGS: {reg}");
            seen |= bit;
        }
    }

    #[test]
    fn internal_fast_fp_pool_invariants() {
        assert_eq!(INTERNAL_FAST_FP_ARGS.len(), 16);
        assert_eq!(&INTERNAL_FAST_FP_ARGS[..8], &[0, 1, 2, 3, 4, 5, 6, 7]);

        let mut seen = 0u32;
        for &reg in &INTERNAL_FAST_FP_ARGS {
            assert!(reg < 32, "FP index out of range: {reg}");
            assert!(
                !(8..=15).contains(&reg),
                "V8..=V15 are callee-saved and must never appear in InternalFast FP pool: {reg}"
            );
            assert!(
                !(29..=31).contains(&reg),
                "V29..=V31 are inline NEON / RA spill scratch: {reg}"
            );
            assert!(
                !C_ABI_PRESERVED.contains(Reg(32 + reg)),
                "FP arg must not be callee-saved: {reg}"
            );

            let bit = 1u32 << u32::from(reg);
            assert_eq!(seen & bit, 0, "duplicate FP reg in INTERNAL_FAST_FP_ARGS: {reg}");
            seen |= bit;
        }
    }

    #[test]
    fn fourteen_arg_private_leaf_uses_internal_fast_while_exported_addr_taken_variadic_stay_c_abi()
    {
        let f_caller = build_i64_fn(2, 1, FnAttrs::default());
        let f_private_leaf = build_i64_fn(14, 1, FnAttrs::default());
        let f_exported_leaf = build_i64_fn(14, 1, FnAttrs::default());
        let f_addr_taken_leaf = build_i64_fn(14, 1, FnAttrs::default());
        let f_variadic_leaf =
            build_i64_fn_with_sig(14, 1, Conv::C, Variadic::Yes, FnAttrs::default());
        let f_foreign_leaf =
            build_i64_fn_with_sig(14, 1, Conv::SysV64, Variadic::No, FnAttrs::default());

        let inputs = [
            AbiFnInput {
                view: f_caller.view(),
                is_exported: true,
                address_taken: false,
                is_cold: false,
                direct_callees: &[1, 2, 3, 4, 5],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_private_leaf.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_exported_leaf.view(),
                is_exported: true,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_addr_taken_leaf.view(),
                is_exported: false,
                address_taken: true,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_variadic_leaf.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_foreign_leaf.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        assert_eq!(count_sig_arg_classes(inputs[1].view), (14, 0));
        assert!(is_internal_fast_eligible(&inputs[1], 14, 0));
        assert!(!is_internal_fast_eligible(&inputs[1], 8, 8));
        assert!(!is_internal_fast_eligible(&inputs[2], 14, 0));
        assert!(!is_internal_fast_eligible(&inputs[3], 14, 0));
        assert!(!is_internal_fast_eligible(&inputs[4], 14, 0));
        assert!(!is_internal_fast_eligible(&inputs[5], 14, 0));

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());
        assert_eq!(out.plans[0].conv, CallConvKind::CAbi);

        // Private 14-arg leaf: upgraded to InternalFast with zero stack args and no X8 collision.
        let leaf_plan = &out.plans[1];
        assert_eq!(leaf_plan.conv, CallConvKind::InternalFast);
        assert_eq!(leaf_plan.param_slots.len(), 14);
        for (idx, &expected_reg) in INTERNAL_FAST_GPR_ARGS[..14].iter().enumerate() {
            assert_eq!(leaf_plan.param_slots[idx], ArgSlot::Gpr(expected_reg));
        }

        // Exported, address-taken, variadic, and foreign-Conv 14-arg leaves stay strictly CAbi.
        for idx in 2..=5 {
            let plan = &out.plans[idx];
            assert_eq!(plan.conv, CallConvKind::CAbi);
            assert_eq!(plan.param_slots[7], ArgSlot::Gpr(7));
            assert_eq!(plan.param_slots[8], ArgSlot::StackOff(0));
            assert_eq!(plan.param_slots[13], ArgSlot::StackOff(40));
        }

        assert_eq!(out.stats.internal_fast_fns, 1);
        assert_eq!(out.stats.wide_arg_fast_fns, 1);
        assert_eq!(out.stats.stack_args_eliminated, 6);
    }

    #[test]
    fn sixteen_fp_arg_and_unpromotable_sret_avoid_v8_v15_and_x8() {
        let f_caller = build_i64_fn(2, 1, FnAttrs::default());
        let f_fp_leaf = build_f64_fn(16);
        // 128-byte Sret (> 8*8 bytes) cannot be register-promoted, so it retains X8 alongside 14 GPR args.
        let f_sret_leaf = build_sret_and_i64_fn(128, 14);

        let inputs = [
            AbiFnInput {
                view: f_caller.view(),
                is_exported: true,
                address_taken: false,
                is_cold: false,
                direct_callees: &[1, 2],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_fp_leaf.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
            AbiFnInput {
                view: f_sret_leaf.view(),
                is_exported: false,
                address_taken: false,
                is_cold: false,
                direct_callees: &[],
                caller_live_pressure_gprs: RegSet::EMPTY,
                caller_live_pressure_fps: RegSet::EMPTY,
            },
        ];

        assert_eq!(count_sig_arg_classes(inputs[1].view), (0, 16));
        assert_eq!(count_sig_arg_classes(inputs[2].view), (14, 0));
        assert!(is_internal_fast_eligible(&inputs[1], 0, 16));
        assert!(is_internal_fast_eligible(&inputs[2], 14, 0));

        let out = plan_module_abi(&inputs, ZeroAbiOpts::arm64_export(), test_alloc());

        let fp_plan = &out.plans[1];
        assert_eq!(fp_plan.conv, CallConvKind::InternalFast);
        assert_eq!(fp_plan.param_slots.len(), 16);
        for (idx, &expected_reg) in INTERNAL_FAST_FP_ARGS.iter().enumerate() {
            assert_eq!(fp_plan.param_slots[idx], ArgSlot::Fp(expected_reg));
        }

        let sret_plan = &out.plans[2];
        assert_eq!(sret_plan.conv, CallConvKind::InternalFast);
        assert_eq!(sret_plan.param_slots.len(), 15);
        assert_eq!(sret_plan.param_slots[0], ArgSlot::Gpr(A64_SRET_REG));
        for (idx, &expected_reg) in INTERNAL_FAST_GPR_ARGS[..14].iter().enumerate() {
            assert_eq!(sret_plan.param_slots[1 + idx], ArgSlot::Gpr(expected_reg));
        }

        assert_eq!(out.stats.internal_fast_fns, 2);
        assert_eq!(out.stats.wide_arg_fast_fns, 2);
        assert_eq!(out.stats.stack_args_eliminated, 14);
    }
}
