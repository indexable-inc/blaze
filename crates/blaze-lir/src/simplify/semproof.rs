//! Rust-Semantic Type Proofs & Pure-Call Optimization (`blaze_lir::semproof`).
//!
//! Exploits high-level Rust semantic invariants that the front end knows statically and lowers
//! into explicit [`ParamProof`] metadata:
//!
//! 1. **Range & `NonZero` Folding**:
//!    - Propagates `NonZero` and unsigned inclusive `[lo, hi]` value ranges from entry block
//!      parameters through value-preserving operations (`Op::Zext`, `Op::Freeze`).
//!    - Folds `Op::Icmp` comparisons implied or refuted by `NonZero` (including pointer `null`
//!      checks) or by `[lo, hi]` range bounds directly to `Op::Iconst(I1, 0/1)`.
//! 2. **Enum-Range Switch Default Elision**:
//!    - When an `Op::Switch` scrutinizes a value (or `Zext` of one) with proven range `[lo, hi]`
//!      and every integer in `lo..=hi` is explicitly handled by a case, redirects the switch's
//!      default edge to a `BlockKind::Cold` block containing only `Op::Unreachable` so backends
//!      drop the out-of-range check and jump-table guard completely.
//! 3. **Pure / `#![blaze::sans_io]` Call GVN + Loop-Invariant Call Hoisting (LICM)**:
//!    - Deduplicates identical pure `Op::Call`s (`(ref_id, sig_id, args...)`) dominated by an
//!      earlier identical call (same block, entry block `0`, or any dominating block along the
//!      dominator tree).
//!    - Detects simple natural loops (header `h` with a single outside preheader `pre` and at
//!      least one back-edge from a block dominated by `h`) and hoists pure calls in `h` or its
//!      unconditional body block whose arguments are all defined strictly outside the loop
//!      directly into `pre` right before `pre`'s terminator.
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Item, NONE, dead, each, filled},
};
use crate::{
    Block, BlockKind, Func, IntCond, Op, Ty, View,
    arith::{iconst_value, wide_value},
    func::{ParamKind, SigId},
    op::{Field, Layout},
    verify,
};

/// Semantic proof attached to an entry parameter of a function.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct ParamProof {
    /// Zero-based index into entry block `0`'s `Op::Param`s.
    pub param_idx: usize,
    /// Proven non-zero (e.g. `NonZero<T>`, `NonNull<T>`, `&T`, `&mut T`).
    pub non_zero: bool,
    /// Proven inclusive unsigned value range `[lo, hi]` (e.g. enum discriminant niche).
    pub range: Option<(u64, u64)>,
    /// Proven unaliased pointer (`&mut T`, `Box<T>`).
    pub no_alias: bool,
}

/// Telemetry from [`apply_semantic_proofs`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SemProofStats {
    pub nonzero_guards_elided: u32,
    pub range_checks_elided: u32,
    pub switch_defaults_elided: u32,
    pub pure_calls_deduped: u32,
    pub pure_calls_hoisted: u32,
}

/// Output of [`apply_semantic_proofs`].
#[derive(Debug)]
pub struct SemProofOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SemProofStats,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FoldKind {
    NonZero,
    Range,
}

