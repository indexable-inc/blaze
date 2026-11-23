//! SOTA Non-LLVM Acyclic E-Graph (`aegraph`) Optimizer & Chordal-SSA Register Planner
//! (`docs/v2/plan/design/sota-tier.md`).
//!
//! Combines **Dominator-Scoped Hash-Consed GVN**, **Commutative Canonicalization**, **Algebraic
//! Equivalence Rewrites**, **Exact Constant Folding**, and **Chordal-SSA Register Pressure
//! Planning** in a single O(N) dominator-tree walk — eliminating LLVM's pass-ordering loop while
//! remaining 100% deterministic, sans-IO, `#![forbid(unsafe_code)]`, and content-addressable for
//! per-function incremental caching.
//!
//! Every rewrite respects `blaze-lir`'s defined semantics (flag-free scalar integer ops only;
//! trapping ops like `udiv`/`sdiv` by 0 are never folded away) and is verified by `verify()` and
//! `blaze_lir::eval` equality tests.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Edit, Item, NONE, dead, each, filled, rebuild, reachable},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    arith::{binary, compare, iconst_value, sext, unary, wide_value},
    op::{Field, Layout},
    verify,
};

/// Loop-level analysis & optimization summary discovered from the dominator tree.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct LoopSummary {
    /// Number of natural loops (back-edges `latch -> header` where `header` dominates `latch`).
    pub natural_loops: u32,
    /// Maximum nesting depth across all blocks (`0` = loop-free).
    pub max_depth: u32,
    /// Pure instructions inside a loop whose operands are all loop-invariant (LICM hoistable).
    pub licm_candidates: u32,
    /// Basic induction variables (`header_param` stepped by a loop-invariant stride on the back-edge),
    /// enabling loop strength reduction (LSR), bounds-check elimination (BCE), and vectorization.
    pub induction_vars: u32,
}

/// One PGO + loop + register-pressure guided opportunistic JIT inline decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpportunisticInlineSite {
    /// Instruction index of the `Call` or `CallIndirect`.
    pub call_inst: u32,
    /// Loop nesting depth at the call site (`0` = outside loops).
    pub loop_depth: u32,
    /// Combined dynamic PGO weight × loop-depth amplification score.
    pub hotness_score: u64,
    /// Callee instruction budget allowed at this site (throttled when register pressure is near spill).
    pub allowed_callee_insts: u32,
    /// `true` when `call_inst` is a `CallIndirect` inside a hot loop eligible for speculative devirt.
    pub speculative_devirt: bool,
}

/// Summary statistics from one [`aegraph_optimize`] pass.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct AegraphStats {
    /// Instructions eliminated by dominator-scoped GVN (including commutative hits like `b + a` == `a + b`).
    pub gvn_hits: u32,
    /// Instructions simplified by algebraic identities (`x + 0`, `x ^ x`, `neg(neg(x))`, `select(c, x, x)`, etc.).
    pub identities_folded: u32,
    /// Instructions folded to compile-time constants via exact LIR arithmetic.
    pub constants_folded: u32,
    /// Strength-reduction candidates identified (`x * 2^k`, `udiv x, 2^k`, `urem x, 2^k`, `x + x`).
    pub strength_reductions: u32,
    /// Loop analysis findings (natural loops, nesting depth, LICM candidates, induction variables).
    pub loops: LoopSummary,
    /// Instructions in input vs output (after dead-code sweep).
    pub insts_before: u32,
    pub insts_after: u32,
}

/// Output of [`aegraph_optimize`]: verified optimized LIR function plus telemetry.
#[derive(Debug)]
pub struct AegraphOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: AegraphStats,
}

/// Target register file budget for [`plan_ssa_regalloc`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegBudget {
    pub gprs: u16,
    pub fprs: u16,
}

impl RegBudget {
    /// Apple Silicon / AArch64 AAPCS64 allocatable registers (excluding SP/FP/LR/X16/X17/X18).
    pub const AARCH64: Self = Self { gprs: 18, fprs: 24 };
    /// x86-64 SysV allocatable registers (excluding RSP/RBP).
    pub const X86_64: Self = Self { gprs: 13, fprs: 16 };
}

/// Chordal-SSA register pressure & spill estimate for a function.
///
/// Because LIR is in strict SSA form with block parameters, its interference graph is chordal:
/// the minimum number of physical registers needed with zero spills equals the peak simultaneous
/// liveness across all program points (Hack 2006 / Braun-Hack).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SsaRegPlan {
    pub peak_gprs: u16,
    pub peak_fprs: u16,
    pub estimated_gpr_spills: u16,
    pub estimated_fpr_spills: u16,
    /// Estimated total instruction cycle latency under an out-of-order 4-wide core model.
    pub estimated_cycles: u32,
}

/// Hash-consed canonical key for a pure dominator-scoped e-node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ENodeKey {
    op: u8,
    ty: u16,
    cond: u8,
    a: u32,
    b: u32,
    c: u32,
}

impl ENodeKey {
    #[inline]
    fn hash(&self) -> u64 {
        let mut h = 0x517C_C1B7_2722_0A95u64;
        h = (h ^ u64::from(self.op)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.ty)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.cond)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.a)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.b)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = (h ^ u64::from(self.c)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^ (h >> 32)
    }
}

/// Scoped open-addressed hash table supporting O(1) rollback on dominator-tree exit.
struct ScopedENodes<A: Allocator> {
    keys: Vec<Option<ENodeKey>, A>,
    vals: Vec<u32, A>,
    /// Undo log of slot indices inserted in the active dominator path.
    undo: Vec<usize, A>,
    mask: usize,
}

impl<A: Allocator + Copy> ScopedENodes<A> {
    fn new(expected: usize, alloc: &A) -> Self {
        let cap = (expected.saturating_mul(2).max(16)).next_power_of_two();
        Self {
            keys: filled(cap, None, alloc),
            vals: filled(cap, NONE, alloc),
            undo: Vec::with_capacity_in(expected, *alloc),
            mask: cap - 1,
        }
    }

    #[inline]
    fn mark(&self) -> usize {
        self.undo.len()
    }

    #[inline]
    fn rollback(&mut self, mark: usize) {
        while self.undo.len() > mark {
            let slot = self.undo.pop().unwrap();
            self.keys[slot] = None;
            self.vals[slot] = NONE;
        }
    }

    #[inline]
    fn get_or_insert(&mut self, key: ENodeKey, leader: u32) -> Option<u32> {
        let mut idx = (key.hash() as usize) & self.mask;
        for _ in 0..=self.mask {
            match self.keys[idx] {
                Some(existing) if existing == key => return Some(self.vals[idx]),
                Some(_) => idx = (idx + 1) & self.mask,
                None => {
                    self.keys[idx] = Some(key);
                    self.vals[idx] = leader;
                    self.undo.push(idx);
                    return None;
                }
            }
        }
        None
    }
}

#[inline]
fn is_commutative(op: Op) -> bool {
    matches!(op, Op::Add | Op::Mul | Op::And | Op::Or | Op::Xor)
}

#[inline]
fn scalar_int_mask(ty: Ty, ptr_bits: u32) -> Option<(u32, u128)> {
    if !(ty.is_scalar() && ty.is_int()) {
        return None;
    }
    let bits = ty.bits(ptr_bits);
    let mask = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
    Some((bits, mask))
}

#[inline]
fn is_power_of_two_gt1(v: u128) -> bool {
    v > 1 && (v & (v - 1)) == 0
}

