//! Relating two terms: equality and subtyping, iteratively, with variance, the occurs check
//! and generalization.
//!
//! References: rustc_type_ir/src/relate/solver_relating.rs (`SolverRelating::tys`, `regions`,
//! `consts`: the order of the cases), rustc_type_ir/src/relate/combine.rs
//! (`super_combine_tys`, `super_combine_consts`: int and float variables, errors),
//! rustc_type_ir/src/relate.rs (`structurally_relate_tys`: what is compared and with which
//! variance; `StructurallyRelateAliases`), and rustc_infer's `generalize.rs` (the generalizer:
//! which variables are kept, which replaced, the universe and occurs checks).
//!
//! Aliases, as this nightly's solver relates them (solver_relating.rs, the `IsRigid::No` arms
//! of `tys`): relating a non-rigid alias with anything gives a fresh variable, emits the
//! projection goal `alias == var` and relates the variable with the other side. A rigid alias
//! (`RIGID_*` kinds, the reference's `IsRigid::Yes`) relates as a constructor. In structural
//! mode ([`TyCx::unify_structurally`], the reference's `StructurallyRelateAliases::Yes`) every
//! alias relates as a constructor and no goal is emitted.

use crate::cx::{Outlives, RelFrame, TyCx};
use crate::error::TypeError;
use crate::flags::Flags;
use crate::fold::{Hook, IN_ALIAS, RULE_SAME, Step, VarianceOf, ctx_of, element, position, var_node, variance_of};
use crate::ids::{Arg, Const, Node, Region, TAG_CONST, TAG_LIST, TAG_REGION, TAG_TY, Ty, Universe, Variance, ix};
use crate::infer::{Table, Vars};
use crate::kind::{self, Role, ct, re, ty};
use crate::tys::Tys;

/// Whether aliases relate as constructors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Aliases {
    /// Non-rigid aliases emit projection goals (the solver's default).
    Defer,
    /// Every alias relates as a constructor; no goal (`StructurallyRelateAliases::Yes`).
    Structural,
}

impl<'a> TyCx<'a> {
    /// `a == b`. Binds variables and emits goals and region constraints; on an error, undoes
    /// everything it did. `Ok(false)`: nothing was bound.
    pub fn eq(&mut self, a: Arg, b: Arg, vo: &mut dyn VarianceOf) -> Result<bool, TypeError> {
        self.relate(a, b, Variance::Invariant, vo)
    }

    /// `a <: b`.
    pub fn sub(&mut self, a: Ty, b: Ty, vo: &mut dyn VarianceOf) -> Result<bool, TypeError> {
        self.relate(a.arg(), b.arg(), Variance::Covariant, vo)
    }

    /// `a` related to `b` with variance `v` (covariant: `a <: b`).
    pub fn relate(&mut self, a: Arg, b: Arg, v: Variance, vo: &mut dyn VarianceOf) -> Result<bool, TypeError> {
        self.relate_atomic(a.node(), b.node(), v, Aliases::Defer, vo)
    }

    /// `a == b` with every alias related as a rigid constructor, its arguments structurally,
    /// and nothing deferred: no goal is emitted (rustc_type_ir/src/relate.rs,
    /// `StructurallyRelateAliases::Yes`, as solver_relating.rs's `relate_structurally` uses
    /// it). For the solver's alias code, which must not loop on its own goals.
    pub fn unify_structurally(&mut self, a: Arg, b: Arg, vo: &mut dyn VarianceOf) -> Result<bool, TypeError> {
        self.relate_atomic(a.node(), b.node(), Variance::Invariant, Aliases::Structural, vo)
    }

    fn relate_atomic(
        &mut self,
        a: Node,
        b: Node,
        v: Variance,
        aliases: Aliases,
        vo: &mut dyn VarianceOf,
    ) -> Result<bool, TypeError> {
        if a == b {
            return Ok(false);
        }
        let s = self.snapshot();
        self.bound = false;
        match self.relate_raw(a, b, v, aliases, vo) {
            Ok(()) => {
                self.commit(s);
                Ok(self.bound)
            }
            Err(e) => {
                self.rollback_to(s);
                Err(e)
            }
        }
    }

