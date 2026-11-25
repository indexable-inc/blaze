//! Phi-discriminant jump threading, trivial forwarding-block collapse, and single-predecessor
//! block merging (`thread_jumps`).
//!
//! After inlining `core::ops::Range::next` or `core::slice::Iter::next` and promoting their stack
//! slots via [`super::scalar_replace`], loop bodies still pass an `Option` discriminant (an
//! integer tag `0`/`1`, or a niche pointer `null`/non-null) through a merge block `M(tag, ...)`:
//!
//! - `SomeArm` jumps to `M(1, payload)` (or `M(non_null_ptr)`),
//! - `NoneArm` jumps to `M(0, undef)` (or `M(null)`),
//! - `M` branches via `icmp` + `br` or `switch` on `tag` to `Exit` or `Body`.
//!
//! [`thread_jumps`] runs a bounded fixpoint (zero recursion) of three verified-LIR-preserving
//! rewrites:
//!
//! - **Pattern A (Phi-Discriminant Jump Threading)**: when a small pure block `M` terminates in
//!   `br` or `switch` whose outcome evaluates statically for a predecessor edge `P -> M(args)`
//!   (from integer constants or statically known `null`/non-null pointer facts), redirects that
//!   edge straight to the resolved successor `S`, reconstructing SSA via iterated dominance
//!   frontiers on the post-threading CFG for any externally used parameter of `M` (including
//!   uses in multi-predecessor post-merge blocks such as loop exits) and hoisting zero-operand
//!   constants of `M` to the entry block when needed.
//! - **Pattern B (Trivial Forwarding Block Collapse)**: when a non-entry block `B` has only pure
//!   dead instructions and ends in `jump S(s_args)`, threads plain incoming edges straight through
//!   `B` to `S`.
//! - **Pattern C (Single-Predecessor Block Merge & Unreachable Cleanup)**: coalesces
//!   `A -> jump B(args)` whenever `B` has `A` as its unique reachable predecessor and drops
//!   unreachable blocks via [`super::dce::coalesce_cfg`].
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, `#![no_std]` compatible, deterministic (Rule 25),
//! bounded loops (Rule 18), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Edit, Edge, Item, NONE, coalesce_cfg, dead, each, filled, reachable, rebuild},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    arith::{binary, compare, convert, iconst_value, mask, unary, wide_value},
    func::{MemAttrsRec, ParamFactsRec, SigId},
    op::{Field, Layout},
    verify,
};

/// Maximum threading + coalescing fixpoint rounds per call.
const MAX_ROUNDS: usize = 64;

/// Maximum non-param, non-terminator pure instructions allowed inside a Pattern A candidate block.
const MAX_THREAD_BLOCK_INSTS: usize = 16;

/// Maximum optimistic fixed-point rounds for pointer non-null analysis.
const MAX_NONNULL_ROUNDS: usize = 16;

/// Sentinel in `unique_pred` indicating a block has two or more distinct reachable predecessors.
const MULTI_PRED: u32 = u32::MAX - 1;

// ---------------------------------------------------------------------------
// Ratchet-clean indexing and conversion helpers
// ---------------------------------------------------------------------------

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn at<T: Copy>(s: &[T], i: usize, default: T) -> T {
    s.get(i).copied().unwrap_or(default)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, val: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = val;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL
        .iter()
        .position(|&o| o == op)
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(0)
}

/// Signed integer constant value of `inst` if it is a flagless scalar `iconst` / `iconst_wide`.
fn signed_const(view: &View<'_>, inst: usize) -> Option<i128> {
    let ty = Ty(at(view.tys, inst, 0));
    if !ty.is_scalar() || !ty.is_int() || at(view.flags, inst, 0) != 0 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let raw = match view.op(inst) {
        Op::Iconst => iconst_value(at(view.data, inst, 0), width),
        Op::IconstWide => {
            let start = to_idx(at(view.data, inst, 0));
            let end = start.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(start..end)?, width)
        }
        _ => return None,
    };
    Some(crate::arith::sext(raw, width))
}

