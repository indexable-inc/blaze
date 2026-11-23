//! Index expressions that are not the builtin array or slice element, and raw borrows: the
//! forms walk.rs dispatches here (`push_index`, `push_raw_ref`, `index_call`).
//!
//! The Reference, "Array and slice indexing expressions": "For other types an index expression
//! a[b] is equivalent to *std::ops::Index::index(&a, b), or *std::ops::IndexMut::index_mut(&mut
//! a, b) in a mutable place expression context." So an overloaded index is a call of the trait
//! method at `[typeof a, typeof b]` with `a` borrowed in place (its autoderef steps first, as the
//! check recorded them on `a`), and the reference it returns is dereferenced to the PLACE the
//! expression denotes: read, written, borrowed or projected like any other place. A subslice
//! (`&s[1..3]`, `&a[..n]`) is the same call: the range is the struct literal the lowering makes
//! of `a..b` (blaze-lower desugar.rs `range_expr`: `Range { start: a, end: b }`), the impl is the
//! library's (`impl<T, I: SliceIndex<[T]>> Index<I> for [T]`), and the returned `&[T]` is a wide
//! pointer, whose dereference is an unsized place (`V::Unsized`: walk.rs "Unsized places").
//!
//! Which method: the check's, never the build's. blaze-typeck types every index with `Index`
//! (rules/field.rs `expr_index`) and, after the walk, lists each overloaded index that is a
//! mutable place and whose type implements `IndexMut` in `Checked::index_mut`
//! (rules/mut_places.rs, rustc's `convert_place_derefs_to_mutable`); a mutable place through an
//! index of a type without `IndexMut` is the check's E0596 there. So the call is
//! `IndexMut::index_mut(&mut a, b)` (`Adts::index_fn(Mutbl::Mut)`) for a listed expression and
//! `Index::index(&a, b)` for any other: `*index_mut(..)` is the place, written, borrowed or
//! projected like any other (`v[i] = x`, `v[i] += 1` with the place evaluated once, `&mut
//! m[k]`, `v[i].f = x`, `v[i][j] = x`, `v[i].bump()` with its `&mut` autoref, a `ref mut`
//! binding). Each mutable place expression context (an assignment's or a compound
//! assignment's left side, `&mut`, `&raw mut`, a `&mut` autoref) still checks its chain
//! (`refuse_mut_through_overloaded_index`): an overloaded index on it the check did not list
//! must sit below a raw pointer's dereference, else it is refused by name, never written
//! through `Index::index`'s shared reference.
//!
//! The check's autoderef of an array base adds the array's slice as a last step
//! (`steps.push(Step { ty: slice, .. })`) and, when `Index` holds only there, records it on the
//! base as a builtin dereference to `[T]`: it is an unsizing, not a dereference. The build makes
//! it one: the array's place is borrowed thin and widened with the array's length into the
//! `&[T]` the call takes ("Type coercions", "Unsized coercions": "`[T; n]` to `[T]`").
//!
//! The Reference, "Raw borrow operators": "The operand expression of these operators is
//! evaluated in place expression context. `&raw const expr` then creates a const raw pointer of
//! type `*const T` to the given place, and `&raw mut expr` creates a mutable raw pointer of type
//! `*mut T`"; they are for when "creating a reference would introduce incorrect aliasing
//! assumptions": the place's address, with no read and no reference made. "Dynamically Sized
//! Types": "Pointer types to DSTs are sized but have twice the size of the size of pointers to
//! sized types": a raw borrow of an unsized place is the wide pointer that reached it.

use core::alloc::Allocator;

use blaze_lir::Flags;
use blaze_lir::Ty;
use blaze_lir::builder::MemAttrs;
use blaze_typeck::{AdjustKind, Expr, ExprId, Mutbl, PNode, UintTy, UnOp};

use super::{Task, V, Walk, malformed, unsupported};
use crate::{BuildError, Instance};

