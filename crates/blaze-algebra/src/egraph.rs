//! Bounded E-Graph Equality Saturation simplifier (`EGraph<A>`, `simplify`, `simplify_with`).
//!
//! Represents equivalence classes of expression nodes with a union-find structure, applies
//! sound algebraic/trig/log/exp/power/distributive/factoring rewrites up to a strict node &
//! iteration budget (`bounded-progress`, rule 18), and extracts the minimum-cost DAG back into
//! [`ExprGraph`].

use core::alloc::Allocator;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec as OwnVec;

use crate::{
    AlgebraError, ExprGraph, ExprId, Op, VarId, canonical_f64_bits, det_tanh, u32_to_usize,
    usize_to_u32,
};

/// E-class identifier inside an [`EGraph`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct EClassId(pub u32);

/// One e-node whose children are [`EClassId`]s rather than concrete [`ExprId`]s.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ENode {
    Const(u64),
    Var(VarId),
    Neg(EClassId),
    Add(EClassId, EClassId),
    Sub(EClassId, EClassId),
    Mul(EClassId, EClassId),
    Div(EClassId, EClassId),
    Pow(EClassId, EClassId),
    Sin(EClassId),
    Cos(EClassId),
    Tan(EClassId),
    Exp(EClassId),
    Ln(EClassId),
    Sqrt(EClassId),
    Tanh(EClassId),
    Abs(EClassId),
    Select(EClassId, EClassId, EClassId),
    Fma(EClassId, EClassId, EClassId),
}

/// Floating-point rewrite soundness policy (`autodiff.md`, "Floating point").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteMode {
    /// Only identities that preserve IEEE-754 bit-exact semantics (`--x`, `x * 1`, `x / 1`,
    /// `x^1`, `x^2 = x*x`, commutativity of `+`/`*`, `select(c, x, x) -> x`, constant folding).
    /// Never applies `x * 0 -> 0`, `x - x -> 0`, `x / x -> 1`, reassociation, or FMA fusion.
    IeeeExact,
    /// Real-algebraic rewrites (factoring, associativity, `x * 0 -> 0`, `x / x -> 1`,
    /// `ln(exp(x)) -> x`, `sin² + cos² -> 1`, and FMA fusion under [`CostModel::CpuFma`] /
    /// [`CostModel::GpuTranscendental`]).
    FastMath,
}

/// Target extraction cost model for e-graph DAG extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostModel {
    /// General CPU cost model (default).
    Balanced,
    /// FMA-preferring CPU model (`a * b + c` fused into `fma(a, b, c)` at cost 4 vs 7).
    CpuFma,
    /// GPU/SPIR-V model heavily penalizing transcendentals (`sin`/`cos`/`exp`/`ln` = 32) and
    /// division (16) while favoring FMA (4).
    GpuTranscendental,
}

impl ENode {
    #[inline]
    fn as_const(self) -> Option<f64> {
        match self {
            Self::Const(b) => Some(f64::from_bits(b)),
            _ => None,
        }
    }

    #[inline]
    fn op_cost(self, model: CostModel) -> u32 {
        match model {
            CostModel::Balanced => match self {
                Self::Const(_) | Self::Var(_) => 1,
                Self::Neg(_) | Self::Abs(_) => 2,
                Self::Add(_, _) | Self::Sub(_, _) => 3,
                Self::Mul(_, _) => 4,
                Self::Fma(_, _, _) => 5,
                Self::Div(_, _) => 6,
                Self::Select(_, _, _) => 5,
                Self::Sqrt(_) => 8,
                Self::Pow(_, _) => 10,
                Self::Sin(_) | Self::Cos(_) | Self::Tan(_) | Self::Exp(_) | Self::Ln(_) | Self::Tanh(_) => 12,
            },
            CostModel::CpuFma => match self {
                Self::Const(_) | Self::Var(_) => 1,
                Self::Neg(_) | Self::Abs(_) => 2,
                Self::Add(_, _) | Self::Sub(_, _) => 3,
                Self::Mul(_, _) | Self::Fma(_, _, _) => 4,
                Self::Select(_, _, _) => 5,
                Self::Div(_, _) => 8,
                Self::Sqrt(_) => 10,
                Self::Pow(_, _) => 14,
                Self::Sin(_) | Self::Cos(_) | Self::Tan(_) | Self::Exp(_) | Self::Ln(_) | Self::Tanh(_) => 16,
            },
            CostModel::GpuTranscendental => match self {
                Self::Const(_) | Self::Var(_) => 1,
                Self::Neg(_) | Self::Abs(_) => 2,
                Self::Add(_, _) | Self::Sub(_, _) => 3,
                Self::Mul(_, _) | Self::Fma(_, _, _) => 4,
                Self::Select(_, _, _) => 5,
                Self::Sqrt(_) => 12,
                Self::Div(_, _) => 16,
                Self::Pow(_, _) => 24,
                Self::Sin(_) | Self::Cos(_) | Self::Tan(_) | Self::Exp(_) | Self::Ln(_) | Self::Tanh(_) => 32,
            },
        }
    }
}

/// Options bounding e-graph saturation (`options-struct`, rule 83; `bounded-progress`, rule 18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimplifyOpts {
    pub max_iters: usize,
    pub max_enodes: usize,
    pub mode: RewriteMode,
    pub cost_model: CostModel,
}

impl Default for SimplifyOpts {
    fn default() -> Self {
        Self {
            max_iters: 12,
            max_enodes: 4_096,
            mode: RewriteMode::FastMath,
            cost_model: CostModel::Balanced,
        }
    }
}

/// Statistics returned by [`simplify_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimplifyStats {
    pub iterations: usize,
    pub total_enodes: usize,
    pub cost_before: u32,
    pub cost_after: u32,
}

/// Minimal zero-dep e-graph over an explicit allocator `A` with O(1) SwissTable hashconsing.
pub struct EGraph<A: Allocator + Copy> {
    parent: OwnVec<u32, A>,
    classes: OwnVec<OwnVec<ENode, A>, A>,
    /// Canonical e-node -> canonical e-class hashcons table (O(1) lookup & O(E) congruence closure).
    hashcons: Map<ENode, EClassId, FixedState, A>,
    /// Canonical constant value per e-class (if any e-node in the class is `Const`).
    const_val: OwnVec<Option<f64>, A>,
    total_enodes: usize,
    alloc: A,
}

impl<A: Allocator + Copy> EGraph<A> {
    pub fn new_in(alloc: A) -> Self {
        Self {
            parent: OwnVec::new_in(alloc),
            classes: OwnVec::new_in(alloc),
            hashcons: Map::new(MapOpts {
                hasher: FixedState,
                alloc: alloc,
                capacity: None,
            }),
            const_val: OwnVec::new_in(alloc),
            total_enodes: 0,
            alloc,
        }
    }

    #[inline]
    fn class_nodes(&self, id: EClassId) -> &[ENode] {
        self.classes.get(u32_to_usize(id.0)).map(OwnVec::as_slice).unwrap_or(&[])
    }