#[inline]
fn swap_cond(c: IntCond) -> IntCond {
    match c {
        IntCond::Eq => IntCond::Eq,
        IntCond::Ne => IntCond::Ne,
        IntCond::Ult => IntCond::Ugt,
        IntCond::Ugt => IntCond::Ult,
        IntCond::Ule => IntCond::Uge,
        IntCond::Uge => IntCond::Ule,
        IntCond::Slt => IntCond::Sgt,
        IntCond::Sgt => IntCond::Slt,
        IntCond::Sle => IntCond::Sge,
        IntCond::Sge => IntCond::Sle,
    }
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 { s.get(i).copied().unwrap_or(0) }
#[inline]
fn at_u16(s: &[u16], i: usize) -> u16 { s.get(i).copied().unwrap_or(0) }

#[inline]
fn at_u8(s: &[u8], i: usize) -> u8 {
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
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&o| o == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

#[inline]
fn block_kind_byte(kind: BlockKind) -> u8 {
    BlockKind::ALL.iter().position(|&k| k == kind).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

/// If `inst` is a scalar integer constant (or `Op::Null` for pointer equality checks), returns its
/// unsigned value.
#[inline]
fn const_zero_or_int(view: &View<'_>, inst: usize) -> Option<(u128, bool)> {
    let op = view.op(inst);
    if op == Op::Null {
        return Some((0, true));
    }
    let ty = Ty(view.tys.get(inst).copied()?);
    if !ty.is_scalar() || !ty.is_int() {
        return None;
    }
    let width = ty.bits(view.head.ptr_bits);
    let v = match op {
        Op::Iconst => iconst_value(view.data.get(inst).copied()?, width),
        Op::IconstWide => {
            let at = to_idx(view.data.get(inst).copied()?);
            let end = at.checked_add(to_idx(ty.const_words()))?;
            wide_value(view.extra.get(at..end)?, width)
        }
        _ => return None,
    };
    Some((v, false))
}

/// Evaluates `var <cond> k` using `non_zero` and inclusive unsigned `range`.
#[inline]
fn eval_semantic_icmp(
    non_zero: bool,
    range: Option<(u64, u64)>,
    cond: IntCond,
    k: u128,
    is_ptr: bool,
) -> Option<(bool, FoldKind)> {
    if non_zero && k == 0 {
        let folded = match cond {
            IntCond::Eq => Some(false),
            IntCond::Ne => Some(true),
            IntCond::Ugt if !is_ptr => Some(true),
            IntCond::Ule if !is_ptr => Some(false),
            _ => None,
        };
        if let Some(res) = folded {
            return Some((res, FoldKind::NonZero));
        }
    }
    if !is_ptr && let Some((lo, hi)) = range {
        let lo = u128::from(lo);
        let hi = u128::from(hi);
        let folded = match cond {
            IntCond::Ult if hi < k => Some(true),
            IntCond::Ult if lo >= k => Some(false),
            IntCond::Ule if hi <= k => Some(true),
            IntCond::Ule if lo > k => Some(false),
            IntCond::Uge if lo >= k => Some(true),
            IntCond::Uge if hi < k => Some(false),
            IntCond::Ugt if lo > k => Some(true),
            IntCond::Ugt if hi <= k => Some(false),
            IntCond::Eq if k < lo || k > hi => Some(false),
            IntCond::Eq if lo == hi && lo == k => Some(true),
            IntCond::Ne if k < lo || k > hi => Some(true),
            IntCond::Ne if lo == hi && lo == k => Some(false),
            _ => None,
        };
        if let Some(res) = folded {
            return Some((res, FoldKind::Range));
        }
    }
    None
}

/// Immediate dominators and CFG adjacency for deterministic dominance & loop queries.
struct DomTree<A: Allocator> {
    preds: Vec<Vec<u32, A>, A>,
    succs: Vec<Vec<u32, A>, A>,
    idom: Vec<u32, A>,
}

impl<A: Allocator + Copy> DomTree<A> {
    fn build(view: &View<'_>, alloc: &A) -> Self {
        let blocks = view.blocks();
        let mut preds: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
        let mut succs: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, *alloc);
        for _ in 0..blocks {
            preds.push(Vec::new_in(*alloc));
            succs.push(Vec::new_in(*alloc));
        }
        for b in 0..blocks {
            let b32 = to_u32(b);
            let term = view.terminator(Block(b32));
            each(view, term, |item| {
                if let Item::Edge(edge) = item {
                    let tb = to_idx(edge.block);
                    if let Some(sb) = succs.get_mut(b) {
                        if !sb.iter().any(|&s| s == edge.block) {
                            sb.push(edge.block);
                        }
                    }
                    if let Some(pt) = preds.get_mut(tb) {
                        if !pt.iter().any(|&p| p == b32) {
                            pt.push(b32);
                        }
                    }
                }
            });
        }

        // Iterative DFS postorder from entry block 0.
        let mut seen = filled(blocks, false, alloc);
        let mut post: Vec<u32, A> = Vec::with_capacity_in(blocks, *alloc);
        let mut stack: Vec<(u32, usize), A> = Vec::with_capacity_in(blocks, *alloc);
        if blocks > 0 {
            set_at(&mut seen, 0, true);
            stack.push((0, 0));
        }
        while let Some(top) = stack.last_mut() {
            let b_u32 = top.0;
            let b = to_idx(b_u32);
            let idx = top.1;
            if let Some(&s) = succs.get(b).and_then(|v| v.get(idx)) {
                top.1 += 1;
                let si = to_idx(s);
                if !at_bool(&seen, si) {
                    set_at(&mut seen, si, true);
                    stack.push((s, 0));
                }
            } else {
                post.push(b_u32);
                stack.pop();
            }
        }

        let mut rpo: Vec<u32, A> = Vec::with_capacity_in(post.len(), *alloc);
        for &b in post.iter().rev() {
            rpo.push(b);
        }
        let mut order = filled(blocks, NONE, alloc);
        for (i, &b) in rpo.iter().enumerate() {
            set_at(&mut order, to_idx(b), to_u32(i));
        }

        let mut idom = filled(blocks, NONE, alloc);
        if blocks > 0 {
            set_at(&mut idom, 0, 0);
        }
        for _ in 0..2 * blocks + 2 {
            let mut changed = false;
            for &b in rpo.iter().skip(1) {
                let bi = to_idx(b);
                let mut new_idom = NONE;
                if let Some(bp) = preds.get(bi) {
                    for &p in bp {
                        if at_u32(&idom, to_idx(p)) == NONE {
                            continue;
                        }
                        new_idom = match new_idom {
                            NONE => p,
                            cur => intersect_dom(&idom, &order, p, cur),
                        };
                    }
                }
                if new_idom != at_u32(&idom, bi) {
                    set_at(&mut idom, bi, new_idom);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        Self { preds, succs, idom }
    }

    #[inline]
    fn reachable(&self, b: u32) -> bool {
        at_u32(&self.idom, to_idx(b)) != NONE
    }

    #[inline]
    fn dominates(&self, a: u32, mut b: u32) -> bool {
        if !self.reachable(a) || !self.reachable(b) {
            return false;
        }
        for _ in 0..self.idom.len() {
            if a == b {
                return true;
            }
            if b == 0 {
                return false;
            }
            b = at_u32(&self.idom, to_idx(b));
        }
        false
    }
}

#[inline]
fn intersect_dom(idom: &[u32], order: &[u32], mut a: u32, mut b: u32) -> u32 {
    for _ in 0..2 * idom.len() + 2 {
        if a == b {
            return a;
        }
        while at_u32(order, to_idx(a)) > at_u32(order, to_idx(b)) {
            a = at_u32(idom, to_idx(a));
        }
        while at_u32(order, to_idx(b)) > at_u32(order, to_idx(a)) {
            b = at_u32(idom, to_idx(b));
        }
    }
    a
}

/// Checks whether instruction `inst` is a pure, non-void, non-sret direct `Op::Call`.
#[inline]
fn is_eligible_pure_call(view: &View<'_>, inst: usize, is_pure_callee: &impl Fn(u32) -> bool) -> bool {
    if view.op(inst) != Op::Call {
        return false;
    }
    if Ty(at_u16(view.tys, inst)).is_none() {
        return false;
    }
    let at = to_idx(at_u32(view.data, inst));
    let ref_id = at_u32(view.extra, at);
    let sig_id = at_u32(view.extra, at + 1);
    if !is_pure_callee(ref_id) {
        return false;
    }
    let sig = view.sig(SigId(sig_id));
    !sig.params.iter().any(|p| ParamKind::from_byte(p.kind) == Some(ParamKind::Sret))
}

/// Returns whether two `Op::Call` instructions have identical `(ref_id, sig_id, resolved args...)`.
fn same_call_signature_and_args(view: &View<'_>, a: usize, b: usize, alias: &[u32]) -> bool {
    let at_a = to_idx(at_u32(view.data, a));
    let at_b = to_idx(at_u32(view.data, b));
    if at_u32(view.extra, at_a) != at_u32(view.extra, at_b)
        || at_u32(view.extra, at_a + 1) != at_u32(view.extra, at_b + 1)
    {
        return false;
    }
    let argc_a = to_idx(at_u32(view.extra, at_a + 2));
    let argc_b = to_idx(at_u32(view.extra, at_b + 2));
    if argc_a != argc_b {
        return false;
    }
    for k in 0..argc_a {
        let va = resolve_alias(alias, at_u32(view.extra, at_a + 3 + k));
        let vb = resolve_alias(alias, at_u32(view.extra, at_b + 3 + k));
        if va != vb {
            return false;
        }
    }
    true
}

#[inline]
fn resolve_alias(alias: &[u32], mut v: u32) -> u32 {
    for _ in 0..alias.len() {
        let next = at_u32(alias, to_idx(v));
        if next == NONE || next == v {
            return v;
        }
        v = next;
    }
    v
}

/// Returns whether a block contains only a single `Op::Unreachable` instruction (and 0 params).
#[inline]
fn is_unreachable_only_block(view: &View<'_>, blk: u32) -> bool {
    let range = view.block(Block(blk));
    range.len() == 1 && view.op(range.start) == Op::Unreachable
}

/// Checks if `Op::Switch` at `inst` covers every integer in `lo..=hi` with explicit cases.
fn switch_covers_range<A: Allocator + Copy>(
    view: &View<'_>,
    inst: usize,
    lo: u64,
    hi: u64,
    alloc: &A,
) -> bool {
    if lo > hi {
        return false;
    }
    let span = u128::from(hi) - u128::from(lo) + 1;
    let at = to_idx(at_u32(view.data, inst));
    let scrutinee = to_idx(at_u32(view.extra, at));
    let def_argc = to_idx(at_u32(view.extra, at + 2));
    let cases_at = at + 3 + def_argc;
    let n_cases = to_idx(at_u32(view.extra, cases_at));
    let Ok(n_cases_u128) = u128::try_from(n_cases) else {
        return false;
    };
    if n_cases_u128 < span {
        return false;
    }
    // Since `span <= n_cases`, `span` fits cleanly in a `usize`.
    let Ok(span_usize) = usize::try_from(span) else {
        return false;
    };
    let mut covered = filled(span_usize, false, alloc);
    let width = to_idx(Ty(at_u16(view.tys, scrutinee)).const_words());
    let mut p = cases_at + 1;
    let lo128 = u128::from(lo);
    let hi128 = u128::from(hi);
    for _ in 0..n_cases {
        let mut case_val = 0u128;
        for k in (0..width).rev() {
            case_val = (case_val << 32) | u128::from(at_u32(view.extra, p + k));
        }
        p += width;
        let tgt_argc = to_idx(at_u32(view.extra, p + 1));
        p += 2 + tgt_argc;
        if case_val >= lo128 && case_val <= hi128 {
            if let Ok(off) = usize::try_from(case_val - lo128) {
                set_at(&mut covered, off, true);
            }
        }
    }
    covered.iter().all(|&c| c)
}

/// Returns the block where value `v` is defined (handling `Invoke`/`InvokeIndirect`/`AsmBr`
/// whose result is defined at the head of their normal target block).
#[inline]
fn def_block_of(view: &View<'_>, block_of: &[u32], v: usize) -> u32 {
    let op = view.op(v);
    if matches!(op, Op::Invoke | Op::InvokeIndirect | Op::AsmBr) {
        let at = to_idx(at_u32(view.data, v));
        // Invoke/InvokeIndirect: [Ref/Value, Sig, NormalBlock, UnwindBlock, Values]
        // AsmBr: [Asm, FallthroughBlock, Blocks, Values]
        if op == Op::AsmBr { at_u32(view.extra, at + 1) } else { at_u32(view.extra, at + 2) }
    } else {
        at_u32(block_of, v)
    }
}

/// Applies Rust-semantic parameter proofs (`NonZero`, enum niche `[lo, hi]` ranges, `NoAlias`)
/// and pure / `#![blaze::sans_io]` call GVN + LICM, then runs `dead` and `verify`.
pub fn apply_semantic_proofs<A: Allocator + Copy>(
    view: View<'_>,
    proofs: &[ParamProof],
    is_pure_callee: impl Fn(u32) -> bool,
    alloc: A,
) -> Result<SemProofOutput<A>, SimplifyError> {
    let blocks = view.blocks();
    let len = view.len();
    if blocks == 0 || len == 0 {
        return Ok(SemProofOutput {
            func: view.to_func_in(alloc),
            stats: SemProofStats::default(),
        });
    }

    let mut stats = SemProofStats::default();

    // Map each instruction to its owning block.
    let mut block_of = filled(len, 0u32, &alloc);
    for b in 0..blocks {
        let b32 = to_u32(b);
        for inst in view.block(Block(b32)) {
            set_at(&mut block_of, inst, b32);
        }
    }

    // Build exact dominator tree + predecessor/successor lists.
    let dom = DomTree::build(&view, &alloc);

    // -------------------------------------------------------------------------
    // Phase 1: Seed entry parameter proofs & propagate through `Zext`/`Freeze`.
    // -------------------------------------------------------------------------
    let mut val_non_zero = filled(len, false, &alloc);
    let mut val_range: Vec<Option<(u64, u64)>, A> = filled(len, None, &alloc);
    let mut val_no_alias = filled(len, false, &alloc);

    let entry_params = view.params(Block(0));
    for proof in proofs {
        if proof.param_idx >= entry_params.len() {
            continue;
        }
        let p_inst = entry_params.start + proof.param_idx;
        if proof.non_zero {
            set_at(&mut val_non_zero, p_inst, true);
        }
        if let Some((lo, hi)) = proof.range
            && lo <= hi
        {
            let existing = val_range.get(p_inst).copied().flatten();
            let merged = match existing {
                Some((elo, ehi)) => {
                    let nlo = elo.max(lo);
                    let nhi = ehi.min(hi);
                    if nlo <= nhi { Some((nlo, nhi)) } else { Some((lo, hi)) }
                }
                None => Some((lo, hi)),
            };
            set_at(&mut val_range, p_inst, merged);
        }
        if proof.no_alias {
            set_at(&mut val_no_alias, p_inst, true);
        }
    }

    // Bounded fixed-point propagation through `Op::Zext` and `Op::Freeze`.
    let _ = &val_no_alias;
    for _ in 0..blocks.max(1) + 2 {
        let mut changed = false;
        for inst in 0..len {
            let op = view.op(inst);
            if !matches!(op, Op::Zext | Op::Freeze) {
                continue;
            }
            let src = to_idx(at_u32(view.data, inst));
            if at_bool(&val_non_zero, src) && !at_bool(&val_non_zero, inst) {
                set_at(&mut val_non_zero, inst, true);
                changed = true;
            }
            if let Some(r) = val_range.get(src).copied().flatten()
                && val_range.get(inst).copied().flatten() != Some(r)
            {
                set_at(&mut val_range, inst, Some(r));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // -------------------------------------------------------------------------
    // Phase 2: Fold `Op::Icmp` using `NonZero` and `[lo, hi]` proofs, and mark
    // exhaustive `Op::Switch` defaults for elision.
    // -------------------------------------------------------------------------
    let mut folded_icmp: Vec<Option<bool>, A> = filled(len, None, &alloc);
    let mut elide_switch_default = filled(len, false, &alloc);

    for b in 0..blocks {
        let b32 = to_u32(b);
        if !dom.reachable(b32) {
            continue;
        }
        for inst in view.block(Block(b32)) {
            match view.op(inst) {
                Op::Icmp => {
                    let at = to_idx(at_u32(view.data, inst));
                    let Some(cond) = IntCond::from_word(at_u32(view.extra, at)) else {
                        continue;
                    };
                    let lhs = to_idx(at_u32(view.extra, at + 1));
                    let rhs = to_idx(at_u32(view.extra, at + 2));
                    let eval_pair = if let Some((k, is_ptr)) = const_zero_or_int(&view, rhs) {
                        eval_semantic_icmp(
                            at_bool(&val_non_zero, lhs),
                            val_range.get(lhs).copied().flatten(),
                            cond,
                            k,
                            is_ptr,
                        )
                    } else if let Some((k, is_ptr)) = const_zero_or_int(&view, lhs) {
                        eval_semantic_icmp(
                            at_bool(&val_non_zero, rhs),
                            val_range.get(rhs).copied().flatten(),
                            swap_cond(cond),
                            k,
                            is_ptr,
                        )
                    } else {
                        None
                    };
                    if let Some((res, kind)) = eval_pair {
                        set_at(&mut folded_icmp, inst, Some(res));
                        match kind {
                            FoldKind::NonZero => stats.nonzero_guards_elided += 1,
                            FoldKind::Range => stats.range_checks_elided += 1,
                        }
                    }
                }
                Op::Switch => {
                    let at = to_idx(at_u32(view.data, inst));
                    let scrutinee = to_idx(at_u32(view.extra, at));
                    let def_blk = at_u32(view.extra, at + 1);
                    if is_unreachable_only_block(&view, def_blk) {
                        continue;
                    }
                    if let Some((lo, hi)) = val_range.get(scrutinee).copied().flatten()
                        && switch_covers_range(&view, inst, lo, hi, &alloc)
                    {
                        set_at(&mut elide_switch_default, inst, true);
                        stats.switch_defaults_elided += 1;
                    }
                }
                _ => {}
            }
        }
    }

    // -------------------------------------------------------------------------
    // Phase 3: Pure / `#![blaze::sans_io]` Call GVN (same block + dominators).
    // -------------------------------------------------------------------------
    let mut alias = filled(len, NONE, &alloc);
    let mut removed = filled(len, false, &alloc);

    // Collect pure calls per block in deterministic instruction order.
    let mut block_pure_calls: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_pure_calls.push(Vec::new_in(alloc));
    }

    // Build dominator-tree preorder so every dominator is processed before its dominated blocks.
    let mut dom_children: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        dom_children.push(Vec::new_in(alloc));
    }
    for b in 1..blocks {
        let p = at_u32(&dom.idom, b);
        if p != NONE {
            if let Some(dc) = dom_children.get_mut(to_idx(p)) {
                dc.push(to_u32(b));
            }
        }
    }
    let mut dom_order: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    let mut dstack: Vec<u32, A> = Vec::with_capacity_in(blocks, alloc);
    dstack.push(0);
    while let Some(b) = dstack.pop() {
        dom_order.push(b);
        if let Some(dc) = dom_children.get(to_idx(b)) {
            for &child in dc.iter().rev() {
                dstack.push(child);
            }
        }
    }

    for &b in &dom_order {
        for inst in view.block(Block(b)) {
            if !is_eligible_pure_call(&view, inst, &is_pure_callee) {
                continue;
            }
            // Walk up the dominator chain starting at `b` (covering same block, all dominators,
            // and entry block `0`) to find an identical earlier pure call.
            let mut found = NONE;
            let mut cur = b;
            for _ in 0..blocks {
                if let Some(prevs) = block_pure_calls.get(to_idx(cur)) {
                    for &prev in prevs {
                        if same_call_signature_and_args(&view, to_idx(prev), inst, &alias) {
                            found = prev;
                            break;
                        }
                    }
                }
                if found != NONE || cur == 0 {
                    break;
                }
                cur = at_u32(&dom.idom, to_idx(cur));
            }
            if found != NONE {
                set_at(&mut alias, inst, found);
                set_at(&mut removed, inst, true);
                stats.pure_calls_deduped += 1;
            } else if let Some(bpc) = block_pure_calls.get_mut(to_idx(b)) {
                bpc.push(to_u32(inst));
            }
        }
    }

    // -------------------------------------------------------------------------
    // Phase 4: Pure-Call LICM (natural loop detection & preheader hoisting).
    // -------------------------------------------------------------------------
    // Per block `pre`, list of original instruction indices to hoist right before `pre`'s terminator.
    let mut hoist_into: Vec<Vec<u32, A>, A> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        hoist_into.push(Vec::new_in(alloc));
    }
    // Original block or hoisted target block of each instruction (updated as instructions hoist).
    let mut effective_block = Vec::with_capacity_in(len, alloc);
    for v in 0..len {
        effective_block.push(def_block_of(&view, &block_of, v));
    }
    let mut hoisted = filled(len, false, &alloc);

    for h in 1..blocks {
        let h_u32 = to_u32(h);
        if !dom.reachable(h_u32) {
            continue;
        }
        // Identify back-edges (preds dominated by `h`) and outside entry edges.
        let mut back_preds: Vec<u32, A> = Vec::new_in(alloc);
        let mut outside_pre = NONE;
        let mut multiple_outside = false;
        if let Some(hp) = dom.preds.get(h) {
            for &p in hp {
                if !dom.reachable(p) {
                    continue;
                }
                if dom.dominates(h_u32, p) {
                    back_preds.push(p);
                } else if outside_pre == NONE {
                    outside_pre = p;
                } else {
                    multiple_outside = true;
                    break;
                }
            }
        }
        if back_preds.is_empty() || outside_pre == NONE || multiple_outside {
            continue;
        }
        if !dom.dominates(outside_pre, h_u32) {
            continue;
        }
        let pre = to_idx(outside_pre);
        let pre_term = view.terminator(Block(outside_pre));

        // Compute the natural loop body by backward walk from `back_preds` stopping at `h`.
        let mut in_loop = filled(blocks, false, &alloc);
        let mut lwork: Vec<u32, A> = Vec::new_in(alloc);
        set_at(&mut in_loop, h, true);
        for &bp in &back_preds {
            let bpi = to_idx(bp);
            if !at_bool(&in_loop, bpi) {
                set_at(&mut in_loop, bpi, true);
                lwork.push(bp);
            }
        }
        let mut reducible = true;
        while let Some(cur) = lwork.pop() {
            if !dom.dominates(h_u32, cur) {
                reducible = false;
                break;
            }
            if let Some(cp) = dom.preds.get(to_idx(cur)) {
                for &p in cp {
                    if !dom.reachable(p) {
                        continue;
                    }
                    let pi = to_idx(p);
                    if !at_bool(&in_loop, pi) {
                        set_at(&mut in_loop, pi, true);
                        lwork.push(p);
                    }
                }
            }
        }
        if !reducible {
            continue;
        }

        // Collect blocks executing unconditionally on every loop iteration:
        // header `h`, plus the unique in-loop successor `body` of `h` if `h` has exactly one.
        let mut hoist_blocks: Vec<u32, A> = Vec::new_in(alloc);
        hoist_blocks.push(h_u32);
        let mut unique_body = NONE;
        let mut body_count = 0u32;
        if let Some(hs) = dom.succs.get(h) {
            for &s in hs {
                if at_bool(&in_loop, to_idx(s)) && s != h_u32 {
                    unique_body = s;
                    body_count += 1;
                }
            }
        }
        if body_count == 1 && dom.dominates(h_u32, unique_body) {
            hoist_blocks.push(unique_body);
        }

        // Scan eligible pure calls in `hoist_blocks` in deterministic order.
        for &hb in &hoist_blocks {
            for inst in view.block(Block(hb)) {
                if at_bool(&removed, inst) || at_bool(&hoisted, inst) {
                    continue;
                }
                if !is_eligible_pure_call(&view, inst, &is_pure_callee) {
                    continue;
                }
                let at = to_idx(at_u32(view.data, inst));
                let argc = to_idx(at_u32(view.extra, at + 2));
                let mut all_invariant = true;
                for k in 0..argc {
                    let arg = to_idx(resolve_alias(&alias, at_u32(view.extra, at + 3 + k)));
                    let arg_blk = at_u32(&effective_block, arg);
                    if at_bool(&in_loop, to_idx(arg_blk)) {
                        all_invariant = false;
                        break;
                    }
                    // Ensure `arg` is defined strictly before `pre`'s terminator so hoisting into
                    // `pre` preserves SSA dominance.
                    if arg_blk == outside_pre {
                        if !at_bool(&hoisted, arg) && arg >= pre_term {
                            all_invariant = false;
                            break;
                        }
                    } else if !dom.dominates(arg_blk, outside_pre) {
                        all_invariant = false;
                        break;
                    }
                }
                if !all_invariant {
                    continue;
                }

                // If an identical pure call already exists in `pre` (or was already hoisted into
                // `pre`), deduplicate against it instead of emitting a duplicate.
                let mut dup_in_pre = NONE;
                if let Some(pre_calls) = block_pure_calls.get(pre) {
                    for &prev in pre_calls {
                        if same_call_signature_and_args(&view, to_idx(prev), inst, &alias) {
                            dup_in_pre = prev;
                            break;
                        }
                    }
                }
                if dup_in_pre != NONE {
                    set_at(&mut alias, inst, dup_in_pre);
                    set_at(&mut removed, inst, true);
                    stats.pure_calls_deduped += 1;
                } else {
                    set_at(&mut hoisted, inst, true);
                    set_at(&mut effective_block, inst, outside_pre);
                    if let Some(hi_vec) = hoist_into.get_mut(pre) {
                        hi_vec.push(to_u32(inst));
                    }
                    if let Some(bpc) = block_pure_calls.get_mut(pre) {
                        bpc.push(to_u32(inst));
                    }
                    stats.pure_calls_hoisted += 1;
                }
            }
        }
    }

    let any_change = stats.nonzero_guards_elided > 0
        || stats.range_checks_elided > 0
        || stats.switch_defaults_elided > 0
        || stats.pure_calls_deduped > 0
        || stats.pure_calls_hoisted > 0;
    if !any_change {
        return Ok(SemProofOutput {
            func: view.to_func_in(alloc),
            stats,
        });
    }

    // -------------------------------------------------------------------------
    // Phase 5: Rebuild `Func<A>` with folded `Icmp`s, elided `Switch` defaults,
    // removed duplicate calls, and hoisted loop-invariant pure calls.
    // -------------------------------------------------------------------------
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

    let need_cold_unreach = stats.switch_defaults_elided > 0;
    let cold_unreach_blk = to_u32(blocks);

    let mut val_map = filled(len, NONE, &alloc);
    // Fixups for any value reference whose target hasn't been emitted yet (non-RPO layouts):
    // `(index_in_column, in_data_col, original_val_id)`.
    let mut val_fixups: Vec<(usize, bool, u32), A> = Vec::new_in(alloc);

    for b in 0..blocks {
        let b32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at_u8(view.block_kinds, b));
        let range = view.block(Block(b32));
        let term = range.end - 1;

        for inst in range {
            // Emit any calls hoisted into `b` right before `b`'s terminator.
            if inst == term {
                if let Some(hb) = hoist_into.get(b) {
                    for &h_inst in hb {
                        let hi = to_idx(h_inst);
                        let new_idx = emit_copied_inst(
                            &view,
                            hi,
                            &alias,
                            &val_map,
                            &mut val_fixups,
                            &mut func,
                        );
                        set_at(&mut val_map, hi, new_idx);
                    }
                }
            }

            if at_bool(&removed, inst) || at_bool(&hoisted, inst) {
                continue;
            }

            if let Some(res) = folded_icmp.get(inst).copied().flatten() {
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(Op::Iconst));
                func.flags.push(0);
                func.tys.push(Ty::I1.0);
                func.data.push(u32::from(res));
                func.attrs.push(0);
                func.locs.push(at_u32(view.locs, inst));
                set_at(&mut val_map, inst, idx);
                continue;
            }

            if at_bool(&elide_switch_default, inst) {
                let idx = emit_switch_with_unreachable_default(
                    &view,
                    inst,
                    cold_unreach_blk,
                    &alias,
                    &val_map,
                    &mut val_fixups,
                    &mut func,
                );
                set_at(&mut val_map, inst, idx);
                continue;
            }

            let mapped = emit_copied_inst(&view, inst, &alias, &val_map, &mut val_fixups, &mut func);
            set_at(&mut val_map, inst, mapped);
        }
    }

    if need_cold_unreach {
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(block_kind_byte(BlockKind::Cold));
        func.ops.push(op_byte(Op::Unreachable));
        func.flags.push(0);
        func.tys.push(Ty::NONE.0);
        func.data.push(0);
        func.attrs.push(0);
        func.locs.push(0);
    }

    // Patch any deferred value fixups.
    for &(place, in_data, orig_v) in &val_fixups {
        let resolved = resolve_alias(&alias, orig_v);
        let mapped = at_u32(&val_map, to_idx(resolved));
        if mapped == NONE {
            return Err(SimplifyError::Refused {
                inst: to_idx(orig_v),
                what: "unresolved value reference after semantic proof rewrite",
            });
        }
        if in_data {
            set_at(&mut func.data, place, mapped);
        } else {
            set_at(&mut func.extra, place, mapped);
        }
    }

    verify(func.view()).map_err(SimplifyError::Verify)?;
    let cleaned = dead(func.view(), alloc)?;
    verify(cleaned.view()).map_err(SimplifyError::Verify)?;
    Ok(SemProofOutput { func: cleaned, stats })
}

#[inline]
fn map_val_or_fixup<A: Allocator>(
    orig_v: u32,
    alias: &[u32],
    val_map: &[u32],
    val_fixups: &mut Vec<(usize, bool, u32), A>,
    place: usize,
    in_data: bool,
) -> u32 {
    let resolved = resolve_alias(alias, orig_v);
    let mapped = at_u32(val_map, to_idx(resolved));
    if mapped == NONE {
        val_fixups.push((place, in_data, resolved));
        0
    } else {
        mapped
    }
}

fn emit_copied_inst<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    alias: &[u32],
    val_map: &[u32],
    val_fixups: &mut Vec<(usize, bool, u32), A>,
    func: &mut Func<A>,
) -> u32 {
    let op = view.op(inst);
    let op_raw = at_u8(view.ops, inst);
    let flags = at_u8(view.flags, inst);
    let ty = at_u16(view.tys, inst);
    let attr = at_u32(view.attrs, inst);
    let loc = at_u32(view.locs, inst);
    let inst_data = at_u32(view.data, inst);
    let next_idx = func.ops.len();
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => {
            map_val_or_fixup(inst_data, alias, val_map, val_fixups, next_idx, true)
        }
        Layout::Data(_) => inst_data,
        Layout::Extra(fields) => {
            let src = to_idx(inst_data);
            let dst = to_u32(func.extra.len());
            let mut p = src;
            let mut scrutinee = None;
            for &f in fields {
                match f {
                    Field::Value => {
                        let ev = at_u32(view.extra, p);
                        scrutinee = Some(ev);
                        let place = func.extra.len();
                        let mv = map_val_or_fixup(ev, alias, val_map, val_fixups, place, false);
                        func.extra.push(mv);
                        p += 1;
                    }
                    Field::Values => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            let place = func.extra.len();
                            let mv = map_val_or_fixup(
                                at_u32(view.extra, p + k),
                                alias,
                                val_map,
                                val_fixups,
                                place,
                                false,
                            );
                            func.extra.push(mv);
                        }
                        p += n;
                    }
                    Field::Blocks => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Target => {
                        func.extra.push(at_u32(view.extra, p));
                        let n_u32 = at_u32(view.extra, p + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 2;
                        for k in 0..n {
                            let place = func.extra.len();
                            let mv = map_val_or_fixup(
                                at_u32(view.extra, p + k),
                                alias,
                                val_map,
                                val_fixups,
                                place,
                                false,
                            );
                            func.extra.push(mv);
                        }
                        p += n;
                    }
                    Field::Cases => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        let s_val = scrutinee.unwrap_or_else(|| at_u32(view.extra, src));
                        let w = to_idx(Ty(at_u16(view.tys, to_idx(s_val))).const_words());
                        for _ in 0..n {
                            for k in 0..w {
                                func.extra.push(at_u32(view.extra, p + k));
                            }
                            p += w;
                            func.extra.push(at_u32(view.extra, p));
                            let an_u32 = at_u32(view.extra, p + 1);
                            let an = to_idx(an_u32);
                            func.extra.push(an_u32);
                            p += 2;
                            for k in 0..an {
                                let place = func.extra.len();
                                let mv = map_val_or_fixup(
                                    at_u32(view.extra, p + k),
                                    alias,
                                    val_map,
                                    val_fixups,
                                    place,
                                    false,
                                );
                                func.extra.push(mv);
                            }
                            p += an;
                        }
                    }
                    Field::Mask => {
                        let n_u32 = at_u32(view.extra, p);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        p += 1;
                        for k in 0..n {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += n;
                    }
                    Field::Const => {
                        let w = to_idx(Ty(ty).const_words());
                        for k in 0..w {
                            func.extra.push(at_u32(view.extra, p + k));
                        }
                        p += w;
                    }
                    _ => {
                        func.extra.push(at_u32(view.extra, p));
                        p += 1;
                    }
                }
            }
            dst
        }
    };
    let idx = to_u32(func.ops.len());
    func.ops.push(op_raw);
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attr);
    func.locs.push(loc);
    idx
}

