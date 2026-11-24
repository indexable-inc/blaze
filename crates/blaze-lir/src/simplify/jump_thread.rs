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
//!   edge straight to the resolved successor `S`, promoting any externally used parameter of `M`
//!   into `S` when `M` is `S`'s unique reachable predecessor.
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
    dce::{Edge, Item, NONE, coalesce_cfg, each, filled},
};
use crate::{
    Block, Flags, Func, IntCond, Op, Ty, View,
    arith::{binary, compare, convert, iconst_value, unary, wide_value},
    func::{MemAttrsRec, ParamFactsRec, SigId},
    op::{Field, Layout},
    verify,
};

/// Maximum threading + coalescing fixpoint rounds per call.
const MAX_ROUNDS: usize = 16;

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

/// Computes sound global static facts (integer constants, `null`, and provably non-null pointers,
/// including loop-inductive `ptr_add inbounds` pointers from `slice::Iter::next`).
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
        for &b in cfg.rpo.iter() {
            let params = view.params(Block(b));
            if b != 0 && at(&cfg.plain_only, to_idx(b), false) {
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

            let range = view.block(Block(b));
            for inst in params.end..range.end {
                let op = view.op(inst);
                let ty = Ty(at(view.tys, inst, 0));
                let next_val = match op {
                    Op::Iconst | Op::IconstWide => match unsigned_const(view, inst) {
                        Some(v) => EvalVal::Int(v),
                        None => EvalVal::Unknown,
                    },
                    Op::Null => EvalVal::PtrNull,
                    Op::SlotAddr | Op::DataAddr | Op::FuncAddr | Op::TlsAddr => EvalVal::PtrNonNull,
                    Op::Load if ty == Ty::PTR => match view.mem_attrs(inst) {
                        Some(m) if (m.bits & MemAttrsRec::NONNULL) != 0 => EvalVal::PtrNonNull,
                        _ => EvalVal::Unknown,
                    },
                    Op::Call if ty == Ty::PTR => {
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
                    Op::CallIndirect if ty == Ty::PTR => {
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
                    Op::PtrAdd if ty == Ty::PTR => {
                        let mut o = view.operands(inst);
                        let base = to_idx(o.value().0);
                        let by = to_idx(o.value().0);
                        let flags = view.flags(inst);
                        let base_val = at(&vals, base, EvalVal::Unknown);
                        if base_val == EvalVal::PtrNonNull
                            && (flags.contains(Flags::INBOUNDS)
                                || flags.contains(Flags::NUW)
                                || signed_const(view, by) == Some(0))
                        {
                            EvalVal::PtrNonNull
                        } else if base < len && view.op(base) == Op::SlotAddr {
                            let slot_id = to_idx(at(view.data, base, 0));
                            let slot_size = view.slots.get(slot_id).map_or(0, |s| i128::from(s.size));
                            match signed_const(view, by) {
                                Some(off) if off >= 0 && off <= slot_size => EvalVal::PtrNonNull,
                                _ => EvalVal::Unknown,
                            }
                        } else if base_val == EvalVal::PtrNull && signed_const(view, by) == Some(0) {
                            EvalVal::PtrNull
                        } else {
                            EvalVal::Unknown
                        }
                    }
                    Op::FromExposed if ty == Ty::PTR => {
                        let src = to_idx(view.operands(inst).value().0);
                        match at(&vals, src, EvalVal::Unknown) {
                            EvalVal::Int(0) => EvalVal::PtrNull,
                            EvalVal::Int(_) | EvalVal::IntNonZero => EvalVal::PtrNonNull,
                            _ => EvalVal::Unknown,
                        }
                    }
                    Op::PtrWithAddr if ty == Ty::PTR => {
                        let mut o = view.operands(inst);
                        let _base = o.value();
                        let addr = to_idx(o.value().0);
                        match at(&vals, addr, EvalVal::Unknown) {
                            EvalVal::Int(0) => EvalVal::PtrNull,
                            EvalVal::Int(_) | EvalVal::IntNonZero => EvalVal::PtrNonNull,
                            _ => EvalVal::Unknown,
                        }
                    }
                    Op::Freeze => {
                        let src = to_idx(view.operands(inst).value().0);
                        at(&vals, src, EvalVal::Unknown)
                    }
                    Op::Select => {
                        let mut o = view.operands(inst);
                        let cond = to_idx(o.value().0);
                        let a = to_idx(o.value().0);
                        let b_v = to_idx(o.value().0);
                        match at(&vals, cond, EvalVal::Unknown) {
                            EvalVal::Int(c) => at(&vals, if c != 0 { a } else { b_v }, EvalVal::Unknown),
                            EvalVal::IntNonZero => at(&vals, a, EvalVal::Unknown),
                            _ => {
                                let va = at(&vals, a, EvalVal::Unknown);
                                let vb = at(&vals, b_v, EvalVal::Unknown);
                                if va == vb { va } else { EvalVal::Unknown }
                            }
                        }
                    }
                    Op::PtrAddr | Op::Expose => {
                        let src = to_idx(view.operands(inst).value().0);
                        match at(&vals, src, EvalVal::Unknown) {
                            EvalVal::PtrNull => EvalVal::Int(0),
                            EvalVal::PtrNonNull => EvalVal::IntNonZero,
                            _ => EvalVal::Unknown,
                        }
                    }
                    Op::Zext | Op::Sext | Op::Trunc if at(view.flags, inst, 0) == 0 => {
                        let src = to_idx(view.operands(inst).value().0);
                        let from_ty = Ty(at(view.tys, src, 0));
                        eval_int_convert(op, from_ty, ty, view.head.ptr_bits, at(&vals, src, EvalVal::Unknown))
                    }
                    _ => EvalVal::Unknown,
                };
                if next_val != at(&vals, inst, EvalVal::Unknown) {
                    set_at(&mut vals, inst, next_val);
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

/// Evaluates `M`'s pure body and terminator for one incoming edge `edge`, returning the resolved
/// successor target if `M`'s control flow is statically decided for `edge`.
fn eval_block_for_edge<A: Allocator>(
    view: &View<'_>,
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

    // Seed M's parameters from the incoming edge's arguments.
    let param_count = params.end.saturating_sub(params.start);
    if edge.args_len != param_count {
        return None;
    }
    for k in 0..param_count {
        let arg = at(view.extra, edge.args_start + k, NONE);
        set_at(local_buf, k, at(global_vals, to_idx(arg), EvalVal::Unknown));
    }

    let lookup = |v: u32, local: &[EvalVal]| -> EvalVal {
        let idx = to_idx(v);
        if idx >= range.start && idx < range.end {
            at(local, idx - range.start, EvalVal::Unknown)
        } else {
            at(global_vals, idx, EvalVal::Unknown)
        }
    };

    // Abstractly evaluate M's body instructions in order.
    for inst in params.end..term {
        let op = view.op(inst);
        let ty = Ty(at(view.tys, inst, 0));
        let flagless = at(view.flags, inst, 0) == 0;
        let val = match op {
            Op::Iconst | Op::IconstWide => match unsigned_const(view, inst) {
                Some(v) => EvalVal::Int(v),
                None => EvalVal::Unknown,
            },
            Op::Null => EvalVal::PtrNull,
            Op::SlotAddr | Op::DataAddr | Op::FuncAddr | Op::TlsAddr => EvalVal::PtrNonNull,
            Op::Freeze => lookup(view.operands(inst).value().0, local_buf),
            Op::PtrAddr => match lookup(view.operands(inst).value().0, local_buf) {
                EvalVal::PtrNull => EvalVal::Int(0),
                EvalVal::PtrNonNull => EvalVal::IntNonZero,
                EvalVal::Int(v) => EvalVal::Int(v),
                _ => EvalVal::Unknown,
            },
            Op::FromExposed if ty == Ty::PTR => match lookup(view.operands(inst).value().0, local_buf) {
                EvalVal::Int(0) => EvalVal::PtrNull,
                EvalVal::Int(_) | EvalVal::IntNonZero => EvalVal::PtrNonNull,
                _ => EvalVal::Unknown,
            },
            Op::PtrWithAddr if ty == Ty::PTR => {
                let mut o = view.operands(inst);
                let _base = o.value();
                match lookup(o.value().0, local_buf) {
                    EvalVal::Int(0) => EvalVal::PtrNull,
                    EvalVal::Int(_) | EvalVal::IntNonZero => EvalVal::PtrNonNull,
                    _ => EvalVal::Unknown,
                }
            }
            Op::PtrAdd if ty == Ty::PTR => {
                let mut o = view.operands(inst);
                let base_v = o.value().0;
                let by_v = o.value().0;
                let base = lookup(base_v, local_buf);
                let by = lookup(by_v, local_buf);
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
                eval_icmp(cond, op_ty, view.head.ptr_bits, lookup(a_v, local_buf), lookup(b_v, local_buf))
            }
            Op::Zext | Op::Sext | Op::Trunc if flagless => {
                let a_v = view.operands(inst).value().0;
                let from_ty = Ty(at(view.tys, to_idx(a_v), 0));
                eval_int_convert(op, from_ty, ty, view.head.ptr_bits, lookup(a_v, local_buf))
            }
            Op::Bitcast if flagless && ty.is_scalar() && ty.is_int() => {
                let a_v = view.operands(inst).value().0;
                let from_ty = Ty(at(view.tys, to_idx(a_v), 0));
                if from_ty.is_scalar() && from_ty.is_int() {
                    lookup(a_v, local_buf)
                } else {
                    EvalVal::Unknown
                }
            }
            Op::Neg | Op::Not if flagless && ty.is_scalar() && ty.is_int() => {
                let a_v = view.operands(inst).value().0;
                match lookup(a_v, local_buf) {
                    EvalVal::Int(a) => EvalVal::Int(unary(op, a, ty.bits(view.head.ptr_bits))),
                    _ => EvalVal::Unknown,
                }
            }
            Op::Or if ty.is_scalar() && ty.is_int() => {
                let mut o = view.operands(inst);
                let a_v = o.value().0;
                let b_v = o.value().0;
                match (lookup(a_v, local_buf), lookup(b_v, local_buf)) {
                    (EvalVal::Int(a), EvalVal::Int(b)) => EvalVal::Int(a | b),
                    (EvalVal::IntNonZero, _) | (_, EvalVal::IntNonZero) => EvalVal::IntNonZero,
                    (EvalVal::Int(v), _) | (_, EvalVal::Int(v)) if v != 0 => EvalVal::IntNonZero,
                    _ => EvalVal::Unknown,
                }
            }
            Op::And if flagless && ty.is_scalar() && ty.is_int() => {
                let mut o = view.operands(inst);
                let a_v = o.value().0;
                let b_v = o.value().0;
                match (lookup(a_v, local_buf), lookup(b_v, local_buf)) {
                    (EvalVal::Int(a), EvalVal::Int(b)) => EvalVal::Int(a & b),
                    (EvalVal::Int(0), _) | (_, EvalVal::Int(0)) => EvalVal::Int(0),
                    _ => EvalVal::Unknown,
                }
            }
            Op::Xor if flagless && ty.is_scalar() && ty.is_int() => {
                let mut o = view.operands(inst);
                let a_v = o.value().0;
                let b_v = o.value().0;
                match (lookup(a_v, local_buf), lookup(b_v, local_buf)) {
                    (EvalVal::Int(a), EvalVal::Int(b)) => EvalVal::Int(a ^ b),
                    (EvalVal::Int(0), x) | (x, EvalVal::Int(0)) => x,
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
                match (lookup(a_v, local_buf), lookup(b_v, local_buf)) {
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
                match lookup(c_v, local_buf) {
                    EvalVal::Int(c) => lookup(if c != 0 { a_v } else { b_v }, local_buf),
                    EvalVal::IntNonZero => lookup(a_v, local_buf),
                    _ => {
                        let la = lookup(a_v, local_buf);
                        let lb = lookup(b_v, local_buf);
                        if la == lb { la } else { EvalVal::Unknown }
                    }
                }
            }
            _ => EvalVal::Unknown,
        };
        set_at(local_buf, inst - range.start, val);
    }

    // Evaluate M's terminator.
    match term_op {
        Op::Br => {
            let cond = view.operands(term).value().0;
            let chosen = match lookup(cond, local_buf) {
                EvalVal::Int(v) => {
                    if v != 0 {
                        0u32
                    } else {
                        1u32
                    }
                }
                EvalVal::IntNonZero => 0u32,
                _ => return None,
            };
            target_by_ordinal(view, term, chosen)
        }
        Op::Switch => {
            let mut o = view.operands(term);
            let scrutinee = o.value();
            let width = Ty(at(view.tys, scrutinee.index(), 0)).bits(view.head.ptr_bits);
            let _default = o.target();
            let chosen = match lookup(scrutinee.0, local_buf) {
                EvalVal::Int(v) => {
                    let mut found = None;
                    for (idx, (words, _)) in o.cases(scrutinee).enumerate() {
                        if wide_value(words, width) == v {
                            found = Some(1 + to_u32(idx));
                            break;
                        }
                    }
                    found.unwrap_or(0)
                }
                EvalVal::IntNonZero if width == 1 => {
                    let mut found = None;
                    for (idx, (words, _)) in o.cases(scrutinee).enumerate() {
                        if wide_value(words, width) == 1 {
                            found = Some(1 + to_u32(idx));
                            break;
                        }
                    }
                    found.unwrap_or(0)
                }
                EvalVal::IntNonZero => {
                    let all_zero = o.cases(scrutinee).all(|(words, _)| wide_value(words, width) == 0);
                    if all_zero { 0 } else { return None }
                }
                _ => return None,
            };
            target_by_ordinal(view, term, chosen)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Per-block threading plan & round rewriter
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct ThreadedEdge {
    pred: u32,
    ordinal: u32,
    m_block: u32,
    target: TargetSlice,
}

/// One parameter of `m_block` promoted to a new parameter of successor `succ`.
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
}

/// Plans threading for candidate block `m_block`, checking pureness and SSA dominance safety.
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
    let mut threaded_targets: Vec<u32, A> = Vec::new_in(alloc);

    for &edge in in_edges {
        if edge.pred == m_block {
            continue;
        }
        let Some(target) = eval_block_for_edge(view, m_block, edge, global_vals, local_buf) else {
            continue;
        };
        if target.block == m_block {
            continue;
        }
        // Every value forwarded by `M -> S(s_args)` must be either a param of `M` or defined outside `M`.
        let mut args_ok = true;
        if let Some(s_args) = view.extra.get(target.args_start..target.args_start + target.args_len) {
            for &sv in s_args {
                let sv_idx = to_idx(sv);
                let is_m_param = sv_idx >= params.start && sv_idx < params.end;
                let outside_m = at(&cfg.owner, sv_idx, NONE) != m_block;
                if !is_m_param && !outside_m {
                    args_ok = false;
                    break;
                }
            }
        } else {
            args_ok = false;
        }
        if !args_ok {
            continue;
        }
        threaded.push(ThreadedEdge { pred: edge.pred, ordinal: edge.ordinal, m_block, target });
        if threaded_targets.scan_for(&target.block).is_none() {
            threaded_targets.push(target.block);
        }
    }

    if threaded.is_empty() {
        return None;
    }

    // Collect all unique successors of `M` that have `M` as their sole reachable predecessor and
    // can safely receive promoted parameters.
    let mut owned_succs: Vec<u32, A> = Vec::new_in(alloc);
    each(view, term, |item| {
        if let Item::Edge(Edge { block: s, args: Some(_), .. }) = item {
            if s != m_block
                && s != 0
                && at(&cfg.unique_pred, to_idx(s), NONE) == m_block
                && at(&cfg.plain_only, to_idx(s), false)
                && !view.block_kind(Block(s)).is_landing()
                && owned_succs.scan_for(&s).is_none()
            {
                owned_succs.push(s);
            }
        }
    });

    // Verify dominance safety for every instruction/param defined in `M` that has uses outside `M`.
    let mut promotions: Vec<Promotion, A> = Vec::new_in(alloc);
    for inst in range.start..term {
        let ext_blocks = cross.of(inst);
        if ext_blocks.is_empty() {
            continue;
        }
        let is_param = inst < params.end;
        for &ub in ext_blocks {
            // Find the unique `M`-owned successor `s` that dominates `ub`.
            let mut dom_succ = None;
            for &s in owned_succs.iter() {
                if cfg.is_dom(ub, s) {
                    dom_succ = Some(s);
                    break;
                }
            }
            let Some(s) = dom_succ else {
                // Used in a block not dominated by any sole-predecessor successor of M.
                return None;
            };
            if threaded_targets.scan_for(&s).is_some() {
                if !is_param {
                    // Non-param instruction of M used inside a threaded successor: cannot forward.
                    return None;
                }
                promotions.push(Promotion {
                    succ: s,
                    m_param_inst: to_u32(inst),
                    m_param_idx: to_u32(inst - params.start),
                });
            }
            // Otherwise `s` has M as sole predecessor and receives no threaded edge, so M still
            // strictly dominates `s` and `ub`.
        }
    }

    promotions.sort_unstable();
    promotions.dedup();

    Some(BlockPlan { m_block, edges: threaded, promotions })
}

struct Fixup {
    place: usize,
    in_data: bool,
}

/// Applies one round's non-interfering `threaded` edges and `promotions`, producing a verified `Func`.
fn apply_round<A: Allocator + Copy>(
    view: &View<'_>,
    cfg: &Cfg<A>,
    threaded: &[ThreadedEdge],
    promotions: &[Promotion],
    alloc: A,
) -> Result<Func<A>, SimplifyError> {
    let len = view.len();
    let blocks = view.blocks();
    let total_virtual = len.saturating_add(promotions.len());
    if total_virtual >= to_idx(NONE) {
        return Err(SimplifyError::Refused { inst: 0, what: "more values than LIR numbers in jump_thread" });
    }

    // Per block `b`: which promoted successor `S` dominates `b` (unique since promoted `S`s are
    // distinct children of one `M` in the dominator tree), or `NONE`.
    let mut dom_promo_succ = filled(blocks, NONE, &alloc);
    if !promotions.is_empty() {
        for b in 0..blocks {
            let b_u32 = to_u32(b);
            for promo in promotions {
                if cfg.is_dom(b_u32, promo.succ) {
                    set_at(&mut dom_promo_succ, b, promo.succ);
                    break;
                }
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

    // Virtual value map: `[0..len]` for original instructions, `[len..len + promotions.len()]` for promoted params.
    let mut full_map = filled(total_virtual, NONE, &alloc);
    let mut fixups: Vec<Fixup, A> = Vec::new_in(alloc);

    let resolve_virtual = |old_v: u32, cur_block: usize| -> u32 {
        let s = at(&dom_promo_succ, cur_block, NONE);
        if s != NONE {
            for (idx, promo) in promotions.iter().enumerate() {
                if promo.succ == s && promo.m_param_inst == old_v {
                    return to_u32(len.saturating_add(idx));
                }
            }
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

        // 3. Body instructions and terminator of block `b`.
        for inst in params.end..range.end {
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
        let cross = CrossUses::build(&v, &cfg, alloc);
        let global_vals = compute_global_vals(&v, &cfg, alloc);

        let mut round_edges: Vec<ThreadedEdge, A> = Vec::new_in(alloc);
        let mut round_promos: Vec<Promotion, A> = Vec::new_in(alloc);
        let mut claimed_m = filled(blocks, false, &alloc);
        let mut claimed_ep = filled(blocks, false, &alloc);
        let mut local_buf: Vec<EvalVal, A> = Vec::new_in(alloc);

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
            if !plan.promotions.is_empty() {
                if !round_edges.is_empty() {
                    // Defer promotion-bearing block to next round so promotion scopes stay disjoint.
                    continue;
                }
                round_edges.extend_from_slice(&plan.edges);
                round_promos.extend_from_slice(&plan.promotions);
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

        let threaded_func = apply_round(&v, &cfg, &round_edges, &round_promos, alloc)?;
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
}