/// Unsigned integer constant value of `inst` (bits above width zero) if scalar `iconst` / `iconst_wide`.
fn unsigned_const(view: &View<'_>, inst: usize) -> Option<u128> {
    let ty = Ty(at(view.tys, inst, 0));
    if !ty.is_scalar() || !ty.is_int() || at(view.flags, inst, 0) != 0 {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    match view.op(inst) {
        Op::Iconst => Some(iconst_value(at(view.data, inst, 0), width)),
        Op::IconstWide => {
            let start = to_idx(at(view.data, inst, 0));
            let end = start.checked_add(to_idx(ty.const_words()))?;
            Some(wide_value(view.extra.get(start..end)?, width))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// CFG, dominators, and cross-block def-use analysis
// ---------------------------------------------------------------------------

/// One reachable plain-target incoming edge into a block.
#[derive(Clone, Copy, Debug)]
struct InEdge {
    pred: u32,
    ordinal: u32,
    args_start: usize,
    args_len: usize,
}

/// Compact CFG, dominators, incoming plain edges, and instruction owners for one round.
struct Cfg<A: Allocator> {
    rpo: Vec<u32, A>,
    idom: Vec<u32, A>,
    plain_only: Vec<bool, A>,
    /// `NONE` = 0 reachable preds; `p` = sole reachable pred `p`; `MULTI_PRED` = 2+ distinct preds.
    unique_pred: Vec<u32, A>,
    in_start: Vec<u32, A>,
    in_edges: Vec<InEdge, A>,
    owner: Vec<u32, A>,
}

impl<A: Allocator + Copy> Cfg<A> {
    fn build(view: &View<'_>, alloc: A) -> Result<Self, SimplifyError> {
        let blocks = view.blocks();
        let len = view.len();

        let mut owner = filled(len, NONE, &alloc);
        let mut succ_start = Vec::with_capacity_in(blocks + 1, alloc);
        let mut succs: Vec<u32, A> = Vec::new_in(alloc);
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            let range = view.block(Block(b_u32));
            for inst in range {
                set_at(&mut owner, inst, b_u32);
            }
            succ_start.push(to_u32(succs.len()));
            each(view, view.terminator(Block(b_u32)), |item| {
                if let Item::Edge(edge) = item {
                    succs.push(edge.block);
                }
            });
        }
        succ_start.push(to_u32(succs.len()));

        // Iterative reverse postorder from block 0.
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        let mut seen = filled(blocks, false, &alloc);
        let mut stack: Vec<(u32, u32), A> = Vec::new_in(alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack.push((0, at(&succ_start, 0, 0)));
        }
        while let Some(top) = stack.last_mut() {
            let (b, next) = *top;
            let b_idx = to_idx(b);
            let end = at(&succ_start, b_idx + 1, 0);
            if next < end {
                top.1 = next + 1;
                let s = at(&succs, to_idx(next), 0);
                let s_idx = to_idx(s);
                if !at(&seen, s_idx, true) {
                    set_at(&mut seen, s_idx, true);
                    stack.push((s, at(&succ_start, s_idx, 0)));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
        let mut rpo = Vec::with_capacity_in(post.len(), alloc);
        rpo.extend_from_slice(&post);
        rpo.reverse();

        let mut rpo_order = filled(blocks, NONE, &alloc);
        for (idx, &b) in rpo.iter().enumerate() {
            set_at(&mut rpo_order, to_idx(b), to_u32(idx));
        }

        // Collect `plain_only` across ALL blocks (including unreachable ones, since `verify` checks
        // non-plain edges on every block), and reachable predecessor / incoming plain edge lists.
        let mut plain_only = filled(blocks, true, &alloc);
        set_at(&mut plain_only, 0, false);
        for b in 0..blocks {
            let term = view.terminator(Block(to_u32(b)));
            each(view, term, |item| {
                if let Item::Edge(Edge { block: target, args: None, .. }) = item {
                    set_at(&mut plain_only, to_idx(target), false);
                }
            });
        }

        let mut unique_pred = filled(blocks, NONE, &alloc);
        let mut pred_pairs: Vec<(u32, u32), A> = Vec::new_in(alloc);
        let mut in_counts = filled(blocks + 1, 0u32, &alloc);

        for &b in rpo.iter() {
            let term = view.terminator(Block(b));
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let to = to_idx(edge.block);
                    pred_pairs.push((edge.block, b));
                    let cur_u = at(&unique_pred, to, NONE);
                    if cur_u == NONE {
                        set_at(&mut unique_pred, to, b);
                    } else if cur_u != b {
                        set_at(&mut unique_pred, to, MULTI_PRED);
                    }
                    if edge.args.is_some() {
                        if let Some(c) = in_counts.get_mut(to + 1) {
                            *c += 1;
                        }
                    }
                }
            });
        }

        // Build reachable predecessor CSR for dominators.
        pred_pairs.sort_unstable();
        pred_pairs.dedup();
        let mut pred_start = filled(blocks + 1, 0u32, &alloc);
        for &(to, _) in pred_pairs.iter() {
            if let Some(c) = pred_start.get_mut(to_idx(to) + 1) {
                *c += 1;
            }
        }
        for b in 0..blocks {
            let prev = at(&pred_start, b, 0);
            if let Some(cur) = pred_start.get_mut(b + 1) {
                *cur += prev;
            }
        }
        let mut preds: Vec<u32, A> = Vec::with_capacity_in(pred_pairs.len(), alloc);
        for &(_, from) in pred_pairs.iter() {
            preds.push(from);
        }

        // Build plain incoming edge CSR.
        for b in 0..blocks {
            let prev = at(&in_counts, b, 0);
            if let Some(cur) = in_counts.get_mut(b + 1) {
                *cur += prev;
            }
        }
        let in_start = in_counts;
        let total_in = to_idx(at(&in_start, blocks, 0));
        let mut in_fill = filled(blocks, 0u32, &alloc);
        let dummy_edge = InEdge { pred: 0, ordinal: NONE, args_start: 0, args_len: 0 };
        let mut in_edges = filled(total_in, dummy_edge, &alloc);
        for &b in rpo.iter() {
            let term = view.terminator(Block(b));
            each(view, term, |item| {
                if let Item::Edge(Edge { block: target, args: Some((args_start, args_len)), ordinal }) = item {
                    let to = to_idx(target);
                    let pos = to_idx(at(&in_start, to, 0)) + to_idx(at(&in_fill, to, 0));
                    set_at(&mut in_edges, pos, InEdge { pred: b, ordinal, args_start, args_len });
                    if let Some(f) = in_fill.get_mut(to) {
                        *f += 1;
                    }
                }
            });
        }

        // Iterative Cooper-Harvey-Kennedy dominators.
        let mut idom = filled(blocks, NONE, &alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        let mut settled = false;
        let max_dom_iters = blocks.saturating_mul(2).saturating_add(2);
        for _ in 0..max_dom_iters {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let b_idx = to_idx(b);
                let p_s = to_idx(at(&pred_start, b_idx, 0));
                let p_e = to_idx(at(&pred_start, b_idx + 1, 0));
                let mut new_idom = NONE;
                if let Some(slice) = preds.get(p_s..p_e) {
                    for &p in slice {
                        if at(&idom, to_idx(p), NONE) == NONE {
                            continue;
                        }
                        new_idom = match new_idom {
                            NONE => p,
                            cur => intersect_dom(&idom, &rpo_order, p, cur),
                        };
                    }
                }
                if new_idom != at(&idom, b_idx, NONE) {
                    set_at(&mut idom, b_idx, new_idom);
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            return Err(SimplifyError::Refused {
                inst: 0,
                what: "a block graph whose dominators did not settle in jump_thread",
            });
        }

        Ok(Self { rpo, idom, plain_only, unique_pred, in_start, in_edges, owner })
    }

    #[inline]
    fn reachable(&self, b: u32) -> bool {
        at(&self.idom, to_idx(b), NONE) != NONE
    }

    fn incoming_of(&self, b: u32) -> &[InEdge] {
        let idx = to_idx(b);
        let s = to_idx(at(&self.in_start, idx, 0));
        let e = to_idx(at(&self.in_start, idx + 1, 0));
        self.in_edges.get(s..e).unwrap_or(&[])
    }

    /// Whether `anc` dominates `b` (including `anc == b`).
    fn is_dom(&self, mut b: u32, anc: u32) -> bool {
        if !self.reachable(b) || !self.reachable(anc) {
            return false;
        }
        let limit = self.idom.len().saturating_add(1);
        for _ in 0..limit {
            if b == anc {
                return true;
            }
            let parent = at(&self.idom, to_idx(b), NONE);
            if parent == NONE || parent == b {
                return false;
            }
            b = parent;
        }
        false
    }
}

fn intersect_dom(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    let limit = idom.len().saturating_mul(2).saturating_add(2);
    for _ in 0..limit {
        if a == b {
            return a;
        }
        if at(order, to_idx(a), 0) > at(order, to_idx(b), 0) {
            a = at(idom, to_idx(a), a);
        } else {
            b = at(idom, to_idx(b), b);
        }
    }
    a
}

/// CSR mapping each instruction `v` to every reachable block `b != owner[v]` that uses `v`.
struct CrossUses<A: Allocator> {
    start: Vec<u32, A>,
    blocks: Vec<u32, A>,
}

impl<A: Allocator + Copy> CrossUses<A> {
    fn build(view: &View<'_>, cfg: &Cfg<A>, alloc: A) -> Self {
        let len = view.len();
        let mut pairs: Vec<(u32, u32), A> = Vec::new_in(alloc);
        for &b in cfg.rpo.iter() {
            for inst in view.block(Block(b)) {
                each(view, inst, |item| match item {
                    Item::Value(v) => {
                        if at(&cfg.owner, to_idx(v), NONE) != b {
                            pairs.push((v, b));
                        }
                    }
                    Item::Edge(Edge { args: Some((args_start, args_len)), .. }) => {
                        if let Some(slice) = view.extra.get(args_start..args_start + args_len) {
                            for &v in slice {
                                if at(&cfg.owner, to_idx(v), NONE) != b {
                                    pairs.push((v, b));
                                }
                            }
                        }
                    }
                    Item::Edge(Edge { args: None, .. }) => {}
                });
            }
        }
        pairs.sort_unstable();
        pairs.dedup();
        let mut start = filled(len + 1, 0u32, &alloc);
        for &(v, _) in pairs.iter() {
            if let Some(c) = start.get_mut(to_idx(v) + 1) {
                *c += 1;
            }
        }
        for v in 0..len {
            let prev = at(&start, v, 0);
            if let Some(cur) = start.get_mut(v + 1) {
                *cur += prev;
            }
        }
        let mut blocks = Vec::with_capacity_in(pairs.len(), alloc);
        for &(_, b) in pairs.iter() {
            blocks.push(b);
        }
        Self { start, blocks }
    }

    fn of(&self, inst: usize) -> &[u32] {
        let s = to_idx(at(&self.start, inst, 0));
        let e = to_idx(at(&self.start, inst + 1, 0));
        self.blocks.get(s..e).unwrap_or(&[])
    }
}

// ---------------------------------------------------------------------------
// Static constant & pointer non-null abstract evaluation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EvalVal {
    Unknown,
    Int(u128),
    /// Provably non-zero scalar integer (e.g., `ptr_addr` of a non-null pointer).
    IntNonZero,
    PtrNull,
    PtrNonNull,
}

#[inline]
fn is_nonnull_facts(facts: &ParamFactsRec) -> bool {
    (facts.bits & ParamFactsRec::NONNULL) != 0
        || (facts.deref > 0 && (facts.bits & ParamFactsRec::DEREF_OR_NULL) == 0)
}

/// Abstractly evaluates `Op::Icmp(cond, la, lb)` for scalar integer or pointer operands.
fn eval_icmp(cond: IntCond, op_ty: Ty, ptr_bits: u32, la: EvalVal, lb: EvalVal) -> EvalVal {
    if op_ty == Ty::PTR {
        match (la, lb) {
            (EvalVal::PtrNull, EvalVal::PtrNull) => {
                EvalVal::Int(u128::from(compare(cond, 0, 0, ptr_bits)))
            }
            (EvalVal::PtrNonNull, EvalVal::PtrNull) => match cond {
                IntCond::Eq | IntCond::Ult | IntCond::Ule => EvalVal::Int(0),
                IntCond::Ne | IntCond::Ugt | IntCond::Uge => EvalVal::Int(1),
                IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge => EvalVal::Unknown,
            },
            (EvalVal::PtrNull, EvalVal::PtrNonNull) => match cond {
                IntCond::Eq | IntCond::Ugt | IntCond::Uge => EvalVal::Int(0),
                IntCond::Ne | IntCond::Ult | IntCond::Ule => EvalVal::Int(1),
                IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge => EvalVal::Unknown,
            },
            _ => EvalVal::Unknown,
        }
    } else if op_ty.is_scalar() && op_ty.is_int() {
        let width = op_ty.bits(ptr_bits);
        match (la, lb) {
            (EvalVal::Int(a), EvalVal::Int(b)) => EvalVal::Int(u128::from(compare(cond, a, b, width))),
            (EvalVal::IntNonZero, EvalVal::Int(0)) => {
                if width == 1 {
                    EvalVal::Int(u128::from(compare(cond, 1, 0, 1)))
                } else {
                    match cond {
                        IntCond::Eq | IntCond::Ult | IntCond::Ule => EvalVal::Int(0),
                        IntCond::Ne | IntCond::Ugt | IntCond::Uge => EvalVal::Int(1),
                        IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge => EvalVal::Unknown,
                    }
                }
            }
            (EvalVal::Int(0), EvalVal::IntNonZero) => {
                if width == 1 {
                    EvalVal::Int(u128::from(compare(cond, 0, 1, 1)))
                } else {
                    match cond {
                        IntCond::Eq | IntCond::Ugt | IntCond::Uge => EvalVal::Int(0),
                        IntCond::Ne | IntCond::Ult | IntCond::Ule => EvalVal::Int(1),
                        IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge => EvalVal::Unknown,
                    }
                }
            }
            (EvalVal::IntNonZero, EvalVal::Int(1)) => {
                if width == 1 {
                    EvalVal::Int(u128::from(compare(cond, 1, 1, 1)))
                } else {
                    match cond {
                        IntCond::Ult => EvalVal::Int(0),
                        IntCond::Uge => EvalVal::Int(1),
                        _ => EvalVal::Unknown,
                    }
                }
            }
            (EvalVal::Int(1), EvalVal::IntNonZero) => {
                if width == 1 {
                    EvalVal::Int(u128::from(compare(cond, 1, 1, 1)))
                } else {
                    match cond {
                        IntCond::Ugt => EvalVal::Int(0),
                        IntCond::Ule => EvalVal::Int(1),
                        _ => EvalVal::Unknown,
                    }
                }
            }
            _ => EvalVal::Unknown,
        }
    } else {
        EvalVal::Unknown
    }
}

/// Abstractly evaluates `Op::Zext`, `Op::Sext`, or `Op::Trunc` on a scalar integer.
fn eval_int_convert(op: Op, from_ty: Ty, to_ty: Ty, ptr_bits: u32, src: EvalVal) -> EvalVal {
    if !from_ty.is_scalar() || !from_ty.is_int() || !to_ty.is_scalar() || !to_ty.is_int() {
        return EvalVal::Unknown;
    }
    let from_bits = from_ty.bits(ptr_bits);
    let to_bits = to_ty.bits(ptr_bits);
    match src {
        EvalVal::Int(a) => EvalVal::Int(convert(op, a, from_bits, to_bits)),
        EvalVal::IntNonZero if matches!(op, Op::Zext | Op::Sext) => {
            if from_bits == 1 {
                EvalVal::Int(convert(op, 1, 1, to_bits))
            } else {
                EvalVal::IntNonZero
            }
        }
        _ => EvalVal::Unknown,
    }
}

#[inline]
fn invert_cond(cond: IntCond) -> IntCond {
    match cond {
        IntCond::Eq => IntCond::Ne,
        IntCond::Ne => IntCond::Eq,
        IntCond::Ult => IntCond::Uge,
        IntCond::Uge => IntCond::Ult,
        IntCond::Ule => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ule,
        IntCond::Slt => IntCond::Sge,
        IntCond::Sge => IntCond::Slt,
        IntCond::Sle => IntCond::Sgt,
        IntCond::Sgt => IntCond::Sle,
    }
}

/// If `cond_inst` being `when_true` proves that scalar integer `var` equals a single constant `c`,
/// returns `Some(c)`. Used to recognize two-valued niche selects such as
/// `select (icmp ule addr, 0), addr, 1` -> `{0, 1}`.
fn cond_implies_eq(view: &View<'_>, mut cond_inst: usize, mut when_true: bool, var: u32) -> Option<u128> {
    for _ in 0..4 {
        if cond_inst >= view.len() {
            return None;
        }
        let flagless = at(view.flags, cond_inst, 0) == 0;
        match view.op(cond_inst) {
            Op::Freeze => {
                cond_inst = to_idx(view.operands(cond_inst).value().0);
            }
            Op::Not if flagless && Ty(at(view.tys, cond_inst, 0)) == Ty::I1 => {
                cond_inst = to_idx(view.operands(cond_inst).value().0);
                when_true = !when_true;
            }
            Op::Icmp if flagless => {
                let mut o = view.operands(cond_inst);
                let raw_cond = o.int_cond();
                let lhs = o.value().0;
                let rhs = o.value().0;
                let op_ty = Ty(at(view.tys, to_idx(lhs), 0));
                if !op_ty.is_scalar() || !op_ty.is_int() {
                    return None;
                }
                let eff = if when_true { raw_cond } else { invert_cond(raw_cond) };
                return match eff {
                    IntCond::Eq => {
                        if lhs == var {
                            unsigned_const(view, to_idx(rhs))
                        } else if rhs == var {
                            unsigned_const(view, to_idx(lhs))
                        } else {
                            None
                        }
                    }
                    IntCond::Ule => {
                        if lhs == var && unsigned_const(view, to_idx(rhs)) == Some(0) {
                            Some(0)
                        } else {
                            None
                        }
                    }
                    IntCond::Uge => {
                        if rhs == var && unsigned_const(view, to_idx(lhs)) == Some(0) {
                            Some(0)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
            }
            _ => return None,
        }
    }
    None
}

/// If scalar integer instruction `inst` can only ever evaluate to one of two distinct constants
/// `(a, b)`, returns `Some((a, b))`. Never relies on `Op::Uninit` being consistent across uses.
fn two_const_range(view: &View<'_>, mut inst: usize) -> Option<(u128, u128)> {
    for _ in 0..4 {
        if inst >= view.len() {
            return None;
        }
        let op = view.op(inst);
        if op == Op::Uninit {
            return None;
        }
        let ty = Ty(at(view.tys, inst, 0));
        if !ty.is_scalar() || !ty.is_int() {
            return None;
        }
        if ty == Ty::I1 {
            return Some((0, 1));
        }
        let flagless = at(view.flags, inst, 0) == 0;
        match op {
            Op::Freeze => {
                inst = to_idx(view.operands(inst).value().0);
            }
            Op::Zext if flagless => {
                inst = to_idx(view.operands(inst).value().0);
            }
            Op::And if flagless => {
                let mut o = view.operands(inst);
                let a = to_idx(o.value().0);
                let b = to_idx(o.value().0);
                if unsigned_const(view, a) == Some(1) || unsigned_const(view, b) == Some(1) {
                    return Some((0, 1));
                }
                return None;
            }
            Op::Select => {
                let mut o = view.operands(inst);
                let cond = to_idx(o.value().0);
                let t_v = to_idx(o.value().0);
                let f_v = to_idx(o.value().0);
                let t_c = unsigned_const(view, t_v);
                let f_c = unsigned_const(view, f_v);
                if let (Some(a), Some(b)) = (t_c, f_c) {
                    return if a != b { Some((a, b)) } else { None };
                }
                if let Some(b) = f_c {
                    if t_v < view.len() && view.op(t_v) != Op::Uninit {
                        if let Some(a) = cond_implies_eq(view, cond, true, to_u32(t_v)) {
                            return if a != b { Some((a, b)) } else { None };
                        }
                    }
                }
                if let Some(a) = t_c {
                    if f_v < view.len() && view.op(f_v) != Op::Uninit {
                        if let Some(b) = cond_implies_eq(view, cond, false, to_u32(f_v)) {
                            return if a != b { Some((a, b)) } else { None };
                        }
                    }
                }
                return None;
            }
            _ => return None,
        }
    }
    None
}

/// Small stack-allocated table of edge-refined value facts valid along a CFG edge or dominated region.
const MAX_EDGE_FACTS: usize = 8;

#[derive(Clone, Copy)]
struct EdgeFacts {
    items: [(u32, EvalVal); MAX_EDGE_FACTS],
    len: usize,
}

impl EdgeFacts {
    #[inline]
    fn empty() -> Self {
        Self { items: [(NONE, EvalVal::Unknown); MAX_EDGE_FACTS], len: 0 }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn is_full(&self) -> bool {
        self.len >= MAX_EDGE_FACTS
    }

    fn push(&mut self, v: u32, val: EvalVal) {
        if val == EvalVal::Unknown {
            return;
        }
        for i in 0..self.len {
            let (cur_v, cur_val) = at(&self.items, i, (NONE, EvalVal::Unknown));
            if cur_v == v {
                if cur_val == EvalVal::IntNonZero && matches!(val, EvalVal::Int(_)) {
                    set_at(&mut self.items, i, (v, val));
                }
                return;
            }
        }
        if self.len < MAX_EDGE_FACTS {
            set_at(&mut self.items, self.len, (v, val));
            self.len += 1;
        }
    }

    #[inline]
    fn lookup(&self, v: u32, global_vals: &[EvalVal]) -> EvalVal {
        for i in 0..self.len {
            let (fv, fval) = at(&self.items, i, (NONE, EvalVal::Unknown));
            if fv == v {
                return fval;
            }
        }
        at(global_vals, to_idx(v), EvalVal::Unknown)
    }
}

/// Records any sound value facts implied by taking edge `ordinal` out of terminator `pred_term`.
fn derive_edge_facts(
    view: &View<'_>,
    pred_term: usize,
    ordinal: u32,
    global_vals: &[EvalVal],
    out: &mut EdgeFacts,
) {
    let is_stable = |v: u32| -> bool {
        let idx = to_idx(v);
        idx < view.len() && view.op(idx) != Op::Uninit
    };

    match view.op(pred_term) {
        Op::Br => {
            let mut cond_v = view.operands(pred_term).value().0;
            let mut taken_true = ordinal == 0;
            if is_stable(cond_v) {
                out.push(cond_v, EvalVal::Int(u128::from(taken_true)));
            }
            for _ in 0..4 {
                let idx = to_idx(cond_v);
                if idx >= view.len() {
                    return;
                }
                let flagless = at(view.flags, idx, 0) == 0;
                match view.op(idx) {
                    Op::Freeze => {
                        cond_v = view.operands(idx).value().0;
                    }
                    Op::Not if flagless && Ty(at(view.tys, idx, 0)) == Ty::I1 => {
                        cond_v = view.operands(idx).value().0;
                        taken_true = !taken_true;
                        if is_stable(cond_v) {
                            out.push(cond_v, EvalVal::Int(u128::from(taken_true)));
                        }
                    }
                    Op::Xor if flagless && Ty(at(view.tys, idx, 0)) == Ty::I1 => {
                        let mut o = view.operands(idx);
                        let a = o.value().0;
                        let b = o.value().0;
                        let av = at(global_vals, to_idx(a), EvalVal::Unknown);
                        let bv = at(global_vals, to_idx(b), EvalVal::Unknown);
                        if av == EvalVal::Int(1) {
                            cond_v = b;
                            taken_true = !taken_true;
                        } else if bv == EvalVal::Int(1) {
                            cond_v = a;
                            taken_true = !taken_true;
                        } else if av == EvalVal::Int(0) {
                            cond_v = b;
                        } else if bv == EvalVal::Int(0) {
                            cond_v = a;
                        } else {
                            return;
                        }
                        if is_stable(cond_v) {
                            out.push(cond_v, EvalVal::Int(u128::from(taken_true)));
                        }
                    }
                    _ => break,
                }
            }

            let cond_idx = to_idx(cond_v);
            if cond_idx >= view.len() || at(view.flags, cond_idx, 0) != 0 {
                return;
            }
            match view.op(cond_idx) {
                Op::Icmp => {
                    let mut o = view.operands(cond_idx);
                    let raw_cond = o.int_cond();
                    let lhs = o.value().0;
                    let rhs = o.value().0;
                    let eff = if taken_true { raw_cond } else { invert_cond(raw_cond) };
                    let op_ty = Ty(at(view.tys, to_idx(lhs), 0));
                    let lval = at(global_vals, to_idx(lhs), EvalVal::Unknown);
                    let rval = at(global_vals, to_idx(rhs), EvalVal::Unknown);

                    if op_ty == Ty::PTR {
                        match eff {
                            IntCond::Eq => {
                                if rval == EvalVal::PtrNull && is_stable(lhs) {
                                    out.push(lhs, EvalVal::PtrNull);
                                } else if lval == EvalVal::PtrNull && is_stable(rhs) {
                                    out.push(rhs, EvalVal::PtrNull);
                                }
                            }
                            IntCond::Ne => {
                                if rval == EvalVal::PtrNull && is_stable(lhs) {
                                    out.push(lhs, EvalVal::PtrNonNull);
                                } else if lval == EvalVal::PtrNull && is_stable(rhs) {
                                    out.push(rhs, EvalVal::PtrNonNull);
                                }
                            }
                            IntCond::Ule if rval == EvalVal::PtrNull && is_stable(lhs) => {
                                out.push(lhs, EvalVal::PtrNull);
                            }
                            IntCond::Uge if lval == EvalVal::PtrNull && is_stable(rhs) => {
                                out.push(rhs, EvalVal::PtrNull);
                            }
                            IntCond::Ugt if rval == EvalVal::PtrNull && is_stable(lhs) => {
                                out.push(lhs, EvalVal::PtrNonNull);
                            }
                            IntCond::Ult if lval == EvalVal::PtrNull && is_stable(rhs) => {
                                out.push(rhs, EvalVal::PtrNonNull);
                            }
                            _ => {}
                        }
                    } else if op_ty.is_scalar() && op_ty.is_int() {
                        let width = op_ty.bits(view.head.ptr_bits);
                        let push_int = |var: u32, refined: EvalVal, out: &mut EdgeFacts| {
                            out.push(var, refined);
                            let v_idx = to_idx(var);
                            if v_idx < view.len() && view.op(v_idx) == Op::PtrAddr {
                                let ptr_v = view.operands(v_idx).value().0;
                                if is_stable(ptr_v) {
                                    match refined {
                                        EvalVal::Int(0) => out.push(ptr_v, EvalVal::PtrNull),
                                        EvalVal::Int(_) | EvalVal::IntNonZero => {
                                            out.push(ptr_v, EvalVal::PtrNonNull);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        };

                        let refine_var_vs_const =
                            |var: u32, c: u128, var_on_left: bool, out: &mut EdgeFacts| {
                                if !is_stable(var) {
                                    return;
                                }
                                if let Some((a, b)) = two_const_range(view, to_idx(var)) {
                                    let a_ok = if var_on_left {
                                        compare(eff, a, c, width)
                                    } else {
                                        compare(eff, c, a, width)
                                    };
                                    let b_ok = if var_on_left {
                                        compare(eff, b, c, width)
                                    } else {
                                        compare(eff, c, b, width)
                                    };
                                    if a_ok && !b_ok {
                                        push_int(var, EvalVal::Int(a), out);
                                        return;
                                    } else if !a_ok && b_ok {
                                        push_int(var, EvalVal::Int(b), out);
                                        return;
                                    }
                                }
                                match (eff, var_on_left, c) {
                                    (IntCond::Eq, _, _) => push_int(var, EvalVal::Int(c), out),
                                    (IntCond::Ule, true, 0) | (IntCond::Uge, false, 0) => {
                                        push_int(var, EvalVal::Int(0), out);
                                    }
                                    (IntCond::Ne, _, 0)
                                    | (IntCond::Ugt, true, 0)
                                    | (IntCond::Ult, false, 0) => {
                                        let v = if width == 1 {
                                            EvalVal::Int(1)
                                        } else {
                                            EvalVal::IntNonZero
                                        };
                                        push_int(var, v, out);
                                    }
                                    _ => {}
                                }
                            };

                        if let EvalVal::Int(c) = rval {
                            refine_var_vs_const(lhs, c, true, out);
                        } else if let EvalVal::Int(c) = lval {
                            refine_var_vs_const(rhs, c, false, out);
                        }
                    }
                }
                Op::Trunc => {
                    let src = view.operands(cond_idx).value().0;
                    if is_stable(src) {
                        if let Some((a, b)) = two_const_range(view, to_idx(src)) {
                            let a_ok = ((a & 1) != 0) == taken_true;
                            let b_ok = ((b & 1) != 0) == taken_true;
                            if a_ok && !b_ok {
                                out.push(src, EvalVal::Int(a));
                            } else if !a_ok && b_ok {
                                out.push(src, EvalVal::Int(b));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Op::Switch => {
            let mut o = view.operands(pred_term);
            let scrut = o.value();
            if !is_stable(scrut.0) {
                return;
            }
            let width = Ty(at(view.tys, scrut.index(), 0)).bits(view.head.ptr_bits);
            let _default = o.target();
            if ordinal >= 1 {
                let want = to_idx(ordinal - 1);
                for (idx, (words, _)) in o.cases(scrut).enumerate() {
                    if idx == want {
                        out.push(scrut.0, EvalVal::Int(wide_value(words, width)));
                        return;
                    }
                }
            } else if let Some((a, b)) = two_const_range(view, scrut.index()) {
                let mut seen_a = false;
                let mut seen_b = false;
                for (words, _) in o.cases(scrut) {
                    let cv = wide_value(words, width);
                    if cv == a {
                        seen_a = true;
                    }
                    if cv == b {
                        seen_b = true;
                    }
                }
                if seen_a && !seen_b {
                    out.push(scrut.0, EvalVal::Int(b));
                } else if !seen_a && seen_b {
                    out.push(scrut.0, EvalVal::Int(a));
                }
            }
        }
        _ => {}
    }
}

/// Walks up to 4 dominators from `cur`, collecting branch facts from every unique-incoming-edge
/// block that dominates `cur`. Sound because every path from entry (and from each refined SSA
/// value's definition) to `cur` must traverse each such unique incoming edge.
fn collect_dom_edge_facts<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &Cfg<A>,
    mut cur: u32,
    global_vals: &[EvalVal],
    out: &mut EdgeFacts,
) {
    for _ in 0..4 {
        if cur == 0 || cur == NONE || out.is_full() {
            break;
        }
        if at(&cfg.plain_only, to_idx(cur), false) {
            let in_edges = cfg.incoming_of(cur);
            if let [sole] = in_edges {
                if sole.pred != cur && at(&cfg.unique_pred, to_idx(cur), NONE) == sole.pred {
                    let pred_term = view.terminator(Block(sole.pred));
                    derive_edge_facts(view, pred_term, sole.ordinal, global_vals, out);
                }
            }
        }
        let parent = at(&cfg.idom, to_idx(cur), NONE);
        if parent == NONE || parent == cur {
            break;
        }
        cur = parent;
    }
}

/// Abstractly evaluates a pure / value-producing instruction `inst` using `lookup`.
fn eval_pure_inst(view: &View<'_>, inst: usize, lookup: impl Fn(u32) -> EvalVal) -> EvalVal {
    let op = view.op(inst);
    let ty = Ty(at(view.tys, inst, 0));
    let flagless = at(view.flags, inst, 0) == 0;
    match op {
        Op::Iconst | Op::IconstWide => match unsigned_const(view, inst) {
            Some(v) => EvalVal::Int(v),
            None => EvalVal::Unknown,
        },
        Op::Null => EvalVal::PtrNull,
        Op::SlotAddr | Op::DataAddr | Op::FuncAddr | Op::TlsAddr => EvalVal::PtrNonNull,
        Op::Freeze => lookup(view.operands(inst).value().0),
        Op::PtrAddr | Op::Expose => match lookup(view.operands(inst).value().0) {
            EvalVal::PtrNull => EvalVal::Int(0),
            EvalVal::PtrNonNull => EvalVal::IntNonZero,
            _ => EvalVal::Unknown,
        },
        Op::FromExposed if ty == Ty::PTR => match lookup(view.operands(inst).value().0) {
            EvalVal::Int(0) => EvalVal::PtrNull,
            EvalVal::Int(_) | EvalVal::IntNonZero => EvalVal::PtrNonNull,
            _ => EvalVal::Unknown,
        },
        Op::PtrWithAddr if ty == Ty::PTR => {
            let mut o = view.operands(inst);
            let _base = o.value();
            match lookup(o.value().0) {
                EvalVal::Int(0) => EvalVal::PtrNull,
                EvalVal::Int(_) | EvalVal::IntNonZero => EvalVal::PtrNonNull,
                _ => EvalVal::Unknown,
            }
        }
        Op::PtrAdd if ty == Ty::PTR => {
            let mut o = view.operands(inst);
            let base = lookup(o.value().0);
            let by = lookup(o.value().0);
            let flags = view.flags(inst);
            if base == EvalVal::PtrNonNull
                && (flags.contains(Flags::INBOUNDS) || flags.contains(Flags::NUW) || by == EvalVal::Int(0))
            {
                EvalVal::PtrNonNull
            } else if base == EvalVal::PtrNull && by == EvalVal::Int(0) {
                EvalVal::PtrNull
            } else {
                EvalVal::Unknown
            }
        }
        Op::Icmp if flagless => {
            let mut o = view.operands(inst);
            let cond = o.int_cond();
            let a_v = o.value().0;
            let b_v = o.value().0;
            let op_ty = Ty(at(view.tys, to_idx(a_v), 0));
            eval_icmp(cond, op_ty, view.head.ptr_bits, lookup(a_v), lookup(b_v))
        }
        Op::Zext | Op::Sext | Op::Trunc if flagless => {
            let a_v = view.operands(inst).value().0;
            let from_ty = Ty(at(view.tys, to_idx(a_v), 0));
            eval_int_convert(op, from_ty, ty, view.head.ptr_bits, lookup(a_v))
        }
        Op::Bitcast if flagless && ty.is_scalar() && ty.is_int() => {
            let a_v = view.operands(inst).value().0;
            let from_ty = Ty(at(view.tys, to_idx(a_v), 0));
            if from_ty.is_scalar() && from_ty.is_int() {
                lookup(a_v)
            } else {
                EvalVal::Unknown
            }
        }
        Op::Neg | Op::Not if flagless && ty.is_scalar() && ty.is_int() => {
            let width = ty.bits(view.head.ptr_bits);
            let a_v = view.operands(inst).value().0;
            match lookup(a_v) {
                EvalVal::Int(a) => EvalVal::Int(unary(op, a, width)),
                EvalVal::IntNonZero if op == Op::Not && width == 1 => EvalVal::Int(0),
                EvalVal::IntNonZero if op == Op::Neg => {
                    if width == 1 {
                        EvalVal::Int(1)
                    } else {
                        EvalVal::IntNonZero
                    }
                }
                _ => EvalVal::Unknown,
            }
        }
        Op::Or if ty.is_scalar() && ty.is_int() => {
            let width = ty.bits(view.head.ptr_bits);
            let all_ones = mask(width);
            let mut o = view.operands(inst);
            let a_v = o.value().0;
            let b_v = o.value().0;
            match (lookup(a_v), lookup(b_v)) {
                (EvalVal::Int(a), EvalVal::Int(b)) => EvalVal::Int(a | b),
                (EvalVal::Int(v), _) | (_, EvalVal::Int(v)) if v == all_ones => EvalVal::Int(all_ones),
                (EvalVal::Int(0), x) | (x, EvalVal::Int(0)) => x,
                (EvalVal::IntNonZero, _) | (_, EvalVal::IntNonZero) => {
                    if width == 1 {
                        EvalVal::Int(1)
                    } else {
                        EvalVal::IntNonZero
                    }
                }
                (EvalVal::Int(v), _) | (_, EvalVal::Int(v)) if v != 0 => EvalVal::IntNonZero,
                _ => EvalVal::Unknown,
            }
        }
        Op::And if flagless && ty.is_scalar() && ty.is_int() => {
            let width = ty.bits(view.head.ptr_bits);
            let all_ones = mask(width);
            let mut o = view.operands(inst);
            let a_v = o.value().0;
            let b_v = o.value().0;
            match (lookup(a_v), lookup(b_v)) {
                (EvalVal::Int(a), EvalVal::Int(b)) => EvalVal::Int(a & b),
                (EvalVal::Int(0), _) | (_, EvalVal::Int(0)) => EvalVal::Int(0),
                (EvalVal::Int(v), x) | (x, EvalVal::Int(v)) if v == all_ones => x,
                _ => EvalVal::Unknown,
            }
        }
        Op::Xor if flagless && ty.is_scalar() && ty.is_int() => {
            let width = ty.bits(view.head.ptr_bits);
            let mut o = view.operands(inst);
            let a_v = o.value().0;
            let b_v = o.value().0;
            match (lookup(a_v), lookup(b_v)) {
                (EvalVal::Int(a), EvalVal::Int(b)) => EvalVal::Int(a ^ b),
                (EvalVal::Int(0), x) | (x, EvalVal::Int(0)) => x,
                (EvalVal::IntNonZero, EvalVal::Int(b)) | (EvalVal::Int(b), EvalVal::IntNonZero)
                    if width == 1 =>
                {
                    EvalVal::Int(1 ^ b)
                }
                _ => EvalVal::Unknown,
            }
        }
        Op::Add | Op::Sub | Op::Mul | Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr | Op::Umin
        | Op::Umax | Op::Smin | Op::Smax
            if flagless && ty.is_scalar() && ty.is_int() =>
        {
            let mut o = view.operands(inst);
            let a_v = o.value().0;
            let b_v = o.value().0;
            match (lookup(a_v), lookup(b_v)) {
                (EvalVal::Int(a), EvalVal::Int(b)) => match binary(op, a, b, ty.bits(view.head.ptr_bits)) {
                    Ok(res) => EvalVal::Int(res),
                    Err(_) => EvalVal::Unknown,
                },
                (EvalVal::Int(0), x) | (x, EvalVal::Int(0)) if op == Op::Add => x,
                (x, EvalVal::Int(0)) if op == Op::Sub => x,
                _ => EvalVal::Unknown,
            }
        }
        Op::Select => {
            let mut o = view.operands(inst);
            let c_v = o.value().0;
            let a_v = o.value().0;
            let b_v = o.value().0;
            match lookup(c_v) {
                EvalVal::Int(c) => lookup(if c != 0 { a_v } else { b_v }),
                EvalVal::IntNonZero => lookup(a_v),
                _ => {
                    let la = lookup(a_v);
                    let lb = lookup(b_v);
                    let is_nonzero = |x: EvalVal| matches!(x, EvalVal::IntNonZero) || matches!(x, EvalVal::Int(v) if v != 0);
                    if la == lb {
                        la
                    } else if ty.is_scalar() && ty.is_int() && is_nonzero(la) && is_nonzero(lb) {
                        if ty == Ty::I1 { EvalVal::Int(1) } else { EvalVal::IntNonZero }
                    } else {
                        EvalVal::Unknown
                    }
                }
            }
        }
        _ => EvalVal::Unknown,
    }
}

/// Evaluates `Op::Br` or `Op::Switch` terminator `term` under `lookup`, returning its resolved
/// target ordinal when statically decided.
fn eval_terminator_static(view: &View<'_>, term: usize, lookup: impl Fn(u32) -> EvalVal) -> Option<u32> {
    match view.op(term) {
        Op::Br => {
            let cond = view.operands(term).value().0;
            match lookup(cond) {
                EvalVal::Int(v) => Some(if v != 0 { 0u32 } else { 1u32 }),
                EvalVal::IntNonZero => Some(0u32),
                _ => None,
            }
        }
        Op::Switch => {
            let mut o = view.operands(term);
            let scrutinee = o.value();
            let width = Ty(at(view.tys, scrutinee.index(), 0)).bits(view.head.ptr_bits);
            let _default = o.target();
            match lookup(scrutinee.0) {
                EvalVal::Int(v) => {
                    let mut found = None;
                    for (idx, (words, _)) in o.cases(scrutinee).enumerate() {
                        if wide_value(words, width) == v {
                            found = Some(1 + to_u32(idx));
                            break;
                        }
                    }
                    Some(found.unwrap_or(0))
                }
                EvalVal::IntNonZero if width == 1 => {
                    let mut found = None;
                    for (idx, (words, _)) in o.cases(scrutinee).enumerate() {
                        if wide_value(words, width) == 1 {
                            found = Some(1 + to_u32(idx));
                            break;
                        }
                    }
                    Some(found.unwrap_or(0))
                }
                EvalVal::IntNonZero => {
                    let all_zero = o.cases(scrutinee).all(|(words, _)| wide_value(words, width) == 0);
                    if all_zero { Some(0) } else { None }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Returns whether byte offset `off` is a provably non-negative multiple of `step` (or any
/// non-negative offset when `step == 1`), so a pointer loop stepping by `+step` from `entry_base`
/// to `PtrAdd[INBOUNDS](entry_base, off)` cannot overshoot `end` and wrap through address 0.
fn offset_is_step_multiple(
    view: &View<'_>,
    mut off: usize,
    step: i128,
    step_u128: u128,
    is_ult: bool,
) -> bool {
    if step <= 0 || step_u128 == 0 {
        return false;
    }
    let ptr_bits = view.head.ptr_bits;
    for _ in 0..4 {
        if off >= view.len() {
            return false;
        }
        if let Some(c) = signed_const(view, off) {
            if c < 0 {
                return false;
            }
            return (is_ult && step == 1) || (c % step) == 0;
        }
        match view.op(off) {
            Op::Freeze | Op::Zext | Op::Sext if at(view.flags, off, 0) == 0 => {
                off = to_idx(view.operands(off).value().0);
            }
            Op::Shl => {
                if step == 1 {
                    return true;
                }
                let mut o = view.operands(off);
                let _v = o.value();
                let sh_idx = to_idx(o.value().0);
                if let Some(sh) = signed_const(view, sh_idx) {
                    if sh >= 0 && sh < i128::from(ptr_bits) {
                        if let Ok(sh_u32) = u32::try_from(sh) {
                            if let Some(stride) = 1u128.checked_shl(sh_u32) {
                                return (stride % step_u128) == 0;
                            }
                        }
                    }
                }
                return false;
            }
            Op::Mul => {
                if step == 1 {
                    return true;
                }
                let mut o = view.operands(off);
                let a = to_idx(o.value().0);
                let b = to_idx(o.value().0);
                let a_ok = signed_const(view, a).is_some_and(|m| m > 0 && (m % step) == 0);
                let b_ok = signed_const(view, b).is_some_and(|m| m > 0 && (m % step) == 0);
                return a_ok || b_ok;
            }
            _ => return step == 1,
        }
    }
    step == 1
}

/// Checks whether `inst = PtrAdd(base, by)` (even without `INBOUNDS`/`NUW`) is the induction step
/// of a loop header `hdr(p, ...)` whose entry arg `entry_base` is `PtrNonNull`, whose header tests
/// `p != end` or `p < end` with `end = PtrAdd[INBOUNDS | NUW](entry_base, byte_len)` where
/// `byte_len` is a non-negative multiple of `step > 0`, and `inst` is dominated by the loop body
/// arm. Under these conditions `p` stays strictly in `[entry_base, end)` and `p + step <= end`
/// inside the inbounds allocation, so `inst` cannot wrap through address 0.
fn is_loop_bounded_nonnull_ptr_add<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &Cfg<A>,
    vals: &[EvalVal],
    inst: usize,
    inst_block: u32,
    base: usize,
    by: usize,
) -> bool {
    let Some(step) = signed_const(view, by) else {
        return false;
    };
    if step <= 0 {
        return false;
    }
    let Ok(step_u128) = u128::try_from(step) else {
        return false;
    };
    if base >= view.len() || view.op(base) != Op::Param {
        return false;
    }
    let hdr = at(&cfg.owner, base, NONE);
    if hdr == NONE || hdr == 0 || !at(&cfg.plain_only, to_idx(hdr), false) {
        return false;
    }
    let hdr_params = view.params(Block(hdr));
    if base < hdr_params.start || base >= hdr_params.end {
        return false;
    }
    let param_k = base - hdr_params.start;
    let base_u32 = to_u32(base);

    let hdr_term = view.terminator(Block(hdr));
    if view.op(hdr_term) != Op::Br {
        return false;
    }
    let mut to = view.operands(hdr_term);
    let cond_v = to.value().0;
    let then_blk = to.target().block.0;
    let else_blk = to.target().block.0;
    let cond_idx = to_idx(cond_v);
    if cond_idx >= view.len() || view.op(cond_idx) != Op::Icmp || at(view.flags, cond_idx, 0) != 0 {
        return false;
    }
    let mut co = view.operands(cond_idx);
    let icond = co.int_cond();
    let raw_lhs = co.value().0;
    let raw_rhs = co.value().0;
    let cmp_ty = Ty(at(view.tys, to_idx(raw_lhs), 0));
    let ptr_int_cmp = cmp_ty.is_scalar()
        && cmp_ty.is_int()
        && cmp_ty.bits(view.head.ptr_bits) == view.head.ptr_bits;
    if cmp_ty != Ty::PTR && !ptr_int_cmp {
        return false;
    }
    let strip_ptr_addr = |v: u32| -> u32 {
        let idx = to_idx(v);
        if cmp_ty != Ty::PTR && idx < view.len() && view.op(idx) == Op::PtrAddr {
            view.operands(idx).value().0
        } else {
            v
        }
    };
    let lhs = strip_ptr_addr(raw_lhs);
    let rhs = strip_ptr_addr(raw_rhs);
    let (end_v, p_on_left) = if lhs == base_u32 {
        (rhs, true)
    } else if rhs == base_u32 {
        (lhs, false)
    } else {
        return false;
    };

    let (body_blk, is_ult) = match (icond, p_on_left) {
        (IntCond::Ne, _) => (then_blk, false),
        (IntCond::Eq, _) => (else_blk, false),
        (IntCond::Ult, true) | (IntCond::Ugt, false) => (then_blk, true),
        (IntCond::Uge, true) | (IntCond::Ule, false) => (else_blk, true),
        _ => return false,
    };
    if body_blk == hdr || at(&cfg.unique_pred, to_idx(body_blk), NONE) != hdr {
        return false;
    }
    if !cfg.is_dom(inst_block, body_blk) {
        return false;
    }

    let edges = cfg.incoming_of(hdr);
    if edges.is_empty() {
        return false;
    }
    let mut entry_base: Option<u32> = None;
    let mut has_self_inst_edge = false;
    let inst_u32 = to_u32(inst);
    for edge in edges {
        if param_k >= edge.args_len {
            return false;
        }
        let arg = at(view.extra, edge.args_start + param_k, NONE);
        if cfg.is_dom(edge.pred, hdr) {
            if arg == base_u32 {
                continue;
            }
            let arg_idx = to_idx(arg);
            if arg_idx >= view.len() || view.op(arg_idx) != Op::PtrAdd {
                return false;
            }
            if !cfg.is_dom(at(&cfg.owner, arg_idx, NONE), body_blk) {
                return false;
            }
            let mut ao = view.operands(arg_idx);
            let a_base = ao.value().0;
            let a_by = to_idx(ao.value().0);
            if a_base != base_u32 || signed_const(view, a_by) != Some(step) {
                return false;
            }
            if arg == inst_u32 {
                has_self_inst_edge = true;
            }
        } else {
            if at(vals, to_idx(arg), EvalVal::Unknown) != EvalVal::PtrNonNull {
                return false;
            }
            match entry_base {
                None => entry_base = Some(arg),
                Some(prev) if prev == arg => {}
                Some(_) => return false,
            }
        }
    }
    let Some(entry_base) = entry_base else {
        return false;
    };
    if !has_self_inst_edge {
        return false;
    }

    let end_idx = to_idx(end_v);
    if end_idx >= view.len() || view.op(end_idx) != Op::PtrAdd {
        return false;
    }
    let end_flags = view.flags(end_idx);
    if !end_flags.contains(Flags::INBOUNDS) && !end_flags.contains(Flags::NUW) {
        return false;
    }
    let mut eo = view.operands(end_idx);
    let end_base = eo.value().0;
    let end_off = to_idx(eo.value().0);
    if end_base != entry_base {
        return false;
    }
    offset_is_step_multiple(view, end_off, step, step_u128, is_ult)
}

/// Computes sound global static facts (integer constants, `null`, and provably non-null pointers,
/// including loop-inductive `ptr_add` pointers from `slice::Iter::next`, plus `icmp`/`select`/
/// bitwise folding over those facts).
fn compute_global_vals<A: Allocator + Copy>(view: &View<'_>, cfg: &Cfg<A>, alloc: A) -> Vec<EvalVal, A> {
    let len = view.len();
    let mut vals = filled(len, EvalVal::Unknown, &alloc);

    // Entry parameters carrying `nonnull` or non-nullable `dereferenceable` facts.
    if view.blocks() > 0 {
        let entry_params = view.params(Block(0));
        let sig = view.sig(SigId(view.head.sig));
        for (k, inst) in entry_params.enumerate() {
            if Ty(at(view.tys, inst, 0)) != Ty::PTR {
                continue;
            }
            if let Some(param_rec) = sig.params.get(k) {
                if let Some(facts) = view.facts(param_rec) {
                    if is_nonnull_facts(facts) {
                        set_at(&mut vals, inst, EvalVal::PtrNonNull);
                    }
                }
            }
        }
    }

    // Optimistically seed non-entry `plain_only` `ptr` block parameters as `PtrNonNull`.
    for &b in cfg.rpo.iter().skip(1) {
        if !at(&cfg.plain_only, to_idx(b), false) {
            continue;
        }
        if cfg.incoming_of(b).is_empty() {
            continue;
        }
        for inst in view.params(Block(b)) {
            if Ty(at(view.tys, inst, 0)) == Ty::PTR {
                set_at(&mut vals, inst, EvalVal::PtrNonNull);
            }
        }
    }

    let mut converged = false;
    for _ in 0..MAX_NONNULL_ROUNDS {
        let mut changed = false;

        // Phase A: evaluate body instructions in RPO using current param facts BEFORE demoting
        // params so loop back-edge values (`p_next = ptr_add p, 8`) are computed before `hdr(p)`
        // checks its incoming back-edge!
        for &b in cfg.rpo.iter() {
            let params = view.params(Block(b));
            let range = view.block(Block(b));
            for inst in params.end..range.end {
                let op = view.op(inst);
                let ty = Ty(at(view.tys, inst, 0));
                let next_val = match op {
                    Op::Load | Op::AtomicLoad if ty == Ty::PTR => match view.mem_attrs(inst) {
                        Some(m) if (m.bits & MemAttrsRec::NONNULL) != 0 => EvalVal::PtrNonNull,
                        _ => EvalVal::Unknown,
                    },
                    Op::Call | Op::Invoke if ty == Ty::PTR => {
                        let mut o = view.operands(inst);
                        let _callee = o.reference();
                        let sig = view.sig(o.sig());
                        if sig.rets.len() == 1 {
                            match sig.rets.first().and_then(|r| view.facts(r)) {
                                Some(facts) if is_nonnull_facts(facts) => EvalVal::PtrNonNull,
                                _ => EvalVal::Unknown,
                            }
                        } else {
                            EvalVal::Unknown
                        }
                    }
                    Op::CallIndirect | Op::InvokeIndirect if ty == Ty::PTR => {
                        let mut o = view.operands(inst);
                        let _callee = o.value();
                        let sig = view.sig(o.sig());
                        if sig.rets.len() == 1 {
                            match sig.rets.first().and_then(|r| view.facts(r)) {
                                Some(facts) if is_nonnull_facts(facts) => EvalVal::PtrNonNull,
                                _ => EvalVal::Unknown,
                            }
                        } else {
                            EvalVal::Unknown
                        }
                    }
                    Op::Project if ty == Ty::PTR => {
                        let mut o = view.operands(inst);
                        let src = to_idx(o.value().0);
                        let slot = to_idx(o.imm());
                        if src < len {
                            let sig_id = match view.op(src) {
                                Op::Call | Op::Invoke => {
                                    let mut so = view.operands(src);
                                    let _callee = so.reference();
                                    Some(so.sig())
                                }
                                Op::CallIndirect | Op::InvokeIndirect => {
                                    let mut so = view.operands(src);
                                    let _callee = so.value();
                                    Some(so.sig())
                                }
                                _ => None,
                            };
                            match sig_id
                                .and_then(|sid| view.sig(sid).rets.get(slot))
                                .and_then(|r| view.facts(r))
                            {
                                Some(facts) if is_nonnull_facts(facts) => EvalVal::PtrNonNull,
                                _ => EvalVal::Unknown,
                            }
                        } else {
                            EvalVal::Unknown
                        }
                    }
                    Op::PtrAdd if ty == Ty::PTR => {
                        let mut o = view.operands(inst);
                        let base = to_idx(o.value().0);
                        let by = to_idx(o.value().0);
                        let flags = view.flags(inst);
                        let base_val = at(&vals, base, EvalVal::Unknown);
                        let zero_off =
                            signed_const(view, by) == Some(0) || at(&vals, by, EvalVal::Unknown) == EvalVal::Int(0);
                        if base_val == EvalVal::PtrNonNull
                            && (flags.contains(Flags::INBOUNDS)
                                || flags.contains(Flags::NUW)
                                || zero_off
                                || is_loop_bounded_nonnull_ptr_add(view, cfg, &vals, inst, b, base, by))
                        {
                            EvalVal::PtrNonNull
                        } else if base < len && view.op(base) == Op::SlotAddr {
                            let slot_id = to_idx(at(view.data, base, 0));
                            let slot_size = view.slots.get(slot_id).map_or(0, |s| i128::from(s.size));
                            match signed_const(view, by) {
                                Some(off) if off >= 0 && off <= slot_size => EvalVal::PtrNonNull,
                                _ => EvalVal::Unknown,
                            }
                        } else if base_val == EvalVal::PtrNull && zero_off {
                            EvalVal::PtrNull
                        } else {
                            EvalVal::Unknown
                        }
                    }
                    _ => eval_pure_inst(view, inst, |v| at(&vals, to_idx(v), EvalVal::Unknown)),
                };
                if next_val != at(&vals, inst, EvalVal::Unknown) {
                    set_at(&mut vals, inst, next_val);
                    changed = true;
                }
            }
        }

        // Phase B: verify optimistically seeded non-entry `ptr` block parameters against all
        // reachable incoming edges, monotonically demoting any param with a non-proven argument.
        for &b in cfg.rpo.iter().skip(1) {
            if !at(&cfg.plain_only, to_idx(b), false) {
                continue;
            }
            let params = view.params(Block(b));
            let edges = cfg.incoming_of(b);
            for (k, inst) in (params.start..params.end).enumerate() {
                if at(&vals, inst, EvalVal::Unknown) != EvalVal::PtrNonNull {
                    continue;
                }
                let inst_u32 = to_u32(inst);
                let mut all_nonnull = !edges.is_empty();
                for edge in edges {
                    if k >= edge.args_len {
                        all_nonnull = false;
                        break;
                    }
                    let arg = at(view.extra, edge.args_start + k, NONE);
                    if arg == inst_u32 {
                        continue;
                    }
                    if at(&vals, to_idx(arg), EvalVal::Unknown) != EvalVal::PtrNonNull {
                        all_nonnull = false;
                        break;
                    }
                }
                if !all_nonnull {
                    set_at(&mut vals, inst, EvalVal::Unknown);
                    changed = true;
                }
            }
        }

        if !changed {
            converged = true;
            break;
        }
    }

    // If the optimistic fixed point did not converge within the bound, drop optimistic param facts
    // and any values that could depend on them, keeping only locally sound base facts.
    if !converged {
        for &b in cfg.rpo.iter() {
            let params = view.params(Block(b));
            if b != 0 {
                for inst in params.start..params.end {
                    set_at(&mut vals, inst, EvalVal::Unknown);
                }
            }
            let range = view.block(Block(b));
            for inst in params.end..range.end {
                let keep = matches!(
                    view.op(inst),
                    Op::Iconst | Op::IconstWide | Op::Null | Op::SlotAddr | Op::DataAddr | Op::FuncAddr | Op::TlsAddr
                );
                if !keep {
                    set_at(&mut vals, inst, EvalVal::Unknown);
                }
            }
        }
    }

    vals
}

/// Whether `op` is strictly pure and non-trapping, safe to bypass when threading around a block `M`.
#[inline]
fn is_pure_thread_op(op: Op) -> bool {
    matches!(
        op,
        Op::Iconst
            | Op::IconstWide
            | Op::Fconst
            | Op::FconstWide
            | Op::Null
            | Op::Uninit
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
            | Op::Rotl
            | Op::Rotr
            | Op::Umin
            | Op::Umax
            | Op::Smin
            | Op::Smax
            | Op::Icmp
            | Op::Select
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Bitcast
            | Op::Freeze
            | Op::PtrAddr
            | Op::FromExposed
            | Op::PtrWithAddr
            | Op::PtrAdd
            | Op::SlotAddr
            | Op::DataAddr
            | Op::FuncAddr
            | Op::TlsAddr
    )
}

/// A resolved target slice in `view.extra` for a terminator of block `M`.
#[derive(Clone, Copy, Debug)]
struct TargetSlice {
    block: u32,
    args_start: usize,
    args_len: usize,
}

/// Finds target `want_ordinal` inside terminator `term_inst` of `view`.
fn target_by_ordinal(view: &View<'_>, term_inst: usize, want_ordinal: u32) -> Option<TargetSlice> {
    let mut found = None;
    each(view, term_inst, |item| {
        if let Item::Edge(Edge { block, args: Some((args_start, args_len)), ordinal }) = item {
            if ordinal == want_ordinal {
                found = Some(TargetSlice { block, args_start, args_len });
            }
        }
    });
    found
}

/// Evaluates `b`'s `Op::Br` or `Op::Switch` terminator under `global_vals` and any dominating
/// single-edge branch facts, returning the resolved target ordinal if statically decided.
fn eval_block_terminator<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &Cfg<A>,
    b: u32,
    global_vals: &[EvalVal],
    local_buf: &mut Vec<EvalVal, A>,
) -> Option<u32> {
    let range = view.block(Block(b));
    let params = view.params(Block(b));
    let term = range.end.checked_sub(1)?;
    if !matches!(view.op(term), Op::Br | Op::Switch) {
        return None;
    }

    // Fast path: terminator already decided directly by `global_vals`.
    if let Some(chosen) = eval_terminator_static(view, term, |v| at(global_vals, to_idx(v), EvalVal::Unknown)) {
        return Some(chosen);
    }

    // Collect dominating single-edge branch facts along `b`'s unique-predecessor dominator chain.
    let mut facts = EdgeFacts::empty();
    collect_dom_edge_facts(view, cfg, b, global_vals, &mut facts);
    if facts.is_empty() {
        return None;
    }

    let b_len = range.end.saturating_sub(range.start);
    local_buf.clear();
    local_buf.resize(b_len, EvalVal::Unknown);

    let param_count = params.end.saturating_sub(params.start);
    if b != 0 && at(&cfg.plain_only, to_idx(b), false) {
        let in_edges = cfg.incoming_of(b);
        if let [sole] = in_edges {
            if sole.pred != b && sole.args_len == param_count {
                for k in 0..param_count {
                    let arg = at(view.extra, sole.args_start + k, NONE);
                    set_at(local_buf, k, facts.lookup(arg, global_vals));
                }
            }
        }
    }

    let lookup = |v: u32, local: &[EvalVal]| -> EvalVal {
        let idx = to_idx(v);
        if idx >= range.start && idx < range.end {
            let lv = at(local, idx - range.start, EvalVal::Unknown);
            if lv != EvalVal::Unknown {
                return lv;
            }
        }
        facts.lookup(v, global_vals)
    };

    for inst in params.end..term {
        let val = eval_pure_inst(view, inst, |v| lookup(v, local_buf));
        let merged = if val != EvalVal::Unknown {
            val
        } else {
            at(global_vals, inst, EvalVal::Unknown)
        };
        set_at(local_buf, inst - range.start, merged);
    }

    eval_terminator_static(view, term, |v| lookup(v, local_buf))
}

/// Rewrites every terminator marked in `jump` into an unconditional `Op::Jump` and drops any
/// blocks made unreachable, returning verified LIR.
fn fold_terminators<A: Allocator + Copy>(
    view: View<'_>,
    jump: &[u32],
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    let reached = reachable(&view, jump, &alloc);
    let mut stands = Vec::with_capacity_in(len, alloc);
    for v in 0..len {
        stands.push(to_u32(v));
    }
    let mut inst_kept = filled(len, false, &alloc);
    for b in 0..blocks {
        if at(&reached, b, false) {
            for inst in view.block(Block(to_u32(b))) {
                set_at(&mut inst_kept, inst, true);
            }
        }
    }
    let mut jump_vec = Vec::with_capacity_in(len, alloc);
    jump_vec.extend_from_slice(jump);
    let edit = Edit {
        block_kept: reached,
        inst_kept,
        stands,
        constant: filled(len, None, &alloc),
        jump: jump_vec,
        devirt: filled(len, NONE, &alloc),
        merge_next: filled(blocks, NONE, &alloc),
        fma_fuse: filled(len, None, &alloc),
    };
    rebuild(view, &edit, alloc)
}

/// Evaluates `M`'s pure body and terminator for one incoming edge `edge`, returning the resolved
/// successor target if `M`'s control flow is statically decided for `edge`.
fn eval_block_for_edge<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &Cfg<A>,
    m_block: u32,
    edge: InEdge,
    global_vals: &[EvalVal],
    local_buf: &mut Vec<EvalVal, A>,
) -> Option<TargetSlice> {
    let range = view.block(Block(m_block));
    let params = view.params(Block(m_block));
    let term = range.end.checked_sub(1)?;
    let term_op = view.op(term);

    // Pattern B: unconditional `Op::Jump` resolves immediately for every incoming plain edge.
    if term_op == Op::Jump {
        return target_by_ordinal(view, term, 0);
    }

    if !matches!(term_op, Op::Br | Op::Switch) {
        return None;
    }

    let m_len = range.end.saturating_sub(range.start);
    local_buf.clear();
    local_buf.resize(m_len, EvalVal::Unknown);

    // Derive facts along `edge` (`edge.pred -> m_block`) and its dominating unique-edge ancestors.
    let mut facts = EdgeFacts::empty();
    let pred_term = view.terminator(Block(edge.pred));
    derive_edge_facts(view, pred_term, edge.ordinal, global_vals, &mut facts);
    collect_dom_edge_facts(view, cfg, edge.pred, global_vals, &mut facts);

    // Seed M's parameters from the incoming edge's arguments refined by `facts`.
    let param_count = params.end.saturating_sub(params.start);
    if edge.args_len != param_count {
        return None;
    }
    for k in 0..param_count {
        let arg = at(view.extra, edge.args_start + k, NONE);
        let mut val = facts.lookup(arg, global_vals);
        if val == EvalVal::Unknown && !facts.is_empty() {
            let a_idx = to_idx(arg);
            if a_idx < view.len() && is_pure_thread_op(view.op(a_idx)) {
                val = eval_pure_inst(view, a_idx, |v| facts.lookup(v, global_vals));
            }
        }
        set_at(local_buf, k, val);
    }

    let lookup = |v: u32, local: &[EvalVal]| -> EvalVal {
        let idx = to_idx(v);
        if idx >= range.start && idx < range.end {
            let lv = at(local, idx - range.start, EvalVal::Unknown);
            if lv != EvalVal::Unknown {
                return lv;
            }
            at(global_vals, idx, EvalVal::Unknown)
        } else {
            facts.lookup(v, global_vals)
        }
    };

    // Abstractly evaluate M's body instructions in order.
    for inst in params.end..term {
        let val = eval_pure_inst(view, inst, |v| lookup(v, local_buf));
        set_at(local_buf, inst - range.start, val);
    }

    let chosen = eval_terminator_static(view, term, |v| lookup(v, local_buf))?;
    target_by_ordinal(view, term, chosen)
}

// ---------------------------------------------------------------------------
// Per-block threading plan & round rewriter
// ---------------------------------------------------------------------------

/// Whether `inst` is a zero-operand pure constant instruction safe to hoist to the entry block
/// so threaded edges and external uses remain dominated without introducing a block parameter.
#[inline]
fn is_hoistable_const(view: &View<'_>, inst: usize) -> bool {
    match view.op(inst) {
        Op::Iconst | Op::IconstWide | Op::Fconst | Op::FconstWide | Op::Null | Op::SlotAddr => true,
        Op::Uninit => Ty(at(view.tys, inst, 0)).is_scalar(),
        _ => false,
    }
}

/// One reachable incoming edge in the post-threading CFG `G'`.
#[derive(Clone, Copy, Debug)]
struct PostInEdge {
    pred: u32,
    is_threaded: bool,
}

/// Post-threading CFG `G'` for a single candidate block `m_block`: immediate dominators,
/// reachable incoming edges (tagged threaded vs normal), and dominance frontiers.
struct PostCfg<A: Allocator> {
    idom: Vec<u32, A>,
    in_start: Vec<u32, A>,
    in_edges: Vec<PostInEdge, A>,
    df_start: Vec<u32, A>,
    df_blocks: Vec<u32, A>,
}

impl<A: Allocator + Copy> PostCfg<A> {
    fn build(view: &View<'_>, threaded: &[ThreadedEdge], alloc: A) -> Option<Self> {
        let blocks = view.blocks();
        let mut succ_start = Vec::with_capacity_in(blocks + 1, alloc);
        let mut succs: Vec<(u32, bool), A> = Vec::new_in(alloc);
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            succ_start.push(to_u32(succs.len()));
            each(view, view.terminator(Block(b_u32)), |item| {
                if let Item::Edge(edge) = item {
                    if edge.args.is_some() {
                        if let Some(te) = threaded.iter().find(|e| e.pred == b_u32 && e.ordinal == edge.ordinal) {
                            succs.push((te.target.block, true));
                            return;
                        }
                    }
                    succs.push((edge.block, false));
                }
            });
        }
        succ_start.push(to_u32(succs.len()));

        // Iterative reverse postorder from block 0 in `G'`.
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
        let mut seen = filled(blocks, false, &alloc);
        let mut stack: Vec<(u32, u32), A> = Vec::new_in(alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack.push((0, at(&succ_start, 0, 0)));
        }
        while let Some(top) = stack.last_mut() {
            let (b, next) = *top;
            let b_idx = to_idx(b);
            let end = at(&succ_start, b_idx + 1, 0);
            if next < end {
                top.1 = next + 1;
                let s = at(&succs, to_idx(next), (0, false)).0;
                let s_idx = to_idx(s);
                if !at(&seen, s_idx, true) {
                    set_at(&mut seen, s_idx, true);
                    stack.push((s, at(&succ_start, s_idx, 0)));
                }
            } else {
                post.push(b);
                stack.pop();
            }
        }
        let mut rpo = Vec::with_capacity_in(post.len(), alloc);
        rpo.extend_from_slice(&post);
        rpo.reverse();

        let mut rpo_order = filled(blocks, NONE, &alloc);
        for (idx, &b) in rpo.iter().enumerate() {
            set_at(&mut rpo_order, to_idx(b), to_u32(idx));
        }

        // Collect reachable predecessor pairs (for idom/DF) and tagged incoming edges (for Need).
        let mut pred_pairs: Vec<(u32, u32), A> = Vec::new_in(alloc);
        let mut in_counts = filled(blocks + 1, 0u32, &alloc);
        for &b in rpo.iter() {
            let b_idx = to_idx(b);
            let s_s = to_idx(at(&succ_start, b_idx, 0));
            let s_e = to_idx(at(&succ_start, b_idx + 1, 0));
            if let Some(slice) = succs.get(s_s..s_e) {
                for &(to, _) in slice {
                    pred_pairs.push((to, b));
                    if let Some(c) = in_counts.get_mut(to_idx(to) + 1) {
                        *c += 1;
                    }
                }
            }
        }

        // Deduplicated reachable predecessor CSR.
        pred_pairs.sort_unstable();
        pred_pairs.dedup();
        let mut pred_start = filled(blocks + 1, 0u32, &alloc);
        for &(to, _) in pred_pairs.iter() {
            if let Some(c) = pred_start.get_mut(to_idx(to) + 1) {
                *c += 1;
            }
        }
        for b in 0..blocks {
            let prev = at(&pred_start, b, 0);
            if let Some(cur) = pred_start.get_mut(b + 1) {
                *cur += prev;
            }
        }
        let mut preds: Vec<u32, A> = Vec::with_capacity_in(pred_pairs.len(), alloc);
        for &(_, from) in pred_pairs.iter() {
            preds.push(from);
        }

        // Tagged reachable incoming edge CSR (preserving parallel threaded + normal edges).
        for b in 0..blocks {
            let prev = at(&in_counts, b, 0);
            if let Some(cur) = in_counts.get_mut(b + 1) {
                *cur += prev;
            }
        }
        let in_start = in_counts;
        let total_in = to_idx(at(&in_start, blocks, 0));
        let mut in_fill = filled(blocks, 0u32, &alloc);
        let mut in_edges = filled(total_in, PostInEdge { pred: 0, is_threaded: false }, &alloc);
        for &b in rpo.iter() {
            let b_idx = to_idx(b);
            let s_s = to_idx(at(&succ_start, b_idx, 0));
            let s_e = to_idx(at(&succ_start, b_idx + 1, 0));
            if let Some(slice) = succs.get(s_s..s_e) {
                for &(to, is_threaded) in slice {
                    let to_idx_v = to_idx(to);
                    let pos = to_idx(at(&in_start, to_idx_v, 0)) + to_idx(at(&in_fill, to_idx_v, 0));
                    set_at(&mut in_edges, pos, PostInEdge { pred: b, is_threaded });
                    if let Some(f) = in_fill.get_mut(to_idx_v) {
                        *f += 1;
                    }
                }
            }
        }

        // Iterative Cooper-Harvey-Kennedy dominators on `G'`.
        let mut idom = filled(blocks, NONE, &alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        let mut settled = false;
        let max_dom_iters = blocks.saturating_mul(2).saturating_add(2);
        for _ in 0..max_dom_iters {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let b_idx = to_idx(b);
                let p_s = to_idx(at(&pred_start, b_idx, 0));
                let p_e = to_idx(at(&pred_start, b_idx + 1, 0));
                let mut new_idom = NONE;
                if let Some(slice) = preds.get(p_s..p_e) {
                    for &p in slice {
                        if at(&idom, to_idx(p), NONE) == NONE {
                            continue;
                        }
                        new_idom = match new_idom {
                            NONE => p,
                            cur => intersect_dom(&idom, &rpo_order, p, cur),
                        };
                    }
                }
                if new_idom != at(&idom, b_idx, NONE) {
                    set_at(&mut idom, b_idx, new_idom);
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            return None;
        }

        // Dominance frontiers `DF'(b)` via Cooper-Harvey-Kennedy.
        let mut df_pairs: Vec<(u32, u32), A> = Vec::new_in(alloc);
        for &b in rpo.iter() {
            let b_idx = to_idx(b);
            let p_s = to_idx(at(&pred_start, b_idx, 0));
            let p_e = to_idx(at(&pred_start, b_idx + 1, 0));
            if p_e.saturating_sub(p_s) < 2 {
                continue;
            }
            let b_idom = at(&idom, b_idx, NONE);
            if let Some(slice) = preds.get(p_s..p_e) {
                for &p in slice {
                    let mut runner = p;
                    for _ in 0..=blocks {
                        if runner == b_idom || runner == NONE {
                            break;
                        }
                        df_pairs.push((runner, b));
                        let parent = at(&idom, to_idx(runner), NONE);
                        if parent == runner {
                            break;
                        }
                        runner = parent;
                    }
                }
            }
        }
        df_pairs.sort_unstable();
        df_pairs.dedup();
        let mut df_start = filled(blocks + 1, 0u32, &alloc);
        for &(runner, _) in df_pairs.iter() {
            if let Some(c) = df_start.get_mut(to_idx(runner) + 1) {
                *c += 1;
            }
        }
        for b in 0..blocks {
            let prev = at(&df_start, b, 0);
            if let Some(cur) = df_start.get_mut(b + 1) {
                *cur += prev;
            }
        }
        let mut df_blocks: Vec<u32, A> = Vec::with_capacity_in(df_pairs.len(), alloc);
        for &(_, target) in df_pairs.iter() {
            df_blocks.push(target);
        }

        Some(Self { idom, in_start, in_edges, df_start, df_blocks })
    }

    #[inline]
    fn reachable(&self, b: u32) -> bool {
        at(&self.idom, to_idx(b), NONE) != NONE
    }

    fn incoming_of(&self, b: u32) -> &[PostInEdge] {
        let idx = to_idx(b);
        let s = to_idx(at(&self.in_start, idx, 0));
        let e = to_idx(at(&self.in_start, idx + 1, 0));
        self.in_edges.get(s..e).unwrap_or(&[])
    }

    fn df_of(&self, b: u32) -> &[u32] {
        let idx = to_idx(b);
        let s = to_idx(at(&self.df_start, idx, 0));
        let e = to_idx(at(&self.df_start, idx + 1, 0));
        self.df_blocks.get(s..e).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, Debug)]
struct ThreadedEdge {
    pred: u32,
    ordinal: u32,
    m_block: u32,
    target: TargetSlice,
}

/// One parameter of `m_block` promoted to a new parameter of block `succ` (either a threaded
/// target of `M` or an iterated-dominance-frontier merge block such as a shared loop exit).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Promotion {
    succ: u32,
    m_param_inst: u32,
    m_param_idx: u32,
}

struct BlockPlan<A: Allocator> {
    m_block: u32,
    edges: Vec<ThreadedEdge, A>,
    promotions: Vec<Promotion, A>,
    hoisted: Vec<u32, A>,
    post_idom: Vec<u32, A>,
}

/// Plans threading for candidate block `m_block`, checking pureness and reconstructing SSA via
/// iterated dominance frontiers on the post-threading CFG for externally used parameters of `M`.
fn plan_block_threading<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &Cfg<A>,
    cross: &CrossUses<A>,
    global_vals: &[EvalVal],
    m_block: u32,
    local_buf: &mut Vec<EvalVal, A>,
    alloc: A,
) -> Option<BlockPlan<A>> {
    if m_block == 0 || !cfg.reachable(m_block) || view.block_kind(Block(m_block)).is_landing() {
        return None;
    }
    let range = view.block(Block(m_block));
    let params = view.params(Block(m_block));
    let term = range.end.checked_sub(1)?;
    let term_op = view.op(term);
    if !matches!(term_op, Op::Jump | Op::Br | Op::Switch) {
        return None;
    }
    let body_len = term.saturating_sub(params.end);
    if body_len > MAX_THREAD_BLOCK_INSTS {
        return None;
    }
    for inst in params.end..term {
        if !is_pure_thread_op(view.op(inst)) {
            return None;
        }
    }

    let in_edges = cfg.incoming_of(m_block);
    if in_edges.is_empty() {
        return None;
    }

    let mut threaded: Vec<ThreadedEdge, A> = Vec::new_in(alloc);
    let mut hoisted: Vec<u32, A> = Vec::new_in(alloc);

    for &edge in in_edges {
        if edge.pred == m_block {
            continue;
        }
        let Some(target) = eval_block_for_edge(view, cfg, m_block, edge, global_vals, local_buf) else {
            continue;
        };
        if target.block == m_block {
            continue;
        }
        // Every value forwarded by `M -> S(s_args)` must be a param of `M`, defined outside `M`,
        // or a zero-operand pure constant of `M` hoistable to block 0.
        let mut args_ok = true;
        let mut local_hoist: Vec<u32, A> = Vec::new_in(alloc);
        if let Some(s_args) = view.extra.get(target.args_start..target.args_start + target.args_len) {
            for &sv in s_args {
                let sv_idx = to_idx(sv);
                let is_m_param = sv_idx >= params.start && sv_idx < params.end;
                let outside_m = at(&cfg.owner, sv_idx, NONE) != m_block;
                if !is_m_param && !outside_m {
                    if is_hoistable_const(view, sv_idx) {
                        local_hoist.push(sv);
                    } else {
                        args_ok = false;
                        break;
                    }
                }
            }
        } else {
            args_ok = false;
        }
        if !args_ok {
            continue;
        }
        hoisted.extend_from_slice(&local_hoist);
        threaded.push(ThreadedEdge { pred: edge.pred, ordinal: edge.ordinal, m_block, target });
    }

    if threaded.is_empty() {
        return None;
    }

    // Fast path: if no instruction or parameter of `M` is used outside `M`, zero SSA
    // reconstruction is needed.
    let mut any_ext = false;
    for inst in range.start..term {
        if !cross.of(inst).is_empty() {
            any_ext = true;
            break;
        }
    }
    if !any_ext {
        hoisted.sort_unstable();
        hoisted.dedup();
        return Some(BlockPlan {
            m_block,
            edges: threaded,
            promotions: Vec::new_in(alloc),
            hoisted,
            post_idom: Vec::new_in(alloc),
        });
    }

    // Build the post-threading CFG `G'` and reconstruct SSA for every value of `M` whose
    // reachable external uses can be reached past a threaded edge.
    let post = PostCfg::build(view, &threaded, alloc)?;
    let blocks = view.blocks();
    let mut in_need = filled(blocks, false, &alloc);
    let mut need_list: Vec<u32, A> = Vec::new_in(alloc);
    let mut in_phi = filled(blocks, false, &alloc);
    let mut phi_list: Vec<u32, A> = Vec::new_in(alloc);
    let mut phi_work: Vec<u32, A> = Vec::new_in(alloc);
    let mut promotions: Vec<Promotion, A> = Vec::new_in(alloc);

    for inst in range.start..term {
        let ext_blocks = cross.of(inst);
        if ext_blocks.is_empty() {
            continue;
        }

        // Clear previous iteration's scratch markers in O(|Need| + |Phi|).
        for &b in need_list.iter() {
            set_at(&mut in_need, to_idx(b), false);
        }
        need_list.clear();
        for &b in phi_list.iter() {
            set_at(&mut in_phi, to_idx(b), false);
        }
        phi_list.clear();

        // Walk backwards in `G'` from every reachable external use along non-threaded edges
        // without crossing `m_block` to collect the exact region `Need(inst)` inside `OldDom(M)`.
        for &ub in ext_blocks {
            if post.reachable(ub) && !at(&in_need, to_idx(ub), true) {
                set_at(&mut in_need, to_idx(ub), true);
                need_list.push(ub);
            }
        }
        if need_list.is_empty() {
            continue;
        }

        let mut has_threaded_entry = false;
        let mut head = 0usize;
        while head < need_list.len() {
            let cur = at(&need_list, head, NONE);
            head += 1;
            for &ie in post.incoming_of(cur) {
                if ie.is_threaded {
                    has_threaded_entry = true;
                } else if ie.pred != m_block && post.reachable(ie.pred) {
                    let p_idx = to_idx(ie.pred);
                    if !at(&in_need, p_idx, true) {
                        set_at(&mut in_need, p_idx, true);
                        need_list.push(ie.pred);
                    }
                }
            }
        }

        // If no threaded edge enters `Need(inst)`, every dynamic path to any reachable use still
        // executes `m_block` last, so `m_block` dominates all uses and no rewrite is needed.
        if !has_threaded_entry {
            continue;
        }

        if inst >= params.end {
            if is_hoistable_const(view, inst) {
                hoisted.push(to_u32(inst));
                continue;
            }
            return None;
        }

        // Parameter `p_k` of `M`: place new block parameters at every block in `Need(p_k)` that
        // receives a reachable threaded edge, then close under `DF'` restricted to `Need(p_k)`.
        phi_work.clear();
        for &b in need_list.iter() {
            if post.incoming_of(b).iter().any(|ie| ie.is_threaded) {
                let b_idx = to_idx(b);
                if !at(&in_phi, b_idx, true) {
                    set_at(&mut in_phi, b_idx, true);
                    phi_list.push(b);
                    phi_work.push(b);
                }
            }
        }
        if post.reachable(m_block) {
            phi_work.push(m_block);
        }

        let mut w_head = 0usize;
        while w_head < phi_work.len() {
            let d = at(&phi_work, w_head, NONE);
            w_head += 1;
            for &y in post.df_of(d) {
                let y_idx = to_idx(y);
                if at(&in_need, y_idx, false) && !at(&in_phi, y_idx, true) {
                    set_at(&mut in_phi, y_idx, true);
                    phi_list.push(y);
                    phi_work.push(y);
                }
            }
        }

        // Every block receiving a promoted parameter must accept plain block-param arguments.
        for &b in phi_list.iter() {
            let b_idx = to_idx(b);
            if b == 0
                || b == m_block
                || !at(&cfg.plain_only, b_idx, false)
                || view.block_kind(Block(b)).is_landing()
            {
                return None;
            }
        }

        // Belt-and-suspenders SSA check: every block in `Need(p_k)` must be dominated in `G'` by
        // either a block in `phi_list` or `m_block`.
        for &b in need_list.iter() {
            let mut cur = b;
            let mut found_def = false;
            for _ in 0..=blocks {
                if cur == m_block || at(&in_phi, to_idx(cur), false) {
                    found_def = true;
                    break;
                }
                let parent = at(&post.idom, to_idx(cur), NONE);
                if parent == NONE || parent == cur {
                    break;
                }
                cur = parent;
            }
            if !found_def {
                return None;
            }
        }

        let m_param_inst = to_u32(inst);
        let m_param_idx = to_u32(inst - params.start);
        for &b in phi_list.iter() {
            promotions.push(Promotion { succ: b, m_param_inst, m_param_idx });
        }
    }

    promotions.sort_unstable();
    promotions.dedup();
    hoisted.sort_unstable();
    hoisted.dedup();

    let post_idom = if promotions.is_empty() {
        Vec::new_in(alloc)
    } else {
        post.idom
    };

    Some(BlockPlan { m_block, edges: threaded, promotions, hoisted, post_idom })
}

struct Fixup {
    place: usize,
    in_data: bool,
}

/// Applies one round's non-interfering `threaded` edges, `promotions`, and `hoisted` constants,
/// producing a verified `Func`.
#[allow(clippy::too_many_arguments)]
fn apply_round<A: Allocator + Copy>(
    view: &View<'_>,
    threaded: &[ThreadedEdge],
    promotions: &[Promotion],
    promo_m_block: u32,
    post_idom: &[u32],
    hoisted: &[u32],
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let len = view.len();
    let blocks = view.blocks();
    let total_virtual = len.saturating_add(promotions.len());
    if total_virtual >= to_idx(NONE) {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers in jump_thread" });
    }

    let m_params = if promo_m_block != NONE {
        view.params(Block(promo_m_block))
    } else {
        0..0
    };

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

    // Virtual value map: `[0..len]` for original instructions, `[len..len + promotions.len()]` for promoted params.
    let mut full_map = filled(total_virtual, NONE, &alloc);
    let mut fixups: Vec<Fixup, A> = Vec::new_in(alloc);

    let resolve_virtual = |old_v: u32, cur_block: usize| -> u32 {
        if promotions.is_empty() {
            return old_v;
        }
        let old_idx = to_idx(old_v);
        if old_idx < m_params.start || old_idx >= m_params.end {
            return old_v;
        }
        let cur_u32 = to_u32(cur_block);
        if cur_u32 == promo_m_block || at(post_idom, cur_block, NONE) == NONE {
            return old_v;
        }
        let mut cur = cur_u32;
        for _ in 0..=blocks {
            if cur == promo_m_block {
                return old_v;
            }
            if let Ok(idx) = promotions.binary_search_by(|pr| {
                pr.succ.cmp(&cur).then_with(|| pr.m_param_inst.cmp(&old_v))
            }) {
                return to_u32(len.saturating_add(idx));
            }
            let parent = at(post_idom, to_idx(cur), NONE);
            if parent == NONE || parent == cur {
                break;
            }
            cur = parent;
        }
        old_v
    };

    for b in 0..blocks {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b, 0));

        let range = view.block(Block(b_u32));
        let params = view.params(Block(b_u32));

        // 1. Original parameters of block `b`.
        for inst in params.start..params.end {
            let new_idx = to_u32(func.ops.len());
            func.ops.push(at(view.ops, inst, 0));
            func.flags.push(at(view.flags, inst, 0));
            func.tys.push(at(view.tys, inst, 0));
            func.data.push(at(view.data, inst, 0));
            func.attrs.push(at(view.attrs, inst, 0));
            func.locs.push(at(view.locs, inst, 0));
            set_at(&mut full_map, inst, new_idx);
        }

        // 2. Newly promoted parameters for block `b`.
        for (promo_idx, promo) in promotions.iter().enumerate() {
            if promo.succ != b_u32 {
                continue;
            }
            let new_idx = to_u32(func.ops.len());
            let src_ty = at(view.tys, to_idx(promo.m_param_inst), 0);
            func.ops.push(op_byte(Op::Param));
            func.flags.push(0);
            func.tys.push(src_ty);
            func.data.push(0);
            func.attrs.push(0);
            func.locs.push(0);
            set_at(&mut full_map, len.saturating_add(promo_idx), new_idx);
        }

        // 2b. Zero-operand constants hoisted from `M` into entry block `0`.
        if b == 0 {
            for &h_u32 in hoisted {
                let inst = to_idx(h_u32);
                let op = view.op(inst);
                let new_idx = func.ops.len();
                let data = match op.info().layout {
                    Layout::None => 0,
                    Layout::Data(_) => at(view.data, inst, 0),
                    Layout::Extra(_) => {
                        let extra_start = to_u32(func.extra.len());
                        let p = to_idx(at(view.data, inst, 0));
                        let w = to_idx(Ty(at(view.tys, inst, 0)).const_words());
                        for k in 0..w {
                            func.extra.push(at(view.extra, p + k, 0));
                        }
                        extra_start
                    }
                };
                func.ops.push(at(view.ops, inst, 0));
                func.flags.push(at(view.flags, inst, 0));
                func.tys.push(at(view.tys, inst, 0));
                func.data.push(data);
                func.attrs.push(at(view.attrs, inst, 0));
                func.locs.push(at(view.locs, inst, 0));
                set_at(&mut full_map, inst, to_u32(new_idx));
            }
        }

        // 3. Body instructions and terminator of block `b`.
        for inst in params.end..range.end {
            if !hoisted.is_empty() && hoisted.binary_search(&to_u32(inst)).is_ok() {
                continue;
            }
            let op = view.op(inst);
            let new_idx = func.ops.len();
            let data = match op.info().layout {
                Layout::None => 0,
                Layout::Data(Field::Value) => {
                    let old_v = at(view.data, inst, 0);
                    fixups.push(Fixup { place: new_idx, in_data: true });
                    resolve_virtual(old_v, b)
                }
                Layout::Data(_) => at(view.data, inst, 0),
                Layout::Extra(fields) => {
                    let extra_start = to_u32(func.extra.len());
                    let mut p = to_idx(at(view.data, inst, 0));
                    let mut ordinal = 0u32;
                    let mut scrutinee = None;

                    for &field in fields {
                        match field {
                            Field::Value => {
                                let old_v = at(view.extra, p, 0);
                                scrutinee = Some(old_v);
                                fixups.push(Fixup { place: func.extra.len(), in_data: false });
                                func.extra.push(resolve_virtual(old_v, b));
                                p += 1;
                            }
                            Field::Values => {
                                let n_u32 = at(view.extra, p, 0);
                                let n = to_idx(n_u32);
                                func.extra.push(n_u32);
                                p += 1;
                                for k in 0..n {
                                    let old_v = at(view.extra, p + k, 0);
                                    fixups.push(Fixup { place: func.extra.len(), in_data: false });
                                    func.extra.push(resolve_virtual(old_v, b));
                                }
                                p += n;
                            }
                            Field::Target => {
                                let orig_target = at(view.extra, p, 0);
                                let orig_n = to_idx(at(view.extra, p + 1, 0));
                                let orig_args_start = p + 2;
                                emit_target(
                                    view,
                                    b_u32,
                                    b,
                                    ordinal,
                                    orig_target,
                                    orig_args_start,
                                    orig_n,
                                    threaded,
                                    promotions,
                                    &resolve_virtual,
                                    &mut func,
                                    &mut fixups,
                                );
                                ordinal += 1;
                                p += 2 + orig_n;
                            }
                            Field::Cases => {
                                let width = scrutinee.map_or(0, |s| to_idx(Ty(at(view.tys, to_idx(s), 0)).const_words()));
                                let n_u32 = at(view.extra, p, 0);
                                let n = to_idx(n_u32);
                                func.extra.push(n_u32);
                                p += 1;
                                for _ in 0..n {
                                    for k in 0..width {
                                        func.extra.push(at(view.extra, p + k, 0));
                                    }
                                    p += width;
                                    let orig_target = at(view.extra, p, 0);
                                    let orig_n = to_idx(at(view.extra, p + 1, 0));
                                    let orig_args_start = p + 2;
                                    emit_target(
                                        view,
                                        b_u32,
                                        b,
                                        ordinal,
                                        orig_target,
                                        orig_args_start,
                                        orig_n,
                                        threaded,
                                        promotions,
                                        &resolve_virtual,
                                        &mut func,
                                        &mut fixups,
                                    );
                                    ordinal += 1;
                                    p += 2 + orig_n;
                                }
                            }
                            Field::Blocks | Field::Mask => {
                                let n_u32 = at(view.extra, p, 0);
                                let n = to_idx(n_u32);
                                func.extra.push(n_u32);
                                p += 1;
                                for k in 0..n {
                                    func.extra.push(at(view.extra, p + k, 0));
                                }
                                p += n;
                            }
                            Field::Const => {
                                let w = to_idx(Ty(at(view.tys, inst, 0)).const_words());
                                for k in 0..w {
                                    func.extra.push(at(view.extra, p + k, 0));
                                }
                                p += w;
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
                                func.extra.push(at(view.extra, p, 0));
                                p += 1;
                            }
                        }
                    }
                    extra_start
                }
            };
            func.ops.push(at(view.ops, inst, 0));
            func.flags.push(at(view.flags, inst, 0));
            func.tys.push(at(view.tys, inst, 0));
            func.data.push(data);
            func.attrs.push(at(view.attrs, inst, 0));
            func.locs.push(at(view.locs, inst, 0));
            set_at(&mut full_map, inst, to_u32(new_idx));
        }
    }

    // Resolve all virtual value IDs to final output instruction indices.
    for fix in fixups.iter() {
        let word = if fix.in_data {
            func.data.get_mut(fix.place)
        } else {
            func.extra.get_mut(fix.place)
        };
        let Some(slot) = word else {
            return Err(SimplifyError::Refused { inst: 0, what: "fixup out of bounds in jump_thread" });
        };
        let mapped = at(&full_map, to_idx(*slot), NONE);
        if mapped == NONE {
            return Err(SimplifyError::Refused {
                inst: 0,
                what: "a use of an unmapped value in jump_thread",
            });
        }
        *slot = mapped;
    }

    if u32::try_from(func.ops.len()).is_err() || u32::try_from(func.extra.len()).is_err() {
        return Err(SimplifyError::Refused { inst: 0, what: "an output past LIR's columns in jump_thread" });
    }
    if cfg!(debug_assertions) {
        verify(func.view()).map_err(SimplifyError::Verify)?;
    }
    Ok(func)
}

#[allow(clippy::too_many_arguments)]
fn emit_target<A: Allocator>(
    view: &View<'_>,
    b_u32: u32,
    b_idx: usize,
    ordinal: u32,
    orig_target: u32,
    orig_args_start: usize,
    orig_n: usize,
    threaded: &[ThreadedEdge],
    promotions: &[Promotion],
    resolve_virtual: &impl Fn(u32, usize) -> u32,
    func: &mut Func<A>,
    fixups: &mut Vec<Fixup, A>,
) {
    let found = threaded.iter().find(|e| e.pred == b_u32 && e.ordinal == ordinal);
    if let Some(te) = found {
        let dest = te.target.block;
        let m_params = view.params(Block(te.m_block));
        let promo_count = promotions.iter().filter(|pr| pr.succ == dest).count();
        func.extra.push(dest);
        func.extra.push(to_u32(te.target.args_len.saturating_add(promo_count)));
        for k in 0..te.target.args_len {
            let sv = at(view.extra, te.target.args_start + k, 0);
            let sv_idx = to_idx(sv);
            let substituted = if sv_idx >= m_params.start && sv_idx < m_params.end {
                at(view.extra, orig_args_start + (sv_idx - m_params.start), 0)
            } else {
                sv
            };
            fixups.push(Fixup { place: func.extra.len(), in_data: false });
            func.extra.push(resolve_virtual(substituted, b_idx));
        }
        for promo in promotions {
            if promo.succ == dest {
                let arg = at(view.extra, orig_args_start + to_idx(promo.m_param_idx), 0);
                fixups.push(Fixup { place: func.extra.len(), in_data: false });
                func.extra.push(resolve_virtual(arg, b_idx));
            }
        }
    } else {
        let promo_count = promotions.iter().filter(|pr| pr.succ == orig_target).count();
        func.extra.push(orig_target);
        func.extra.push(to_u32(orig_n.saturating_add(promo_count)));
        for k in 0..orig_n {
            let ov = at(view.extra, orig_args_start + k, 0);
            fixups.push(Fixup { place: func.extra.len(), in_data: false });
            func.extra.push(resolve_virtual(ov, b_idx));
        }
        for promo in promotions {
            if promo.succ == orig_target {
                fixups.push(Fixup { place: func.extra.len(), in_data: false });
                func.extra.push(resolve_virtual(promo.m_param_inst, b_idx));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public Entry Point
// ---------------------------------------------------------------------------

/// Threads phi-discriminant branches (`br`/`switch` on integer tags or niche `null`/non-null
/// pointers), collapses trivial forwarding blocks, and merges single-predecessor `jump` chains
/// over `view` in `alloc`, returning verified LIR.
pub fn thread_jumps<A: Allocator + Copy>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    if view.blocks() == 0 {
        return Err(SimplifyError::Refused { inst: 0, what: "a function with no blocks" });
    }
    let mut cur = view.to_func_in(alloc);
    let mut any_coalesced = false;

    for _round in 0..MAX_ROUNDS {
        let v = cur.view();
        let blocks = v.blocks();
        let cfg = Cfg::build(&v, alloc)?;
        let global_vals = compute_global_vals(&v, &cfg, alloc);
        let mut local_buf: Vec<EvalVal, A> = Vec::new_in(alloc);

        // Phase 1: direct terminator folding for any reachable block whose `Br` or `Switch`
        // outcome is statically decided by `global_vals` or dominating single-edge branch facts.
        let mut fold_jumps = filled(v.len(), NONE, &alloc);
        let mut any_fold = false;
        for &b in cfg.rpo.iter() {
            let term = v.terminator(Block(b));
            if !matches!(v.op(term), Op::Br | Op::Switch) {
                continue;
            }
            if let Some(chosen) = eval_block_terminator(&v, &cfg, b, &global_vals, &mut local_buf) {
                set_at(&mut fold_jumps, term, chosen);
                any_fold = true;
            }
        }
        if any_fold {
            let folded = fold_terminators(v, &fold_jumps, alloc)?;
            let cleaned = dead(folded.view(), alloc)?;
            cur = coalesce_cfg(cleaned.view(), alloc)?;
            any_coalesced = true;
            continue;
        }

        // Phase 2: phi-discriminant & forwarding-block edge threading (Patterns A & B).
        let cross = CrossUses::build(&v, &cfg, alloc);
        let mut round_edges: Vec<ThreadedEdge, A> = Vec::new_in(alloc);
        let mut round_promos: Vec<Promotion, A> = Vec::new_in(alloc);
        let mut round_hoisted: Vec<u32, A> = Vec::new_in(alloc);
        let mut round_promo_m: u32 = NONE;
        let mut round_post_idom: Vec<u32, A> = Vec::new_in(alloc);
        let mut claimed_m = filled(blocks, false, &alloc);
        let mut claimed_ep = filled(blocks, false, &alloc);

        // Bottom-up walk so nested multi-level Switch chains (B0 -> B1 -> Body) thread inner B1
        // before outer B0, collapsing cross-block payload uses cleanly in consecutive rounds.
        for m in (1..blocks).rev() {
            if at(&claimed_m, m, false) || at(&claimed_ep, m, false) {
                continue;
            }
            let Some(plan) = plan_block_threading(&v, &cfg, &cross, &global_vals, to_u32(m), &mut local_buf, alloc)
            else {
                continue;
            };
            if !plan.promotions.is_empty() || !plan.hoisted.is_empty() {
                if !round_edges.is_empty() {
                    // Defer promotion/hoist-bearing block to next round so scopes stay disjoint.
                    continue;
                }
                round_edges.extend_from_slice(&plan.edges);
                round_promos.extend_from_slice(&plan.promotions);
                round_hoisted.extend_from_slice(&plan.hoisted);
                round_promo_m = plan.m_block;
                round_post_idom = plan.post_idom;
                break;
            }
            // Zero-promotion threading: ensure none of this block's endpoints collide with an
            // already-accepted `M` in the same round.
            let mut conflict = false;
            for edge in plan.edges.iter() {
                if at(&claimed_m, to_idx(edge.pred), false) || at(&claimed_m, to_idx(edge.target.block), false) {
                    conflict = true;
                    break;
                }
            }
            if conflict {
                continue;
            }
            set_at(&mut claimed_m, to_idx(plan.m_block), true);
            for edge in plan.edges.iter() {
                set_at(&mut claimed_ep, to_idx(edge.pred), true);
                set_at(&mut claimed_ep, to_idx(edge.target.block), true);
            }
            round_edges.extend_from_slice(&plan.edges);
        }

        if round_edges.is_empty() {
            if !any_coalesced {
                cur = coalesce_cfg(cur.view(), alloc)?;
            }
            break;
        }

        let threaded_func = apply_round(
            &v,
            &round_edges,
            &round_promos,
            round_promo_m,
            &round_post_idom,
            &round_hoisted,
            alloc,
        )?;
        cur = coalesce_cfg(threaded_func.view(), alloc)?;
        any_coalesced = true;
    }

    verify(cur.view()).map_err(SimplifyError::Verify)?;
    Ok(cur)
}

// ---------------------------------------------------------------------------
// Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BlockKind, Builder, Full,
        builder::{Facts, MemAttrs, Param, Variadic},
        eval::{Outcome, Val, run_in},
        func::{Conv, SlotKind},
        print,
        simplify::{dead, propagate, scalar_replace},
    };

    const FUEL: u64 = 200_000;

    fn count_op(view: View<'_>, target: Op) -> usize {
        (0..view.len()).filter(|&inst| view.op(inst) == target).count()
    }

    /// Full end-to-end inlined `Range::into_iter` + `Range::next` loop collapse test:
    /// starts with stack-allocated `Range<usize>` (16B) + `Memcpy` into `iter_slot` +
    /// `sret Option<usize>` (16B, partially written on `None` arm!) + `Switch` on tag,
    /// runs `scalar_replace -> propagate -> thread_jumps -> propagate -> dead`, verifies:
    /// - 0 `StackSlot`s, 0 `Switch`es, 0 memory ops
    /// - exact 2-block canonical counted loop (`entry -> Header(acc, i) -> Body -> Header / Exit`)
    /// - exact evaluator equivalence across `n in [0, 1, 2, 5, 10, 50]`.
    #[test]
    fn inlined_range_next_collapses_to_canonical_counted_loop() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let [entry, header, some_arm, none_arm, join, unreach, body, exit] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let range_slot = b.slot(16, 3, SlotKind::Temp);
        let iter_slot = b.slot(16, 3, SlotKind::Temp);
        let opt_slot = b.slot(16, 3, SlotKind::Temp);
        let p_range = b.slot_addr(range_slot);
        let p_iter = b.slot_addr(iter_slot);
        let zero = b.iconst(Ty::I64, 0);
        b.store(Flags::NONE, zero, p_range, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, n, p_range, 8, MemAttrs::aligned(3));
        let len16 = b.iconst(Ty::I64, 16);
        b.bulk(Op::Memcpy, Flags::NONE, p_iter, p_range, len16, MemAttrs::aligned(3));
        b.jump((header, &[zero]));

        b.begin(header);
        let acc = b.param(Ty::I64);
        let p_it = b.slot_addr(iter_slot);
        let cur = b.load(Ty::I64, Flags::NONE, p_it, 0, MemAttrs::aligned(3));
        let end = b.load(Ty::I64, Flags::NONE, p_it, 8, MemAttrs::aligned(3));
        let has_next = b.icmp(IntCond::Ult, cur, end);
        b.br(Flags::NONE, has_next, (some_arm, &[]), (none_arm, &[]));

        b.begin(some_arm);
        let one = b.iconst(Ty::I64, 1);
        let next_i = b.binary(Op::Add, Flags::NONE, cur, one);
        let p_it2 = b.slot_addr(iter_slot);
        b.store(Flags::NONE, next_i, p_it2, 0, MemAttrs::aligned(3));
        let p_opt = b.slot_addr(opt_slot);
        b.store(Flags::NONE, one, p_opt, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, cur, p_opt, 8, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(none_arm);
        let p_opt2 = b.slot_addr(opt_slot);
        let tag_none = b.iconst(Ty::I64, 0);
        // Partially written Option::None: payload at +8 is NOT written!
        b.store(Flags::NONE, tag_none, p_opt2, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(join);
        let p_opt3 = b.slot_addr(opt_slot);
        let tag = b.load(Ty::I64, Flags::NONE, p_opt3, 0, MemAttrs::aligned(3));
        b.switch(tag, (unreach, &[]), &[(0, (exit, &[])), (1, (body, &[]))]);

        b.begin(unreach);
        b.unreachable();

        b.begin(body);
        let p_opt4 = b.slot_addr(opt_slot);
        let val = b.load(Ty::I64, Flags::NONE, p_opt4, 8, MemAttrs::aligned(3));
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, val);
        b.jump((header, &[acc_next]));

        b.begin(exit);
        b.ret(&[acc]);

        let input = b.finish_checked().expect("valid input LIR");

        let s1 = scalar_replace(input.view(), alloc).expect("sroa");
        let s2 = propagate(s1.view(), alloc).expect("propagate 1");
        let s3 = thread_jumps(s2.view(), alloc).expect("thread_jumps");
        let s4 = propagate(s3.view(), alloc).expect("propagate 2");
        let out = dead(s4.view(), alloc).expect("dead");
        verify(out.view()).expect("final output verifies");

        let v = out.view();
        assert_eq!(out.slots.len(), 0, "expected 0 stack slots:\n{}", print(v));
        assert_eq!(count_op(v, Op::Switch), 0, "expected 0 switches:\n{}", print(v));
        assert_eq!(count_op(v, Op::Load), 0, "expected 0 loads:\n{}", print(v));
        assert_eq!(count_op(v, Op::Store), 0, "expected 0 stores:\n{}", print(v));
        assert_eq!(count_op(v, Op::Memcpy), 0, "expected 0 memcpys:\n{}", print(v));
        assert_eq!(count_op(v, Op::Br), 1, "expected exactly 1 loop branch:\n{}", print(v));
        // Exact 4-block canonical counted loop CFG: entry -> header <-> body, header -> exit.
        assert_eq!(v.blocks(), 4, "expected 4 blocks (entry, header, body, exit):\n{}", print(v));

        for n_val in [0u128, 1, 2, 5, 10, 50] {
            let expected = n_val * n_val.saturating_sub(1) / 2;
            let before = run_in(input.view(), &[Val::Int(n_val)], FUEL, alloc).expect("eval input");
            let after = run_in(v, &[Val::Int(n_val)], FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for n={n_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(expected)]),
                "unexpected result for n={n_val}: {after:?}"
            );
        }
    }

    /// Full end-to-end inlined `slice::Iter::next` loop collapse test (niche pointer `Option<&T>`):
    /// `iter_slot` holds `{ ptr: ptr, end: ptr }` and `opt_slot` holds niche `Option<&i64>`
    /// (`null` = `None`, non-null = `Some(elem_ptr)`). Runs the full pipeline and verifies:
    /// - BOTH `iter_slot` and `opt_slot` promote 100% (only the backing slice `data_slot` remains)
    /// - 0 `Op::Null` instructions remain
    /// - exact 4-block canonical loop (`entry -> header <-> body, header -> exit`)
    /// - exact evaluator equivalence across `len in [0, 1, 2, 3, 4]`.
    #[test]
    fn inlined_slice_iter_next_collapses_niche_option_loop() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let [entry, header, some_arm, none_arm, join, body, exit] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let len = b.param(Ty::I64);
        let data_slot = b.slot(32, 3, SlotKind::Temp);
        let iter_slot = b.slot(16, 3, SlotKind::Temp);
        let opt_slot = b.slot(8, 3, SlotKind::Temp);
        let base = b.slot_addr(data_slot);
        for (idx, val) in [10u128, 20, 30, 40].into_iter().enumerate() {
            let c = b.iconst(Ty::I64, val);
            b.store(Flags::NONE, c, base, (idx * 8) as i32, MemAttrs::aligned(3));
        }
        let eight_e = b.iconst(Ty::I64, 8);
        let byte_len = b.binary(Op::Mul, Flags::NONE, len, eight_e);
        let end_ptr = b.ptr_add(Flags::INBOUNDS, base, byte_len);
        let p_iter = b.slot_addr(iter_slot);
        b.store(Flags::NONE, base, p_iter, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, end_ptr, p_iter, 8, MemAttrs::aligned(3));
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));

        b.begin(header);
        let acc = b.param(Ty::I64);
        let p_it = b.slot_addr(iter_slot);
        let cur = b.load(Ty::PTR, Flags::NONE, p_it, 0, MemAttrs::aligned(3));
        let end = b.load(Ty::PTR, Flags::NONE, p_it, 8, MemAttrs::aligned(3));
        let done = b.icmp(IntCond::Eq, cur, end);
        b.br(Flags::NONE, done, (none_arm, &[]), (some_arm, &[]));

        b.begin(some_arm);
        let eight = b.iconst(Ty::I64, 8);
        let next_ptr = b.ptr_add(Flags::INBOUNDS, cur, eight);
        let p_it2 = b.slot_addr(iter_slot);
        b.store(Flags::NONE, next_ptr, p_it2, 0, MemAttrs::aligned(3));
        let p_opt = b.slot_addr(opt_slot);
        b.store(Flags::NONE, cur, p_opt, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(none_arm);
        let p_opt2 = b.slot_addr(opt_slot);
        let null_p = b.null();
        b.store(Flags::NONE, null_p, p_opt2, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(join);
        let p_opt3 = b.slot_addr(opt_slot);
        let opt_ptr = b.load(Ty::PTR, Flags::NONE, p_opt3, 0, MemAttrs::aligned(3));
        let null_cmp = b.null();
        let is_none = b.icmp(IntCond::Eq, opt_ptr, null_cmp);
        b.br(Flags::NONE, is_none, (exit, &[]), (body, &[]));

        b.begin(body);
        let p_opt4 = b.slot_addr(opt_slot);
        let elem_ptr = b.load(Ty::PTR, Flags::NONE, p_opt4, 0, MemAttrs::aligned(3));
        let elem = b.load(Ty::I64, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(3));
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, elem);
        b.jump((header, &[acc_next]));

        b.begin(exit);
        b.ret(&[acc]);

        let input = b.finish_checked().expect("valid slice iter input");

        let s1 = scalar_replace(input.view(), alloc).expect("sroa");
        let s2 = propagate(s1.view(), alloc).expect("propagate 1");
        let s3 = thread_jumps(s2.view(), alloc).expect("thread_jumps");
        let s4 = propagate(s3.view(), alloc).expect("propagate 2");
        let out = dead(s4.view(), alloc).expect("dead");
        verify(out.view()).expect("final output verifies");

        let v = out.view();
        assert_eq!(out.slots.len(), 1, "only data_slot should remain:\n{}", print(v));
        assert_eq!(count_op(v, Op::Null), 0, "expected 0 null niche checks left:\n{}", print(v));
        assert_eq!(count_op(v, Op::Br), 1, "expected 1 loop branch:\n{}", print(v));
        assert_eq!(v.blocks(), 4, "expected 4 blocks (entry, header, body, exit):\n{}", print(v));

        for (len_val, expected) in [(0u128, 0u128), (1, 10), (2, 30), (3, 60), (4, 100)] {
            let before = run_in(input.view(), &[Val::Int(len_val)], FUEL, alloc).expect("eval input");
            let after = run_in(v, &[Val::Int(len_val)], FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for len={len_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(expected)]),
                "unexpected result for len={len_val}: {after:?}"
            );
        }
    }

    /// Tests external `nonnull` slice pointer parameter (`&[i64]`) through niche `Option<&i64>`
    /// loop collapse, confirming 0 stack slots and 0 null niche checks remain.
    #[test]
    fn nonnull_param_slice_iter_collapses_to_canonical_loop() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let mut ptr_param = Param::of(Ty::PTR);
        ptr_param.facts = Facts { bits: ParamFactsRec::NONNULL, ..Facts::default() };
        let sig = b.sig(Conv::C, Variadic::No, &[ptr_param, Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let [entry, header, some_arm, none_arm, join, body, exit] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let start_p = b.param(Ty::PTR);
        let end_p = b.param(Ty::PTR);
        let iter_slot = b.slot(16, 3, SlotKind::Temp);
        let opt_slot = b.slot(8, 3, SlotKind::Temp);
        let p_iter = b.slot_addr(iter_slot);
        b.store(Flags::NONE, start_p, p_iter, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, end_p, p_iter, 8, MemAttrs::aligned(3));
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));

        b.begin(header);
        let acc = b.param(Ty::I64);
        let p_it = b.slot_addr(iter_slot);
        let cur = b.load(Ty::PTR, Flags::NONE, p_it, 0, MemAttrs::aligned(3));
        let end = b.load(Ty::PTR, Flags::NONE, p_it, 8, MemAttrs::aligned(3));
        let done = b.icmp(IntCond::Eq, cur, end);
        b.br(Flags::NONE, done, (none_arm, &[]), (some_arm, &[]));

        b.begin(some_arm);
        let eight = b.iconst(Ty::I64, 8);
        let next_ptr = b.ptr_add(Flags::INBOUNDS, cur, eight);
        let p_it2 = b.slot_addr(iter_slot);
        b.store(Flags::NONE, next_ptr, p_it2, 0, MemAttrs::aligned(3));
        let p_opt = b.slot_addr(opt_slot);
        b.store(Flags::NONE, cur, p_opt, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(none_arm);
        let p_opt2 = b.slot_addr(opt_slot);
        let null_p = b.null();
        b.store(Flags::NONE, null_p, p_opt2, 0, MemAttrs::aligned(3));
        b.jump((join, &[]));

        b.begin(join);
        let p_opt3 = b.slot_addr(opt_slot);
        let opt_ptr = b.load(Ty::PTR, Flags::NONE, p_opt3, 0, MemAttrs::aligned(3));
        let null_cmp = b.null();
        let is_some = b.icmp(IntCond::Ne, opt_ptr, null_cmp);
        b.br(Flags::NONE, is_some, (body, &[]), (exit, &[]));

        b.begin(body);
        let p_opt4 = b.slot_addr(opt_slot);
        let elem_ptr = b.load(Ty::PTR, Flags::NONE, p_opt4, 0, MemAttrs::aligned(3));
        let elem = b.load(Ty::I64, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(3));
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, elem);
        b.jump((header, &[acc_next]));

        b.begin(exit);
        b.ret(&[acc]);

        let input = b.finish_checked().expect("valid nonnull slice iter input");
        let s1 = scalar_replace(input.view(), alloc).expect("sroa");
        let s2 = propagate(s1.view(), alloc).expect("propagate 1");
        let s3 = thread_jumps(s2.view(), alloc).expect("thread_jumps");
        let s4 = propagate(s3.view(), alloc).expect("propagate 2");
        let out = dead(s4.view(), alloc).expect("dead");
        verify(out.view()).expect("final output verifies");

        let v = out.view();
        assert_eq!(out.slots.len(), 0, "expected 0 slots:\n{}", print(v));
        assert_eq!(count_op(v, Op::Null), 0, "expected 0 nulls:\n{}", print(v));
        assert_eq!(count_op(v, Op::Br), 1, "expected 1 branch:\n{}", print(v));
        assert_eq!(v.blocks(), 4, "expected 4 blocks:\n{}", print(v));
    }

    /// Tests Pattern A (`icmp` + `br` on integer phi), Pattern B (chained forwarding blocks),
    /// and Pattern C (single-predecessor merge) standalone via `thread_jumps`.
    #[test]
    fn threads_icmp_br_forwarding_chain_and_single_pred_merge() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let [entry, left, right, fwd, join, take_a, take_b, tail] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let c = b.icmp(IntCond::Sgt, x, z);
        b.br(Flags::NONE, c, (left, &[]), (right, &[]));

        b.begin(left);
        let one = b.iconst(Ty::I64, 1);
        b.jump((fwd, &[one, x]));

        b.begin(right);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((fwd, &[zero, y]));

        // Trivial forwarding block (Pattern B).
        b.begin(fwd);
        let f_tag = b.param(Ty::I64);
        let f_val = b.param(Ty::I64);
        b.jump((join, &[f_tag, f_val]));

        // Phi-discriminant join block (Pattern A).
        b.begin(join);
        let tag = b.param(Ty::I64);
        let val = b.param(Ty::I64);
        let zero_cmp = b.iconst(Ty::I64, 0);
        let is_zero = b.icmp(IntCond::Eq, tag, zero_cmp);
        b.br(Flags::NONE, is_zero, (take_b, &[]), (take_a, &[]));

        b.begin(take_a);
        let ten = b.iconst(Ty::I64, 10);
        let a_res = b.binary(Op::Add, Flags::NONE, val, ten);
        b.jump((tail, &[a_res]));

        b.begin(take_b);
        let twenty = b.iconst(Ty::I64, 20);
        let b_res = b.binary(Op::Add, Flags::NONE, val, twenty);
        b.jump((tail, &[b_res]));

        b.begin(tail);
        let out_val = b.param(Ty::I64);
        b.ret(&[out_val]);

        let input = b.finish_checked().expect("valid input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        assert_eq!(count_op(out.view(), Op::Br), 1, "join branch must be threaded away:\n{}", print(out.view()));
        assert_eq!(out.view().blocks(), 4, "expected entry, left+take_a, right+take_b, tail:\n{}", print(out.view()));

        for args in [[5u128, 7], [0, 7]] {
            let vals = args.map(Val::Int);
            let before = run_in(input.view(), &vals, FUEL, alloc).expect("eval input");
            let after = run_in(out.view(), &vals, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "mismatch for {args:?}");
        }
    }

    /// Exact Rust `slice::Iter::next` niche `Option<&T>` discriminant pattern:
    /// `d = ptr_addr(opt_ptr)` -> `is_le0 = icmp ule d, 0` -> `tag = select is_le0, d, 1` ->
    /// `trunc i1 tag` -> `br`. Verifies `thread_jumps` collapses to 1 branch, 0 `Select`,
    /// 0 `PtrAddr`, and exact evaluator equivalence via `run_in`.
    #[test]
    fn threads_niche_ptr_to_int_select_ule_zero_loop() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let mut ptr_param = Param::of(Ty::PTR);
        ptr_param.facts = Facts { bits: ParamFactsRec::NONNULL, ..Facts::default() };
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[ptr_param, Param::of(Ty::I64)],
            &[Param::of(Ty::I64), Param::of(Ty::PTR)],
        );
        b.set_signature(sig);

        let [entry, header, some_arm, none_arm, merge, body, exit] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let start_ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let init_last = b.null();
        b.jump((header, &[zero, zero, init_last]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let last_ptr = b.param(Ty::PTR);
        let has_next = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, has_next, (some_arm, &[]), (none_arm, &[]));

        b.begin(some_arm);
        b.jump((merge, &[start_ptr]));

        b.begin(none_arm);
        let null_p = b.null();
        b.jump((merge, &[null_p]));

        b.begin(merge);
        let opt_ptr = b.param(Ty::PTR);
        let d = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, opt_ptr);
        let z0 = b.iconst(Ty::I64, 0);
        let is_le0 = b.icmp(IntCond::Ule, d, z0);
        let c1 = b.iconst(Ty::I64, 1);
        let tag = b.select(Flags::NONE, is_le0, d, c1);
        let is_some = b.convert(Op::Trunc, Flags::NONE, Ty::I1, tag);
        b.br(Flags::NONE, is_some, (body, &[]), (exit, &[]));

        b.begin(body);
        let one = b.iconst(Ty::I64, 1);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, i);
        b.jump((header, &[i_next, acc_next, opt_ptr]));

        b.begin(exit);
        b.ret(&[acc, last_ptr]);

        let input = b.finish_checked().expect("valid niche ptr_addr/select input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        assert_eq!(count_op(v, Op::Br), 1, "expected 1 loop branch:\n{}", print(v));
        assert_eq!(count_op(v, Op::Select), 0, "expected 0 selects:\n{}", print(v));
        assert_eq!(count_op(v, Op::PtrAddr), 0, "expected 0 ptr_addrs:\n{}", print(v));
        assert_eq!(v.blocks(), 4, "expected 4 blocks (entry, header, body, exit):\n{}", print(v));

        let p0 = Val::Ptr { slot: Val::NO_SLOT, offset: 0x1000 };
        let p_null = Val::Ptr { slot: Val::NO_SLOT, offset: 0 };
        for n_val in [0u128, 1, 2, 5, 10] {
            let expected_sum = n_val * n_val.saturating_sub(1) / 2;
            let expected_ptr = if n_val == 0 { p_null } else { p0 };
            let args = [p0, Val::Int(n_val)];
            let before = run_in(input.view(), &args, FUEL, alloc).expect("eval input");
            let after = run_in(v, &args, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for n={n_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(expected_sum), expected_ptr]),
                "unexpected result for n={n_val}: {after:?}"
            );
        }
    }

    /// Nested 2-layer `Switch` chain with cross-block payload uses:
    /// `B0(tag1, pay1, next_iv) -> {B_some1, B_none1} -> B1(tag2, pay2) -> {Exit, Body(uses pay1 & pay2)}`,
    /// including a `Switch` default arm passing `tag2` to `Trap2`.
    /// Verifies `thread_jumps` collapses BOTH `Switch`es completely in one call via bottom-up threading.
    #[test]
    fn threads_nested_double_switch_with_cross_block_payload() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let [entry, header, p_some, p_none, b0, trap1, b_some1, b_none1, b1, trap2, body, exit] =
            [(); 12].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let has_next = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, has_next, (p_some, &[]), (p_none, &[]));

        b.begin(p_some);
        let i_next = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((b0, &[one, i, i_next]));

        b.begin(p_none);
        b.jump((b0, &[zero, zero, i]));

        b.begin(b0);
        let tag1 = b.param(Ty::I64);
        let pay1 = b.param(Ty::I64);
        let next_iv = b.param(Ty::I64);
        b.switch(tag1, (trap1, &[]), &[(0, (b_none1, &[])), (1, (b_some1, &[]))]);

        b.begin(trap1);
        b.trap(1);

        b.begin(b_some1);
        let ten = b.iconst(Ty::I64, 10);
        let pay1_scaled = b.binary(Op::Add, Flags::NONE, pay1, ten);
        b.jump((b1, &[one, pay1_scaled]));

        b.begin(b_none1);
        b.jump((b1, &[zero, acc]));

        b.begin(b1);
        let tag2 = b.param(Ty::I64);
        let pay2 = b.param(Ty::I64);
        b.switch(tag2, (trap2, &[tag2]), &[(0, (exit, &[])), (1, (body, &[]))]);

        b.begin(trap2);
        let _bad_tag = b.param(Ty::I64);
        b.trap(2);

        b.begin(body);
        // Uses BOTH `pay1` (param of `b0`) and `pay2` (param of `b1`), plus `next_iv` (param of `b0`).
        let both = b.binary(Op::Add, Flags::NONE, pay1, pay2);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, both);
        b.jump((header, &[next_iv, acc_next]));

        b.begin(exit);
        b.ret(&[acc]);

        let input = b.finish_checked().expect("valid nested double switch input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        assert_eq!(count_op(v, Op::Switch), 0, "expected BOTH switches collapsed:\n{}", print(v));
        assert_eq!(count_op(v, Op::Trap), 0, "expected BOTH traps eliminated:\n{}", print(v));
        assert_eq!(count_op(v, Op::Br), 1, "expected 1 loop branch:\n{}", print(v));
        assert_eq!(v.blocks(), 4, "expected 4 blocks (entry, header, body, exit):\n{}", print(v));

        for n_val in [0u128, 1, 2, 5, 10] {
            let expected = n_val * (n_val + 9);
            let before = run_in(input.view(), &[Val::Int(n_val)], FUEL, alloc).expect("eval input");
            let after = run_in(v, &[Val::Int(n_val)], FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for n={n_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(expected)]),
                "unexpected result for n={n_val}: {after:?}"
            );
        }
    }

    /// Exact live `for &x in slice` loop pattern with `NonNull` double niche checks + trap:
    /// `addr = ptr_addr(p); c0 = icmp ule addr, 0; tag = select c0, addr, 1;
    ///  br icmp(eq, tag, 0), exit, chk1; chk1: br icmp(ne, tag, 1), trap, body2`.
    /// Verifies BOTH `PtrAdd[INBOUNDS]` and loop-bounded `PtrAdd(Flags::NONE)` eliminate both
    /// niche branches, the trap block, `Select`, and `PtrAddr`, collapsing to a 4-block loop with
    /// a single exit (`icmp ne p, end`), plus evaluator equivalence via `run_in`.
    #[test]
    fn folds_slice_iter_nonnull_double_niche_branch_and_trap() {
        let alloc = blaze_own::testing::heap().alloc();

        for step_flags in [Flags::INBOUNDS, Flags::NONE] {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let mut ptr_param = Param::of(Ty::PTR);
            ptr_param.facts = Facts { bits: ParamFactsRec::NONNULL, ..Facts::default() };
            let sig = b.sig(Conv::C, Variadic::No, &[ptr_param, Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);

            let [entry, hdr, body, chk1, trap_blk, body2, exit] =
                [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let x0 = b.param(Ty::PTR);
            let x1 = b.param(Ty::I64);
            let sh3 = b.iconst(Ty::I64, 3);
            let byte_len = b.binary(Op::Shl, Flags::NONE, x1, sh3);
            let end = b.ptr_add(Flags::INBOUNDS, x0, byte_len);
            let zero = b.iconst(Ty::I64, 0);
            b.jump((hdr, &[x0, zero]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(Ty::I64);
            let has_next = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, has_next, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let eight = b.iconst(Ty::I64, 8);
            let p_next = b.ptr_add(step_flags, p, eight);
            let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p);
            let z0 = b.iconst(Ty::I64, 0);
            let c0 = b.icmp(IntCond::Ule, addr, z0);
            let c1 = b.iconst(Ty::I64, 1);
            let tag = b.select(Flags::NONE, c0, addr, c1);
            let is_none = b.icmp(IntCond::Eq, tag, z0);
            b.br(Flags::NONE, is_none, (exit, &[acc]), (chk1, &[]));

            b.begin(chk1);
            let not_one = b.icmp(IntCond::Ne, tag, c1);
            b.br(Flags::NONE, not_one, (trap_blk, &[]), (body2, &[]));

            b.begin(trap_blk);
            b.trap(1);

            b.begin(body2);
            let elem = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, elem);
            b.jump((hdr, &[p_next, acc_next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let input = b.finish_checked().expect("valid slice iter niche branch input");
            let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
            verify(out.view()).expect("output verifies");

            let v = out.view();
            assert_eq!(count_op(v, Op::Trap), 0, "expected 0 traps ({step_flags:?}):\n{}", print(v));
            assert_eq!(count_op(v, Op::Select), 0, "expected 0 selects ({step_flags:?}):\n{}", print(v));
            assert_eq!(count_op(v, Op::PtrAddr), 0, "expected 0 ptr_addrs ({step_flags:?}):\n{}", print(v));
            assert_eq!(count_op(v, Op::Br), 1, "expected single loop exit branch ({step_flags:?}):\n{}", print(v));
            assert_eq!(v.blocks(), 4, "expected 4 blocks (entry, hdr, body, exit) ({step_flags:?}):\n{}", print(v));
        }

        // Runtime-evaluated equivalence check on the same double-niche-branch + trap pattern.
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let mut ptr_param = Param::of(Ty::PTR);
        ptr_param.facts = Facts { bits: ParamFactsRec::NONNULL, ..Facts::default() };
        let sig = b.sig(Conv::C, Variadic::No, &[ptr_param, Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let [entry, hdr, body, chk1, trap_blk, body2, exit] =
            [(); 7].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let x0 = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[x0, zero, zero]));

        b.begin(hdr);
        let p = b.param(Ty::PTR);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let has_next = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, has_next, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let z0 = b.iconst(Ty::I64, 0);
        let p_next = b.ptr_add(Flags::INBOUNDS, p, z0);
        let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p);
        let c0 = b.icmp(IntCond::Ule, addr, z0);
        let c1 = b.iconst(Ty::I64, 1);
        let tag = b.select(Flags::NONE, c0, addr, c1);
        let is_none = b.icmp(IntCond::Eq, tag, z0);
        b.br(Flags::NONE, is_none, (exit, &[acc]), (chk1, &[]));

        b.begin(chk1);
        let not_one = b.icmp(IntCond::Ne, tag, c1);
        b.br(Flags::NONE, not_one, (trap_blk, &[]), (body2, &[]));

        b.begin(trap_blk);
        b.trap(1);

        b.begin(body2);
        let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, i);
        b.jump((hdr, &[p_next, i_next, acc_next]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let input = b.finish_checked().expect("valid eval double-niche input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        assert_eq!(count_op(v, Op::Trap), 0, "expected 0 traps:\n{}", print(v));
        assert_eq!(count_op(v, Op::Select), 0, "expected 0 selects:\n{}", print(v));
        assert_eq!(count_op(v, Op::PtrAddr), 0, "expected 0 ptr_addrs:\n{}", print(v));
        assert_eq!(count_op(v, Op::Br), 1, "expected 1 loop branch:\n{}", print(v));
        assert_eq!(v.blocks(), 4, "expected 4 blocks:\n{}", print(v));

        let p0 = Val::Ptr { slot: Val::NO_SLOT, offset: 0x1000 };
        for n_val in [0u128, 1, 2, 5, 10] {
            let expected = n_val * n_val.saturating_sub(1) / 2;
            let args = [p0, Val::Int(n_val)];
            let before = run_in(input.view(), &args, FUEL, alloc).expect("eval input");
            let after = run_in(v, &args, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for n={n_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(expected)]),
                "unexpected result for n={n_val}: {after:?}"
            );
        }
    }

    /// Tests `Op::Switch` on the `NonNull` niche tag (`select (icmp ule ptr_addr(p), 0), addr, 1`),
    /// verifying direct switch folding removes the switch, trap block, `Select`, and `PtrAddr`
    /// while preserving exact runtime semantics.
    #[test]
    fn folds_slice_iter_nonnull_switch_niche_tag() {
        let alloc = blaze_own::testing::heap().alloc();

        // Real slice::Iter loop with `PtrAdd[INBOUNDS](p, 8)` + `Load` + `Switch` on niche tag.
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let mut ptr_param = Param::of(Ty::PTR);
            ptr_param.facts = Facts { bits: ParamFactsRec::NONNULL, ..Facts::default() };
            let sig = b.sig(Conv::C, Variadic::No, &[ptr_param, Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);

            let [entry, hdr, body, trap_blk, body2, exit] =
                [(); 6].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let x0 = b.param(Ty::PTR);
            let x1 = b.param(Ty::I64);
            let sh3 = b.iconst(Ty::I64, 3);
            let byte_len = b.binary(Op::Shl, Flags::NONE, x1, sh3);
            let end = b.ptr_add(Flags::INBOUNDS, x0, byte_len);
            let zero = b.iconst(Ty::I64, 0);
            b.jump((hdr, &[x0, zero]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(Ty::I64);
            let has_next = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, has_next, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let eight = b.iconst(Ty::I64, 8);
            let p_next = b.ptr_add(Flags::INBOUNDS, p, eight);
            let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p);
            let z0 = b.iconst(Ty::I64, 0);
            let c0 = b.icmp(IntCond::Ule, addr, z0);
            let c1 = b.iconst(Ty::I64, 1);
            let tag = b.select(Flags::NONE, c0, addr, c1);
            b.switch(tag, (trap_blk, &[]), &[(0, (exit, &[acc])), (1, (body2, &[]))]);

            b.begin(trap_blk);
            b.trap(2);

            b.begin(body2);
            let elem = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, elem);
            b.jump((hdr, &[p_next, acc_next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let input = b.finish_checked().expect("valid slice iter switch input");
            let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
            verify(out.view()).expect("output verifies");

            let v = out.view();
            assert_eq!(count_op(v, Op::Switch), 0, "expected 0 switches:\n{}", print(v));
            assert_eq!(count_op(v, Op::Trap), 0, "expected 0 traps:\n{}", print(v));
            assert_eq!(count_op(v, Op::Select), 0, "expected 0 selects:\n{}", print(v));
            assert_eq!(count_op(v, Op::PtrAddr), 0, "expected 0 ptr_addrs:\n{}", print(v));
            assert_eq!(count_op(v, Op::Br), 1, "expected 1 loop branch:\n{}", print(v));
            assert_eq!(v.blocks(), 4, "expected 4 blocks:\n{}", print(v));
        }

        // Runtime-evaluated equivalence check for `Op::Switch` on niche tag.
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let mut ptr_param = Param::of(Ty::PTR);
        ptr_param.facts = Facts { bits: ParamFactsRec::NONNULL, ..Facts::default() };
        let sig = b.sig(Conv::C, Variadic::No, &[ptr_param, Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);

        let [entry, hdr, body, trap_blk, body2, exit] =
            [(); 6].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let x0 = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[x0, zero, zero]));

        b.begin(hdr);
        let p = b.param(Ty::PTR);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let has_next = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, has_next, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let z0 = b.iconst(Ty::I64, 0);
        let p_next = b.ptr_add(Flags::INBOUNDS, p, z0);
        let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p);
        let c0 = b.icmp(IntCond::Ule, addr, z0);
        let c1 = b.iconst(Ty::I64, 1);
        let tag = b.select(Flags::NONE, c0, addr, c1);
        b.switch(tag, (trap_blk, &[]), &[(0, (exit, &[acc])), (1, (body2, &[]))]);

        b.begin(trap_blk);
        b.trap(2);

        b.begin(body2);
        let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, i);
        b.jump((hdr, &[p_next, i_next, acc_next]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let input = b.finish_checked().expect("valid eval switch niche input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        assert_eq!(count_op(v, Op::Switch), 0, "expected 0 switches:\n{}", print(v));
        assert_eq!(count_op(v, Op::Trap), 0, "expected 0 traps:\n{}", print(v));
        assert_eq!(count_op(v, Op::Select), 0, "expected 0 selects:\n{}", print(v));
        assert_eq!(count_op(v, Op::PtrAddr), 0, "expected 0 ptr_addrs:\n{}", print(v));
        assert_eq!(count_op(v, Op::Br), 1, "expected 1 loop branch:\n{}", print(v));
        assert_eq!(v.blocks(), 4, "expected 4 blocks:\n{}", print(v));

        let p0 = Val::Ptr { slot: Val::NO_SLOT, offset: 0x1000 };
        for n_val in [0u128, 1, 2, 5, 10] {
            let expected = n_val * n_val.saturating_sub(1) / 2;
            let args = [p0, Val::Int(n_val)];
            let before = run_in(input.view(), &args, FUEL, alloc).expect("eval input");
            let after = run_in(v, &args, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for n={n_val}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(expected)]),
                "unexpected result for n={n_val}: {after:?}"
            );
        }
    }

    /// Soundness controls:
    /// 1. When the entry pointer does NOT carry `ParamFactsRec::NONNULL`, the `tag == 0` exit
    ///    branch MUST be preserved (and runtime behavior on `null` and wrap-to-zero matches
    ///    identically), while the impossible `tag != 1 -> trap` arm still folds cleanly because
    ///    `select (icmp ule addr, 0), addr, 1` is provably in `{0, 1}`.
    /// 2. When `end = ptr_add inbounds x0, -8` has a negative offset and `p_next = ptr_add p, 8`
    ///    lacks `INBOUNDS`, loop-bounded non-null propagation MUST refuse to assume `p_next` is
    ///    non-null so `tag == 0` is preserved.
    #[test]
    fn preserves_niche_zero_branch_for_nullable_raw_ptr() {
        let alloc = blaze_own::testing::heap().alloc();

        // Control 1: nullable raw pointer (no `ParamFactsRec::NONNULL`).
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);

            let [entry, hdr, body, chk1, trap_blk, body2, exit] =
                [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let x0 = b.param(Ty::PTR);
            let x1 = b.param(Ty::I64);
            let sh3 = b.iconst(Ty::I64, 3);
            let byte_len = b.binary(Op::Shl, Flags::NONE, x1, sh3);
            let end = b.ptr_add(Flags::NONE, x0, byte_len);
            let zero = b.iconst(Ty::I64, 0);
            b.jump((hdr, &[x0, zero]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(Ty::I64);
            let has_next = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, has_next, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let eight = b.iconst(Ty::I64, 8);
            let p_next = b.ptr_add(Flags::NONE, p, eight);
            let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p);
            let z0 = b.iconst(Ty::I64, 0);
            let c0 = b.icmp(IntCond::Ule, addr, z0);
            let c1 = b.iconst(Ty::I64, 1);
            let tag = b.select(Flags::NONE, c0, addr, c1);
            let is_none = b.icmp(IntCond::Eq, tag, z0);
            b.br(Flags::NONE, is_none, (exit, &[acc]), (chk1, &[]));

            b.begin(chk1);
            let not_one = b.icmp(IntCond::Ne, tag, c1);
            b.br(Flags::NONE, not_one, (trap_blk, &[]), (body2, &[]));

            b.begin(trap_blk);
            b.trap(1);

            b.begin(body2);
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, c1);
            b.jump((hdr, &[p_next, acc_next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let input = b.finish_checked().expect("valid nullable raw ptr input");
            let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
            verify(out.view()).expect("output verifies");

            let v = out.view();
            assert_eq!(
                count_op(v, Op::Br),
                2,
                "nullable ptr MUST keep `tag == 0` exit branch:\n{}",
                print(v)
            );
            assert_eq!(
                count_op(v, Op::Trap),
                0,
                "impossible `tag != 1 -> trap` arm should still fold via two-valued select:\n{}",
                print(v)
            );

            for (offset, n_val, expected) in [
                (0i128, 4u128, 0u128),      // null entry exits immediately with 0
                (0x1000, 0, 0),             // empty range exits at header
                (0x1000, 4, 4),             // normal non-null slice counts all 4
                (-8, 4, 1),                 // wraps through 0 on iteration 2 -> exits with 1
            ] {
                let args = [Val::Ptr { slot: Val::NO_SLOT, offset }, Val::Int(n_val)];
                let before = run_in(input.view(), &args, FUEL, alloc).expect("eval input");
                let after = run_in(v, &args, FUEL, alloc).expect("eval output");
                assert_eq!(before, after, "semantics diverged for offset={offset}, n={n_val}");
                assert!(
                    matches!(after, Outcome::Returned(ref vals) if vals[..] == [Val::Int(expected)]),
                    "unexpected result for offset={offset}, n={n_val}: {after:?}"
                );
            }
        }

        // Control 2: negative `end` offset with non-INBOUNDS `p_next` MUST NOT assume non-null.
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let mut ptr_param = Param::of(Ty::PTR);
            ptr_param.facts = Facts { bits: ParamFactsRec::NONNULL, ..Facts::default() };
            let sig = b.sig(Conv::C, Variadic::No, &[ptr_param], &[Param::of(Ty::I64)]);
            b.set_signature(sig);

            let [entry, hdr, body, chk1, trap_blk, body2, exit] =
                [(); 7].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let x0 = b.param(Ty::PTR);
            let neg8 = b.iconst(Ty::I64, (-8i64) as u64 as u128);
            let end = b.ptr_add(Flags::INBOUNDS, x0, neg8);
            let zero = b.iconst(Ty::I64, 0);
            b.jump((hdr, &[x0, zero]));

            b.begin(hdr);
            let p = b.param(Ty::PTR);
            let acc = b.param(Ty::I64);
            let has_next = b.icmp(IntCond::Ne, p, end);
            b.br(Flags::NONE, has_next, (body, &[]), (exit, &[acc]));

            b.begin(body);
            let eight = b.iconst(Ty::I64, 8);
            let p_next = b.ptr_add(Flags::NONE, p, eight);
            let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, p);
            let z0 = b.iconst(Ty::I64, 0);
            let c0 = b.icmp(IntCond::Ule, addr, z0);
            let c1 = b.iconst(Ty::I64, 1);
            let tag = b.select(Flags::NONE, c0, addr, c1);
            let is_none = b.icmp(IntCond::Eq, tag, z0);
            b.br(Flags::NONE, is_none, (exit, &[acc]), (chk1, &[]));

            b.begin(chk1);
            let not_one = b.icmp(IntCond::Ne, tag, c1);
            b.br(Flags::NONE, not_one, (trap_blk, &[]), (body2, &[]));

            b.begin(trap_blk);
            b.trap(1);

            b.begin(body2);
            let acc_next = b.binary(Op::Add, Flags::NONE, acc, c1);
            b.jump((hdr, &[p_next, acc_next]));

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let input = b.finish_checked().expect("valid negative-end control input");
            let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
            verify(out.view()).expect("output verifies");

            let v = out.view();
            assert_eq!(
                count_op(v, Op::Br),
                2,
                "negative end offset with flagless ptr_add MUST keep `tag == 0` branch:\n{}",
                print(v)
            );
            assert_eq!(count_op(v, Op::Trap), 0, "trap still folds via two-valued select:\n{}", print(v));
        }
    }

    /// Exact `Option<u64>` merge diamond inside a loop (mirroring `decode_exec_leaf_single` +
    /// `RevBitSlack8::reload_window_slack8`):
    /// - `p_some -> merge(1, win, next_cur, rem_bits)`
    /// - `p_none -> merge(0, 0, cur, bits)`
    /// - `merge(tag, win, m_cur, m_bits)`: `is_some = icmp eq tag, 1; br is_some, body, loop_exit(acc, m_bits)`
    /// - `body` uses `win, m_cur, m_bits` directly and exits to `loop_exit` or loops to `header`
    /// - `loop_exit` (multi-predecessor post-dominator of `merge`) uses `m_cur` directly via SSA dominance.
    /// Verifies `merge` and its tag branch are completely eliminated, `p_some` merges into `body`,
    /// `verify` passes, and `run_in` matches bit-for-bit across immediate-None, full-Some, and
    /// mid-loop-None trajectories.
    #[test]
    fn threads_option_merge_diamond_with_post_merge_exit_uses() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let [entry, header, chk_bits, p_some, p_none, merge, body, loop_exit] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let init_cur = b.param(Ty::I64);
        let limit = b.param(Ty::I64);
        let init_bits = b.param(Ty::I64);
        let rem0 = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        let one = b.iconst(Ty::I64, 1);
        let c3 = b.iconst(Ty::I64, 3);
        let c7 = b.iconst(Ty::I64, 7);
        let c10 = b.iconst(Ty::I64, 10);
        let c64 = b.iconst(Ty::I64, 64);
        b.jump((header, &[init_cur, init_bits, zero, rem0]));

        b.begin(header);
        let cur = b.param(Ty::I64);
        let bits = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let rem = b.param(Ty::I64);
        let low = b.icmp(IntCond::Ult, cur, limit);
        b.br(Flags::NONE, low, (p_none, &[]), (chk_bits, &[]));

        b.begin(chk_bits);
        let bad_bits = b.icmp(IntCond::Uge, bits, c64);
        b.br(Flags::NONE, bad_bits, (p_none, &[]), (p_some, &[]));

        b.begin(p_some);
        let adv = b.binary(Op::Lshr, Flags::NONE, bits, c3);
        let next_cur = b.binary(Op::Sub, Flags::NONE, cur, adv);
        let rem_bits = b.binary(Op::And, Flags::NONE, bits, c7);
        let win = b.binary(Op::Add, Flags::NONE, next_cur, rem_bits);
        b.jump((merge, &[one, win, next_cur, rem_bits]));

        b.begin(p_none);
        b.jump((merge, &[zero, zero, cur, bits]));

        b.begin(merge);
        let tag = b.param(Ty::I64);
        let m_win = b.param(Ty::I64);
        let m_cur = b.param(Ty::I64);
        let m_bits = b.param(Ty::I64);
        let is_some = b.icmp(IntCond::Eq, tag, one);
        b.br(Flags::NONE, is_some, (body, &[]), (loop_exit, &[acc, m_bits]));

        b.begin(body);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, m_win);
        let bits_after = b.binary(Op::Add, Flags::NONE, m_bits, c10);
        let rem_next = b.binary(Op::Sub, Flags::NONE, rem, one);
        let done = b.icmp(IntCond::Eq, rem_next, zero);
        b.br(
            Flags::NONE,
            done,
            (loop_exit, &[acc_next, bits_after]),
            (header, &[m_cur, bits_after, acc_next, rem_next]),
        );

        b.begin(loop_exit);
        let out_acc = b.param(Ty::I64);
        let out_bits = b.param(Ty::I64);
        // Uses `m_cur` directly via SSA dominance from `merge` across BOTH `merge -> loop_exit`
        // and `body -> loop_exit`.
        b.ret(&[out_acc, m_cur, out_bits]);

        let input = b.finish_checked().expect("valid option merge diamond input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        assert_eq!(
            count_op(v, Op::Br),
            3,
            "Option tag branch in merge block must be completely threaded away:\n{}",
            print(v)
        );
        assert_eq!(
            v.blocks(),
            5,
            "expected 5 blocks (entry, header, chk_bits, p_some+body, loop_exit):\n{}",
            print(v)
        );

        for (args, expected) in [
            ([5u128, 10, 0, 4], [0u128, 5, 0]),
            ([100, 10, 64, 4], [0, 100, 64]),
            ([100, 10, 8, 3], [300, 97, 14]),
            ([11, 10, 16, 5], [9, 9, 10]),
        ] {
            let vals = args.map(Val::Int);
            let before = run_in(input.view(), &vals, FUEL, alloc).expect("eval input");
            let after = run_in(v, &vals, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for {args:?}");
            assert!(
                matches!(after, Outcome::Returned(ref got) if got[..] == expected.map(Val::Int)),
                "unexpected result for {args:?}: {after:?}"
            );
        }
    }

    /// Tests a merge diamond whose `state` param is used in blocks strictly dominated by a
    /// post-merge block reachable from BOTH `arm_a` and `arm_b`, while `merge` also passes a
    /// local `iconst 0` as an edge argument to `arm_b`.
    #[test]
    fn threads_merge_with_shared_post_merge_and_hoisted_const() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let [entry, p_some, p_none, merge, arm_a, arm_b, post_merge, sub_a, sub_b] =
            [(); 9].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let cond = b.param(Ty::I64);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let c = b.icmp(IntCond::Ne, cond, z);
        b.br(Flags::NONE, c, (p_some, &[]), (p_none, &[]));

        b.begin(p_some);
        let one = b.iconst(Ty::I64, 1);
        let sx = b.binary(Op::Add, Flags::NONE, x, one);
        b.jump((merge, &[one, x, sx]));

        b.begin(p_none);
        let z0 = b.iconst(Ty::I64, 0);
        let sy = b.binary(Op::Add, Flags::NONE, y, y);
        b.jump((merge, &[z0, z0, sy]));

        b.begin(merge);
        let tag = b.param(Ty::I64);
        let pay = b.param(Ty::I64);
        let state = b.param(Ty::I64);
        let m_zero = b.iconst(Ty::I64, 0);
        let is_zero = b.icmp(IntCond::Eq, tag, m_zero);
        b.br(Flags::NONE, is_zero, (arm_b, &[m_zero]), (arm_a, &[]));

        b.begin(arm_a);
        let ten = b.iconst(Ty::I64, 10);
        let va = b.binary(Op::Add, Flags::NONE, pay, ten);
        b.jump((post_merge, &[va]));

        b.begin(arm_b);
        let b0 = b.param(Ty::I64);
        let twenty = b.iconst(Ty::I64, 20);
        let vb = b.binary(Op::Add, Flags::NONE, b0, twenty);
        b.jump((post_merge, &[vb]));

        b.begin(post_merge);
        let pv = b.param(Ty::I64);
        let nz = b.icmp(IntCond::Ne, pv, z);
        b.br(Flags::NONE, nz, (sub_a, &[]), (sub_b, &[]));

        b.begin(sub_a);
        let ra = b.binary(Op::Add, Flags::NONE, pv, state);
        b.ret(&[ra]);

        b.begin(sub_b);
        b.ret(&[state]);

        let input = b.finish_checked().expect("valid shared post-merge input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        assert_eq!(
            count_op(v, Op::Br),
            2,
            "merge branch must be threaded away leaving entry and post_merge branches:\n{}",
            print(v)
        );

        for (args, expected) in [([1u128, 5, 7], 21u128), ([0, 5, 7], 34)] {
            let vals = args.map(Val::Int);
            let before = run_in(input.view(), &vals, FUEL, alloc).expect("eval input");
            let after = run_in(v, &vals, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for {args:?}");
            assert!(
                matches!(after, Outcome::Returned(ref got) if got[..] == [Val::Int(expected)]),
                "unexpected result for {args:?}: {after:?}"
            );
        }
    }

    /// Partial threading with a live `merge` block (`p_some` and `p_none` thread while `p_dyn`
    /// remains on `merge`) and a shared `post_merge` block using `merge`'s `state` param directly.
    #[test]
    fn partial_threading_with_post_merge_preserves_semantics() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let [entry, p_some, p_none, p_dyn, merge, arm_a, arm_b, post_merge] =
            [(); 8].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let sel = b.param(Ty::I64);
        let dyn_tag = b.param(Ty::I64);
        let x = b.param(Ty::I64);
        b.switch(sel, (p_dyn, &[]), &[(0, (p_none, &[])), (1, (p_some, &[]))]);

        b.begin(p_some);
        let c1 = b.iconst(Ty::I64, 1);
        let s1 = b.binary(Op::Add, Flags::NONE, x, c1);
        b.jump((merge, &[c1, s1]));

        b.begin(p_none);
        let c0 = b.iconst(Ty::I64, 0);
        let c2 = b.iconst(Ty::I64, 2);
        let s0 = b.binary(Op::Add, Flags::NONE, x, c2);
        b.jump((merge, &[c0, s0]));

        b.begin(p_dyn);
        let c3 = b.iconst(Ty::I64, 3);
        let sd = b.binary(Op::Add, Flags::NONE, x, c3);
        b.jump((merge, &[dyn_tag, sd]));

        b.begin(merge);
        let tag = b.param(Ty::I64);
        let state = b.param(Ty::I64);
        let z0 = b.iconst(Ty::I64, 0);
        let is_zero = b.icmp(IntCond::Eq, tag, z0);
        b.br(Flags::NONE, is_zero, (arm_b, &[]), (arm_a, &[]));

        b.begin(arm_a);
        let c10 = b.iconst(Ty::I64, 10);
        b.jump((post_merge, &[c10]));

        b.begin(arm_b);
        let c20 = b.iconst(Ty::I64, 20);
        b.jump((post_merge, &[c20]));

        b.begin(post_merge);
        let base = b.param(Ty::I64);
        let res = b.binary(Op::Add, Flags::NONE, base, state);
        b.ret(&[res]);

        let input = b.finish_checked().expect("valid partial threading input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        for (args, expected) in [
            ([1u128, 0, 100], 111u128),
            ([0, 0, 100], 122),
            ([2, 0, 100], 123),
            ([2, 1, 100], 113),
        ] {
            let vals = args.map(Val::Int);
            let before = run_in(input.view(), &vals, FUEL, alloc).expect("eval input");
            let after = run_in(v, &vals, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for {args:?}");
            assert!(
                matches!(after, Outcome::Returned(ref got) if got[..] == [Val::Int(expected)]),
                "unexpected result for {args:?}: {after:?}"
            );
        }
    }

    /// Loop-header bypass control: `entry -> merge(dyn_tag, 0, 0)` stays unthreaded while the
    /// backedge `back -> merge(1, i_next, acc_next)` threads straight to `body`. Even though
    /// `merge` STILL dominates `body` in `G'`, `body` MUST receive promoted parameters for `i`
    /// and `acc` so iterations `>= 2` read the updated backedge values instead of `0`.
    #[test]
    fn loop_header_self_bypass_threads_soundly() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let [entry, merge, body, back, exit] = [(); 5].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let start_tag = b.param(Ty::I64);
        let n = b.param(Ty::I64);
        let z0 = b.iconst(Ty::I64, 0);
        b.jump((merge, &[start_tag, z0, z0]));

        b.begin(merge);
        let tag = b.param(Ty::I64);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        let go = b.icmp(IntCond::Ne, tag, z);
        b.br(Flags::NONE, go, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let c1 = b.iconst(Ty::I64, 1);
        let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
        let acc_next = b.binary(Op::Add, Flags::NONE, acc, i_next);
        let done = b.icmp(IntCond::Uge, i_next, n);
        b.br(Flags::NONE, done, (exit, &[acc_next]), (back, &[i_next, acc_next]));

        b.begin(back);
        let bi = b.param(Ty::I64);
        let bacc = b.param(Ty::I64);
        let one = b.iconst(Ty::I64, 1);
        b.jump((merge, &[one, bi, bacc]));

        b.begin(exit);
        let out_acc = b.param(Ty::I64);
        b.ret(&[out_acc]);

        let input = b.finish_checked().expect("valid loop header bypass input");
        let out = thread_jumps(input.view(), alloc).expect("thread_jumps");
        verify(out.view()).expect("output verifies");

        let v = out.view();
        for (args, expected) in [([0u128, 5], 0u128), ([1, 1], 1), ([1, 5], 15)] {
            let vals = args.map(Val::Int);
            let before = run_in(input.view(), &vals, FUEL, alloc).expect("eval input");
            let after = run_in(v, &vals, FUEL, alloc).expect("eval output");
            assert_eq!(before, after, "semantics diverged for {args:?}");
            assert!(
                matches!(after, Outcome::Returned(ref got) if got[..] == [Val::Int(expected)]),
                "unexpected result for {args:?}: {after:?}"
            );
        }
    }
}
