//! Writeback: the walk's handles resolved and exported into the record's own type table.
//!
//! Follows rustc_hir_typeck/src/writeback.rs (`resolve_type_vars_in_body`, `Resolver`,
//! `report_error`); Relax maps it in typeck.rs (`resolve_all`, `finish`).
//!
//! Every type is resolved fully and exported children first, each handle once (a cache from
//! handle to portable type), into a hash-consed table, so the record's bytes depend only on
//! what was inferred and the order of first use (rule 25). A variable still unresolved is
//! E0282 at the first node that holds one, when the body has no other error (rustc reports
//! once and is tainted after), and becomes the error type. Sparse tables are sorted by
//! expression, stably, so each expression's adjustments stay in order.

use blaze_own::{Soa, Vec};
use core::alloc::Allocator;

use crate::columns::{Adjusts, Callees, Captures, Checked, Fields};
use crate::diag::{At, Diag, DiagCode};
use crate::error::{CheckError, Limit};
use crate::hash::StableHasher;
use crate::ids::{ExprId, List, PatId};
use crate::machine::Machine;
use crate::map::HashIndex;
use crate::portable::{PNode, PTy, PortableTys};
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

/// The most steps one body's export may take: far above any real body, a typed error past it.
const EXPORT_STEPS: u64 = 1 << 28;

/// The export state: the table, and the handle-to-portable cache.
struct Export<A: Allocator + Clone> {
    out: PortableTys<A>,
    keys: Vec<Ty, A>,
    vals: Vec<PTy, A>,
    index: HashIndex<A>,
    steps: u64,
    /// Whether E0282 was reported by writeback.
    reported: bool,
}

fn ty_hash(t: Ty) -> u64 {
    let mut h = StableHasher::new("writeback.ty");
    h.u32(t.0);
    h.finish()
}

impl<A: Allocator + Clone> Export<A> {
    fn cached(&self, t: Ty) -> Option<PTy> {
        let keys = &self.keys;
        self.index.find(ty_hash(t), |i| keys[i as usize] == t).map(|i| self.vals[i as usize])
    }

    fn insert(&mut self, t: Ty, p: PTy) -> Result<(), CheckError> {
        let at = u32::try_from(self.keys.len()).map_err(|_| CheckError::Limit(Limit::Export))?;
        self.keys.push(t);
        self.vals.push(p);
        self.index.insert(ty_hash(t), at)
    }
}

