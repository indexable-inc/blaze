#![forbid(unsafe_code)]
//! Unified, deterministic, SoA **`BlockFreq` + Newell–Pupyrev `Ext-TSP` Basic-Block Layout Engine**
//! for `TierName::Sota` (shared by AArch64 and x86-64 emitters).
//!
//! Replaces both `isel.rs::compute_hot_emit_layout` and
//! `release_opts.rs::{compute_block_order, hoist_multi_pred_br_latches}` with a single
//! `#![no_std]`, zero-`unsafe`, explicit-allocator, bounded-time pipeline:
//!
//! 1. **Stage 1 — CFG & Natural Loop Analysis (`analyze_cfg`)**:
//!    - Iterative-DFS RPO (`rpo`, `rpo_rank`, `reachable`), entry `0` first.
//!    - Direct + backward-propagated cold classification (`BlockKind::Cold`, landing pads,
//!      `Trap`/`Unreachable`/`Resume`, and acyclic blocks whose every forward successor is cold).
//!    - Direct + backward-propagated terminal-exit classification (`Return`/`TailCall`/
//!      `TailCallIndirect`/cold and acyclic depth-0 blocks whose every forward successor exits).
//!    - Exact natural loop headers (`is_loop_hdr`) and loop nesting depths (`loop_depth`) via
//!      backward predecessor reachability (`EXACT_LOOP_MAX_BLOCKS = 4096`), with an `O(V + E)`
//!      RPO-interval difference-array fallback above 4096 blocks (Rule 18).
//! 2. **Stage 2 — Principled `BlockFreq` & Edge-Weight Solver (`solve_frequencies_and_edges`)**:
//!    - Scaled branch probabilities (`PROB_SCALE = 1024`) from optional PGO `(taken, not_taken)`
//!      counts (`Bpg2Profile`) or static Wu–Larus + loop-nest heuristics.
//!    - Single-pass RPO frequency propagation with bounded loop amplification (`* 10` per natural
//!      loop nesting level, clamped at `MAX_FREQ = 1 << 40`) producing a flat CSR of edge weights
//!      (`edge_off`, `edge_dst`, `edge_weight`).
//! 3. **Stage 3 — Newell–Pupyrev `Ext-TSP` Basic-Block Chain Merging (`ext_tsp_order`)**:
//!    - Flat SoA intrusive chain table with exact Ext-TSP distance-weighted objective:
//!      - Fallthrough (`dist == 0`): `w * 100`.
//!      - Short forward jump (`0 < dist < 1024` B): `w * 10 * (1024 - dist) / 1024`.
//!      - Short backward jump (`0 < dist < 640` B): `w * 10 * (640 - dist) / 640`.
//!      - Non-rotatable loop back-edges earn `0` whenever `pos_v > pos_u` (neither fallthrough nor
//!        forward short-jump credit), so loops never invert their body/latch before their header
//!        when body size exceeds `EXT_TSP_BWD_LIMIT` and preserve preheader->header fallthrough,
//!        while tiny `>= 3`-pred bottom-tested `Op::Br` latches remain eligible and rotate
//!        naturally immediately before their loop header.
//!    - 10-way split/rotate merge candidates with version-stamped cross-chain edge caching,
//!      bounded to `MAX_EXT_TSP_BLOCKS = 1024` hot blocks and `MAX_MERGES = 2048` merges (with an
//!      `O(V log V + E)` weighted-greedy trace fallback for mega-functions).
//!    - Pins entry block `0` at `order[0]`, sinks all cold blocks to the tail in deterministic RPO
//!      order, and marks rotated multi-pred latches in `is_loop_hdr` for 16B alignment.

use core::cmp::Reverse;

use blaze_lir::{Block, BlockKind, Flags, Op, View};
use blaze_own::{Alloc, Vec};

/// Maximum block count for exact backward-BFS natural-loop analysis; mega-functions above this
/// bound use the `O(V + E)` RPO-interval fallback (Rule 18).
const EXACT_LOOP_MAX_BLOCKS: usize = 4096;

/// Maximum hot block count for full Newell–Pupyrev Ext-TSP chain merging; mega-functions above
/// this bound use the `O(V log V + E)` weighted-greedy trace fallback (Rule 18).
const MAX_EXT_TSP_BLOCKS: usize = 1024;

/// Maximum chain-merge steps in Ext-TSP (Rule 18).
const MAX_MERGES: usize = 2048;

/// Maximum combined chain length for split/rotate merge evaluation; larger pairs evaluate direct
/// concatenations only (`X ++ Y`, `Y ++ X`) so mega-chains stay fast.
const MAX_SPLIT_CHAIN_LEN: usize = 192;

/// Fixed-point denominator for outgoing branch probabilities (`1.0 == 1024`).
const PROB_SCALE: u64 = 1024;

/// Base execution frequency seeded at entry block `0`.
const ENTRY_FREQ: u64 = 1024;

/// Loop trip-count amplification factor per natural loop header.
const LOOP_TRIP_MULT: u64 = 10;

/// Safe upper ceiling for `block_freq` preventing `u64` overflow across deep loop nests.
const MAX_FREQ: u64 = 1u64 << 40;

/// Ext-TSP score weight for an adjacent fallthrough edge (`dist == 0`).
const EXT_TSP_FALLTHROUGH_WEIGHT: u128 = 100;

/// Ext-TSP base weight for short forward/backward jumps.
const EXT_TSP_JUMP_WEIGHT: u128 = 10;

/// Maximum forward jump byte distance earning Ext-TSP locality credit.
const EXT_TSP_FWD_LIMIT: u64 = 1024;

/// Maximum backward jump byte distance earning Ext-TSP locality credit.
const EXT_TSP_BWD_LIMIT: u64 = 640;

/// Maximum material instructions in a tiny multi-predecessor bottom-tested loop latch eligible for
/// natural latch-before-header rotation.
const MAX_ROTATABLE_LATCH_INSTS: u32 = 4;

/// Minimum distinct predecessors converging on a bottom-tested `Op::Br` loop latch for
/// latch-before-header rotation to strictly eliminate taken branches across arms.
const MIN_ROTATABLE_LATCH_PREDS: u32 = 3;

/// Maximum estimated post-isel byte size (`<= 6` instructions) for an arm to qualify as a compact
/// fast path in the Wu–Larus / LLVM BPI diamond heuristic.
const COMPACT_FAST_PATH_MAX_BYTES: u32 = 24;

/// Minimum estimated post-isel byte-size gap (`>= 2` instructions) between a slow arm and a
/// compact fast-path arm in the diamond heuristic.
const COMPACT_FAST_PATH_MIN_DIFF_BYTES: u32 = 8;

/// Minimum estimated post-isel byte size (`>= 4` instructions) for the slower arm in the compact
/// fast-path diamond heuristic so tiny 1-op + cold-trap check blocks are not penalized as slow paths.
const COMPACT_FAST_PATH_MIN_SLOW_BYTES: u32 = 16;

/// Maximum forward CFG hops from an inner loop header (`0` = header, `1` = preheader, `2` =
/// bounds-check guard + preheader) over which `entry_depth` propagates backward so only genuine
/// conditional inner-loop guards/preheaders (like `if lit_len > 32 { bounds_check; while_loop }`)
/// trigger Step 7, never multi-stage loop bodies (`match_body -> rbit -> lazy -> emit_seq ->
/// while_back_ext`).
const MAX_ENTRY_DEPTH_DIST: u8 = 2;

/// Unified basic-block emission order, loop/cold metadata, block frequencies, and flat CSR edge
/// weights produced by [`compute_block_layout`] / [`compute_block_layout_with_pgo`].
pub(crate) struct BlockLayout<'a> {
    pub order: Vec<usize, Alloc<'a>>,
    pub loop_depth: Vec<u32, Alloc<'a>>,
    pub is_loop_hdr: Vec<bool, Alloc<'a>>,
    pub is_cold: Vec<bool, Alloc<'a>>,
    pub is_terminal_exit: Vec<bool, Alloc<'a>>,
    pub block_freq: Vec<u64, Alloc<'a>>,
    /// Flat CSR of outgoing edge weights per block (indexed by `edge_off[b]..edge_off[b+1]`).
    pub edge_off: Vec<usize, Alloc<'a>>,
    pub edge_dst: Vec<usize, Alloc<'a>>,
    pub edge_weight: Vec<u64, Alloc<'a>>,
}

impl<'a> BlockLayout<'a> {
    /// Returns the total solved edge weight from `src` to `dst` (`0` if no edge or out of bounds).
    #[inline]
    pub(crate) fn edge_weight_between(&self, src: usize, dst: usize) -> u64 {
        let (Some(&start), Some(&end)) = (self.edge_off.get(src), self.edge_off.get(src + 1)) else {
            return 0;
        };
        let (Some(dsts), Some(weights)) = (self.edge_dst.get(start..end), self.edge_weight.get(start..end)) else {
            return 0;
        };
        let mut total = 0u64;
        for (&d, &w) in dsts.iter().zip(weights.iter()) {
            if d == dst {
                total = total.saturating_add(w);
            }
        }
        total
    }
}

/// Estimates emitted post-isel instruction size in bytes for `block`, minimum `4` B.
///
/// Skips zero-cost SSA pseudo-ops, conversions, addressing (`PtrAdd`), unconditional `Jump`, and
/// immediate shifts that fold into addressing modes or barrel-shifter operands.
#[inline]
pub(crate) fn block_size_bytes(view: &View<'_>, block: usize) -> u32 {
    if block >= view.blocks() {
        return 4;
    }
    let (_, est_code_bytes, _) = analyze_block_body(view, block);
    est_code_bytes
}

/// Computes the unified `BlockFreq` + Newell–Pupyrev `Ext-TSP` basic-block layout using static
/// Wu–Larus + loop-nest branch probabilities (zero PGO callback overhead).
pub(crate) fn compute_block_layout<'a>(view: &View<'_>, alloc: Alloc<'a>) -> BlockLayout<'a> {
    compute_block_layout_with_pgo(view, None, alloc)
}

/// Fast `O(V + E)` RPO-only basic-block layout for [`super::EmitProfile::Fast`] cold compilation:
/// skips instruction body scanning, backward natural-loop BFS, Wu–Larus frequency solving, and
/// Ext-TSP chain merging while preserving entry block `0` first and exact block coverage.
pub(crate) fn compute_fast_block_layout<'a>(view: &View<'_>, alloc: Alloc<'a>) -> BlockLayout<'a> {
    let blocks = view.blocks();
    if blocks == 0 {
        let mut edge_off = Vec::with_capacity_in(1, alloc);
        edge_off.push(0);
        return BlockLayout {
            order: Vec::new_in(alloc),
            loop_depth: Vec::new_in(alloc),
            is_loop_hdr: Vec::new_in(alloc),
            is_cold: Vec::new_in(alloc),
            is_terminal_exit: Vec::new_in(alloc),
            block_freq: Vec::new_in(alloc),
            edge_off,
            edge_dst: Vec::new_in(alloc),
            edge_weight: Vec::new_in(alloc),
        };
    }

    let mut dfs_state: Vec<u8, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut order: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut dfs_stack: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks.min(1024), alloc);
    dfs_stack.push(0);
    while let Some(b) = dfs_stack.pop() {
        let st = at_u8(&dfs_state, b);
        if st == 0 {
            set_at(&mut dfs_state, b, 1);
            dfs_stack.push(b);
            for_each_succ(view, b, |t| {
                if at_u8(&dfs_state, t) == 0 {
                    dfs_stack.push(t);
                }
            });
        } else if st == 1 {
            set_at(&mut dfs_state, b, 2);
            order.push(b);
        }
    }
    order.reverse();
    for b in 0..blocks {
        if at_u8(&dfs_state, b) != 2 {
            order.push(b);
        }
    }

    let mut is_cold: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let kind = view.block_kind(Block(u32_of(b)));
        let top = view.op(view.terminator(Block(u32_of(b))));
        let cold = b != 0
            && (kind == BlockKind::Cold
                || kind.is_landing()
                || matches!(top, Op::Trap | Op::Unreachable | Op::Resume));
        is_cold.push(cold);
    }

    BlockLayout {
        order,
        loop_depth: Vec::from_elem_in(0, blocks, alloc),
        is_loop_hdr: Vec::from_elem_in(false, blocks, alloc),
        is_cold,
        is_terminal_exit: Vec::from_elem_in(false, blocks, alloc),
        block_freq: Vec::new_in(alloc),
        edge_off: Vec::from_elem_in(0, blocks + 1, alloc),
        edge_dst: Vec::new_in(alloc),
        edge_weight: Vec::new_in(alloc),
    }
}

/// Computes the unified `BlockFreq` + Newell–Pupyrev `Ext-TSP` basic-block layout with optional
/// PGO branch weights `branch_weights(src_block, then_target_block) -> Option<(taken, not_taken)>`.
pub(crate) fn compute_block_layout_with_pgo<'a>(
    view: &View<'_>,
    branch_weights: Option<&dyn Fn(usize, usize) -> Option<(u64, u64)>>,
    alloc: Alloc<'a>,
) -> BlockLayout<'a> {
    let blocks = view.blocks();
    if blocks == 0 {
        let mut edge_off = Vec::with_capacity_in(1, alloc);
        edge_off.push(0);
        return BlockLayout {
            order: Vec::new_in(alloc),
            loop_depth: Vec::new_in(alloc),
            is_loop_hdr: Vec::new_in(alloc),
            is_cold: Vec::new_in(alloc),
            is_terminal_exit: Vec::new_in(alloc),
            block_freq: Vec::new_in(alloc),
            edge_off,
            edge_dst: Vec::new_in(alloc),
            edge_weight: Vec::new_in(alloc),
        };
    }

    // Stage 1: Exact RPO, cold/exit classification, natural loop headers & depths.
    let mut cfg = analyze_cfg(view, alloc);

    // Stage 2: Principled BlockFreq & flat CSR edge-weight solver.
    let freq = solve_frequencies_and_edges(view, &cfg, branch_weights, alloc);

    // Stage 3: Newell–Pupyrev Ext-TSP basic-block chain merging (with O(V log V + E) fallback).
    let order = if cfg.hot_reachable_count <= MAX_EXT_TSP_BLOCKS {
        ext_tsp_order(&cfg, &freq, alloc)
    } else {
        greedy_trace_order(&cfg, &freq, alloc)
    };

    // Post-condition: mark any multi-pred (`>= 3` preds) bottom-tested `Op::Br` loop latch that
    // Ext-TSP rotated immediately before its header (`pos(l) + 1 == pos(h)` with strictly shallower
    // predecessor) in `is_loop_hdr` so the emitter 16B-aligns the rotated latch cleanly.
    mark_rotated_multi_pred_latches(&order, &mut cfg);

    assert_eq!(order.len(), blocks, "compute_block_layout must emit every block exactly once");
    assert_eq!(
        order.first().copied(),
        Some(0),
        "compute_block_layout must keep entry block 0 first"
    );

    BlockLayout {
        order,
        loop_depth: cfg.loop_depth,
        is_loop_hdr: cfg.is_loop_hdr,
        is_cold: cfg.is_cold,
        is_terminal_exit: cfg.is_terminal_exit,
        block_freq: freq.block_freq,
        edge_off: freq.edge_off,
        edge_dst: freq.edge_dst,
        edge_weight: freq.edge_weight,
    }
}