    #[inline]
    fn class_len(&self, id: EClassId) -> usize {
        self.class_nodes(id).len()
    }

    #[inline]
    fn class_node(&self, id: EClassId, idx: usize) -> Option<ENode> {
        self.class_nodes(id).get(idx).copied()
    }

    #[inline]
    fn is_canonical_cid(&self, cid: usize) -> bool {
        self.parent.get(cid).copied() == Some(usize_to_u32(cid))
    }

    pub fn find(&self, mut id: EClassId) -> EClassId {
        while let Some(&p) = self.parent.get(u32_to_usize(id.0)) {
            if p == id.0 {
                break;
            }
            id = EClassId(p);
        }
        id
    }

    fn find_mut(&mut self, id: EClassId) -> EClassId {
        let root = self.find(id);
        let mut cur = id;
        while let Some(&p) = self.parent.get(u32_to_usize(cur.0)) {
            if p == root.0 {
                break;
            }
            if let Some(slot) = self.parent.get_mut(u32_to_usize(cur.0)) {
                *slot = root.0;
            }
            cur = EClassId(p);
        }
        root
    }

    fn canonicalize(&self, node: ENode) -> ENode {
        match node {
            ENode::Const(b) => ENode::Const(b),
            ENode::Var(v) => ENode::Var(v),
            ENode::Neg(a) => ENode::Neg(self.find(a)),
            ENode::Add(a, b) => ENode::Add(self.find(a), self.find(b)),
            ENode::Sub(a, b) => ENode::Sub(self.find(a), self.find(b)),
            ENode::Mul(a, b) => ENode::Mul(self.find(a), self.find(b)),
            ENode::Div(a, b) => ENode::Div(self.find(a), self.find(b)),
            ENode::Pow(a, b) => ENode::Pow(self.find(a), self.find(b)),
            ENode::Sin(a) => ENode::Sin(self.find(a)),
            ENode::Cos(a) => ENode::Cos(self.find(a)),
            ENode::Tan(a) => ENode::Tan(self.find(a)),
            ENode::Exp(a) => ENode::Exp(self.find(a)),
            ENode::Ln(a) => ENode::Ln(self.find(a)),
            ENode::Sqrt(a) => ENode::Sqrt(self.find(a)),
            ENode::Tanh(a) => ENode::Tanh(self.find(a)),
            ENode::Abs(a) => ENode::Abs(self.find(a)),
            ENode::Select(c, t, e) => ENode::Select(self.find(c), self.find(t), self.find(e)),
            ENode::Fma(a, b, c) => ENode::Fma(self.find(a), self.find(b), self.find(c)),
        }
    }

    #[inline]
    fn lookup(&self, canon: ENode) -> Option<EClassId> {
        self.hashcons.get(&canon).copied().map(|cid| self.find(cid))
    }

    pub fn add(&mut self, node: ENode) -> EClassId {
        let canon = self.canonicalize(node);
        if let Some(existing) = self.lookup(canon) {
            return existing;
        }
        let cid = EClassId(usize_to_u32(self.parent.len()));
        self.parent.push(cid.0);
        let mut list = OwnVec::new_in(self.alloc);
        list.push(canon);
        self.classes.push(list);
        self.hashcons.insert(canon, cid);
        self.const_val.push(canon.as_const());
        self.total_enodes += 1;
        cid
    }

    pub fn add_const(&mut self, v: f64) -> EClassId {
        self.add(ENode::Const(canonical_f64_bits(v)))
    }

    pub fn union(&mut self, a: EClassId, b: EClassId) -> bool {
        let ra = self.find_mut(a);
        let rb = self.find_mut(b);
        if ra == rb {
            return false;
        }
        let (keep, merge) = if ra.0 < rb.0 { (ra, rb) } else { (rb, ra) };
        let keep_idx = u32_to_usize(keep.0);
        let merge_idx = u32_to_usize(merge.0);
        if let Some(slot) = self.parent.get_mut(merge_idx) {
            *slot = keep.0;
        }
        let merge_const = self.const_val.get(merge_idx).copied().flatten();
        if let Some(keep_c) = self.const_val.get_mut(keep_idx)
            && keep_c.is_none()
        {
            *keep_c = merge_const;
        }
        let moved_len = self.class_len(merge);
        for i in 0..moved_len {
            let Some(n) = self.class_node(merge, i) else {
                break;
            };
            if let Some(keep_list) = self.classes.get_mut(keep_idx)
                && !keep_list.iter().any(|&e| e == n)
            {
                keep_list.push(n);
            }
        }
        if let Some(merge_list) = self.classes.get_mut(merge_idx) {
            merge_list.clear();
        }
        true
    }

    /// O(E) deterministic hashcons congruence closure: re-canonicalizes every live e-class in
    /// class-id order and merges any two classes whose canonicalized e-nodes collide in `hashcons`.
    fn rebuild(&mut self) {
        let mut changed = true;
        let mut guard = 0usize;
        while changed && guard < 32 {
            changed = false;
            guard += 1;
            let nclasses = self.classes.len();
            self.hashcons.clear();
            let mut merges: OwnVec<(EClassId, EClassId), A> = OwnVec::new_in(self.alloc);

            for cid in 0..nclasses {
                if !self.is_canonical_cid(cid) {
                    continue;
                }
                let root = EClassId(usize_to_u32(cid));
                let mut dedup = OwnVec::new_in(self.alloc);
                let len = self.class_len(root);
                for i in 0..len {
                    let Some(raw) = self.class_node(root, i) else {
                        break;
                    };
                    let c = self.canonicalize(raw);
                    if let Some(&prev) = self.hashcons.get(&c) {
                        let prev_root = self.find(prev);
                        if prev_root != root {
                            merges.push((prev_root, root));
                        }
                        if !dedup.iter().any(|&e| e == c) {
                            dedup.push(c);
                        }
                    } else {
                        self.hashcons.insert(c, root);
                        dedup.push(c);
                    }
                }
                if let Some(slot) = self.classes.get_mut(cid) {
                    *slot = dedup;
                }
            }
            for &(a, b) in merges.iter() {
                if self.union(a, b) {
                    changed = true;
                }
            }
        }
    }

    fn is_const(&self, id: EClassId, val: f64) -> bool {
        self.const_of(id) == Some(val)
    }

    fn const_of(&self, id: EClassId) -> Option<f64> {
        let r = self.find(id);
        self.const_val.get(u32_to_usize(r.0)).copied().flatten()
    }

