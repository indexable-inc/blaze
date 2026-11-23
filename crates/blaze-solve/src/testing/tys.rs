//! The tests' term arena: the model of what blaze-ty implements (`Tys`'s contract in tys.rs):
//! hash-consed terms, a union-find of inference variables with an undo log, and unification
//! by an explicit work list.

use std::collections::HashMap;
use std::vec::Vec;

use blaze_own::{Alloc, Vec as OVec};

use crate::error::{Limit, SolveError};
use crate::term::{Children, Flags, List, Markers, Rigid, Ty, TyKind, Var, VarKind, children, is_region};
use crate::tys::{Relate, Tys};

/// The arena.
pub struct TestTys {
    nodes: Vec<TyKind>,
    flags: Vec<Flags>,
    markers: Vec<Markers>,
    map: HashMap<TyKind, Ty>,
    lists: Vec<Ty>,
    list_map: HashMap<Vec<Ty>, List>,
    /// Per variable: its kind and its value (`Ty::NONE` while unbound).
    vars: Vec<(VarKind, Ty)>,
    /// Variable indices in binding order (the binding log).
    log: Vec<u32>,
    work: Vec<(Ty, Ty)>,
}

/// A snapshot: the binding log's length.
#[derive(Clone, Copy, Debug)]
pub struct Snap(u32);

/// The arena's bound (rule 18).
pub const MAX_NODES: usize = 1 << 24;

impl Default for TestTys {
    fn default() -> Self {
        Self::new()
    }
}

impl TestTys {
    pub fn new() -> TestTys {
        TestTys {
            nodes: Vec::new(),
            flags: Vec::new(),
            markers: Vec::new(),
            map: HashMap::new(),
            lists: Vec::new(),
            list_map: HashMap::new(),
            vars: Vec::new(),
            log: Vec::new(),
            work: Vec::new(),
        }
    }

    fn own_flags(k: TyKind) -> Flags {
        let mut f = 0u16;
        match k {
            TyKind::Infer(v) => {
                f |= if v.kind == VarKind::Region { Flags::HAS_RE_INFER } else { Flags::HAS_INFER };
            }
            TyKind::Param(_) | TyKind::ConstParam(_) => f |= Flags::HAS_PARAM,
            TyKind::ReParam(_) => f |= Flags::HAS_FREE_REGION,
            TyKind::Placeholder(..) => f |= Flags::HAS_PLACEHOLDER,
            TyKind::Alias(r, a, ..) => {
                f |= Flags::HAS_ALIAS;
                if a == crate::term::AliasKind::Opaque {
                    f |= Flags::HAS_OPAQUE;
                }
                if r == Rigid::No {
                    f |= Flags::HAS_NON_RIGID_ALIAS;
                }
            }
            TyKind::Bound(..) => f |= Flags::HAS_BOUND,
            TyKind::Error => f |= Flags::HAS_ERROR,
            TyKind::Canonical(_) => f |= Flags::HAS_CANONICAL,
            _ => {}
        }
        Flags(f)
    }

    fn each_child(&self, k: TyKind, mut f: impl FnMut(Ty)) {
        match children(k) {
            Children::None => {}
            Children::One(a) => f(a),
            Children::Two(a, b) => {
                f(a);
                f(b);
            }
            Children::List(l) => {
                for i in 0..l.len {
                    f(self.lists[(l.start + i) as usize]);
                }
            }
            Children::ListThen(l, t) => {
                for i in 0..l.len {
                    f(self.lists[(l.start + i) as usize]);
                }
                f(t);
            }
        }
    }

    /// The variable `t` is, if it is one.
    fn var_of(&self, t: Ty) -> Option<Var> {
        match self.nodes[t.0 as usize] {
            TyKind::Infer(v) => Some(v),
            _ => None,
        }
    }

    fn bind(&mut self, v: Var, t: Ty) {
        self.vars[v.index as usize].1 = t;
        self.log.push(v.index);
    }