#[inline]
fn to_idx(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[inline]
fn at_u8(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u64(s: &[u64], i: usize) -> u64 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_u128(s: &[u128], i: usize) -> u128 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_usize(s: &[usize], i: usize) -> usize {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_bool(s: &[bool], i: usize) -> bool {
    s.get(i).copied().unwrap_or(false)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn inc_sat_u32(s: &mut [u32], i: usize) {
    if let Some(slot) = s.get_mut(i) {
        *slot = slot.saturating_add(1);
    }
}

/// Counts material instructions (excluding `Param`/`Iconst`/`IconstWide`), estimates emitted
/// post-isel code size in bytes (skipping zero-cost/folded/addressing/fallthrough ops and
/// immediate shifts), and checks whether `block` contains any call instruction.
fn analyze_block_body(view: &View<'_>, block: usize) -> (u32, u32, bool) {
    let mut material: u32 = 0;
    let mut est_code_insts: u32 = 0;
    let mut has_call = false;
    let inst_count = view.len();
    for inst in view.block(Block(u32_of(block))) {
        let op = view.op(inst);
        if matches!(
            op,
            Op::Call
                | Op::CallIndirect
                | Op::Invoke
                | Op::InvokeIndirect
                | Op::TailCall
                | Op::TailCallIndirect
        ) {
            has_call = true;
        }
        if !matches!(op, Op::Param | Op::Iconst | Op::IconstWide) {
            material = material.saturating_add(1);
        }
        let is_zero_cost = matches!(
            op,
            Op::Param
                | Op::Iconst
                | Op::IconstWide
                | Op::Fconst
                | Op::FconstWide
                | Op::Zext
                | Op::Sext
                | Op::Trunc
                | Op::Bitcast
                | Op::PtrAdd
                | Op::Jump
        ) || (matches!(op, Op::Shl | Op::Lshr | Op::Ashr) && {
            let mut ops = view.operands(inst);
            let _lhs = ops.value();
            let rhs = ops.value();
            let rhs_idx = to_idx(rhs.0);
            rhs_idx < inst_count && matches!(view.op(rhs_idx), Op::Iconst | Op::IconstWide)
        });
        if !is_zero_cost {
            est_code_insts = est_code_insts.saturating_add(1);
        }
    }
    let est_code_bytes = est_code_insts.max(1).saturating_mul(4);
    (material, est_code_bytes, has_call)
}

/// Visits every valid in-bounds CFG successor block index of `block` in deterministic operand order.
fn for_each_succ(view: &View<'_>, block: usize, mut f: impl FnMut(usize)) {
    let blocks = view.blocks();
    let term = view.terminator(Block(u32_of(block)));
    let mut ops = view.operands(term);
    let mut emit = |blk: Block| {
        let t = to_idx(blk.0);
        if t < blocks {
            f(t);
        }
    };
    match view.op(term) {
        Op::Jump => emit(ops.target().block),
        Op::Br => {
            let _cond = ops.value();
            emit(ops.target().block);
            emit(ops.target().block);
        }
        Op::Switch => {
            let scrut = ops.value();
            emit(ops.target().block);
            for (_, tgt) in ops.cases(scrut) {
                emit(tgt.block);
            }
        }
        Op::Invoke => {
            let _callee = ops.reference();
            let _sig = ops.sig();
            emit(ops.block());
            emit(ops.block());
        }
        Op::InvokeIndirect => {
            let _callee = ops.value();
            let _sig = ops.sig();
            emit(ops.block());
            emit(ops.block());
        }
        Op::AsmBr => {
            let _asm = ops.asm();
            emit(ops.block());
            for &lbl in ops.blocks() {
                emit(Block(lbl));
            }
        }
        Op::Suspend => {
            let _imm = ops.imm();
            emit(ops.block());
            emit(ops.block());
        }
        _ => {}
    }
}

/// Stage-1 CFG, loop-nest, cold/exit, and block-size analysis results (SoA).
struct CfgInfo<'a> {
    blocks: usize,
    hot_reachable_count: usize,
    rpo: Vec<usize, Alloc<'a>>,
    rpo_rank: Vec<usize, Alloc<'a>>,
    reachable: Vec<bool, Alloc<'a>>,
    is_cold: Vec<bool, Alloc<'a>>,
    is_direct_exit: Vec<bool, Alloc<'a>>,
    is_terminal_exit: Vec<bool, Alloc<'a>>,
    loop_depth: Vec<u32, Alloc<'a>>,
    is_loop_hdr: Vec<bool, Alloc<'a>>,
    eff_depth: Vec<u32, Alloc<'a>>,
    entry_depth: Vec<u32, Alloc<'a>>,
    /// For a 1-hop conditional inner-loop guard `s` (`Op::Br` with one deeper inner-loop arm and
    /// one non-cold, non-continue-stub same-depth bypass arm), holds the bypass block index;
    /// `usize::MAX` for all other blocks.
    guard_bypass: Vec<usize, Alloc<'a>>,
    /// Deepest `entry_depth` guarded by 1-hop inner-loop guard `s`; `0` for all other blocks.
    guard_deep_depth: Vec<u32, Alloc<'a>>,
    /// `true` iff `b` is a reachable, non-cold, non-header in-loop block with no forward hot
    /// successor whose terminator is an unconditional back-edge `Op::Jump` to a loop header.
    is_continue_stub: Vec<bool, Alloc<'a>>,
    size_bytes: Vec<u32, Alloc<'a>>,
    distinct_preds: Vec<u32, Alloc<'a>>,
    /// `true` iff `b` has at least one non-cold CFG successor with strictly greater RPO rank.
    has_forward_succ: Vec<bool, Alloc<'a>>,
    /// For a block terminating in `Op::Jump` to an in-bounds target `dst`, holds `dst`;
    /// `usize::MAX` for all other terminators.
    jump_target: Vec<usize, Alloc<'a>>,
    /// For a tiny, call-free, `>= 3`-pred bottom-tested `Op::Br` loop latch `l`, holds its target
    /// loop header `h`; `usize::MAX` for all other blocks.
    rotatable_latch_hdr: Vec<usize, Alloc<'a>>,
}

/// Stage 1: Computes iterative-DFS RPO, cold/exit classification, exact natural loop headers &
/// nesting depths (with coarse fallback above [`EXACT_LOOP_MAX_BLOCKS`]), forward `eff_depth` /
/// `entry_depth`, estimated block byte sizes, and rotatable multi-pred latch metadata.
fn analyze_cfg<'a>(view: &View<'_>, alloc: Alloc<'a>) -> CfgInfo<'a> {
    let blocks = view.blocks();

    // 1. Iterative-DFS RPO order over `0..blocks` (entry `0` first; unreachable blocks appended).
    let mut dfs_state: Vec<u8, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut rpo: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut dfs_stack: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks.min(1024), alloc);
    dfs_stack.push(0);
    while let Some(b) = dfs_stack.pop() {
        let st = at_u8(&dfs_state, b);
        if st == 0 {
            set_at(&mut dfs_state, b, 1);
            dfs_stack.push(b);
            for_each_succ(view, b, |t| {
                if at_u8(&dfs_state, t) == 0 {
                    dfs_stack.push(t);
                }
            });
        } else if st == 1 {
            set_at(&mut dfs_state, b, 2);
            rpo.push(b);
        }
    }
    rpo.reverse();
    let mut reachable: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let is_reach = at_u8(&dfs_state, b) == 2;
        reachable.push(is_reach);
        if !is_reach {
            rpo.push(b);
        }
    }
    let mut rpo_rank: Vec<usize, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    for (rank, &b) in rpo.iter().enumerate() {
        set_at(&mut rpo_rank, b, rank);
    }

    // 2. Direct cold / direct exit classification + per-block material size, call flags, & jump target.
    let mut is_cold: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut is_direct_exit: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut material_insts: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut size_bytes: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut has_call: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut jump_target: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for b in 0..blocks {
        let kind = view.block_kind(Block(u32_of(b)));
        let term = view.terminator(Block(u32_of(b)));
        let top = view.op(term);
        let cold = b != 0
            && (kind == BlockKind::Cold
                || kind.is_landing()
                || matches!(top, Op::Trap | Op::Unreachable | Op::Resume));
        let direct_exit = b != 0
            && (cold || matches!(top, Op::Return | Op::TailCall | Op::TailCallIndirect));
        let jt = if top == Op::Jump {
            let mut ops = view.operands(term);
            let dst = to_idx(ops.target().block.0);
            if dst < blocks { dst } else { usize::MAX }
        } else {
            usize::MAX
        };
        let (mat, est_bytes, calls) = analyze_block_body(view, b);
        is_cold.push(cold);
        is_direct_exit.push(direct_exit);
        material_insts.push(mat);
        size_bytes.push(est_bytes);
        has_call.push(calls);
        jump_target.push(jt);
    }

    // Propagate coldness backward across acyclic blocks whose EVERY forward successor is cold.
    for &b in rpo.iter().rev() {
        if b == 0 || at_bool(&is_cold, b) {
            continue;
        }
        let b_rank = at_usize(&rpo_rank, b);
        let mut has_succ = false;
        let mut all_cold = true;
        for_each_succ(view, b, |t| {
            has_succ = true;
            if at_usize(&rpo_rank, t) <= b_rank || !at_bool(&is_cold, t) {
                all_cold = false;
            }
        });
        if has_succ && all_cold {
            set_at(&mut is_cold, b, true);
            set_at(&mut is_direct_exit, b, true);
        }
    }

    // 3. Exact natural loop headers & depths (bounded to `EXACT_LOOP_MAX_BLOCKS`, coarse above).
    let mut loop_depth: Vec<u32, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut is_loop_hdr: Vec<bool, Alloc<'a>> = Vec::from_elem_in(false, blocks, alloc);
    if blocks <= EXACT_LOOP_MAX_BLOCKS {
        let mut pred_counts: Vec<usize, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
        let mut latch_head: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
        let mut latch_src: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
        let mut latch_next: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
        let mut total_edges: usize = 0;

        for &b in &rpo {
            if !at_bool(&reachable, b) {
                continue;
            }
            let b_rank = at_usize(&rpo_rank, b);
            for_each_succ(view, b, |t| {
                if let Some(cnt) = pred_counts.get_mut(t) {
                    *cnt += 1;
                    total_edges += 1;
                }
                if at_usize(&rpo_rank, t) <= b_rank && !at_bool(&is_direct_exit, t) {
                    set_at(&mut is_loop_hdr, t, true);
                    let prev_head = at_usize(&latch_head, t);
                    let node = latch_src.len();
                    latch_src.push(b);
                    latch_next.push(prev_head);
                    set_at(&mut latch_head, t, node);
                }
            });
        }

        let mut pred_starts: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks + 1, alloc);
        let mut pred_cursor: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
        let mut offset: usize = 0;
        for b in 0..blocks {
            pred_starts.push(offset);
            pred_cursor.push(offset);
            offset += at_usize(&pred_counts, b);
        }
        pred_starts.push(offset);

        let mut pred_list: Vec<usize, Alloc<'a>> = Vec::from_elem_in(0, total_edges, alloc);
        for &b in &rpo {
            if !at_bool(&reachable, b) {
                continue;
            }
            for_each_succ(view, b, |t| {
                if let Some(cur) = pred_cursor.get_mut(t) {
                    let pos = *cur;
                    *cur += 1;
                    set_at(&mut pred_list, pos, b);
                }
            });
        }

        let mut stamp: Vec<u32, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
        let mut work: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
        let mut cur_stamp: u32 = 0;

        for hdr in 0..blocks {
            let mut node = at_usize(&latch_head, hdr);
            if node == usize::MAX {
                continue;
            }
            let hdr_rank = at_usize(&rpo_rank, hdr);
            cur_stamp = cur_stamp.wrapping_add(1);
            if cur_stamp == 0 {
                stamp.as_mut_slice().fill(0);
                cur_stamp = 1;
            }
            work.clear();
            set_at(&mut stamp, hdr, cur_stamp);
            if !at_bool(&is_direct_exit, hdr) {
                inc_sat_u32(&mut loop_depth, hdr);
            }
            while node != usize::MAX {
                let src = at_usize(&latch_src, node);
                if src < blocks && at_u32(&stamp, src) != cur_stamp {
                    set_at(&mut stamp, src, cur_stamp);
                    if !at_bool(&is_direct_exit, src) {
                        inc_sat_u32(&mut loop_depth, src);
                    }
                    work.push(src);
                }
                node = at_usize(&latch_next, node);
            }
            while let Some(u) = work.pop() {
                let start = at_usize(&pred_starts, u);
                let end = at_usize(&pred_starts, u + 1);
                if let Some(preds) = pred_list.get(start..end) {
                    for &p in preds {
                        if p < blocks
                            && at_usize(&rpo_rank, p) >= hdr_rank
                            && at_u32(&stamp, p) != cur_stamp
                        {
                            set_at(&mut stamp, p, cur_stamp);
                            if !at_bool(&is_direct_exit, p) {
                                inc_sat_u32(&mut loop_depth, p);
                            }
                            work.push(p);
                        }
                    }
                }
            }
        }
    } else {
        // O(V + E) RPO-interval fallback for mega-functions (`blocks > 4096`).
        let mut max_latch_rank: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
        for (rank, &b) in rpo.iter().enumerate() {
            if !at_bool(&reachable, b) {
                continue;
            }
            for_each_succ(view, b, |t| {
                let tr = at_usize(&rpo_rank, t);
                if tr <= rank && !at_bool(&is_direct_exit, t) {
                    set_at(&mut is_loop_hdr, t, true);
                    let cur = at_usize(&max_latch_rank, tr);
                    if cur == usize::MAX || rank > cur {
                        set_at(&mut max_latch_rank, tr, rank);
                    }
                }
            });
        }
        let mut depth_diff: Vec<i32, Alloc<'a>> = Vec::from_elem_in(0, blocks + 1, alloc);
        for (tr, &latch) in max_latch_rank.iter().enumerate() {
            if latch != usize::MAX {
                if let Some(s) = depth_diff.get_mut(tr) {
                    *s = s.saturating_add(1);
                }
                if let Some(e) = depth_diff.get_mut(latch + 1) {
                    *e = e.saturating_sub(1);
                }
            }
        }
        let mut cur_depth = 0i32;
        for (rank, &b) in rpo.iter().enumerate() {
            cur_depth = cur_depth.saturating_add(depth_diff.get(rank).copied().unwrap_or(0));
            let d = if at_bool(&is_direct_exit, b) {
                0
            } else {
                u32::try_from(cur_depth.max(0)).unwrap_or(u32::MAX)
            };
            set_at(&mut loop_depth, b, d);
        }
    }

    // 4. Direct + backward-propagated terminal exit classification.
    let mut is_terminal_exit: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for &direct in &is_direct_exit {
        is_terminal_exit.push(direct);
    }
    for &b in rpo.iter().rev() {
        if b == 0 || at_bool(&is_terminal_exit, b) || at_u32(&loop_depth, b) > 0 {
            continue;
        }
        let b_rank = at_usize(&rpo_rank, b);
        let mut has_succ = false;
        let mut all_exit = true;
        for_each_succ(view, b, |t| {
            has_succ = true;
            if at_usize(&rpo_rank, t) <= b_rank || !at_bool(&is_terminal_exit, t) {
                all_exit = false;
            }
        });
        if has_succ && all_exit {
            set_at(&mut is_terminal_exit, b, true);
        }
    }

    // 5. Forward effective loop depth (pre-loop checks fall into loop preheaders) and
    //    `entry_depth` (identifies conditional inner-loop preheaders like `if ml > 32 { while }`,
    //    bounded to `MAX_ENTRY_DEPTH_DIST` forward hops from the inner loop header).
    let mut eff_depth: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut entry_depth: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for &d in &loop_depth {
        eff_depth.push(d);
        entry_depth.push(d);
    }
    let mut entry_dist: Vec<u8, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    for &b in rpo.iter().rev() {
        if at_bool(&is_cold, b) {
            continue;
        }
        let b_depth = at_u32(&loop_depth, b);
        let b_rank = at_usize(&rpo_rank, b);
        if b_depth == 0 {
            let mut max_d = 0u32;
            for_each_succ(view, b, |t| {
                if at_usize(&rpo_rank, t) > b_rank && !at_bool(&is_cold, t) {
                    max_d = max_d.max(at_u32(&eff_depth, t));
                }
            });
            set_at(&mut eff_depth, b, max_d);
        }
        if !at_bool(&is_loop_hdr, b) {
            let mut has_hot_succ = false;
            let mut all_enter_deeper = true;
            let mut min_d = u32::MAX;
            let mut max_dist = 0u8;
            for_each_succ(view, b, |t| {
                if at_bool(&is_cold, t) || (b_depth > 0 && at_bool(&is_terminal_exit, t)) {
                    return;
                }
                has_hot_succ = true;
                let td = at_u32(&entry_depth, t);
                let t_dist = at_u8(&entry_dist, t);
                if at_usize(&rpo_rank, t) <= b_rank
                    || td <= b_depth
                    || t_dist >= MAX_ENTRY_DEPTH_DIST
                {
                    all_enter_deeper = false;
                } else {
                    min_d = min_d.min(td);
                    max_dist = max_dist.max(t_dist);
                }
            });
            if has_hot_succ && all_enter_deeper && min_d != u32::MAX {
                set_at(&mut entry_depth, b, min_d);
                set_at(&mut entry_dist, b, max_dist.saturating_add(1));
            }
        }
    }

    // 6. Distinct reachable predecessor counts, forward hot successor flags, 1-hop `continue;`
    //    stubs, 1-hop conditional inner-loop guards, and rotatable multi-pred bottom-tested
    //    `Op::Br` loop latches (`pred_count >= 3`, call-free, `material_insts <= 4`, exits loop
    //    on other arm).
    let mut distinct_preds: Vec<u32, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut has_forward_succ: Vec<bool, Alloc<'a>> = Vec::from_elem_in(false, blocks, alloc);
    let mut pred_stamp: Vec<u32, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    for b in 0..blocks {
        if !at_bool(&reachable, b) {
            continue;
        }
        let mark = u32_of(b).saturating_add(1);
        let b_rank = at_usize(&rpo_rank, b);
        let mut fwd_hot = false;
        for_each_succ(view, b, |t| {
            if at_u32(&pred_stamp, t) != mark {
                set_at(&mut pred_stamp, t, mark);
                inc_sat_u32(&mut distinct_preds, t);
            }
            if !at_bool(&is_cold, t) && at_usize(&rpo_rank, t) > b_rank {
                fwd_hot = true;
            }
        });
        set_at(&mut has_forward_succ, b, fwd_hot);
    }

    // 6a. Detect 1-hop `continue;` stubs (`ip += 1; jump outer_hdr`): reachable non-cold non-header
    //     in-loop blocks with no forward hot successor whose terminator is an unconditional
    //     back-edge `Op::Jump` to a loop header at the same or enclosing loop depth.
    let mut is_continue_stub: Vec<bool, Alloc<'a>> = Vec::from_elem_in(false, blocks, alloc);
    for b in 0..blocks {
        if !at_bool(&reachable, b)
            || at_bool(&is_cold, b)
            || at_bool(&is_loop_hdr, b)
            || at_u32(&loop_depth, b) == 0
            || at_bool(&has_forward_succ, b)
        {
            continue;
        }
        let term = view.terminator(Block(u32_of(b)));
        if view.op(term) != Op::Jump {
            continue;
        }
        let mut ops = view.operands(term);
        let dst = to_idx(ops.target().block.0);
        if dst < blocks
            && at_bool(&is_loop_hdr, dst)
            && at_usize(&rpo_rank, dst) <= at_usize(&rpo_rank, b)
            && at_u32(&loop_depth, dst) <= at_u32(&loop_depth, b)
        {
            set_at(&mut is_continue_stub, b, true);
        }
    }

    // 6b. Detect 1-hop conditional inner-loop guards (`if 16 < lit_len { while_loop } goto join`):
    //     an in-loop `Op::Br` whose one arm enters a deeper loop (`entry_depth > s_depth`) and
    //     whose other arm is a non-cold, non-continue-stub same-depth bypass (`entry_depth == s_depth`).
    let mut guard_bypass: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    let mut guard_deep_depth: Vec<u32, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    for s in 0..blocks {
        if !at_bool(&reachable, s) || at_bool(&is_cold, s) {
            continue;
        }
        let s_depth = at_u32(&loop_depth, s);
        if s_depth == 0 {
            continue;
        }
        let term = view.terminator(Block(u32_of(s)));
        if view.op(term) != Op::Br {
            continue;
        }
        let mut ops = view.operands(term);
        let _cond = ops.value();
        let s_t = to_idx(ops.target().block.0);
        let s_e = to_idx(ops.target().block.0);
        if s_t == s_e || s_t >= blocks || s_e >= blocks {
            continue;
        }
        let st_ed = at_u32(&entry_depth, s_t);
        let se_ed = at_u32(&entry_depth, s_e);
        if st_ed > s_depth
            && se_ed == s_depth
            && !at_bool(&is_cold, s_e)
            && !at_bool(&is_continue_stub, s_e)
        {
            set_at(&mut guard_bypass, s, s_e);
            set_at(&mut guard_deep_depth, s, st_ed);
        } else if se_ed > s_depth
            && st_ed == s_depth
            && !at_bool(&is_cold, s_t)
            && !at_bool(&is_continue_stub, s_t)
        {
            set_at(&mut guard_bypass, s, s_t);
            set_at(&mut guard_deep_depth, s, se_ed);
        }
    }

    let mut rotatable_latch_hdr: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    for l in 1..blocks {
        if !at_bool(&reachable, l)
            || at_bool(&is_cold, l)
            || at_bool(&is_loop_hdr, l)
            || at_bool(&has_call, l)
            || at_u32(&material_insts, l) > MAX_ROTATABLE_LATCH_INSTS
            || at_u32(&distinct_preds, l) < MIN_ROTATABLE_LATCH_PREDS
        {
            continue;
        }
        let l_depth = at_u32(&loop_depth, l);
        if l_depth == 0 {
            continue;
        }
        let term = view.terminator(Block(u32_of(l)));
        if view.op(term) != Op::Br {
            continue;
        }
        let mut ops = view.operands(term);
        let _cond = ops.value();
        let t_blk = to_idx(ops.target().block.0);
        let e_blk = to_idx(ops.target().block.0);
        if t_blk >= blocks || e_blk >= blocks || t_blk == e_blk {
            continue;
        }
        let l_rank = at_usize(&rpo_rank, l);
        let is_cand_hdr = |cand: usize| -> bool {
            cand > 0
                && cand != l
                && !at_bool(&is_cold, cand)
                && at_bool(&is_loop_hdr, cand)
                && at_u32(&loop_depth, cand) == l_depth
                && at_usize(&rpo_rank, cand) <= l_rank
        };
        let (hdr, exit_blk) = match (is_cand_hdr(t_blk), is_cand_hdr(e_blk)) {
            (true, false) => (t_blk, e_blk),
            (false, true) => (e_blk, t_blk),
            _ => continue,
        };
        let exit_outside = at_u32(&loop_depth, exit_blk) < l_depth
            || at_bool(&is_cold, exit_blk)
            || at_bool(&is_terminal_exit, exit_blk);
        if exit_outside {
            set_at(&mut rotatable_latch_hdr, l, hdr);
        }
    }

    let mut hot_reachable_count = 0usize;
    for b in 0..blocks {
        if at_bool(&reachable, b) && !at_bool(&is_cold, b) {
            hot_reachable_count += 1;
        }
    }

    CfgInfo {
        blocks,
        hot_reachable_count,
        rpo,
        rpo_rank,
        reachable,
        is_cold,
        is_direct_exit,
        is_terminal_exit,
        loop_depth,
        is_loop_hdr,
        eff_depth,
        entry_depth,
        guard_bypass,
        guard_deep_depth,
        is_continue_stub,
        size_bytes,
        distinct_preds,
        has_forward_succ,
        jump_target,
        rotatable_latch_hdr,
    }
}