    /// Run one equality-saturation rewrite pass over all live e-classes.
    fn saturate_once(&mut self, max_enodes: usize, mode: RewriteMode, cost_model: CostModel) -> bool {
        let fast = mode == RewriteMode::FastMath;
        let fuse_fma = fast && matches!(cost_model, CostModel::CpuFma | CostModel::GpuTranscendental);
        let mut pending_unions: OwnVec<(EClassId, EClassId), A> = OwnVec::new_in(self.alloc);
        let nclasses = self.classes.len();

        for cid in 0..nclasses {
            if self.total_enodes >= max_enodes {
                break;
            }
            if !self.is_canonical_cid(cid) {
                continue;
            }
            let me = EClassId(usize_to_u32(cid));
            let nlen = self.class_len(me);
            for i in 0..nlen {
                if self.total_enodes >= max_enodes {
                    break;
                }
                let Some(node) = self.class_node(me, i) else {
                    break;
                };
                match node {
                    ENode::Neg(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(-ca);
                            pending_unions.push((me, c));
                        }
                        // -(-x) = x (IEEE-754 bit-exact)
                        let ra = self.find(a);
                        let alen = self.class_len(ra);
                        for j in 0..alen {
                            if let Some(ENode::Neg(x)) = self.class_node(ra, j) {
                                pending_unions.push((me, x));
                            }
                        }
                    }
                    ENode::Add(a, b) => {
                        if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                            let c = self.add_const(ca + cb);
                            pending_unions.push((me, c));
                        }
                        // Commutativity: a + b = b + a (IEEE-754 exact for non-NaN payloads)
                        if a != b {
                            let comm = self.add(ENode::Add(b, a));
                            pending_unions.push((me, comm));
                        }
                        if fast {
                            // x + 0 = x, 0 + x = x (not IEEE-exact for -0.0)
                            if self.is_const(b, 0.0) {
                                pending_unions.push((me, a));
                            }
                            if self.is_const(a, 0.0) {
                                pending_unions.push((me, b));
                            }
                            // x + x = 2 * x
                            if self.find(a) == self.find(b) {
                                let two = self.add_const(2.0);
                                let mul = self.add(ENode::Mul(two, a));
                                pending_unions.push((me, mul));
                            }
                            // x + (-x) = 0
                            let rb = self.find(b);
                            let blen = self.class_len(rb);
                            for j in 0..blen {
                                if let Some(ENode::Neg(x)) = self.class_node(rb, j) {
                                    if self.find(x) == self.find(a) {
                                        let z = self.add_const(0.0);
                                        pending_unions.push((me, z));
                                    }
                                }
                            }
                            // Factoring: (k * x) + (k * y) = k * (x + y)
                            // FMA fusion: (x * y) + b = fma(x, y, b)
                            // Pythagorean trig identity: sin(u)^2 + cos(u)^2 = 1
                            let ra = self.find(a);
                            let alen = self.class_len(ra);
                            for ai in 0..alen {
                                let Some(an) = self.class_node(ra, ai) else {
                                    break;
                                };
                                if fuse_fma
                                    && let ENode::Mul(x, y) = an
                                {
                                    let f = self.add(ENode::Fma(x, y, b));
                                    pending_unions.push((me, f));
                                }
                                for bi in 0..blen {
                                    let Some(bn) = self.class_node(rb, bi) else {
                                        break;
                                    };
                                    match (an, bn) {
                                        (ENode::Mul(k1, x), ENode::Mul(k2, y))
                                            if self.find(k1) == self.find(k2) =>
                                        {
                                            let sum = self.add(ENode::Add(x, y));
                                            let fac = self.add(ENode::Mul(k1, sum));
                                            pending_unions.push((me, fac));
                                        }
                                        (ENode::Pow(p1, e1), ENode::Pow(p2, e2))
                                            if self.is_const(e1, 2.0) && self.is_const(e2, 2.0) =>
                                        {
                                            if self.is_sin_cos_pair(p1, p2) {
                                                let one = self.add_const(1.0);
                                                pending_unions.push((me, one));
                                            }
                                        }
                                        (ENode::Mul(s1, s2), ENode::Mul(c1, c2))
                                            if self.find(s1) == self.find(s2)
                                                && self.find(c1) == self.find(c2)
                                                && self.is_sin_cos_pair(s1, c1) =>
                                        {
                                            let one = self.add_const(1.0);
                                            pending_unions.push((me, one));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            // Associativity: (x + y) + z = x + (y + z)
                            for ai in 0..alen {
                                if let Some(ENode::Add(x, y)) = self.class_node(ra, ai) {
                                    let yz = self.add(ENode::Add(y, b));
                                    let assoc = self.add(ENode::Add(x, yz));
                                    pending_unions.push((me, assoc));
                                }
                            }
                        }
                    }
                    ENode::Sub(a, b) => {
                        if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                            let c = self.add_const(ca - cb);
                            pending_unions.push((me, c));
                        }
                        if fast {
                            if self.find(a) == self.find(b) {
                                let z = self.add_const(0.0);
                                pending_unions.push((me, z));
                            }
                            if self.is_const(b, 0.0) {
                                pending_unions.push((me, a));
                            }
                            if self.is_const(a, 0.0) {
                                let nb = self.add(ENode::Neg(b));
                                pending_unions.push((me, nb));
                            }
                            // Factoring: (k * x) - (k * y) = k * (x - y)
                            // FMA fusion: (x * y) - b = fma(x, y, -b)
                            let ra = self.find(a);
                            let rb = self.find(b);
                            let alen = self.class_len(ra);
                            let blen = self.class_len(rb);
                            for ai in 0..alen {
                                if let Some(ENode::Mul(k1, x)) = self.class_node(ra, ai) {
                                    if fuse_fma {
                                        let nb = self.add(ENode::Neg(b));
                                        let f = self.add(ENode::Fma(k1, x, nb));
                                        pending_unions.push((me, f));
                                    }
                                    for bi in 0..blen {
                                        if let Some(ENode::Mul(k2, y)) = self.class_node(rb, bi) {
                                            if self.find(k1) == self.find(k2) {
                                                let diff_xy = self.add(ENode::Sub(x, y));
                                                let fac = self.add(ENode::Mul(k1, diff_xy));
                                                pending_unions.push((me, fac));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    ENode::Mul(a, b) => {
                        if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                            let c = self.add_const(ca * cb);
                            pending_unions.push((me, c));
                        }
                        if self.is_const(a, 1.0) {
                            pending_unions.push((me, b));
                        }
                        if self.is_const(b, 1.0) {
                            pending_unions.push((me, a));
                        }
                        if a != b {
                            let comm = self.add(ENode::Mul(b, a));
                            pending_unions.push((me, comm));
                        }
                        if fast {
                            if self.is_const(a, 0.0) || self.is_const(b, 0.0) {
                                let z = self.add_const(0.0);
                                pending_unions.push((me, z));
                            }
                            // sqrt(x) * sqrt(x) = x
                            let ra = self.find(a);
                            if ra == self.find(b) {
                                let alen = self.class_len(ra);
                                for ai in 0..alen {
                                    if let Some(ENode::Sqrt(x)) = self.class_node(ra, ai) {
                                        pending_unions.push((me, x));
                                    }
                                }
                            }
                            // Associativity: (x * y) * z = x * (y * z)
                            let alen = self.class_len(ra);
                            for ai in 0..alen {
                                if let Some(ENode::Mul(x, y)) = self.class_node(ra, ai) {
                                    let yz = self.add(ENode::Mul(y, b));
                                    let assoc = self.add(ENode::Mul(x, yz));
                                    pending_unions.push((me, assoc));
                                }
                            }
                        }
                    }
                    ENode::Div(a, b) => {
                        if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                            if cb != 0.0 {
                                let c = self.add_const(ca / cb);
                                pending_unions.push((me, c));
                            }
                        }
                        if self.is_const(b, 1.0) {
                            pending_unions.push((me, a));
                        }
                        if fast {
                            if self.is_const(a, 0.0) {
                                let z = self.add_const(0.0);
                                pending_unions.push((me, z));
                            }
                            if self.find(a) == self.find(b) {
                                let one = self.add_const(1.0);
                                pending_unions.push((me, one));
                            }
                            // (x * y) / y = x
                            let ra = self.find(a);
                            let rb = self.find(b);
                            let alen = self.class_len(ra);
                            for ai in 0..alen {
                                if let Some(ENode::Mul(x, y)) = self.class_node(ra, ai) {
                                    if self.find(y) == rb {
                                        pending_unions.push((me, x));
                                    } else if self.find(x) == rb {
                                        pending_unions.push((me, y));
                                    }
                                }
                            }
                        }
                    }
                    ENode::Pow(a, b) => {
                        if self.is_const(b, 1.0) {
                            pending_unions.push((me, a));
                        }
                        if self.is_const(b, 2.0) {
                            let sq = self.add(ENode::Mul(a, a));
                            pending_unions.push((me, sq));
                        }
                        if fast && self.is_const(b, 0.0) {
                            let one = self.add_const(1.0);
                            pending_unions.push((me, one));
                        }
                        if let (Some(ca), Some(cb)) = (self.const_of(a), self.const_of(b)) {
                            let c = self.add_const(blaze_math::pow(ca, cb));
                            pending_unions.push((me, c));
                        }
                    }
                    ENode::Ln(a) => {
                        if let Some(ca) = self.const_of(a) {
                            if ca > 0.0 {
                                let c = self.add_const(blaze_math::ln(ca));
                                pending_unions.push((me, c));
                            }
                        }
                        if fast {
                            // ln(exp(x)) = x
                            let ra = self.find(a);
                            let alen = self.class_len(ra);
                            for ai in 0..alen {
                                if let Some(ENode::Exp(x)) = self.class_node(ra, ai) {
                                    pending_unions.push((me, x));
                                }
                            }
                        }
                    }
                    ENode::Exp(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(blaze_math::exp(ca));
                            pending_unions.push((me, c));
                        }
                        if fast {
                            // exp(ln(x)) = x
                            let ra = self.find(a);
                            let alen = self.class_len(ra);
                            for ai in 0..alen {
                                if let Some(ENode::Ln(x)) = self.class_node(ra, ai) {
                                    pending_unions.push((me, x));
                                }
                            }
                        }
                    }
                    ENode::Sin(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(blaze_math::sin(ca));
                            pending_unions.push((me, c));
                        }
                    }
                    ENode::Cos(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(blaze_math::cos(ca));
                            pending_unions.push((me, c));
                        }
                    }
                    ENode::Tan(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(blaze_math::tan(ca));
                            pending_unions.push((me, c));
                        }
                    }
                    ENode::Sqrt(a) => {
                        if let Some(ca) = self.const_of(a) {
                            if ca >= 0.0 {
                                let c = self.add_const(blaze_math::sqrt(ca));
                                pending_unions.push((me, c));
                            }
                        }
                    }
                    ENode::Tanh(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(det_tanh(ca));
                            pending_unions.push((me, c));
                        }
                    }
                    ENode::Abs(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(blaze_math::abs(ca));
                            pending_unions.push((me, c));
                        }
                    }
                    ENode::Select(c, t, e) => {
                        if self.find(t) == self.find(e) {
                            pending_unions.push((me, t));
                        } else if let Some(cv) = self.const_of(c) {
                            pending_unions.push((me, if cv > 0.0 { t } else { e }));
                        }
                    }
                    ENode::Fma(a, b, c) => {
                        if let (Some(ca), Some(cb), Some(cc)) =
                            (self.const_of(a), self.const_of(b), self.const_of(c))
                        {
                            let v = self.add_const(blaze_math::mul_add(ca, cb, cc));
                            pending_unions.push((me, v));
                        }
                    }
                    ENode::Const(_) | ENode::Var(_) => {}
                }
            }
        }

        let mut any = false;
        for &(a, b) in pending_unions.iter() {
            if self.union(a, b) {
                any = true;
            }
        }
        if any {
            self.rebuild();
        }
        any
    }

