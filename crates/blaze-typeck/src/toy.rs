//! The tests' toy type context and world: a hash-consed type table, a union-find over variables
//! with an undo log, a solver over the param env and the world's impls, and a world of owned
//! answers that can be perturbed and can hold answers back (typeck.md, "Testing").
//!
//! Small on purpose: it implements what the generated bodies reach. Not modelled: impl
//! where-clauses (an impl applies when its header unifies), coherence, auto traits, regions,
//! occurs checks (the generated bodies cannot build a cyclic type).

use blaze_hash::HashMap;
use blaze_own::Vec;
use std::alloc::Global;

use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::hash::StableHasher;
use crate::ids::{ItemKey, LangItem, List, Name, PathIdx, ScopeId};
use crate::intern::ListIndex;
use crate::portable::{PNode, PTy, PTys, PortableTys};
use crate::ty::{FulfillError, FulfillErrorKind, Goal, Obligation, Ty, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::{
    Adt, AdtKind, AssocItem, CtorKind, Def, Fetch, Field, FnSig, Hidden, ImplHeader, Missing, PClause, PathRes,
    Predicates, Question, Reads, Seal, Shape, TypeAttrs, Variant, World,
};

fn list_hash(tys: &[Ty]) -> u64 {
    let mut h = StableHasher::new("toy.list");
    for t in tys {
        h.u32(t.0);
    }
    h.finish()
}

/// The most rounds `select_where_possible` makes over the pending obligations.
const SOLVER_ROUNDS: u32 = 64;
/// The deepest a type is walked by unify or resolve.
const DEPTH: u32 = 256;

/// A snapshot: the undo log's length and the pending obligations. Variables made inside a
/// snapshot stay as unbound handles after a rollback; none is reachable from a type made before.
pub(crate) struct Snap {
    undo: usize,
    pending: Vec<Obligation, Global>,
    errors: usize,
}

pub(crate) struct ToyCtx {
    nodes: Vec<TyView, Global>,
    node_index: HashMap<TyView, u32, Global>,
    lists: Vec<Vec<Ty, Global>, Global>,
    list_index: ListIndex<Global>,
    /// Each variable's value, by index.
    vals: Vec<Option<Ty>, Global>,
    /// Each variable's own handle.
    var_ty: Vec<Ty, Global>,
    undo: Vec<u32, Global>,
    env: Vec<Goal, Global>,
    pending: Vec<Obligation, Global>,
    errors: Vec<FulfillError, Global>,
    errors_taken: usize,
}

impl ToyCtx {
    pub(crate) fn new() -> ToyCtx {
        ToyCtx {
            nodes: Vec::new_in(Global),
            node_index: HashMap::new_in(Global),
            lists: Vec::new_in(Global),
            list_index: ListIndex::new_in(Global),
            vals: Vec::new_in(Global),
            var_ty: Vec::new_in(Global),
            undo: Vec::new_in(Global),
            env: Vec::new_in(Global),
            pending: Vec::new_in(Global),
            errors: Vec::new_in(Global),
            errors_taken: 0,
        }
    }

    /// The obligations pending, for the snapshot test.
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn bind(&mut self, v: Var, t: Ty) {
        self.vals[v.index as usize] = Some(t);
        self.undo.push(v.index);
    }

    fn unify_at(&mut self, a: Ty, b: Ty, depth: u32) -> Result<bool, CheckError> {
        if depth > DEPTH {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let a = self.shallow_resolve(a);
        let b = self.shallow_resolve(b);
        if a == b {
            return Ok(true);
        }
        let (va, vb) = (self.view(a), self.view(b));
        Ok(match (va, vb) {
            (TyView::Error, _) | (_, TyView::Error) => true,
            (TyView::Infer(x), TyView::Infer(y)) => match (x.kind, y.kind) {
                (VarKind::Ty, _) => {
                    self.bind(x, b);
                    true
                }
                (_, VarKind::Ty) => {
                    self.bind(y, a);
                    true
                }
                (k, l) if k == l => {
                    self.bind(x, b);
                    true
                }
                _ => false,
            },
            (TyView::Infer(x), other) | (other, TyView::Infer(x)) => {
                let fits = match x.kind {
                    VarKind::Ty => true,
                    VarKind::Int => matches!(other, TyView::Int(_) | TyView::Uint(_)),
                    VarKind::Float => matches!(other, TyView::Float(_)),
                };
                if fits {
                    let to = if matches!(va, TyView::Infer(_)) { b } else { a };
                    self.bind(x, to);
                }
                fits
            }
            (TyView::Tuple(l), TyView::Tuple(m)) | (TyView::FnPtr(l), TyView::FnPtr(m)) => self.unify_lists(l, m, depth)?,
            (TyView::Ref(m1, x), TyView::Ref(m2, y)) | (TyView::Ptr(m1, x), TyView::Ptr(m2, y)) => {
                m1 == m2 && self.unify_at(x, y, depth + 1)?
            }
            (TyView::Array(x, n), TyView::Array(y, k)) => n == k && self.unify_at(x, y, depth + 1)?,
            (TyView::Slice(x), TyView::Slice(y)) => self.unify_at(x, y, depth + 1)?,
            (TyView::Adt(k, l), TyView::Adt(j, m))
            | (TyView::FnDef(k, l), TyView::FnDef(j, m))
            | (TyView::Opaque(k, l), TyView::Opaque(j, m)) => k == j && self.unify_lists(l, m, depth)?,
            (TyView::Closure(e, x), TyView::Closure(f, y)) => e == f && self.unify_at(x, y, depth + 1)?,
            (x, y) => x == y,
        })
    }

    fn unify_lists(&mut self, l: TyList, m: TyList, depth: u32) -> Result<bool, CheckError> {
        let n = self.list_len(l);
        if n != self.list_len(m) {
            return Ok(false);
        }
        for i in 0..n {
            let (x, y) = (self.list_get(l, i), self.list_get(m, i));
            if !self.unify_at(x, y, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn resolve_at(&mut self, t: Ty, depth: u32) -> Result<Ty, CheckError> {
        if depth > DEPTH {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let t = self.shallow_resolve(t);
        let v = match self.view(t) {
            TyView::Tuple(l) => TyView::Tuple(self.resolve_list(l, depth)?),
            TyView::FnPtr(l) => TyView::FnPtr(self.resolve_list(l, depth)?),
            TyView::Adt(k, l) => TyView::Adt(k, self.resolve_list(l, depth)?),
            TyView::FnDef(k, l) => TyView::FnDef(k, self.resolve_list(l, depth)?),
            TyView::Opaque(k, l) => TyView::Opaque(k, self.resolve_list(l, depth)?),
            TyView::Alias { trait_, name, args } => TyView::Alias { trait_, name, args: self.resolve_list(args, depth)? },
            TyView::Ref(m, x) => TyView::Ref(m, self.resolve_at(x, depth + 1)?),
            TyView::Ptr(m, x) => TyView::Ptr(m, self.resolve_at(x, depth + 1)?),
            TyView::Array(x, n) => TyView::Array(self.resolve_at(x, depth + 1)?, n),
            TyView::Slice(x) => TyView::Slice(self.resolve_at(x, depth + 1)?),
            TyView::Closure(e, x) => TyView::Closure(e, self.resolve_at(x, depth + 1)?),
            _ => return Ok(t),
        };
        self.mk(v)
    }

    fn resolve_list(&mut self, l: TyList, depth: u32) -> Result<TyList, CheckError> {
        let n = self.list_len(l);
        let mut v: Vec<Ty, Global> = Vec::with_capacity_in(n as usize, Global);
        for i in 0..n {
            let t = self.list_get(l, i);
            v.push(self.resolve_at(t, depth + 1)?);
        }
        self.mk_list(&v)
    }

    /// A world answer's type, its parameters replaced by `args` (children come first in a
    /// portable table, so one pass in index order lowers it).
    fn lower(&mut self, types: PTys<'_>, root: PTy, args: TyList) -> Result<Ty, CheckError> {
        let n = root.0 as usize + 1;
        if n > types.nodes.len() {
            return Err(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }));
        }
        let mut memo: Vec<Ty, Global> = Vec::with_capacity_in(n, Global);
        for i in 0..n {
            let node = types.node(PTy(i as u32))?;
            let one = |memo: &Vec<Ty, Global>, c: PTy| {
                memo.get(c.0 as usize).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: i as u32, child: c.0 }))
            };
            let list = |s: &mut ToyCtx, memo: &Vec<Ty, Global>, l: List| -> Result<TyList, CheckError> {
                let src = types.list(l)?;
                let mut v: Vec<Ty, Global> = Vec::with_capacity_in(src.len(), Global);
                for &c in src {
                    v.push(one(memo, c)?);
                }
                s.mk_list(&v)
            };
            let v = match node {
                PNode::Bool => TyView::Bool,
                PNode::Char => TyView::Char,
                PNode::Int(x) => TyView::Int(x),
                PNode::Uint(x) => TyView::Uint(x),
                PNode::Float(x) => TyView::Float(x),
                PNode::Str => TyView::Str,
                PNode::Never => TyView::Never,
                PNode::Tuple(l) => TyView::Tuple(list(self, &memo, l)?),
                PNode::Ref(m, c) => TyView::Ref(m, one(&memo, c)?),
                PNode::Ptr(m, c) => TyView::Ptr(m, one(&memo, c)?),
                PNode::Array(c, k) => TyView::Array(one(&memo, c)?, k),
                PNode::Slice(c) => TyView::Slice(one(&memo, c)?),
                PNode::Adt(k, l) => TyView::Adt(k, list(self, &memo, l)?),
                PNode::FnDef(k, l) => TyView::FnDef(k, list(self, &memo, l)?),
                PNode::FnPtr(l) => TyView::FnPtr(list(self, &memo, l)?),
                PNode::Closure(e, c) => TyView::Closure(e, one(&memo, c)?),
                PNode::Param(p) => {
                    let len = self.list_len(args);
                    if p >= len {
                        return Err(CheckError::Malformed(Malformed::Param { index: p, args: len }));
                    }
                    memo.push(self.list_get(args, p));
                    continue;
                }
                PNode::Alias { trait_, name, args: a } => TyView::Alias { trait_, name, args: list(self, &memo, a)? },
                PNode::Opaque(k, l) => TyView::Opaque(k, list(self, &memo, l)?),
                PNode::Infer => return Err(CheckError::Malformed(Malformed::HoleInAnswer)),
                PNode::Error => TyView::Error,
            };
            let t = self.mk(v)?;
            memo.push(t);
        }
        Ok(memo[root.0 as usize])
    }

    fn lower_list(&mut self, types: PTys<'_>, l: List, args: TyList) -> Result<TyList, CheckError> {
        let src = types.list(l)?;
        let mut v: Vec<Ty, Global> = Vec::with_capacity_in(src.len(), Global);
        for &c in src {
            v.push(self.lower(types, c, args)?);
        }
        self.mk_list(&v)
    }

    fn shape(&mut self, t: Ty) -> Option<Shape> {
        let t = self.shallow_resolve(t);
        Some(match self.view(t) {
            TyView::Bool => Shape::Bool,
            TyView::Char => Shape::Char,
            TyView::Int(i) => Shape::Int(i),
            TyView::Uint(u) => Shape::Uint(u),
            TyView::Float(f) => Shape::Float(f),
            TyView::Str => Shape::Str,
            TyView::Never => Shape::Never,
            TyView::Tuple(l) => Shape::Tuple(self.list_len(l)),
            TyView::Ref(m, _) => Shape::Ref(m),
            TyView::Ptr(m, _) => Shape::Ptr(m),
            TyView::Array(..) => Shape::Array,
            TyView::Slice(_) => Shape::Slice,
            TyView::Adt(k, _) => Shape::Adt(k),
            TyView::FnDef(k, _) => Shape::FnDef(k),
            TyView::FnPtr(l) => Shape::FnPtr(self.list_len(l).saturating_sub(1)),
            TyView::Closure(..) => Shape::Closure,
            TyView::Param(_) => Shape::Param,
            TyView::Opaque(k, _) => Shape::Opaque(k),
            TyView::Alias { .. } | TyView::Infer(_) | TyView::Error => return None,
        })
    }

    /// Unifies `trait_<args>` with a clause's, rolling back unless `keep`.
    fn try_clause(&mut self, trait_: ItemKey, args: TyList, other: ItemKey, other_args: TyList, keep: bool) -> Result<bool, CheckError> {
        if trait_ != other {
            return Ok(false);
        }
        let s = self.snapshot();
        let ok = self.unify_lists(args, other_args, 0)?;
        if ok && keep { self.commit(s) } else { self.rollback_to(s) }
        Ok(ok)
    }

    /// Selects `trait_<args>`: `Some(Some(impl args))` for one impl, `Some(None)` for a bound
    /// of the env, `None` when nothing or more than one applies (the second value says which).
    fn select_trait<W: World, A: core::alloc::Allocator + Clone>(
        &mut self,
        trait_: ItemKey,
        args: TyList,
        reads: &mut Reads<'_, W, A>,
    ) -> Result<Selection, Stop> {
        let self_ty = self.list_get(args, 0);
        let Some(shape) = self.shape(self_ty) else {
            let self_ty = self.shallow_resolve(self_ty);
            return Ok(if matches!(self.view(self_ty), TyView::Error) { Selection::Env } else { Selection::Ambiguous });
        };
        for i in 0..self.env.len() {
            if let Goal::Trait { trait_: t, args: a } = self.env[i] {
                if self.try_clause(trait_, args, t, a, true)? {
                    return Ok(Selection::Env);
                }
            }
        }
        let impls = reads.impls_of(trait_, shape)?;
        let mut found: Option<(ItemKey, TyList)> = None;
        for &imp in impls {
            let Some(h) = reads.impl_header(imp)? else { continue };
            let fresh = self.fresh_list(h.generics)?;
            let header_self = self.lower(h.types, h.self_ty, fresh)?;
            let rest = self.lower_list(h.types, h.trait_args, fresh)?;
            let mut all: Vec<Ty, Global> = Vec::with_capacity_in(1 + self.list_len(rest) as usize, Global);
            all.push(header_self);
            for j in 0..self.list_len(rest) {
                all.push(self.list_get(rest, j));
            }
            let header_args = self.mk_list(&all)?;
            if self.try_clause(trait_, args, trait_, header_args, false)? {
                if found.is_some() {
                    return Ok(Selection::Ambiguous);
                }
                found = Some((imp, header_args));
            }
        }
        match found {
            Some((imp, header_args)) => {
                self.try_clause(trait_, args, trait_, header_args, true)?;
                Ok(Selection::Impl(imp))
            }
            None => Ok(Selection::None),
        }
    }

    fn fresh_list(&mut self, n: u32) -> Result<TyList, CheckError> {
        let mut v: Vec<Ty, Global> = Vec::with_capacity_in(n as usize, Global);
        for _ in 0..n {
            v.push(self.fresh(VarKind::Ty)?);
        }
        self.mk_list(&v)
    }

    /// The associated type `name` of the selected impl: the header instantiated afresh and
    /// unified with `args`, then the type lowered with the same arguments.
    fn assoc_of<W: World, A: core::alloc::Allocator + Clone>(
        &mut self,
        imp: ItemKey,
        args: TyList,
        name: Name,
        reads: &mut Reads<'_, W, A>,
    ) -> Result<Option<Ty>, Stop> {
        let Some(h) = reads.impl_header(imp)? else { return Ok(None) };
        let fresh = self.fresh_list(h.generics)?;
        let header_self = self.lower(h.types, h.self_ty, fresh)?;
        let self_ty = self.list_get(args, 0);
        self.unify_at(header_self, self_ty, 0)?;
        let rest = self.lower_list(h.types, h.trait_args, fresh)?;
        for j in 0..self.list_len(rest) {
            let (x, y) = (self.list_get(rest, j), self.list_get(args, j + 1));
            self.unify_at(x, y, 0)?;
        }
        for &(n, t) in h.assoc_types {
            if n == name {
                return Ok(Some(self.lower(h.types, t, fresh)?));
            }
        }
        Ok(None)
    }

    /// One attempt at an obligation: `Some(true)` held, `Some(false)` failed, `None` not yet.
    fn attempt<W: World, A: core::alloc::Allocator + Clone>(
        &mut self,
        ob: Obligation,
        reads: &mut Reads<'_, W, A>,
    ) -> Result<Option<FulfillErrorKind>, Stop> {
        Ok(match ob.goal {
            Goal::WellFormed(_) => None,
            Goal::Trait { trait_, args } => match self.select_trait(trait_, args, reads)? {
                Selection::Env | Selection::Impl(_) => None,
                Selection::Ambiguous => Some(FulfillErrorKind::Ambiguous),
                Selection::None => Some(FulfillErrorKind::Unsatisfied),
            },
            Goal::Projection { trait_, args, name, term } => match self.normalize(trait_, args, name, reads)? {
                Some(t) => {
                    if self.unify_at(t, term, 0)? {
                        None
                    } else {
                        Some(FulfillErrorKind::Mismatch)
                    }
                }
                None => {
                    let self_ty = self.list_get(args, 0);
                    if self.shape(self_ty).is_none() { Some(FulfillErrorKind::Ambiguous) } else { Some(FulfillErrorKind::Unsatisfied) }
                }
            },
        })
    }
}

enum Selection {
    Env,
    Impl(ItemKey),
    Ambiguous,
    None,
}

impl TypeCtx for ToyCtx {
    type Snapshot = Snap;

    fn mk(&mut self, v: TyView) -> Result<Ty, CheckError> {
        if let Some(&i) = self.node_index.get(&v) {
            return Ok(Ty(i));
        }
        let at = u32::try_from(self.nodes.len()).map_err(|_| CheckError::Limit(Limit::Interner))?;
        self.nodes.push(v);
        self.node_index.insert(v, at);
        Ok(Ty(at))
    }

    fn view(&self, t: Ty) -> TyView {
        self.nodes[t.0 as usize]
    }

    fn mk_list(&mut self, tys: &[Ty]) -> Result<TyList, CheckError> {
        let h = list_hash(tys);
        let lists = &self.lists;
        if let Some(i) = self.list_index.find(h, |i| lists[i as usize].as_slice() == tys) {
            return Ok(TyList(i));
        }
        let at = self.list_index.push(h)?;
        let mut v = Vec::with_capacity_in(tys.len(), Global);
        v.extend_from_slice(tys);
        self.lists.push(v);
        Ok(TyList(at))
    }

    fn list_len(&self, l: TyList) -> u32 {
        self.lists[l.0 as usize].len() as u32
    }

    fn list_get(&self, l: TyList, i: u32) -> Ty {
        self.lists[l.0 as usize][i as usize]
    }

    fn fresh(&mut self, kind: VarKind) -> Result<Ty, CheckError> {
        let index = u32::try_from(self.vals.len()).map_err(|_| CheckError::Limit(Limit::Interner))?;
        let t = self.mk(TyView::Infer(Var { kind, index }))?;
        self.vals.push(None);
        self.var_ty.push(t);
        Ok(t)
    }

    fn shallow_resolve(&mut self, mut t: Ty) -> Ty {
        // A chain of bound variables is at most as long as the variables.
        for _ in 0..=self.vals.len() {
            match self.nodes[t.0 as usize] {
                TyView::Infer(v) => match self.vals[v.index as usize] {
                    Some(next) => t = next,
                    None => return t,
                },
                _ => return t,
            }
        }
        t
    }

    fn resolve(&mut self, t: Ty) -> Result<Ty, CheckError> {
        self.resolve_at(t, 0)
    }

    fn unify(&mut self, a: Ty, b: Ty) -> Result<bool, CheckError> {
        let s = self.snapshot();
        let ok = self.unify_at(a, b, 0)?;
        if ok { self.commit(s) } else { self.rollback_to(s) }
        Ok(ok)
    }

    fn snapshot(&mut self) -> Snap {
        let mut pending = Vec::with_capacity_in(self.pending.len(), Global);
        pending.extend_from_slice(&self.pending);
        Snap { undo: self.undo.len(), pending, errors: self.errors.len() }
    }

    fn rollback_to(&mut self, s: Snap) {
        while self.undo.len() > s.undo {
            if let Some(i) = self.undo.pop() {
                self.vals[i as usize] = None;
            }
        }
        self.pending = s.pending;
        self.errors.truncate(s.errors);
        self.errors_taken = self.errors_taken.min(s.errors);
    }

    fn commit(&mut self, s: Snap) {
        let _ = s;
    }

    fn assume(&mut self, clause: Goal) -> Result<(), CheckError> {
        self.env.push(clause);
        Ok(())
    }

    fn register(&mut self, ob: Obligation) -> Result<(), CheckError> {
        self.pending.push(ob);
        Ok(())
    }

    fn select_where_possible<W: World, A: core::alloc::Allocator + Clone>(
        &mut self,
        reads: &mut Reads<'_, W, A>,
    ) -> Result<(), Stop> {
        for _ in 0..SOLVER_ROUNDS {
            let mut progress = false;
            let mut i = 0;
            while i < self.pending.len() {
                let ob = self.pending[i];
                match self.attempt(ob, reads)? {
                    None => {
                        self.pending.remove(i);
                        progress = true;
                    }
                    Some(FulfillErrorKind::Ambiguous) => i += 1,
                    Some(kind) => {
                        self.pending.remove(i);
                        self.errors.push(FulfillError { obligation: ob, kind });
                        progress = true;
                    }
                }
            }
            if !progress {
                return Ok(());
            }
        }
        Err(CheckError::Limit(Limit::Solver).into())
    }

    fn select_all<W: World, A: core::alloc::Allocator + Clone>(&mut self, reads: &mut Reads<'_, W, A>) -> Result<(), Stop> {
        self.select_where_possible(reads)?;
        for ob in core::mem::replace(&mut self.pending, Vec::new_in(Global)).iter() {
            self.errors.push(FulfillError { obligation: *ob, kind: FulfillErrorKind::Ambiguous });
        }
        Ok(())
    }

    fn next_error(&mut self) -> Option<FulfillError> {
        let e = self.errors.get(self.errors_taken).copied();
        if e.is_some() {
            self.errors_taken += 1;
        }
        e
    }

    fn may_hold<W: World, A: core::alloc::Allocator + Clone>(&mut self, goal: Goal, reads: &mut Reads<'_, W, A>) -> Result<bool, Stop> {
        let s = self.snapshot();
        let got = self.attempt(Obligation { goal, cause: crate::ty::Cause::Body }, reads);
        self.rollback_to(s);
        Ok(matches!(got?, None | Some(FulfillErrorKind::Ambiguous)))
    }

    fn normalize<W: World, A: core::alloc::Allocator + Clone>(
        &mut self,
        trait_: ItemKey,
        args: TyList,
        name: Name,
        reads: &mut Reads<'_, W, A>,
    ) -> Result<Option<Ty>, Stop> {
        for i in 0..self.env.len() {
            if let Goal::Projection { trait_: t, args: a, name: n, term } = self.env[i] {
                if n == name && self.try_clause(trait_, args, t, a, true)? {
                    return Ok(Some(term));
                }
            }
        }
        match self.select_trait(trait_, args, reads)? {
            Selection::Impl(imp) => self.assoc_of(imp, args, name, reads),
            Selection::Env | Selection::Ambiguous | Selection::None => Ok(None),
        }
    }

    fn closure_hint(&mut self, var: Ty, fn_traits: [ItemKey; 3], output: Name) -> Option<(TyList, Option<Ty>)> {
        let var = self.shallow_resolve(var);
        let mut inputs = None;
        let mut out = None;
        for i in 0..self.pending.len() {
            let ob = self.pending[i];
            match ob.goal {
                Goal::Trait { trait_, args } if inputs.is_none() && fn_traits.contains(&trait_) && self.list_len(args) == 2 => {
                    let s = self.list_get(args, 0);
                    if self.shallow_resolve(s) == var {
                        let tup = self.list_get(args, 1);
                        let tup = self.shallow_resolve(tup);
                        if let TyView::Tuple(l) = self.view(tup) {
                            inputs = Some(l);
                        }
                    }
                }
                Goal::Projection { args, name, term, .. } if out.is_none() && name == output => {
                    let s = self.list_get(args, 0);
                    if self.shallow_resolve(s) == var {
                        out = Some(term);
                    }
                }
                _ => {}
            }
        }
        inputs.map(|l| (l, out))
    }
}

// The world.

/// A field's type among the toy's few.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldTy {
    I32,
    U8,
    Bool,
    /// Another struct of the world, by index.
    Struct(u32),
}

