//! The inference context of one task: the arena, the variable tables, the region constraints
//! and goals that relating emits, and the scratch every pass reuses
//! (rustc_type_ir/src/infer_ctxt.rs, `InferCtxtLike`: this is the half of it that is about
//! types; the solver half is `crate::solve`).

use core::convert::Infallible;

use blaze_own::{Alloc, Vec};

use crate::error::FoldError;
use crate::fold::{AllInvariant, BoundTo, Engine, ReplaceBound, Resolve, Shift, ShiftIn, Subst, ctx_of};
use crate::ids::{Const, List, Node, Pred, Region, Ty, Universe, Variance};
use crate::infer::{Table, VarSnapshot, Vars};
use crate::kind::{ct, ty};
use crate::memo::Memo;
use crate::tys::Tys;

/// `longer: shorter`, from relating two regions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Outlives {
    pub longer: Region,
    pub shorter: Region,
}

/// A position to roll everything of the context back to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    vars: VarSnapshot,
    outlives: u32,
    goals: u32,
    tainted: bool,
}

/// One frame of relate's stack.
#[derive(Clone, Copy)]
pub(crate) struct RelFrame {
    pub(crate) a: Node,
    pub(crate) b: Node,
    pub(crate) def: u32,
    pub(crate) ctx: u8,
    pub(crate) rule: u8,
}

/// The context (see the module).
pub struct TyCx<'a> {
    pub tys: Tys<'a>,
    pub vars: Vars<'a>,
    pub(crate) outlives: Vec<Outlives, Alloc<'a>>,
    pub(crate) goals: Vec<Pred, Alloc<'a>>,
    pub(crate) eng: Engine<'a>,
    pub(crate) made: Memo<'a>,
    pub(crate) rel: Vec<RelFrame, Alloc<'a>>,
    pub(crate) rel_seen: Memo<'a>,
    pub(crate) canon_map: Memo<'a>,
    pub(crate) canon_stack: Vec<crate::canon::CanonFrame, Alloc<'a>>,
    pub(crate) canon_out: Vec<Node, Alloc<'a>>,
    pub(crate) tainted: bool,
    /// Set by relate when it binds or joins a variable.
    pub(crate) bound: bool,
}

