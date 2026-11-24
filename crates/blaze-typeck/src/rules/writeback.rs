//! Writeback: the walk's handles resolved and exported into the record's own type table.
//!
//! Follows rustc_hir_typeck/src/writeback.rs (`resolve_type_vars_in_body`, `Resolver`),
//! checked against Relax's port of it @8d1a764 (types/src/typeck/writeback.rs,
//! `resolve_type_vars_in_body` and `WritebackCx::resolve`).
//!
//! Every type is resolved fully and exported children first, each handle once (a cache from
//! handle to portable type), into a hash-consed table, so the record's bytes depend only on
//! what was inferred and the order of first use (rule 25). A type that still holds an
//! unresolved variable becomes the error type as a whole (Relax: `if has_infer(ty) { unresolved
//! = true; ty = error }`), and the body gets one E0282 when it has no other error (Relax reports
//! it once, after every type, at the body's last position; here at the first node holding one,
//! which says where). Sparse tables are sorted by expression, stably, so each expression's
//! adjustments stay in order.

use blaze_own::{Alloc, Soa, Vec};

use crate::columns::{Adjusts, Callees, CapturePlaceRows, CaptureProjs, Captures, Checked, Closures, ConstArgs, ExprDefs, Fields, FruFields, PatConstArgs, PatDefs, UnionPats};
use crate::ty::ClosureKind;

/// `Checked::selected`'s rows (impl-per-call).
mod selected_rows;
use crate::diag::{At, Diag, DiagCode};
use crate::error::{CheckError, Limit, Malformed};
use blaze_hash::HashMap;

use crate::ids::{ExprId, ItemKey, List, PatId};
use crate::machine::Machine;
use crate::portable::{PNode, PTy, PortableTys};
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

/// The most steps one body's export may take: far above any real body, a typed error past it.
const EXPORT_STEPS: u64 = 1 << 28;

/// The most distinct types one walk of [`Machine::undecided_of`] visits (rule 18): each
/// handle once (a seen set), so a deep shared type costs its size; a typed error past it.
const MOST_UNDECIDED_STEPS: usize = 1 << 22;

/// The export state: the table, and the handle-to-portable cache.
struct Export<'a> {
    out: PortableTys<'a>,
    /// Each exported handle: its portable type, and whether it held an unresolved variable
    /// (then the portable type is the error type).
    cache: HashMap<Ty, (PTy, bool), Alloc<'a>>,
    steps: u64,
    /// Whether E0282 was reported by writeback.
    reported: bool,
}

impl<'a> Export<'a> {
    fn cached(&self, t: Ty) -> Option<PTy> {
        self.cache.get(&t).map(|&(p, _)| p)
    }

    fn child(&self, owner: ItemKey, parent: Ty, child: Ty) -> Result<(PTy, bool), CheckError> {
        self.cache.get(&child).copied().ok_or(CheckError::Malformed(Malformed::WritebackChild { owner, parent, child }))
    }

    fn insert(&mut self, t: Ty, p: PTy, holds_var: bool) {
        self.cache.insert(t, (p, holds_var));
    }
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The children of one level of a type.
    pub(super) fn ty_children(&self, v: TyView, out: &mut Vec<Ty, Alloc<'a>>) {
        let list = |l: TyList, out: &mut Vec<Ty, Alloc<'a>>| {
            for i in 0..self.ty.list_len(l) {
                out.push(self.ty.list_get(l, i));
            }
        };
        match v {
            TyView::Tuple(l) | TyView::Record(_, l) | TyView::Union(l) | TyView::Adt(_, l) | TyView::FnDef(_, l) | TyView::FnPtr(l, _) | TyView::Opaque(_, l) => {
                list(l, out)
            }
            TyView::Alias { args, .. } | TyView::ClosureOf { args, .. } => list(args, out),
            TyView::CoroutineOf { args, output, interior, .. } => {
                out.push(output);
                out.push(interior);
                list(args, out)
            }
            TyView::Dynamic(l) | TyView::ExTrait(_, l) | TyView::ExProj { args_term: l, .. } => list(l, out),
            TyView::Ref(_, t) | TyView::Ptr(_, t) | TyView::Slice(t) | TyView::Closure(_, t) | TyView::ConstValue { ty: t, .. } => {
                out.push(t)
            }
            TyView::Future(_, o, i) | TyView::Array(o, i) => {
                out.push(o);
                out.push(i);
            }
            TyView::Pat(b, l) => {
                out.push(b);
                list(l, out)
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
            | TyView::ConstParam(_)
            | TyView::ConstInfer(_)
            | TyView::ExAuto(_)
            | TyView::Foreign(_)
            | TyView::Error => {}
        }
    }

    /// An unmet goal of a finding, into the record's table: each argument exported, then the list.
    fn export_goal(&mut self, ex: &mut Export<'a>, g: crate::diag::PendingGoal, at: At) -> Result<crate::diag::UnmetGoal, CheckError> { use crate::diag::{PendingGoal, UnmetGoal};
    let (PendingGoal::Trait { args, .. } | PendingGoal::Projection { args, .. }) = g;
    let n = self.ty.list_len(args);
    let mut v: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
    for i in 0..n {
        let t = self.ty.list_get(args, i);
        v.push(self.export(ex, t, at)?);
    }
    let list = ex.out.intern_list(&v)?;
    Ok(match g {
        PendingGoal::Trait { trait_, .. } => UnmetGoal::Trait { trait_, args: list },
        PendingGoal::Projection { trait_, name, term, .. } => {
            UnmetGoal::Projection { trait_, args: list, name, term: self.export(ex, term, at)? }
        }
    }) }