/// Stage-2 solved block frequencies and flat CSR edge weights.
struct FreqInfo<'a> {
    block_freq: Vec<u64, Alloc<'a>>,
    edge_off: Vec<usize, Alloc<'a>>,
    edge_dst: Vec<usize, Alloc<'a>>,
    edge_weight: Vec<u64, Alloc<'a>>,
}

/// Computes scaled `(then_prob, else_prob)` summing to [`PROB_SCALE`] for an `Op::Br` at block `b`.
fn branch_probs(
    cfg: &CfgInfo<'_>,
    b: usize,
    fl: Flags,
    t: usize,
    e: usize,
    branch_weights: Option<&dyn Fn(usize, usize) -> Option<(u64, u64)>>,
) -> (u64, u64) {
    // 1. Optional PGO `(taken, not_taken)` counts override static heuristics.
    if let Some(cb) = branch_weights
        && let Some((t_cnt, e_cnt)) = cb(b, t)
    {
        let total = u128::from(t_cnt) + u128::from(e_cnt);
        if total > 0 {
            let num = u128::from(t_cnt) * u128::from(PROB_SCALE) + (total / 2);
            let t_prob = u64::try_from(num / total)
                .unwrap_or(PROB_SCALE / 2)
                .min(PROB_SCALE);
            return (t_prob, PROB_SCALE - t_prob);
        }
    }

    // 2. Explicit branch expectation hints.
    if fl.contains(Flags::EXPECT_TRUE) {
        return (972, 52);
    }
    if fl.contains(Flags::EXPECT_FALSE) {
        return (52, 972);
    }

    // 3. Cold vs non-cold.
    let t_cold = at_bool(&cfg.is_cold, t);
    let e_cold = at_bool(&cfg.is_cold, e);
    if t_cold != e_cold {
        return if t_cold { (10, 1014) } else { (1014, 10) };
    }

    let b_depth = at_u32(&cfg.loop_depth, b);
    let b_rank = at_usize(&cfg.rpo_rank, b);
    let t_depth = at_u32(&cfg.loop_depth, t);
    let e_depth = at_u32(&cfg.loop_depth, e);
    let t_term = at_bool(&cfg.is_terminal_exit, t);
    let e_term = at_bool(&cfg.is_terminal_exit, e);

    // 4. Loop back-edge vs loop exit.
    let is_back = |s: usize, s_depth: u32| -> bool {
        at_usize(&cfg.rpo_rank, s) <= b_rank
            || (b_depth > 0 && at_bool(&cfg.is_loop_hdr, s) && s_depth == b_depth)
    };
    let is_exit = |s_depth: u32, s_term: bool| -> bool {
        (b_depth > 0 && s_depth < b_depth) || s_term
    };
    let t_back = is_back(t, t_depth);
    let e_back = is_back(e, e_depth);
    let t_exit = is_exit(t_depth, t_term);
    let e_exit = is_exit(e_depth, e_term);
    if t_back && e_exit {
        return (922, 102);
    }
    if e_back && t_exit {
        return (102, 922);
    }

    // 4b. Wu–Larus Loop-Branch Heuristic: when one target is a natural loop-header back-edge
    //     and the other is ALSO a backward edge (`t_back && e_back`) or a shared loop-closing tail
    //     (same loop depth, non-header, `distinct_preds >= 2`, no forward hot continuation — e.g.
    //     `fast_path -> (slow_tail, loop_hdr)` where `slow_tail -> jump(loop_hdr)`), strongly
    //     prefer the direct loop-header back-edge (`88%` vs `12%`). Never fire against a forward
    //     in-loop continuation like an early-`continue;` guard (`if !valid { continue outer_hdr }`).
    let t_hdr_back = b_depth > 0
        && at_bool(&cfg.is_loop_hdr, t)
        && t_depth >= b_depth
        && at_usize(&cfg.rpo_rank, t) <= b_rank;
    let e_hdr_back = b_depth > 0
        && at_bool(&cfg.is_loop_hdr, e)
        && e_depth >= b_depth
        && at_usize(&cfg.rpo_rank, e) <= b_rank;
    let is_back_or_shared_tail = |s: usize, s_depth: u32, s_back: bool| -> bool {
        s_back
            || (s_depth == b_depth
                && !at_bool(&cfg.is_loop_hdr, s)
                && at_u32(&cfg.distinct_preds, s) >= 2
                && !at_bool(&cfg.has_forward_succ, s))
    };
    if t_hdr_back && !e_hdr_back && is_back_or_shared_tail(e, e_depth, e_back) {
        return (900, 124);
    }
    if e_hdr_back && !t_hdr_back && is_back_or_shared_tail(t, t_depth, t_back) {
        return (124, 900);
    }

    // 5. Stay in loop vs exit loop.
    let t_stay = b_depth > 0 && t_depth >= b_depth && !t_term;
    let e_stay = b_depth > 0 && e_depth >= b_depth && !e_term;
    if t_stay && e_exit {
        return (900, 124);
    }
    if e_stay && t_exit {
        return (124, 900);
    }

    // 7. Deeper conditional inner loop (`entry_depth > b_depth`, or 1-hop inner-loop guard
    //    bypassing straight to the other arm) vs outer-loop spine (`!deeper && entry_depth ==
    //    b_depth && !term`): keep the outer spine as the preferred fallthrough. Ordered BEFORE
    //    Step 5b so `if ml <= 32 { fast; continue; } else { while_loop; continue; }` always
    //    prefers the outer fast path rather than treating `fast; continue` as a cold stub.
    if b_depth > 0 {
        let t_ed = at_u32(&cfg.entry_depth, t);
        let e_ed = at_u32(&cfg.entry_depth, e);
        let t_deeper = t_ed > b_depth
            || (at_u32(&cfg.guard_deep_depth, t) > b_depth
                && at_usize(&cfg.guard_bypass, t) == e);
        let e_deeper = e_ed > b_depth
            || (at_u32(&cfg.guard_deep_depth, e) > b_depth
                && at_usize(&cfg.guard_bypass, e) == t);
        let t_spine = !t_deeper && t_ed == b_depth && !t_term;
        let e_spine = !e_deeper && e_ed == b_depth && !e_term;
        if t_deeper && e_spine {
            return (256, 768);
        }
        if e_deeper && t_spine {
            return (768, 256);
        }
    }

    // 5b. Early-`continue;` guard inside a loop: when both targets stay in the loop and one is a
    //     forward in-loop continuation while the other is a direct back-edge or a 1-hop
    //     `continue;` stub (`is_continue_stub`, e.g. `if !valid_dist { ip += 1; continue; }`),
    //     favor continuing forward in the loop body (`68.75%` vs `31.25%`).
    if t_stay && e_stay {
        if !t_back && e_back {
            return (704, 320);
        }
        if !e_back && t_back {
            return (320, 704);
        }
        let t_stub = t_depth == b_depth && at_bool(&cfg.is_continue_stub, t);
        let e_stub = e_depth == b_depth && at_bool(&cfg.is_continue_stub, e);
        if !t_stub && e_stub {
            return (704, 320);
        }
        if !e_stub && t_stub {
            return (320, 704);
        }
    }

    // 6. Pre-loop (`depth == 0`) enter loop vs non-loop / early exit.
    if b_depth == 0 {
        let t_loop = (t_depth > 0 || at_u32(&cfg.eff_depth, t) > 0) && !t_term;
        let e_loop = (e_depth > 0 || at_u32(&cfg.eff_depth, e) > 0) && !e_term;
        if t_loop != e_loop {
            return if t_loop { (900, 124) } else { (124, 900) };
        }
    }

    // 8. At depth 0, prefer continuing work over an immediate `Return`/`TailCall` early exit.
    if b_depth == 0 {
        let t_direct = at_bool(&cfg.is_direct_exit, t);
        let e_direct = at_bool(&cfg.is_direct_exit, e);
        if t_direct != e_direct {
            return if t_direct { (256, 768) } else { (768, 256) };
        }
    }

    // 8b. Compact Fast-Path Diamond Heuristic (Wu–Larus / LLVM BPI):
    //     When both arms are non-cold, at the same loop depth, neither is a loop header or exit,
    //     BOTH arms form a true diamond terminating in `Op::Jump` to the exact same merge block
    //     `m` (excluding `if/else if` cascades where the `else if` block is a tiny `Op::Br`),
    //     and one arm is a compact fast path (`<= 24 B`, i.e., `<= 6` post-isel insts) while the
    //     other arm is substantially larger (`>= 16 B` and `>= size_fast + 8 B`, i.e., `>= 2`
    //     post-isel insts larger — e.g. `if ml < 128 { fast_lut } else { slow_clz }`), strongly
    //     favor the compact fast-path arm (`740` vs `284`) so Ext-TSP places `H -> fast -> M`
    //     straight-through with `slow` out-of-line.
    let jt = at_usize(&cfg.jump_target, t);
    let je = at_usize(&cfg.jump_target, e);
    if !t_cold
        && !e_cold
        && t_depth == b_depth
        && e_depth == b_depth
        && !t_exit
        && !e_exit
        && !at_bool(&cfg.is_loop_hdr, t)
        && !at_bool(&cfg.is_loop_hdr, e)
        && at_bool(&cfg.has_forward_succ, t) == at_bool(&cfg.has_forward_succ, e)
        && jt != usize::MAX
        && jt == je
    {
        let size_t = at_u32(&cfg.size_bytes, t);
        let size_e = at_u32(&cfg.size_bytes, e);
        if size_t <= COMPACT_FAST_PATH_MAX_BYTES
            && size_e >= COMPACT_FAST_PATH_MIN_SLOW_BYTES
            && size_e >= size_t.saturating_add(COMPACT_FAST_PATH_MIN_DIFF_BYTES)
        {
            return (740, 284);
        }
        if size_e <= COMPACT_FAST_PATH_MAX_BYTES
            && size_t >= COMPACT_FAST_PATH_MIN_SLOW_BYTES
            && size_t >= size_e.saturating_add(COMPACT_FAST_PATH_MIN_DIFF_BYTES)
        {
            return (284, 740);
        }
    }

    // 9. General hot/hot diamond: deterministically favor the `then` arm
    //    (`if common { ... } else { ... }`) instead of letting arbitrary RPO merge order flip
    //    equal-size 50/50 diamonds backwards.
    (520, 504)
}

/// Collects coalesced `(dst, prob)` outgoing edges for `block` into `out` (in deterministic
/// first-seen order), using `edge_slot` as an O(out_degree) deduplication scratch table.
fn collect_block_edge_probs<'a>(
    view: &View<'_>,
    cfg: &CfgInfo<'_>,
    block: usize,
    branch_weights: Option<&dyn Fn(usize, usize) -> Option<(u64, u64)>>,
    edge_slot: &mut [usize],
    raw_targets: &mut Vec<usize, Alloc<'a>>,
    out: &mut Vec<(usize, u64), Alloc<'a>>,
) {
    out.clear();
    let blocks = cfg.blocks;
    let term = view.terminator(Block(u32_of(block)));
    let op = view.op(term);
    let fl = Flags(at_u8(view.flags, term));

    let push_prob = |dst: usize, prob: u64, slot_tbl: &mut [usize], edges: &mut Vec<(usize, u64), Alloc<'a>>| {
        if dst >= blocks {
            return;
        }
        let existing = at_usize(slot_tbl, dst);
        if existing == usize::MAX {
            let idx = edges.len();
            set_at(slot_tbl, dst, idx);
            edges.push((dst, prob));
        } else if let Some(entry) = edges.get_mut(existing) {
            entry.1 = entry.1.saturating_add(prob).min(PROB_SCALE);
        }
    };

    let mut ops = view.operands(term);
    match op {
        Op::Jump => {
            let t = to_idx(ops.target().block.0);
            push_prob(t, PROB_SCALE, edge_slot, out);
        }
        Op::Invoke => {
            let _callee = ops.reference();
            let _sig = ops.sig();
            let normal = to_idx(ops.block().0);
            let unwind = to_idx(ops.block().0);
            push_prob(normal, 1014, edge_slot, out);
            push_prob(unwind, 10, edge_slot, out);
        }
        Op::InvokeIndirect => {
            let _callee = ops.value();
            let _sig = ops.sig();
            let normal = to_idx(ops.block().0);
            let unwind = to_idx(ops.block().0);
            push_prob(normal, 1014, edge_slot, out);
            push_prob(unwind, 10, edge_slot, out);
        }
        Op::Br => {
            let _cond = ops.value();
            let t = to_idx(ops.target().block.0);
            let e = to_idx(ops.target().block.0);
            if t == e {
                push_prob(t, PROB_SCALE, edge_slot, out);
            } else if t < blocks && e < blocks {
                let (tp, ep) = branch_probs(cfg, block, fl, t, e, branch_weights);
                push_prob(t, tp, edge_slot, out);
                push_prob(e, ep, edge_slot, out);
            } else {
                if t < blocks {
                    push_prob(t, PROB_SCALE, edge_slot, out);
                }
                if e < blocks {
                    push_prob(e, PROB_SCALE, edge_slot, out);
                }
            }
        }
        Op::Switch => {
            raw_targets.clear();
            let scrut = ops.value();
            let def = to_idx(ops.target().block.0);
            if def < blocks {
                raw_targets.push(def);
            }
            for (_, tgt) in ops.cases(scrut) {
                let dst = to_idx(tgt.block.0);
                if dst < blocks {
                    raw_targets.push(dst);
                }
            }
            let total_arms = raw_targets.len();
            if total_arms > 0 {
                let mut hot_arms: u64 = 0;
                let mut cold_arms: u64 = 0;
                for &dst in raw_targets.iter() {
                    if at_bool(&cfg.is_cold, dst) {
                        cold_arms += 1;
                    } else {
                        hot_arms += 1;
                    }
                }
                if hot_arms == 0 {
                    let total_u64 = u64::try_from(total_arms).unwrap_or(1).max(1);
                    let base = (PROB_SCALE / total_u64).max(1);
                    let rem = PROB_SCALE % total_u64;
                    for (i, &dst) in raw_targets.iter().enumerate() {
                        let extra = u64::from(u64::try_from(i).unwrap_or(u64::MAX) < rem);
                        push_prob(dst, base + extra, edge_slot, out);
                    }
                } else {
                    let cold_budget = cold_arms.min(PROB_SCALE / 2);
                    let hot_budget = PROB_SCALE - cold_budget;
                    let base = (hot_budget / hot_arms).max(1);
                    let rem = hot_budget % hot_arms;
                    let mut hot_seen: u64 = 0;
                    for &dst in raw_targets.iter() {
                        if at_bool(&cfg.is_cold, dst) {
                            push_prob(dst, 1, edge_slot, out);
                        } else {
                            let extra = u64::from(hot_seen < rem);
                            hot_seen += 1;
                            push_prob(dst, base + extra, edge_slot, out);
                        }
                    }
                }
            }
        }
        Op::AsmBr => {
            raw_targets.clear();
            let _asm = ops.asm();
            let fall = to_idx(ops.block().0);
            if fall < blocks {
                raw_targets.push(fall);
            }
            for &lbl in ops.blocks() {
                let dst = to_idx(lbl);
                if dst < blocks {
                    raw_targets.push(dst);
                }
            }
            let n = u64::try_from(raw_targets.len()).unwrap_or(0);
            if n > 0 {
                let base = (PROB_SCALE / n).max(1);
                let rem = PROB_SCALE % n;
                for (i, &dst) in raw_targets.iter().enumerate() {
                    let extra = u64::from(u64::try_from(i).unwrap_or(u64::MAX) < rem);
                    push_prob(dst, base + extra, edge_slot, out);
                }
            }
        }
        Op::Suspend => {
            let _imm = ops.imm();
            let a = to_idx(ops.block().0);
            let b_tgt = to_idx(ops.block().0);
            if a == b_tgt {
                push_prob(a, PROB_SCALE, edge_slot, out);
            } else {
                push_prob(a, PROB_SCALE / 2, edge_slot, out);
                push_prob(b_tgt, PROB_SCALE / 2, edge_slot, out);
            }
        }
        _ => {}
    }

    for &(dst, _) in out.iter() {
        set_at(edge_slot, dst, usize::MAX);
    }
}

/// Stage 2: Solves per-block execution frequencies in a single O(V + E) RPO forward pass and builds
/// the flat CSR of outgoing edge weights `(edge_off, edge_dst, edge_weight)`.
fn solve_frequencies_and_edges<'a>(
    view: &View<'_>,
    cfg: &CfgInfo<'_>,
    branch_weights: Option<&dyn Fn(usize, usize) -> Option<(u64, u64)>>,
    alloc: Alloc<'a>,
) -> FreqInfo<'a> {
    let blocks = cfg.blocks;

    // 1. Build coalesced outgoing edge probabilities in CSR order `0..blocks`.
    let mut edge_off: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks + 1, alloc);
    let mut edge_dst: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks.saturating_mul(2), alloc);
    let mut edge_prob: Vec<u64, Alloc<'a>> = Vec::with_capacity_in(blocks.saturating_mul(2), alloc);

    let mut edge_slot: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    let mut raw_targets: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut tmp_edges: Vec<(usize, u64), Alloc<'a>> = Vec::new_in(alloc);

    for b in 0..blocks {
        edge_off.push(edge_dst.len());
        if !at_bool(&cfg.reachable, b) {
            continue;
        }
        collect_block_edge_probs(
            view,
            cfg,
            b,
            branch_weights,
            &mut edge_slot,
            &mut raw_targets,
            &mut tmp_edges,
        );
        for &(dst, prob) in &tmp_edges {
            edge_dst.push(dst);
            edge_prob.push(prob);
        }
    }
    edge_off.push(edge_dst.len());

    // 2. Propagate `block_freq` forward in RPO order:
    //    - Entry `0` starts at `ENTRY_FREQ` (`1024`).
    //    - Each natural loop header multiplies its accumulated preheader inflow by `LOOP_TRIP_MULT`
    //      (`10`), clamped safely at `MAX_FREQ` (`1 << 40`) so `u64` never overflows.
    //    - Forward edges propagate `(freq[u] * prob) / 1024` (at least `1` for hot edges, `0` for
    //      cold edges); back-edges record their exact `edge_weight` without re-adding to headers.
    let mut block_freq: Vec<u64, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut edge_weight: Vec<u64, Alloc<'a>> = Vec::from_elem_in(0, edge_dst.len(), alloc);
    set_at(&mut block_freq, 0, ENTRY_FREQ);

    for &b in &cfg.rpo {
        if !at_bool(&cfg.reachable, b) {
            continue;
        }
        let b_cold = at_bool(&cfg.is_cold, b);
        let mut f = at_u64(&block_freq, b);
        if b_cold {
            f = f.min(10);
            set_at(&mut block_freq, b, f);
            continue;
        }
        if at_bool(&cfg.is_loop_hdr, b) {
            f = f.max(1).saturating_mul(LOOP_TRIP_MULT).min(MAX_FREQ);
            set_at(&mut block_freq, b, f);
        } else if f > MAX_FREQ {
            f = MAX_FREQ;
            set_at(&mut block_freq, b, f);
        }

        let b_rank = at_usize(&cfg.rpo_rank, b);
        let start = at_usize(&edge_off, b);
        let end = at_usize(&edge_off, b + 1);
        for ei in start..end {
            let dst = at_usize(&edge_dst, ei);
            if at_bool(&cfg.is_cold, dst) {
                set_at(&mut edge_weight, ei, 0);
                continue;
            }
            let prob = at_u64(&edge_prob, ei);
            let w_raw = u64::try_from((u128::from(f) * u128::from(prob)) / u128::from(PROB_SCALE))
                .unwrap_or(MAX_FREQ);
            let w = w_raw.clamp(1, MAX_FREQ);
            set_at(&mut edge_weight, ei, w);

            // Propagate only across forward RPO edges so back-edges do not double-count headers.
            if at_usize(&cfg.rpo_rank, dst) > b_rank {
                if let Some(slot) = block_freq.get_mut(dst) {
                    *slot = slot.saturating_add(w).min(MAX_FREQ);
                }
            }
        }
    }

    FreqInfo { block_freq, edge_off, edge_dst, edge_weight }
}