impl<'a> TyCx<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> TyCx<'a> {
        TyCx::with_tys(Tys::new_in(alloc), alloc)
    }

    /// A context over an arena the caller made (sized for its task).
    pub fn with_tys(tys: Tys<'a>, alloc: Alloc<'a>) -> TyCx<'a> {
        TyCx {
            tys,
            vars: Vars::new_in(alloc),
            outlives: Vec::new_in(alloc),
            goals: Vec::new_in(alloc),
            eng: Engine::new_in(alloc),
            made: Memo::new_in(alloc),
            rel: Vec::new_in(alloc),
            rel_seen: Memo::new_in(alloc),
            canon_map: Memo::new_in(alloc),
            canon_stack: Vec::new_in(alloc),
            canon_out: Vec::new_in(alloc),
            tainted: false,
            bound: false,
        }
    }

    // ---- variables -----------------------------------------------------------------------

    /// A new type variable in the current universe (`next_ty_infer`).
    pub fn next_ty_var(&mut self) -> Ty {
        let u = self.vars.universe();
        self.next_ty_var_in(u)
    }

    pub fn next_ty_var_in(&mut self, u: Universe) -> Ty {
        let v = self.vars.new_var(Table::Ty, u);
        self.tys.ty_var(v)
    }

    /// A new `{integer}` variable.
    pub fn next_int_var(&mut self) -> Ty {
        let v = self.vars.new_var(Table::Int, Universe::ROOT);
        self.tys.int_var(v)
    }

    /// A new `{float}` variable.
    pub fn next_float_var(&mut self) -> Ty {
        let v = self.vars.new_var(Table::Float, Universe::ROOT);
        self.tys.float_var(v)
    }

    pub fn next_region_var(&mut self) -> Region {
        let u = self.vars.universe();
        self.next_region_var_in(u)
    }

    pub fn next_region_var_in(&mut self, u: Universe) -> Region {
        let v = self.vars.new_region_var(u);
        self.tys.re_var(v)
    }

    pub fn next_const_var(&mut self) -> Const {
        let u = self.vars.universe();
        self.next_const_var_in(u)
    }

    pub fn next_const_var_in(&mut self, u: Universe) -> Const {
        let v = self.vars.new_var(Table::Const, u);
        self.tys.const_var(v)
    }

    /// A bound variable's value, an unbound variable's root, any other type itself
    /// (`shallow_resolve`). A value may itself be a variable (a type variable bound to an
    /// integer variable, or to another type variable by generalization): it is followed until
    /// it is not a bound variable, as rustc's `shallow_resolve` does.
    #[inline]
    pub fn shallow_resolve(&mut self, mut t: Ty) -> Ty {
        loop {
            let i = crate::ids::ix(t.0);
            let k = self.tys.ty.kind(i);
            let table = match k {
                ty::TY_VAR => Table::Ty,
                ty::INT_VAR => Table::Int,
                ty::FLOAT_VAR => Table::Float,
                _ => return t,
            };
            let v = self.tys.ty.a(i);
            let r = self.vars.find(table, v);
            match self.vars.value(table, r) {
                Some(x) => t = Ty(x),
                None if r == v => return t,
                None => {
                    return match table {
                        Table::Ty => self.tys.ty_var(r),
                        Table::Int => self.tys.int_var(r),
                        _ => self.tys.float_var(r),
                    };
                }
            }
        }
    }

    /// [`TyCx::shallow_resolve`] without writing: no path halving, no row interned, so it
    /// takes `&self` (the solver's walks read terms through a shared borrow). An unbound
    /// variable gives its root's row, which exists: a variable's row is interned when the
    /// variable is made, and a root is a variable some term named.
    #[inline]
    pub fn probe(&self, mut t: Ty) -> Ty {
        loop {
            let i = crate::ids::ix(t.0);
            let k = self.tys.ty.kind(i);
            let table = match k {
                ty::TY_VAR => Table::Ty,
                ty::INT_VAR => Table::Int,
                ty::FLOAT_VAR => Table::Float,
                _ => return t,
            };
            let v = self.tys.ty.a(i);
            let r = self.vars.root(table, v);
            match self.vars.value(table, r) {
                Some(x) => t = Ty(x),
                None if r == v => return t,
                None => return Ty(self.var_row(crate::ids::TAG_TY, k, r)),
            }
        }
    }

    /// [`TyCx::probe`] of a const.
    #[inline]
    pub fn probe_const(&self, mut c: Const) -> Const {
        loop {
            if self.tys.ct_kind_of(c) != ct::VAR {
                return c;
            }
            let v = self.tys.ct.a(crate::ids::ix(c.0));
            let r = self.vars.root(Table::Const, v);
            match self.vars.value(Table::Const, r) {
                Some(x) => c = Const(x),
                None if r == v => return c,
                None => return Const(self.var_row(crate::ids::TAG_CONST, ct::VAR, r)),
            }
        }
    }

    /// The row of variable `v` of kind `k` in class `tag`, which must exist.
    pub(crate) fn var_row(&self, tag: u32, k: u8, v: u32) -> u32 {
        let rows = if tag == crate::ids::TAG_TY { &self.tys.ty } else { &self.tys.ct };
        match rows.find(k, v, 0) {
            crate::index::Probe::Found(id) => id,
            crate::index::Probe::Vacant(_) => {
                panic!("blaze-ty: variable {v} of kind {k} is a class's root but has no row (a variable made without its row)")
            }
        }
    }

    /// The term of variable `v` (its row exists: a variable's row is interned when it is
    /// made), as a node: what a caller that names variables by [`crate::VarId`] (blaze-solve's
    /// bridge, reading the binding log) turns back into a term. `None` past the variable count.
    pub fn var_node(&self, v: crate::infer::VarId) -> Option<Node> {
        use crate::ids::VarKind;
        if v.index as usize >= self.vars.var_count(v.kind) {
            return None;
        }
        let (rows, k, tag) = match v.kind {
            VarKind::Ty => (&self.tys.ty, ty::TY_VAR, crate::ids::TAG_TY),
            VarKind::Int => (&self.tys.ty, ty::INT_VAR, crate::ids::TAG_TY),
            VarKind::Float => (&self.tys.ty, ty::FLOAT_VAR, crate::ids::TAG_TY),
            VarKind::Const => (&self.tys.ct, ct::VAR, crate::ids::TAG_CONST),
            VarKind::Region => (&self.tys.re, crate::kind::re::VAR, crate::ids::TAG_REGION),
        };
        match rows.find(k, v.index, 0) {
            crate::index::Probe::Found(id) => Some(Node((id << 2) | tag)),
            crate::index::Probe::Vacant(_) => None,
        }
    }

    /// [`TyCx::shallow_resolve`] of a const.
    #[inline]
    pub fn shallow_resolve_const(&mut self, mut c: Const) -> Const {
        loop {
            if self.tys.ct_kind_of(c) != ct::VAR {
                return c;
            }
            let v = self.tys.ct.a(crate::ids::ix(c.0));
            let r = self.vars.find(Table::Const, v);
            match self.vars.value(Table::Const, r) {
                Some(x) => c = Const(x),
                None if r == v => return c,
                None => return self.tys.const_var(r),
            }
        }
    }

    /// The universe of a type, int, float or const variable's class, or of a region variable.
    pub fn universe_of(&self, n: Node) -> Option<Universe> {
        let (k, v, _) = self.tys.raw(n);
        match n.tag() {
            crate::ids::TAG_TY if k == ty::TY_VAR => Some(self.vars.universe_of(Table::Ty, v)),
            crate::ids::TAG_REGION if k == crate::kind::re::VAR => Some(self.vars.region_universe(v)),
            crate::ids::TAG_CONST if k == ct::VAR => Some(self.vars.universe_of(Table::Const, v)),
            _ => None,
        }
    }

    // ---- passes --------------------------------------------------------------------------

    /// Every variable in `n` to its value, unbound ones to their root
    /// (`resolve_vars_if_possible`). Walks only where the flags say there are variables.
    pub fn resolve(&mut self, n: Node) -> Node {
        if !self.tys.node_flags(n).has(crate::flags::Flags::TY_INFER | crate::flags::Flags::CT_INFER) {
            return n;
        }
        let r: Result<Node, Infallible> =
            self.eng.run(&mut self.tys, &mut self.vars, &mut AllInvariant, &mut Resolve, n, ctx_of(Variance::Invariant));
        match r {
            Ok(x) => x,
            Err(never) => match never {},
        }
    }

    pub fn resolve_ty(&mut self, t: Ty) -> Ty {
        self.resolve(t.node()).ty()
    }

    /// `n` with its generic parameters replaced by `args` (`EarlyBinder::instantiate`).
    /// Arguments landing under binders have their escaping bound variables shifted in.
    pub fn subst(&mut self, n: Node, args: List) -> Result<Node, FoldError> {
        if !self.tys.node_flags(n).has(crate::flags::Flags::PARAM) {
            return Ok(n);
        }
        let mut h = Subst { args };
        self.eng.run(&mut self.tys, &mut self.vars, &mut AllInvariant, &mut h, n, ctx_of(Variance::Invariant))
    }

    pub fn subst_ty(&mut self, t: Ty, args: List) -> Result<Ty, FoldError> {
        Ok(self.subst(t.node(), args)?.ty())
    }

    /// `n` with its escaping bound variables shifted in by `by` binders (`shift_vars`).
    pub fn shift_in(&mut self, n: Node, by: u32) -> Node {
        if by == 0 || !self.tys.node_flags(n).escapes(0) {
            return n;
        }
        let r = self.eng.run(&mut self.tys, &mut self.vars, &mut AllInvariant, &mut ShiftIn(by), n, ctx_of(Variance::Invariant));
        match r {
            Ok(x) => x,
            Err(never) => match never {},
        }
    }

    /// `n` with its escaping bound variables shifted out by `by` binders.
    pub fn shift_out(&mut self, n: Node, by: u32) -> Result<Node, FoldError> {
        if by == 0 || !self.tys.node_flags(n).escapes(0) {
            return Ok(n);
        }
        let mut h = Shift { by, out: true };
        self.eng.run(&mut self.tys, &mut self.vars, &mut AllInvariant, &mut h, n, ctx_of(Variance::Invariant))
    }

    /// The contents of a binder (the node's bound variables of De Bruijn index 0) with its
    /// variables replaced as `to` says (`instantiate_binder_with_*`, `enter_forall`).
    pub fn instantiate(&mut self, n: Node, to: BoundTo) -> Result<Node, FoldError> {
        if !self.tys.node_flags(n).escapes(0) {
            return Ok(n);
        }
        self.made.clear();
        let mut h = ReplaceBound { to, made: &mut self.made };
        self.eng.run(&mut self.tys, &mut self.vars, &mut AllInvariant, &mut h, n, ctx_of(Variance::Invariant))
    }

    /// Enters a new universe and replaces the binder's variables with its placeholders
    /// (`enter_forall`).
    pub fn enter_forall(&mut self, n: Node) -> Result<(Node, Universe), FoldError> {
        let u = self.vars.create_next_universe();
        Ok((self.instantiate(n, BoundTo::Placeholders(u))?, u))
    }

    // ---- snapshots and outputs -----------------------------------------------------------

    pub fn snapshot(&mut self) -> Snapshot {
        let len32 = |n: usize| match crate::ids::row(n) {
            Some(x) => x,
            None => panic!("blaze-ty: a task's goal or constraint buffer is full (rule 18)"),
        };
        Snapshot {
            vars: self.vars.snapshot(),
            outlives: len32(self.outlives.len()),
            goals: len32(self.goals.len()),
            tainted: self.tainted,
        }
    }

    pub fn rollback_to(&mut self, s: Snapshot) {
        self.vars.rollback_to(s.vars);
        self.outlives.truncate(crate::ids::ix(s.outlives));
        self.goals.truncate(crate::ids::ix(s.goals));
        self.tainted = s.tainted;
    }

    pub fn commit(&mut self, s: Snapshot) {
        self.vars.commit(s.vars);
    }

    /// The region constraints relating has emitted.
    pub fn outlives(&self) -> &[Outlives] {
        &self.outlives
    }

    pub fn clear_outlives(&mut self) {
        self.outlives.clear();
    }

    /// The goals relating has emitted and not yet taken (projection goals of aliases,
    /// subtyping of two variables).
    pub fn goals(&self) -> &[Pred] {
        &self.goals
    }

    /// Takes the emitted goals into `out`.
    pub fn take_goals(&mut self, out: &mut Vec<Pred, Alloc<'_>>) {
        out.extend_from_slice(&self.goals);
        self.goals.clear();
    }

    /// Whether an error type, region or const was related.
    pub fn tainted(&self) -> bool {
        self.tainted
    }

    /// Bytes held by the context's columns.
    pub fn bytes(&self) -> usize {
        self.tys.bytes() + self.vars.bytes()
    }
}