    /// Whether the variable `v` occurs in `t` (after resolving).
    fn occurs(&self, v: Var, t: Ty) -> bool {
        let mut stack = std::vec![t];
        while let Some(x) = stack.pop() {
            let x = self.shallow_resolve(x);
            if self.var_of(x).is_some_and(|w| w.index == v.index) {
                return true;
            }
            if !self.flags[x.0 as usize].has(Flags::HAS_INFER) {
                continue;
            }
            self.each_child(self.nodes[x.0 as usize], |c| stack.push(c));
        }
        false
    }

    /// `t` with every bound variable replaced by its value, at every depth (the case runner's
    /// comparison; test terms are a few levels deep, `depth` bounds the walk).
    pub fn resolve(&mut self, t: Ty, depth: u32) -> Result<Ty, SolveError> {
        if depth > 256 {
            return Err(SolveError::Limit(Limit::Walk));
        }
        let t = self.shallow_resolve(t);
        if !self.flags[t.0 as usize].has(Flags::HAS_INFER | Flags::HAS_RE_INFER) {
            return Ok(t);
        }
        let k = self.nodes[t.0 as usize];
        let sub = |s: &mut TestTys, x: Ty| s.resolve(x, depth + 1);
        let list = |s: &mut TestTys, l: List| -> Result<List, SolveError> {
            let mut v = Vec::new();
            for i in 0..l.len {
                let x = s.lists[(l.start + i) as usize];
                v.push(s.resolve(x, depth + 1)?);
            }
            s.mk_list(&v)
        };
        let k2 = match k {
            TyKind::Tuple(l) => TyKind::Tuple(list(self, l)?),
            TyKind::Record(s, l) => TyKind::Record(s, list(self, l)?),
            TyKind::Union(l) => TyKind::Union(list(self, l)?),
            TyKind::Ref(r, x, m) => TyKind::Ref(sub(self, r)?, sub(self, x)?, m),
            TyKind::RawPtr(x, m) => TyKind::RawPtr(sub(self, x)?, m),
            TyKind::Array(x, n) => TyKind::Array(sub(self, x)?, sub(self, n)?),
            TyKind::Slice(x) => TyKind::Slice(sub(self, x)?),
            TyKind::Adt(d, l) => TyKind::Adt(d, list(self, l)?),
            TyKind::FnDef(d, l) => TyKind::FnDef(d, list(self, l)?),
            TyKind::FnPtr(l, h) => TyKind::FnPtr(list(self, l)?, h),
            TyKind::Closure(d, l) => TyKind::Closure(d, list(self, l)?),
            TyKind::Coroutine(d, l) => TyKind::Coroutine(d, list(self, l)?),
            TyKind::Dynamic(l, r) => TyKind::Dynamic(list(self, l)?, sub(self, r)?),
            TyKind::Alias(r, a, d, l) => TyKind::Alias(r, a, d, list(self, l)?),
            TyKind::ExTrait(d, l) => TyKind::ExTrait(d, list(self, l)?),
            TyKind::ExProj(d, l, x) => TyKind::ExProj(d, list(self, l)?, sub(self, x)?),
            other => other,
        };
        self.mk(k2)
    }
}