    fn push_rel(&mut self, a: Node, b: Node, v: Variance) {
        self.rel.push(RelFrame { a, b, def: 0, ctx: ctx_of(v), rule: RULE_SAME });
    }

    fn relate_raw(
        &mut self,
        a: Node,
        b: Node,
        v: Variance,
        aliases: Aliases,
        vo: &mut dyn VarianceOf,
    ) -> Result<(), TypeError> {
        self.rel.clear();
        self.rel_seen.clear();
        self.push_rel(a, b, v);
        while let Some(f) = self.rel.pop() {
            if f.a == f.b {
                continue;
            }
            let v = variance_of(f.ctx);
            if v == Variance::Bivariant {
                continue;
            }
            if f.a.tag() != f.b.tag() {
                return Err(TypeError::Mismatch { a: f.a, b: f.b });
            }
            match f.a.tag() {
                TAG_TY => self.relate_tys(f.a.ty(), f.b.ty(), v, aliases, vo)?,
                TAG_REGION => self.relate_regions(f.a.region(), f.b.region(), v)?,
                TAG_CONST => self.relate_consts(f.a.konst(), f.b.konst(), aliases, vo)?,
                _ => {
                    let (la, lb) = (f.a.list(), f.b.list());
                    let len = self.tys.list_len(la);
                    if len != self.tys.list_len(lb) {
                        return Err(TypeError::Arity { a: f.a, b: f.b });
                    }
                    let mut i = len;
                    while i > 0 {
                        i -= 1;
                        let (Some(x), Some(y)) = (self.tys.list_get(la, i), self.tys.list_get(lb, i)) else { break };
                        let ev = element(v, f.rule, f.def, i, len, vo);
                        self.push_rel(x.node(), y.node(), ev);
                    }
                }
            }
        }
        Ok(())
    }