    fn is_sin_cos_pair(&self, p1: EClassId, p2: EClassId) -> bool {
        let r1 = self.find(p1);
        let r2 = self.find(p2);
        for &n1 in self.class_nodes(r1).iter() {
            for &n2 in self.class_nodes(r2).iter() {
                match (n1, n2) {
                    (ENode::Sin(u), ENode::Cos(v)) | (ENode::Cos(u), ENode::Sin(v))
                        if self.find(u) == self.find(v) =>
                    {
                        return true;
                    }
                    _ => {}
                }
            }
        }
        false
    }

    /// Compute minimum tree-cost e-node for every live e-class via Bellman-Ford relaxation.
    fn compute_best(&self, model: CostModel) -> (OwnVec<u32, A>, OwnVec<Option<ENode>, A>) {
        let n = self.classes.len();
        let inf = u32::MAX / 4;
        let mut costs: OwnVec<u32, A> = OwnVec::with_capacity_in(n, self.alloc);
        let mut best: OwnVec<Option<ENode>, A> = OwnVec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            costs.push(inf);
            best.push(None);
        }
        let cost_of = |costs: &[u32], id: EClassId| -> u32 {
            costs.get(u32_to_usize(self.find(id).0)).copied().unwrap_or(inf)
        };
        let mut changed = true;
        let mut guard = 0usize;
        while changed && guard < 64 {
            changed = false;
            guard += 1;
            for cid in 0..n {
                if !self.is_canonical_cid(cid) {
                    continue;
                }
                let class_slice = self.classes.get(cid).map(OwnVec::as_slice).unwrap_or(&[]);
                for &node in class_slice.iter() {
                    let c = node.op_cost(model).saturating_add(match node {
                        ENode::Const(_) | ENode::Var(_) => 0,
                        ENode::Neg(a)
                        | ENode::Sin(a)
                        | ENode::Cos(a)
                        | ENode::Tan(a)
                        | ENode::Exp(a)
                        | ENode::Ln(a)
                        | ENode::Sqrt(a)
                        | ENode::Tanh(a)
                        | ENode::Abs(a) => cost_of(costs.as_slice(), a),
                        ENode::Add(a, b)
                        | ENode::Sub(a, b)
                        | ENode::Mul(a, b)
                        | ENode::Div(a, b)
                        | ENode::Pow(a, b) => cost_of(costs.as_slice(), a)
                            .saturating_add(cost_of(costs.as_slice(), b)),
                        ENode::Select(c, t, e) | ENode::Fma(c, t, e) => cost_of(costs.as_slice(), c)
                            .saturating_add(cost_of(costs.as_slice(), t))
                            .saturating_add(cost_of(costs.as_slice(), e)),
                    });
                    if costs.get(cid).copied().unwrap_or(inf) > c {
                        if let Some(slot) = costs.get_mut(cid) {
                            *slot = c;
                        }
                        if let Some(slot) = best.get_mut(cid) {
                            *slot = Some(node);
                        }
                        changed = true;
                    }
                }
            }
        }
        (costs, best)
    }

    fn extract_into(&self, root: EClassId, best: &[Option<ENode>], g: &mut ExprGraph<A>) -> ExprId {
        enum Frame {
            Enter(EClassId, usize),
            Build { idx: usize, node: ENode },
        }

        let n = self.classes.len();
        let mut memo: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            memo.push(None);
        }
        let mut frames: OwnVec<Frame, A> = OwnVec::new_in(self.alloc);
        let mut vals: OwnVec<ExprId, A> = OwnVec::new_in(self.alloc);
        frames.push(Frame::Enter(root, 0));

        while let Some(frame) = frames.pop() {
            match frame {
                Frame::Enter(id, depth) => {
                    let r = self.find(id);
                    let idx = u32_to_usize(r.0);
                    if let Some(hit) = memo.get(idx).copied().flatten() {
                        vals.push(hit);
                        continue;
                    }
                    if depth > 128 {
                        vals.push(g.lit(0.0));
                        continue;
                    }
                    let Some(node) = best
                        .get(idx)
                        .copied()
                        .flatten()
                        .or_else(|| self.class_node(r, 0))
                    else {
                        vals.push(g.lit(0.0));
                        continue;
                    };
                    match node {
                        ENode::Const(b) => {
                            let eid = g.intern(Op::Const(b));
                            if let Some(slot) = memo.get_mut(idx) {
                                *slot = Some(eid);
                            }
                            vals.push(eid);
                        }
                        ENode::Var(v) => {
                            let eid = g.intern(Op::Var(v));
                            if let Some(slot) = memo.get_mut(idx) {
                                *slot = Some(eid);
                            }
                            vals.push(eid);
                        }
                        ENode::Neg(a)
                        | ENode::Sin(a)
                        | ENode::Cos(a)
                        | ENode::Tan(a)
                        | ENode::Exp(a)
                        | ENode::Ln(a)
                        | ENode::Sqrt(a)
                        | ENode::Tanh(a)
                        | ENode::Abs(a) => {
                            frames.push(Frame::Build { idx, node });
                            frames.push(Frame::Enter(a, depth + 1));
                        }
                        ENode::Add(a, b)
                        | ENode::Sub(a, b)
                        | ENode::Mul(a, b)
                        | ENode::Div(a, b)
                        | ENode::Pow(a, b) => {
                            frames.push(Frame::Build { idx, node });
                            frames.push(Frame::Enter(b, depth + 1));
                            frames.push(Frame::Enter(a, depth + 1));
                        }
                        ENode::Select(a, b, c) | ENode::Fma(a, b, c) => {
                            frames.push(Frame::Build { idx, node });
                            frames.push(Frame::Enter(c, depth + 1));
                            frames.push(Frame::Enter(b, depth + 1));
                            frames.push(Frame::Enter(a, depth + 1));
                        }
                    }
                }
                Frame::Build { idx, node } => {
                    let eid = match node {
                        ENode::Const(b) => g.intern(Op::Const(b)),
                        ENode::Var(v) => g.intern(Op::Var(v)),
                        ENode::Neg(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.neg(ea)
                        }
                        ENode::Add(_, _) => {
                            let eb = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.add(ea, eb)
                        }
                        ENode::Sub(_, _) => {
                            let eb = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.sub(ea, eb)
                        }
                        ENode::Mul(_, _) => {
                            let eb = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.mul(ea, eb)
                        }
                        ENode::Div(_, _) => {
                            let eb = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.div(ea, eb)
                        }
                        ENode::Pow(_, _) => {
                            let eb = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.pow(ea, eb)
                        }
                        ENode::Sin(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.sin(ea)
                        }
                        ENode::Cos(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.cos(ea)
                        }
                        ENode::Tan(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.tan(ea)
                        }
                        ENode::Exp(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.exp(ea)
                        }
                        ENode::Ln(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.ln(ea)
                        }
                        ENode::Sqrt(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.sqrt(ea)
                        }
                        ENode::Tanh(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.tanh(ea)
                        }
                        ENode::Abs(_) => {
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.abs(ea)
                        }
                        ENode::Select(_, _, _) => {
                            let ee = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let et = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let ec = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.select(ec, et, ee)
                        }
                        ENode::Fma(_, _, _) => {
                            let ec = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let eb = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            let ea = vals.pop().unwrap_or_else(|| g.lit(0.0));
                            g.fma(ea, eb, ec)
                        }
                    };
                    if let Some(slot) = memo.get_mut(idx) {
                        *slot = Some(eid);
                    }
                    vals.push(eid);
                }
            }
        }
        vals.pop().unwrap_or_else(|| g.lit(0.0))
    }
}