/// Newell–Pupyrev 10-way split/rotate chain-merge candidates for an edge `(u -> v)` connecting
/// chain `X` (`u` in `X`, split after `u` into `X1 | X2`) and chain `Y` (`v` in `Y`, split before
/// `v` into `Y1 | Y2`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum MergeMode {
    XY = 0,
    YX = 1,
    X1YX2 = 2,
    X2X1Y = 3,
    X2YX1 = 4,
    YX2X1 = 5,
    XY2Y1 = 6,
    Y1XY2 = 7,
    Y2Y1X = 8,
    Y2XY1 = 9,
}

const ALL_MERGE_MODES: [MergeMode; 10] = [
    MergeMode::XY,
    MergeMode::YX,
    MergeMode::X1YX2,
    MergeMode::X2X1Y,
    MergeMode::X2YX1,
    MergeMode::YX2X1,
    MergeMode::XY2Y1,
    MergeMode::Y1XY2,
    MergeMode::Y2Y1X,
    MergeMode::Y2XY1,
];

/// Version-stamped cached Ext-TSP merge evaluation for one active cross-chain hot edge `(u -> v)`.
#[derive(Clone, Copy)]
struct EdgeCand {
    u: usize,
    v: usize,
    cached_cx: usize,
    cached_cy: usize,
    stamp_x: u32,
    stamp_y: u32,
    best_gain: i128,
    best_mode: MergeMode,
}

/// Materializes an intrusive linked chain starting at `head` into `out`.
fn materialize_chain<'a>(
    head: usize,
    next_blk: &[usize],
    out: &mut Vec<usize, Alloc<'a>>,
) {
    out.clear();
    let mut cur = head;
    let limit = next_blk.len();
    let mut steps = 0usize;
    while cur != usize::MAX && steps < limit {
        out.push(cur);
        cur = at_usize(next_blk, cur);
        steps += 1;
    }
}

/// Checks whether `mode` is valid given entry-pinning (`0` must stay at index `0`) and split bounds.
#[inline]
fn merge_mode_valid(
    mode: MergeMode,
    x_has_entry: bool,
    y_has_entry: bool,
    has_split_x: bool,
    has_split_y: bool,
    allow_split: bool,
) -> bool {
    match mode {
        MergeMode::XY => !y_has_entry,
        MergeMode::YX => !x_has_entry,
        MergeMode::X1YX2 => allow_split && has_split_x && !y_has_entry,
        MergeMode::X2X1Y | MergeMode::X2YX1 => {
            allow_split && has_split_x && !x_has_entry && !y_has_entry
        }
        MergeMode::YX2X1 => allow_split && has_split_x && !x_has_entry,
        MergeMode::XY2Y1 => allow_split && has_split_y && !y_has_entry,
        MergeMode::Y1XY2 => allow_split && has_split_y && !x_has_entry,
        MergeMode::Y2Y1X | MergeMode::Y2XY1 => {
            allow_split && has_split_y && !x_has_entry && !y_has_entry
        }
    }
}

/// Builds the merged block sequence for `mode` into `cand`.
fn build_merge_cand<'a>(
    mode: MergeMode,
    bx: &[usize],
    by: &[usize],
    split_x: usize,
    split_y: usize,
    cand: &mut Vec<usize, Alloc<'a>>,
) {
    cand.clear();
    let (x1, x2) = if split_x <= bx.len() {
        bx.split_at(split_x)
    } else {
        (bx, &[][..])
    };
    let (y1, y2) = if split_y <= by.len() {
        by.split_at(split_y)
    } else {
        (by, &[][..])
    };
    match mode {
        MergeMode::XY => {
            cand.extend_from_slice(bx);
            cand.extend_from_slice(by);
        }
        MergeMode::YX => {
            cand.extend_from_slice(by);
            cand.extend_from_slice(bx);
        }
        MergeMode::X1YX2 => {
            cand.extend_from_slice(x1);
            cand.extend_from_slice(by);
            cand.extend_from_slice(x2);
        }
        MergeMode::X2X1Y => {
            cand.extend_from_slice(x2);
            cand.extend_from_slice(x1);
            cand.extend_from_slice(by);
        }
        MergeMode::X2YX1 => {
            cand.extend_from_slice(x2);
            cand.extend_from_slice(by);
            cand.extend_from_slice(x1);
        }
        MergeMode::YX2X1 => {
            cand.extend_from_slice(by);
            cand.extend_from_slice(x2);
            cand.extend_from_slice(x1);
        }
        MergeMode::XY2Y1 => {
            cand.extend_from_slice(bx);
            cand.extend_from_slice(y2);
            cand.extend_from_slice(y1);
        }
        MergeMode::Y1XY2 => {
            cand.extend_from_slice(y1);
            cand.extend_from_slice(bx);
            cand.extend_from_slice(y2);
        }
        MergeMode::Y2Y1X => {
            cand.extend_from_slice(y2);
            cand.extend_from_slice(y1);
            cand.extend_from_slice(bx);
        }
        MergeMode::Y2XY1 => {
            cand.extend_from_slice(y2);
            cand.extend_from_slice(bx);
            cand.extend_from_slice(y1);
        }
    }
}

/// Computes the exact Newell–Pupyrev Ext-TSP score of block sequence `seq` in `O(|seq| + |E(seq)|)`
/// using reusable scratch arrays `block_pos` (restored to `usize::MAX` on exit) and `block_off`.
fn score_sequence(
    seq: &[usize],
    cfg: &CfgInfo<'_>,
    freq: &FreqInfo<'_>,
    block_pos: &mut [usize],
    block_off: &mut [u64],
) -> u128 {
    let mut cur_off = 0u64;
    for (pos, &b) in seq.iter().enumerate() {
        set_at(block_pos, b, pos);
        set_at(block_off, b, cur_off);
        cur_off = cur_off.saturating_add(u64::from(at_u32(&cfg.size_bytes, b)));
    }

    let mut total: u128 = 0;
    for (pos_u, &u) in seq.iter().enumerate() {
        let start = at_usize(&freq.edge_off, u);
        let end = at_usize(&freq.edge_off, u + 1);
        let off_u = at_u64(block_off, u);
        let end_u = off_u.saturating_add(u64::from(at_u32(&cfg.size_bytes, u)));
        let u_rank = at_usize(&cfg.rpo_rank, u);
        let u_rot_hdr = at_usize(&cfg.rotatable_latch_hdr, u);

        for ei in start..end {
            let w = at_u64(&freq.edge_weight, ei);
            if w == 0 {
                continue;
            }
            let v = at_usize(&freq.edge_dst, ei);
            if v == u {
                continue;
            }
            let pos_v = at_usize(block_pos, v);
            if pos_v == usize::MAX {
                continue;
            }
            let is_non_rotatable_backedge =
                at_bool(&cfg.is_loop_hdr, v) && at_usize(&cfg.rpo_rank, v) <= u_rank && u_rot_hdr != v;
            if is_non_rotatable_backedge && pos_v > pos_u {
                // Never incentivize placing a non-rotatable loop body/latch BEFORE its loop header
                // (neither as fallthrough nor as a forward short jump).
                continue;
            }
            let w128 = u128::from(w);
            if pos_v == pos_u + 1 {
                total = total.saturating_add(w128 * EXT_TSP_FALLTHROUGH_WEIGHT);
            } else if pos_v > pos_u + 1 {
                let off_v = at_u64(block_off, v);
                let dist = off_v.saturating_sub(end_u);
                if dist < EXT_TSP_FWD_LIMIT {
                    let bonus = (w128 * EXT_TSP_JUMP_WEIGHT * u128::from(EXT_TSP_FWD_LIMIT - dist))
                        / u128::from(EXT_TSP_FWD_LIMIT);
                    total = total.saturating_add(bonus);
                }
            } else {
                let off_v = at_u64(block_off, v);
                let dist = end_u.saturating_sub(off_v);
                if dist < EXT_TSP_BWD_LIMIT {
                    let bonus = (w128 * EXT_TSP_JUMP_WEIGHT * u128::from(EXT_TSP_BWD_LIMIT - dist))
                        / u128::from(EXT_TSP_BWD_LIMIT);
                    total = total.saturating_add(bonus);
                }
            }
        }
    }

    for &b in seq {
        set_at(block_pos, b, usize::MAX);
    }
    total
}

/// Evaluates all valid Ext-TSP merge modes for a cross-chain edge and returns the highest
/// `(delta_score, best_mode)` (with `delta_score <= 0` when no positive-gain merge exists).
fn evaluate_edge_merge<'a>(
    bx: &[usize],
    by: &[usize],
    split_x: usize,
    split_y: usize,
    base_score: u128,
    cfg: &CfgInfo<'_>,
    freq: &FreqInfo<'_>,
    cand: &mut Vec<usize, Alloc<'a>>,
    block_pos: &mut [usize],
    block_off: &mut [u64],
) -> (i128, MergeMode) {
    let x_has_entry = bx.first().copied() == Some(0);
    let y_has_entry = by.first().copied() == Some(0);
    let has_split_x = split_x > 0 && split_x < bx.len();
    let has_split_y = split_y > 0 && split_y < by.len();
    let allow_split = bx.len().saturating_add(by.len()) <= MAX_SPLIT_CHAIN_LEN;

    let mut best_gain = i128::MIN;
    let mut best_mode = MergeMode::XY;

    let base_i128 = i128::try_from(base_score).unwrap_or(i128::MAX);
    for mode in ALL_MERGE_MODES {
        if !merge_mode_valid(mode, x_has_entry, y_has_entry, has_split_x, has_split_y, allow_split) {
            continue;
        }
        build_merge_cand(mode, bx, by, split_x, split_y, cand);
        let s = score_sequence(cand.as_slice(), cfg, freq, block_pos, block_off);
        let gain = i128::try_from(s).unwrap_or(i128::MAX).saturating_sub(base_i128);
        if gain > best_gain {
            best_gain = gain;
            best_mode = mode;
        }
    }
    (best_gain, best_mode)
}

/// Stage 3 (primary): Newell–Pupyrev Ext-TSP basic-block chain merging for functions with at most
/// [`MAX_EXT_TSP_BLOCKS`] hot reachable blocks.
fn ext_tsp_order<'a>(
    cfg: &CfgInfo<'_>,
    freq: &FreqInfo<'_>,
    alloc: Alloc<'a>,
) -> Vec<usize, Alloc<'a>> {
    let blocks = cfg.blocks;

    // Initialize flat SoA intrusive singleton chains for every hot reachable block.
    let mut chain_head: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    let mut next_blk: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    let mut chain_of: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    let mut pos_in_chain: Vec<usize, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut chain_ver: Vec<u32, Alloc<'a>> = Vec::from_elem_in(1, blocks, alloc);
    let mut chain_score: Vec<u128, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut chain_min_rpo: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    let mut chain_max_freq: Vec<u64, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);
    let mut chain_all_term_exit: Vec<bool, Alloc<'a>> = Vec::from_elem_in(false, blocks, alloc);

    for b in 0..blocks {
        if at_bool(&cfg.reachable, b) && !at_bool(&cfg.is_cold, b) {
            set_at(&mut chain_head, b, b);
            set_at(&mut chain_of, b, b);
            set_at(&mut pos_in_chain, b, 0);
            set_at(&mut chain_min_rpo, b, at_usize(&cfg.rpo_rank, b));
            set_at(&mut chain_max_freq, b, at_u64(&freq.block_freq, b));
            set_at(
                &mut chain_all_term_exit,
                b,
                at_u32(&cfg.loop_depth, b) == 0 && at_bool(&cfg.is_terminal_exit, b),
            );
        }
    }

    // Collect active cross-chain hot edges, deferring edges into depth-0 direct function exits
    // when the function contains a loop so `ret` epilogues are never interleaved into loop bodies.
    let has_any_loop = (0..blocks).any(|b| at_u32(&cfg.loop_depth, b) > 0);
    let mut active_edges: Vec<EdgeCand, Alloc<'a>> = Vec::with_capacity_in(freq.edge_dst.len(), alloc);
    for u in 0..blocks {
        if at_usize(&chain_of, u) == usize::MAX {
            continue;
        }
        let u_depth = at_u32(&cfg.loop_depth, u);
        let start = at_usize(&freq.edge_off, u);
        let end = at_usize(&freq.edge_off, u + 1);
        for ei in start..end {
            let w = at_u64(&freq.edge_weight, ei);
            if w == 0 {
                continue;
            }
            let v = at_usize(&freq.edge_dst, ei);
            if v == u || at_usize(&chain_of, v) == usize::MAX {
                continue;
            }
            if has_any_loop
                && !at_bool(&cfg.is_terminal_exit, u)
                && at_u32(&cfg.loop_depth, v) == 0
                && (u_depth > 0 && at_bool(&cfg.is_terminal_exit, v) || at_bool(&cfg.is_direct_exit, v))
            {
                continue;
            }
            active_edges.push(EdgeCand {
                u,
                v,
                cached_cx: usize::MAX,
                cached_cy: usize::MAX,
                stamp_x: 0,
                stamp_y: 0,
                best_gain: 0,
                best_mode: MergeMode::XY,
            });
        }
    }

    let mut buf_x: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks.min(128), alloc);
    let mut buf_y: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks.min(128), alloc);
    let mut cand: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks.min(256), alloc);
    let mut block_pos: Vec<usize, Alloc<'a>> = Vec::from_elem_in(usize::MAX, blocks, alloc);
    let mut block_off: Vec<u64, Alloc<'a>> = Vec::from_elem_in(0, blocks, alloc);

    for _round in 0..MAX_MERGES {
        let mut best_edge_idx = usize::MAX;
        let mut best_key: Option<(i128, Reverse<usize>, Reverse<usize>, Reverse<MergeMode>, Reverse<usize>, Reverse<usize>)> = None;

        let mut write = 0usize;
        let len = active_edges.len();
        for read in 0..len {
            let Some(mut edge) = active_edges.get(read).copied() else {
                break;
            };
            let cx = at_usize(&chain_of, edge.u);
            let cy = at_usize(&chain_of, edge.v);
            if cx == cy || cx == usize::MAX || cy == usize::MAX {
                continue;
            }
            let vx = at_u32(&chain_ver, cx);
            let vy = at_u32(&chain_ver, cy);
            if edge.cached_cx != cx || edge.cached_cy != cy || edge.stamp_x != vx || edge.stamp_y != vy {
                materialize_chain(at_usize(&chain_head, cx), &next_blk, &mut buf_x);
                materialize_chain(at_usize(&chain_head, cy), &next_blk, &mut buf_y);
                let split_x = at_usize(&pos_in_chain, edge.u).saturating_add(1);
                let split_y = at_usize(&pos_in_chain, edge.v);
                let base_score = at_u128(&chain_score, cx).saturating_add(at_u128(&chain_score, cy));
                let (gain, mode) = evaluate_edge_merge(
                    buf_x.as_slice(),
                    buf_y.as_slice(),
                    split_x,
                    split_y,
                    base_score,
                    cfg,
                    freq,
                    &mut cand,
                    &mut block_pos,
                    &mut block_off,
                );
                edge.cached_cx = cx;
                edge.cached_cy = cy;
                edge.stamp_x = vx;
                edge.stamp_y = vy;
                edge.best_gain = gain;
                edge.best_mode = mode;
            }
            set_at(&mut active_edges, write, edge);
            if edge.best_gain > 0 {
                let ru = at_usize(&cfg.rpo_rank, edge.u);
                let rv = at_usize(&cfg.rpo_rank, edge.v);
                let key = (
                    edge.best_gain,
                    Reverse(ru.min(rv)),
                    Reverse(ru.max(rv)),
                    Reverse(edge.best_mode),
                    Reverse(edge.u),
                    Reverse(edge.v),
                );
                if best_key.is_none_or(|cur| key > cur) {
                    best_key = Some(key);
                    best_edge_idx = write;
                }
            }
            write += 1;
        }
        active_edges.truncate(write);

        if best_edge_idx == usize::MAX {
            break;
        }

        let Some(chosen) = active_edges.get(best_edge_idx).copied() else {
            break;
        };
        let cx = at_usize(&chain_of, chosen.u);
        let cy = at_usize(&chain_of, chosen.v);
        if cx == cy || cx == usize::MAX || cy == usize::MAX {
            continue;
        }
        materialize_chain(at_usize(&chain_head, cx), &next_blk, &mut buf_x);
        materialize_chain(at_usize(&chain_head, cy), &next_blk, &mut buf_y);
        let split_x = at_usize(&pos_in_chain, chosen.u).saturating_add(1);
        let split_y = at_usize(&pos_in_chain, chosen.v);
        build_merge_cand(chosen.best_mode, buf_x.as_slice(), buf_y.as_slice(), split_x, split_y, &mut cand);
        let new_score = score_sequence(cand.as_slice(), cfg, freq, &mut block_pos, &mut block_off);

        // Keep `cx` as the merged chain identifier (or `cy` if `cy == 0` so entry stays on chain 0).
        let keep = if cy == 0 { cy } else { cx };
        let drop = if keep == cx { cy } else { cx };

        if let Some(&first_blk) = cand.first() {
            set_at(&mut chain_head, keep, first_blk);
        }
        set_at(&mut chain_score, keep, new_score);
        let merged_min_rpo = at_usize(&chain_min_rpo, keep).min(at_usize(&chain_min_rpo, drop));
        let merged_max_freq = at_u64(&chain_max_freq, keep).max(at_u64(&chain_max_freq, drop));
        let merged_all_term_exit = at_bool(&chain_all_term_exit, keep) && at_bool(&chain_all_term_exit, drop);
        set_at(&mut chain_min_rpo, keep, merged_min_rpo);
        set_at(&mut chain_max_freq, keep, merged_max_freq);
        set_at(&mut chain_all_term_exit, keep, merged_all_term_exit);
        if let Some(ver) = chain_ver.get_mut(keep) {
            *ver = ver.wrapping_add(1).max(1);
        }

        for (pos, &blk) in cand.iter().enumerate() {
            set_at(&mut chain_of, blk, keep);
            set_at(&mut pos_in_chain, blk, pos);
            let nxt = cand.get(pos + 1).copied().unwrap_or(usize::MAX);
            set_at(&mut next_blk, blk, nxt);
        }
        set_at(&mut chain_head, drop, usize::MAX);
        set_at(&mut chain_score, drop, 0);
    }

    // Order final hot chains:
    // 1. Chain containing entry block `0` first.
    // 2. Remaining non-terminal-exit hot chains sorted by `(Reverse(max_block_freq), min_rpo, head)`.
    // 3. Remaining pure terminal-exit hot chains sorted by `(Reverse(max_block_freq), min_rpo, head)`.
    // 4. Unreachable non-cold blocks in RPO order.
    // 5. Cold blocks in RPO order.
    let entry_chain = at_usize(&chain_of, 0);
    let mut rem_chains: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    for c in 0..blocks {
        if c != entry_chain && at_usize(&chain_head, c) != usize::MAX {
            rem_chains.push(c);
        }
    }
    rem_chains.sort_unstable_by_key(|&c| {
        (
            at_bool(&chain_all_term_exit, c),
            Reverse(at_u64(&chain_max_freq, c)),
            at_usize(&chain_min_rpo, c),
            c,
        )
    });

    let mut order: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut placed: Vec<bool, Alloc<'a>> = Vec::from_elem_in(false, blocks, alloc);

    let emit_chain = |c: usize,
                      order: &mut Vec<usize, Alloc<'a>>,
                      placed: &mut [bool]| {
        let mut cur = at_usize(&chain_head, c);
        let mut steps = 0usize;
        while cur != usize::MAX && steps < blocks {
            if !at_bool(placed, cur) {
                set_at(placed, cur, true);
                order.push(cur);
            }
            cur = at_usize(&next_blk, cur);
            steps += 1;
        }
    };

    if entry_chain != usize::MAX {
        emit_chain(entry_chain, &mut order, &mut placed);
    }
    for &c in &rem_chains {
        emit_chain(c, &mut order, &mut placed);
    }
    for &b in &cfg.rpo {
        if !at_bool(&placed, b) && !at_bool(&cfg.is_cold, b) {
            set_at(&mut placed, b, true);
            order.push(b);
        }
    }
    for &b in &cfg.rpo {
        if !at_bool(&placed, b) {
            set_at(&mut placed, b, true);
            order.push(b);
        }
    }

    order
}