impl<W: World, C: TypeCtx, A: Allocator + Clone> Machine<'_, '_, '_, W, C, A> {
    /// The children of one level of a type.
    fn ty_children(&self, v: TyView, out: &mut Vec<Ty, A>) {
        let list = |l: TyList, out: &mut Vec<Ty, A>| {
            for i in 0..self.ty.list_len(l) {
                out.push(self.ty.list_get(l, i));
            }
        };
        match v {
            TyView::Tuple(l) | TyView::Adt(_, l) | TyView::FnDef(_, l) | TyView::FnPtr(l) | TyView::Opaque(_, l) => {
                list(l, out)
            }
            TyView::Alias { args, .. } => list(args, out),
            TyView::Ref(_, t) | TyView::Ptr(_, t) | TyView::Array(t, _) | TyView::Slice(t) | TyView::Closure(_, t) => {
                out.push(t)
            }
            TyView::Bool
            | TyView::Char
            | TyView::Int(_)
            | TyView::Uint(_)
            | TyView::Float(_)
            | TyView::Str
            | TyView::Never
            | TyView::Param(_)
            | TyView::Infer(_)
            | TyView::Error => {}
        }
    }

    fn export_list(&mut self, ex: &mut Export<A>, l: TyList) -> Result<List, CheckError> {
        let n = self.ty.list_len(l);
        let mut v: Vec<PTy, A> = Vec::with_capacity_in(n as usize, self.alloc.clone());
        for i in 0..n {
            let t = self.ty.list_get(l, i);
            let t = self.ty.shallow_resolve(t);
            match ex.cached(t) {
                Some(p) => v.push(p),
                None => return Err(CheckError::Limit(Limit::Export)),
            }
        }
        ex.out.intern_list(&v)
    }

    /// Exports `t`, resolved, reporting an unresolved variable at `at`.
    fn export(&mut self, ex: &mut Export<A>, t: Ty, at: At) -> Result<PTy, CheckError> {
        let t = self.ty.resolve(t)?;
        if let Some(p) = ex.cached(t) {
            return Ok(p);
        }
        let mut stack: Vec<(Ty, bool), A> = Vec::new_in(self.alloc.clone());
        let mut kids: Vec<Ty, A> = Vec::new_in(self.alloc.clone());
        stack.push((t, false));
        while let Some((x, expanded)) = stack.pop() {
            ex.steps += 1;
            if ex.steps > EXPORT_STEPS {
                return Err(CheckError::Limit(Limit::Export));
            }
            if ex.cached(x).is_some() {
                continue;
            }
            let x_res = self.ty.shallow_resolve(x);
            let v = self.ty.view(x_res);
            if !expanded {
                stack.push((x, true));
                kids.clear();
                self.ty_children(v, &mut kids);
                for &k in kids.iter() {
                    let k = self.ty.shallow_resolve(k);
                    if ex.cached(k).is_none() {
                        stack.push((k, false));
                    }
                }
                continue;
            }
            let one = |ex: &Export<A>, c: Ty, ctx: &mut C| -> Result<PTy, CheckError> {
                let c = ctx.shallow_resolve(c);
                ex.cached(c).ok_or(CheckError::Limit(Limit::Export))
            };
            let node = match v {
                TyView::Bool => PNode::Bool,
                TyView::Char => PNode::Char,
                TyView::Int(i) => PNode::Int(i),
                TyView::Uint(u) => PNode::Uint(u),
                TyView::Float(f) => PNode::Float(f),
                TyView::Str => PNode::Str,
                TyView::Never => PNode::Never,
                TyView::Tuple(l) => PNode::Tuple(self.export_list(ex, l)?),
                TyView::Ref(m, c) => PNode::Ref(m, one(ex, c, self.ty)?),
                TyView::Ptr(m, c) => PNode::Ptr(m, one(ex, c, self.ty)?),
                TyView::Array(c, n) => PNode::Array(one(ex, c, self.ty)?, n),
                TyView::Slice(c) => PNode::Slice(one(ex, c, self.ty)?),
                TyView::Adt(k, l) => PNode::Adt(k, self.export_list(ex, l)?),
                TyView::FnDef(k, l) => PNode::FnDef(k, self.export_list(ex, l)?),
                TyView::FnPtr(l) => PNode::FnPtr(self.export_list(ex, l)?),
                TyView::Closure(e, sig) => PNode::Closure(e, one(ex, sig, self.ty)?),
                TyView::Param(i) => PNode::Param(i),
                TyView::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: self.export_list(ex, args)? },
                TyView::Opaque(k, l) => PNode::Opaque(k, self.export_list(ex, l)?),
                TyView::Infer(_) => {
                    if self.errors == 0 && !ex.reported {
                        self.diag(DiagCode::AnnotationsNeeded, at, None, None, None);
                        ex.reported = true;
                    }
                    PNode::Error
                }
                TyView::Error => PNode::Error,
            };
            let p = ex.out.intern(node)?;
            ex.insert(x, p)?;
            if x_res != x {
                ex.insert(x_res, p)?;
            }
        }
        ex.cached(t).ok_or(CheckError::Limit(Limit::Export))
    }

    /// `resolve_type_vars_in_body`: the record.
    pub(crate) fn writeback(mut self) -> Result<Checked<A>, CheckError> {
        let alloc = self.alloc.clone();
        let mut ex = Export {
            out: PortableTys::new_in(alloc.clone()),
            keys: Vec::new_in(alloc.clone()),
            vals: Vec::new_in(alloc.clone()),
            index: HashIndex::new_in(alloc.clone()),
            steps: 0,
            reported: false,
        };
        let mut expr_ty: Vec<PTy, A> = Vec::with_capacity_in(self.expr_ty.len(), alloc.clone());
        for i in 0..self.expr_ty.len() {
            let t = self.expr_ty[i];
            expr_ty.push(self.export(&mut ex, t, At::Expr(ExprId(i as u32)))?);
        }
        let mut pat_ty: Vec<PTy, A> = Vec::with_capacity_in(self.pat_ty.len(), alloc.clone());
        for i in 0..self.pat_ty.len() {
            let t = self.pat_ty[i];
            pat_ty.push(self.export(&mut ex, t, At::Pat(PatId(i as u32)))?);
        }
        let mut local_ty: Vec<PTy, A> = Vec::with_capacity_in(self.local_ty.len(), alloc.clone());
        for i in 0..self.local_ty.len() {
            let t = self.local_ty[i];
            local_ty.push(self.export(&mut ex, t, At::Pat(PatId(i as u32)))?);
        }
        let mut adjusts: Adjusts<A> = Soa::with_capacity_in(self.adjust.len(), alloc.clone());
        for i in 0..self.adjust.len() {
            let Some((&e, &kind, &t)) = self.adjust.get(i) else { break };
            let p = self.export(&mut ex, t, At::Expr(e))?;
            adjusts.push((e, kind, p));
        }
        adjusts.sort_by_key(|rows, i| rows.column::<0>()[i]);
        let mut callees: Callees<A> = Soa::with_capacity_in(self.callees.len(), alloc.clone());
        for i in 0..self.callees.len() {
            let (e, item, args) = self.callees[i];
            for j in 0..self.ty.list_len(args) {
                let t = self.ty.list_get(args, j);
                let t = self.ty.resolve(t)?;
                self.export(&mut ex, t, At::Expr(e))?;
            }
            let args = self.ty_resolve_list(args)?;
            let l = self.export_list(&mut ex, args)?;
            callees.push((e, item, l));
        }
        callees.sort_by_key(|rows, i| rows.column::<0>()[i]);
        let mut fields: Fields<A> = Soa::with_capacity_in(self.fields.len(), alloc.clone());
        for &(e, v, f) in self.fields.iter() {
            fields.push((e, v, f));
        }
        fields.sort_by_key(|rows, i| rows.column::<0>()[i]);
        let mut captures: Captures<A> = Soa::with_capacity_in(self.captures.len(), alloc.clone());
        for &(c, p, k) in self.captures.iter() {
            captures.push((c, p, k));
        }
        let mut diags: Vec<Diag, A> = Vec::with_capacity_in(self.diags.len(), alloc.clone());
        let mut i = 0;
        // Exporting a diagnostic's types may add an E0282 diagnostic once; the loop reaches it.
        while i < self.diags.len() {
            let d = self.diags[i];
            i += 1;
            let expected = match d.expected {
                Some(t) => Some(self.export(&mut ex, t, d.at)?),
                None => None,
            };
            let found = match d.found {
                Some(t) => Some(self.export(&mut ex, t, d.at)?),
                None => None,
            };
            diags.push(Diag { code: d.code, at: self.range_of(d.at), expected, found, name: d.name });
        }
        let (reads, needs) = self.reads.finish(alloc);
        Ok(Checked {
            types: ex.out,
            expr_ty,
            pat_ty,
            local_ty,
            adjusts,
            callees,
            fields,
            captures,
            diags,
            reads,
            needs,
        })
    }

    /// The list with every element resolved, as a list of the context.
    fn ty_resolve_list(&mut self, l: TyList) -> Result<TyList, CheckError> {
        let n = self.ty.list_len(l);
        let mut v: Vec<Ty, A> = Vec::with_capacity_in(n as usize, self.alloc.clone());
        for i in 0..n {
            let t = self.ty.list_get(l, i);
            v.push(self.ty.resolve(t)?);
        }
        self.ty.mk_list(&v)
    }
}
