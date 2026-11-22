//! Bounded E-Graph Equality Saturation simplifier (`EGraph<A>`, `simplify`, `simplify_with`).
//!
//! Represents equivalence classes of expression nodes with a union-find structure, applies
//! sound algebraic/trig/log/exp/power/distributive/factoring rewrites up to a strict node &
//! iteration budget (`bounded-progress`, rule 18), and extracts the minimum-cost DAG back into
//! [`ExprGraph`].

use core::alloc::Allocator;

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec as OwnVec;

use crate::{AlgebraError, ExprGraph, ExprId, Op, VarId, canonical_f64_bits, det_tanh};

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

    pub fn find(&self, mut id: EClassId) -> EClassId {
        while self.parent[id.0 as usize] != id.0 {
            id = EClassId(self.parent[id.0 as usize]);
        }
        id
    }

    fn find_mut(&mut self, id: EClassId) -> EClassId {
        let root = self.find(id);
        let mut cur = id;
        while self.parent[cur.0 as usize] != root.0 {
            let next = EClassId(self.parent[cur.0 as usize]);
            self.parent[cur.0 as usize] = root.0;
            cur = next;
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
        let cid = EClassId(self.parent.len() as u32);
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
        self.parent[merge.0 as usize] = keep.0;
        if self.const_val[keep.0 as usize].is_none() {
            self.const_val[keep.0 as usize] = self.const_val[merge.0 as usize];
        }
        let moved_len = self.classes[merge.0 as usize].len();
        for i in 0..moved_len {
            let n = self.classes[merge.0 as usize][i];
            if !self.classes[keep.0 as usize].iter().any(|&e| e == n) {
                self.classes[keep.0 as usize].push(n);
            }
        }
        self.classes[merge.0 as usize].clear();
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
                if self.parent[cid] != cid as u32 {
                    continue;
                }
                let root = EClassId(cid as u32);
                let mut dedup = OwnVec::new_in(self.alloc);
                let len = self.classes[cid].len();
                for i in 0..len {
                    let c = self.canonicalize(self.classes[cid][i]);
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
                self.classes[cid] = dedup;
            }
            for &(a, b) in merges.iter() {
                if self.union(a, b) {
                    changed = true;
                }
            }
        }
    }

    fn is_const(&self, id: EClassId, val: f64) -> bool {
        let r = self.find(id);
        self.const_val[r.0 as usize] == Some(val)
    }

    fn const_of(&self, id: EClassId) -> Option<f64> {
        let r = self.find(id);
        self.const_val[r.0 as usize]
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
            if self.parent[cid] != cid as u32 {
                continue;
            }
            let me = EClassId(cid as u32);
            let nlen = self.classes[cid].len();
            for i in 0..nlen {
                if self.total_enodes >= max_enodes {
                    break;
                }
                let node = self.classes[cid][i];
                match node {
                    ENode::Neg(a) => {
                        if let Some(ca) = self.const_of(a) {
                            let c = self.add_const(-ca);
                            pending_unions.push((me, c));
                        }
                        // -(-x) = x (IEEE-754 bit-exact)
                        let ra = self.find(a);
                        let alen = self.classes[ra.0 as usize].len();
                        for j in 0..alen {
                            if let ENode::Neg(x) = self.classes[ra.0 as usize][j] {
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
                            let blen = self.classes[rb.0 as usize].len();
                            for j in 0..blen {
                                if let ENode::Neg(x) = self.classes[rb.0 as usize][j] {
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
                            let alen = self.classes[ra.0 as usize].len();
                            for ai in 0..alen {
                                let an = self.classes[ra.0 as usize][ai];
                                if fuse_fma
                                    && let ENode::Mul(x, y) = an
                                {
                                    let f = self.add(ENode::Fma(x, y, b));
                                    pending_unions.push((me, f));
                                }
                                for bi in 0..blen {
                                    let bn = self.classes[rb.0 as usize][bi];
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
                                if let ENode::Add(x, y) = self.classes[ra.0 as usize][ai] {
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
                            let alen = self.classes[ra.0 as usize].len();
                            let blen = self.classes[rb.0 as usize].len();
                            for ai in 0..alen {
                                if let ENode::Mul(k1, x) = self.classes[ra.0 as usize][ai] {
                                    if fuse_fma {
                                        let nb = self.add(ENode::Neg(b));
                                        let f = self.add(ENode::Fma(k1, x, nb));
                                        pending_unions.push((me, f));
                                    }
                                    for bi in 0..blen {
                                        if let ENode::Mul(k2, y) = self.classes[rb.0 as usize][bi] {
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
                                let alen = self.classes[ra.0 as usize].len();
                                for ai in 0..alen {
                                    if let ENode::Sqrt(x) = self.classes[ra.0 as usize][ai] {
                                        pending_unions.push((me, x));
                                    }
                                }
                            }
                            // Associativity: (x * y) * z = x * (y * z)
                            let alen = self.classes[ra.0 as usize].len();
                            for ai in 0..alen {
                                if let ENode::Mul(x, y) = self.classes[ra.0 as usize][ai] {
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
                            let alen = self.classes[ra.0 as usize].len();
                            for ai in 0..alen {
                                if let ENode::Mul(x, y) = self.classes[ra.0 as usize][ai] {
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
                            let alen = self.classes[ra.0 as usize].len();
                            for ai in 0..alen {
                                if let ENode::Exp(x) = self.classes[ra.0 as usize][ai] {
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
                            let alen = self.classes[ra.0 as usize].len();
                            for ai in 0..alen {
                                if let ENode::Ln(x) = self.classes[ra.0 as usize][ai] {
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
        for &n1 in self.classes[r1.0 as usize].iter() {
            for &n2 in self.classes[r2.0 as usize].iter() {
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
        let mut costs: OwnVec<u32, A> = OwnVec::with_capacity_in(n, self.alloc);
        let mut best: OwnVec<Option<ENode>, A> = OwnVec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            costs.push(u32::MAX / 4);
            best.push(None);
        }
        let mut changed = true;
        let mut guard = 0usize;
        while changed && guard < 64 {
            changed = false;
            guard += 1;
            for cid in 0..n {
                if self.parent[cid] != cid as u32 {
                    continue;
                }
                for &node in self.classes[cid].iter() {
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
                        | ENode::Abs(a) => costs[self.find(a).0 as usize],
                        ENode::Add(a, b)
                        | ENode::Sub(a, b)
                        | ENode::Mul(a, b)
                        | ENode::Div(a, b)
                        | ENode::Pow(a, b) => costs[self.find(a).0 as usize]
                            .saturating_add(costs[self.find(b).0 as usize]),
                        ENode::Select(c, t, e) | ENode::Fma(c, t, e) => costs[self.find(c).0 as usize]
                            .saturating_add(costs[self.find(t).0 as usize])
                            .saturating_add(costs[self.find(e).0 as usize]),
                    });
                    if c < costs[cid] {
                        costs[cid] = c;
                        best[cid] = Some(node);
                        changed = true;
                    }
                }
            }
        }
        (costs, best)
    }

    fn extract_into(&self, root: EClassId, best: &[Option<ENode>], g: &mut ExprGraph<A>) -> ExprId {
        let n = self.classes.len();
        let mut memo: OwnVec<Option<ExprId>, A> = OwnVec::with_capacity_in(n, self.alloc);
        for _ in 0..n {
            memo.push(None);
        }
        self.extract_rec(root, best, g, &mut memo, 0)
    }

    fn extract_rec(
        &self,
        id: EClassId,
        best: &[Option<ENode>],
        g: &mut ExprGraph<A>,
        memo: &mut OwnVec<Option<ExprId>, A>,
        depth: usize,
    ) -> ExprId {
        let r = self.find(id);
        let idx = r.0 as usize;
        if let Some(hit) = memo[idx] {
            return hit;
        }
        if depth > 128 {
            return g.lit(0.0);
        }
        let node = best[idx].unwrap_or_else(|| self.classes[idx][0]);
        let eid = match node {
            ENode::Const(b) => g.intern(Op::Const(b)),
            ENode::Var(v) => g.intern(Op::Var(v)),
            ENode::Neg(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.neg(ea)
            }
            ENode::Add(a, b) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                let eb = self.extract_rec(b, best, g, memo, depth + 1);
                g.add(ea, eb)
            }
            ENode::Sub(a, b) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                let eb = self.extract_rec(b, best, g, memo, depth + 1);
                g.sub(ea, eb)
            }
            ENode::Mul(a, b) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                let eb = self.extract_rec(b, best, g, memo, depth + 1);
                g.mul(ea, eb)
            }
            ENode::Div(a, b) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                let eb = self.extract_rec(b, best, g, memo, depth + 1);
                g.div(ea, eb)
            }
            ENode::Pow(a, b) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                let eb = self.extract_rec(b, best, g, memo, depth + 1);
                g.pow(ea, eb)
            }
            ENode::Sin(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.sin(ea)
            }
            ENode::Cos(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.cos(ea)
            }
            ENode::Tan(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.tan(ea)
            }
            ENode::Exp(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.exp(ea)
            }
            ENode::Ln(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.ln(ea)
            }
            ENode::Sqrt(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.sqrt(ea)
            }
            ENode::Tanh(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.tanh(ea)
            }
            ENode::Abs(a) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                g.abs(ea)
            }
            ENode::Select(c, t, e) => {
                let ec = self.extract_rec(c, best, g, memo, depth + 1);
                let et = self.extract_rec(t, best, g, memo, depth + 1);
                let ee = self.extract_rec(e, best, g, memo, depth + 1);
                g.select(ec, et, ee)
            }
            ENode::Fma(a, b, c) => {
                let ea = self.extract_rec(a, best, g, memo, depth + 1);
                let eb = self.extract_rec(b, best, g, memo, depth + 1);
                let ec = self.extract_rec(c, best, g, memo, depth + 1);
                g.fma(ea, eb, ec)
            }
        };
        memo[idx] = Some(eid);
        eid
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
    if (root.0 as usize) >= n {
        return Err(AlgebraError::InvalidExpr { expr: root, node_count: n });
    }
    let mut memo: OwnVec<Option<u32>, A> = OwnVec::with_capacity_in(n, g.alloc());
    for _ in 0..n {
        memo.push(None);
    }
    expr_cost_memo(g, root, model, &mut memo)
}

fn expr_cost_memo<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    id: ExprId,
    model: CostModel,
    memo: &mut OwnVec<Option<u32>, A>,
) -> Result<u32, AlgebraError> {
    let idx = id.0 as usize;
    if let Some(hit) = memo[idx] {
        return Ok(hit);
    }
    let op = g.node(id)?;
    let dummy = EClassId(0);
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
        | Op::Abs(a) => base.saturating_add(expr_cost_memo(g, a, model, memo)?),
        Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Pow(a, b) => base
            .saturating_add(expr_cost_memo(g, a, model, memo)?)
            .saturating_add(expr_cost_memo(g, b, model, memo)?),
        Op::Select(c, t, e) | Op::Fma(c, t, e) => base
            .saturating_add(expr_cost_memo(g, c, model, memo)?)
            .saturating_add(expr_cost_memo(g, t, model, memo)?)
            .saturating_add(expr_cost_memo(g, e, model, memo)?),
    };
    memo[idx] = Some(c);
    Ok(c)
}

fn insert_expr<A: Allocator + Copy>(
    g: &ExprGraph<A>,
    id: ExprId,
    eg: &mut EGraph<A>,
    memo: &mut OwnVec<Option<EClassId>, A>,
) -> Result<EClassId, AlgebraError> {
    let idx = id.0 as usize;
    if let Some(hit) = memo[idx] {
        return Ok(hit);
    }
    let cid = match g.node(id)? {
        Op::Const(b) => eg.add(ENode::Const(b)),
        Op::Var(v) => eg.add(ENode::Var(v)),
        Op::Neg(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Neg(ca))
        }
        Op::Add(a, b) => {
            let ca = insert_expr(g, a, eg, memo)?;
            let cb = insert_expr(g, b, eg, memo)?;
            eg.add(ENode::Add(ca, cb))
        }
        Op::Sub(a, b) => {
            let ca = insert_expr(g, a, eg, memo)?;
            let cb = insert_expr(g, b, eg, memo)?;
            eg.add(ENode::Sub(ca, cb))
        }
        Op::Mul(a, b) => {
            let ca = insert_expr(g, a, eg, memo)?;
            let cb = insert_expr(g, b, eg, memo)?;
            eg.add(ENode::Mul(ca, cb))
        }
        Op::Div(a, b) => {
            let ca = insert_expr(g, a, eg, memo)?;
            let cb = insert_expr(g, b, eg, memo)?;
            eg.add(ENode::Div(ca, cb))
        }
        Op::Pow(a, b) => {
            let ca = insert_expr(g, a, eg, memo)?;
            let cb = insert_expr(g, b, eg, memo)?;
            eg.add(ENode::Pow(ca, cb))
        }
        Op::Sin(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Sin(ca))
        }
        Op::Cos(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Cos(ca))
        }
        Op::Tan(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Tan(ca))
        }
        Op::Exp(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Exp(ca))
        }
        Op::Ln(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Ln(ca))
        }
        Op::Sqrt(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Sqrt(ca))
        }
        Op::Tanh(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Tanh(ca))
        }
        Op::Abs(a) => {
            let ca = insert_expr(g, a, eg, memo)?;
            eg.add(ENode::Abs(ca))
        }
        Op::Select(c, t, e) => {
            let cc = insert_expr(g, c, eg, memo)?;
            let ct = insert_expr(g, t, eg, memo)?;
            let ce = insert_expr(g, e, eg, memo)?;
            eg.add(ENode::Select(cc, ct, ce))
        }
        Op::Fma(a, b, c) => {
            let ca = insert_expr(g, a, eg, memo)?;
            let cb = insert_expr(g, b, eg, memo)?;
            let cc = insert_expr(g, c, eg, memo)?;
            eg.add(ENode::Fma(ca, cb, cc))
        }
    };
    memo[idx] = Some(cid);
    Ok(cid)
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
        if out.len() >= MAX_MATCHES_PER_CLASS {
            return;
        }
        let cid = self.find(class_id);
        match pat[pat_idx] {
            PatNode::Var(v) => {
                let vi = v as usize;
                if vi >= MAX_PAT_VARS {
                    return;
                }
                match subst[vi] {
                    Some(prev) if self.find(prev) == cid => out.push(subst),
                    Some(_) => {}
                    None => {
                        let mut next = subst;
                        next[vi] = Some(cid);
                        out.push(next);
                    }
                }
            }
            PatNode::Const(bits) => {
                let want = f64::from_bits(bits);
                if self.const_of(cid) == Some(want) {
                    out.push(subst);
                }
            }
            node_pat => {
                for &enode in self.classes[cid.0 as usize].iter() {
                    if out.len() >= MAX_MATCHES_PER_CLASS {
                        break;
                    }
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
                            self.ematch_node(pat, pa, ea, subst, out);
                        }
                        (PatNode::Add(pa, pb), ENode::Add(ea, eb))
                        | (PatNode::Sub(pa, pb), ENode::Sub(ea, eb))
                        | (PatNode::Mul(pa, pb), ENode::Mul(ea, eb))
                        | (PatNode::Div(pa, pb), ENode::Div(ea, eb))
                        | (PatNode::Pow(pa, pb), ENode::Pow(ea, eb)) => {
                            let mut mid: OwnVec<Subst, A> = OwnVec::new_in(self.alloc);
                            self.ematch_node(pat, pa, ea, subst, &mut mid);
                            for &s1 in mid.iter() {
                                self.ematch_node(pat, pb, eb, s1, out);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn instantiate_pat(&mut self, pat: &[PatNode], idx: usize, subst: &Subst) -> Option<EClassId> {
        let cid = match pat[idx] {
            PatNode::Var(v) => subst.get(v as usize).copied().flatten()?,
            PatNode::Const(bits) => self.add(ENode::Const(bits)),
            PatNode::Neg(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Neg(ca))
            }
            PatNode::Add(a, b) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                let cb = self.instantiate_pat(pat, b, subst)?;
                self.add(ENode::Add(ca, cb))
            }
            PatNode::Sub(a, b) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                let cb = self.instantiate_pat(pat, b, subst)?;
                self.add(ENode::Sub(ca, cb))
            }
            PatNode::Mul(a, b) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                let cb = self.instantiate_pat(pat, b, subst)?;
                self.add(ENode::Mul(ca, cb))
            }
            PatNode::Div(a, b) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                let cb = self.instantiate_pat(pat, b, subst)?;
                self.add(ENode::Div(ca, cb))
            }
            PatNode::Pow(a, b) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                let cb = self.instantiate_pat(pat, b, subst)?;
                self.add(ENode::Pow(ca, cb))
            }
            PatNode::Sin(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Sin(ca))
            }
            PatNode::Cos(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Cos(ca))
            }
            PatNode::Tan(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Tan(ca))
            }
            PatNode::Exp(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Exp(ca))
            }
            PatNode::Ln(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Ln(ca))
            }
            PatNode::Sqrt(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Sqrt(ca))
            }
            PatNode::Tanh(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Tanh(ca))
            }
            PatNode::Abs(a) => {
                let ca = self.instantiate_pat(pat, a, subst)?;
                self.add(ENode::Abs(ca))
            }
        };
        Some(cid)
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
                if self.parent[cid] != cid as u32 {
                    continue;
                }
                let me = EClassId(cid as u32);
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