/// Stage 3 (mega-function fallback for `hot_reachable_count > MAX_EXT_TSP_BLOCKS`): deterministic
/// `O(V log V + E)` frequency-weighted greedy trace layout.
fn greedy_trace_order<'a>(
    cfg: &CfgInfo<'_>,
    freq: &FreqInfo<'_>,
    alloc: Alloc<'a>,
) -> Vec<usize, Alloc<'a>> {
    let blocks = cfg.blocks;
    let mut placed: Vec<bool, Alloc<'a>> = Vec::from_elem_in(false, blocks, alloc);
    let mut order: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);

    let pick_next = |u: usize, placed: &[bool]| -> Option<usize> {
        let u_depth = at_u32(&cfg.loop_depth, u);
        let start = at_usize(&freq.edge_off, u);
        let end = at_usize(&freq.edge_off, u + 1);
        let mut best: Option<usize> = None;
        let mut best_key: Option<(u8, u64, Reverse<usize>, Reverse<usize>)> = None;
        for ei in start..end {
            let v = at_usize(&freq.edge_dst, ei);
            if v >= blocks || v == u || at_bool(placed, v) || at_bool(&cfg.is_cold, v) {
                continue;
            }
            let v_depth = at_u32(&cfg.loop_depth, v);
            let v_term = at_bool(&cfg.is_terminal_exit, v);
            if u_depth > 0 && v_depth == 0 && v_term {
                continue;
            }
            let w = at_u64(&freq.edge_weight, ei);
            if w == 0 {
                continue;
            }
            let same_or_deeper = u8::from(u_depth == 0 || v_depth >= u_depth);
            let key = (same_or_deeper, w, Reverse(at_usize(&cfg.rpo_rank, v)), Reverse(v));
            if best_key.is_none_or(|cur| key > cur) {
                best_key = Some(key);
                best = Some(v);
            }
        }
        best
    };

    let extend_trace = |seed: usize, placed: &mut [bool], order: &mut Vec<usize, Alloc<'a>>| {
        let mut cur = Some(seed);
        while let Some(b) = cur {
            if at_bool(placed, b) {
                break;
            }
            set_at(placed, b, true);
            order.push(b);
            cur = pick_next(b, placed);
        }
    };

    // 1. Entry trace starting from block `0`.
    extend_trace(0, &mut placed, &mut order);

    // 2. Remaining hot reachable seeds ordered by `(is_pure_term_exit, Reverse(block_freq), rpo)`.
    let mut seeds: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(cfg.hot_reachable_count, alloc);
    for &b in &cfg.rpo {
        if at_bool(&cfg.reachable, b) && !at_bool(&cfg.is_cold, b) && !at_bool(&placed, b) {
            seeds.push(b);
        }
    }
    seeds.sort_unstable_by_key(|&b| {
        (
            at_u32(&cfg.loop_depth, b) == 0 && at_bool(&cfg.is_terminal_exit, b),
            Reverse(at_u64(&freq.block_freq, b)),
            at_usize(&cfg.rpo_rank, b),
            b,
        )
    });
    for &seed in &seeds {
        if !at_bool(&placed, seed) {
            extend_trace(seed, &mut placed, &mut order);
        }
    }

    // 3. Remaining unreachable non-cold blocks, then cold blocks in deterministic RPO order.
    for &b in &cfg.rpo {
        if !at_bool(&placed, b) && !at_bool(&cfg.is_cold, b) {
            set_at(&mut placed, b, true);
            order.push(b);
        }
    }
    for &b in &cfg.rpo {
        if !at_bool(&placed, b) {
            set_at(&mut placed, b, true);
            order.push(b);
        }
    }

    order
}