    /// An E0282's undecided variable into the record: its origin as kept, each pending goal
    /// exported as an unmet goal is (the variable itself exports as the error type: the
    /// finding is this one).
    fn export_undecided(&mut self, ex: &mut Export<'a>, u: crate::diag::PendingUndecided, at: At) -> Result<crate::diag::UndecidedVar, CheckError> {
        let mut pending = [None; crate::diag::MOST_PENDING_SHOWN];
        for (slot, goal) in pending.iter_mut().zip(u.pending) {
            if let Some(g) = goal {
                *slot = Some(self.export_goal(ex, g, at)?);
            }
        }
        Ok(crate::diag::UndecidedVar { origin: u.origin, pending, pending_count: u.pending_count })
    }

    /// The variable an E0282 about `t` is about, when `t` holds one (the first met, children
    /// in order): where it was made ([`Machine::var_origins`], the first variable made in its
    /// unification set that a rule named) and the goals still pending that name it (rule 19).
    /// `None` when `t` holds no type variable. Bounded by [`MOST_UNDECIDED_STEPS`] (rule 18).
    pub(crate) fn undecided_of(&mut self, t: Ty) -> Result<Option<crate::diag::PendingUndecided>, CheckError> {
        use crate::diag::{MOST_PENDING_SHOWN, PendingGoal, PendingUndecided, VarOrigin};
        let Some(root) = self.first_ty_var(t)? else { return Ok(None) };
        let mut origin = VarOrigin::Other;
        for i in 0..self.var_origins.len() {
            let (v, o) = self.var_origins[i];
            if self.ty.shallow_resolve(v) == root {
                origin = o;
                break;
            }
        }
        let mut goals: Vec<crate::ty::Goal, Alloc<'a>> = Vec::new_in(self.alloc);
        self.ty.pending_goals(&mut goals);
        let mut pending = [None; MOST_PENDING_SHOWN];
        let mut count = 0u32;
        let mut tys: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        for &g in goals.iter() {
            tys.clear();
            let (args, extra) = match g {
                crate::ty::Goal::Trait { args, .. } | crate::ty::Goal::ConstHolds { args, .. } => (Some(args), None),
                crate::ty::Goal::Projection { args, term, .. } => (Some(args), Some(term)),
                crate::ty::Goal::ForFields { container, args, .. } => (Some(args), Some(container)),
                crate::ty::Goal::WellFormed(t) => (None, Some(t)),
            };
            if let Some(l) = args {
                for i in 0..self.ty.list_len(l) {
                    tys.push(self.ty.list_get(l, i));
                }
            }
            tys.extend(extra);
            if !self.mentions_var(&tys, root)? {
                continue;
            }
            let kept = match g {
                crate::ty::Goal::Trait { trait_, args } => Some(PendingGoal::Trait { trait_, args }),
                crate::ty::Goal::Projection { trait_, args, name, term } => Some(PendingGoal::Projection { trait_, args, name, term }),
                crate::ty::Goal::WellFormed(_) | crate::ty::Goal::ConstHolds { .. } | crate::ty::Goal::ForFields { .. } => None,
            };
            if let (Some(k), Some(slot)) = (kept, pending.iter_mut().find(|s| s.is_none())) {
                *slot = Some(k);
            }
            count = count.saturating_add(1);
        }
        Ok(Some(PendingUndecided { origin, pending, pending_count: count }))
    }

    /// The first type variable `t` holds, resolved (itself when it is one), children in order.
    fn first_ty_var(&mut self, t: Ty) -> Result<Option<Ty>, CheckError> {
        let mut stack: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut kids: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut seen = blaze_hash::HashSet::new_in(self.alloc);
        stack.push(t);
        while let Some(t) = stack.pop() {
            let t = self.ty.shallow_resolve(t);
            if !seen.insert(t) {
                continue;
            }
            if seen.len() > MOST_UNDECIDED_STEPS {
                return Err(CheckError::Limit(Limit::Steps));
            }
            let v = self.ty.view(t);
            if matches!(v, TyView::Infer(crate::ty::Var { kind: crate::ty::VarKind::Ty, .. })) {
                return Ok(Some(t));
            }
            kids.clear();
            self.ty_children(v, &mut kids);
            // Reversed onto the stack: the first child is met first.
            stack.extend(kids.iter().rev().copied());
        }
        Ok(None)
    }

    /// Whether any of `tys` holds the (resolved) variable `root`.
    fn mentions_var(&mut self, tys: &[Ty], root: Ty) -> Result<bool, CheckError> {
        let mut stack: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut kids: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut seen = blaze_hash::HashSet::new_in(self.alloc);
        stack.extend_from_slice(tys);
        while let Some(t) = stack.pop() {
            let t = self.ty.shallow_resolve(t);
            if t == root {
                return Ok(true);
            }
            if !seen.insert(t) {
                continue;
            }
            if seen.len() > MOST_UNDECIDED_STEPS {
                return Err(CheckError::Limit(Limit::Steps));
            }
            kids.clear();
            let v = self.ty.view(t);
            self.ty_children(v, &mut kids);
            stack.extend_from_slice(&kids);
        }
        Ok(false)
    }

