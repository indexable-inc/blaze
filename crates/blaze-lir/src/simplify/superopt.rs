//! Bounded AArch64 E-Graph Equality-Saturation Superoptimizer (`blaze_lir::superopt`).
//!
//! Unlike the single-pass `aegraph` (designed for microsecond compile times on every function),
//! this pass runs **bounded multi-round equality saturation** over pure scalar integer DAGs
//! (typically PGO-gated to the hottest ~1–2% of functions/loops or release `blaze export`),
//! then extracts the minimum-cost DAG under an explicit AArch64 microarchitectural cost model
//! (`AppleSilicon` / `NeoverseV2`) that accounts for:
//!
//! - Fused `madd`/`msub` (`add(mul(a, b), c)`, `sub(c, mul(a, b))`)
//! - Fused shifted-register ALU (`add/sub lsl #k`, `add lsr #k`)
//! - Fused bitwise-not ALU (`bic`, `orn`, `eon`)
//! - Shift-add constant-multiply synthesis (`x * 3`, `x * 5`, `x * 7`, `x * 9`, `x * 10`, ...)
//! - Distributivity / common-factor extraction (`a*c + b*c -> (a+b)*c`, `(a<<k) + (b<<k) -> (a+b)<<k`)
//! - De Morgan reductions (`(!a) & (!b) -> !(a | b)`) and associative constant folding
//!
//! Pure sans-IO, `#![forbid(unsafe_code)]`, deterministic (Rule 25), explicit allocator only.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{NONE, dead, filled},
};
use crate::{
    Block, Flags, Func, Op, Ty, View,
    arith::{binary, iconst_value, sext, unary, wide_value},
    op::{Field, Layout},
    verify,
};

/// Target CPU microarchitecture for superoptimizer cost-model extraction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AArch64MicroArch {
    /// Apple M-series (wide OOO, 3-cycle `mul`/`madd`, 1-cycle fused shifted ALU & `bic`/`orn`/`eon`).
    AppleSilicon,
    /// Arm Neoverse V2 / V3 (AWS Graviton 4/5: slightly higher multiply/shift latency weighting).
    NeoverseV2,
    /// AMD Zen 4/5 / Intel x86-64-v3 (1-cycle 3-operand `lea [a + b*{1,2,4,8}]`, BMI1 `andn`, 3-cycle `imul`).
    X86_64Zen5,
}

/// Options for [`superopt_a64`] (rule `options-struct`: single plain struct with explicit knobs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SuperoptOpts {
    pub microarch: AArch64MicroArch,
    /// Maximum equality-saturation rounds per basic block.
    pub max_iters: u32,
    /// Hard upper bound on total e-nodes per basic block (guarantees bounded progress, rule 18).
    pub max_enodes: u32,
    /// When `true`, only superoptimize blocks for which `is_hot(block_idx)` returns `true`.
    pub hot_only: bool,
}

impl SuperoptOpts {
    /// Default release/export superoptimizer configuration for Apple Silicon.
    pub const fn apple_silicon() -> Self {
        Self {
            microarch: AArch64MicroArch::AppleSilicon,
            max_iters: 4,
            max_enodes: 1024,
            hot_only: false,
        }
    }

    /// Default release/export superoptimizer configuration for Graviton 4/5 (Neoverse V2/V3).
    pub const fn neoverse_v2() -> Self {
        Self {
            microarch: AArch64MicroArch::NeoverseV2,
            max_iters: 4,
            max_enodes: 1024,
            hot_only: false,
        }
    }

    /// Default release/export superoptimizer configuration for x86-64 (EPYC Zen 5 / x86-64-v3).
    pub const fn x86_64_zen5() -> Self {
        Self {
            microarch: AArch64MicroArch::X86_64Zen5,
            max_iters: 4,
            max_enodes: 1024,
            hot_only: false,
        }
    }
}

/// Telemetry returned by [`superopt_a64`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SuperoptStats {
    pub blocks_superopted: u32,
    pub enodes_explored: u32,
    pub rewrites_applied: u32,
    /// Sum of estimated AArch64 tenths-of-cycle cost across rewritten roots before extraction.
    pub cost_before: u32,
    /// Sum of estimated AArch64 tenths-of-cycle cost across rewritten roots after extraction.
    pub cost_after: u32,
}

/// Output of [`superopt_a64`]: verified LIR function plus superoptimization telemetry.
#[derive(Debug)]
pub struct SuperoptOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: SuperoptStats,
}

type EId = u32;

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

#[inline]
fn to_u32(v: usize) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