/// Compute the tree-weighted operation cost of `root` in `g` under [`CostModel::Balanced`].
pub fn expr_cost<A: Allocator + Copy>(g: &ExprGraph<A>, root: ExprId) -> Result<u32, AlgebraError> {
    expr_cost_with(g, root, CostModel::Balanced)
}

/// Compute the tree-weighted operation cost of `root` in `g` under a specific [`CostModel`].
pub fn expr_cost_with<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    model: CostModel,
) -> Result<u32, AlgebraError> {
    let n = g.len();
    if u32_to_usize(root.0) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut memo: OwnVec<Option<u32>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        memo.push(None);
    }
    let dummy = EClassId(0);
    let get_c = |memo: &[Option<u32>], e: ExprId| -> u32 {
        memo.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(0)
    };
    let mut stack: OwnVec<(ExprId, bool), A> = OwnVec::new_in(g.alloc());
    stack.push((root, false));
    while let Some((id, ready)) = stack.pop() {
        let idx = u32_to_usize(id.0);
        if memo.get(idx).copied().flatten().is_some() {
            continue;
        }
        let op = g.node(id)?;
        if !ready {
            match op {
                Op::Const(_) | Op::Var(_) => {}
                Op::Neg(a)
                | Op::Sin(a)
                | Op::Cos(a)
                | Op::Tan(a)
                | Op::Exp(a)
                | Op::Ln(a)
                | Op::Sqrt(a)
                | Op::Tanh(a)
                | Op::Abs(a) => {
                    stack.push((id, true));
                    stack.push((a, false));
                    continue;
                }
                Op::Add(a, b)
                | Op::Sub(a, b)
                | Op::Mul(a, b)
                | Op::Div(a, b)
                | Op::Pow(a, b) => {
                    stack.push((id, true));
                    stack.push((b, false));
                    stack.push((a, false));
                    continue;
                }
                Op::Select(c, t, e) | Op::Fma(c, t, e) => {
                    stack.push((id, true));
                    stack.push((e, false));
                    stack.push((t, false));
                    stack.push((c, false));
                    continue;
                }
            }
        }
        let base = match op {
            Op::Const(_) => ENode::Const(0).op_cost(model),
            Op::Var(v) => ENode::Var(v).op_cost(model),
            Op::Neg(_) => ENode::Neg(dummy).op_cost(model),
            Op::Add(_, _) => ENode::Add(dummy, dummy).op_cost(model),
            Op::Sub(_, _) => ENode::Sub(dummy, dummy).op_cost(model),
            Op::Mul(_, _) => ENode::Mul(dummy, dummy).op_cost(model),
            Op::Div(_, _) => ENode::Div(dummy, dummy).op_cost(model),
            Op::Pow(_, _) => ENode::Pow(dummy, dummy).op_cost(model),
            Op::Sin(_) => ENode::Sin(dummy).op_cost(model),
            Op::Cos(_) => ENode::Cos(dummy).op_cost(model),
            Op::Tan(_) => ENode::Tan(dummy).op_cost(model),
            Op::Exp(_) => ENode::Exp(dummy).op_cost(model),
            Op::Ln(_) => ENode::Ln(dummy).op_cost(model),
            Op::Sqrt(_) => ENode::Sqrt(dummy).op_cost(model),
            Op::Tanh(_) => ENode::Tanh(dummy).op_cost(model),
            Op::Abs(_) => ENode::Abs(dummy).op_cost(model),
            Op::Select(_, _, _) => ENode::Select(dummy, dummy, dummy).op_cost(model),
            Op::Fma(_, _, _) => ENode::Fma(dummy, dummy, dummy).op_cost(model),
        };
        let c = match op {
            Op::Const(_) | Op::Var(_) => base,
            Op::Neg(a)
            | Op::Sin(a)
            | Op::Cos(a)
            | Op::Tan(a)
            | Op::Exp(a)
            | Op::Ln(a)
            | Op::Sqrt(a)
            | Op::Tanh(a)
            | Op::Abs(a) => base.saturating_add(get_c(memo.as_slice(), a)),
            Op::Add(a, b)
            | Op::Sub(a, b)
            | Op::Mul(a, b)
            | Op::Div(a, b)
            | Op::Pow(a, b) => base
                .saturating_add(get_c(memo.as_slice(), a))
                .saturating_add(get_c(memo.as_slice(), b)),
            Op::Select(c, t, e) | Op::Fma(c, t, e) => base
                .saturating_add(get_c(memo.as_slice(), c))
                .saturating_add(get_c(memo.as_slice(), t))
                .saturating_add(get_c(memo.as_slice(), e)),
        };
        if let Some(slot) = memo.get_mut(idx) {
            *slot = Some(c);
        }
    }
    Ok(get_c(memo.as_slice(), root))
}