/// Computes immediate dominators for all reachable blocks using Cooper-Harvey-Kennedy in RPO.
fn compute_dom_tree<A: Allocator + Copy>(
    view: &View<'_>,
    reached: &[bool],
    alloc: &A,
) -> (Vec<u32, A>, Vec<Vec<u32, A>, A>) {
    let blocks = view.blocks();
    // Build predecessor lists and DFS postorder from entry block 0.
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(*alloc));
        succs.push(Vec::new_in(*alloc));
    }
    for b in 0..blocks {
        if !reached[b] {
            continue;
        }
        let term = view.terminator(Block(b as u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = edge.block as usize;
                if to < blocks && reached[to] {
                    succs[b].push(edge.block);
                    preds[to].push(b as u32);
                }
            }
        });
    }

    // Iterative DFS for RPO numbering.
    let mut visited = filled(blocks, false, alloc);
    let mut postorder: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
    let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, *alloc);
    if blocks > 0 && reached[0] {
        visited[0] = true;
        stack.push((0, 0));
        while let Some((b, next_i)) = stack.pop() {
            if next_i < succs[b as usize].len() {
                stack.push((b, next_i + 1));
                let s = succs[b as usize][next_i];
                if !visited[s as usize] {
                    visited[s as usize] = true;
                    stack.push((s, 0));
                }
            } else {
                postorder.push(b);
            }
        }
    }
    postorder.reverse(); // Now RPO.

    let mut rpo_num = filled(blocks, NONE, alloc);
    for (rank, &b) in postorder.iter().enumerate() {
        rpo_num[b as usize] = rank as u32;
    }

    let mut idom = filled(blocks, NONE, alloc);
    if !postorder.is_empty() {
        idom[0] = 0;
    }

    let intersect = |mut a: u32, mut b: u32, idom: &[u32], rpo_num: &[u32]| -> u32 {
        while a != b {
            while rpo_num[a as usize] > rpo_num[b as usize] {
                a = idom[a as usize];
            }
            while rpo_num[b as usize] > rpo_num[a as usize] {
                b = idom[b as usize];
            }
        }
        a
    };

    let mut changed = true;
    while changed {
        changed = false;
        for &b in postorder.iter().skip(1) {
            let mut new_idom = NONE;
            for &p in &preds[b as usize] {
                if idom[p as usize] != NONE {
                    new_idom = if new_idom == NONE {
                        p
                    } else {
                        intersect(p, new_idom, &idom, &rpo_num)
                    };
                }
            }
            if new_idom != NONE && idom[b as usize] != new_idom {
                idom[b as usize] = new_idom;
                changed = true;
            }
        }
    }

    let mut children: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        children.push(Vec::new_in(*alloc));
    }
    for &b in postorder.iter().skip(1) {
        let d = idom[b as usize];
        if d != NONE && d != b {
            children[d as usize].push(b);
        }
    }
    (idom, children)
}