    /// A finding's why-chain into the record: each step's goal exported as an unmet goal is,
    /// the steps appended to `out` (the record's `why` column), root first.
    fn export_why(&mut self, ex: &mut Export<'a>, w: crate::diag::PendingWhy, at: At, out: &mut Vec<crate::diag::WhyStep, Alloc<'a>>) -> Result<crate::diag::Why, CheckError> {
        let start = u32::try_from(out.len()).map_err(|_| CheckError::Limit(Limit::Steps))?;
        for i in 0..w.len {
            let Some(&step) = self.why_steps.get(w.start.saturating_add(i) as usize) else {
                return Err(CheckError::Malformed(Malformed::List { column: "why_steps", start: w.start, len: w.len }));
            };
            let goal = match step.goal {
                Some((trait_, args)) => Some(self.export_goal(ex, crate::diag::PendingGoal::Trait { trait_, args }, at)?),
                None => None,
            };
            out.push(crate::diag::WhyStep { goal, reached: step.reached });
        }
        Ok(crate::diag::Why { start, len: w.len, leaf: w.leaf })
    }

    fn export_list(&mut self, ex: &mut Export<'a>, parent: Ty, l: TyList) -> Result<List, CheckError> {
        let n = self.ty.list_len(l);
        let mut v: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
        for i in 0..n {
            let t = self.ty.list_get(l, i);
            let t = self.ty.shallow_resolve(t);
            v.push(ex.child(self.body.owner, parent, t)?.0);
        }
        ex.out.intern_list(&v)
    }