    /// `SolverRelating::tys` with `super_combine_tys`.
    fn relate_tys(
        &mut self,
        a: Ty,
        b: Ty,
        v: Variance,
        aliases: Aliases,
        vo: &mut dyn VarianceOf,
    ) -> Result<(), TypeError> {
        let a = self.shallow_resolve(a);
        let b = self.shallow_resolve(b);
        if a == b {
            return Ok(());
        }
        let (ka, kb) = (self.tys.kind_of(a), self.tys.kind_of(b));
        match (ka, kb) {
            (ty::TY_VAR, ty::TY_VAR) => {
                match v {
                    Variance::Covariant => {
                        let g = self.tys.pred_subtype(a, b);
                        self.goals.push(g);
                    }
                    Variance::Contravariant => {
                        let g = self.tys.pred_subtype(b, a);
                        self.goals.push(g);
                    }
                    _ => {
                        let (va, vb) = (self.var_of(a), self.var_of(b));
                        self.vars.union(Table::Ty, va, vb);
                        self.bound = true;
                    }
                }
                return Ok(());
            }
            _ if aliases == Aliases::Defer && kind::is_unnormalized_alias(ka) => {
                let var = self.next_ty_var();
                let g = self.tys.pred_projection(a.arg(), var.arg());
                self.goals.push(g);
                self.push_rel(var.node(), b.node(), v);
                return Ok(());
            }
            _ if aliases == Aliases::Defer && kind::is_unnormalized_alias(kb) => {
                let var = self.next_ty_var();
                let g = self.tys.pred_projection(b.arg(), var.arg());
                self.goals.push(g);
                self.push_rel(a.node(), var.node(), v);
                return Ok(());
            }
            (ty::TY_VAR, _) => {
                let va = self.var_of(a);
                let g = self.generalize(Table::Ty, va, b.node(), v, aliases, vo)?;
                self.vars.bind(Table::Ty, va, g.index());
                self.bound = true;
                // `a` is now `g`: relate it with `b` (for the regions and variables
                // generalization made).
                self.push_rel(a.node(), b.node(), v);
                return Ok(());
            }
            (_, ty::TY_VAR) => {
                let vb = self.var_of(b);
                let g = self.generalize(Table::Ty, vb, a.node(), v.xform(Variance::Contravariant), aliases, vo)?;
                self.vars.bind(Table::Ty, vb, g.index());
                self.bound = true;
                self.push_rel(a.node(), b.node(), v);
                return Ok(());
            }
            (ty::ERROR, _) | (_, ty::ERROR) => {
                self.tainted = true;
                return Ok(());
            }
            (ty::INT_VAR, ty::INT_VAR) => {
                let (va, vb) = (self.var_of(a), self.var_of(b));
                self.vars.union(Table::Int, va, vb);
                self.bound = true;
                return Ok(());
            }
            (ty::INT_VAR, ty::INT | ty::UINT) => {
                let va = self.var_of(a);
                self.vars.bind(Table::Int, va, b.index());
                self.bound = true;
                return Ok(());
            }
            (ty::INT | ty::UINT, ty::INT_VAR) => {
                let vb = self.var_of(b);
                self.vars.bind(Table::Int, vb, a.index());
                self.bound = true;
                return Ok(());
            }
            (ty::FLOAT_VAR, ty::FLOAT_VAR) => {
                let (va, vb) = (self.var_of(a), self.var_of(b));
                self.vars.union(Table::Float, va, vb);
                self.bound = true;
                return Ok(());
            }
            (ty::FLOAT_VAR, ty::FLOAT) => {
                let va = self.var_of(a);
                self.vars.bind(Table::Float, va, b.index());
                self.bound = true;
                return Ok(());
            }
            (ty::FLOAT, ty::FLOAT_VAR) => {
                let vb = self.var_of(b);
                self.vars.bind(Table::Float, vb, a.index());
                self.bound = true;
                return Ok(());
            }
            _ => {}
        }
        if ka != kb || kind::is_infer(ka) {
            return Err(TypeError::Mismatch { a: a.node(), b: b.node() });
        }
        // Structural: one pair is related once per relation (the reference's `cache`).
        let key = u64::from(a.index()) | u64::from(b.index()) << 30 | u64::from(ctx_of(v)) << 60;
        if self.rel_seen.get(key).is_some() {
            return Ok(());
        }
        self.rel_seen.insert(key, 1);
        self.structural(a.node(), b.node(), v)
    }

    /// Two rows of one kind: their numbers and items equal, their children related.
    fn structural(&mut self, a: Node, b: Node, v: Variance) -> Result<(), TypeError> {
        let (k, a1, b1) = self.tys.raw(a);
        let (_, a2, b2) = self.tys.raw(b);
        let d = self.tys.desc(a);
        let same = |r: Role, x: u32, y: u32| r == Role::Child || x == y;
        if !same(d.a, a1, a2) || !same(d.b, b1, b2) {
            return Err(TypeError::Mismatch { a, b });
        }
        let def = if d.a == Role::Def { a1 } else { 0 };
        if d.b == Role::Child {
            let (pv, rule) = position(a.tag(), k, true);
            self.rel.push(RelFrame { a: Node(b1), b: Node(b2), def, ctx: ctx_of(v.xform(pv)), rule });
        }
        if d.a == Role::Child {
            let (pv, rule) = position(a.tag(), k, false);
            self.rel.push(RelFrame { a: Node(a1), b: Node(a2), def, ctx: ctx_of(v.xform(pv)), rule });
        }
        Ok(())
    }

    /// `SolverRelating::regions`: a constraint per direction.
    fn relate_regions(&mut self, a: Region, b: Region, v: Variance) -> Result<(), TypeError> {
        let (ka, kb) = (self.tys.re_kind_of(a), self.tys.re_kind_of(b));
        if ka == re::BOUND || kb == re::BOUND {
            return Err(TypeError::BoundRegion { a: a.node(), b: b.node() });
        }
        if ka == re::ERROR || kb == re::ERROR {
            self.tainted = true;
            return Ok(());
        }
        match v {
            // `&'a u8 <: &'b u8` needs `'a: 'b`.
            Variance::Covariant => self.outlives.push(Outlives { longer: a, shorter: b }),
            Variance::Contravariant => self.outlives.push(Outlives { longer: b, shorter: a }),
            _ => {
                self.outlives.push(Outlives { longer: a, shorter: b });
                self.outlives.push(Outlives { longer: b, shorter: a });
            }
        }
        Ok(())
    }