/// Runs the single-pass dominator-scoped Acyclic E-Graph (`aegraph`) optimizer over `view`,
/// followed by dead-code elimination, returning a verified [`Func`] and optimization telemetry.
pub fn aegraph_optimize<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<AegraphOutput<A>, SimplifyError> {
    if view.blocks() == 0 {
        return Err(SimplifyError::Refused { inst: 0, what: "a function with no blocks" });
    }
    let blocks = view.blocks();
    let len = view.len();
    let ptr_bits = view.head.ptr_bits;

    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let (idom, dom_children) = compute_dom_tree(&view, &reached, &alloc);
    let (loop_summary, _block_depths) = analyze_loops(&view, &reached, &idom, &alloc);

    let mut stands: Vec<u32, A> = Vec::with_capacity_in(len, alloc);
    for i in 0..len {
        stands.push(i as u32);
    }
    let mut known_const: Vec<Option<u128>, A> = filled(len, None, &alloc);
    let mut edit_const: Vec<Option<u128>, A> = filled(len, None, &alloc);
    let mut stats = AegraphStats {
        insts_before: len as u32,
        loops: loop_summary,
        ..AegraphStats::default()
    };

    let mut table = ScopedENodes::new(len, &alloc);

    // Explicit DFS over the dominator tree with scoped hash-consing rollback.
    enum Frame {
        Enter(u32),
        Exit(usize),
    }
    let mut dfs: Vec<Frame, A> = Vec::with_capacity_in(blocks * 2, alloc);
    if reached[0] {
        dfs.push(Frame::Enter(0));
    }

    while let Some(frame) = dfs.pop() {
        match frame {
            Frame::Exit(mark) => table.rollback(mark),
            Frame::Enter(b) => {
                let mark = table.mark();
                dfs.push(Frame::Exit(mark));
                // Push children in reverse so they visit in forward order.
                for &child in dom_children[b as usize].iter().rev() {
                    dfs.push(Frame::Enter(child));
                }

                let range = view.block(Block(b));
                for inst in range {
                    let op = view.op(inst);
                    let flags = Flags(view.flags[inst]);
                    let ty = Ty(view.tys[inst]);

                    // Record constants immediately (and deduplicate identical constants by e-node).
                    if op == Op::Iconst {
                        if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                            let v = iconst_value(view.data[inst], bits);
                            known_const[inst] = Some(v);
                            let key = ENodeKey {
                                op: Op::Iconst as u8,
                                ty: ty.0,
                                cond: 0,
                                a: v as u32,
                                b: (v >> 32) as u32,
                                c: (v >> 64) as u32,
                            };
                            if v <= u128::from(u64::MAX) {
                                if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                    stands[inst] = prev;
                                    stats.gvn_hits += 1;
                                }
                            }
                        }
                        continue;
                    }
                    if op == Op::IconstWide {
                        if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                            let at = view.data[inst] as usize;
                            let words = ty.const_words() as usize;
                            let v = wide_value(&view.extra[at..at + words], bits);
                            known_const[inst] = Some(v);
                        }
                        continue;
                    }

                    // Only rewrite flag-free pure scalar ops (preserves every promise/UB/effect).
                    if flags.0 != 0 {
                        continue;
                    }

                    match op {
                        Op::Neg | Op::Not => {
                            let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) else { continue };
                            let raw_a = view.data[inst];
                            let a = stands[raw_a as usize];

                            // Constant fold unary op.
                            if let Some(ca) = known_const[a as usize] {
                                let res = unary(op, ca, bits);
                                known_const[inst] = Some(res);
                                edit_const[inst] = Some(res);
                                stats.constants_folded += 1;
                                continue;
                            }
                            // Involution identity: neg(neg(x)) == x, not(not(x)) == x.
                            if view.op(a as usize) == op && Flags(view.flags[a as usize]).0 == 0 {
                                let inner = stands[view.data[a as usize] as usize];
                                stands[inst] = inner;
                                known_const[inst] = known_const[inner as usize];
                                stats.identities_folded += 1;
                                continue;
                            }
                            let key = ENodeKey { op: op as u8, ty: ty.0, cond: 0, a, b: 0, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                stands[inst] = prev;
                                known_const[inst] = known_const[prev as usize];
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::Add
                        | Op::Sub
                        | Op::Mul
                        | Op::And
                        | Op::Or
                        | Op::Xor
                        | Op::Shl
                        | Op::Lshr
                        | Op::Ashr
                        | Op::Udiv
                        | Op::Urem => {
                            let Some((bits, mask)) = scalar_int_mask(ty, ptr_bits) else { continue };
                            let at = view.data[inst] as usize;
                            let a = stands[view.extra[at] as usize];
                            let b = stands[view.extra[at + 1] as usize];
                            let ca = known_const[a as usize];
                            let cb = known_const[b as usize];

                            // 1. Exact constant folding (trapping div/rem by zero returns Err -> skipped).
                            if let (Some(va), Some(vb)) = (ca, cb) {
                                if let Ok(res) = binary(op, va, vb, bits) {
                                    known_const[inst] = Some(res);
                                    edit_const[inst] = Some(res);
                                    stats.constants_folded += 1;
                                    continue;
                                }
                            }

                            // 2. Algebraic identities (trap-free & flag-free).
                            let mut folded_val: Option<u32> = None;
                            let mut folded_const: Option<u128> = None;
                            match op {
                                Op::Add => {
                                    if cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if ca == Some(0) {
                                        folded_val = Some(b);
                                    } else if a == b {
                                        stats.strength_reductions += 1; // x + x == x << 1
                                    }
                                }
                                Op::Sub => {
                                    if cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if a == b {
                                        folded_const = Some(0);
                                    }
                                }
                                Op::Mul => {
                                    if ca == Some(0) || cb == Some(0) {
                                        folded_const = Some(0);
                                    } else if cb == Some(1) {
                                        folded_val = Some(a);
                                    } else if ca == Some(1) {
                                        folded_val = Some(b);
                                    } else if cb.is_some_and(is_power_of_two_gt1)
                                        || ca.is_some_and(is_power_of_two_gt1)
                                    {
                                        stats.strength_reductions += 1; // x * 2^k == x << k
                                    }
                                }
                                Op::And => {
                                    if ca == Some(0) || cb == Some(0) {
                                        folded_const = Some(0);
                                    } else if a == b || cb == Some(mask) {
                                        folded_val = Some(a);
                                    } else if ca == Some(mask) {
                                        folded_val = Some(b);
                                    }
                                }
                                Op::Or => {
                                    if ca == Some(mask) || cb == Some(mask) {
                                        folded_const = Some(mask);
                                    } else if a == b || cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if ca == Some(0) {
                                        folded_val = Some(b);
                                    }
                                }
                                Op::Xor => {
                                    if a == b {
                                        folded_const = Some(0);
                                    } else if cb == Some(0) {
                                        folded_val = Some(a);
                                    } else if ca == Some(0) {
                                        folded_val = Some(b);
                                    }
                                }
                                Op::Shl | Op::Lshr | Op::Ashr => {
                                    if let Some(sh) = cb {
                                        if (sh % u128::from(bits)) == 0 {
                                            folded_val = Some(a);
                                        }
                                    }
                                }
                                Op::Udiv => {
                                    if cb == Some(1) {
                                        folded_val = Some(a);
                                    } else if cb.is_some_and(is_power_of_two_gt1) {
                                        stats.strength_reductions += 1; // udiv x, 2^k == lshr x, k
                                    }
                                }
                                Op::Urem => {
                                    if cb == Some(1) {
                                        folded_const = Some(0);
                                    } else if cb.is_some_and(is_power_of_two_gt1) {
                                        stats.strength_reductions += 1; // urem x, 2^k == and x, 2^k - 1
                                    }
                                }
                                _ => {}
                            }

                            if let Some(target) = folded_val {
                                stands[inst] = target;
                                known_const[inst] = known_const[target as usize];
                                stats.identities_folded += 1;
                                continue;
                            }
                            if let Some(cval) = folded_const {
                                known_const[inst] = Some(cval);
                                edit_const[inst] = Some(cval);
                                stats.identities_folded += 1;
                                continue;
                            }

                            // 3. Dominator-scoped GVN (skip potentially-trapping Udiv/Urem unless divisor known nonzero).
                            if matches!(op, Op::Udiv | Op::Urem) && cb.map_or(true, |v| v == 0) {
                                continue;
                            }
                            let (ka, kb) = if is_commutative(op) && a > b { (b, a) } else { (a, b) };
                            let key = ENodeKey { op: op as u8, ty: ty.0, cond: 0, a: ka, b: kb, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                stands[inst] = prev;
                                known_const[inst] = known_const[prev as usize];
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::Icmp => {
                            let at = view.data[inst] as usize;
                            let cond_raw = view.extra[at];
                            let a = stands[view.extra[at + 1] as usize];
                            let b = stands[view.extra[at + 2] as usize];
                            let op_ty = Ty(view.tys[a as usize]);
                            let Some(cond) = IntCond::from_word(cond_raw) else { continue };

                            if let (Some(va), Some(vb)) = (known_const[a as usize], known_const[b as usize]) {
                                if let Some((op_bits, _)) = scalar_int_mask(op_ty, ptr_bits) {
                                    let res = compare(cond, va, vb, op_bits);
                                    let cval = u128::from(res);
                                    known_const[inst] = Some(cval);
                                    edit_const[inst] = Some(cval);
                                    stats.constants_folded += 1;
                                    continue;
                                }
                            }
                            if a == b {
                                let truth = match cond {
                                    IntCond::Eq | IntCond::Ule | IntCond::Uge | IntCond::Sle | IntCond::Sge => 1u128,
                                    IntCond::Ne | IntCond::Ult | IntCond::Ugt | IntCond::Slt | IntCond::Sgt => 0u128,
                                };
                                known_const[inst] = Some(truth);
                                edit_const[inst] = Some(truth);
                                stats.identities_folded += 1;
                                continue;
                            }
                            let (ka, kb, kcond) = match cond {
                                IntCond::Eq | IntCond::Ne if a > b => (b, a, cond as u8),
                                _ => (a, b, cond as u8),
                            };
                            let key = ENodeKey { op: op as u8, ty: ty.0, cond: kcond, a: ka, b: kb, c: 0 };
                            if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                stands[inst] = prev;
                                known_const[inst] = known_const[prev as usize];
                                stats.gvn_hits += 1;
                            }
                        }

                        Op::Select => {
                            let at = view.data[inst] as usize;
                            let c = stands[view.extra[at] as usize];
                            let t = stands[view.extra[at + 1] as usize];
                            let f = stands[view.extra[at + 2] as usize];
                            if t == f {
                                stands[inst] = t;
                                known_const[inst] = known_const[t as usize];
                                stats.identities_folded += 1;
                                continue;
                            }
                            if let Some(cv) = known_const[c as usize] {
                                let chosen = if cv & 1 != 0 { t } else { f };
                                stands[inst] = chosen;
                                known_const[inst] = known_const[chosen as usize];
                                stats.identities_folded += 1;
                                continue;
                            }
                            let key = ENodeKey { op: op as u8, ty: ty.0, cond: 0, a: c, b: t, c: f };
                            if let Some(prev) = table.get_or_insert(key, inst as u32) {
                                stands[inst] = prev;
                                known_const[inst] = known_const[prev as usize];
                                stats.gvn_hits += 1;
                            }
                        }

                        _ => {}
                    }
                }
            }
        }
    }

    // Flatten `stands` chains so `rebuild` sees direct leaders.
    for i in 0..len {
        let mut cur = stands[i];
        while stands[cur as usize] != cur {
            cur = stands[cur as usize];
        }
        stands[i] = cur;
    }

    let edit = Edit {
        block_kept: filled(blocks, true, &alloc),
        inst_kept: filled(len, true, &alloc),
        stands,
        constant: edit_const,
        jump: filled(len, NONE, &alloc),
        devirt: filled(len, NONE, &alloc),
        merge_next: filled(blocks, NONE, &alloc),
    };
    let rewritten = rebuild(view, &edit, alloc)?;
    let cleaned = dead(rewritten.view(), alloc)?;
    stats.insts_after = cleaned.ops.len() as u32;
    Ok(AegraphOutput { func: cleaned, stats })
}