    /// Exports `t`, resolved, reporting an unresolved variable at `at`.
    fn export(&mut self, ex: &mut Export<'a>, t: Ty, at: At) -> Result<PTy, CheckError> {
        let owner = self.body.owner;
        let t = self.ty.resolve(t)?;
        if let Some(p) = ex.cached(t) {
            return Ok(p);
        }
        let mut stack: Vec<(Ty, bool), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut kids: Vec<Ty, Alloc<'a>> = Vec::new_in(self.alloc);
        stack.push((t, false));
        while let Some((x, expanded)) = stack.pop() {
            ex.steps += 1;
            if ex.steps > EXPORT_STEPS {
                return Err(CheckError::Limit(Limit::WritebackSteps { owner, node: x, steps: ex.steps, limit: EXPORT_STEPS }));
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
            kids.clear();
            self.ty_children(v, &mut kids);
            let mut holds_var = matches!(v, TyView::Infer(_) | TyView::ConstInfer(_));
            for &k in kids.iter() {
                let k = self.ty.shallow_resolve(k);
                holds_var |= ex.child(owner, x_res, k)?.1;
            }
            if holds_var {
                if self.errors == 0 && !ex.reported {
                    // What is undecided (rule 19): the variable's origin and its pending goals.
                    let undecided = self.undecided_of(t)?;
                    self.errors += 1;
                    self.diags.push(crate::diag::PendingDiag::new(DiagCode::AnnotationsNeeded, at, crate::diag::About::found(t)).with_undecided(undecided));
                    ex.reported = true;
                }
                let p = ex.out.intern(PNode::Error)?;
                ex.insert(x, p, true);
                ex.insert(x_res, p, true);
                continue;
            }
            let one = |ex: &Export<'a>, c: Ty, ctx: &mut C| -> Result<PTy, CheckError> {
                let c = ctx.shallow_resolve(c);
                Ok(ex.child(owner, x_res, c)?.0)
            };
            let node = match v {
                TyView::Bool => PNode::Bool,
                TyView::Char => PNode::Char,
                TyView::Int(i) => PNode::Int(i),
                TyView::Uint(u) => PNode::Uint(u),
                TyView::Float(f) => PNode::Float(f),
                TyView::Str => PNode::Str,
                TyView::Never => PNode::Never,
                TyView::Tuple(l) => PNode::Tuple(self.export_list(ex, x_res, l)?),
                TyView::Record(s, l) => PNode::Record(s, self.export_list(ex, x_res, l)?),
                TyView::Union(l) => PNode::Union(self.export_list(ex, x_res, l)?),
                TyView::Ref(m, c) => PNode::Ref(m, one(ex, c, self.ty)?),
                TyView::Ptr(m, c) => PNode::Ptr(m, one(ex, c, self.ty)?),
                TyView::Array(c, n) => PNode::Array(one(ex, c, self.ty)?, one(ex, n, self.ty)?),
                TyView::Slice(c) => PNode::Slice(one(ex, c, self.ty)?),
                TyView::Pat(c, l) => PNode::Pat(one(ex, c, self.ty)?, self.export_list(ex, x_res, l)?),
                TyView::Adt(k, l) => PNode::Adt(k, self.export_list(ex, x_res, l)?),
                TyView::FnDef(k, l) => PNode::FnDef(k, self.export_list(ex, x_res, l)?),
                TyView::FnPtr(l, h) => PNode::FnPtr(self.export_list(ex, x_res, l)?, h),
                // A closure of this body is named by its ordinal at the owner's own parameters
                // (typeck.md, "A closure's identity"); its signature goes to `Checked::closures`.
                TyView::Closure(e, _) => {
                    let ordinal = self.closure_ordinal(e)?;
                    let mut ps: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(self.own_params as usize, self.alloc);
                    for i in 0..self.own_params {
                        let is_const = self.own_consts.binary_search_by_key(&i, |&(index, _)| index).is_ok();
                        ps.push(ex.out.intern(if is_const { PNode::ConstParam(i) } else { PNode::Param(i) })?);
                    }
                    PNode::Closure { owner: self.body.owner, ordinal, args: ex.out.intern_list(&ps)? }
                }
                TyView::ClosureOf { owner, ordinal, args, .. } => PNode::Closure { owner, ordinal, args: self.export_list(ex, x_res, args)? },
                TyView::Future(e, o, i) => PNode::Future(e, one(ex, o, self.ty)?, one(ex, i, self.ty)?),
                TyView::CoroutineOf { owner, ordinal, args, .. } => PNode::Coroutine { owner, ordinal, args: self.export_list(ex, x_res, args)? },
                TyView::Param(i) => PNode::Param(i),
                TyView::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: self.export_list(ex, x_res, args)? },
                TyView::Opaque(k, l) => PNode::Opaque(k, self.export_list(ex, x_res, l)?),
                TyView::ConstValue { ty, bits } => PNode::ConstValue { ty: one(ex, ty, self.ty)?, bits },
                TyView::ConstParam(i) => PNode::ConstParam(i),
                TyView::Dynamic(l) => PNode::Dynamic(self.export_list(ex, x_res, l)?),
                TyView::ExTrait(k, l) => PNode::ExTrait(k, self.export_list(ex, x_res, l)?),
                TyView::ExProj { trait_, name, args_term } => {
                    let n = self.ty.list_len(args_term);
                    let mut v: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
                    for i in 0..n {
                        let c = self.ty.list_get(args_term, i);
                        v.push(one(ex, c, self.ty)?);
                    }
                    let Some((&term, args)) = v.split_last() else {
                        return Err(CheckError::Malformed(crate::error::Malformed::ObjectPreds { node: x.0 }));
                    };
                    PNode::ExProj { trait_, name, args: ex.out.intern_list(args)?, term }
                }
                TyView::ExAuto(k) => PNode::ExAuto(k),
                TyView::Foreign(k) => PNode::Foreign(k),
                // A variable is `holds_var` above.
                TyView::Infer(_) | TyView::ConstInfer(_) | TyView::Error => PNode::Error,
            };
            let p = ex.out.intern(node)?;
            ex.insert(x, p, false);
            ex.insert(x_res, p, false);
        }
        ex.cached(t).ok_or(CheckError::Malformed(Malformed::WritebackRoot { owner, root: t }))
    }

    /// Closure `e`'s ordinal: the number of closure expressions before it in `ExprId` order
    /// (typeck.md, "A closure's identity": nested closures count in the item's sequence).
    fn closure_ordinal(&self, e: ExprId) -> Result<u32, CheckError> {
        let Some(before) = self.body.exprs.get(..e.ix()) else {
            return Err(CheckError::Malformed(crate::error::Malformed::Unreached(e)));
        };
        Ok(before.iter().filter(|x| matches!(x, crate::body::Expr::Closure { .. })).count() as u32)
    }

    /// `resolve_type_vars_in_body`: the record, or the needs when writeback's own reads met a
    /// hole (a run past a hole makes no record: world.rs `Hole`).
    pub(crate) fn writeback(mut self) -> Result<crate::Outcome<'a>, CheckError> {
        let alloc = self.alloc;
        let mut ex = Export {
            out: PortableTys::new_in(alloc),
            cache: HashMap::new_in(alloc),
            steps: 0,
            reported: false,
        };
        for &(schema, list) in self.record_schemas.iter() {
            let names = if list.len == 0 {
                &[][..]
            } else {
                self.record_names.get(list.range()).unwrap_or(&[])
            };
            ex.out
                .register_record_schema(schema, names)
                .map_err(|_| CheckError::Limit(Limit::Table))?;
        }
        let mut expr_ty: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(self.expr_ty.len(), alloc);
        for i in 0..self.expr_ty.len() {
            let t = self.expr_ty[i];
            expr_ty.push(self.export(&mut ex, t, At::Expr(ExprId(i as u32)))?);
        }
        let mut pat_ty: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(self.pat_ty.len(), alloc);
        for i in 0..self.pat_ty.len() {
            let t = self.pat_ty[i];
            pat_ty.push(self.export(&mut ex, t, At::Pat(PatId(i as u32)))?);
        }
        let mut local_ty: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(self.local_ty.len(), alloc);
        for i in 0..self.local_ty.len() {
            let t = self.local_ty[i];
            local_ty.push(self.export(&mut ex, t, At::Pat(PatId(i as u32)))?);
        }
        let mut adjusts: Adjusts<'a> = Soa::with_capacity_in(self.adjust.len(), alloc);
        for i in 0..self.adjust.len() {
            let Some((&e, &kind, &t)) = self.adjust.get(i) else { break };
            let p = self.export(&mut ex, t, At::Expr(e))?;
            adjusts.push((e, kind, p));
        }
        adjusts.sort_by_key(|rows, i| rows.column::<0>()[i]);
        let mut callees: Callees<'a> = Soa::with_capacity_in(self.callees.len(), alloc);
        for i in 0..self.callees.len() {
            let (e, item, args) = self.callees[i];
            // An operator whose final operand types are builtin scalars is the builtin
            // operator: no row (rules/op.rs).
            if self.operator_settled_builtin(e, args)? {
                continue;
            }
            // A top-level list has no parent type: each argument is exported and the list made
            // of what was exported, as `export_goal` does (`export_list` names a parent for a
            // missing child, lane TG3).
            let n = self.ty.list_len(args);
            let mut exported: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
            for j in 0..n {
                let t = self.ty.list_get(args, j);
                let t = self.ty.resolve(t)?;
                exported.push(self.export(&mut ex, t, At::Expr(e))?);
            }
            let l = ex.out.intern_list(&exported)?;
            callees.push((e, item, l));
        }
        callees.sort_by_key(|rows, i| rows.column::<0>()[i]);
        // A constant path's arguments, resolved like a callee's: an argument still unknown here
        // is the export's ordinary "type annotations needed" at the path, never an empty list.
        let mut const_args: ConstArgs<'a> = Soa::with_capacity_in(self.const_args.len(), alloc);
        let mut pat_const_args: PatConstArgs<'a> = Soa::new_in(alloc);
        for i in 0..self.const_args.len() {
            let (at, args) = self.const_args[i];
            let n = self.ty.list_len(args);
            let mut exported: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
            for j in 0..n {
                let t = self.ty.list_get(args, j);
                let t = self.ty.resolve(t)?;
                exported.push(self.export(&mut ex, t, at)?);
            }
            let l = ex.out.intern_list(&exported)?;
            match at {
                At::Expr(e) => const_args.push((e, l)),
                At::Pat(p) => pat_const_args.push((p, l)),
                At::Body | At::Written(_) => {}
            }
        }
        const_args.sort_by_key(|rows, i| rows.column::<0>()[i]);
        pat_const_args.sort_by_key(|rows, i| rows.column::<0>()[i]);
        let mut expr_defs: ExprDefs<'a> = Soa::new_in(alloc);
        let mut pat_defs: PatDefs<'a> = Soa::new_in(alloc);
        for &(at, d) in self.defs.iter() {
            match at {
                At::Expr(e) => expr_defs.push((e, d)),
                At::Pat(p) => pat_defs.push((p, d)),
                At::Body | At::Written(_) => {}
            }
        }
        expr_defs.sort_by_key(|rows, i| rows.column::<0>()[i]);
        pat_defs.sort_by_key(|rows, i| rows.column::<0>()[i]);
        let mut union_pats: UnionPats<'a> = Soa::with_capacity_in(self.union_pats.len(), alloc);
        for &(p, kind) in self.union_pats.iter() {
            union_pats.push((p, kind));
        }
        union_pats.sort_by_key(|rows, i| rows.column::<0>().get(i).copied().unwrap_or(PatId(0)));
        let mut fields: Fields<'a> = Soa::with_capacity_in(self.fields.len(), alloc);
        for &(e, v, f) in self.fields.iter() {
            fields.push((e, v, f));
        }
        fields.sort_by_key(|rows, i| rows.column::<0>()[i]);
        let mut captures: Captures<'a> = Soa::with_capacity_in(self.captures.len(), alloc);
        for &(c, p, k) in self.captures.iter() {
            captures.push((c, p, k));
        }
        // The captured places, one row per environment field: sorted by closure, stable, so a
        // closure's rows stay in field order; the projection runs stay where they were filed.
        // Each place's run was filed just before its row (capture_places.rs `file_places`), so
        // the runs, taken in row order, are the projection rows in order.
        let mut capture_places: CapturePlaceRows<'a> = Soa::with_capacity_in(self.capture_place_rows.len(), alloc);
        let mut capture_projs: CaptureProjs<'a> = Soa::with_capacity_in(self.capture_proj_rows.len(), alloc);
        for i in 0..self.capture_place_rows.len() {
            let (c, p, start, len, k, t) = self.capture_place_rows[i];
            if start as usize != capture_projs.len() || (start + len) as usize > self.capture_proj_rows.len() {
                return Err(CheckError::Malformed(Malformed::List { column: "capture_projs", start, len }));
            }
            for j in start..start + len {
                let (pk, v, f, pt) = self.capture_proj_rows[j as usize];
                let pt = self.export(&mut ex, pt, At::Expr(c))?;
                capture_projs.push((pk, v, f, pt));
            }
            let t = self.export(&mut ex, t, At::Expr(c))?;
            capture_places.push((c, p, start, len, k, t));
        }
        capture_places.sort_by_key(|rows, i| rows.column::<0>()[i]);
        // One row per closure expression, in `ExprId` order: row `i` is ordinal `i`. The kind
        // is the strongest of its captures' (`analyze_closures`'s rule, which settled it).
        let mut closures: Closures<'a> = Soa::new_in(alloc);
        for i in 0..self.body.exprs.len() {
            let e = ExprId(i as u32);
            if !matches!(self.body.exprs[i], crate::body::Expr::Closure { .. }) {
                continue;
            }
            let mut kind = ClosureKind::Fn;
            let mut upvars = None;
            for &(c, k, env) in self.closure_kinds.iter() {
                if c == e {
                    kind = k;
                    upvars = env;
                    break;
                }
            }
            let t = self.ty.resolve(self.expr_ty[i])?;
            let sig = match self.ty.view(t) {
                TyView::Closure(_, sig) => self.export(&mut ex, sig, At::Expr(e))?,
                _ => ex.out.intern(PNode::Error)?,
            };
            let upvars = upvars.map(|t| self.export(&mut ex, t, At::Expr(e))).transpose()?;
            // Where its signature came from (D148): a closure the walk never reached took no
            // expectation, so every region of it is its maker's (`Inferred`).
            let sig_from = self.closures.iter().find(|c| c.expr == e).map_or(crate::columns::SigFrom::Inferred, |c| c.sig_from);
            closures.push((e, kind, sig, upvars, sig_from));
        }
        // The fields each struct update takes from its base, exported before the diagnostics
        // are made so a field type still unknown is the export's E0282 at the struct
        // expression, never a missing row the flow lowering would read as "not taken".
        let mut fru_fields: FruFields<'a> = Soa::with_capacity_in(self.fru_fields.len(), alloc);
        for i in 0..self.fru_fields.len() {
            let (e, at, t) = self.fru_fields[i];
            let t = self.ty.resolve(t)?;
            let p = self.export(&mut ex, t, At::Expr(e))?;
            fru_fields.push((e, at, p));
        }
        fru_fields.sort_by_key(|rows, i| (rows.column::<0>()[i], rows.column::<1>()[i]));
        let mut default_fields: crate::columns::DefaultFields<'a> = Soa::with_capacity_in(self.default_fields.len(), alloc);
        for &row in self.default_fields.iter() {
            default_fields.push(row);
        }
        default_fields.sort_by_key(|rows, i| (rows.column::<0>()[i], rows.column::<1>()[i]));
        // The stalled auto-trait goals (src/stalled.rs), each with the finding it becomes when it
        // fails on the exact interior, made as a finding of `diags` is made below.
        let mut stalled: Vec<crate::stalled::Stalled, Alloc<'a>> = Vec::with_capacity_in(self.stalled.len(), alloc);
        for si in 0..self.stalled.len() {
            let s = self.stalled[si];
            let d = s.diag;
            let ty = self.export(&mut ex, s.ty, d.at)?;
            let found = match d.found {
                Some(t) => Some(self.export(&mut ex, t, d.at)?),
                None => None,
            };
            let at = self.range_of(d.at);
            if at.end <= at.start {
                return Err(CheckError::DiagWithoutPlace { code: d.code, name: d.name });
            }
            let because = match d.unmet {
                Some((goal, leaf)) => {
                    let goal = self.export_goal(&mut ex, goal, d.at)?;
                    let leaf = match leaf {
                        Some(l) => Some(self.export_goal(&mut ex, l, d.at)?),
                        None => None,
                    };
                    Some(crate::diag::Because::Unmet { goal, leaf })
                }
                None => d.because,
            };
            // No chain: one walked on the superset interior is not the exact interior's.
            let diag = d.export(at, None, found, because, None, None);
            stalled.push(crate::stalled::Stalled { trait_: s.trait_, ty, diag });
        }
        let mut diags: Vec<Diag, Alloc<'a>> = Vec::with_capacity_in(self.diags.len(), alloc);
        // The findings' why-chains, each finding's `Why` a range of it.
        let mut why_rows: Vec<crate::diag::WhyStep, Alloc<'a>> = Vec::with_capacity_in(self.why_steps.len(), alloc);
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
            let at = self.range_of(d.at);
            if at.end <= at.start && !d.code.covers_the_item() {
                return Err(CheckError::DiagWithoutPlace { code: d.code, name: d.name }.into());
            }
            let because = match d.unmet {
                Some((goal, leaf)) => {
                    let goal = self.export_goal(&mut ex, goal, d.at)?;
                    let leaf = match leaf {
                        Some(l) => Some(self.export_goal(&mut ex, l, d.at)?),
                        None => None,
                    };
                    Some(crate::diag::Because::Unmet { goal, leaf })
                }
                None => d.because,
            };
            let why = match d.why {
                Some(w) => Some(self.export_why(&mut ex, w, d.at, &mut why_rows)?),
                None => None,
            };
            let undecided = match d.undecided {
                Some(u) => Some(self.export_undecided(&mut ex, u, d.at)?),
                None => None,
            };
            diags.push(d.export(at, expected, found, because, why, undecided));
        }
        // Writeback runs only when the walk finished (`needs` here are prefetch hints for next
        // time, not open questions): with no diagnostic, every recorded type must be named.
        if diags.is_empty() {
            // `local_ty` has a row per pattern; only a binding's row is a recorded type.
            let mut binds: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), alloc);
            for p in self.body.pats {
                binds.push(matches!(p, crate::body::Pat::Bind { .. }));
            }
            let mut holes: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.exprs.len(), alloc);
            for e in self.body.exprs {
                holes.push(matches!(e, crate::body::Expr::Missing));
            }
            // A pattern hole (`Pat::Missing`: a tuple's or struct's `..`, in no list) has no
            // value, as an expression hole has none.
            let mut pat_holes: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.body.pats.len(), alloc);
            for p in self.body.pats {
                pat_holes.push(matches!(p, crate::body::Pat::Missing));
            }
            let r = Recs { types: &ex.out, expr_ty: &expr_ty, pat_ty: &pat_ty, local_ty: &local_ty, binds: &binds, holes: &holes, pat_holes: &pat_holes, adjusts: &adjusts, callees: &callees };
            if let Some(at) = first_unknown(r, alloc)? {
                // A node the walk never reached is the lowering's, not an unknown type.
                if let crate::error::Recorded::ExprTy(x) = at
                    && !self.reached.get(x.ix()).copied().unwrap_or(true)
                {
                    return Err(CheckError::Malformed(crate::error::Malformed::Unreached(x)));
                }
                return Err(CheckError::UnknownInCleanBody { at });
            }
        }
        // The fixes' facts (rules/suggest.rs): only where a one-segment name did not resolve.
        let mut expects: Vec<(ExprId, Option<PTy>), Alloc<'a>> = Vec::with_capacity_in(self.named_holes.len(), alloc);
        for i in 0..self.named_holes.len() {
            let (e, expect) = self.named_holes[i];
            let expected = match expect.only_has_type() {
                Some(t) => {
                    let t = self.ty.resolve(t)?;
                    if self.is_ty_var(t) {
                        None
                    } else {
                        // Exporting may report an unknown type once (E0282): a fix's facts add
                        // no finding, so that one is taken back and the hole has no expectation.
                        let n = self.diags.len();
                        let p = self.export(&mut ex, t, At::Expr(e))?;
                        if self.diags.len() == n { Some(p) } else {
                            self.diags.truncate(n);
                            None
                        }
                    }
                }
                None => None,
            };
            expects.push((e, expected));
        }
        let suggests = crate::rules::suggest::collect(&crate::rules::suggest::Facts {
            body: &self.body,
            types: ex.out.view(),
            expr_ty: &expr_ty,
            local_ty: &local_ty,
            diags: &diags,
            holes: &expects,
            alloc,
        })?;
        // The hidden type of each opaque this body defines (rules/opaque_def.rs): the Reference,
        // "Abstract return types": "Each possible return value from the function must resolve
        // to the same concrete type": that type, per opaque key. A hidden type that is not
        // wholly known adds no finding here (the body's own diagnostics say why) and no row.
        let defined = self.defined_hidden()?;
        let mut hidden: Vec<(crate::ids::ItemKey, PTy), Alloc<'a>> = Vec::with_capacity_in(defined.len(), alloc);
        for &(k, t) in defined.iter() {
            let n = self.diags.len();
            let p = self.export(&mut ex, t, At::Body)?;
            let p = match ex.out.view().node(p)? {
                PNode::Future(e, ..) => {
                    let Some(ordinal) = crate::coroutine_id::coroutine_ordinal(&self.body, e) else {
                        return Err(CheckError::Malformed(crate::error::Malformed::Unreached(e)));
                    };
                    let mut ps: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(self.own_params as usize, self.alloc);
                    for i in 0..self.own_params {
                        let is_const = self.own_consts.binary_search_by_key(&i, |&(index, _)| index).is_ok();
                        ps.push(ex.out.intern(if is_const { PNode::ConstParam(i) } else { PNode::Param(i) })?);
                    }
                    let args = ex.out.intern_list(&ps)?;
                    ex.out.intern(PNode::Coroutine { owner: self.body.owner, ordinal, args })?
                }
                _ => p,
            };
            if self.diags.len() == n {
                hidden.push((k, p));
            } else {
                self.diags.truncate(n);
            }
        }
        let mut promoted: Vec<crate::ids::ExprId, Alloc<'a>> = Vec::with_capacity_in(self.promoted.len(), alloc);
        promoted.extend_from_slice(&self.promoted);
        // The future each `.await` holds while suspended (rules/async_fmt.rs `expr_await`):
        // what the flow lowering's `.await` holds across its suspension (blaze-flow-lower
        // coroutine.rs). A future not wholly known adds no finding here (the body's own
        // diagnostics say why) and no row: the lowering refuses that `.await` by name.
        let mut awaited: Vec<(crate::ids::ExprId, PTy), Alloc<'a>> = Vec::with_capacity_in(self.awaited.len(), alloc);
        for i in 0..self.awaited.len() {
            let (e, t) = self.awaited[i];
            let n = self.diags.len();
            let p = self.export(&mut ex, t, At::Expr(e))?;
            if self.diags.len() == n {
                awaited.push((e, p));
            } else {
                self.diags.truncate(n);
            }
        }
        awaited.sort_unstable_by_key(|r| r.0);
        // The impl chosen for each projection of a callee's signature at each call
        // (writeback/selected_rows.rs), over the body's own reads: before they are finished.
        let selected = self.selected_rows(&mut ex, &closures, &expr_ty);
        // Reading an unfiled coroutine supplies a stand-in. Any result of selection
        // using it (including a missing-facts error) belongs to this unfinished round.
        if self.reads.holed() {
            return Ok(crate::Outcome::Need(self.reads.finish_need(None, alloc)));
        }
        let selected = selected?;
        let mut findings: Vec<crate::rules::args_inline::TypedFinding, Alloc<'a>> = Vec::with_capacity_in(self.typed_findings.len(), alloc);
        findings.extend_from_slice(&self.typed_findings);
        findings.sort_unstable_by_key(|f| f.order());
        let mut undecided: Vec<crate::rules::unsafety::Undecided, Alloc<'a>> = Vec::with_capacity_in(self.undecided.len(), alloc);
        undecided.extend_from_slice(&self.undecided);
        undecided.sort_unstable_by_key(|u| u.order());
        // The overloaded indexes that are mutable places (rules/mut_places.rs), sorted.
        let mut index_mut: Vec<crate::ids::ExprId, Alloc<'a>> = Vec::with_capacity_in(self.index_mut.len(), alloc);
        index_mut.extend_from_slice(&self.index_mut);
        index_mut.sort_unstable();
        let mut coroutine_witnesses = Vec::with_capacity_in(self.futures.len(), alloc);
        coroutine_witnesses.extend(self.futures.iter().map(|f| (f.expr, f.witness)));
        coroutine_witnesses.sort_unstable_by_key(|r| r.0);
        let inferred_output = match self.inferred_output_ty {
            Some(t) => Some(self.export(&mut ex, t, At::Expr(self.body.root))?),
            None => None,
        };
        let (reads, needs) = self.reads.finish(alloc);
        Ok(crate::Outcome::Done(Checked {
            findings,
            undecided,
            inferred_output,
            index_mut,
            suggests,
            hidden,
            promoted,
            stalled,
            coroutine_witnesses,
            awaited,
            selected,
            fru_fields,
            default_fields,
            types: ex.out,
            expr_ty,
            pat_ty,
            local_ty,
            adjusts,
            callees,
            expr_defs,
            const_args,
            pat_const_args,
            pat_defs,
            union_pats,
            fields,
            captures,
            capture_places,
            capture_projs,
            closures,
            diags,
            why: why_rows,
            reads,
            needs,
        }))
    }

}