/// The refusal of a mutable place through an overloaded index (the module doc).
const MUT_INDEX: &str = "a mutable place through an overloaded index the check did not list as `IndexMut::index_mut` \
                         (`Checked::index_mut`, blaze-typeck rules/mut_places.rs) and no raw pointer's dereference is \
                         between: a write or `&mut` through `Index::index`'s shared reference (rustc's E0596/E0594)";

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Whether index expression `e` is the builtin element of an array or a slice: the check's
    /// `builtin_index` took it only for an array or slice base (after autoderef) indexed by
    /// `usize`; every other index it typed is `Index` (rules/field.rs `expr_index`).
    pub(super) fn index_builtin(&self, e: ExprId) -> Result<bool, BuildError> {
        let Expr::Index { base, index } = self.expr(e)? else { return Err(malformed("an index question on another expression")) };
        let past = |_| malformed("a type past the record's table");
        let base_node = self.types.node(self.adjusted_ty(base)?).map_err(past)?;
        let index_node = self.types.node(self.adjusted_ty(index)?).map_err(past)?;
        Ok(matches!(base_node, PNode::Array(..) | PNode::Slice(_)) && matches!(index_node, PNode::Uint(UintTy::Usize)))
    }

    /// Pushes the tasks of index expression `e` as a place (`load`: then its value read out of
    /// it): the builtin element, or the place `*Index::index(&base, index)` points to.
    pub(super) fn push_index(&mut self, e: ExprId, load: bool) -> Result<(), BuildError> {
        let Expr::Index { base, index } = self.expr(e)? else { return Err(malformed("an index task on another expression")) };
        if load {
            self.work.push(Task::LoadPlace(e));
        }
        // The array's place, then the index: Rust's order.
        if self.index_builtin(e)? {
            self.work.push(Task::IndexPlace(e));
            self.work.push(Task::Eval(index));
            return self.push_index_base(base);
        }
        // A `&mut` autoref of the index's own place (a `&mut self` method on `w[i]`).
        let rows = self.adjust_rows(e);
        if !self.checked.index_is_mut(e)
            && self.checked.adjusts.column::<1>()[rows].iter().any(|k| matches!(k, AdjustKind::Borrow { mutbl: Mutbl::Mut, .. }))
        {
            return Err(unsupported(e, MUT_INDEX));
        }
        // Run first to last: `&base` (its autoderef steps applied), the index, the call, then
        // the place its reference points to.
        self.work.push(Task::DerefPlace);
        self.work.push(Task::IndexCall(e));
        self.work.push(Task::Eval(index));
        self.work.push(Task::Ref);
        if self.array_as_slice(base)? {
            // The steps up to the array: its place, borrowed thin; `index_call` widens it.
            let n = self.adjust_rows(base).len() as u32;
            return self.place_derefs(base, n - 1);
        }
        self.work.push(Task::Place(base));
        Ok(())
    }

    /// Whether the base of an overloaded index carries the check's array-to-slice step as its
    /// last adjustment (the module doc): a builtin dereference from an array type.
    fn array_as_slice(&self, base: ExprId) -> Result<bool, BuildError> {
        let rows = self.adjust_rows(base);
        let kinds = &self.checked.adjusts.column::<1>()[rows.start..rows.end];
        let Some(&AdjustKind::Deref { overloaded: None }) = kinds.last() else { return Ok(false) };
        let before = self.ty_before(base, kinds.len() - 1)?;
        if !matches!(self.types.node(before).map_err(|_| malformed("a type past the record's table"))?, PNode::Array(..)) {
            return Ok(false);
        }
        if !kinds.iter().all(|k| *k == AdjustKind::Deref { overloaded: None }) {
            return Err(unsupported(base, "an overloaded index of an array reached through an overloaded dereference"));
        }
        Ok(true)
    }

    /// The call of overloaded index `e`: `Index::index` at `[Self, Idx]` with `&Self`, or
    /// `IndexMut::index_mut` with `&mut Self` when the check listed `e` (the module doc), and
    /// the index on top of the value stack (a thin `&[T; n]` widened to `&[T]` first when the
    /// check's array-to-slice step is on the base); leaves the reference it returns, thin or
    /// wide, for the `DerefPlace` after.
    pub(super) fn index_call(&mut self, e: ExprId) -> Result<(), BuildError> {
        let Expr::Index { base, index } = self.expr(e)? else { return Err(malformed("an index call's task on another expression")) };
        let (m, name) = if self.checked.index_is_mut(e) { (Mutbl::Mut, "index_mut") } else { (Mutbl::Not, "index") };
        let item = self.adts.index_fn(m).ok_or(BuildError::NoLangFn { expr: Some(e), name })?;
        let widen = match self.array_as_slice(base)? {
            false => None,
            true => {
                let n = self.adjust_rows(base).len();
                let before = self.ty_before(base, n - 1)?;
                let PNode::Array(_, len) = self.node_at(base, before)? else {
                    return Err(malformed("an array-to-slice step whose array is not an array at the instance"));
                };
                Some(self.ground_len(len).map_err(|error| Self::ty_err(base, error))?)
            }
        };
        let self_t = self.adjusted_ty(base)?;
        let idx_t = self.adjusted_ty(index)?;
        let out_t = self.expr_ty(e)?;
        let g_self = self.ground_ty(self_t).map_err(|error| Self::ty_err(e, error))?;
        let g_idx = self.ground_ty(idx_t).map_err(|error| Self::ty_err(e, error))?;
        let g_out = self.ground_ty(out_t).map_err(|error| Self::ty_err(e, error))?;
        let g_arg = self.ground.intern(PNode::Ref(m, g_self)).map_err(|_| malformed("the ground table is full"))?;
        let g_ret = self.ground.intern(PNode::Ref(m, g_out)).map_err(|_| malformed("the ground table is full"))?;
        let arepr = self.repr(g_arg).map_err(|error| Self::ty_err(e, error))?;
        let irepr = self.repr(g_idx).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr(g_ret).map_err(|error| Self::ty_err(e, error))?;
        if let Some(len) = widen {
            // `&[T]` is the array's address at 0 and its length at 8 (lib.rs, "Wide pointers").
            let i = self.pop()?;
            let thin = self.pop()?;
            let p = self.scalar(thin, Ty::PTR)?;
            let at = self.temp(arepr);
            self.b.store(Flags::NONE, p, at.0, at.1, MemAttrs::aligned(3));
            let n = self.b.iconst(Ty::I64, u128::from(len));
            self.b.store(Flags::NONE, n, at.0, at.1 + 8, MemAttrs::aligned(3));
            self.push_val(V::Place(at.0, at.1), None);
            self.push_val(i, None);
        }
        let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
        let apass = self.pass(g_arg, arepr, abi, Some(item))?;
        let ipass = self.pass(g_idx, irepr, abi, Some(item))?;
        let rpass = self.pass(g_ret, rrepr, abi, Some(item))?;
        let args = self.ground.intern_list(&[g_self, g_idx]).map_err(|_| malformed("the ground table is full"))?;
        // core's `impl Index<I> for [T]` is `#[track_caller]` (walk/track_caller.rs).
        self.call_instance(Some(e), Instance { item, args }, &[arepr, irepr], &[apass, ipass], None, rrepr, rpass)
    }

    /// Refuses place expression `place`, met in a mutable place expression context, when an
    /// overloaded index is on its path (through parentheses, fields, indexes and dereferences)
    /// that the check did not list as `IndexMut::index_mut` and that no raw pointer's
    /// dereference is above (`*v[0] = x` with `v[0]: *mut T` only reads `v[0]`, so
    /// `Index::index` is right there): a refusal by name, never a write through a shared
    /// reference.
    pub(super) fn refuse_mut_through_overloaded_index(&self, place: ExprId) -> Result<(), BuildError> {
        let mut e = place;
        loop {
            match self.expr(e)? {
                Expr::Paren(x) | Expr::Field { base: x, .. } | Expr::TupleField { base: x, .. } => e = x,
                Expr::Unary { op: UnOp::Deref, operand } => {
                    let t = self.adjusted_ty(operand)?;
                    if matches!(self.types.node(t).map_err(|_| malformed("a type past the record's table"))?, PNode::Ptr(..)) {
                        return Ok(());
                    }
                    e = operand;
                }
                Expr::Index { base, .. } => {
                    if !self.index_builtin(e)? && !self.checked.index_is_mut(e) {
                        return Err(unsupported(e, MUT_INDEX));
                    }
                    e = base;
                }
                _ => return Ok(()),
            }
        }
    }

    /// Pushes the tasks of raw borrow `e` (`&raw const operand`, `&raw mut operand`): the
    /// operand's place, then its address (`Task::Ref`: a thin pointer, or for an unsized place
    /// the wide pointer that reached it), with no read and no reference made (the module doc).
    pub(super) fn push_raw_ref(&mut self, e: ExprId, mutbl: Mutbl, operand: ExprId) -> Result<(), BuildError> {
        if !self.is_place_expr(operand)? {
            return Err(unsupported(e, "a raw borrow of an expression that is not a place"));
        }
        if mutbl == Mutbl::Mut {
            self.refuse_mut_through_overloaded_index(operand)?;
        }
        self.work.push(Task::Ref);
        self.work.push(Task::Place(operand));
        Ok(())
    }
}