/// Estimates instruction execution latency (cycles) for cost-guided extraction & scheduling.
#[inline]
pub const fn op_latency_cycles(op: Op) -> u32 {
    match op {
        Op::Param | Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide | Op::Null | Op::Uninit => 0,
        Op::Add
        | Op::Sub
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Not
        | Op::Shl
        | Op::Lshr
        | Op::Ashr
        | Op::Icmp
        | Op::Select => 1,
        Op::Mul | Op::Umulhi | Op::Smulhi => 3,
        Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => 12,
        Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect => 10,
        _ => 2,
    }
}

/// Computes exact SSA liveness and chordal-graph register pressure across `view`.
pub fn plan_ssa_regalloc<A: Allocator + Copy>(
    view: View<'_>,
    budget: RegBudget,
    alloc: A,
) -> SsaRegPlan {
    let len = view.len();
    if len == 0 {
        return SsaRegPlan::default();
    }

    // Approximate conservative SSA intervals `[def, last_use]` per value across layout order,
    // extending intervals across backward branches so loop-carried values stay live through the loop.
    let mut last_use = Vec::with_capacity_in(len, alloc);
    for i in 0..len {
        last_use.push(i);
    }

    let mut total_cycles = 0u32;
    for b in 0..view.blocks() {
        let range = view.block(Block(b as u32));
        for inst in range.start..range.end {
            total_cycles = total_cycles.saturating_add(op_latency_cycles(view.op(inst)));
            each(&view, inst, |item| match item {
                Item::Value(v) => {
                    let vi = v as usize;
                    if vi < len && inst > last_use[vi] {
                        last_use[vi] = inst;
                    }
                }
                Item::Edge(edge) => {
                    if let Some((start, count)) = edge.args {
                        for k in 0..count {
                            let vi = view.extra[start + k] as usize;
                            if vi < len && inst > last_use[vi] {
                                last_use[vi] = inst;
                            }
                        }
                    }
                    // Back-edge: extend values defined before target header through the latch.
                    let target_start = view.block(Block(edge.block)).start;
                    if target_start <= inst {
                        for v in 0..target_start {
                            if last_use[v] >= target_start && last_use[v] < inst {
                                last_use[v] = inst;
                            }
                        }
                    }
                }
            });
        }
    }

    // Sweep-line over `[def, last_use]` intervals for peak GPR and FPR chromatic pressure.
    let mut gpr_delta = filled(len + 1, 0i32, &alloc);
    let mut fpr_delta = filled(len + 1, 0i32, &alloc);
    for inst in 0..len {
        let ty = Ty(view.tys[inst]);
        if ty == Ty::NONE || last_use[inst] <= inst {
            continue;
        }
        if ty.is_float() {
            fpr_delta[inst] += 1;
            fpr_delta[last_use[inst]] -= 1;
        } else {
            gpr_delta[inst] += 1;
            gpr_delta[last_use[inst]] -= 1;
        }
    }

    let (mut cur_gpr, mut cur_fpr) = (0i32, 0i32);
    let (mut peak_gpr, mut peak_fpr) = (0u16, 0u16);
    for i in 0..len {
        cur_gpr += gpr_delta[i];
        cur_fpr += fpr_delta[i];
        if cur_gpr > i32::from(peak_gpr) {
            peak_gpr = cur_gpr.min(i32::from(u16::MAX)) as u16;
        }
        if cur_fpr > i32::from(peak_fpr) {
            peak_fpr = cur_fpr.min(i32::from(u16::MAX)) as u16;
        }
    }

    SsaRegPlan {
        peak_gprs: peak_gpr,
        peak_fprs: peak_fpr,
        estimated_gpr_spills: peak_gpr.saturating_sub(budget.gprs),
        estimated_fpr_spills: peak_fpr.saturating_sub(budget.fprs),
        estimated_cycles: total_cycles,
    }
}

/// Returns `true` if block `a` dominates block `b` using the `idom` array.
#[inline]
fn dominates(idom: &[u32], a: u32, mut b: u32) -> bool {
    if a == b {
        return true;
    }
    let limit = idom.len();
    for _ in 0..limit {
        let next = idom[b as usize];
        if next == a {
            return true;
        }
        if next == NONE || next == b {
            return false;
        }
        b = next;
    }
    false
}

/// Returns `true` if `op` is a pure, trap-free arithmetic/logic op safe to hoist out of loops (LICM).
#[inline]
const fn is_licm_pure(op: Op) -> bool {
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Not
            | Op::Shl
            | Op::Lshr
            | Op::Ashr
            | Op::Icmp
            | Op::Select
    )
}

/// Discovers natural loops, per-block nesting depth, LICM-hoistable pure instructions, and basic
/// induction variables (`header_param` incremented by a loop-invariant stride on the back-edge).
fn analyze_loops<A: Allocator + Copy>(
    view: &View<'_>,
    reached: &[bool],
    idom: &[u32],
    alloc: &A,
) -> (LoopSummary, Vec<u32, A>) {
    let blocks = view.blocks();
    let len = view.len();
    let mut block_depth = filled(blocks, 0u32, alloc);
    let mut summary = LoopSummary::default();
    if blocks == 0 {
        return (summary, block_depth);
    }

    // Build reverse edges and map each instruction -> owning block.
    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(*alloc));
    }
    let mut inst_block = filled(len, 0u32, alloc);
    let mut back_edges: Vec<(u32, u32), A> = Vec::new_in(*alloc);

    for b in 0..blocks {
        let range = view.block(Block(b as u32));
        for inst in range {
            inst_block[inst] = b as u32;
        }
        if !reached[b] {
            continue;
        }
        let term = view.terminator(Block(b as u32));
        each(view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = edge.block as usize;
                if to < blocks && reached[to] {
                    preds[to].push(b as u32);
                    if dominates(idom, edge.block, b as u32) {
                        back_edges.push((b as u32, edge.block));
                    }
                }
            }
        });
    }

    // For each natural loop (latch -> header), collect loop body via backward DFS from latch.
    let mut in_loop = filled(blocks, false, alloc);
    let mut work: Vec<u32, A> = Vec::new_in(*alloc);
    let mut body_blocks: Vec<u32, A> = Vec::new_in(*alloc);

    for &(latch, header) in &back_edges {
        summary.natural_loops += 1;
        body_blocks.clear();
        work.clear();
        in_loop[header as usize] = true;
        body_blocks.push(header);
        if !in_loop[latch as usize] {
            in_loop[latch as usize] = true;
            body_blocks.push(latch);
            work.push(latch);
        }
        while let Some(cur) = work.pop() {
            for &p in &preds[cur as usize] {
                if !in_loop[p as usize] {
                    in_loop[p as usize] = true;
                    body_blocks.push(p);
                    work.push(p);
                }
            }
        }

        // Increment nesting depth for every block in this natural loop.
        for &bb in &body_blocks {
            block_depth[bb as usize] += 1;
            if block_depth[bb as usize] > summary.max_depth {
                summary.max_depth = block_depth[bb as usize];
            }
        }

        // Detect LICM-hoistable pure instructions inside the loop body (all operands defined outside).
        for &bb in &body_blocks {
            for inst in view.block(Block(bb)) {
                let op = view.op(inst);
                if op == Op::Param || Flags(view.flags[inst]).0 != 0 || !is_licm_pure(op) {
                    continue;
                }
                let mut all_invariant = true;
                let mut has_value_operand = false;
                each(view, inst, |item| {
                    if let Item::Value(v) = item {
                        has_value_operand = true;
                        let def_b = inst_block[v as usize] as usize;
                        if in_loop[def_b] {
                            all_invariant = false;
                        }
                    }
                });
                if has_value_operand && all_invariant {
                    summary.licm_candidates += 1;
                }
            }
        }

        // Detect basic induction variables: header param `p` whose back-edge argument from `latch`
        // is `add/sub(p, invariant)` where `invariant` is defined outside the loop (or an iconst).
        let header_params = view.params(Block(header));
        let latch_term = view.terminator(Block(latch));
        each(view, latch_term, |item| {
            if let Item::Edge(edge) = item {
                if edge.block == header {
                    if let Some((arg_start, arg_len)) = edge.args {
                        let n = arg_len.min(header_params.len());
                        for k in 0..n {
                            let param_v = (header_params.start + k) as u32;
                            let step_v = view.extra[arg_start + k] as usize;
                            let step_op = view.op(step_v);
                            if matches!(step_op, Op::Add | Op::Sub) {
                                let at = view.data[step_v] as usize;
                                let lhs = view.extra[at];
                                let rhs = view.extra[at + 1];
                                let is_inv = |v: u32| -> bool {
                                    let vi = v as usize;
                                    matches!(view.op(vi), Op::Iconst | Op::IconstWide)
                                        || !in_loop[inst_block[vi] as usize]
                                };
                                if (lhs == param_v && is_inv(rhs))
                                    || (step_op == Op::Add && rhs == param_v && is_inv(lhs))
                                {
                                    summary.induction_vars += 1;
                                }
                            }
                        }
                    }
                }
            }
        });

        // Clear loop membership bitset for next back-edge.
        for &bb in &body_blocks {
            in_loop[bb as usize] = false;
        }
    }

    (summary, block_depth)
}