fn insert_expr<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    root: ExprId,
    eg: &mut EGraph<A>,
    memo: &mut OwnVec<Option<EClassId>, A>,
) -> Result<EClassId, AlgebraError> {
    let get_cid = |memo: &[Option<EClassId>], e: ExprId| -> EClassId {
        memo.get(u32_to_usize(e.0)).copied().flatten().unwrap_or(EClassId(0))
    };
    let mut stack: OwnVec<(ExprId, bool), A> = OwnVec::new_in(g.alloc());
    stack.push((root, false));
    while let Some((id, ready)) = stack.pop() {
        let idx = u32_to_usize(id.0);
        if memo.get(idx).copied().flatten().is_some() {
            continue;
        }
        let op = g.node(id)?;
        if !ready {
            match op {
                Op::Const(_) | Op::Var(_) => {}
                Op::Neg(a)
                | Op::Sin(a)
                | Op::Cos(a)
                | Op::Tan(a)
                | Op::Exp(a)
                | Op::Ln(a)
                | Op::Sqrt(a)
                | Op::Tanh(a)
                | Op::Abs(a) => {
                    stack.push((id, true));
                    stack.push((a, false));
                    continue;
                }
                Op::Add(a, b)
                | Op::Sub(a, b)
                | Op::Mul(a, b)
                | Op::Div(a, b)
                | Op::Pow(a, b) => {
                    stack.push((id, true));
                    stack.push((b, false));
                    stack.push((a, false));
                    continue;
                }
                Op::Select(c, t, e) | Op::Fma(c, t, e) => {
                    stack.push((id, true));
                    stack.push((e, false));
                    stack.push((t, false));
                    stack.push((c, false));
                    continue;
                }
            }
        }
        let cid = match op {
            Op::Const(b) => eg.add(ENode::Const(b)),
            Op::Var(v) => eg.add(ENode::Var(v)),
            Op::Neg(a) => eg.add(ENode::Neg(get_cid(memo.as_slice(), a))),
            Op::Add(a, b) => eg.add(ENode::Add(
                get_cid(memo.as_slice(), a),
                get_cid(memo.as_slice(), b),
            )),
            Op::Sub(a, b) => eg.add(ENode::Sub(
                get_cid(memo.as_slice(), a),
                get_cid(memo.as_slice(), b),
            )),
            Op::Mul(a, b) => eg.add(ENode::Mul(
                get_cid(memo.as_slice(), a),
                get_cid(memo.as_slice(), b),
            )),
            Op::Div(a, b) => eg.add(ENode::Div(
                get_cid(memo.as_slice(), a),
                get_cid(memo.as_slice(), b),
            )),
            Op::Pow(a, b) => eg.add(ENode::Pow(
                get_cid(memo.as_slice(), a),
                get_cid(memo.as_slice(), b),
            )),
            Op::Sin(a) => eg.add(ENode::Sin(get_cid(memo.as_slice(), a))),
            Op::Cos(a) => eg.add(ENode::Cos(get_cid(memo.as_slice(), a))),
            Op::Tan(a) => eg.add(ENode::Tan(get_cid(memo.as_slice(), a))),
            Op::Exp(a) => eg.add(ENode::Exp(get_cid(memo.as_slice(), a))),
            Op::Ln(a) => eg.add(ENode::Ln(get_cid(memo.as_slice(), a))),
            Op::Sqrt(a) => eg.add(ENode::Sqrt(get_cid(memo.as_slice(), a))),
            Op::Tanh(a) => eg.add(ENode::Tanh(get_cid(memo.as_slice(), a))),
            Op::Abs(a) => eg.add(ENode::Abs(get_cid(memo.as_slice(), a))),
            Op::Select(c, t, e) => eg.add(ENode::Select(
                get_cid(memo.as_slice(), c),
                get_cid(memo.as_slice(), t),
                get_cid(memo.as_slice(), e),
            )),
            Op::Fma(a, b, c) => eg.add(ENode::Fma(
                get_cid(memo.as_slice(), a),
                get_cid(memo.as_slice(), b),
                get_cid(memo.as_slice(), c),
            )),
        };
        if let Some(slot) = memo.get_mut(idx) {
            *slot = Some(cid);
        }
    }
    Ok(get_cid(memo.as_slice(), root))
}