/// One struct of the world: its fields' names and types.
pub(crate) struct ToyStruct {
    pub fields: Vec<(Name, FieldTy), Global>,
}

/// One answer table per question, owned.
struct Owned {
    nodes: Vec<PNode, Global>,
    lists: Vec<PTy, Global>,
}

impl Owned {
    fn view(&self) -> PTys<'_> {
        PTys { nodes: &self.nodes, lists: &self.lists }
    }
}

struct AdtAnswer {
    variants: Vec<Variant, Global>,
    fields: Vec<Field, Global>,
    types: Owned,
}

/// The toy world: the body's owner (a fn), its parameters' and output's types, structs, and
/// the questions it holds back.
pub(crate) struct ToyWorld {
    pub owner: ItemKey,
    sig_inputs: Vec<PTy, Global>,
    sig_output: PTy,
    sig_types: Owned,
    adts: Vec<(ItemKey, AdtAnswer), Global>,
    /// Questions answered `Missing`.
    pub missing: Vec<Question, Global>,
}

/// The key of struct `i`.
pub(crate) fn struct_key(i: u32) -> ItemKey {
    ItemKey(0x5_0000 + u128::from(i))
}

fn to_owned(t: PortableTys<Global>) -> Owned {
    let v = t.view();
    let mut nodes = Vec::with_capacity_in(v.nodes.len(), Global);
    nodes.extend_from_slice(v.nodes);
    let mut lists = Vec::with_capacity_in(v.lists.len(), Global);
    lists.extend_from_slice(v.lists);
    Owned { nodes, lists }
}