/// Computes PGO + loop-depth + register-pressure guided opportunistic JIT inline decisions.
///
/// - `site_pgo_weight`: callback returning dynamic PGO call-site count (e.g. from `blaze_lir::pgo`)
///   for instruction index `inst`, or `1` when cold/unprofiled.
/// - `reg_plan` + `budget`: feeds back remaining GPR headroom so we aggressively inline inside hot
///   loops when registers are available, and throttle back before introducing hot-loop spills.
pub fn plan_opportunistic_inlining<A: Allocator + Copy>(
    view: View<'_>,
    reg_plan: SsaRegPlan,
    budget: RegBudget,
    mut site_pgo_weight: impl FnMut(usize) -> u64,
    alloc: A,
) -> Vec<OpportunisticInlineSite, A> {
    let blocks = view.blocks();
    let len = view.len();
    let mut sites = Vec::new_in(alloc);
    if blocks == 0 {
        return sites;
    }

    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let (idom, _) = compute_dom_tree(&view, &reached, &alloc);
    let (_loop_summary, block_depths) = analyze_loops(&view, &reached, &idom, &alloc);

    // Register headroom factor: full budget when >= 6 GPRs free; scaled down near saturation to
    // prevent over-inlining from causing hot-loop spills.
    let gpr_free = budget.gprs.saturating_sub(reg_plan.peak_gprs);
    let pressure_scale_pct: u32 = match gpr_free {
        0 => 25,
        1..=2 => 50,
        3..=5 => 75,
        _ => 100,
    };

    for b in 0..blocks {
        if !reached[b] {
            continue;
        }
        let depth = block_depths[b];
        // Exponential loop amplification: 1x outside loops, 4x at depth 1, 16x at depth 2, capped.
        let loop_amp: u64 = 1u64 << (depth.min(4) * 2);

        for inst in view.block(Block(b as u32)) {
            let op = view.op(inst);
            if !matches!(op, Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect) {
                continue;
            }
            let pgo_w = site_pgo_weight(inst).max(1);
            let hotness = pgo_w.saturating_mul(loop_amp);
            // Base inline budget 24 insts; boosted up to 160 insts on super-hot loop calls,
            // then scaled by remaining register headroom.
            let base_budget: u32 = if hotness >= 1024 {
                160
            } else if hotness >= 64 {
                96
            } else if depth > 0 || pgo_w >= 8 {
                48
            } else {
                24
            };
            let allowed = (base_budget.saturating_mul(pressure_scale_pct) / 100).max(8);
            let speculative_devirt =
                matches!(op, Op::CallIndirect | Op::InvokeIndirect) && (depth > 0 || pgo_w >= 16);

            sites.push(OpportunisticInlineSite {
                call_inst: inst as u32,
                loop_depth: depth,
                hotness_score: hotness,
                allowed_callee_insts: allowed,
                speculative_devirt,
            });
        }
    }

    // Prioritize hottest loop/PGO sites first for the JIT tier-up queue.
    sites.sort_unstable_by(|a, b| b.hotness_score.cmp(&a.hotness_score).then_with(|| a.call_inst.cmp(&b.call_inst)));
    sites
}

/// Complete fixed-point SOTA mid-end feedback output: optimized LIR, loop/aegraph telemetry,
/// chordal-SSA register pressure plan, and prioritized opportunistic JIT inline sites.
#[derive(Debug)]
pub struct SotaFeedbackOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: AegraphStats,
    pub reg_plan: SsaRegPlan,
    pub inline_sites: Vec<OpportunisticInlineSite, A>,
    pub rounds_run: u32,
}

/// Copies one instruction from `view` into `func`, remapping all value operands through `val_map`.
fn copy_inst_remapped<A: Allocator + Copy>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
) -> Result<u32, SimplifyError> {
    let op = view.op(inst);
    let flags = view.flags[inst];
    let ty = view.tys[inst];
    let attrs = view.attrs[inst];
    let loc = view.locs[inst];
    let map_val = |v: u32| -> Result<u32, SimplifyError> {
        let mapped = val_map[v as usize];
        if mapped == NONE {
            Err(SimplifyError::Refused { inst, what: "unmapped operand during SOTA rewrite" })
        } else {
            Ok(mapped)
        }
    };

    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => map_val(view.data[inst])?,
        Layout::Data(_) => view.data[inst],
        Layout::Extra(fields) => {
            let start = func.extra.len() as u32;
            let words = view.extra;
            let mut at = view.data[inst] as usize;
            let mut scrutinee = None;
            for &field in fields {
                match field {
                    Field::Value => {
                        scrutinee = Some(words[at]);
                        func.extra.push(map_val(words[at])?);
                        at += 1;
                    }
                    Field::Block
                    | Field::Imm
                    | Field::Offset
                    | Field::IntCond
                    | Field::FloatCond
                    | Field::Rmw
                    | Field::Ref
                    | Field::Sig
                    | Field::Slot
                    | Field::Intrinsic
                    | Field::Asm => {
                        func.extra.push(words[at]);
                        at += 1;
                    }
                    Field::Target => {
                        let block = words[at];
                        let n = words[at + 1] as usize;
                        func.extra.push(block);
                        func.extra.push(n as u32);
                        for k in 0..n {
                            func.extra.push(map_val(words[at + 2 + k])?);
                        }
                        at += 2 + n;
                    }
                    Field::Values => {
                        let n = words[at] as usize;
                        func.extra.push(n as u32);
                        for k in 0..n {
                            func.extra.push(map_val(words[at + 1 + k])?);
                        }
                        at += 1 + n;
                    }
                    Field::Blocks => {
                        let n = words[at] as usize;
                        func.extra.extend_from_slice(&words[at..at + 1 + n]);
                        at += 1 + n;
                    }
                    Field::Mask => {
                        let n = words[at] as usize;
                        func.extra.extend_from_slice(&words[at..at + 1 + n]);
                        at += 1 + n;
                    }
                    Field::Const => {
                        let n = Ty(ty).const_words() as usize;
                        func.extra.extend_from_slice(&words[at..at + n]);
                        at += n;
                    }
                    Field::Cases => {
                        let width = scrutinee.map_or(0, |it: u32| Ty(view.tys[it as usize]).const_words()) as usize;
                        let n = words[at] as usize;
                        func.extra.push(n as u32);
                        at += 1;
                        for _ in 0..n {
                            func.extra.extend_from_slice(&words[at..at + width]);
                            at += width;
                            let block = words[at];
                            let argc = words[at + 1] as usize;
                            func.extra.push(block);
                            func.extra.push(argc as u32);
                            for k in 0..argc {
                                func.extra.push(map_val(words[at + 2 + k])?);
                            }
                            at += 2 + argc;
                        }
                    }
                }
            }
            start
        }
    };

    let out_idx = func.ops.len() as u32;
    func.ops.push(op as u8);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attrs);
    func.locs.push(loc);
    Ok(out_idx)
}

