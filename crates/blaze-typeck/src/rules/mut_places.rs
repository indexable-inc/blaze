//! Mutable places: an overloaded dereference that a place used mutably goes through calls
//! `DerefMut::deref_mut`, not `Deref::deref`.
//!
//! Follows rustc_hir_typeck/src/place_op.rs (`convert_place_derefs_to_mutable`, called there
//! from method confirmation, assignment, `&mut` and `ref mut` bindings); here it is one pass
//! after the walk (rules/fallback.rs `end_sequence`), before every obligation is selected, so
//! the `DerefMut` goals it registers are proven with the rest.
//!
//! The rule. The Reference, "Operator expressions", "The dereference operator": "On non-pointer
//! types `*x` is equivalent to `*std::ops::Deref::deref(&x)` in an immutable place expression
//! context and `*std::ops::DerefMut::deref_mut(&mut x)` in a mutable place expression context."
//! "Place expressions and value expressions", "Mutability": "For a place expression to be
//! assigned to, mutably borrowed, implicitly mutably borrowed, or bound to a pattern containing
//! `ref mut`, it must be mutable. We call these mutable place expressions." The pinned library
//! (core/src/ops/deref.rs, "Mutable deref coercion"): "If `T` implements `DerefMut<Target =
//! U>`, and `v` is a value of type `T`, then: In mutable contexts, `*v` (where `T` is neither a
//! reference nor a raw pointer) is equivalent to `*DerefMut::deref_mut(&mut v)`."
//!
//! The mutable roots: an expression with a recorded `&mut` (or `&raw mut`) borrow adjustment
//! (a method receiver's autoref, a reborrowing coercion); the operand of `&mut` and `&raw
//! mut`; the left of `=` and of a compound assignment (each place of a destructuring one); the
//! scrutinee of a `let`, `let` condition or `match` whose pattern holds an explicit `ref mut`
//! binding (rustc's `contains_explicit_ref_binding`). From each root the place chain is
//! followed down through parentheses, fields, indexing and `*`; every overloaded dereference
//! recorded on an expression of the chain (an autoderef step, or an explicit `*x`, which the
//! check records on `x`) becomes `DerefMut` when the type it dereferences may implement
//! `DerefMut`, and that goal is registered. Where it does not (an `Rc`), the dereference stays
//! `Deref::deref`, as rustc's `try_mutable_overloaded_place_op` finds no method there: the
//! program writes through a shared reference, which the borrow check refuses (E0596), and the
//! code builder refuses by name meanwhile (blaze-build walk.rs `deref_call`). A world with no
//! `DerefMut` lang item has no type implementing it: the same.
//!
//! Not here: choosing `IndexMut::index_mut` for an overloaded index in a mutable place (the
//! record has no column naming the index method; the builder's `refuse_mut_through_overloaded_index`
//! refuses it by name). The derefs of the indexed base ARE upgraded: `b[0] = 2` through a
//! `DerefMut` wrapper records `*DerefMut` on `b`.
//!
//! Linear in the body: each expression joins the chain at most once (a marked expression's
//! chain below is marked already), and the adjustments are scanned once. No recursion: work
//! lists.

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, Pat, Stmt, UnOp};
use crate::columns::AdjustKind;
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, PatId};
use crate::machine::Machine;
use crate::ty::{Cause, Goal, Ty, TypeCtx};
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Upgrades every overloaded dereference a mutable place goes through to `DerefMut` (the
    /// module doc).
    pub(crate) fn mut_places(&mut self) -> Result<(), Stop> {
        let shared_deref = AdjustKind::Deref { overloaded: Some(Mutbl::Not) };
        if !self.adjust.column::<1>().contains(&shared_deref) {
            return Ok(());
        }
        let n = self.body.exprs.len();
        let mut mutable: Vec<bool, Alloc<'a>> = Vec::new_in(self.alloc);
        for _ in 0..n {
            mutable.push(false);
        }
        let mut roots: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        self.mutable_roots(&mut roots)?;
        while let Some(root) = roots.pop() {
            let mut x = root;
            loop {
                let node = self.body.expr(x)?;
                if mutable[x.ix()] {
                    // Its chain below was walked from it already.
                    break;
                }
                mutable[x.ix()] = true;
                match node {
                    Expr::Paren(base)
                    | Expr::Field { base, .. }
                    | Expr::TupleField { base, .. }
                    | Expr::Index { base, .. }
                    | Expr::Unary { op: UnOp::Deref, operand: base } => x = base,
                    _ => break,
                }
            }
        }
        // Each expression's adjustments, in order: the type an adjustment starts from is the
        // expression's own for its first, else the one the adjustment before it made.
        let mut last: Vec<Option<Ty>, Alloc<'a>> = Vec::new_in(self.alloc);
        for _ in 0..n {
            last.push(None);
        }
        // The `DerefMut` lang item, read only when a mutable place meets an overloaded deref.
        let mut deref_mut: Option<Option<ItemKey>> = None;
        for i in 0..self.adjust.len() {
            let e = self.adjust.column::<0>()[i];
            let kind = self.adjust.column::<1>()[i];
            let target = self.adjust.column::<2>()[i];
            let Some(prev) = last.get_mut(e.ix()) else {
                return Err(Stop::Error(CheckError::Malformed(Malformed::Expr(e))));
            };
            let source = match *prev {
                Some(t) => t,
                None => self.expr_ty[e.ix()],
            };
            *prev = Some(target);
            if kind != shared_deref || !mutable[e.ix()] {
                continue;
            }
            let tr = match deref_mut {
                Some(known) => known,
                None => {
                    let got = self.lang(LangItem::DerefMut)?;
                    deref_mut = Some(got);
                    got
                }
            };
            let Some(tr) = tr else {
                continue;
            };
            let source = self.try_structurally_resolve(source)?;
            let args = self.mk_list(&[source])?;
            if self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)? {
                self.register_trait(tr, args, Cause::Expr(e))?;
                self.adjust.column_mut::<1>()[i] = AdjustKind::Deref { overloaded: Some(Mutbl::Mut) };
            }
        }
        Ok(())
    }

    /// The expressions used as mutable places (the module doc's roots), into `roots`.
    fn mutable_roots(&mut self, roots: &mut Vec<ExprId, Alloc<'a>>) -> Result<(), Stop> {
        let body = self.body;
        let (es, ks) = (self.adjust.column::<0>(), self.adjust.column::<1>());
        for i in 0..es.len() {
            if matches!(ks[i], AdjustKind::Borrow { mutbl: Mutbl::Mut, .. }) {
                roots.push(es[i]);
            }
        }
        let mut lhs: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(self.alloc);
        for &node in body.exprs {
            match node {
                Expr::Ref { mutbl: Mutbl::Mut, operand } | Expr::RawRef { mutbl: Mutbl::Mut, operand } => {
                    roots.push(operand)
                }
                Expr::Assign { lhs: l, .. } | Expr::AssignOp { lhs: l, .. } => lhs.push(l),
                Expr::Let { pat, init } => {
                    if self.binds_ref_mut(pat, &mut pats)? {
                        roots.push(init);
                    }
                }
                Expr::Match { scrutinee, arms } => {
                    for arm in body.arm_list(arms)? {
                        if self.binds_ref_mut(arm.pat, &mut pats)? {
                            roots.push(scrutinee);
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        for &stmt in body.stmts {
            if let Stmt::Let { pat, init: Some(init), .. } = stmt {
                if self.binds_ref_mut(pat, &mut pats)? {
                    roots.push(init);
                }
            }
        }
        // The left of an assignment: a place, or a destructuring one's places (the Reference,
        // "Destructuring assignments": tuples, slices, tuple structs, structs, `_`).
        while let Some(l) = lhs.pop() {
            match body.expr(l)? {
                Expr::Paren(inner) => lhs.push(inner),
                Expr::Tuple { elems } | Expr::Array { elems } => lhs.extend_from_slice(body.expr_list(elems)?),
                Expr::Call { args, .. } => lhs.extend_from_slice(body.expr_list(args)?),
                Expr::Struct { fields, .. } => {
                    for fi in body.field_init_list(fields)? {
                        lhs.push(fi.expr);
                    }
                }
                Expr::Underscore => {}
                _ => roots.push(l),
            }
        }
        Ok(())
    }

    /// Whether pattern `p` holds an explicit `ref mut` binding anywhere (rustc's
    /// `contains_explicit_ref_binding` giving `Mut`). `work` is scratch.
    fn binds_ref_mut(&self, p: PatId, work: &mut Vec<PatId, Alloc<'a>>) -> Result<bool, Stop> {
        let body = self.body;
        work.clear();
        work.push(p);
        while let Some(p) = work.pop() {
            match body.pat(p)? {
                Pat::Bind { ann, sub, .. } => {
                    if ann.by_ref == Some(Mutbl::Mut) {
                        return Ok(true);
                    }
                    if let Some(s) = sub {
                        work.push(s);
                    }
                }
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } => {
                    work.extend_from_slice(body.pat_list(elems)?)
                }
                Pat::Or { alts } => work.extend_from_slice(body.pat_list(alts)?),
                Pat::Ref { inner, .. } => work.push(inner),
                Pat::Struct { fields, .. } => {
                    for fp in body.field_pat_list(fields)? {
                        work.push(fp.pat);
                    }
                }
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
            }
        }
        Ok(false)
    }
}