fn field_pty(t: &mut PortableTys<Global>, f: FieldTy) -> Result<PTy, CheckError> {
    t.intern(match f {
        FieldTy::I32 => PNode::Int(crate::ids::IntTy::I32),
        FieldTy::U8 => PNode::Uint(crate::ids::UintTy::U8),
        FieldTy::Bool => PNode::Bool,
        FieldTy::Struct(j) => PNode::Adt(struct_key(j), List::EMPTY),
    })
}

impl ToyWorld {
    /// A world whose owner takes one parameter of each type in `params` and returns `output`.
    pub(crate) fn new(structs: &[ToyStruct], params: &[FieldTy], output: FieldTy) -> Result<ToyWorld, CheckError> {
        let mut sig = PortableTys::new_in(Global);
        let mut sig_inputs = Vec::new_in(Global);
        for &p in params {
            sig_inputs.push(field_pty(&mut sig, p)?);
        }
        let sig_output = field_pty(&mut sig, output)?;
        let mut adts = Vec::new_in(Global);
        for (i, s) in structs.iter().enumerate() {
            let mut t = PortableTys::new_in(Global);
            let mut fields = Vec::new_in(Global);
            for &(name, f) in s.fields.iter() {
                fields.push(Field { name, ty: field_pty(&mut t, f)? });
            }
            let mut variants = Vec::new_in(Global);
            variants.push(Variant {
                key: struct_key(i as u32),
                name: Name(i as u64),
                ctor: CtorKind::Braces,
                fields: List { start: 0, len: fields.len() as u32 },
            });
            adts.push((struct_key(i as u32), AdtAnswer { variants, fields, types: to_owned(t) }));
        }
        Ok(ToyWorld {
            owner: ItemKey(1),
            sig_inputs,
            sig_output,
            sig_types: to_owned(sig),
            adts,
            missing: Vec::new_in(Global),
        })
    }