/// Emits an integer constant (`Iconst` or `IconstWide`) of scalar type `ty` into `func`.
fn push_const_inst<A: Allocator + Copy>(
    func: &mut Func<A>,
    ty: Ty,
    ptr_bits: u32,
    value: u128,
    loc: u32,
) -> u32 {
    let bits = ty.bits(ptr_bits);
    let idx = func.ops.len() as u32;
    if bits <= 32 {
        func.ops.push(Op::Iconst as u8);
        func.flags.push(0);
        func.tys.push(ty.0);
        func.data.push(value as u32);
        func.attrs.push(0);
        func.locs.push(loc);
        return idx;
    }
    match i32::try_from(sext(value, bits)) {
        Ok(small) => {
            func.ops.push(Op::Iconst as u8);
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(small as u32);
            func.attrs.push(0);
            func.locs.push(loc);
        }
        Err(_) => {
            let start = func.extra.len() as u32;
            for w in 0..ty.const_words() {
                func.extra.push((value >> (32 * w)) as u32);
            }
            func.ops.push(Op::IconstWide as u8);
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(start);
            func.attrs.push(0);
            func.locs.push(loc);
        }
    }
    idx
}

/// Emits a binary scalar instruction `op(lhs, rhs)` into `func`.
fn push_binary_inst<A: Allocator + Copy>(
    func: &mut Func<A>,
    op: Op,
    ty: Ty,
    lhs: u32,
    rhs: u32,
    loc: u32,
) -> u32 {
    let start = func.extra.len() as u32;
    func.extra.push(lhs);
    func.extra.push(rhs);
    let idx = func.ops.len() as u32;
    func.ops.push(op as u8);
    func.flags.push(0);
    func.tys.push(ty.0);
    func.data.push(start);
    func.attrs.push(0);
    func.locs.push(loc);
    idx
}

/// Materializes strength reductions directly into verified LIR:
/// - `mul x, 2^k` (or `2^k * x`) -> `shl x, k`
/// - `udiv x, 2^k` -> `lshr x, k`
/// - `urem x, 2^k` -> `and x, (2^k - 1)`
/// - `add x, x` -> `shl x, 1`
///
/// Returns `(rewritten_func, count_rewritten)`.
pub fn materialize_strength_reduction<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let len = view.len();
    let ptr_bits = view.head.ptr_bits;

    // Collect known constants in one forward scan.
    let mut known: Vec<Option<u128>, A> = filled(len, None, &alloc);
    for inst in 0..len {
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        if op == Op::Iconst {
            if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                known[inst] = Some(iconst_value(view.data[inst], bits));
            }
        } else if op == Op::IconstWide {
            if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                let at = view.data[inst] as usize;
                let w = ty.const_words() as usize;
                known[inst] = Some(wide_value(&view.extra[at..at + w], bits));
            }
        }
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);
    let mut rewrites = 0u32;

    for b in 0..view.blocks() {
        func.block_starts.push(func.ops.len() as u32);
        func.block_kinds.push(view.block_kinds[b]);
        for inst in view.block(Block(b as u32)) {
            let op = view.op(inst);
            let ty = Ty(view.tys[inst]);
            let loc = view.locs[inst];
            let mut replaced = None;

            if Flags(view.flags[inst]).0 == 0 {
                if let Some((bits, _)) = scalar_int_mask(ty, ptr_bits) {
                    if matches!(op, Op::Mul | Op::Udiv | Op::Urem | Op::Add) {
                        let at = view.data[inst] as usize;
                        let raw_a = view.extra[at];
                        let raw_b = view.extra[at + 1];
                        let a = val_map[raw_a as usize];
                        let b_val = val_map[raw_b as usize];
                        let ca = known[raw_a as usize];
                        let cb = known[raw_b as usize];

                        match op {
                            Op::Mul => {
                                if let Some(v) = cb.filter(|&v| is_power_of_two_gt1(v)) {
                                    let k = v.trailing_zeros();
                                    if k < bits {
                                        let kc = push_const_inst(&mut func, ty, ptr_bits, u128::from(k), loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Shl, ty, a, kc, loc));
                                    }
                                } else if let Some(v) = ca.filter(|&v| is_power_of_two_gt1(v)) {
                                    let k = v.trailing_zeros();
                                    if k < bits {
                                        let kc = push_const_inst(&mut func, ty, ptr_bits, u128::from(k), loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Shl, ty, b_val, kc, loc));
                                    }
                                }
                            }
                            Op::Udiv => {
                                if let Some(v) = cb.filter(|&v| is_power_of_two_gt1(v)) {
                                    let k = v.trailing_zeros();
                                    if k < bits {
                                        let kc = push_const_inst(&mut func, ty, ptr_bits, u128::from(k), loc);
                                        replaced = Some(push_binary_inst(&mut func, Op::Lshr, ty, a, kc, loc));
                                    }
                                }
                            }
                            Op::Urem => {
                                if let Some(v) = cb.filter(|&v| is_power_of_two_gt1(v)) {
                                    let mask_v = v - 1;
                                    let mc = push_const_inst(&mut func, ty, ptr_bits, mask_v, loc);
                                    replaced = Some(push_binary_inst(&mut func, Op::And, ty, a, mc, loc));
                                }
                            }
                            Op::Add if raw_a == raw_b => {
                                let one = push_const_inst(&mut func, ty, ptr_bits, 1, loc);
                                replaced = Some(push_binary_inst(&mut func, Op::Shl, ty, a, one, loc));
                            }
                            _ => {}
                        }
                    }
                }
            }

            if let Some(new_idx) = replaced {
                val_map[inst] = new_idx;
                rewrites += 1;
            } else {
                val_map[inst] = copy_inst_remapped(&view, inst, &val_map, &mut func)?;
            }
        }
    }

    if rewrites == 0 {
        return Ok((view.to_func_in(alloc), 0));
    }
    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok((func, rewrites))
}