    /// `super_combine_consts`: consts are always equated.
    fn relate_consts(&mut self, a: Const, b: Const, aliases: Aliases, vo: &mut dyn VarianceOf) -> Result<(), TypeError> {
        let a = self.shallow_resolve_const(a);
        let b = self.shallow_resolve_const(b);
        if a == b {
            return Ok(());
        }
        let (ka, kb) = (self.tys.ct_kind_of(a), self.tys.ct_kind_of(b));
        match (ka, kb) {
            (ct::VAR, ct::VAR) => {
                let (va, vb) = (self.tys.ct.a(ix(a.0)), self.tys.ct.a(ix(b.0)));
                self.vars.union(Table::Const, va, vb);
                self.bound = true;
                return Ok(());
            }
            (ct::UNEVALUATED, _) if aliases == Aliases::Defer => {
                let g = self.tys.pred_projection(a.arg(), b.arg());
                self.goals.push(g);
                return Ok(());
            }
            (_, ct::UNEVALUATED) if aliases == Aliases::Defer => {
                let g = self.tys.pred_projection(b.arg(), a.arg());
                self.goals.push(g);
                return Ok(());
            }
            (ct::VAR, _) => {
                let va = self.tys.ct.a(ix(a.0));
                let g = self.generalize(Table::Const, va, b.node(), Variance::Invariant, aliases, vo)?;
                self.vars.bind(Table::Const, va, g.index());
                self.bound = true;
                self.push_rel(a.node(), b.node(), Variance::Invariant);
                return Ok(());
            }
            (_, ct::VAR) => {
                let vb = self.tys.ct.a(ix(b.0));
                let g = self.generalize(Table::Const, vb, a.node(), Variance::Invariant, aliases, vo)?;
                self.vars.bind(Table::Const, vb, g.index());
                self.bound = true;
                self.push_rel(a.node(), b.node(), Variance::Invariant);
                return Ok(());
            }
            (ct::ERROR, _) | (_, ct::ERROR) => {
                self.tainted = true;
                return Ok(());
            }
            _ => {}
        }
        if ka != kb {
            return Err(TypeError::Mismatch { a: a.node(), b: b.node() });
        }
        self.structural(a.node(), b.node(), Variance::Invariant)
    }

    #[inline]
    fn var_of(&self, t: Ty) -> u32 {
        self.tys.ty.a(ix(t.0))
    }

    /// The generalization of `term` for the variable `var` of `table` related to it with
    /// variance `v` (rustc_infer's `generalize.rs`, `Generalizer`): a term the variable can be
    /// bound to, with the variable's universe able to name all of it, and fresh variables
    /// where a relation other than equality must still be checked.
    fn generalize(
        &mut self,
        table: Table,
        var: u32,
        term: Node,
        v: Variance,
        aliases: Aliases,
        vo: &mut dyn VarianceOf,
    ) -> Result<Node, TypeError> {
        let root = self.vars.find(table, var);
        let u = self.vars.universe_of(table, root);
        let f = self.tys.node_flags(term);
        let mask = Flags::INFER | Flags::PLACEHOLDER | Flags::FREE_REGIONS | Flags::NEEDS_NORMALIZE;
        if !f.has(mask) {
            return Ok(term);
        }
        let root_node = var_node(&mut self.tys, table, root);
        let mut h = Generalize { table, root, root_node, u, structural: aliases == Aliases::Structural };
        self.eng.run(&mut self.tys, &mut self.vars, vo, &mut h, term, ctx_of(v))
    }
}

/// The generalizer's hook.
struct Generalize {
    table: Table,
    root: u32,
    root_node: Node,
    u: Universe,
    structural: bool,
}