    fn held(&self, q: Question) -> Fetch<()> {
        if self.missing.contains(&q) { Err(Missing) } else { Ok(()) }
    }

    /// Changes struct `i`'s answer: every field's type becomes the next of i32, u8, bool.
    pub(crate) fn perturb(&mut self, i: u32) {
        let Some((_, a)) = self.adts.iter_mut().find(|(k, _)| *k == struct_key(i)) else { return };
        for n in a.types.nodes.iter_mut() {
            *n = match *n {
                PNode::Int(_) => PNode::Uint(crate::ids::UintTy::U8),
                PNode::Uint(_) => PNode::Bool,
                PNode::Bool => PNode::Int(crate::ids::IntTy::I32),
                other => other,
            };
        }
    }
}

impl World for ToyWorld {
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        self.held(Question::FnSig(item))?;
        if item != self.owner {
            return Ok(None);
        }
        Ok(Some(FnSig {
            parent_count: 0,
            own_count: 0,
            self_param: false,
            inputs: &self.sig_inputs,
            output: self.sig_output,
            types: self.sig_types.view(),
        }))
    }

    fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        self.held(Question::Predicates(item))?;
        Ok(Predicates { clauses: &[] as &[PClause], types: PTys::EMPTY })
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>> {
        self.held(Question::Adt(item))?;
        Ok(self.adts.iter().find(|(k, _)| *k == item).map(|(_, a)| Adt {
            kind: AdtKind::Struct,
            generics: 0,
            variants: &a.variants,
            fields: &a.fields,
            types: a.types.view(),
        }))
    }

    fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        self.held(Question::ImplsOf(trait_, shape))?;
        Ok(&[])
    }

    fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        self.held(Question::ImplHeader(item))?;
        Ok(None)
    }

    fn inherent(&self, _: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        self.held(Question::Inherent(shape, name))?;
        Ok(&[])
    }

    fn assoc_item(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        self.held(Question::AssocItem(container, name))?;
        Ok(None)
    }

    fn traits_in_scope(&self, _: Seal, scope: ScopeId) -> Fetch<&[ItemKey]> {
        self.held(Question::TraitsInScope(scope))?;
        Ok(&[])
    }

    fn resolve_path(&self, _: Seal, scope: ScopeId, path: PathIdx, segments: &[Name]) -> Fetch<Option<PathRes>> {
        self.held(Question::Path(scope, path))?;
        // `S<i>` names struct `i`, by the name `Name(i)` its variant has.
        Ok(match segments {
            [n] => self
                .adts
                .iter()
                .position(|(_, a)| a.variants[0].name == *n)
                .map(|i| PathRes { def: Def::Adt(struct_key(i as u32)), consumed: 1 }),
            _ => None,
        })
    }

    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        self.held(Question::Lang(item))?;
        Ok(None)
    }

    fn const_value(&self, _: Seal, item: ItemKey) -> Fetch<Option<u64>> {
        self.held(Question::Const(item))?;
        Ok(None)
    }

    fn opaque_hidden(&self, _: Seal, item: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        self.held(Question::Opaque(item))?;
        Ok(None)
    }

    fn attrs(&self, _: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        self.held(Question::Attrs(item))?;
        Ok(TypeAttrs(0))
    }
}