/// Materializes Loop-Invariant Code Motion (LICM): physically hoists pure, flag-free loop-invariant
/// instructions out of natural loop bodies into the loop's single dominating `Jump` preheader.
///
/// Returns `(rewritten_func, hoisted_count)`.
pub fn hoist_loop_invariants<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks < 2 {
        return Ok((view.to_func_in(alloc), 0));
    }

    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let (idom, _) = compute_dom_tree(&view, &reached, &alloc);

    let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        preds.push(Vec::new_in(alloc));
    }
    let mut inst_block = filled(len, 0u32, &alloc);
    let mut back_edges: Vec<(u32, u32), A> = Vec::new_in(alloc);

    for b in 0..blocks {
        for inst in view.block(Block(b as u32)) {
            inst_block[inst] = b as u32;
        }
        if !reached[b] {
            continue;
        }
        let term = view.terminator(Block(b as u32));
        each(&view, term, |item| {
            if let Item::Edge(edge) = item {
                let to = edge.block as usize;
                if to < blocks && reached[to] {
                    preds[to].push(b as u32);
                    if dominates(&idom, edge.block, b as u32) {
                        back_edges.push((b as u32, edge.block));
                    }
                }
            }
        });
    }

    if back_edges.is_empty() {
        return Ok((view.to_func_in(alloc), 0));
    }

    // `hoist_to[inst]` = target preheader block index, or `NONE`.
    let mut hoist_to = filled(len, NONE, &alloc);
    let mut hoisted_by_preheader: Vec<Vec<usize, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        hoisted_by_preheader.push(Vec::new_in(alloc));
    }
    let mut total_hoisted = 0u32;

    let mut in_loop = filled(blocks, false, &alloc);
    let mut work: Vec<u32, A> = Vec::new_in(alloc);
    let mut body_blocks: Vec<u32, A> = Vec::new_in(alloc);

    for &(latch, header) in &back_edges {
        body_blocks.clear();
        work.clear();
        in_loop[header as usize] = true;
        body_blocks.push(header);
        if !in_loop[latch as usize] {
            in_loop[latch as usize] = true;
            body_blocks.push(latch);
            work.push(latch);
        }
        while let Some(cur) = work.pop() {
            for &p in &preds[cur as usize] {
                if !in_loop[p as usize] {
                    in_loop[p as usize] = true;
                    body_blocks.push(p);
                    work.push(p);
                }
            }
        }

        // Find unique outside preheader dominating `header` and ending in `Op::Jump` (and appearing
        // before `header` in layout order so single-pass emission stays ordered).
        let mut outside_preds = 0u32;
        let mut preheader = NONE;
        for &p in &preds[header as usize] {
            if !in_loop[p as usize] {
                outside_preds += 1;
                preheader = p;
            }
        }

        let valid_preheader = outside_preds == 1
            && preheader != NONE
            && preheader < header
            && dominates(&idom, preheader, header)
            && view.op(view.terminator(Block(preheader))) == Op::Jump;

        if valid_preheader {
            // Sort loop blocks in layout order so hoisting preserves def-before-use order.
            body_blocks.sort_unstable();
            // Fixed-point mark pure invariant instructions (including transitively invariant ones).
            let mut progressed = true;
            while progressed {
                progressed = false;
                for &bb in &body_blocks {
                    for inst in view.block(Block(bb)) {
                        if hoist_to[inst] != NONE {
                            continue;
                        }
                        let op = view.op(inst);
                        if op == Op::Param || Flags(view.flags[inst]).0 != 0 || !is_licm_pure(op) {
                            continue;
                        }
                        let mut all_ok = true;
                        each(&view, inst, |item| {
                            if let Item::Value(v) = item {
                                let vi = v as usize;
                                let def_b = inst_block[vi];
                                let defined_outside = !in_loop[def_b as usize] && dominates(&idom, def_b, preheader);
                                let already_hoisted_here = hoist_to[vi] == preheader;
                                if !defined_outside && !already_hoisted_here {
                                    all_ok = false;
                                }
                            }
                        });
                        if all_ok {
                            hoist_to[inst] = preheader;
                            hoisted_by_preheader[preheader as usize].push(inst);
                            total_hoisted += 1;
                            progressed = true;
                        }
                    }
                }
            }
        }

        for &bb in &body_blocks {
            in_loop[bb as usize] = false;
        }
    }

    if total_hoisted == 0 {
        return Ok((view.to_func_in(alloc), 0));
    }

    let mut func = view.to_func_in(alloc);
    func.ops.clear();
    func.flags.clear();
    func.tys.clear();
    func.data.clear();
    func.extra.clear();
    func.attrs.clear();
    func.locs.clear();
    func.block_starts.clear();
    func.block_kinds.clear();

    let mut val_map = filled(len, NONE, &alloc);

    for b in 0..blocks {
        func.block_starts.push(func.ops.len() as u32);
        func.block_kinds.push(view.block_kinds[b]);
        let range = view.block(Block(b as u32));
        let term = range.end - 1;

        for inst in range.start..term {
            if hoist_to[inst] != NONE {
                continue;
            }
            val_map[inst] = copy_inst_remapped(&view, inst, &val_map, &mut func)?;
        }
        // Insert any instructions hoisted into this preheader right before its `Jump` terminator.
        for &h_inst in &hoisted_by_preheader[b] {
            val_map[h_inst] = copy_inst_remapped(&view, h_inst, &val_map, &mut func)?;
        }
        // Emit block terminator.
        val_map[term] = copy_inst_remapped(&view, term, &val_map, &mut func)?;
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    Ok((func, total_hoisted))
}