/// One node in a flat, `const`-constructible declarative e-graph pattern (`PatNode`).
/// Child indices refer to earlier elements of the pattern slice; the last element is the root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PatNode {
    /// Pattern wildcard `?0..=?7` binding any e-class (repeated uses enforce equality!).
    Var(u8),
    /// Exact constant bits (`canonical_f64_bits(v)`).
    Const(u64),
    Neg(usize),
    Add(usize, usize),
    Sub(usize, usize),
    Mul(usize, usize),
    Div(usize, usize),
    Pow(usize, usize),
    Sin(usize),
    Cos(usize),
    Tan(usize),
    Exp(usize),
    Ln(usize),
    Sqrt(usize),
    Tanh(usize),
    Abs(usize),
    Fma(usize, usize, usize),
}

impl PatNode {
    #[inline]
    pub const fn lit(v: f64) -> Self {
        Self::Const(canonical_f64_bits(v))
    }
}

/// Declarative, zero-allocation user-extensible e-graph rewrite rule `lhs => rhs`.
/// Both `lhs` and `rhs` are flat slices of [`PatNode`] whose last element is the pattern root.
#[derive(Clone, Copy, Debug)]
pub struct RewriteRule<'r> {
    pub name: &'r str,
    pub lhs: &'r [PatNode],
    pub rhs: &'r [PatNode],
}

/// Maximum number of pattern wildcards (`?0..=?7`) in one [`RewriteRule`].
const MAX_PAT_VARS: usize = 8;
/// Maximum e-matching substitutions collected per rule per e-class (`bounded-progress`).
const MAX_MATCHES_PER_CLASS: usize = 16;

type Subst = [Option<EClassId>; MAX_PAT_VARS];