/// The recorded facts of one body, as written back: what [`first_unknown`] scans.
#[derive(Clone, Copy)]
struct Recs<'r, 'a> {
    types: &'r PortableTys<'a>,
    expr_ty: &'r [PTy],
    pat_ty: &'r [PTy],
    local_ty: &'r [PTy],
    /// Whether each pattern is a binding (whose `local_ty` row is recorded).
    binds: &'r [bool],
    /// Whether each expression is `Expr::Missing`: a hole carries no value, so its row records
    /// nothing. The lowering writes one for every node a desugaring orphaned (a destructuring
    /// assignment's assignee: Reference, "Assignee expressions": not a value expression).
    holes: &'r [bool],
    pat_holes: &'r [bool],
    adjusts: &'r Adjusts<'a>,
    callees: &'r Callees<'a>,
}

/// The first recorded fact of a clean body that holds the error type or a hole, in the order
/// expression types, pattern types, binding types, adjustments, callees' arguments. A table's
/// children come before their parent (`Malformed::TypeOrder`), so one pass in index order
/// marks every node holding one: no recursion, linear in the table.
fn first_unknown(r: Recs<'_, '_>, alloc: Alloc<'_>) -> Result<Option<crate::error::Recorded>, CheckError> {
    use crate::error::Recorded;
    let v = r.types.view();
    let mut bad: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(v.len(), alloc);
    for &n in v.nodes() {
        let any = |l: List| -> Result<bool, CheckError> { Ok(v.list(l)?.iter().any(|c| bad.get(c.0 as usize).copied().unwrap_or(true))) };
        let one = |c: PTy| bad.get(c.0 as usize).copied().unwrap_or(true);
        let held = match n {
            PNode::Error | PNode::Infer => true,
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::ConstParam(_)
            | PNode::ExAuto(_)
            | PNode::Foreign(_) => false,
            PNode::Ref(_, c) | PNode::Ptr(_, c) | PNode::Slice(c) | PNode::ConstValue { ty: c, .. } => one(c),
            PNode::Closure { args, .. } | PNode::Coroutine { args, .. } => any(args)?,
            PNode::Array(a, b) | PNode::Future(_, a, b) => one(a) || one(b),
            PNode::Pat(c, l) => one(c) || any(l)?,
            PNode::ExProj { args, term, .. } => one(term) || any(args)?,
            PNode::Tuple(l)
            | PNode::Record(_, l)
            | PNode::Union(l)
            | PNode::Adt(_, l)
            | PNode::FnDef(_, l)
            | PNode::FnPtr(l, _)
            | PNode::Alias { args: l, .. }
            | PNode::Opaque(_, l)
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => any(l)?,
        };
        bad.push(held);
    }
    let is_bad = |t: PTy| bad.get(t.0 as usize).copied().unwrap_or(true);
    let firsts = [
        r.expr_ty.iter().enumerate().position(|(i, &t)| !r.holes.get(i).copied().unwrap_or(false) && is_bad(t)).map(|i| Recorded::ExprTy(ExprId(i as u32))),
        r.pat_ty.iter().enumerate().position(|(i, &t)| !r.pat_holes.get(i).copied().unwrap_or(false) && is_bad(t)).map(|i| Recorded::PatTy(PatId(i as u32))),
        r.local_ty.iter().enumerate().position(|(i, &t)| r.binds.get(i).copied().unwrap_or(false) && is_bad(t)).map(|i| Recorded::LocalTy(PatId(i as u32))),
        r.adjusts.column::<2>().iter().position(|&t| is_bad(t)).map(|i| Recorded::Adjust(r.adjusts.column::<0>()[i])),
    ];
    if let Some(at) = firsts.into_iter().flatten().next() {
        return Ok(Some(at));
    }
    for i in 0..r.callees.len() {
        if v.list(r.callees.column::<2>()[i])?.iter().any(|&t| is_bad(t)) {
            return Ok(Some(Recorded::Callee(r.callees.column::<0>()[i])));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Bb, Fx};
    use crate::infer::InferCtx;
    use crate::machine::Plant;
    use crate::world::Reads;

    #[test]
    fn writeback_distinguishes_a_missing_tuple_child_from_step_exhaustion() {
        let alloc = Alloc::global();
        let mut body = Bb::new();
        let boolean = body.bool();
        let root = body.tuple(&[boolean]);
        let world = Fx::new();
        let owner = ItemKey(42);
        let mut cx = InferCtx::new_in(alloc);
        let reads = Reads::new_in(&world, alloc);
        let mut machine = Machine::new_in(body.view(owner, root), reads, &mut cx, Plant::None, alloc).expect("checker");
        let child = machine.common.bool_;
        let children = machine.ty.mk_list(&[child]).expect("elements");
        let parent = machine.ty.mk(TyView::Tuple(children)).expect("tuple");
        let mut ex = Export { out: PortableTys::new_in(alloc), cache: HashMap::new_in(alloc), steps: 0, reported: false };

        // Plant the traversal bug: assemble the list before its child was visited.
        assert_eq!(machine.export_list(&mut ex, parent, children), Err(CheckError::Malformed(Malformed::WritebackChild { owner, parent, child })));
        ex.steps = EXPORT_STEPS;
        assert_eq!(
            machine.export(&mut ex, parent, At::Expr(root)),
            Err(CheckError::Limit(Limit::WritebackSteps { owner, node: parent, steps: EXPORT_STEPS + 1, limit: EXPORT_STEPS })),
        );

        // The ordinary walk of `(true,)` records the one element and gives it type bool.
        ex.steps = 0;
        let tuple = machine.export(&mut ex, parent, At::Expr(root)).expect("tuple writeback");
        let types = ex.out.view();
        let PNode::Tuple(list) = types.node(tuple).expect("tuple") else { panic!("expected (bool,)") };
        let &[element] = types.list(list).expect("elements") else { panic!("expected one element") };
        assert_eq!(types.node(element), Ok(PNode::Bool));
    }
}