/// Post-condition: if Ext-TSP rotated a multi-pred (`>= 3` preds) bottom-tested `Op::Br` loop
/// latch `l` immediately before its loop header `h` (`pos(l) + 1 == pos(h)` and `prev(l)` has
/// strictly shallower loop depth), mark `is_loop_hdr[l] = true` so `isel` 16B-aligns `l`.
fn mark_rotated_multi_pred_latches(order: &[usize], cfg: &mut CfgInfo<'_>) {
    if order.len() < 3 {
        return;
    }
    for pos in 1..order.len() - 1 {
        let l = at_usize(order, pos);
        let h = at_usize(order, pos + 1);
        let prev = at_usize(order, pos - 1);
        if at_usize(&cfg.rotatable_latch_hdr, l) == h
            && at_bool(&cfg.is_loop_hdr, h)
            && at_u32(&cfg.loop_depth, prev) < at_u32(&cfg.loop_depth, h)
        {
            set_at(&mut cfg.is_loop_hdr, l, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_lir::{
        Block, Flags, IntCond, Op, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv},
    };
    use blaze_own::Alloc;

    use super::{block_size_bytes, compute_block_layout, compute_block_layout_with_pgo};

    fn test_alloc() -> Alloc<'static> {
        Alloc::global()
    }

    fn idx_of(blk: Block) -> usize {
        usize::try_from(blk.0).expect("block index fits usize")
    }

    fn pos_of(order: &[usize], blk: Block) -> usize {
        let target = idx_of(blk);
        order
            .iter()
            .position(|&x| x == target)
            .expect("block must appear in layout order")
    }

    fn assert_valid_permutation(order: &[usize], blocks: usize) {
        assert_eq!(order.len(), blocks, "order length must equal block count");
        if blocks > 0 {
            assert_eq!(order.first().copied(), Some(0), "entry block 0 must come first");
        }
        let mut seen = [false; 64];
        assert!(blocks <= seen.len(), "test helper expects <= 64 blocks");
        for &b in order {
            assert!(b < blocks, "out-of-range block id {b} >= {blocks}");
            assert!(!seen[b], "duplicate block id {b} in order={order:?}");
            seen[b] = true;
        }
    }

    #[test]
    fn ext_tsp_keeps_outer_spine_over_conditional_inner_loop() {
        let alloc = test_alloc();
        // Mirrors `hot_loop_layout_prefers_outer_spine_over_conditional_inner_loop`:
        //   entry -> br(is_empty, exit, outer_hdr)
        //   outer_hdr -> br(is_long, inner_pre, outer_latch)
        //   inner_pre -> jump(inner_hdr)
        //   inner_hdr -> br(k_done, outer_latch, inner_hdr)
        //   outer_latch -> br(seq_done, exit, outer_hdr)
        //   exit -> ret
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let outer_hdr = b.create_block(BlockKind::Normal);
        let inner_pre = b.create_block(BlockKind::Normal);
        let inner_hdr = b.create_block(BlockKind::Normal);
        let outer_latch = b.create_block(BlockKind::Normal);
        let exit_blk = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let seq_count = b.param(Ty::I64);
        let match_len = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c32 = b.iconst(Ty::I64, 32);
        let is_empty = b.icmp(IntCond::Eq, seq_count, c0);
        b.br(Flags::NONE, is_empty, (exit_blk, &[c0]), (outer_hdr, &[seq_count, c0]));

        b.begin(outer_hdr);
        let rem_seq = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let base_acc = b.binary(Op::Add, Flags::NONE, acc, match_len);
        let is_long = b.icmp(IntCond::Ugt, match_len, c32);
        b.br(Flags::NONE, is_long, (inner_pre, &[base_acc]), (outer_latch, &[base_acc]));

        b.begin(inner_pre);
        let pre_acc = b.param(Ty::I64);
        let extra = b.binary(Op::Sub, Flags::NONE, match_len, c32);
        b.jump((inner_hdr, &[extra, pre_acc]));

        b.begin(inner_hdr);
        let k = b.param(Ty::I64);
        let in_acc = b.param(Ty::I64);
        let in_acc2 = b.binary(Op::Add, Flags::NONE, in_acc, c3);
        let k_next = b.binary(Op::Sub, Flags::NONE, k, c1);
        let k_done = b.icmp(IntCond::Eq, k_next, c0);
        b.br(Flags::NONE, k_done, (outer_latch, &[in_acc2]), (inner_hdr, &[k_next, in_acc2]));

        b.begin(outer_latch);
        let lat_acc = b.param(Ty::I64);
        let rem_next = b.binary(Op::Sub, Flags::NONE, rem_seq, c1);
        let seq_done = b.icmp(IntCond::Eq, rem_next, c0);
        b.br(Flags::NONE, seq_done, (exit_blk, &[lat_acc]), (outer_hdr, &[rem_next, lat_acc]));

        b.begin(exit_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().expect("outer-spine + conditional inner loop CFG builds");
        let view = func.view();
        let layout = compute_block_layout(&view, alloc);

        assert_valid_permutation(&layout.order, view.blocks());
        assert_eq!(
            layout.order.as_slice(),
            &[
                idx_of(entry),
                idx_of(outer_hdr),
                idx_of(outer_latch),
                idx_of(inner_pre),
                idx_of(inner_hdr),
                idx_of(exit_blk),
            ],
            "outer spine must stay fallthrough with conditional inner loop deferred before exit, order={:?}",
            layout.order
        );
        assert_eq!(layout.loop_depth[idx_of(outer_hdr)], 1);
        assert_eq!(layout.loop_depth[idx_of(inner_hdr)], 2);
        assert!(layout.is_loop_hdr[idx_of(outer_hdr)]);
        assert!(layout.is_loop_hdr[idx_of(inner_hdr)]);
        assert!(!layout.is_loop_hdr[idx_of(outer_latch)]);
        assert!(
            layout.edge_weight_between(idx_of(outer_hdr), idx_of(outer_latch))
                > layout.edge_weight_between(idx_of(outer_hdr), idx_of(inner_pre)),
            "outer spine edge must outweigh conditional inner-loop entry"
        );
        assert!(block_size_bytes(&view, idx_of(outer_hdr)) >= 4);
    }

    #[test]
    fn ext_tsp_colocates_inner_scan_hit_checks_before_sibling_inner_loops() {
        let alloc = test_alloc();
        // Models `lz77_scan_default_leaf`:
        //   entry -> outer_hdr (d=1)
        //   outer_hdr -> br(done, exit, inner_scan_hdr)
        //   inner_scan_hdr (d=2) -> br(found, hit_check_1, inner_scan_step)
        //   inner_scan_step (d=2) -> jump(inner_scan_hdr)
        //   hit_check_1 (d=1) -> br(ok1, hit_check_2, outer_latch)
        //   hit_check_2 (d=1) -> br(ok2, match_ext, outer_latch)
        //   match_ext (d=2, sibling inner loop) -> br(more, match_ext, outer_latch)
        //   outer_latch (d=1) -> jump(outer_hdr)
        //   exit (d=0) -> ret
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let outer_hdr = b.create_block(BlockKind::Normal);
        let inner_scan_hdr = b.create_block(BlockKind::Normal);
        let inner_scan_step = b.create_block(BlockKind::Normal);
        let hit_check_1 = b.create_block(BlockKind::Normal);
        let hit_check_2 = b.create_block(BlockKind::Normal);
        let match_ext = b.create_block(BlockKind::Normal);
        let outer_latch = b.create_block(BlockKind::Normal);
        let exit_blk = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c7 = b.iconst(Ty::I64, 7);
        b.jump((outer_hdr, &[c0, seed]));

        b.begin(outer_hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Uge, i, n);
        b.br(Flags::NONE, done, (exit_blk, &[acc]), (inner_scan_hdr, &[i]));

        b.begin(inner_scan_hdr);
        let pos = b.param(Ty::I64);
        let found = b.icmp(IntCond::Eq, pos, acc);
        b.br(Flags::NONE, found, (hit_check_1, &[pos]), (inner_scan_step, &[]));

        b.begin(inner_scan_step);
        let pos_next = b.binary(Op::Add, Flags::NONE, pos, c1);
        b.jump((inner_scan_hdr, &[pos_next]));

        b.begin(hit_check_1);
        let hp = b.param(Ty::I64);
        let m1 = b.binary(Op::And, Flags::NONE, hp, c7);
        let ok1 = b.icmp(IntCond::Eq, m1, c0);
        b.br(Flags::NONE, ok1, (hit_check_2, &[hp]), (outer_latch, &[acc]));

        b.begin(hit_check_2);
        let hp2 = b.param(Ty::I64);
        let m2 = b.binary(Op::Xor, Flags::NONE, hp2, acc);
        let ok2 = b.icmp(IntCond::Ugt, m2, c7);
        b.br(Flags::NONE, ok2, (match_ext, &[m2, acc]), (outer_latch, &[m2]));

        b.begin(match_ext);
        let ext_len = b.param(Ty::I64);
        let ext_acc = b.param(Ty::I64);
        let ext_next = b.binary(Op::Sub, Flags::NONE, ext_len, c1);
        let ext_acc2 = b.binary(Op::Add, Flags::NONE, ext_acc, c1);
        let more = b.icmp(IntCond::Ugt, ext_next, c0);
        b.br(Flags::NONE, more, (match_ext, &[ext_next, ext_acc2]), (outer_latch, &[ext_acc2]));

        b.begin(outer_latch);
        let lat_acc = b.param(Ty::I64);
        let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
        b.jump((outer_hdr, &[i_next, lat_acc]));

        b.begin(exit_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().expect("lz77 inner_scan + hit_checks + match_ext CFG builds");
        let view = func.view();
        let layout = compute_block_layout(&view, alloc);

        assert_valid_permutation(&layout.order, view.blocks());
        assert_eq!(layout.loop_depth[idx_of(inner_scan_hdr)], 2);
        assert_eq!(layout.loop_depth[idx_of(hit_check_1)], 1);
        assert_eq!(layout.loop_depth[idx_of(hit_check_2)], 1);
        assert_eq!(layout.loop_depth[idx_of(match_ext)], 2);

        let p_scan_hdr = pos_of(&layout.order, inner_scan_hdr);
        let p_scan_step = pos_of(&layout.order, inner_scan_step);
        let p_hc1 = pos_of(&layout.order, hit_check_1);
        let p_hc2 = pos_of(&layout.order, hit_check_2);
        let p_match_ext = pos_of(&layout.order, match_ext);

        assert_eq!(
            p_scan_step,
            p_scan_hdr + 1,
            "inner_scan_step must immediately follow inner_scan_hdr, order={:?}",
            layout.order
        );
        assert_eq!(
            p_hc1,
            p_scan_step + 1,
            "hit_check_1 must be immediately adjacent to inner_scan loop, order={:?}",
            layout.order
        );
        assert_eq!(
            p_hc2,
            p_hc1 + 1,
            "hit_check_2 must immediately follow hit_check_1, order={:?}",
            layout.order
        );
        assert!(
            p_hc2 < p_match_ext,
            "hit_check_1 and hit_check_2 must be placed BEFORE sibling inner loop match_ext, order={:?}",
            layout.order
        );
        assert!(
            layout.block_freq[idx_of(inner_scan_hdr)] > layout.block_freq[idx_of(outer_hdr)],
            "depth-2 inner_scan frequency must exceed depth-1 outer_hdr frequency"
        );
    }

    #[test]
    fn ext_tsp_rotates_multi_arm_latch_before_loop_header() {
        let alloc = test_alloc();
        // Models `decode_exec_leaf_single`:
        //   preheader -> header -> switch across 4 arms -> tiny countdown `Op::Br` latch -> header / exit
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let preheader = b.create_block(BlockKind::Normal);
        let header = b.create_block(BlockKind::Normal);
        let arm0 = b.create_block(BlockKind::Normal);
        let arm1 = b.create_block(BlockKind::Normal);
        let arm2 = b.create_block(BlockKind::Normal);
        let arm3 = b.create_block(BlockKind::Normal);
        let latch = b.create_block(BlockKind::Normal);
        let exit_blk = b.create_block(BlockKind::Normal);

        b.begin(preheader);
        let n = b.param(Ty::I64);
        let acc0 = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c2 = b.iconst(Ty::I64, 2);
        let c3 = b.iconst(Ty::I64, 3);
        b.jump((header, &[n, acc0]));

        b.begin(header);
        let rem = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let tag = b.binary(Op::And, Flags::NONE, acc, c3);
        b.switch(tag, (arm3, &[]), &[(0, (arm0, &[])), (1, (arm1, &[])), (2, (arm2, &[]))]);

        b.begin(arm0);
        let a0 = b.binary(Op::Add, Flags::NONE, acc, c1);
        b.jump((latch, &[rem, a0]));

        b.begin(arm1);
        let a1 = b.binary(Op::Add, Flags::NONE, acc, c2);
        b.jump((latch, &[rem, a1]));

        b.begin(arm2);
        let a2 = b.binary(Op::Xor, Flags::NONE, acc, c3);
        b.jump((latch, &[rem, a2]));

        b.begin(arm3);
        let a3 = b.binary(Op::Sub, Flags::NONE, acc, c1);
        b.jump((latch, &[rem, a3]));

        // Tiny bottom-tested countdown latch (3 material insts <= 4, 4 predecessors >= 3).
        b.begin(latch);
        let l_rem = b.param(Ty::I64);
        let l_acc = b.param(Ty::I64);
        let next_rem = b.binary(Op::Sub, Flags::NONE, l_rem, c1);
        let done = b.icmp(IntCond::Eq, next_rem, c0);
        b.br(Flags::NONE, done, (exit_blk, &[l_acc]), (header, &[next_rem, l_acc]));

        b.begin(exit_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().expect("4-arm multi-pred latch CFG builds");
        let view = func.view();
        let layout = compute_block_layout(&view, alloc);

        assert_valid_permutation(&layout.order, view.blocks());
        let p_pre = pos_of(&layout.order, preheader);
        let p_latch = pos_of(&layout.order, latch);
        let p_header = pos_of(&layout.order, header);
        let p_exit = pos_of(&layout.order, exit_blk);

        assert_eq!(p_pre, 0, "preheader must stay first, order={:?}", layout.order);
        assert_eq!(
            p_latch + 1,
            p_header,
            "Ext-TSP must rotate 4-arm countdown latch immediately before header, order={:?}",
            layout.order
        );
        let prev_latch = layout.order[p_latch - 1];
        let prev_is_shallower = layout.loop_depth[prev_latch] < layout.loop_depth[idx_of(header)];
        assert_eq!(
            layout.is_loop_hdr[idx_of(latch)],
            prev_is_shallower,
            "rotated latch gets is_loop_hdr alignment iff its predecessor is strictly shallower, order={:?}",
            layout.order
        );
        assert_eq!(
            p_exit,
            view.blocks() - 1,
            "terminal exit must sit after the entire loop, order={:?}",
            layout.order
        );
        assert_eq!(
            layout.block_freq[idx_of(header)],
            10_240,
            "depth-1 header with unconditional entry 1024 must have block_freq 10_240"
        );
    }

    #[test]
    fn ext_tsp_sinks_cold_blocks_and_honors_pgo_weights() {
        let alloc = test_alloc();
        // Diamond with a cold trap off `branch_a`:
        //   entry -> br(cond, branch_a, branch_b)
        //   branch_a -> br(err, cold_trap, merge)
        //   branch_b -> jump(merge)
        //   cold_trap (BlockKind::Cold) -> trap
        //   merge -> ret
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let branch_a = b.create_block(BlockKind::Normal);
        let branch_b = b.create_block(BlockKind::Normal);
        let cold_trap = b.create_block(BlockKind::Cold);
        let merge = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c9 = b.iconst(Ty::I64, 9);
        let take_a = b.icmp(IntCond::Eq, x, c0);
        b.br(Flags::NONE, take_a, (branch_a, &[]), (branch_b, &[]));

        b.begin(branch_a);
        let is_err = b.icmp(IntCond::Eq, y, c0);
        let va = b.binary(Op::Add, Flags::NONE, y, c1);
        b.br(Flags::NONE, is_err, (cold_trap, &[]), (merge, &[va]));

        b.begin(branch_b);
        let vb = b.binary(Op::Xor, Flags::NONE, y, c9);
        b.jump((merge, &[vb]));

        b.begin(cold_trap);
        b.trap(1);

        b.begin(merge);
        let out = b.param(Ty::I64);
        b.ret(&[out]);

        let func = b.finish_checked().expect("cold + PGO diamond CFG builds");
        let view = func.view();

        // 1. Static layout (`branch_weights = None`): cold_trap sunk to tail, tie broken by RPO
        //    so `branch_a` is fallthrough after `entry`.
        let static_layout = compute_block_layout(&view, alloc);
        assert_valid_permutation(&static_layout.order, view.blocks());
        assert!(static_layout.is_cold[idx_of(cold_trap)]);
        assert!(
            static_layout.block_freq[idx_of(cold_trap)] <= 10,
            "cold block frequency must be <= 10, got {}",
            static_layout.block_freq[idx_of(cold_trap)]
        );
        assert_eq!(
            static_layout.order.last().copied(),
            Some(idx_of(cold_trap)),
            "cold_trap must be sunk to tail, order={:?}",
            static_layout.order
        );
        assert_eq!(
            pos_of(&static_layout.order, branch_a),
            pos_of(&static_layout.order, entry) + 1,
            "static tie-break must place branch_a as fallthrough, order={:?}",
            static_layout.order
        );

        // 2. PGO favoring `branch_b` (`taken = 50, not_taken = 9950`): flips fallthrough to `branch_b`!
        let pgo_favor_b = |src: usize, then_tgt: usize| -> Option<(u64, u64)> {
            if src == idx_of(entry) && then_tgt == idx_of(branch_a) {
                Some((50, 9950))
            } else {
                None
            }
        };
        let layout_b = compute_block_layout_with_pgo(&view, Some(&pgo_favor_b), alloc);
        assert_valid_permutation(&layout_b.order, view.blocks());
        assert_eq!(
            layout_b.order.last().copied(),
            Some(idx_of(cold_trap)),
            "cold_trap must remain sunk to tail under PGO, order={:?}",
            layout_b.order
        );
        assert_eq!(
            pos_of(&layout_b.order, branch_b),
            pos_of(&layout_b.order, entry) + 1,
            "PGO favoring else arm must flip fallthrough to branch_b, order={:?}",
            layout_b.order
        );
        assert!(
            layout_b.edge_weight_between(idx_of(entry), idx_of(branch_b))
                > layout_b.edge_weight_between(idx_of(entry), idx_of(branch_a)),
            "PGO edge weight to branch_b must strictly exceed weight to branch_a"
        );

        // 3. PGO favoring `branch_a` (`taken = 9950, not_taken = 50`): flips fallthrough back to `branch_a`!
        let pgo_favor_a = |src: usize, then_tgt: usize| -> Option<(u64, u64)> {
            if src == idx_of(entry) && then_tgt == idx_of(branch_a) {
                Some((9950, 50))
            } else {
                None
            }
        };
        let layout_a = compute_block_layout_with_pgo(&view, Some(&pgo_favor_a), alloc);
        assert_valid_permutation(&layout_a.order, view.blocks());
        assert_eq!(
            layout_a.order.last().copied(),
            Some(idx_of(cold_trap)),
            "cold_trap must remain sunk to tail under PGO, order={:?}",
            layout_a.order
        );
        assert_eq!(
            pos_of(&layout_a.order, branch_a),
            pos_of(&layout_a.order, entry) + 1,
            "PGO favoring then arm must keep branch_a as fallthrough, order={:?}",
            layout_a.order
        );
        assert!(
            layout_a.edge_weight_between(idx_of(entry), idx_of(branch_a))
                > layout_a.edge_weight_between(idx_of(entry), idx_of(branch_b)),
            "PGO edge weight to branch_a must strictly exceed weight to branch_b"
        );
    }

    #[test]
    fn ext_tsp_never_inverts_large_non_rotatable_loop() {
        let alloc = test_alloc();
        // 1. Straight-line large non-rotatable loop (body > EXT_TSP_BWD_LIMIT = 640 B):
        //    entry -> hdr -> b1 -> b2 -> b3 -> b4 -> b5 -> latch -> (hdr | exit)
        //    `latch` has 1 predecessor (`non-rotatable`); entry must fall straight into `hdr`.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let hdr = b.create_block(BlockKind::Normal);
            let b1 = b.create_block(BlockKind::Normal);
            let b2 = b.create_block(BlockKind::Normal);
            let b3 = b.create_block(BlockKind::Normal);
            let b4 = b.create_block(BlockKind::Normal);
            let b5 = b.create_block(BlockKind::Normal);
            let latch = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let is_empty = b.icmp(IntCond::Eq, n, c0);
            b.br(Flags::NONE, is_empty, (exit_blk, &[seed]), (hdr, &[n, seed]));

            b.begin(hdr);
            let rem = b.param(Ty::I64);
            let mut v0 = b.param(Ty::I64);
            for _ in 0..30 {
                v0 = b.binary(Op::Add, Flags::NONE, v0, c1);
            }
            b.jump((b1, &[v0]));

            b.begin(b1);
            let mut v1 = b.param(Ty::I64);
            for _ in 0..30 {
                v1 = b.binary(Op::Add, Flags::NONE, v1, c1);
            }
            b.jump((b2, &[v1]));

            b.begin(b2);
            let mut v2 = b.param(Ty::I64);
            for _ in 0..30 {
                v2 = b.binary(Op::Add, Flags::NONE, v2, c1);
            }
            b.jump((b3, &[v2]));

            b.begin(b3);
            let mut v3 = b.param(Ty::I64);
            for _ in 0..30 {
                v3 = b.binary(Op::Add, Flags::NONE, v3, c1);
            }
            b.jump((b4, &[v3]));

            b.begin(b4);
            let mut v4 = b.param(Ty::I64);
            for _ in 0..30 {
                v4 = b.binary(Op::Add, Flags::NONE, v4, c1);
            }
            b.jump((b5, &[v4]));

            b.begin(b5);
            let mut v5 = b.param(Ty::I64);
            for _ in 0..30 {
                v5 = b.binary(Op::Add, Flags::NONE, v5, c1);
            }
            b.jump((latch, &[v5]));

            b.begin(latch);
            let acc_l = b.param(Ty::I64);
            let rem_next = b.binary(Op::Sub, Flags::NONE, rem, c1);
            let done = b.icmp(IntCond::Eq, rem_next, c0);
            b.br(Flags::NONE, done, (exit_blk, &[acc_l]), (hdr, &[rem_next, acc_l]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("large straight-line non-rotatable loop CFG builds");
            let view = func.view();
            let body_bytes = block_size_bytes(&view, idx_of(hdr))
                + block_size_bytes(&view, idx_of(b1))
                + block_size_bytes(&view, idx_of(b2))
                + block_size_bytes(&view, idx_of(b3))
                + block_size_bytes(&view, idx_of(b4))
                + block_size_bytes(&view, idx_of(b5))
                + block_size_bytes(&view, idx_of(latch));
            assert!(
                body_bytes > 640,
                "loop body must exceed EXT_TSP_BWD_LIMIT (640 B), got {body_bytes}"
            );

            let layout = compute_block_layout(&view, alloc);
            assert_valid_permutation(&layout.order, view.blocks());
            assert_eq!(layout.order[0], idx_of(entry), "entry must stay first, order={:?}", layout.order);
            assert_eq!(
                layout.order[1],
                idx_of(hdr),
                "entry must fall straight through into hdr without inverting large loop, order={:?}",
                layout.order
            );
            assert_eq!(
                layout.order.as_slice(),
                &[
                    idx_of(entry),
                    idx_of(hdr),
                    idx_of(b1),
                    idx_of(b2),
                    idx_of(b3),
                    idx_of(b4),
                    idx_of(b5),
                    idx_of(latch),
                    idx_of(exit_blk),
                ],
                "large non-rotatable loop must preserve natural forward order, order={:?}",
                layout.order
            );
            assert!(layout.is_loop_hdr[idx_of(hdr)]);
            assert!(!layout.is_loop_hdr[idx_of(latch)]);
        }

        // 2. Branched large non-rotatable loop with inner scan (models `lz77_scan_*_leaf`):
        //    When the tail chain `[arm_b, tail1, tail2, outer_latch]` exceeds `EXT_TSP_BWD_LIMIT`
        //    (640 B) and `outer_latch -> outer_hdr` outweighs `entry -> outer_hdr`, crediting
        //    non-rotatable back-edges at `w * 10` inverted the loop into
        //    `[entry, arm_b, tail1, tail2, outer_latch, outer_hdr, ...]`. Scoring them at `0`
        //    preserves `entry -> outer_hdr` fallthrough.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let inner_scan_hdr = b.create_block(BlockKind::Normal);
            let inner_scan_step = b.create_block(BlockKind::Normal);
            let dispatch = b.create_block(BlockKind::Normal);
            let arm_a = b.create_block(BlockKind::Normal);
            let arm_a1 = b.create_block(BlockKind::Normal);
            let arm_a2 = b.create_block(BlockKind::Normal);
            let arm_b = b.create_block(BlockKind::Normal);
            let tail1 = b.create_block(BlockKind::Normal);
            let tail2 = b.create_block(BlockKind::Normal);
            let outer_latch = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c2 = b.iconst(Ty::I64, 2);
            b.jump((outer_hdr, &[c0, seed]));

            b.begin(outer_hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let done = b.icmp(IntCond::Uge, i, n);
            b.br(Flags::NONE, done, (exit_blk, &[acc]), (inner_scan_hdr, &[i]));

            b.begin(inner_scan_hdr);
            let pos = b.param(Ty::I64);
            let found = b.icmp(IntCond::Eq, pos, acc);
            b.br(Flags::NONE, found, (dispatch, &[pos]), (inner_scan_step, &[]));

            b.begin(inner_scan_step);
            let pos_next = b.binary(Op::Add, Flags::NONE, pos, c1);
            b.jump((inner_scan_hdr, &[pos_next]));

            b.begin(dispatch);
            let dp = b.param(Ty::I64);
            let bit0 = b.binary(Op::And, Flags::NONE, dp, c1);
            let take_a = b.icmp(IntCond::Eq, bit0, c0);
            b.br(Flags::NONE, take_a, (arm_a, &[dp]), (arm_b, &[dp]));

            b.begin(arm_a);
            let mut va = b.param(Ty::I64);
            for _ in 0..30 {
                va = b.binary(Op::Add, Flags::NONE, va, c1);
            }
            let bit1 = b.binary(Op::And, Flags::NONE, va, c2);
            let take_a1 = b.icmp(IntCond::Eq, bit1, c0);
            b.br(Flags::NONE, take_a1, (arm_a1, &[va]), (arm_a2, &[va]));

            b.begin(arm_a1);
            let mut va1 = b.param(Ty::I64);
            for _ in 0..30 {
                va1 = b.binary(Op::Add, Flags::NONE, va1, c1);
            }
            b.jump((tail1, &[va1]));

            b.begin(arm_a2);
            let mut va2 = b.param(Ty::I64);
            for _ in 0..30 {
                va2 = b.binary(Op::Add, Flags::NONE, va2, c1);
            }
            b.jump((tail1, &[va2]));

            b.begin(arm_b);
            let mut vb = b.param(Ty::I64);
            for _ in 0..30 {
                vb = b.binary(Op::Add, Flags::NONE, vb, c1);
            }
            b.jump((tail1, &[vb]));

            b.begin(tail1);
            let mut vt1 = b.param(Ty::I64);
            for _ in 0..80 {
                vt1 = b.binary(Op::Add, Flags::NONE, vt1, c1);
            }
            b.jump((tail2, &[vt1]));

            b.begin(tail2);
            let mut vt2 = b.param(Ty::I64);
            for _ in 0..80 {
                vt2 = b.binary(Op::Add, Flags::NONE, vt2, c1);
            }
            b.jump((outer_latch, &[vt2]));

            b.begin(outer_latch);
            let lat_acc = b.param(Ty::I64);
            let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
            b.jump((outer_hdr, &[i_next, lat_acc]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("large branched non-rotatable loop CFG builds");
            let view = func.view();
            let tail_bytes = block_size_bytes(&view, idx_of(arm_b))
                + block_size_bytes(&view, idx_of(tail1))
                + block_size_bytes(&view, idx_of(tail2))
                + block_size_bytes(&view, idx_of(outer_latch));
            assert!(
                tail_bytes > 640,
                "loop tail must exceed EXT_TSP_BWD_LIMIT (640 B), got {tail_bytes}"
            );

            let layout = compute_block_layout(&view, alloc);
            assert_valid_permutation(&layout.order, view.blocks());
            assert_eq!(layout.order[0], idx_of(entry), "entry must stay first, order={:?}", layout.order);
            assert_eq!(
                layout.order[1],
                idx_of(outer_hdr),
                "entry must fall straight through into outer_hdr without inverting large branched loop, order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, outer_hdr) < pos_of(&layout.order, outer_latch),
                "outer_hdr must precede non-rotatable outer_latch, order={:?}",
                layout.order
            );
        }

        // 3. Branched large non-rotatable loop where `repeat_check` is followed by 2 small
        //    sibling loop blocks (`stub1`, `stub2`, ~80 B total < EXT_TSP_FWD_LIMIT = 1024 B):
        //    Models the exact `lz77_scan_default_leaf` pattern where inverting the tail chain
        //    before `outer_hdr` places `outer_hdr` at `pos_v == pos_u + 3` (`dist < 1024 B`).
        //    Non-rotatable back-edges must earn `0` for ALL `pos_v > pos_u` (not only
        //    `pos_v == pos_u + 1`) so forward short-jump credit never inverts the loop.
        {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let inner_scan_hdr = b.create_block(BlockKind::Normal);
            let inner_scan_step = b.create_block(BlockKind::Normal);
            let dispatch = b.create_block(BlockKind::Normal);
            let arm_a = b.create_block(BlockKind::Normal);
            let arm_a1 = b.create_block(BlockKind::Normal);
            let arm_a2 = b.create_block(BlockKind::Normal);
            let arm_b = b.create_block(BlockKind::Normal);
            let tail1 = b.create_block(BlockKind::Normal);
            let match_ext_hdr = b.create_block(BlockKind::Normal);
            let match_ext_step = b.create_block(BlockKind::Normal);
            let tail2 = b.create_block(BlockKind::Normal);
            let repeat_check = b.create_block(BlockKind::Normal);
            let stub1 = b.create_block(BlockKind::Normal);
            let stub2 = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c2 = b.iconst(Ty::I64, 2);
            let is_empty = b.icmp(IntCond::Eq, n, c0);
            b.br(Flags::NONE, is_empty, (exit_blk, &[seed]), (outer_hdr, &[c0, seed]));

            b.begin(outer_hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let done = b.icmp(IntCond::Uge, i, n);
            b.br(Flags::NONE, done, (exit_blk, &[acc]), (inner_scan_hdr, &[i]));

            b.begin(inner_scan_hdr);
            let pos = b.param(Ty::I64);
            let found = b.icmp(IntCond::Eq, pos, acc);
            b.br(Flags::NONE, found, (dispatch, &[pos]), (inner_scan_step, &[]));

            b.begin(inner_scan_step);
            let pos_next = b.binary(Op::Add, Flags::NONE, pos, c1);
            b.jump((inner_scan_hdr, &[pos_next]));

            b.begin(dispatch);
            let dp = b.param(Ty::I64);
            let bit0 = b.binary(Op::And, Flags::NONE, dp, c1);
            let take_a = b.icmp(IntCond::Eq, bit0, c0);
            b.br(Flags::NONE, take_a, (arm_a, &[dp]), (arm_b, &[dp]));

            b.begin(arm_a);
            let mut va = b.param(Ty::I64);
            for _ in 0..140 {
                va = b.binary(Op::Add, Flags::NONE, va, c1);
            }
            let bit1 = b.binary(Op::And, Flags::NONE, va, c2);
            let take_a1 = b.icmp(IntCond::Eq, bit1, c0);
            b.br(Flags::NONE, take_a1, (arm_a1, &[va]), (arm_a2, &[va]));

            b.begin(arm_a1);
            let mut va1 = b.param(Ty::I64);
            for _ in 0..140 {
                va1 = b.binary(Op::Add, Flags::NONE, va1, c1);
            }
            b.jump((tail1, &[va1]));

            b.begin(arm_a2);
            let mut va2 = b.param(Ty::I64);
            for _ in 0..30 {
                va2 = b.binary(Op::Add, Flags::NONE, va2, c1);
            }
            b.jump((tail1, &[va2]));

            b.begin(arm_b);
            let mut vb = b.param(Ty::I64);
            for _ in 0..260 {
                vb = b.binary(Op::Add, Flags::NONE, vb, c1);
            }
            b.jump((tail1, &[vb]));

            b.begin(tail1);
            let mut vt1 = b.param(Ty::I64);
            for _ in 0..80 {
                vt1 = b.binary(Op::Add, Flags::NONE, vt1, c1);
            }
            b.jump((match_ext_hdr, &[vt1]));

            b.begin(match_ext_hdr);
            let ext = b.param(Ty::I64);
            let done_ext = b.icmp(IntCond::Eq, ext, acc);
            b.br(Flags::NONE, done_ext, (tail2, &[ext]), (match_ext_step, &[]));

            b.begin(match_ext_step);
            let ext_next = b.binary(Op::Add, Flags::NONE, ext, c1);
            b.jump((match_ext_hdr, &[ext_next]));

            b.begin(tail2);
            let mut vt2 = b.param(Ty::I64);
            for _ in 0..80 {
                vt2 = b.binary(Op::Add, Flags::NONE, vt2, c1);
            }
            b.jump((repeat_check, &[vt2]));

            b.begin(repeat_check);
            let rc = b.param(Ty::I64);
            let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
            let bit_rc = b.binary(Op::And, Flags::NONE, rc, c1);
            let keep_outer = b.icmp(IntCond::Eq, bit_rc, c0);
            b.br(Flags::NONE, keep_outer, (outer_hdr, &[i_next, rc]), (stub1, &[rc]));

            b.begin(stub1);
            let mut vs1 = b.param(Ty::I64);
            for _ in 0..9 {
                vs1 = b.binary(Op::Add, Flags::NONE, vs1, c1);
            }
            b.jump((stub2, &[vs1]));

            b.begin(stub2);
            let mut vs2 = b.param(Ty::I64);
            for _ in 0..8 {
                vs2 = b.binary(Op::Add, Flags::NONE, vs2, c1);
            }
            let i_stub = b.binary(Op::Add, Flags::NONE, i, c1);
            b.jump((outer_hdr, &[i_stub, vs2]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("large non-rotatable loop with sibling tail stubs builds");
            let view = func.view();
            let tail_bytes = block_size_bytes(&view, idx_of(arm_b))
                + block_size_bytes(&view, idx_of(tail1))
                + block_size_bytes(&view, idx_of(match_ext_hdr))
                + block_size_bytes(&view, idx_of(match_ext_step))
                + block_size_bytes(&view, idx_of(tail2))
                + block_size_bytes(&view, idx_of(repeat_check))
                + block_size_bytes(&view, idx_of(stub1))
                + block_size_bytes(&view, idx_of(stub2));
            let stub_bytes =
                block_size_bytes(&view, idx_of(stub1)) + block_size_bytes(&view, idx_of(stub2));
            assert!(
                tail_bytes > 640,
                "loop tail must exceed EXT_TSP_BWD_LIMIT (640 B), got {tail_bytes}"
            );
            assert!(
                stub_bytes > 0 && stub_bytes < 1024,
                "sibling stubs after repeat_check must be within EXT_TSP_FWD_LIMIT (1024 B), got {stub_bytes}"
            );

            let layout = compute_block_layout(&view, alloc);
            assert_valid_permutation(&layout.order, view.blocks());
            assert_eq!(layout.order[0], idx_of(entry), "entry must stay first, order={:?}", layout.order);
            assert_eq!(
                layout.order[1],
                idx_of(outer_hdr),
                "entry must fall straight through into outer_hdr without inverting loop via forward back-edge bonus, order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, outer_hdr) < pos_of(&layout.order, repeat_check),
                "outer_hdr must precede non-rotatable repeat_check, order={:?}",
                layout.order
            );
            assert!(layout.is_loop_hdr[idx_of(outer_hdr)]);
            assert!(!layout.is_loop_hdr[idx_of(repeat_check)]);
        }
    }

    #[test]
    fn ext_tsp_falls_through_in_loop_guard_over_early_continue() {
        let alloc = test_alloc();
        // Models `lz77_scan_default_leaf`'s `long_hit` / `check_match` early-`continue` guards:
        //   entry -> outer_hdr (d=1)
        //   outer_hdr -> br(done, exit_blk, inner_scan)
        //   inner_scan (d=2) -> br(found, long_hit, inner_scan)
        //   long_hit (d=1) -> br(valid_dist, check_match, outer_hdr)
        //   check_match (d=1) -> br(bad_bytes, outer_hdr, emit_blk)
        //   emit_blk (d=1) -> jump(outer_hdr)
        //   exit_blk (d=0) -> ret
        // Both polarities (`check_match` as `then` and `emit_blk` as `else`) must favor the
        // forward in-loop continuation over the early-`continue` back-edge to `outer_hdr`.
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let outer_hdr = b.create_block(BlockKind::Normal);
        let inner_scan = b.create_block(BlockKind::Normal);
        let long_hit = b.create_block(BlockKind::Normal);
        let check_match = b.create_block(BlockKind::Normal);
        let emit_blk = b.create_block(BlockKind::Normal);
        let exit_blk = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let seed = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c4 = b.iconst(Ty::I64, 4);
        let c7 = b.iconst(Ty::I64, 7);
        b.jump((outer_hdr, &[c0, seed]));

        b.begin(outer_hdr);
        let pos = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let done = b.icmp(IntCond::Uge, pos, n);
        b.br(Flags::NONE, done, (exit_blk, &[acc]), (inner_scan, &[pos]));

        b.begin(inner_scan);
        let sp = b.param(Ty::I64);
        let sp_next = b.binary(Op::Add, Flags::NONE, sp, c1);
        let tag_bits = b.binary(Op::And, Flags::NONE, sp_next, c7);
        let found = b.icmp(IntCond::Eq, tag_bits, c0);
        b.br(Flags::NONE, found, (long_hit, &[sp_next]), (inner_scan, &[sp_next]));

        b.begin(long_hit);
        let hit_pos = b.param(Ty::I64);
        let miss_pos = b.binary(Op::Add, Flags::NONE, hit_pos, c1);
        let valid_dist = b.icmp(IntCond::Ugt, hit_pos, c4);
        b.br(Flags::NONE, valid_dist, (check_match, &[hit_pos]), (outer_hdr, &[miss_pos, acc]));

        b.begin(check_match);
        let cm_pos = b.param(Ty::I64);
        let cm_miss_pos = b.binary(Op::Add, Flags::NONE, cm_pos, c1);
        let diff = b.binary(Op::Xor, Flags::NONE, cm_pos, acc);
        let bad_bytes = b.icmp(IntCond::Ne, diff, c0);
        b.br(Flags::NONE, bad_bytes, (outer_hdr, &[cm_miss_pos, acc]), (emit_blk, &[cm_pos, diff]));

        b.begin(emit_blk);
        let em_pos = b.param(Ty::I64);
        let em_diff = b.param(Ty::I64);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, em_diff);
        let pos_next = b.binary(Op::Add, Flags::NONE, em_pos, c4);
        b.jump((outer_hdr, &[pos_next, acc_next]));

        b.begin(exit_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().expect("lz77 long_hit early-continue CFG builds");
        let view = func.view();
        let layout = compute_block_layout(&view, alloc);

        assert_valid_permutation(&layout.order, view.blocks());
        assert!(
            layout.edge_weight_between(idx_of(long_hit), idx_of(check_match))
                > layout.edge_weight_between(idx_of(long_hit), idx_of(outer_hdr)),
            "long_hit -> check_match must strictly outweigh early continue to outer_hdr"
        );
        assert!(
            layout.edge_weight_between(idx_of(check_match), idx_of(emit_blk))
                > layout.edge_weight_between(idx_of(check_match), idx_of(outer_hdr)),
            "check_match -> emit_blk must strictly outweigh early continue to outer_hdr"
        );
        assert_eq!(
            pos_of(&layout.order, check_match),
            pos_of(&layout.order, long_hit) + 1,
            "check_match must immediately follow long_hit as fallthrough, order={:?}",
            layout.order
        );
        assert_eq!(
            pos_of(&layout.order, emit_blk),
            pos_of(&layout.order, check_match) + 1,
            "emit_blk must immediately follow check_match as fallthrough, order={:?}",
            layout.order
        );
    }

    #[test]
    fn ext_tsp_falls_through_compact_fast_path_diamond() {
        let alloc = test_alloc();
        // Models `fse_pack_sequences_leaf`'s `if ml < 128 { 2-inst fast path } else { 7-inst
        // slow path }` in both branch polarities (`fast_arm` as `then` AND `fast_arm` as `else`),
        // verifying the compact fast-path arm (`8 B <= 24 B` vs `28 B >= 8 + 8 B`) always
        // outweighs the slow arm and immediately follows `hdr` in `layout.order`.
        for fast_is_then in [true, false] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let hdr = b.create_block(BlockKind::Normal);
            let fast_arm = b.create_block(BlockKind::Normal);
            let slow_arm = b.create_block(BlockKind::Normal);
            let merge = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let ml = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c7 = b.iconst(Ty::I64, 7);
            let c128 = b.iconst(Ty::I64, 128);
            b.jump((hdr, &[n, c0]));

            b.begin(hdr);
            let rem = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            if fast_is_then {
                let is_short = b.icmp(IntCond::Ult, ml, c128);
                b.br(Flags::NONE, is_short, (fast_arm, &[acc]), (slow_arm, &[acc]));
            } else {
                let is_long = b.icmp(IntCond::Uge, ml, c128);
                b.br(Flags::NONE, is_long, (slow_arm, &[acc]), (fast_arm, &[acc]));
            }

            // Compact fast-path arm: 2 post-isel insts (`8 B <= 24 B`, `Jump` skipped).
            b.begin(fast_arm);
            let f_acc = b.param(Ty::I64);
            let f1 = b.binary(Op::Add, Flags::NONE, f_acc, ml);
            let f2 = b.binary(Op::Xor, Flags::NONE, f1, c7);
            b.jump((merge, &[f2]));

            // Larger slow-path arm: 7 post-isel insts (`28 B >= 8 + 8 B`, `Jump` skipped).
            b.begin(slow_arm);
            let mut s = b.param(Ty::I64);
            for _ in 0..7 {
                s = b.binary(Op::Add, Flags::NONE, s, c1);
            }
            b.jump((merge, &[s]));

            b.begin(merge);
            let m_acc = b.param(Ty::I64);
            let rem_next = b.binary(Op::Sub, Flags::NONE, rem, c1);
            let done = b.icmp(IntCond::Eq, rem_next, c0);
            b.br(Flags::NONE, done, (exit_blk, &[m_acc]), (hdr, &[rem_next, m_acc]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("compact fast-path diamond CFG builds");
            let view = func.view();
            assert_eq!(block_size_bytes(&view, idx_of(fast_arm)), 8);
            assert_eq!(block_size_bytes(&view, idx_of(slow_arm)), 28);

            let layout = compute_block_layout(&view, alloc);
            assert_valid_permutation(&layout.order, view.blocks());
            assert!(
                layout.edge_weight_between(idx_of(hdr), idx_of(fast_arm))
                    > layout.edge_weight_between(idx_of(hdr), idx_of(slow_arm)),
                "compact fast_arm must outweigh slow_arm (fast_is_then={fast_is_then})"
            );
            assert_eq!(
                pos_of(&layout.order, fast_arm),
                pos_of(&layout.order, hdr) + 1,
                "fast_arm must immediately follow hdr as fallthrough (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert_eq!(
                pos_of(&layout.order, merge),
                pos_of(&layout.order, fast_arm) + 1,
                "merge must immediately follow fast_arm (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
        }
    }

    #[test]
    fn ext_tsp_entry_depth_ignores_terminal_exit_inside_loop() {
        let alloc = test_alloc();
        // Models `if lit_len <= 32 { fast } else { if lit_len > rem { return Err; } while_loop }`:
        // `err_exit` is a normal (non-cold) terminal return block (`is_terminal_exit == true`,
        // `is_cold == false`). Inside the loop, `entry_depth` ignores `is_terminal_exit`
        // successors so `bounds_check` inherits `entry_depth = 2` from `while_hdr`, keeping
        // `fast_copy -> outer_latch` as the outer-loop spine fallthrough.
        for fast_is_then in [true, false] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let fast_copy = b.create_block(BlockKind::Normal);
            let bounds_check = b.create_block(BlockKind::Normal);
            let err_exit = b.create_block(BlockKind::Normal);
            let while_hdr = b.create_block(BlockKind::Normal);
            let outer_latch = b.create_block(BlockKind::Normal);
            let ok_exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let lit_len = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c3 = b.iconst(Ty::I64, 3);
            let c32 = b.iconst(Ty::I64, 32);
            let c_err = b.iconst(Ty::I64, 0xFFFF);
            let is_empty = b.icmp(IntCond::Eq, n, c0);
            b.br(Flags::NONE, is_empty, (ok_exit, &[c0]), (outer_hdr, &[n, c0]));

            b.begin(outer_hdr);
            let rem = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            if fast_is_then {
                let is_short = b.icmp(IntCond::Ule, lit_len, c32);
                b.br(Flags::NONE, is_short, (fast_copy, &[acc]), (bounds_check, &[acc]));
            } else {
                let is_long = b.icmp(IntCond::Ugt, lit_len, c32);
                b.br(Flags::NONE, is_long, (bounds_check, &[acc]), (fast_copy, &[acc]));
            }

            b.begin(fast_copy);
            let f_acc = b.param(Ty::I64);
            let f_next = b.binary(Op::Add, Flags::NONE, f_acc, lit_len);
            b.jump((outer_latch, &[f_next]));

            b.begin(bounds_check);
            let bc_acc = b.param(Ty::I64);
            let oob = b.icmp(IntCond::Ugt, lit_len, rem);
            b.br(Flags::NONE, oob, (err_exit, &[]), (while_hdr, &[lit_len, bc_acc]));

            b.begin(err_exit);
            b.ret(&[c_err]);

            b.begin(while_hdr);
            let k = b.param(Ty::I64);
            let w_acc = b.param(Ty::I64);
            let w_acc2 = b.binary(Op::Add, Flags::NONE, w_acc, c3);
            let k_next = b.binary(Op::Sub, Flags::NONE, k, c1);
            let k_done = b.icmp(IntCond::Eq, k_next, c0);
            b.br(Flags::NONE, k_done, (outer_latch, &[w_acc2]), (while_hdr, &[k_next, w_acc2]));

            b.begin(outer_latch);
            let lat_acc = b.param(Ty::I64);
            let rem_next = b.binary(Op::Sub, Flags::NONE, rem, c1);
            let seq_done = b.icmp(IntCond::Eq, rem_next, c0);
            b.br(Flags::NONE, seq_done, (ok_exit, &[lat_acc]), (outer_hdr, &[rem_next, lat_acc]));

            b.begin(ok_exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("bounds-checked inner loop CFG builds");
            let view = func.view();
            let layout = compute_block_layout(&view, alloc);

            assert_valid_permutation(&layout.order, view.blocks());
            assert!(!layout.is_cold[idx_of(err_exit)]);
            assert!(layout.is_terminal_exit[idx_of(err_exit)]);
            assert!(
                layout.edge_weight_between(idx_of(outer_hdr), idx_of(fast_copy))
                    > layout.edge_weight_between(idx_of(outer_hdr), idx_of(bounds_check)),
                "fast_copy must outweigh bounds_check -> while_hdr (fast_is_then={fast_is_then})"
            );
            assert_eq!(
                pos_of(&layout.order, fast_copy),
                pos_of(&layout.order, outer_hdr) + 1,
                "fast_copy must immediately follow outer_hdr (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert_eq!(
                pos_of(&layout.order, outer_latch),
                pos_of(&layout.order, fast_copy) + 1,
                "outer_latch must immediately follow fast_copy (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, outer_latch) < pos_of(&layout.order, bounds_check),
                "bounds_check must be deferred past outer spine (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, outer_latch) < pos_of(&layout.order, while_hdr),
                "while_hdr must be deferred past outer spine (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
        }
    }

    #[test]
    fn ext_tsp_one_hop_inner_loop_guard_keeps_fast_bypass() {
        let alloc = test_alloc();
        // Models `if lit_len <= 16 { goto join } else { if 16 < lit_len { while_loop } goto join }`:
        // `guard` itself has `entry_depth == 1` because it can bypass straight to `join`, so
        // 1-hop `guard_bypass`/`guard_deep_depth` metadata tells Step 7 that `guard` guards a
        // depth-2 inner loop and keeps `outer_hdr -> join` as the outer-loop spine fallthrough.
        for fast_is_then in [true, false] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let guard = b.create_block(BlockKind::Normal);
            let while_hdr = b.create_block(BlockKind::Normal);
            let join = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let lit_len = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c3 = b.iconst(Ty::I64, 3);
            let c16 = b.iconst(Ty::I64, 16);
            let is_empty = b.icmp(IntCond::Eq, n, c0);
            b.br(Flags::NONE, is_empty, (exit_blk, &[c0]), (outer_hdr, &[n, c0]));

            b.begin(outer_hdr);
            let rem = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let base = b.binary(Op::Add, Flags::NONE, acc, lit_len);
            if fast_is_then {
                let is_short = b.icmp(IntCond::Ule, lit_len, c16);
                b.br(Flags::NONE, is_short, (join, &[base]), (guard, &[base]));
            } else {
                let is_long = b.icmp(IntCond::Ugt, lit_len, c16);
                b.br(Flags::NONE, is_long, (guard, &[base]), (join, &[base]));
            }

            b.begin(guard);
            let g_acc = b.param(Ty::I64);
            let need_loop = b.icmp(IntCond::Ugt, lit_len, c16);
            b.br(Flags::NONE, need_loop, (while_hdr, &[lit_len, g_acc]), (join, &[g_acc]));

            b.begin(while_hdr);
            let k = b.param(Ty::I64);
            let w_acc = b.param(Ty::I64);
            let w_acc2 = b.binary(Op::Add, Flags::NONE, w_acc, c3);
            let k_next = b.binary(Op::Sub, Flags::NONE, k, c1);
            let k_done = b.icmp(IntCond::Eq, k_next, c0);
            b.br(Flags::NONE, k_done, (join, &[w_acc2]), (while_hdr, &[k_next, w_acc2]));

            b.begin(join);
            let j_acc = b.param(Ty::I64);
            // Model real post-copy work in `join` (e.g. sequence emission) so `join` is a regular
            // loop body continuation rather than a tiny rotatable multi-pred latch.
            let j1 = b.binary(Op::Add, Flags::NONE, j_acc, c1);
            let j2 = b.binary(Op::Xor, Flags::NONE, j1, lit_len);
            let j3 = b.binary(Op::Add, Flags::NONE, j2, c3);
            let rem_next = b.binary(Op::Sub, Flags::NONE, rem, c1);
            let seq_done = b.icmp(IntCond::Eq, rem_next, c0);
            b.br(Flags::NONE, seq_done, (exit_blk, &[j3]), (outer_hdr, &[rem_next, j3]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("1-hop inner-loop guard CFG builds");
            let view = func.view();
            let layout = compute_block_layout(&view, alloc);

            assert_valid_permutation(&layout.order, view.blocks());
            assert!(
                layout.edge_weight_between(idx_of(outer_hdr), idx_of(join))
                    > layout.edge_weight_between(idx_of(outer_hdr), idx_of(guard)),
                "outer_hdr -> join must outweigh 1-hop inner-loop guard (fast_is_then={fast_is_then})"
            );
            assert_eq!(
                pos_of(&layout.order, join),
                pos_of(&layout.order, outer_hdr) + 1,
                "join must immediately follow outer_hdr as fallthrough (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, join) < pos_of(&layout.order, guard),
                "guard must be deferred past join (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, join) < pos_of(&layout.order, while_hdr),
                "while_hdr must be deferred past join (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
        }
    }

    #[test]
    fn ext_tsp_one_hop_continue_stub_and_step7_precedence() {
        let alloc = test_alloc();

        // 1. 1-hop `continue;` stub: `valid_dist -> (match_check, cont_stub -> jump outer_hdr)`.
        //    In both branch polarities, `match_check` must be placed as fallthrough over the
        //    1-hop `continue;` stub.
        for check_is_then in [true, false] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let inner_scan = b.create_block(BlockKind::Normal);
            let long_hit = b.create_block(BlockKind::Normal);
            let cont_stub = b.create_block(BlockKind::Normal);
            let match_check = b.create_block(BlockKind::Normal);
            let emit_blk = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c4 = b.iconst(Ty::I64, 4);
            let c7 = b.iconst(Ty::I64, 7);
            b.jump((outer_hdr, &[c0, seed]));

            b.begin(outer_hdr);
            let pos = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let done = b.icmp(IntCond::Uge, pos, n);
            b.br(Flags::NONE, done, (exit_blk, &[acc]), (inner_scan, &[pos]));

            b.begin(inner_scan);
            let sp = b.param(Ty::I64);
            let sp_next = b.binary(Op::Add, Flags::NONE, sp, c1);
            let tag = b.binary(Op::And, Flags::NONE, sp_next, c7);
            let found = b.icmp(IntCond::Eq, tag, c0);
            b.br(Flags::NONE, found, (long_hit, &[sp_next]), (inner_scan, &[sp_next]));

            b.begin(long_hit);
            let hp = b.param(Ty::I64);
            if check_is_then {
                let valid = b.icmp(IntCond::Ugt, hp, c4);
                b.br(Flags::NONE, valid, (match_check, &[hp]), (cont_stub, &[hp]));
            } else {
                let invalid = b.icmp(IntCond::Ule, hp, c4);
                b.br(Flags::NONE, invalid, (cont_stub, &[hp]), (match_check, &[hp]));
            }

            b.begin(cont_stub);
            let cs_hp = b.param(Ty::I64);
            let cs_next = b.binary(Op::Add, Flags::NONE, cs_hp, c1);
            b.jump((outer_hdr, &[cs_next, acc]));

            b.begin(match_check);
            let mc_hp = b.param(Ty::I64);
            let diff = b.binary(Op::Xor, Flags::NONE, mc_hp, acc);
            let ok = b.icmp(IntCond::Eq, diff, c0);
            b.br(Flags::NONE, ok, (emit_blk, &[mc_hp, diff]), (cont_stub, &[mc_hp]));

            b.begin(emit_blk);
            let em_hp = b.param(Ty::I64);
            let em_diff = b.param(Ty::I64);
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, em_diff);
            let pos_next = b.binary(Op::Add, Flags::NONE, em_hp, c4);
            b.jump((outer_hdr, &[pos_next, acc_next]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("1-hop continue-stub CFG builds");
            let view = func.view();
            let layout = compute_block_layout(&view, alloc);

            assert_valid_permutation(&layout.order, view.blocks());
            assert!(
                layout.edge_weight_between(idx_of(long_hit), idx_of(match_check))
                    > layout.edge_weight_between(idx_of(long_hit), idx_of(cont_stub)),
                "match_check must outweigh 1-hop continue stub (check_is_then={check_is_then})"
            );
            assert_eq!(
                pos_of(&layout.order, match_check),
                pos_of(&layout.order, long_hit) + 1,
                "match_check must immediately follow long_hit (check_is_then={check_is_then}), order={:?}",
                layout.order
            );
        }

        // 2. Step 7 beats Step 5b: `if ml <= 32 { fast; jump outer_hdr } else { while_loop }`.
        //    Even though `fast` ends with `jump outer_hdr` (`is_continue_stub == true`), Step 7
        //    fires BEFORE Step 5b and keeps `fast` as fallthrough over the conditional inner loop.
        for fast_is_then in [true, false] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let check_ml = b.create_block(BlockKind::Normal);
            let fast_blk = b.create_block(BlockKind::Normal);
            let while_hdr = b.create_block(BlockKind::Normal);
            let slow_done = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let ml = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c3 = b.iconst(Ty::I64, 3);
            let c32 = b.iconst(Ty::I64, 32);
            b.jump((outer_hdr, &[n, c0]));

            b.begin(outer_hdr);
            let rem = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let done = b.icmp(IntCond::Eq, rem, c0);
            b.br(Flags::NONE, done, (exit_blk, &[acc]), (check_ml, &[]));

            b.begin(check_ml);
            if fast_is_then {
                let is_short = b.icmp(IntCond::Ule, ml, c32);
                b.br(Flags::NONE, is_short, (fast_blk, &[acc]), (while_hdr, &[ml, acc]));
            } else {
                let is_long = b.icmp(IntCond::Ugt, ml, c32);
                b.br(Flags::NONE, is_long, (while_hdr, &[ml, acc]), (fast_blk, &[acc]));
            }

            b.begin(fast_blk);
            let f_acc = b.param(Ty::I64);
            let f_next = b.binary(Op::Add, Flags::NONE, f_acc, ml);
            let f_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
            b.jump((outer_hdr, &[f_rem, f_next]));

            b.begin(while_hdr);
            let k = b.param(Ty::I64);
            let w_acc = b.param(Ty::I64);
            let w_acc2 = b.binary(Op::Add, Flags::NONE, w_acc, c3);
            let k_next = b.binary(Op::Sub, Flags::NONE, k, c1);
            let k_done = b.icmp(IntCond::Eq, k_next, c0);
            b.br(Flags::NONE, k_done, (slow_done, &[w_acc2]), (while_hdr, &[k_next, w_acc2]));

            b.begin(slow_done);
            let s_acc = b.param(Ty::I64);
            let s_rem = b.binary(Op::Sub, Flags::NONE, rem, c1);
            b.jump((outer_hdr, &[s_rem, s_acc]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("Step 7 over Step 5b CFG builds");
            let view = func.view();
            let layout = compute_block_layout(&view, alloc);

            assert_valid_permutation(&layout.order, view.blocks());
            assert!(
                layout.edge_weight_between(idx_of(check_ml), idx_of(fast_blk))
                    > layout.edge_weight_between(idx_of(check_ml), idx_of(while_hdr)),
                "Step 7 must beat Step 5b so fast;continue outweighs while_hdr (fast_is_then={fast_is_then})"
            );
            assert_eq!(
                pos_of(&layout.order, fast_blk),
                pos_of(&layout.order, check_ml) + 1,
                "fast_blk must immediately follow check_ml as fallthrough (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, fast_blk) < pos_of(&layout.order, while_hdr),
                "while_hdr must be placed after fast_blk (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
        }
    }

    #[test]
    fn ext_tsp_fast_lut_diamond_vs_slow_clz_post_isel_size() {
        let alloc = test_alloc();
        // Models `if ml < 128 { fast_lut } else { slow_clz }` in both polarities:
        // - `fast_lut` has 9 LIR ops (`Zext + Shl(const 2) + PtrAdd + Load + Lshr(const 16) +
        //   Sub + Lshr(const 8) + And + Jump`) which fold to 3 post-isel insts (`12 B`).
        // - `slow_clz` has 7 LIR ops (`Clz + Sub + Shl(var) + Add + Sub + Add + Jump`) which
        //   emit 6 post-isel insts (`24 B`).
        // Step 8b (`740` vs `284`) must lay out `hdr -> fast_lut -> merge` contiguously with
        // `slow_clz` moved out-of-line.
        for fast_is_then in [true, false] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[
                    Param::of(Ty::I64),
                    Param::of(Ty::I32),
                    Param::of(Ty::PTR),
                    Param::of(Ty::I32),
                ],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let hdr = b.create_block(BlockKind::Normal);
            let fast_lut = b.create_block(BlockKind::Normal);
            let slow_clz = b.create_block(BlockKind::Normal);
            let merge = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let ml = b.param(Ty::I32);
            let lut_ptr = b.param(Ty::PTR);
            let var_sh = b.param(Ty::I32);
            let c0_64 = b.iconst(Ty::I64, 0);
            let c1_64 = b.iconst(Ty::I64, 1);
            let c2_64 = b.iconst(Ty::I64, 2);
            let c1_32 = b.iconst(Ty::I32, 1);
            let c8_32 = b.iconst(Ty::I32, 8);
            let c16_32 = b.iconst(Ty::I32, 16);
            let c31_32 = b.iconst(Ty::I32, 31);
            let c128_32 = b.iconst(Ty::I32, 128);
            let c255_32 = b.iconst(Ty::I32, 255);
            b.jump((hdr, &[n, c0_64]));

            b.begin(hdr);
            let rem = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            if fast_is_then {
                let is_short = b.icmp(IntCond::Ult, ml, c128_32);
                b.br(Flags::NONE, is_short, (fast_lut, &[]), (slow_clz, &[]));
            } else {
                let is_long = b.icmp(IntCond::Uge, ml, c128_32);
                b.br(Flags::NONE, is_long, (slow_clz, &[]), (fast_lut, &[]));
            }

            // Fast LUT arm: Zext + Shl(const 2) + PtrAdd + Load + Lshr(const 16) + Sub
            // + Lshr(const 8) + And + Jump => 3 post-isel insts (12 B).
            b.begin(fast_lut);
            let idx64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, ml);
            let byte_off = b.binary(Op::Shl, Flags::NONE, idx64, c2_64);
            let elem_ptr = b.ptr_add(Flags::NONE, lut_ptr, byte_off);
            let packed = b.load(Ty::I32, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(2));
            let hi = b.binary(Op::Lshr, Flags::NONE, packed, c16_32);
            let diff = b.binary(Op::Sub, Flags::NONE, ml, hi);
            let mid = b.binary(Op::Lshr, Flags::NONE, packed, c8_32);
            let code = b.binary(Op::And, Flags::NONE, mid, c255_32);
            b.jump((merge, &[diff, code]));

            // Slow Clz arm: Clz + Sub + Shl(var) + Add + Sub + Add + Jump => 6 post-isel insts (24 B).
            b.begin(slow_clz);
            let lz = b.unary(Op::Clz, Flags::NONE, ml);
            let bits = b.binary(Op::Sub, Flags::NONE, c31_32, lz);
            let shifted = b.binary(Op::Shl, Flags::NONE, c1_32, var_sh);
            let s1 = b.binary(Op::Add, Flags::NONE, bits, shifted);
            let s2 = b.binary(Op::Sub, Flags::NONE, ml, shifted);
            let s3 = b.binary(Op::Add, Flags::NONE, s1, c8_32);
            b.jump((merge, &[s2, s3]));

            b.begin(merge);
            let m_a = b.param(Ty::I32);
            let m_b = b.param(Ty::I32);
            let m_sum32 = b.binary(Op::Add, Flags::NONE, m_a, m_b);
            let m_sum64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, m_sum32);
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, m_sum64);
            let rem_next = b.binary(Op::Sub, Flags::NONE, rem, c1_64);
            let done = b.icmp(IntCond::Eq, rem_next, c0_64);
            b.br(Flags::NONE, done, (exit_blk, &[acc_next]), (hdr, &[rem_next, acc_next]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("fast LUT vs slow Clz diamond CFG builds");
            let view = func.view();
            assert_eq!(block_size_bytes(&view, idx_of(fast_lut)), 12);
            assert_eq!(block_size_bytes(&view, idx_of(slow_clz)), 24);

            let layout = compute_block_layout(&view, alloc);
            assert_valid_permutation(&layout.order, view.blocks());
            assert!(
                layout.edge_weight_between(idx_of(hdr), idx_of(fast_lut))
                    > layout.edge_weight_between(idx_of(hdr), idx_of(slow_clz)),
                "fast_lut must outweigh slow_clz (fast_is_then={fast_is_then})"
            );
            assert_eq!(
                pos_of(&layout.order, fast_lut),
                pos_of(&layout.order, hdr) + 1,
                "fast_lut must immediately follow hdr (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert_eq!(
                pos_of(&layout.order, merge),
                pos_of(&layout.order, fast_lut) + 1,
                "merge must immediately follow fast_lut (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
            assert!(
                pos_of(&layout.order, slow_clz) > pos_of(&layout.order, merge),
                "slow_clz must be moved out-of-line after merge (fast_is_then={fast_is_then}), order={:?}",
                layout.order
            );
        }
    }

    #[test]
    fn ext_tsp_compact_fast_path_does_not_misfire_on_else_if_cascade() {
        let alloc = test_alloc();
        // Models `decode_exec_leaf_single`'s `if/else if` cascade:
        //   if offset >= 32 { fast_32b (5 insts = 20 B); jump latch }
        //   else { if offset >= 16 { mid_16b; jump latch } else { small_lt16; jump latch } }
        // `else_if` is only 2 insts (`8 B`: `icmp + br`) vs `fast_32b`'s 5 insts (`20 B`).
        // Because `else_if` terminates in `Op::Br` rather than `Op::Jump` to `latch`, they do
        // NOT form a true diamond, so Step 8b must NOT fire and starve `fast_32b` below `else_if`.
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let hdr = b.create_block(BlockKind::Normal);
        let fast_32b = b.create_block(BlockKind::Normal);
        let else_if = b.create_block(BlockKind::Normal);
        let mid_16b = b.create_block(BlockKind::Normal);
        let small_lt16 = b.create_block(BlockKind::Normal);
        let latch = b.create_block(BlockKind::Normal);
        let exit_blk = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let offset = b.param(Ty::I64);
        let c0 = b.iconst(Ty::I64, 0);
        let c1 = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c16 = b.iconst(Ty::I64, 16);
        let c32 = b.iconst(Ty::I64, 32);
        b.jump((hdr, &[n, c0]));

        b.begin(hdr);
        let rem = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let is_ge32 = b.icmp(IntCond::Uge, offset, c32);
        b.br(Flags::NONE, is_ge32, (fast_32b, &[acc]), (else_if, &[acc]));

        // Fast 32B arm: 5 post-isel insts (`20 B`, `Jump` skipped).
        b.begin(fast_32b);
        let mut f = b.param(Ty::I64);
        for _ in 0..5 {
            f = b.binary(Op::Add, Flags::NONE, f, offset);
        }
        b.jump((latch, &[f]));

        // `else if offset >= 16` branch block: 2 post-isel insts (`8 B`: `icmp + br`).
        b.begin(else_if);
        let e_acc = b.param(Ty::I64);
        let is_ge16 = b.icmp(IntCond::Uge, offset, c16);
        b.br(Flags::NONE, is_ge16, (mid_16b, &[e_acc]), (small_lt16, &[e_acc]));

        b.begin(mid_16b);
        let m_acc = b.param(Ty::I64);
        let m1 = b.binary(Op::Add, Flags::NONE, m_acc, c16);
        let m2 = b.binary(Op::Xor, Flags::NONE, m1, offset);
        b.jump((latch, &[m2]));

        b.begin(small_lt16);
        let s_acc = b.param(Ty::I64);
        let s1 = b.binary(Op::Add, Flags::NONE, s_acc, c3);
        let s2 = b.binary(Op::Xor, Flags::NONE, s1, offset);
        b.jump((latch, &[s2]));

        b.begin(latch);
        let l_acc = b.param(Ty::I64);
        let l1 = b.binary(Op::Add, Flags::NONE, l_acc, c1);
        let l2 = b.binary(Op::Xor, Flags::NONE, l1, offset);
        let rem_next = b.binary(Op::Sub, Flags::NONE, rem, c1);
        let done = b.icmp(IntCond::Eq, rem_next, c0);
        b.br(Flags::NONE, done, (exit_blk, &[l2]), (hdr, &[rem_next, l2]));

        b.begin(exit_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().expect("else-if cascade CFG builds");
        let view = func.view();
        assert_eq!(block_size_bytes(&view, idx_of(fast_32b)), 20);
        assert_eq!(block_size_bytes(&view, idx_of(else_if)), 8);

        let layout = compute_block_layout(&view, alloc);
        assert_valid_permutation(&layout.order, view.blocks());
        assert!(
            layout.edge_weight_between(idx_of(hdr), idx_of(fast_32b))
                >= layout.edge_weight_between(idx_of(hdr), idx_of(else_if)),
            "Step 8b must not misfire on else-if cascade and starve fast_32b below else_if"
        );
        assert_eq!(
            pos_of(&layout.order, fast_32b),
            pos_of(&layout.order, hdr) + 1,
            "fast_32b must immediately follow hdr as fallthrough, order={:?}",
            layout.order
        );
    }

    #[test]
    fn ext_tsp_entry_depth_dist_cap_prevents_far_inner_loop_starving_match_body() {
        let alloc = test_alloc();
        // Models `lz77_scan_default_leaf`:
        //   outer_hdr -> match_verify -> (
        //     match_check -> rbit -> lazy -> emit_seq -> while_back_ext -> post_ext -> jump outer_hdr,
        //     cont_stub -> jump outer_hdr
        //   )
        // `while_back_ext` is a depth-2 inner loop 4 forward hops past `match_check`.
        // Because `entry_depth` propagation stops at `MAX_ENTRY_DEPTH_DIST = 2` hops (`emit_seq`
        // dist 1, `lazy` dist 2), `rbit` and `match_check` retain `entry_depth == 1`, so Step 7
        // does not mistake the hot match-emission body for a rare inner-loop guard and Step 5b
        // keeps `match_check` as the fallthrough over `cont_stub` in both branch polarities.
        for check_is_then in [true, false] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let outer_hdr = b.create_block(BlockKind::Normal);
            let match_verify = b.create_block(BlockKind::Normal);
            let match_check = b.create_block(BlockKind::Normal);
            let rbit = b.create_block(BlockKind::Normal);
            let lazy = b.create_block(BlockKind::Normal);
            let emit_seq = b.create_block(BlockKind::Normal);
            let while_back_ext = b.create_block(BlockKind::Normal);
            let post_ext = b.create_block(BlockKind::Normal);
            let cont_stub = b.create_block(BlockKind::Normal);
            let exit_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let c0 = b.iconst(Ty::I64, 0);
            let c1 = b.iconst(Ty::I64, 1);
            let c4 = b.iconst(Ty::I64, 4);
            let c7 = b.iconst(Ty::I64, 7);
            b.jump((outer_hdr, &[c0, seed]));

            b.begin(outer_hdr);
            let pos = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let done = b.icmp(IntCond::Uge, pos, n);
            b.br(Flags::NONE, done, (exit_blk, &[acc]), (match_verify, &[pos]));

            b.begin(match_verify);
            let vp = b.param(Ty::I64);
            if check_is_then {
                let valid = b.icmp(IntCond::Ugt, vp, c4);
                b.br(Flags::NONE, valid, (match_check, &[vp]), (cont_stub, &[vp]));
            } else {
                let invalid = b.icmp(IntCond::Ule, vp, c4);
                b.br(Flags::NONE, invalid, (cont_stub, &[vp]), (match_check, &[vp]));
            }

            b.begin(match_check);
            let mc_p = b.param(Ty::I64);
            let diff = b.binary(Op::Xor, Flags::NONE, mc_p, acc);
            b.jump((rbit, &[mc_p, diff]));

            b.begin(rbit);
            let rb_p = b.param(Ty::I64);
            let rb_d = b.param(Ty::I64);
            let rb_len = b.binary(Op::Add, Flags::NONE, rb_d, c1);
            b.jump((lazy, &[rb_p, rb_len]));

            b.begin(lazy);
            let lz_p = b.param(Ty::I64);
            let lz_len = b.param(Ty::I64);
            let lz_adj = b.binary(Op::Xor, Flags::NONE, lz_len, c7);
            b.jump((emit_seq, &[lz_p, lz_adj]));

            b.begin(emit_seq);
            let es_p = b.param(Ty::I64);
            let es_len = b.param(Ty::I64);
            let es_acc = b.binary(Op::Add, Flags::NONE, acc, es_len);
            b.jump((while_back_ext, &[es_p, es_acc]));

            b.begin(while_back_ext);
            let wb_k = b.param(Ty::I64);
            let wb_acc = b.param(Ty::I64);
            let wb_acc2 = b.binary(Op::Add, Flags::NONE, wb_acc, c1);
            let wb_k_next = b.binary(Op::Sub, Flags::NONE, wb_k, c1);
            let wb_done = b.icmp(IntCond::Eq, wb_k_next, c0);
            b.br(
                Flags::NONE,
                wb_done,
                (post_ext, &[wb_acc2]),
                (while_back_ext, &[wb_k_next, wb_acc2]),
            );

            b.begin(post_ext);
            let pe_acc = b.param(Ty::I64);
            let pe_pos = b.binary(Op::Add, Flags::NONE, pos, c4);
            b.jump((outer_hdr, &[pe_pos, pe_acc]));

            b.begin(cont_stub);
            let cs_p = b.param(Ty::I64);
            let cs_next = b.binary(Op::Add, Flags::NONE, cs_p, c1);
            b.jump((outer_hdr, &[cs_next, acc]));

            b.begin(exit_blk);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b
                .finish_checked()
                .expect("far inner-loop match body CFG builds");
            let view = func.view();
            let layout = compute_block_layout(&view, alloc);

            assert_valid_permutation(&layout.order, view.blocks());
            assert!(
                layout.edge_weight_between(idx_of(match_verify), idx_of(match_check))
                    > layout.edge_weight_between(idx_of(match_verify), idx_of(cont_stub)),
                "match_check must outweigh cont_stub despite far inner loop (check_is_then={check_is_then})"
            );
            assert_eq!(
                pos_of(&layout.order, match_check),
                pos_of(&layout.order, match_verify) + 1,
                "match_check must immediately follow match_verify as fallthrough (check_is_then={check_is_then}), order={:?}",
                layout.order
            );
        }
    }
}