impl<A: Allocator + Copy> EGraph<A> {
    fn ematch_node(
        &self,
        pat: &[PatNode],
        pat_idx: usize,
        class_id: EClassId,
        subst: Subst,
        out: &mut OwnVec<Subst, A>,
    ) {
        let mut goal_pool: OwnVec<(usize, EClassId), A> = OwnVec::new_in(self.alloc);
        let mut work: OwnVec<(Subst, usize), A> = OwnVec::new_in(self.alloc);
        let mut scratch: OwnVec<(usize, EClassId), A> = OwnVec::new_in(self.alloc);
        goal_pool.push((pat_idx, class_id));
        work.push((subst, 1));

        while let Some((cur_subst, n_goals)) = work.pop() {
            if out.len() >= MAX_MATCHES_PER_CLASS {
                break;
            }
            if n_goals == 0 {
                out.push(cur_subst);
                continue;
            }
            let Some((p_idx, c_id)) = goal_pool.pop() else {
                continue;
            };
            let rem = n_goals - 1;
            let base = goal_pool.len().saturating_sub(rem);
            let cid = self.find(c_id);
            let Some(&pnode) = pat.get(p_idx) else {
                goal_pool.truncate(base);
                continue;
            };
            match pnode {
                PatNode::Var(v) => {
                    let vi = usize::from(v);
                    let Some(&bound) = cur_subst.get(vi) else {
                        goal_pool.truncate(base);
                        continue;
                    };
                    match bound {
                        Some(prev) if self.find(prev) == cid => {
                            work.push((cur_subst, rem));
                        }
                        Some(_) => {
                            goal_pool.truncate(base);
                        }
                        None => {
                            let mut next = cur_subst;
                            if let Some(slot) = next.get_mut(vi) {
                                *slot = Some(cid);
                            }
                            work.push((next, rem));
                        }
                    }
                }
                PatNode::Const(bits) => {
                    let want = f64::from_bits(bits);
                    if self.const_of(cid) == Some(want) {
                        work.push((cur_subst, rem));
                    } else {
                        goal_pool.truncate(base);
                    }
                }
                node_pat => {
                    scratch.clear();
                    for j in 0..rem {
                        if let Some(&g) = goal_pool.get(base + j) {
                            scratch.push(g);
                        }
                    }
                    goal_pool.truncate(base);
                    for &enode in self.class_nodes(cid).iter().rev() {
                        match (node_pat, enode) {
                            (PatNode::Neg(pa), ENode::Neg(ea))
                            | (PatNode::Sin(pa), ENode::Sin(ea))
                            | (PatNode::Cos(pa), ENode::Cos(ea))
                            | (PatNode::Tan(pa), ENode::Tan(ea))
                            | (PatNode::Exp(pa), ENode::Exp(ea))
                            | (PatNode::Ln(pa), ENode::Ln(ea))
                            | (PatNode::Sqrt(pa), ENode::Sqrt(ea))
                            | (PatNode::Tanh(pa), ENode::Tanh(ea))
                            | (PatNode::Abs(pa), ENode::Abs(ea)) => {
                                for &g in scratch.iter() {
                                    goal_pool.push(g);
                                }
                                goal_pool.push((pa, ea));
                                work.push((cur_subst, rem + 1));
                            }
                            (PatNode::Add(pa, pb), ENode::Add(ea, eb))
                            | (PatNode::Sub(pa, pb), ENode::Sub(ea, eb))
                            | (PatNode::Mul(pa, pb), ENode::Mul(ea, eb))
                            | (PatNode::Div(pa, pb), ENode::Div(ea, eb))
                            | (PatNode::Pow(pa, pb), ENode::Pow(ea, eb)) => {
                                for &g in scratch.iter() {
                                    goal_pool.push(g);
                                }
                                goal_pool.push((pb, eb));
                                goal_pool.push((pa, ea));
                                work.push((cur_subst, rem + 2));
                            }
                            (PatNode::Fma(pa, pb, pc), ENode::Fma(ea, eb, ec)) => {
                                for &g in scratch.iter() {
                                    goal_pool.push(g);
                                }
                                goal_pool.push((pc, ec));
                                goal_pool.push((pb, eb));
                                goal_pool.push((pa, ea));
                                work.push((cur_subst, rem + 3));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn instantiate_pat(&mut self, pat: &[PatNode], root: usize, subst: &Subst) -> Option<EClassId> {
        let mut results: OwnVec<Option<EClassId>, A> = OwnVec::with_capacity_in(pat.len(), self.alloc);
        for _ in 0..pat.len() {
            results.push(None);
        }
        let mut stack: OwnVec<(usize, bool), A> = OwnVec::new_in(self.alloc);
        stack.push((root, false));
        while let Some((idx, ready)) = stack.pop() {
            if results.get(idx).copied().flatten().is_some() {
                continue;
            }
            let &node = pat.get(idx)?;
            if !ready {
                match node {
                    PatNode::Var(_) | PatNode::Const(_) => {}
                    PatNode::Neg(a)
                    | PatNode::Sin(a)
                    | PatNode::Cos(a)
                    | PatNode::Tan(a)
                    | PatNode::Exp(a)
                    | PatNode::Ln(a)
                    | PatNode::Sqrt(a)
                    | PatNode::Tanh(a)
                    | PatNode::Abs(a) => {
                        stack.push((idx, true));
                        stack.push((a, false));
                        continue;
                    }
                    PatNode::Add(a, b)
                    | PatNode::Sub(a, b)
                    | PatNode::Mul(a, b)
                    | PatNode::Div(a, b)
                    | PatNode::Pow(a, b) => {
                        stack.push((idx, true));
                        stack.push((b, false));
                        stack.push((a, false));
                        continue;
                    }
                    PatNode::Fma(a, b, c) => {
                        stack.push((idx, true));
                        stack.push((c, false));
                        stack.push((b, false));
                        stack.push((a, false));
                        continue;
                    }
                }
            }
            let cid = match node {
                PatNode::Var(v) => subst.get(usize::from(v)).copied().flatten()?,
                PatNode::Const(bits) => self.add(ENode::Const(bits)),
                PatNode::Neg(a) => self.add(ENode::Neg(results.get(a).copied().flatten()?)),
                PatNode::Add(a, b) => self.add(ENode::Add(
                    results.get(a).copied().flatten()?,
                    results.get(b).copied().flatten()?,
                )),
                PatNode::Sub(a, b) => self.add(ENode::Sub(
                    results.get(a).copied().flatten()?,
                    results.get(b).copied().flatten()?,
                )),
                PatNode::Mul(a, b) => self.add(ENode::Mul(
                    results.get(a).copied().flatten()?,
                    results.get(b).copied().flatten()?,
                )),
                PatNode::Div(a, b) => self.add(ENode::Div(
                    results.get(a).copied().flatten()?,
                    results.get(b).copied().flatten()?,
                )),
                PatNode::Pow(a, b) => self.add(ENode::Pow(
                    results.get(a).copied().flatten()?,
                    results.get(b).copied().flatten()?,
                )),
                PatNode::Sin(a) => self.add(ENode::Sin(results.get(a).copied().flatten()?)),
                PatNode::Cos(a) => self.add(ENode::Cos(results.get(a).copied().flatten()?)),
                PatNode::Tan(a) => self.add(ENode::Tan(results.get(a).copied().flatten()?)),
                PatNode::Exp(a) => self.add(ENode::Exp(results.get(a).copied().flatten()?)),
                PatNode::Ln(a) => self.add(ENode::Ln(results.get(a).copied().flatten()?)),
                PatNode::Sqrt(a) => self.add(ENode::Sqrt(results.get(a).copied().flatten()?)),
                PatNode::Tanh(a) => self.add(ENode::Tanh(results.get(a).copied().flatten()?)),
                PatNode::Abs(a) => self.add(ENode::Abs(results.get(a).copied().flatten()?)),
                PatNode::Fma(a, b, c) => self.add(ENode::Fma(
                    results.get(a).copied().flatten()?,
                    results.get(b).copied().flatten()?,
                    results.get(c).copied().flatten()?,
                )),
            };
            if let Some(slot) = results.get_mut(idx) {
                *slot = Some(cid);
            }
        }
        results.get(root).copied().flatten()
    }

    fn apply_custom_rules(&mut self, rules: &[RewriteRule<'_>], max_enodes: usize) -> bool {
        if rules.is_empty() {
            return false;
        }
        let mut pending: OwnVec<(EClassId, EClassId), A> = OwnVec::new_in(self.alloc);
        let nclasses = self.classes.len();
        for rule in rules {
            if rule.lhs.is_empty() || rule.rhs.is_empty() {
                continue;
            }
            let lhs_root = rule.lhs.len() - 1;
            let rhs_root = rule.rhs.len() - 1;
            for cid in 0..nclasses {
                if self.total_enodes >= max_enodes {
                    break;
                }
                if !self.is_canonical_cid(cid) {
                    continue;
                }
                let me = EClassId(usize_to_u32(cid));
                let mut matches: OwnVec<Subst, A> = OwnVec::new_in(self.alloc);
                self.ematch_node(rule.lhs, lhs_root, me, [None; MAX_PAT_VARS], &mut matches);
                for subst in matches.iter() {
                    if self.total_enodes >= max_enodes {
                        break;
                    }
                    if let Some(rhs_cid) = self.instantiate_pat(rule.rhs, rhs_root, subst) {
                        pending.push((me, rhs_cid));
                    }
                }
            }
        }
        let mut any = false;
        for &(a, b) in pending.iter() {
            if self.union(a, b) {
                any = true;
            }
        }
        if any {
            self.rebuild();
        }
        any
    }
}

/// Simplify `root` using bounded e-graph equality saturation with default options.
pub fn simplify<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
) -> Result<ExprId, AlgebraError> {
    let (id, _) = simplify_with_rules(g, root, SimplifyOpts::default(), &[])?;
    Ok(id)
}

/// Simplify `root` with explicit [`SimplifyOpts`], returning the simplified [`ExprId`] and stats.
pub fn simplify_with<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    opts: SimplifyOpts,
) -> Result<(ExprId, SimplifyStats), AlgebraError> {
    simplify_with_rules(g, root, opts, &[])
}

/// Simplify `root` with explicit [`SimplifyOpts`] AND declarative user-defined [`RewriteRule`]s!
pub fn simplify_with_rules<A: Allocator + Copy>(
    g: &mut ExprGraph<A>,
    root: ExprId,
    opts: SimplifyOpts,
    custom_rules: &[RewriteRule<'_>],
) -> Result<(ExprId, SimplifyStats), AlgebraError> {
    let cost_before = expr_cost_with(g, root, opts.cost_model)?;
    let n = g.len();
    let mut eg = EGraph::new_in(g.alloc());
    let mut memo: OwnVec<Option<EClassId>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        memo.push(None);
    }
    let root_cid = insert_expr(g, root, &mut eg, &mut memo)?;

    let mut iterations = 0usize;
    for _ in 0..opts.max_iters {
        iterations += 1;
        let builtin_changed = eg.saturate_once(opts.max_enodes, opts.mode, opts.cost_model);
        let custom_changed = eg.apply_custom_rules(custom_rules, opts.max_enodes);
        if !builtin_changed && !custom_changed {
            break;
        }
    }

    let (_, best) = eg.compute_best(opts.cost_model);
    let extracted = eg.extract_into(root_cid, best.as_slice(), g);
    let cost_after = expr_cost_with(g, extracted, opts.cost_model)?;
    let chosen = if cost_after <= cost_before { extracted } else { root };
    let final_cost = cost_after.min(cost_before);

    Ok((
        chosen,
        SimplifyStats {
            iterations,
            total_enodes: eg.total_enodes,
            cost_before,
            cost_after: final_cost,
        },
    ))
}