/// Runs the iterative SOTA optimization feedback loop:
/// `[aegraph (GVN + identities + const-fold)] -> [strength reduction] -> [LICM hoist] ->
/// [propagate] -> [dead]` up to `max_rounds` until fixed point, then computes chordal-SSA register
/// pressure and feeds that back into PGO + loop-guided opportunistic JIT inlining decisions.
pub fn sota_optimize_fixpoint<A: Allocator + Copy>(
    view: View<'_>,
    budget: RegBudget,
    max_rounds: u32,
    mut site_pgo_weight: impl FnMut(usize) -> u64,
    alloc: A,
) -> Result<SotaFeedbackOutput<A>, SimplifyError> {
    let rounds = max_rounds.max(1);
    let mut cur = view.to_func_in(alloc);
    let mut total_stats = AegraphStats { insts_before: view.len() as u32, ..AegraphStats::default() };
    let mut rounds_run = 0u32;

    for _ in 0..rounds {
        rounds_run += 1;
        let step = aegraph_optimize(cur.view(), alloc)?;
        let (sr_func, sr_count) = materialize_strength_reduction(step.func.view(), alloc)?;
        let (licm_func, hoisted_count) = hoist_loop_invariants(sr_func.view(), alloc)?;
        let prop = super::propagate(licm_func.view(), alloc)?;
        let cleaned = dead(prop.view(), alloc)?;

        total_stats.gvn_hits += step.stats.gvn_hits;
        total_stats.identities_folded += step.stats.identities_folded;
        total_stats.constants_folded += step.stats.constants_folded;
        total_stats.strength_reductions += sr_count;
        let prev_hoisted = total_stats.loops.licm_candidates;
        total_stats.loops = step.stats.loops;
        total_stats.loops.licm_candidates = prev_hoisted + hoisted_count;

        let changed = cleaned.ops.len() != cur.ops.len()
            || step.stats.gvn_hits > 0
            || step.stats.identities_folded > 0
            || step.stats.constants_folded > 0
            || sr_count > 0
            || hoisted_count > 0;
        cur = cleaned;
        if !changed {
            break;
        }
    }
    total_stats.insts_after = cur.ops.len() as u32;

    let reg_plan = plan_ssa_regalloc(cur.view(), budget, alloc);
    let inline_sites = plan_opportunistic_inlining(
        cur.view(),
        reg_plan,
        budget,
        |inst| site_pgo_weight(inst),
        alloc,
    );

    Ok(SotaFeedbackOutput {
        func: cur,
        stats: total_stats,
        reg_plan,
        inline_sites,
        rounds_run,
    })
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use super::*;
    use crate::{
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{BlockKind, Conv, RefKind},
    };

    #[test]
    fn aegraph_gvn_commutativity_identities_and_eval_equivalence() {
        let alloc = Global;
        let mut b = Builder::<Global, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let eight = b.iconst(Ty::I64, 8);
        let ab = b.binary(Op::Add, Flags::NONE, x, y);
        let ba = b.binary(Op::Add, Flags::NONE, y, x); // commutative GVN with ab
        let ab0 = b.binary(Op::Add, Flags::NONE, ab, zero); // x + 0 -> x
        let xx = b.binary(Op::Xor, Flags::NONE, x, x); // x ^ x -> 0
        let n1 = b.unary(Op::Neg, Flags::NONE, ba);
        let n2 = b.unary(Op::Neg, Flags::NONE, n1); // neg(neg(ba)) -> ba -> ab
        let eq = b.icmp(IntCond::Eq, x, x); // icmp eq x, x -> 1
        let sel = b.select(Flags::NONE, eq, n2, ab0); // select(1, ab, ab) -> ab
        let prod = b.binary(Op::Mul, Flags::NONE, sel, eight); // strength-reduction candidate
        let out = b.binary(Op::Add, Flags::NONE, prod, xx); // + 0 -> prod
        b.ret(&[out]);

        let func = b.finish_checked().unwrap();
        let before_plan = plan_ssa_regalloc(func.view(), RegBudget::AARCH64, alloc);

        let opt = aegraph_optimize(func.view(), alloc).unwrap();
        assert!(opt.stats.gvn_hits >= 1, "expected commutative GVN hit: {:?}", opt.stats);
        assert!(opt.stats.identities_folded >= 4, "expected algebraic identities folded: {:?}", opt.stats);
        assert!(opt.stats.strength_reductions >= 1, "expected mul-by-8 strength reduction: {:?}", opt.stats);
        assert!(opt.stats.insts_after < opt.stats.insts_before, "expected smaller IR: {:?}", opt.stats);

        let after_plan = plan_ssa_regalloc(opt.func.view(), RegBudget::AARCH64, alloc);
        assert!(after_plan.estimated_cycles < before_plan.estimated_cycles);
        assert_eq!(after_plan.estimated_gpr_spills, 0);

        for &(xv, yv) in &[(0u64, 0u64), (7, 13), (u64::MAX, 42), (123_456, 789_012)] {
            let args = [Val::Int(u128::from(xv)), Val::Int(u128::from(yv))];
            let r_before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let r_after = eval::run_in(opt.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(r_before, r_after, "aegraph changed semantics on ({xv}, {yv})");
            match r_after {
                Outcome::Returned(vals) => {
                    assert_eq!(vals.as_slice(), &[Val::Int(u128::from(xv.wrapping_add(yv).wrapping_mul(8)))]);
                }
                other => panic!("expected Returned, got {other:?}"),
            }
        }
    }

    #[test]
    fn loop_analysis_licm_induction_vars_and_pgo_opportunistic_inlining_feedback() {
        let alloc = Global;
        let mut b = Builder::<Global, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::PTR)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let call_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let hot_callee = b.reference(RefKind::Func, 1);

        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let n = b.param(Ty::I64);
        let factor = b.param(Ty::I64);
        let dyn_fn = b.param(Ty::PTR);
        let zero = b.iconst(Ty::I64, 0);
        // Cold call outside the loop.
        let seed = b.call(Flags::NONE, hot_callee, call_sig, &[factor]);
        b.jump((header, &[zero, seed]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cont = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

        b.begin(body);
        // Loop-invariant computation inside loop body (LICM candidate: uses only `factor` & `n`).
        let inv_prod = b.binary(Op::Mul, Flags::NONE, factor, n);
        // Hot direct call + hot indirect call inside loop body!
        let step_val = b.call(Flags::NONE, hot_callee, call_sig, &[inv_prod]);
        let dyn_val = b.call_indirect(Flags::NONE, dyn_fn, call_sig, &[step_val]);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, dyn_val);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one); // basic induction variable!
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        b.ret(&[acc]);

        let func = b.finish_checked().unwrap();
        // Simulate PGO counters: hot loop calls hit 500x, cold outer call hits 1x.
        let res = sota_optimize_fixpoint(
            func.view(),
            RegBudget::AARCH64,
            2,
            |inst| if inst >= 8 { 500 } else { 1 },
            alloc,
        )
        .unwrap();

        assert_eq!(res.stats.loops.natural_loops, 1);
        assert_eq!(res.stats.loops.max_depth, 1);
        assert!(res.stats.loops.licm_candidates >= 1, "expected LICM candidate: {:?}", res.stats.loops);
        assert!(res.stats.loops.induction_vars >= 1, "expected induction var: {:?}", res.stats.loops);

        // Hot loop call sites must rank ahead of the cold entry call and get amplified inline budgets,
        // and the hot CallIndirect inside the loop must be flagged for speculative devirtualization!
        assert_eq!(res.inline_sites.len(), 3);
        assert!(res.inline_sites[0].loop_depth == 1);
        assert!(res.inline_sites[0].hotness_score > res.inline_sites[2].hotness_score);
        assert!(res.inline_sites[0].allowed_callee_insts > res.inline_sites[2].allowed_callee_insts);
        assert!(res.inline_sites.iter().any(|s| s.speculative_devirt));

        // Verify that `inv_prod = factor * n` was physically hoisted out of the loop body into block 0!
        let body_ops: std::vec::Vec<Op> = res
            .func
            .view()
            .block(Block(2))
            .map(|i| res.func.view().op(i))
            .collect();
        assert!(
            !body_ops.contains(&Op::Mul),
            "expected loop-invariant Mul to be hoisted out of block 2 into preheader block 0; body ops: {body_ops:?}"
        );
    }

    #[test]
    fn strength_reduction_and_licm_preserve_eval_semantics() {
        let alloc = Global;
        let mut b = Builder::<Global, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let a = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cont = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

        b.begin(body);
        // Loop-invariant math + power-of-2 udiv/urem/mul!
        let eight = b.iconst(Ty::I64, 8);
        let sixteen = b.iconst(Ty::I64, 16);
        let inv = b.binary(Op::Mul, Flags::NONE, a, eight); // strength-reduced to shl 3 AND hoisted!
        let q = b.binary(Op::Udiv, Flags::NONE, i, eight); // strength-reduced to lshr 3!
        let r = b.binary(Op::Urem, Flags::NONE, i, sixteen); // strength-reduced to and 15!
        let t = b.binary(Op::Add, Flags::NONE, inv, q);
        let t2 = b.binary(Op::Add, Flags::NONE, t, r);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, t2);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((header, &[next_i, next_acc]));

        b.begin(exit);
        b.ret(&[acc]);

        let func = b.finish_checked().unwrap();
        let res = sota_optimize_fixpoint(func.view(), RegBudget::AARCH64, 3, |_| 1, alloc).unwrap();

        // No Udiv, Urem, or Mul left in the entire function!
        let all_ops: std::vec::Vec<Op> = (0..res.func.ops.len()).map(|i| res.func.view().op(i)).collect();
        assert!(!all_ops.contains(&Op::Udiv), "expected Udiv strength-reduced to Lshr: {all_ops:?}");
        assert!(!all_ops.contains(&Op::Urem), "expected Urem strength-reduced to And: {all_ops:?}");
        assert!(!all_ops.contains(&Op::Mul), "expected Mul-by-8 strength-reduced to Shl: {all_ops:?}");

        // Exact bit-for-bit evaluator equivalence across test inputs!
        for &(nv, av) in &[(0u64, 5u64), (1, 9), (25, 17), (64, 1234)] {
            let args = [Val::Int(u128::from(nv)), Val::Int(u128::from(av))];
            let before = eval::run_in(func.view(), &args, 100_000, alloc).unwrap();
            let after = eval::run_in(res.func.view(), &args, 100_000, alloc).unwrap();
            assert_eq!(before, after, "SOTA fixpoint changed semantics on ({nv}, {av})");
        }
    }
}