impl Generalize {
    /// A variable of `t`: the occurs check, its value, or it kept or replaced.
    fn var(&mut self, tys: &mut Tys<'_>, vars: &mut Vars<'_>, t: Table, w: u32, n: Node, ctx: u8) -> Result<Step, TypeError> {
        let r = vars.root(t, w);
        if t == self.table && r == self.root {
            return Err(TypeError::Cyclic { var: self.root_node, term: n });
        }
        if let Some(x) = vars.value(t, r) {
            let node = if t == Table::Const { Const(x).node() } else { Ty(x).node() };
            return Ok(Step::Again(node));
        }
        let uw = vars.universe_of(t, r);
        // Consts are always equated: a const variable is kept wherever it can be named.
        let invariant = t == Table::Const || variance_of(ctx) == Variance::Invariant;
        if invariant && self.u.can_name(uw) {
            return Ok(Step::Replace(var_node(tys, t, r)));
        }
        let nv = vars.new_var(t, self.u);
        if ctx & IN_ALIAS != 0 {
            // Inside an alias the new variable is equated with the old one, so the alias does
            // not become ambiguous (generalize.rs, `in_alias`). The union lowers the class's
            // universe to `u`.
            vars.union(t, r, nv);
        }
        Ok(Step::Replace(var_node(tys, t, nv)))
    }
}

impl Hook for Generalize {
    type Error = TypeError;

    #[inline(always)]
    fn skip(&self, f: Flags, _: u32) -> bool {
        !f.has(Flags::INFER | Flags::PLACEHOLDER | Flags::FREE_REGIONS | Flags::NEEDS_NORMALIZE)
    }

    fn pre(&mut self, tys: &mut Tys<'_>, vars: &mut Vars<'_>, n: Node, _depth: u32, ctx: u8) -> Result<Step, TypeError> {
        let f = tys.node_flags(n);
        if !f.has(Flags::INFER | Flags::PLACEHOLDER | Flags::FREE_REGIONS | Flags::NEEDS_NORMALIZE) {
            return Ok(Step::Keep);
        }
        let (k, a, _) = tys.raw(n);
        match n.tag() {
            TAG_TY => match k {
                ty::TY_VAR => self.var(tys, vars, Table::Ty, a, n, ctx),
                ty::INT_VAR | ty::FLOAT_VAR => Ok(Step::Keep),
                ty::PLACEHOLDER => {
                    if self.u.can_name(Universe(a)) {
                        Ok(Step::Keep)
                    } else {
                        Err(TypeError::Universe { var: self.root_node, placeholder: n, universe: self.u })
                    }
                }
                _ if !self.structural && kind::is_unnormalized_alias(k) && ctx & IN_ALIAS == 0 && !f.escapes(0) => {
                    // The new solver replaces an alias by a variable, which relating then
                    // ties to the alias with a projection goal (`generalize_alias_ty`).
                    let nv = vars.new_var(Table::Ty, self.u);
                    Ok(Step::Replace(tys.ty_var(nv).node()))
                }
                _ => Ok(Step::Descend),
            },
            TAG_REGION => {
                let ur = match k {
                    re::BOUND | re::ERASED | re::ERROR => return Ok(Step::Keep),
                    re::VAR => vars.region_universe(a),
                    re::PLACEHOLDER => Universe(a),
                    _ => Universe::ROOT,
                };
                if variance_of(ctx) == Variance::Invariant && self.u.can_name(ur) {
                    return Ok(Step::Keep);
                }
                let nv = vars.new_region_var(self.u);
                Ok(Step::Replace(tys.re_var(nv).node()))
            }
            TAG_CONST => match k {
                ct::VAR => self.var(tys, vars, Table::Const, a, n, ctx),
                ct::PLACEHOLDER => {
                    if self.u.can_name(Universe(a)) {
                        Ok(Step::Keep)
                    } else {
                        Err(TypeError::Universe { var: self.root_node, placeholder: n, universe: self.u })
                    }
                }
                _ => Ok(Step::Descend),
            },
            _ => {
                debug_assert!(n.tag() == TAG_LIST);
                Ok(Step::Descend)
            }
        }
    }
}
