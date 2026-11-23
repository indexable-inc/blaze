//! Mutable places: an overloaded dereference that a place used mutably goes through calls
//! `DerefMut::deref_mut`, not `Deref::deref`, and an overloaded index `IndexMut::index_mut`,
//! not `Index::index`: one rule for which of each pair a place takes.
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
//! IndexMut. The same rule for an index: the Reference, "Array and slice indexing
//! expressions": "For other types an index expression a[b] is equivalent to
//! *std::ops::Index::index(&a, b), or *std::ops::IndexMut::index_mut(&mut a, b) in a mutable
//! place expression context." The walk types every index with `Index` (rules/field.rs
//! `expr_index`); here each overloaded index on a mutable place's chain (its base after its
//! adjustments not an array or slice indexed by `usize`) asks `Self: IndexMut<Idx>` at the
//! types the walk chose, and where it may hold registers it and lists the expression in
//! `Checked::index_mut`, which the builder reads (blaze-build walk/index.rs). Where it cannot
//! hold, the program writes or uniquely borrows through `Index::index`'s shared reference:
//! E0596 (rustc's borrow check: "cannot borrow data in an index of `T` as mutable"), except
//! below a raw pointer's dereference (`*v[0] = x` with `v[0]: *mut T`), where the index's place
//! is only read for the pointer and `Index::index` is right, as rustc's
//! `try_mutable_overloaded_place_op` leaves it there. The derefs of the indexed base are
//! upgraded too: `b[0] = 2` through a `DerefMut` wrapper records `*DerefMut` on `b`.
//!
//! Linear in the body: each expression joins a chain at most twice (once below a raw pointer,
//! once directly; a marked expression's chain below is marked already, as strongly), and the
//! adjustments are scanned once. No recursion: work lists.

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, Pat, Stmt, UnOp};
use crate::columns::AdjustKind;
use crate::diag::{About, At, DiagCode};
use crate::error::{CheckError, Malformed, Stop};
use crate::ids::{ExprId, ItemKey, LangItem, Mutbl, PatId, UintTy};
use crate::machine::Machine;
use crate::ty::{Cause, Goal, Ty, TyView, TypeCtx};
use crate::world::World;

/// An expression no mutable place goes through.
const UNUSED: u8 = 0;
/// Part of a mutable place only below a raw pointer's dereference (`*p` with `p: *mut T` read
/// out of it): the place itself is only read, for the pointer.
const THROUGH_RAW: u8 = 1;
/// Part of a mutable place with no raw pointer's dereference between it and the root: written
/// or uniquely borrowed itself.
const DIRECT: u8 = 2;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// Upgrades every overloaded dereference a mutable place goes through to `DerefMut` (the
    /// module doc).
    pub(crate) fn mut_places(&mut self) -> Result<(), Stop> {
        let shared_deref = AdjustKind::Deref { overloaded: Some(Mutbl::Not) };
        let any_deref = self.adjust.column::<1>().contains(&shared_deref);
        let any_index = self.body.exprs.iter().any(|x| matches!(x, Expr::Index { .. }));
        if !any_deref && !any_index {
            return Ok(());
        }
        let n = self.body.exprs.len();
        // Per expression: `UNUSED`, `THROUGH_RAW` (a mutable place only below a raw pointer's
        // dereference) or `DIRECT` (the module doc's "IndexMut").
        let mut mutable: Vec<u8, Alloc<'a>> = Vec::new_in(self.alloc);
        for _ in 0..n {
            mutable.push(UNUSED);
        }
        let mut roots: Vec<ExprId, Alloc<'a>> = Vec::new_in(self.alloc);
        self.mutable_roots(&mut roots)?;
        while let Some(root) = roots.pop() {
            let mut x = root;
            let mut how = DIRECT;
            loop {
                let node = self.body.expr(x)?;
                if mutable[x.ix()] >= how {
                    // Its chain below was walked from it already, as strongly.
                    break;
                }
                mutable[x.ix()] = how;
                match node {
                    Expr::Paren(base)
                    | Expr::Field { base, .. }
                    | Expr::TupleField { base, .. }
                    | Expr::Index { base, .. } => x = base,
                    Expr::Unary { op: UnOp::Deref, operand } => {
                        // `*p` of a raw pointer: the place below is only read for the pointer.
                        let t = self.try_structurally_resolve(self.expr_ty[operand.ix()])?;
                        if matches!(self.view(t), TyView::Ptr(..)) {
                            how = THROUGH_RAW;
                        }
                        x = operand
                    }
                    _ => break,
                }
            }
        }
        let adjusted = self.mut_derefs(&mutable)?;
        if any_index {
            self.mut_indexes(&mutable, &adjusted)?;
        }
        Ok(())
    }

    /// The overloaded dereferences of the mutable places become `DerefMut` (the module doc).
    /// Gives each expression's type after its adjustments, when it has any.
    fn mut_derefs(&mut self, mutable: &[u8]) -> Result<Vec<Option<Ty>, Alloc<'a>>, Stop> {
        let shared_deref = AdjustKind::Deref { overloaded: Some(Mutbl::Not) };
        let n = self.body.exprs.len();
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
            if kind != shared_deref || mutable[e.ix()] == UNUSED {
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
        Ok(last)
    }

    /// Each overloaded index that is a mutable place calls `IndexMut::index_mut` where `Self:
    /// IndexMut<Idx>` may hold (that goal registered, the expression listed in `index_mut`);
    /// where it does not, a `DIRECT` one is E0596 and one `THROUGH_RAW` stays `Index::index`
    /// (the module doc).
    fn mut_indexes(&mut self, mutable: &[u8], adjusted: &[Option<Ty>]) -> Result<(), Stop> {
        let body = self.body;
        // The `IndexMut` lang item, read only when a mutable place meets an overloaded index.
        let mut index_mut: Option<Option<ItemKey>> = None;
        for (ix, &node) in body.exprs.iter().enumerate() {
            let Expr::Index { base, index } = node else { continue };
            let how = mutable[ix];
            if how == UNUSED {
                continue;
            }
            let e = ExprId(ix as u32);
            let self_ty = adjusted[base.ix()].unwrap_or(self.expr_ty[base.ix()]);
            let self_ty = self.try_structurally_resolve(self_ty)?;
            let idx_ty = adjusted[index.ix()].unwrap_or(self.expr_ty[index.ix()]);
            let idx_ty = self.try_structurally_resolve(idx_ty)?;
            if self.is_err(self_ty) || self.is_err(idx_ty) {
                continue;
            }
            // The builtin element (rules/field.rs `builtin_index`, which made an integer
            // variable index `usize`): no method.
            let builtin = matches!(self.view(self_ty), TyView::Array(..) | TyView::Slice(_))
                && matches!(self.view(idx_ty), TyView::Uint(UintTy::Usize));
            if builtin {
                continue;
            }
            let tr = match index_mut {
                Some(known) => known,
                None => {
                    let got = self.lang(LangItem::IndexMut)?;
                    index_mut = Some(got);
                    got
                }
            };
            let Some(tr) = tr else {
                if how == DIRECT {
                    self.missing_lang(LangItem::IndexMut, At::Expr(e));
                }
                continue;
            };
            let args = self.mk_list(&[self_ty, idx_ty])?;
            if self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)? {
                self.register_trait(tr, args, Cause::Expr(e))?;
                self.index_mut.push(e);
            } else if how == DIRECT {
                self.diag(DiagCode::IndexNotMut, At::Expr(e), About::found(self_ty));
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
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
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