/// Emits `Op::Switch` at `inst` with its default target redirected to `(cold_unreach_blk, &[])`.
fn emit_switch_with_unreachable_default<A: Allocator>(
    view: &View<'_>,
    inst: usize,
    cold_unreach_blk: u32,
    alias: &[u32],
    val_map: &[u32],
    val_fixups: &mut Vec<(usize, bool, u32), A>,
    func: &mut Func<A>,
) -> u32 {
    let src = to_idx(at_u32(view.data, inst));
    let dst = to_u32(func.extra.len());
    let scrutinee = at_u32(view.extra, src);
    let place = func.extra.len();
    let mv_scrut = map_val_or_fixup(scrutinee, alias, val_map, val_fixups, place, false);
    func.extra.push(mv_scrut);

    // Replace default target with `(cold_unreach_blk, 0 args)`.
    let old_def_argc = to_idx(at_u32(view.extra, src + 2));
    func.extra.push(cold_unreach_blk);
    func.extra.push(0);

    // Copy all explicit cases verbatim with remapped argument values.
    let mut p = src + 3 + old_def_argc;
    let n_cases_u32 = at_u32(view.extra, p);
    let n_cases = to_idx(n_cases_u32);
    func.extra.push(n_cases_u32);
    p += 1;
    let w = to_idx(Ty(at_u16(view.tys, to_idx(scrutinee))).const_words());
    for _ in 0..n_cases {
        for k in 0..w {
            func.extra.push(at_u32(view.extra, p + k));
        }
        p += w;
        func.extra.push(at_u32(view.extra, p));
        let an_u32 = at_u32(view.extra, p + 1);
        let an = to_idx(an_u32);
        func.extra.push(an_u32);
        p += 2;
        for k in 0..an {
            let ap = func.extra.len();
            let mv = map_val_or_fixup(at_u32(view.extra, p + k), alias, val_map, val_fixups, ap, false);
            func.extra.push(mv);
        }
        p += an;
    }

    let idx = to_u32(func.ops.len());
    func.ops.push(op_byte(Op::Switch));
    func.flags.push(at_u8(view.flags, inst));
    func.tys.push(at_u16(view.tys, inst));
    func.data.push(dst);
    func.attrs.push(at_u32(view.attrs, inst));
    func.locs.push(at_u32(view.locs, inst));
    idx
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;
    use core::cell::Cell;

    use super::*;
    use crate::{
        Flags,
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Bodies, BodyId, Outcome, Val},
        func::{BlockKind, Conv, RefId, RefKind},
    };

    #[test]
    fn nonzero_and_enum_range_eliminate_guards_and_make_switch_exhaustive() {
        // Function taking `(nz: i64 [NonZero], tag: i8 [0..=3]) -> i64`:
        //   if nz == 0 { trap(1); }
        //   let q = 100 / nz;
        //   let tag64 = zext tag as i64;
        //   if tag64 > 3 { trap(2); }
        //   match tag64 { 0 => q + 10, 1 => q + 20, 2 => q + 30, 3 => q + 40, _ => trap(3) }
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I8)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let [
            entry,
            trap_zero,
            after_div,
            trap_range,
            do_switch,
            arm0,
            arm1,
            arm2,
            arm3,
            arm_def,
        ] = [(); 10].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let nz = b.param(Ty::I64);
        let tag = b.param(Ty::I8);
        let z64 = b.iconst(Ty::I64, 0);
        let is_zero = b.icmp(IntCond::Eq, nz, z64);
        b.br(Flags::NONE, is_zero, (trap_zero, &[]), (after_div, &[]));

        b.begin(trap_zero);
        b.trap(1);

        b.begin(after_div);
        let k100 = b.iconst(Ty::I64, 100);
        let q = b.binary(Op::Udiv, Flags::NONE, k100, nz);
        let tag64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, tag);
        let k3 = b.iconst(Ty::I64, 3);
        let oob = b.icmp(IntCond::Ugt, tag64, k3);
        b.br(Flags::NONE, oob, (trap_range, &[]), (do_switch, &[]));

        b.begin(trap_range);
        b.trap(2);

        b.begin(do_switch);
        b.switch(
            tag64,
            (arm_def, &[]),
            &[
                (0, (arm0, &[])),
                (1, (arm1, &[])),
                (2, (arm2, &[])),
                (3, (arm3, &[])),
            ],
        );

        b.begin(arm0);
        let k10 = b.iconst(Ty::I64, 10);
        let r0 = b.binary(Op::Add, Flags::NONE, q, k10);
        b.ret(&[r0]);

        b.begin(arm1);
        let k20 = b.iconst(Ty::I64, 20);
        let r1 = b.binary(Op::Add, Flags::NONE, q, k20);
        b.ret(&[r1]);

        b.begin(arm2);
        let k30 = b.iconst(Ty::I64, 30);
        let r2 = b.binary(Op::Add, Flags::NONE, q, k30);
        b.ret(&[r2]);

        b.begin(arm3);
        let k40 = b.iconst(Ty::I64, 40);
        let r3 = b.binary(Op::Add, Flags::NONE, q, k40);
        b.ret(&[r3]);

        b.begin(arm_def);
        b.trap(3);

        let func = b.finish_checked().unwrap();

        let proofs = [
            ParamProof { param_idx: 0, non_zero: true, range: None, no_alias: false },
            ParamProof { param_idx: 1, non_zero: false, range: Some((0, 3)), no_alias: false },
        ];
        let out = apply_semantic_proofs(func.view(), &proofs, |_| false, TestHeap).unwrap();

        assert_eq!(out.stats.nonzero_guards_elided, 1, "NonZero guard must be elided");
        assert_eq!(out.stats.range_checks_elided, 1, "enum range check must be elided");
        assert_eq!(out.stats.switch_defaults_elided, 1, "4-case enum switch default must become Unreachable");

        let ov = out.func.view();
        assert!(
            !(0..ov.len()).any(|i| ov.op(i) == Op::Trap),
            "all div-by-zero, range, and default traps must be eliminated"
        );

        // Verify the surviving `Op::Switch`'s default points to a `BlockKind::Cold` `Unreachable`-only block.
        let sw_inst = (0..ov.len()).find(|&i| ov.op(i) == Op::Switch).expect("switch remains");
        let def_blk = ov.extra[ov.data[sw_inst] as usize + 1];
        assert_eq!(ov.block_kind(Block(def_blk)), BlockKind::Cold);
        let def_range = ov.block(Block(def_blk));
        assert_eq!(def_range.len(), 1);
        assert_eq!(ov.op(def_range.start), Op::Unreachable);

        // Semantic equivalence verified via `eval::run_in` across valid inputs.
        for &nz_v in &[1u128, 2, 4, 5, 10, 25] {
            for tag_v in 0u128..=3 {
                let Outcome::Returned(orig) =
                    eval::run_in(func.view(), &[Val::Int(nz_v), Val::Int(tag_v)], 10_000, TestHeap).unwrap()
                else {
                    panic!("orig did not return");
                };
                let Outcome::Returned(opt) =
                    eval::run_in(ov, &[Val::Int(nz_v), Val::Int(tag_v)], 10_000, TestHeap).unwrap()
                else {
                    panic!("opt did not return");
                };
                assert_eq!(orig, opt, "mismatch for nz={nz_v}, tag={tag_v}");
            }
        }
    }

    #[test]
    fn pure_call_gvn_and_licm_hoist_out_of_loop() {
        const ROOT: BodyId = BodyId([1; 32]);
        const PURE_F: BodyId = BodyId([2; 32]);

        // Pure callee `f(x: i64) -> i64 = x * 3 + 7`.
        let mut fb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let f_sig = fb.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        fb.set_signature(f_sig);
        let f_entry = fb.create_block(BlockKind::Normal);
        fb.begin(f_entry);
        let fx = fb.param(Ty::I64);
        let k3 = fb.iconst(Ty::I64, 3);
        let k7 = fb.iconst(Ty::I64, 7);
        let mul = fb.binary(Op::Mul, Flags::NONE, fx, k3);
        let res = fb.binary(Op::Add, Flags::NONE, mul, k7);
        fb.ret(&[res]);
        let pure_f = fb.finish_checked().unwrap();

        // Caller `caller(x: i64, n: i64) -> i64`:
        //   let mut acc = 0;
        //   for i in 0..n {
        //       let a = f(x);
        //       let b = f(x);
        //       acc += a + b;
        //   }
        //   acc
        let mut cb = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let c_sig = cb.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        cb.set_signature(c_sig);
        let callee_sig = cb.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let f_ref = cb.reference(RefKind::Func, 0);
        let [entry, header, body, exit] = [(); 4].map(|_| cb.create_block(BlockKind::Normal));

        cb.begin(entry);
        let x = cb.param(Ty::I64);
        let n = cb.param(Ty::I64);
        let zero = cb.iconst(Ty::I64, 0);
        cb.jump((header, &[zero, zero]));

        cb.begin(header);
        let i = cb.param(Ty::I64);
        let acc = cb.param(Ty::I64);
        let keep = cb.icmp(IntCond::Ult, i, n);
        cb.br(Flags::NONE, keep, (body, &[]), (exit, &[]));

        cb.begin(body);
        let c1 = cb.call(Flags::NONE, f_ref, callee_sig, &[x]);
        let c2 = cb.call(Flags::NONE, f_ref, callee_sig, &[x]);
        let pair = cb.binary(Op::Add, Flags::NONE, c1, c2);
        let acc2 = cb.binary(Op::Add, Flags::NONE, acc, pair);
        let one = cb.iconst(Ty::I64, 1);
        let i2 = cb.binary(Op::Add, Flags::NONE, i, one);
        cb.jump((header, &[i2, acc2]));

        cb.begin(exit);
        cb.ret(&[acc]);

        let caller = cb.finish_checked().unwrap();

        let out = apply_semantic_proofs(caller.view(), &[], |r| r == f_ref.0, TestHeap).unwrap();
        assert_eq!(out.stats.pure_calls_deduped, 1, "duplicate pure call must be deduped by GVN");
        assert_eq!(out.stats.pure_calls_hoisted, 1, "loop-invariant pure call must be hoisted to preheader");

        let ov = out.func.view();
        let mut calls: Vec<usize, TestHeap> = Vec::new_in(TestHeap);
        for k in 0..ov.len() {
            if ov.op(k) == Op::Call {
                calls.push(k);
            }
        }
        assert_eq!(calls.len(), 1, "exactly one pure call must remain");
        assert!(ov.block(Block(0)).contains(&calls[0]), "surviving pure call must be hoisted into entry/preheader");

        // Verify via `eval::run_calls_in` that values match AND `f(x)` runs ONCE instead of `2 * N` times!
        struct CountingBodies<'a> {
            pure_f: &'a Func<TestHeap>,
            calls: Cell<u32>,
        }
        impl Bodies for CountingBodies<'_> {
            fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
                if from == ROOT && reference == RefId(0) {
                    self.calls.set(self.calls.get() + 1);
                    Some((PURE_F, self.pure_f.view()))
                } else {
                    None
                }
            }
        }

        for &(xv, nv) in &[(10u128, 5u128), (4, 8), (0, 3), (7, 1)] {
            let orig_bodies = CountingBodies { pure_f: &pure_f, calls: Cell::new(0) };
            let Outcome::Returned(orig_ret) =
                eval::run_calls_in(caller.view(), ROOT, &[Val::Int(xv), Val::Int(nv)], 100_000, &orig_bodies, TestHeap)
                    .unwrap()
            else {
                panic!("orig did not return");
            };
            assert_eq!(orig_bodies.calls.get(), (2 * nv) as u32, "orig must execute f(x) 2*N times");

            let opt_bodies = CountingBodies { pure_f: &pure_f, calls: Cell::new(0) };
            let Outcome::Returned(opt_ret) =
                eval::run_calls_in(ov, ROOT, &[Val::Int(xv), Val::Int(nv)], 100_000, &opt_bodies, TestHeap)
                    .unwrap()
            else {
                panic!("opt did not return");
            };
            assert_eq!(opt_ret, orig_ret, "hoisted result mismatch for x={xv}, n={nv}");
            assert_eq!(opt_bodies.calls.get(), 1, "hoisted loop must execute f(x) exactly ONCE");
        }
    }
}