#[inline]
fn trunc_u32(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn trunc_u64(v: u128) -> u64 {
    let b = v.to_le_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[inline]
fn at<T: Copy + Default>(s: &[T], i: usize) -> T {
    s.get(i).copied().unwrap_or_default()
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn op_byte(op: Op) -> u8 {
    Op::ALL.iter().position(|&c| c == op).and_then(|i| u8::try_from(i).ok()).unwrap_or(0)
}

/// One pure scalar integer e-node inside a single basic block's e-graph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ENode {
    /// Opaque external value (defined outside the block or by a non-pure op), holding its original LIR inst id.
    Var(u32),
    /// Known integer constant (masked to the class's bit width).
    Const(u64),
    /// Unary pure op (`Neg`, `Not`).
    Unary(Op, EId),
    /// Binary pure op (`Add`, `Sub`, `Mul`, `And`, `Or`, `Xor`, `Shl`, `Lshr`, `Ashr`).
    Binary(Op, EId, EId),
}

impl ENode {
    #[inline]
    fn canonicalize(self, uf: &mut UnionFind<impl Allocator>) -> Self {
        match self {
            ENode::Var(v) => ENode::Var(v),
            ENode::Const(c) => ENode::Const(c),
            ENode::Unary(op, a) => ENode::Unary(op, uf.find(a)),
            ENode::Binary(op, a, b) => {
                let fa = uf.find(a);
                let fb = uf.find(b);
                if matches!(op, Op::Add | Op::Mul | Op::And | Op::Or | Op::Xor) && fa > fb {
                    ENode::Binary(op, fb, fa)
                } else {
                    ENode::Binary(op, fa, fb)
                }
            }
        }
    }
}

struct UnionFind<A: Allocator> {
    parent: Vec<EId, A>,
}

impl<A: Allocator> UnionFind<A> {
    fn new(cap: usize, alloc: A) -> Self {
        Self { parent: Vec::with_capacity_in(cap, alloc) }
    }

    #[inline]
    fn make_set(&mut self) -> EId {
        let id = to_u32(self.parent.len());
        self.parent.push(id);
        id
    }

    #[inline]
    fn find(&mut self, mut x: EId) -> EId {
        while at(&self.parent, to_idx(x)) != x {
            let p = at(&self.parent, to_idx(x));
            let gp = at(&self.parent, to_idx(p));
            set_at(&mut self.parent, to_idx(x), gp);
            x = p;
        }
        x
    }

    #[inline]
    fn union(&mut self, a: EId, b: EId) -> EId {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        // Deterministic canonical leader: always pick the smaller class id.
        let (keep, drop) = if ra < rb { (ra, rb) } else { (rb, ra) };
        set_at(&mut self.parent, to_idx(drop), keep);
        keep
    }
}

struct EClass<A: Allocator> {
    ty: Ty,
    bits: u32,
    nodes: Vec<ENode, A>,
    const_val: Option<u64>,
}

struct EGraph<A: Allocator> {
    uf: UnionFind<A>,
    classes: Vec<EClass<A>, A>,
    total_nodes: u32,
    max_nodes: u32,
    alloc: A,
}

impl<A: Allocator + Copy> EGraph<A> {
    fn new(max_nodes: u32, alloc: A) -> Self {
        Self {
            uf: UnionFind::new(64, alloc),
            classes: Vec::with_capacity_in(64, alloc),
            total_nodes: 0,
            max_nodes,
            alloc,
        }
    }

    #[inline]
    fn at_budget(&self) -> bool {
        self.total_nodes >= self.max_nodes
    }

    /// Looks up or inserts `node` of scalar type `ty` (`bits` wide), returning its canonical class id.
    fn add(&mut self, node: ENode, ty: Ty, bits: u32) -> Option<EId> {
        let canon = node.canonicalize(&mut self.uf);
        // Linear search over existing active classes (bounded by `max_nodes <= 1024`, deterministic).
        for id in 0..self.classes.len() {
            let id_e = to_u32(id);
            if self.uf.find(id_e) != id_e {
                continue;
            }
            if let Some(cls) = self.classes.get(id)
                && cls.ty == ty
                && cls.nodes.scan_for(&canon).is_some()
            {
                return Some(id_e);
            }
        }
        if self.total_nodes >= self.max_nodes {
            return None;
        }
        let id = self.uf.make_set();
        let mut nodes = Vec::with_capacity_in(4, self.alloc);
        nodes.push(canon);
        let const_val = match canon {
            ENode::Const(c) => Some(c),
            _ => None,
        };
        self.classes.push(EClass { ty, bits, nodes, const_val });
        self.total_nodes += 1;
        Some(id)
    }

    /// Merges two e-classes of the same type, returning `true` if a merge occurred.
    fn union(&mut self, a: EId, b: EId) -> bool {
        let ra = self.uf.find(a);
        let rb = self.uf.find(b);
        if ra == rb {
            return false;
        }
        let keep = self.uf.union(ra, rb);
        let drop = if keep == ra { rb } else { ra };
        let drop_const = self.classes.get(to_idx(drop)).and_then(|c| c.const_val);
        if let Some(kc) = self.classes.get_mut(to_idx(keep))
            && kc.const_val.is_none()
        {
            kc.const_val = drop_const;
        }
        // Move nodes from dropped class into kept class (deduplicated).
        let mut moved: Vec<ENode, A> = Vec::new_in(self.alloc);
        if let Some(dc) = self.classes.get_mut(to_idx(drop)) {
            core::mem::swap(&mut moved, &mut dc.nodes);
        }
        if let Some(kc) = self.classes.get_mut(to_idx(keep)) {
            for n in moved {
                if kc.nodes.scan_for(&n).is_none() {
                    kc.nodes.push(n);
                }
            }
        }
        true
    }

    /// Re-canonicalizes all e-nodes after unions and merges congruence-equivalent classes.
    fn rebuild(&mut self) {
        loop {
            let mut merged_any = false;
            for cid in 0..self.classes.len() {
                let cid_e = to_u32(cid);
                let root = self.uf.find(cid_e);
                if root != cid_e {
                    continue;
                }
                let mut i = 0;
                while let Some(old) = self.classes.get(cid).and_then(|c| c.nodes.get(i).copied()) {
                    let new = old.canonicalize(&mut self.uf);
                    if new != old {
                        if let Some(cls) = self.classes.get_mut(cid) {
                            if cls.nodes.scan_for(&new).is_some() {
                                cls.nodes.remove_swap(i);
                                continue;
                            }
                            set_at(&mut cls.nodes, i, new);
                        }
                    }
                    i += 1;
                }
            }
            // Congruence closure: if two distinct root classes share an identical canonical e-node + type, union them.
            'outer: for i in 0..self.classes.len() {
                let i_e = to_u32(i);
                if self.uf.find(i_e) != i_e {
                    continue;
                }
                let Some(ty_i) = self.classes.get(i).map(|c| c.ty) else {
                    continue;
                };
                for j in (i + 1)..self.classes.len() {
                    let j_e = to_u32(j);
                    if self.uf.find(j_e) != j_e {
                        continue;
                    }
                    let overlap = match (self.classes.get(i), self.classes.get(j)) {
                        (Some(ci), Some(cj)) if cj.ty == ty_i => {
                            ci.nodes.iter().any(|n| cj.nodes.scan_for(n).is_some())
                        }
                        _ => false,
                    };
                    if overlap {
                        self.union(i_e, j_e);
                        merged_any = true;
                        break 'outer;
                    }
                }
            }
            if !merged_any {
                break;
            }
        }
    }
}

#[inline]
fn mask_of(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// Returns `true` if `val` is a valid AArch64 64-bit `movz`/`movn` single-instruction constant.
#[inline]
fn is_single_mov_const(val: u64) -> bool {
    if val == 0 {
        return true;
    }
    let inv = !val;
    for hw in 0..4u32 {
        let chunk = (val >> (hw * 16)) & 0xffff;
        if val == (chunk << (hw * 16)) {
            return true;
        }
        let ichunk = (inv >> (hw * 16)) & 0xffff;
        if inv == (ichunk << (hw * 16)) {
            return true;
        }
    }
    false
}

/// Evaluates the AArch64 cost (in tenths of cycles) of emitting `node` given minimum child class costs.
///
/// Explicitly models AArch64 fused instruction patterns:
/// - `add(mul(a, b), c)` / `sub(c, mul(a, b))` -> `madd` / `msub`
/// - `add(a, shl/lsr(b, k))` / `sub(a, shl(b, k))` -> `add_lsl` / `add_lsr` / `sub_lsl`
/// - `and/or/xor(a, not(b))` -> `bic` / `orn` / `eon`
/// - `add/sub(a, imm12)` -> immediate ALU (0 extra cost for constant)
fn a64_node_cost<A: Allocator>(
    eg: &EGraph<A>,
    node: ENode,
    bits: u32,
    best_cost: &[u32],
    best_node: &[Option<ENode>],
    arch: AArch64MicroArch,
) -> u32 {
    let mul_cost: u32 = match arch {
        AArch64MicroArch::AppleSilicon => 30,
        AArch64MicroArch::NeoverseV2 => 36,
        AArch64MicroArch::X86_64Zen5 => 30,
    };
    let fused_madd_cost: u32 = match arch {
        AArch64MicroArch::AppleSilicon => 32,
        AArch64MicroArch::NeoverseV2 => 38,
        // x86-64 has no scalar integer `madd` (must execute `imul` + `add` = 40).
        AArch64MicroArch::X86_64Zen5 => 40,
    };

    match node {
        ENode::Var(_) => 0,
        ENode::Const(c) => {
            if c <= 4095 {
                2
            } else if is_single_mov_const(c) {
                8
            } else {
                16
            }
        }
        ENode::Unary(Op::Neg | Op::Not, a) => at(best_cost, to_idx(a)).saturating_add(10),
        ENode::Unary(_, a) => at(best_cost, to_idx(a)).saturating_add(12),
        ENode::Binary(op, a, b) => {
            let ai = to_idx(a);
            let bi = to_idx(b);
            let ca = at(best_cost, ai);
            let cb = at(best_cost, bi);
            let const_a = eg.classes.get(ai).and_then(|c| c.const_val);
            let const_b = eg.classes.get(bi).and_then(|c| c.const_val);
            let best_a = at(best_node, ai);
            let best_b = at(best_node, bi);

            // Check AArch64 fused patterns (only pay children's operand cost, NOT intermediate op cost!).
            match op {
                Op::Add => {
                    // Immediate add (`add rd, rn, #imm12`): constant costs 0!
                    if const_b.is_some_and(|v| v <= 4095) {
                        return ca.saturating_add(10);
                    }
                    if const_a.is_some_and(|v| v <= 4095) {
                        return cb.saturating_add(10);
                    }
                    // Fused `add rd, a, b, lsl/lsr #k` (1 cycle total!).
                    if let Some(ENode::Binary(Op::Shl | Op::Lshr, sb, sk)) = best_b
                        && eg.classes.get(to_idx(sk)).and_then(|c| c.const_val).is_some()
                    {
                        return ca.saturating_add(at(best_cost, to_idx(sb))).saturating_add(10);
                    }
                    if let Some(ENode::Binary(Op::Shl | Op::Lshr, sa, sk)) = best_a
                        && eg.classes.get(to_idx(sk)).and_then(|c| c.const_val).is_some()
                    {
                        return cb.saturating_add(at(best_cost, to_idx(sa))).saturating_add(10);
                    }
                    // Fused `madd rd, x, y, acc`.
                    if let Some(ENode::Binary(Op::Mul, mx, my)) = best_a {
                        return at(best_cost, to_idx(mx))
                            .saturating_add(at(best_cost, to_idx(my)))
                            .saturating_add(cb)
                            .saturating_add(fused_madd_cost);
                    }
                    if let Some(ENode::Binary(Op::Mul, mx, my)) = best_b {
                        return at(best_cost, to_idx(mx))
                            .saturating_add(at(best_cost, to_idx(my)))
                            .saturating_add(ca)
                            .saturating_add(fused_madd_cost);
                    }
                    ca.saturating_add(cb).saturating_add(10)
                }
                Op::Sub => {
                    if const_b.is_some_and(|v| v <= 4095) {
                        return ca.saturating_add(10);
                    }
                    // Fused `sub rd, a, b, lsl #k`.
                    if let Some(ENode::Binary(Op::Shl, sb, sk)) = best_b
                        && eg.classes.get(to_idx(sk)).and_then(|c| c.const_val).is_some()
                    {
                        return ca.saturating_add(at(best_cost, to_idx(sb))).saturating_add(10);
                    }
                    // Fused `msub rd, x, y, acc`.
                    if let Some(ENode::Binary(Op::Mul, mx, my)) = best_b {
                        return at(best_cost, to_idx(mx))
                            .saturating_add(at(best_cost, to_idx(my)))
                            .saturating_add(ca)
                            .saturating_add(fused_madd_cost);
                    }
                    ca.saturating_add(cb).saturating_add(10)
                }
                Op::And | Op::Or | Op::Xor => {
                    // Fused `bic` / `orn` / `eon`: `op(a, not(b))` = 1 instruction!
                    if let Some(ENode::Unary(Op::Not, nb)) = best_b {
                        return ca.saturating_add(at(best_cost, to_idx(nb))).saturating_add(10);
                    }
                    if let Some(ENode::Unary(Op::Not, na)) = best_a {
                        return cb.saturating_add(at(best_cost, to_idx(na))).saturating_add(10);
                    }
                    // Low bitmask `x & (2^k - 1)` -> `ubfm` (0 extra const cost).
                    if op == Op::And {
                        let is_mask = |v: u64| v > 0 && v < u64::MAX && v.wrapping_add(1).is_power_of_two();
                        if const_b.is_some_and(is_mask) {
                            return ca.saturating_add(10);
                        }
                        if const_a.is_some_and(is_mask) {
                            return cb.saturating_add(10);
                        }
                    }
                    ca.saturating_add(cb).saturating_add(10)
                }
                Op::Shl | Op::Lshr | Op::Ashr => {
                    // Immediate shift (`lsl/lsr/asr #k`): constant shift amount costs 0!
                    if const_b.is_some() {
                        return ca.saturating_add(10);
                    }
                    ca.saturating_add(cb).saturating_add(10)
                }
                Op::Mul => {
                    let _ = bits;
                    ca.saturating_add(cb).saturating_add(mul_cost)
                }
                _ => ca.saturating_add(cb).saturating_add(12),
            }
        }
    }
}

/// Runs one round of AArch64-oriented rewrite rules across all active e-classes.
fn saturate_round<A: Allocator + Copy>(eg: &mut EGraph<A>) -> u32 {
    let mut unions: Vec<(EId, EId), A> = Vec::new_in(eg.alloc);
    let num_classes = eg.classes.len();

    for cid in 0..num_classes {
        if eg.at_budget() {
            break;
        }
        let cid_e = to_u32(cid);
        if eg.uf.find(cid_e) != cid_e {
            continue;
        }
        let Some((ty, bits, node_len)) = eg.classes.get(cid).map(|c| (c.ty, c.bits, c.nodes.len())) else {
            continue;
        };
        let mask = mask_of(bits);

        // Snapshot nodes in this class to avoid borrow conflict during `eg.add`.
        let mut snap: Vec<ENode, A> = Vec::with_capacity_in(node_len, eg.alloc);
        if let Some(cls) = eg.classes.get(cid) {
            snap.extend_from_slice(&cls.nodes);
        }

        for node in snap {
            if eg.at_budget() {
                break;
            }
            match node {
                ENode::Unary(op, a) => {
                    let ra = eg.uf.find(a);
                    // Constant fold unary.
                    if let Some(ca) = eg.classes.get(to_idx(ra)).and_then(|c| c.const_val) {
                        let res = trunc_u64(unary(op, u128::from(ca), bits)) & mask;
                        if let Some(c_id) = eg.add(ENode::Const(res), ty, bits) {
                            unions.push((cid_e, c_id));
                        }
                    }
                    // Involution: `neg(neg(x)) == x`, `not(not(x)) == x`.
                    let mut inner_snap: Vec<ENode, A> = Vec::new_in(eg.alloc);
                    if let Some(cls) = eg.classes.get(to_idx(ra)) {
                        inner_snap.extend_from_slice(&cls.nodes);
                    }
                    for in_node in inner_snap {
                        if let ENode::Unary(iop, x) = in_node
                            && iop == op
                        {
                            unions.push((cid_e, x));
                        }
                    }
                }
                ENode::Binary(op, a, b) => {
                    let ra = eg.uf.find(a);
                    let rb = eg.uf.find(b);
                    let ca = eg.classes.get(to_idx(ra)).and_then(|c| c.const_val);
                    let cb = eg.classes.get(to_idx(rb)).and_then(|c| c.const_val);

                    // 1. Exact constant folding.
                    if let (Some(va), Some(vb)) = (ca, cb)
                        && let Ok(res) = binary(op, u128::from(va), u128::from(vb), bits)
                        && let Some(c_id) = eg.add(ENode::Const(trunc_u64(res) & mask), ty, bits)
                    {
                        unions.push((cid_e, c_id));
                    }

                    // 2. Algebraic identities & shift-add constant-multiply synthesis.
                    match op {
                        Op::Add => {
                            if cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(0) {
                                unions.push((cid_e, rb));
                            } else if ra == rb {
                                // `x + x == x << 1`
                                if let Some(one) = eg.add(ENode::Const(1), ty, bits)
                                    && let Some(sh) = eg.add(ENode::Binary(Op::Shl, ra, one), ty, bits)
                                {
                                    unions.push((cid_e, sh));
                                }
                            }
                        }
                        Op::Sub => {
                            if cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ra == rb {
                                if let Some(z) = eg.add(ENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if ca == Some(0)
                                && let Some(neg) = eg.add(ENode::Unary(Op::Neg, rb), ty, bits)
                            {
                                unions.push((cid_e, neg));
                            }
                        }
                        Op::Mul => {
                            if ca == Some(0) || cb == Some(0) {
                                if let Some(z) = eg.add(ENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if cb == Some(1) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(1) {
                                unions.push((cid_e, rb));
                            } else {
                                // Synthesize AArch64 shift-add sequences for constant multipliers!
                                let pair = if let Some(c) = cb {
                                    Some((ra, c))
                                } else if let Some(c) = ca {
                                    Some((rb, c))
                                } else {
                                    None
                                };
                                if let Some((x, c)) = pair {
                                    if c > 1 && c.is_power_of_two() {
                                        let k = c.trailing_zeros();
                                        if k < bits
                                            && let Some(kc) = eg.add(ENode::Const(u64::from(k)), ty, bits)
                                            && let Some(sh) = eg.add(ENode::Binary(Op::Shl, x, kc), ty, bits)
                                        {
                                            unions.push((cid_e, sh));
                                        }
                                    } else if c > 2 && (c - 1).is_power_of_two() {
                                        // `x * (2^k + 1) -> add(x, shl(x, k))` = ONE AArch64 `add_lsl`!
                                        let k = (c - 1).trailing_zeros();
                                        if k < bits
                                            && let Some(kc) = eg.add(ENode::Const(u64::from(k)), ty, bits)
                                            && let Some(sh) = eg.add(ENode::Binary(Op::Shl, x, kc), ty, bits)
                                            && let Some(sum) = eg.add(ENode::Binary(Op::Add, x, sh), ty, bits)
                                        {
                                            unions.push((cid_e, sum));
                                        }
                                    } else if c >= 7 && c < u64::MAX && (c + 1).is_power_of_two() {
                                        // `x * (2^k - 1) -> sub(shl(x, k), x)` (2 single-cycle ALU ops vs mov+mul!)
                                        let k = (c + 1).trailing_zeros();
                                        if k < bits
                                            && let Some(kc) = eg.add(ENode::Const(u64::from(k)), ty, bits)
                                            && let Some(sh) = eg.add(ENode::Binary(Op::Shl, x, kc), ty, bits)
                                            && let Some(diff) = eg.add(ENode::Binary(Op::Sub, sh, x), ty, bits)
                                        {
                                            unions.push((cid_e, diff));
                                        }
                                    } else if c > 4 && c % 2 == 0 {
                                        // `x * ((2^k + 1) << m)` e.g. 6, 10, 12, 18, 20, 24, 36, 40 -> `(x + (x << k)) << m`!
                                        let m = c.trailing_zeros();
                                        let odd = c >> m;
                                        if odd > 2 && (odd - 1).is_power_of_two() {
                                            let k = (odd - 1).trailing_zeros();
                                            if k < bits
                                                && m < bits
                                                && let (Some(kc), Some(mc)) = (
                                                    eg.add(ENode::Const(u64::from(k)), ty, bits),
                                                    eg.add(ENode::Const(u64::from(m)), ty, bits),
                                                )
                                                && let Some(sh1) = eg.add(ENode::Binary(Op::Shl, x, kc), ty, bits)
                                                && let Some(sum) = eg.add(ENode::Binary(Op::Add, x, sh1), ty, bits)
                                                && let Some(sh2) = eg.add(ENode::Binary(Op::Shl, sum, mc), ty, bits)
                                            {
                                                unions.push((cid_e, sh2));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Op::And => {
                            if ca == Some(0) || cb == Some(0) {
                                if let Some(z) = eg.add(ENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if ra == rb || cb == Some(mask) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(mask) {
                                unions.push((cid_e, rb));
                            }
                        }
                        Op::Or => {
                            if ca == Some(mask) || cb == Some(mask) {
                                if let Some(m) = eg.add(ENode::Const(mask), ty, bits) {
                                    unions.push((cid_e, m));
                                }
                            } else if ra == rb || cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(0) {
                                unions.push((cid_e, rb));
                            }
                        }
                        Op::Xor => {
                            if ra == rb {
                                if let Some(z) = eg.add(ENode::Const(0), ty, bits) {
                                    unions.push((cid_e, z));
                                }
                            } else if cb == Some(0) {
                                unions.push((cid_e, ra));
                            } else if ca == Some(0) {
                                unions.push((cid_e, rb));
                            } else if cb == Some(mask) {
                                if let Some(nt) = eg.add(ENode::Unary(Op::Not, ra), ty, bits) {
                                    unions.push((cid_e, nt));
                                }
                            } else if ca == Some(mask)
                                && let Some(nt) = eg.add(ENode::Unary(Op::Not, rb), ty, bits)
                            {
                                unions.push((cid_e, nt));
                            }
                        }
                        Op::Shl | Op::Lshr | Op::Ashr => {
                            if let Some(sh) = cb
                                && (sh % u64::from(bits)) == 0
                            {
                                unions.push((cid_e, ra));
                            }
                        }
                        _ => {}
                    }

                    // 3. Structural multi-node rewrites: factoring, associativity, De Morgan, cancellation.
                    let mut a_nodes: Vec<ENode, A> = Vec::new_in(eg.alloc);
                    let mut b_nodes: Vec<ENode, A> = Vec::new_in(eg.alloc);
                    if let Some(cls) = eg.classes.get(to_idx(ra)) {
                        a_nodes.extend_from_slice(&cls.nodes);
                    }
                    if let Some(cls) = eg.classes.get(to_idx(rb)) {
                        b_nodes.extend_from_slice(&cls.nodes);
                    }

                    match op {
                        Op::Add | Op::Sub => {
                            // Factor common multiplier: `(x * c) +/- (y * c) -> (x +/- y) * c`
                            // Factor common shift: `(x << k) +/- (y << k) -> (x +/- y) << k`
                            for &an in &a_nodes {
                                if eg.at_budget() {
                                    break;
                                }
                                if let ENode::Binary(Op::Mul, x1, x2) = an {
                                    for &bn in &b_nodes {
                                        if let ENode::Binary(Op::Mul, y1, y2) = bn {
                                            let common = if x2 == y2 {
                                                Some((x1, y1, x2))
                                            } else if x1 == y1 {
                                                Some((x2, y2, x1))
                                            } else if x1 == y2 {
                                                Some((x2, y1, x1))
                                            } else if x2 == y1 {
                                                Some((x1, y2, x2))
                                            } else {
                                                None
                                            };
                                            if let Some((lhs, rhs, factor)) = common
                                                && let Some(comb) = eg.add(ENode::Binary(op, lhs, rhs), ty, bits)
                                                && let Some(prod) = eg.add(ENode::Binary(Op::Mul, comb, factor), ty, bits)
                                            {
                                                unions.push((cid_e, prod));
                                            }
                                        }
                                    }
                                } else if let ENode::Binary(Op::Shl, x, k1) = an {
                                    for &bn in &b_nodes {
                                        if let ENode::Binary(Op::Shl, y, k2) = bn
                                            && k1 == k2
                                            && let Some(comb) = eg.add(ENode::Binary(op, x, y), ty, bits)
                                            && let Some(sh) = eg.add(ENode::Binary(Op::Shl, comb, k1), ty, bits)
                                        {
                                            unions.push((cid_e, sh));
                                        }
                                    }
                                }
                            }
                            // Cancellation: `(x + y) - y == x`, `(x - y) + y == x`.
                            if op == Op::Sub {
                                for &an in &a_nodes {
                                    if let ENode::Binary(Op::Add, x, y) = an {
                                        if y == rb {
                                            unions.push((cid_e, x));
                                        } else if x == rb {
                                            unions.push((cid_e, y));
                                        }
                                    }
                                }
                            } else {
                                for &an in &a_nodes {
                                    if let ENode::Binary(Op::Sub, x, y) = an
                                        && y == rb
                                    {
                                        unions.push((cid_e, x));
                                    }
                                }
                                for &bn in &b_nodes {
                                    if let ENode::Binary(Op::Sub, x, y) = bn
                                        && y == ra
                                    {
                                        unions.push((cid_e, x));
                                    }
                                }
                            }
                        }
                        Op::And => {
                            // De Morgan: `(!x) & (!y) -> !(x | y)`.
                            for &an in &a_nodes {
                                if let ENode::Unary(Op::Not, x) = an {
                                    for &bn in &b_nodes {
                                        if let ENode::Unary(Op::Not, y) = bn
                                            && let Some(or_xy) = eg.add(ENode::Binary(Op::Or, x, y), ty, bits)
                                            && let Some(not_or) = eg.add(ENode::Unary(Op::Not, or_xy), ty, bits)
                                        {
                                            unions.push((cid_e, not_or));
                                        }
                                    }
                                }
                            }
                            // Absorption: `x & (x | y) == x`.
                            for &bn in &b_nodes {
                                if let ENode::Binary(Op::Or, x, y) = bn
                                    && (x == ra || y == ra)
                                {
                                    unions.push((cid_e, ra));
                                }
                            }
                        }
                        Op::Or => {
                            // De Morgan: `(!x) | (!y) -> !(x & y)`.
                            for &an in &a_nodes {
                                if let ENode::Unary(Op::Not, x) = an {
                                    for &bn in &b_nodes {
                                        if let ENode::Unary(Op::Not, y) = bn
                                            && let Some(and_xy) = eg.add(ENode::Binary(Op::And, x, y), ty, bits)
                                            && let Some(not_and) = eg.add(ENode::Unary(Op::Not, and_xy), ty, bits)
                                        {
                                            unions.push((cid_e, not_and));
                                        }
                                    }
                                }
                            }
                            // Absorption: `x | (x & y) == x`.
                            for &bn in &b_nodes {
                                if let ENode::Binary(Op::And, x, y) = bn
                                    && (x == ra || y == ra)
                                {
                                    unions.push((cid_e, ra));
                                }
                            }
                        }
                        Op::Xor => {
                            // Cancellation: `(x ^ y) ^ y == x`.
                            for &an in &a_nodes {
                                if let ENode::Binary(Op::Xor, x, y) = an {
                                    if y == rb {
                                        unions.push((cid_e, x));
                                    } else if x == rb {
                                        unions.push((cid_e, y));
                                    }
                                }
                            }
                            for &bn in &b_nodes {
                                if let ENode::Binary(Op::Xor, x, y) = bn {
                                    if y == ra {
                                        unions.push((cid_e, x));
                                    } else if x == ra {
                                        unions.push((cid_e, y));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    let mut applied = 0u32;
    for (a, b) in unions {
        if eg.union(a, b) {
            applied += 1;
        }
    }
    if applied > 0 {
        eg.rebuild();
    }
    applied
}

#[inline]
fn is_superopt_pure(op: Op, ty: Ty, flags: Flags, ptr_bits: u32) -> Option<u32> {
    if op.info().flags.meanings_only(flags).0 != 0 {
        return None;
    }
    if !(ty.is_scalar() && ty.is_int()) {
        return None;
    }
    let bits = ty.bits(ptr_bits);
    if bits != 32 && bits != 64 {
        return None;
    }
    match op {
        Op::Iconst
        | Op::IconstWide
        | Op::Neg
        | Op::Not
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Lshr
        | Op::Ashr => Some(bits),
        _ => None,
    }
}

fn push_const<A: Allocator + Copy>(func: &mut Func<A>, ty: Ty, ptr_bits: u32, value: u64, loc: u32) -> u32 {
    let bits = ty.bits(ptr_bits);
    let idx = to_u32(func.ops.len());
    if bits <= 32 {
        func.ops.push(op_byte(Op::Iconst));
        func.flags.push(0);
        func.tys.push(ty.0);
        func.data.push(trunc_u32(value));
        func.attrs.push(0);
        func.locs.push(loc);
        return idx;
    }
    match i32::try_from(sext(u128::from(value), bits)) {
        Ok(small) => {
            func.ops.push(op_byte(Op::Iconst));
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(small.cast_unsigned());
            func.attrs.push(0);
            func.locs.push(loc);
        }
        Err(_) => {
            let start = to_u32(func.extra.len());
            func.extra.push(trunc_u32(value));
            func.extra.push(trunc_u32(value >> 32));
            func.ops.push(op_byte(Op::IconstWide));
            func.flags.push(0);
            func.tys.push(ty.0);
            func.data.push(start);
            func.attrs.push(0);
            func.locs.push(loc);
        }
    }
    idx
}

fn copy_inst<A: Allocator + Copy>(
    view: &View<'_>,
    inst: usize,
    val_map: &[u32],
    func: &mut Func<A>,
) -> Result<u32, SimplifyError> {
    let op = view.op(inst);
    let flags = at(view.flags, inst);
    let ty = at(view.tys, inst);
    let attrs = at(view.attrs, inst);
    let loc = at(view.locs, inst);
    let map_val = |v: u32| -> Result<u32, SimplifyError> {
        let mapped = val_map.get(to_idx(v)).copied().unwrap_or(NONE);
        if mapped == NONE {
            Err(SimplifyError::Refused { inst, what: "unmapped operand in superopt" })
        } else {
            Ok(mapped)
        }
    };
    let data = match op.info().layout {
        Layout::None => 0,
        Layout::Data(Field::Value) => map_val(at(view.data, inst))?,
        Layout::Data(_) => at(view.data, inst),
        Layout::Extra(fields) => {
            let start = to_u32(func.extra.len());
            let words = view.extra;
            let mut pos = to_idx(at(view.data, inst));
            let mut scrutinee = None;
            for &field in fields {
                match field {
                    Field::Value => {
                        let w = at(words, pos);
                        scrutinee = Some(w);
                        func.extra.push(map_val(w)?);
                        pos += 1;
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
                        func.extra.push(at(words, pos));
                        pos += 1;
                    }
                    Field::Target => {
                        let block = at(words, pos);
                        let n_u32 = at(words, pos + 1);
                        let n = to_idx(n_u32);
                        func.extra.push(block);
                        func.extra.push(n_u32);
                        for k in 0..n {
                            func.extra.push(map_val(at(words, pos + 2 + k))?);
                        }
                        pos += 2 + n;
                    }
                    Field::Values => {
                        let n_u32 = at(words, pos);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        for k in 0..n {
                            func.extra.push(map_val(at(words, pos + 1 + k))?);
                        }
                        pos += 1 + n;
                    }
                    Field::Blocks | Field::Mask => {
                        let n = to_idx(at(words, pos));
                        func.extra.extend_from_slice(words.get(pos..pos + 1 + n).unwrap_or(&[]));
                        pos += 1 + n;
                    }
                    Field::Const => {
                        let n = to_idx(Ty(ty).const_words());
                        func.extra.extend_from_slice(words.get(pos..pos + n).unwrap_or(&[]));
                        pos += n;
                    }
                    Field::Cases => {
                        let width = to_idx(scrutinee.map_or(0, |it: u32| Ty(at(view.tys, to_idx(it))).const_words()));
                        let n_u32 = at(words, pos);
                        let n = to_idx(n_u32);
                        func.extra.push(n_u32);
                        pos += 1;
                        for _ in 0..n {
                            func.extra.extend_from_slice(words.get(pos..pos + width).unwrap_or(&[]));
                            pos += width;
                            let block = at(words, pos);
                            let argc_u32 = at(words, pos + 1);
                            let argc = to_idx(argc_u32);
                            func.extra.push(block);
                            func.extra.push(argc_u32);
                            for k in 0..argc {
                                func.extra.push(map_val(at(words, pos + 2 + k))?);
                            }
                            pos += 2 + argc;
                        }
                    }
                }
            }
            start
        }
    };
    let out_idx = to_u32(func.ops.len());
    func.ops.push(at(view.ops, inst));
    func.flags.push(flags);
    func.tys.push(ty);
    func.data.push(data);
    func.attrs.push(attrs);
    func.locs.push(loc);
    Ok(out_idx)
}

enum MatFrame {
    Enter(EId),
    FinishUnary { root: EId, op: Op, ty: Ty },
    FinishBinary { root: EId, op: Op, ty: Ty },
}

fn materialize_class<A: Allocator + Copy>(
    cid: EId,
    eg: &mut EGraph<A>,
    best_node: &[Option<ENode>],
    class_emitted: &mut [u32],
    visiting: &mut [bool],
    val_map: &[u32],
    func: &mut Func<A>,
    ptr_bits: u32,
    loc: u32,
    alloc: A,
) -> Option<u32> {
    let mut frames: Vec<MatFrame, A> = Vec::with_capacity_in(32, alloc);
    let mut results: Vec<u32, A> = Vec::with_capacity_in(32, alloc);
    let mut active: Vec<EId, A> = Vec::with_capacity_in(32, alloc);
    frames.push(MatFrame::Enter(cid));

    while let Some(frame) = frames.pop() {
        match frame {
            MatFrame::Enter(cur) => {
                let root = eg.uf.find(cur);
                let ri = to_idx(root);
                let existing = class_emitted.get(ri).copied().unwrap_or(NONE);
                if existing != NONE {
                    results.push(existing);
                    continue;
                }
                if at(visiting, ri) {
                    for &v in &active {
                        set_at(visiting, to_idx(v), false);
                    }
                    return None;
                }
                let Some(node) = at(best_node, ri) else {
                    for &v in &active {
                        set_at(visiting, to_idx(v), false);
                    }
                    return None;
                };
                let Some(ty) = eg.classes.get(ri).map(|c| c.ty) else {
                    for &v in &active {
                        set_at(visiting, to_idx(v), false);
                    }
                    return None;
                };
                match node {
                    ENode::Var(orig_v) => {
                        let mapped = val_map.get(to_idx(orig_v)).copied().unwrap_or(NONE);
                        if mapped == NONE {
                            for &v in &active {
                                set_at(visiting, to_idx(v), false);
                            }
                            return None;
                        }
                        set_at(class_emitted, ri, mapped);
                        results.push(mapped);
                    }
                    ENode::Const(c) => {
                        let id = push_const(func, ty, ptr_bits, c, loc);
                        set_at(class_emitted, ri, id);
                        results.push(id);
                    }
                    ENode::Unary(op, a) => {
                        set_at(visiting, ri, true);
                        active.push(root);
                        frames.push(MatFrame::FinishUnary { root, op, ty });
                        frames.push(MatFrame::Enter(a));
                    }
                    ENode::Binary(op, a, b) => {
                        set_at(visiting, ri, true);
                        active.push(root);
                        frames.push(MatFrame::FinishBinary { root, op, ty });
                        frames.push(MatFrame::Enter(b));
                        frames.push(MatFrame::Enter(a));
                    }
                }
            }
            MatFrame::FinishUnary { root, op, ty } => {
                let Some(ma) = results.pop() else {
                    for &v in &active {
                        set_at(visiting, to_idx(v), false);
                    }
                    return None;
                };
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(op));
                func.flags.push(0);
                func.tys.push(ty.0);
                func.data.push(ma);
                func.attrs.push(0);
                func.locs.push(loc);
                let ri = to_idx(root);
                set_at(visiting, ri, false);
                let _ = active.pop();
                set_at(class_emitted, ri, idx);
                results.push(idx);
            }
            MatFrame::FinishBinary { root, op, ty } => {
                let (Some(mb), Some(ma)) = (results.pop(), results.pop()) else {
                    for &v in &active {
                        set_at(visiting, to_idx(v), false);
                    }
                    return None;
                };
                let start = to_u32(func.extra.len());
                func.extra.push(ma);
                func.extra.push(mb);
                let idx = to_u32(func.ops.len());
                func.ops.push(op_byte(op));
                func.flags.push(0);
                func.tys.push(ty.0);
                func.data.push(start);
                func.attrs.push(0);
                func.locs.push(loc);
                let ri = to_idx(root);
                set_at(visiting, ri, false);
                let _ = active.pop();
                set_at(class_emitted, ri, idx);
                results.push(idx);
            }
        }
    }

    results.pop()
}

/// Runs bounded multi-round AArch64 e-graph equality saturation & microarchitectural cost-model
/// extraction over `view`.
///
/// `is_hot` selects which basic blocks to superoptimize when `opts.hot_only` is `true` (e.g.
/// inner loops or PGO-hot blocks). When `opts.hot_only` is `false`, all blocks are eligible.
pub fn superopt_a64<A: Allocator + Copy>(
    view: View<'_>,
    opts: SuperoptOpts,
    mut is_hot: impl FnMut(u32) -> bool,
    alloc: A,
) -> Result<SuperoptOutput<A>, SimplifyError> {
    let len = view.len();
    let ptr_bits = view.head.ptr_bits;
    if view.blocks() == 0 {
        return Ok(SuperoptOutput { func: view.to_func_in(alloc), stats: SuperoptStats::default() });
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
    let mut stats = SuperoptStats::default();
    let mut any_rewritten = false;

    for b in 0..view.blocks() {
        let b_u32 = to_u32(b);
        func.block_starts.push(to_u32(func.ops.len()));
        func.block_kinds.push(at(view.block_kinds, b));
        let range = view.block(Block(b_u32));

        let run_block = !opts.hot_only || is_hot(b_u32);
        if !run_block {
            for inst in range.start..range.end {
                let copied = copy_inst(&view, inst, &val_map, &mut func)?;
                set_at(&mut val_map, inst, copied);
            }
            continue;
        }

        // Count pure arithmetic ops in this block; skip trivial blocks with < 1 arithmetic op.
        let mut pure_arith = 0u32;
        for inst in range.start..range.end {
            let op = view.op(inst);
            let ty = Ty(at(view.tys, inst));
            if is_superopt_pure(op, ty, Flags(at(view.flags, inst)), ptr_bits).is_some()
                && !matches!(op, Op::Iconst | Op::IconstWide)
            {
                pure_arith += 1;
            }
        }
        if pure_arith == 0 {
            for inst in range.start..range.end {
                let copied = copy_inst(&view, inst, &val_map, &mut func)?;
                set_at(&mut val_map, inst, copied);
            }
            continue;
        }

        stats.blocks_superopted += 1;

        // 1. Build bounded E-Graph for all pure scalar integer ops in this block.
        let mut eg = EGraph::new(opts.max_enodes.max(64), alloc);
        let mut inst_eid: Vec<u32, A> = filled(range.len(), NONE, &alloc);
        let mut orig_node: Vec<Option<ENode>, A> = filled(range.len(), None, &alloc);

        // Helper to resolve an operand `v` to an EId (either in-block pure op or external `Var`).
        let resolve_operand = |eg: &mut EGraph<A>, inst_eid: &[u32], v: u32, ty: Ty, bits: u32| -> Option<EId> {
            let vi = to_idx(v);
            if vi >= range.start && vi < range.end {
                let eid = inst_eid.get(vi - range.start).copied().unwrap_or(NONE);
                if eid != NONE {
                    return Some(eg.uf.find(eid));
                }
            }
            eg.add(ENode::Var(v), ty, bits)
        };

        for inst in range.start..range.end {
            let rel = inst - range.start;
            let op = view.op(inst);
            let ty = Ty(at(view.tys, inst));
            let Some(bits) = is_superopt_pure(op, ty, Flags(at(view.flags, inst)), ptr_bits) else {
                continue;
            };
            let mask = mask_of(bits);
            let node_opt = match op {
                Op::Iconst => {
                    let v = trunc_u64(iconst_value(at(view.data, inst), bits)) & mask;
                    Some(ENode::Const(v))
                }
                Op::IconstWide => {
                    let idx = to_idx(at(view.data, inst));
                    let w = to_idx(ty.const_words());
                    let v = trunc_u64(wide_value(view.extra.get(idx..idx + w).unwrap_or(&[]), bits)) & mask;
                    Some(ENode::Const(v))
                }
                Op::Neg | Op::Not => {
                    let src = at(view.data, inst);
                    resolve_operand(&mut eg, &inst_eid, src, ty, bits).map(|a| ENode::Unary(op, a))
                }
                Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor | Op::Shl | Op::Lshr | Op::Ashr => {
                    let idx = to_idx(at(view.data, inst));
                    let lhs = at(view.extra, idx);
                    let rhs = at(view.extra, idx + 1);
                    match (
                        resolve_operand(&mut eg, &inst_eid, lhs, ty, bits),
                        resolve_operand(&mut eg, &inst_eid, rhs, ty, bits),
                    ) {
                        (Some(a), Some(bv)) => Some(ENode::Binary(op, a, bv)),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(n) = node_opt
                && let Some(eid) = eg.add(n, ty, bits)
            {
                set_at(&mut inst_eid, rel, eid);
                set_at(&mut orig_node, rel, Some(n));
            }
        }

        // Compute pre-saturation baseline cost for each class.
        let pre_classes = eg.classes.len();
        let mut orig_cost: Vec<u32, A> = filled(pre_classes, u32::MAX / 4, &alloc);
        let mut orig_best: Vec<Option<ENode>, A> = filled(pre_classes, None, &alloc);
        for _ in 0..8 {
            let mut changed = false;
            for cid in 0..pre_classes {
                let cid_e = to_u32(cid);
                if eg.uf.find(cid_e) != cid_e {
                    continue;
                }
                let Some(cls) = eg.classes.get(cid) else {
                    continue;
                };
                let bits = cls.bits;
                for &n in &cls.nodes {
                    let c = a64_node_cost(&eg, n, bits, &orig_cost, &orig_best, opts.microarch);
                    if c < at(&orig_cost, cid) {
                        set_at(&mut orig_cost, cid, c);
                        set_at(&mut orig_best, cid, Some(n));
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // 2. Bounded multi-round equality saturation.
        for _ in 0..opts.max_iters {
            let applied = saturate_round(&mut eg);
            stats.rewrites_applied += applied;
            if applied == 0 || eg.at_budget() {
                break;
            }
        }
        stats.enodes_explored += eg.total_nodes;

        // 3. Extract minimum AArch64-cost e-node for every root e-class (Bellman-Ford fixed point).
        let n_classes = eg.classes.len();
        let mut best_cost: Vec<u32, A> = filled(n_classes, u32::MAX / 4, &alloc);
        let mut best_node: Vec<Option<ENode>, A> = filled(n_classes, None, &alloc);
        for _ in 0..16 {
            let mut changed = false;
            for cid in 0..n_classes {
                let cid_e = to_u32(cid);
                if eg.uf.find(cid_e) != cid_e {
                    continue;
                }
                let Some(cls) = eg.classes.get(cid) else {
                    continue;
                };
                let bits = cls.bits;
                for &n in &cls.nodes {
                    let c = a64_node_cost(&eg, n, bits, &best_cost, &best_node, opts.microarch);
                    if c < at(&best_cost, cid) {
                        set_at(&mut best_cost, cid, c);
                        set_at(&mut best_node, cid, Some(n));
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // 4. Emit instructions: for pure ops whose extracted e-class strictly beats baseline cost,
        //    materialize the optimal extracted DAG (memoized per e-class so shared subexpressions emit once!).
        let mut class_emitted: Vec<u32, A> = filled(n_classes, NONE, &alloc);
        let mut visiting: Vec<bool, A> = filled(n_classes, false, &alloc);

        for inst in range.start..range.end {
            let rel = inst - range.start;
            let eid = inst_eid.get(rel).copied().unwrap_or(NONE);
            if eid != NONE && !matches!(view.op(inst), Op::Iconst | Op::IconstWide) {
                let root = eg.uf.find(eid);
                let eid_idx = to_idx(eid);
                let before = if eid_idx < pre_classes { at(&orig_cost, eid_idx) } else { u32::MAX / 4 };
                let after = at(&best_cost, to_idx(root));
                if after < before
                    && let Some(new_val) = materialize_class(
                        root,
                        &mut eg,
                        &best_node,
                        &mut class_emitted,
                        &mut visiting,
                        &val_map,
                        &mut func,
                        ptr_bits,
                        at(view.locs, inst),
                        alloc,
                    )
                {
                    set_at(&mut val_map, inst, new_val);
                    stats.cost_before += before;
                    stats.cost_after += after;
                    any_rewritten = true;
                    continue;
                }
            }
            let copied = copy_inst(&view, inst, &val_map, &mut func)?;
            set_at(&mut val_map, inst, copied);
            if eid != NONE {
                let root = eg.uf.find(eid);
                let ri = to_idx(root);
                if class_emitted.get(ri).copied().unwrap_or(NONE) == NONE {
                    set_at(&mut class_emitted, ri, copied);
                }
            }
        }
    }

    if !any_rewritten {
        return Ok(SuperoptOutput { func: view.to_func_in(alloc), stats });
    }
    verify(func.view()).map_err(SimplifyError::Verify)?;
    let cleaned = dead(func.view(), alloc)?;
    Ok(SuperoptOutput { func: cleaned, stats })
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        builder::{Builder, Full, Param, Variadic},
        eval::{self, Outcome, Val},
        func::{BlockKind, Conv},
    };

    #[test]
    fn superopt_synthesizes_shift_add_multiply_and_factors_distributive_sum() {
        // Compute `(a * 10) + (b * 10)` -> factors to `(a + b) * 10` -> synthesizes `((a + b) + ((a + b) << 2)) << 1`!
        // Zero `Op::Mul` left in the function, strictly lower AArch64 cycle cost, exact evaluator equivalence!
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let ten = b.iconst(Ty::I64, 10);
        let p1 = b.binary(Op::Mul, Flags::NONE, a, ten);
        let p2 = b.binary(Op::Mul, Flags::NONE, bv, ten);
        let sum = b.binary(Op::Add, Flags::NONE, p1, p2);
        b.ret(&[sum]);

        let func = b.finish_checked().unwrap();
        let out = superopt_a64(func.view(), SuperoptOpts::apple_silicon(), |_| true, alloc).unwrap();

        assert!(out.stats.cost_after < out.stats.cost_before, "expected cost drop: {:?}", out.stats);
        let mut ops: Vec<Op, TestHeap> = Vec::with_capacity_in(out.func.ops.len(), alloc);
        for i in 0..out.func.ops.len() {
            ops.push(out.func.view().op(i));
        }
        assert!(!ops.as_slice().contains(&Op::Mul), "expected Mul-by-10 factored & synthesized to shift-add: {ops:?}");

        for &(av, bval) in &[(0u64, 0u64), (1, 2), (7, 13), (u64::MAX, 42), (123_456, 789_012)] {
            let args = [Val::Int(av.into()), Val::Int(bval.into())];
            let r_before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let r_after = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(r_before, r_after, "superopt diverged on ({av}, {bval})");
            match r_after {
                Outcome::Returned(vals) => {
                    let expected = av.wrapping_add(bval).wrapping_mul(10);
                    assert_eq!(vals.as_slice(), &[Val::Int(expected.into())]);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn superopt_applies_de_morgan_and_shift_factoring() {
        // Compute `((!a) & (!b)) ^ (((x << 3) + (y << 3)) - (y << 3))` -> `!(a | b) ^ (x << 3)`
        let alloc = TestHeap;
        let mut b = Builder::<TestHeap, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let x = b.param(Ty::I64);
        let y = b.param(Ty::I64);
        let na = b.unary(Op::Not, Flags::NONE, a);
        let nb = b.unary(Op::Not, Flags::NONE, bv);
        let dm = b.binary(Op::And, Flags::NONE, na, nb);
        let three = b.iconst(Ty::I64, 3);
        let sx = b.binary(Op::Shl, Flags::NONE, x, three);
        let sy = b.binary(Op::Shl, Flags::NONE, y, three);
        let ssum = b.binary(Op::Add, Flags::NONE, sx, sy);
        let scancel = b.binary(Op::Sub, Flags::NONE, ssum, sy);
        let res = b.binary(Op::Xor, Flags::NONE, dm, scancel);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();
        let out = superopt_a64(func.view(), SuperoptOpts::apple_silicon(), |_| true, alloc).unwrap();
        assert!(out.stats.cost_after < out.stats.cost_before, "expected cost drop: {:?}", out.stats);
        assert!(out.func.ops.len() < func.ops.len(), "expected fewer ops after superopt");

        for &(av, bval, xv, yv) in &[(0u64, 0u64, 1u64, 2u64), (0xff00, 0x0f0f, 17, 99), (u64::MAX, 123, 456, 789)] {
            let args = [Val::Int(av.into()), Val::Int(bval.into()), Val::Int(xv.into()), Val::Int(yv.into())];
            let r_before = eval::run_in(func.view(), &args, 10_000, alloc).unwrap();
            let r_after = eval::run_in(out.func.view(), &args, 10_000, alloc).unwrap();
            assert_eq!(r_before, r_after, "superopt diverged");
        }
    }
}