fn same_head(a: TyKind, b: TyKind) -> bool {
    use TyKind::*;
    match (a, b) {
        (Int(x), Int(y)) => x == y,
        (Uint(x), Uint(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Tuple(x), Tuple(y)) | (Union(x), Union(y)) => x.len == y.len,
        (Record(s1, x), Record(s2, y)) => s1 == s2 && x.len == y.len,
        (Ref(_, _, m), Ref(_, _, n)) | (RawPtr(_, m), RawPtr(_, n)) => m == n,
        (Adt(d, _), Adt(e, _))
        | (FnDef(d, _), FnDef(e, _))
        | (Closure(d, _), Closure(e, _))
        | (Coroutine(d, _), Coroutine(e, _))
        | (ExTrait(d, _), ExTrait(e, _))
        | (ExProj(d, _, _), ExProj(e, _, _)) => d == e,
        (Foreign(d), Foreign(e)) | (ExAuto(d), ExAuto(e)) => d == e,
        (Alias(_, k, d, _), Alias(_, l, e, _)) => k == l && d == e,
        (FnPtr(x, h), FnPtr(y, i)) => x.len == y.len && h == i,
        (Dynamic(x, _), Dynamic(y, _)) => x.len == y.len,
        (Array(..), Array(..)) | (Slice(_), Slice(_)) => true,
        (Param(i), Param(j)) | (ConstParam(i), ConstParam(j)) => i == j,
        (Placeholder(u, i), Placeholder(v, j)) | (Bound(u, i), Bound(v, j)) => u == v && i == j,
        (ConstValue(a, x), ConstValue(b, y)) => a == b && x == y,
        (Canonical(i), Canonical(j)) => i == j,
        _ => core::mem::discriminant(&a) == core::mem::discriminant(&b) && children_free(a),
    }
}

fn children_free(k: TyKind) -> bool {
    matches!(children(k), Children::None)
}

impl Tys for TestTys {
    type Snapshot = Snap;

    fn kind(&self, t: Ty) -> TyKind {
        self.nodes[t.0 as usize]
    }

    fn flags(&self, t: Ty) -> Flags {
        self.flags[t.0 as usize]
    }

    fn markers(&self, t: Ty) -> Markers {
        self.markers[t.0 as usize]
    }

    fn set_markers(&mut self, t: Ty, m: Markers) {
        self.markers[t.0 as usize] = m;
    }

    fn list_get(&self, l: List, i: u32) -> Ty {
        self.lists[(l.start + i) as usize]
    }

    fn mk(&mut self, k: TyKind) -> Result<Ty, SolveError> {
        if let Some(&t) = self.map.get(&k) {
            return Ok(t);
        }
        if self.nodes.len() >= MAX_NODES {
            return Err(SolveError::Limit(Limit::Arena));
        }
        let mut f = Self::own_flags(k);
        self.each_child(k, |c| f = f.union(self.flags[c.0 as usize]));
        let t = Ty(self.nodes.len() as u32);
        self.nodes.push(k);
        self.flags.push(f);
        self.markers.push(Markers::default());
        self.map.insert(k, t);
        Ok(t)
    }

    fn mk_list(&mut self, items: &[Ty]) -> Result<List, SolveError> {
        if items.is_empty() {
            return Ok(List::EMPTY);
        }
        if let Some(&l) = self.list_map.get(items) {
            return Ok(l);
        }
        if self.lists.len() >= MAX_NODES {
            return Err(SolveError::Limit(Limit::Arena));
        }
        let l = List { start: self.lists.len() as u32, len: items.len() as u32 };
        self.lists.extend_from_slice(items);
        self.list_map.insert(items.to_vec(), l);
        Ok(l)
    }

    fn fresh(&mut self, kind: VarKind) -> Result<Ty, SolveError> {
        let index = self.vars.len() as u32;
        self.vars.push((kind, Ty::NONE));
        self.mk(TyKind::Infer(Var { kind, index }))
    }

    fn shallow_resolve(&self, mut t: Ty) -> Ty {
        for _ in 0..1 << 20 {
            match self.nodes[t.0 as usize] {
                TyKind::Infer(v) => {
                    let val = self.vars[v.index as usize].1;
                    if val == Ty::NONE {
                        return t;
                    }
                    t = val;
                }
                _ => return t,
            }
        }
        t
    }

    fn unify(&mut self, a: Ty, b: Ty, how: Relate, deferred: &mut OVec<(Ty, Ty), Alloc<'_>>) -> Result<bool, SolveError> {
        let log0 = self.log.len();
        let def0 = deferred.len();
        let base = self.work.len();
        self.work.push((a, b));
        let mut steps = 0usize;
        let fail = |s: &mut TestTys, deferred: &mut OVec<(Ty, Ty), Alloc<'_>>| {
            for &v in s.log[log0..].iter() {
                s.vars[v as usize].1 = Ty::NONE;
            }
            s.log.truncate(log0);
            s.work.truncate(base);
            deferred.truncate(def0);
        };
        while self.work.len() > base {
            steps += 1;
            if steps > 1 << 22 {
                fail(self, deferred);
                return Err(SolveError::Limit(Limit::Walk));
            }
            let Some((x, y)) = self.work.pop() else { break };
            let x = self.shallow_resolve(x);
            let y = self.shallow_resolve(y);
            if x == y {
                continue;
            }
            let (kx, ky) = (self.nodes[x.0 as usize], self.nodes[y.0 as usize]);
            if is_region(kx) || is_region(ky) {
                continue;
            }
            // `Error` relates to anything, but a variable still binds to it.
            let var = matches!(kx, TyKind::Infer(_)) || matches!(ky, TyKind::Infer(_));
            if !var && (matches!(kx, TyKind::Error) || matches!(ky, TyKind::Error)) {
                continue;
            }
            if how == Relate::Lazy {
                if matches!(kx, TyKind::Alias(Rigid::No, ..)) {
                    deferred.push((x, y));
                    continue;
                }
                if matches!(ky, TyKind::Alias(Rigid::No, ..)) {
                    deferred.push((y, x));
                    continue;
                }
            }
            match (kx, ky) {
                (TyKind::Infer(v), TyKind::Infer(w)) => {
                    // An integer or float variable keeps its kind: bind the other to it.
                    let (from, to) = match (v.kind, w.kind) {
                        (VarKind::Ty, _) => (v, y),
                        (_, VarKind::Ty) => (w, x),
                        (k, l) if k == l => (v, y),
                        _ => {
                            fail(self, deferred);
                            return Ok(false);
                        }
                    };
                    self.bind(from, to);
                }
                (TyKind::Infer(v), k) | (k, TyKind::Infer(v)) => {
                    let other = if matches!(kx, TyKind::Infer(_)) { y } else { x };
                    let ok = match v.kind {
                        VarKind::Ty => !self.occurs(v, other),
                        VarKind::Int => matches!(k, TyKind::Int(_) | TyKind::Uint(_)),
                        VarKind::Float => matches!(k, TyKind::Float(_)),
                        VarKind::Const => !self.occurs(v, other),
                        VarKind::Region => true,
                    };
                    if !ok {
                        fail(self, deferred);
                        return Ok(false);
                    }
                    self.bind(v, other);
                }
                _ => {
                    if !same_head(kx, ky) {
                        fail(self, deferred);
                        return Ok(false);
                    }
                    let mut xs = Vec::new();
                    self.each_child(kx, |c| xs.push(c));
                    let mut ys = Vec::new();
                    self.each_child(ky, |c| ys.push(c));
                    if xs.len() != ys.len() {
                        fail(self, deferred);
                        return Ok(false);
                    }
                    for (p, q) in xs.into_iter().zip(ys).rev() {
                        self.work.push((p, q));
                    }
                }
            }
        }
        Ok(true)
    }

    fn snapshot(&mut self) -> Snap {
        Snap(self.log.len() as u32)
    }

    fn rollback_to(&mut self, s: Snap) {
        let n = s.0 as usize;
        while self.log.len() > n {
            let Some(v) = self.log.pop() else { break };
            self.vars[v as usize].1 = Ty::NONE;
        }
    }

    fn commit(&mut self, _s: Snap) {}

    fn bindings_len(&self) -> u32 {
        self.log.len() as u32
    }

    fn binding(&self, i: u32) -> u32 {
        self.log[i as usize]
    }

    fn var_count(&self) -> u32 {
        self.vars.len() as u32
    }
}
