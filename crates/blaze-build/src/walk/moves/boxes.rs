//! Moves out of a `Box` and a box's two halves: the one mechanism of walk/moves.rs (a part is a
//! path of steps from a local, with a bit in the local's word) given the step it lacked, the
//! contents of a box ([`Step::Deref`]), and the box's drop glue split in two.
//!
//! The rules, from the Reference:
//! - [expr.move.movable-place]: "Only the following place expressions may be moved out of:
//!   Variables which are not currently borrowed. Temporary values. Fields of a place expression
//!   which can be moved out of and don't implement `Drop`. The result of dereferencing an
//!   expression with type `Box<T>` and that can also be moved out of."
//! - [expr.move.deinitialization]: "After moving out of a place expression that evaluates to a
//!   local variable, the location is deinitialized and cannot be read from again until it is
//!   reinitialized"; "Destructors": "If a variable has been partially initialized, only its
//!   initialized fields are dropped".
//! - A `Box`'s destructor, the pinned library (library/alloc/src/boxed.rs, 2126..):
//!   `unsafe impl<#[may_dangle] T: ?Sized, A: Allocator> Drop for Box<T, A> { fn drop(&mut
//!   self) { // the T in the Box is dropped by the compiler before the destructor is run` /
//!   `let ptr = self.0;` / `let layout = unsafe { Layout::for_value_raw(ptr.as_ptr()) };` /
//!   `if layout.size() != 0 { ... self.1.deallocate(From::from(ptr.cast()), layout); }`. The
//!   pinned library has no `box_free` lang item: the box's glue is (1) the contents' drop, then
//!   (2) `<Box<T, A> as Drop>::drop`, which only deallocates, then the box's own fields (the
//!   allocator `A`). Half (2) alone is the drop of a box whose contents moved out
//!   ([`Walk::drop_box_free`], which walk.rs `glue` calls after
//!   `drop_box_contents` for a whole box: one home for the split).
//! - `*b` on a `Box` is the builtin dereference: alloc's `impl Deref for Box` is
//!   `fn deref(&self) -> &T { &**self }` (boxed.rs), meaningful only because `**self` is
//!   builtin there. The check records it as an overloaded dereference (the `deref` lang item);
//!   the build never calls `Deref::deref` for a `Box` (it would call itself): it loads the
//!   box's data pointer ([`Walk::box_deref_builtin`]).
//!
//! Built: `let x = *b;` (and an autoderef by value: `b.consume()` with `consume(self)` on the
//! contents) moves the part `[Deref]` of `b`; `let y = (*b).a;` (or `b.a`) the part
//! `[Deref, Field a]`; `let z = *s.bx;` the part `[Field bx, Deref]` of `s`. The local gets a
//! word in `scan_part_moves` (which replaces walk/moves.rs `scan_field_moves`: one scan for
//! every place path), the move clears the part's bit (`move_box_contents`, or `move_field`
//! through `field_path`, which now follows box dereferences), and every drop of the local (its
//! scope's end, an early exit, an assignment over it, the cleanup of a call that unwinds:
//! walk/unwind.rs drops by the same `drop_from(0)`) walks its type by `drop_tree`: at a box
//! with a moved part under it, the contents are visited at the data pointer (each moved part
//! tested by its bit, every other part dropped whole), then the allocation is freed alone.
//! An assignment over the box whole (`b = mk();`) drops the old box by the same walk and sets
//! every bit again (walk.rs `hold_all`); an assignment over the contents (`*b = v;`) or a
//! field of them drops the old value by its bits and sets them again (`assign_over`).
//!
//! Static where known, a run-time test only where branches differ: the word is one stack slot
//! loaded, masked and stored with constants; blaze-lir's simplify stage replaces such a slot
//! by values (simplify/sroa.rs) and folds the tests (simplify/prop.rs), so a straight-line
//! move leaves no test in the tiered code, and a move under one branch leaves one block
//! parameter, the run-time flag.
//!
//! A TEMPORARY box (`let x = *make();`) moves its contents by its `hold_temp` entry instead,
//! made a `Held::Moved` with the part `[Deref]` (`move_temp_box_contents`): the box is found by
//! the pointer `box_deref_builtin` recorded, since the contents' place is the data pointer.
//!
//! Refused by name: a move out of the contents of a box that is a place no local's field flags
//! cover (a box behind a reference: E0507); `**make()` (a temporary reached by two box
//! dereferences); a dereference other than a
//! box's on the way (E0507); a moved part under a box other than through its contents; the
//! contents of an unsized box (E0161, the checker's); more than 64 parts of one local.
//!
//! A match by tests moving out of `*b` or `*s.bx` (`if let Type::Path(mut ty) = *group.elem`,
//! syn's `ambig_ty`) is built by the same paths: walk/moves/field_scrutinee.rs
//! `field_chain_local` finds the owning local by `path_steps`, the binding's part is the
//! scrutinee's path then the pattern's (`[Field elem, Deref, Variant Path, Field 0]`).

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::SlotKind;
use blaze_lir::{Flags, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{AdjustKind, Expr, ExprId, LangItem, Mutbl, PNode, PTy, PatId, UnOp};

use super::super::{Held, NO_EXPR, Repr, Shape, TyErr, V, Walk, malformed, unsupported};
use super::{Do, Node, Step};
use crate::BuildError;

/// How deep the first-field chain from a `Box` to its data pointer may go (`Box` → `Unique` →
/// `NonNull` → the pointer in the pinned library: three; a twin's box: one).
const BOX_NESTING: u32 = 16;

/// A grounding error as a body's build reports it (as walk.rs `glue`'s `sig_err`).
pub(in crate::walk) fn from_ty(error: TyErr) -> BuildError {
    match error {
        TyErr::Unsupported(what) => BuildError::UnsupportedSig { what },
        TyErr::Malformed(what) => malformed(what),
        TyErr::Other(error) => error,
    }
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The contents type `T` when ground type `g` is a `Box<T, ..>` (the `owned_box` lang
    /// item), else `None`.
    pub(in crate::walk) fn box_arg(&self, g: PTy) -> Result<Option<PTy>, TyErr> {
        let PNode::Adt(k, list) = self.ground_node(g)? else { return Ok(None) };
        if self.adts.lang_adt(LangItem::OwnedBox) != Some(k) {
            return Ok(None);
        }
        let args = self.ground.view().list(list).map_err(|_| TyErr::Malformed("a `Box`'s arguments past the ground table"))?;
        let t = *args.first().ok_or(TyErr::Malformed("a `Box` without its type argument"))?;
        Ok(Some(t))
    }

    /// The offset of the data pointer in a `Box` of ground type `g`: down its first fields
    /// (`Box(Unique(NonNull(*const T)), A)` in the pinned library) to the pointer, thin
    /// (`Repr::Ptr`) or wide (an aggregate whose type is a pointer: its first word is the data
    /// address, its second the metadata). Read from the layout, so an allocator field placed
    /// first is passed over.
    pub(in crate::walk) fn box_data_word(&mut self, g: PTy) -> Result<i32, TyErr> {
        const NOT_FIRST: TyErr = TyErr::Malformed("a `Box` whose data pointer is not reached through its first fields");
        let mut l = self.layout(g)?;
        let mut off = 0i32;
        for _ in 0..BOX_NESTING {
            let Shape::Fields { first, len } = self.layouts[l as usize].shape else { return Err(NOT_FIRST) };
            if len == 0 || self.layouts[l as usize].is_union {
                return Err(NOT_FIRST);
            }
            let (_, o, repr) = self.layout_fields[first as usize];
            off += o as i32;
            match repr {
                Repr::Ptr => return Ok(off),
                Repr::Agg(fl) => {
                    let ty = self.layouts[fl as usize].ty;
                    if matches!(self.ground_node(ty)?, PNode::Ptr(..) | PNode::Ref(..)) {
                        return Ok(off);
                    }
                    l = fl;
                }
                _ => return Err(NOT_FIRST),
            }
        }
        Err(TyErr::Malformed("a `Box` whose data pointer is nested deeper than 16 first fields"))
    }

    /// Whether the first `k` adjustments of `e` are each a dereference of a `Box` (the type
    /// before it a box), overloaded as the check records it or builtin.
    pub(in crate::walk) fn box_rows(&mut self, e: ExprId, k: usize) -> Result<bool, BuildError> {
        let rows = self.adjust_rows(e);
        if k > rows.len() {
            return Ok(false);
        }
        for i in 0..k {
            if !matches!(self.checked.adjusts.column::<1>()[rows.start + i], AdjustKind::Deref { .. }) {
                return Ok(false);
            }
            let t = self.ty_before(e, i)?;
            let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
            if self.box_arg(g).map_err(|error| Self::ty_err(e, error))?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// All of `e`'s adjustments when they are box dereferences only (their count, 0 for none);
    /// `None` when any other adjustment is among them (a borrow, a reference's dereference, a
    /// coercion): `e` is then no step of a place path.
    fn box_derefs(&mut self, e: ExprId) -> Result<Option<u32>, BuildError> {
        let n = self.adjust_rows(e).len();
        Ok(self.box_rows(e, n)?.then_some(n as u32))
    }

    /// The local at the root of place expression `e` after `derefs` box dereferences of its
    /// value, and the path to the place (appended to `steps`, root first): struct and tuple
    /// fields, box contents (an overloaded `*x` of a box, or a base's autoderef through boxes),
    /// through parentheses; and whether a container on the path implements `Drop` (E0509 for a
    /// move out; a box's contents are not such a field). `None` (`steps` as they were) for
    /// any other place. `e`'s own adjustments are the caller's (`derefs`).
    pub(super) fn path_steps(&mut self, e: ExprId, derefs: u32) -> Result<Option<(PatId, bool)>, BuildError> {
        let start = self.moves.steps.len();
        let mut drops = false;
        for _ in 0..derefs {
            self.moves.steps.push(Step::Deref);
        }
        let mut cur = e;
        let mut is_base = false;
        let x = loop {
            if is_base {
                // A base's own adjustments come after its value: nearer the leaf.
                let Some(k) = self.box_derefs(cur)? else { break None };
                for _ in 0..k {
                    self.moves.steps.push(Step::Deref);
                }
            }
            is_base = true;
            match self.expr(cur)? {
                Expr::Paren(inner) => cur = inner,
                Expr::Local(x) => break Some(x),
                Expr::Field { base, .. } | Expr::TupleField { base, .. } => {
                    let col = self.checked.fields.column::<0>();
                    let row = col.binary_search(&cur).map_err(|_| malformed("a field access without its record row"))?;
                    let k = self.checked.fields.column::<2>()[row];
                    // A base reached through anything but box dereferences ends the path
                    // before its adjusted value is asked a repr (it may be unsized: `p.body`
                    // of a `&Packet` with a slice tail).
                    if self.box_derefs(base)?.is_none() {
                        break None;
                    }
                    let Repr::Agg(l) = self.adjusted_repr(base)? else { break None };
                    if self.layouts[l as usize].is_union {
                        break None;
                    }
                    let Shape::Fields { first, len } = self.layouts[l as usize].shape else { break None };
                    if k >= len {
                        return Err(malformed("a field index past its aggregate's fields"));
                    }
                    drops |= self.layouts[l as usize].drop_fn.is_some();
                    self.moves.steps.push(Step::Field(first + k));
                    cur = base;
                }
                // `*b` overloaded: the dereference is `b`'s own last adjustment, pushed as a
                // step when `b` is reached as a base (a box's only; any other ends the path).
                Expr::Unary { op: UnOp::Deref, operand } if self.deref_on_operand(operand) => cur = operand,
                _ => break None,
            }
        };
        let Some(x) = x else {
            self.moves.steps.truncate(start);
            return Ok(None);
        };
        self.moves.steps[start..].reverse();
        Ok(Some((x, drops)))
    }

    /// `path_steps` for a local with field flags (`Owned::rest`): the local, its word, the
    /// path's start and length in `steps`, and whether a container on it implements `Drop`.
    /// walk/moves.rs `field_path` is this with no dereference of `e`'s own.
    pub(in crate::walk) fn place_path(&mut self, e: ExprId, derefs: u32) -> Result<Option<(PatId, blaze_lir::func::SlotId, u32, u32, bool)>, BuildError> {
        let start = self.moves.steps.len() as u32;
        let Some((x, drops)) = self.path_steps(e, derefs)? else { return Ok(None) };
        let Some(mask) = self.field_mask(x) else {
            self.moves.steps.truncate(start as usize);
            return Ok(None);
        };
        let len = self.moves.steps.len() as u32 - start;
        Ok(Some((x, mask, start, len, drops)))
    }

    /// `let x = *b;`: the value of `e` after its first `n + 1` adjustments, all dereferences,
    /// at `at` (held as `repr`, which needs drop), moved out of a `Box`'s contents. The part's
    /// bit in its local's word is cleared, so the local's drops free the allocation and drop
    /// only what is left (`drop_tree`, `box_node`); the value is copied out first, as
    /// `move_field` does (a later assignment over the place in the same expression must not
    /// change it). Refused by name: a dereference other than a box's on the way, a box that is
    /// not a local's place path, a container that implements `Drop` on the path (E0509), more
    /// than 64 parts.
    pub(in crate::walk) fn move_box_contents(&mut self, e: ExprId, n: u32, at: (Value, i32), repr: Repr) -> Result<V, BuildError> {
        let k = n + 1;
        if !self.box_rows(e, k as usize)? {
            return Err(unsupported(e, "a move out of a dereference whose type needs drop, other than of a `Box` (behind a reference or a pointer: E0507)"));
        }
        let Some((_, mask, start, len, drops)) = self.place_path(e, k)? else {
            return self.move_temp_box_contents(e, n, at, repr);
        };
        if drops {
            self.moves.steps.truncate(start as usize);
            return Err(unsupported(e, "a move out of a field of a type that implements `Drop` (E0509)"));
        }
        let Some(bit) = self.part_bit(mask, start, len, 0, Some(e))? else {
            return Err(unsupported(e, "a move out of more than 64 parts of one local (its word)"));
        };
        self.clear_bit(mask, bit);
        let tmp = self.temp(repr);
        self.store(V::Place(at.0, at.1), repr, tmp)?;
        Ok(V::Place(tmp.0, tmp.1))
    }

    /// `let x = *make();`, `let s = *Box::from_raw(p);`: `move_box_contents` when the box is a
    /// TEMPORARY (`e`, a value expression whose one adjustment `n == 0` is the box's
    /// dereference; the contents at `at`, held as `repr`). The Reference,
    /// [expr.move.movable-place]: "Temporary values" and "The result of dereferencing an
    /// expression with type `Box<T>` and that can also be moved out of"; "Temporary scopes":
    /// the box lives to the end of its temporary scope, where it is dropped minus what moved:
    /// its allocation is freed, its contents are not dropped again. Built as walk/moves.rs
    /// `move_temp_field` builds a temporary's field: the box's `hold_temp` entry (the whole
    /// `Held::Rest` that `Task::Spill` made) becomes a `Held::Moved` with a word of its own, all
    /// held, and the bit of the part `[Deref]` is cleared; the scope's end then drops it by
    /// `drop_moved`, where `box_node` tests the contents' bit (clear: not dropped) and frees
    /// the allocation alone (`Do::BoxFree`). The box is found by the pointer to it that
    /// `box_deref_builtin` recorded for `e` (`Moves::box_self`). Refused by name: a box that is
    /// a place no local's field flags cover (behind a reference or a pointer: E0507, the
    /// checker's), a box reached by more than one dereference (`**make()` of a
    /// `Box<Box<T>>`), and a box held by no entry of its scope (an extended temporary).
    fn move_temp_box_contents(&mut self, e: ExprId, n: u32, at: (Value, i32), repr: Repr) -> Result<V, BuildError> {
        if self.is_place_expr(e)? {
            return Err(unsupported(
                e,
                "a move out of a `Box`'s contents whose box is a place no local's field flags cover (behind a reference or a pointer: E0507)",
            ));
        }
        if n != 0 {
            return Err(unsupported(e, "a move out of the contents of a temporary `Box` reached by more than one dereference (`**make()`)"));
        }
        if !self.live {
            // Unreachable code: nothing runs, nothing is held (`Task::Spill` held nothing).
            let tmp = self.temp(repr);
            self.store(V::Place(at.0, at.1), repr, tmp)?;
            return Ok(V::Place(tmp.0, tmp.1));
        }
        let p = match self.moves.box_self {
            Some((b, p)) if b == e => p,
            _ => return Err(malformed("a move out of a temporary `Box`'s contents with no recorded dereference of the box")),
        };
        let found = self
            .scope_pats
            .iter()
            .rposition(|h| matches!(*h, Held::Rest { at: a, variant: None, .. } | Held::Moved { at: a, .. } if a == (p, 0)));
        let Some(i) = found else {
            return Err(unsupported(e, "a move out of the contents of a temporary `Box` not held to its scope's end (an extended temporary)"));
        };
        let mask = match self.scope_pats[i] {
            Held::Moved { mask, .. } => mask,
            _ => {
                let t = self.expr_ty(e)?;
                let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
                let slot = self.b.slot(8, 3, SlotKind::Named);
                let fa = self.b.slot_addr(slot);
                let all = self.b.iconst(Ty::I64, u128::from(u64::MAX));
                self.b.store(Flags::NONE, all, fa, 0, MemAttrs::aligned(3));
                self.scope_pats[i] = Held::Moved { at: (p, 0), g, mask: slot };
                slot
            }
        };
        let start = self.moves.steps.len() as u32;
        self.moves.steps.push(Step::Deref);
        let Some(bit) = self.part_bit(mask, start, 1, 0, Some(e))? else {
            return Err(unsupported(e, "a move out of more than 64 parts of one temporary (its word)"));
        };
        self.clear_bit(mask, bit);
        // Copied out first, as `move_field` does.
        let tmp = self.temp(repr);
        self.store(V::Place(at.0, at.1), repr, tmp)?;
        Ok(V::Place(tmp.0, tmp.1))
    }

    /// walk.rs `deref_call` for a `Box` `Self` (`g_self`, its target `g_target`, `m` the
    /// recorded mutability): the builtin dereference, never a call of `Deref::deref` (alloc's
    /// body, `&**self`, is itself this dereference). The pointer to the box is on top; the
    /// reference to the contents is left: the data pointer loaded from the box (thin), or the
    /// box's pointer words copied to a temporary of their own (wide: `DerefPlace` keeps that
    /// memory as the unsized place, as for a call's wide result).
    pub(in crate::walk) fn box_deref_builtin(&mut self, e: ExprId, m: Mutbl, g_self: PTy, g_target: PTy) -> Result<(), BuildError> {
        let g_ret = self.ground.intern(PNode::Ref(m, g_target)).map_err(|_| malformed("the ground table is full"))?;
        let rrepr = self.repr(g_ret).map_err(|error| Self::ty_err(e, error))?;
        let off = self.box_data_word(g_self).map_err(|error| Self::ty_err(e, error))?;
        let v = self.pop()?;
        let p = self.scalar(v, Ty::PTR)?;
        self.moves.box_self = Some((e, p));
        match rrepr {
            Repr::Ptr => {
                let data = self.b.load(Ty::PTR, Flags::NONE, p, off, MemAttrs::aligned(3));
                self.push_val(V::Scalar(data), None);
            }
            Repr::Agg(_) => {
                let tmp = self.temp(rrepr);
                self.store(V::Place(p, off), rrepr, tmp)?;
                self.push_val(V::Place(tmp.0, tmp.1), None);
            }
            _ => return Err(malformed("a reference to a `Box`'s contents held as neither a thin nor a wide pointer")),
        }
        Ok(())
    }

    /// The second half of the drop glue of the `Box` of ground type `g` at `p`: its `Drop`
    /// impl (the library's, which deallocates and never drops the contents), then its own
    /// fields (the allocator). walk.rs `glue` runs `drop_box_contents` then this; a box whose
    /// contents moved out runs this alone (`Do::BoxFree`).
    pub(in crate::walk) fn drop_box_free(&mut self, g: PTy, p: Value) -> Result<(), BuildError> {
        let l = self.layout(g).map_err(from_ty)? as usize;
        if self.layouts[l].is_union || !matches!(self.layouts[l].shape, Shape::Fields { .. }) {
            return Err(malformed("a `Box` whose layout is not a struct's fields"));
        }
        // Its `Drop` (the free), whose landing drops its fields, then its fields
        // (walk/unwind_ladder.rs).
        self.drop_impl_then_parts(g, l, p)
    }

    /// `drop_node` at a `Box` node `n` (contents type `t`, at `place`) with a moved part under
    /// it: the contents are visited at the data pointer (the one step under a box is
    /// [`Step::Deref`]), then the allocation is freed alone (`Do::BoxFree`, pushed first: the
    /// work list runs last pushed first).
    pub(super) fn box_node(&mut self, n: Node, t: PTy, place: (Value, i32), ps: &[u32], todo: &mut Vec<Do, A>) -> Result<(), BuildError> {
        let rep = n.rep.ok_or(malformed("a `Box` descended into with no moved part under it"))?;
        if ps.iter().any(|&q| self.under(q, rep, n.depth, None) && !self.under(q, rep, n.depth, Some(Step::Deref))) {
            return Err(malformed("a moved part under a `Box` other than through its contents"));
        }
        if matches!(self.ground_node(t).map_err(from_ty)?, PNode::Dynamic(_) | PNode::Slice(_) | PNode::Str) {
            return Err(malformed("a moved part of an unsized `Box`'s contents (E0161)"));
        }
        // The controls of tests/box_moves.rs: the path state ignored, the box dropped whole by
        // its glue (a double drop of what moved out), or its allocation never freed (a leak).
        #[cfg(test)]
        let planted = self.adts.planted();
        #[cfg(not(test))]
        let planted = ();
        #[cfg(test)]
        if planted == crate::Planted::MovedBoxDroppedWhole {
            return self.drop_at(n.g, place);
        }
        let off = self.box_data_word(n.g).map_err(from_ty)?;
        let data = self.b.load(Ty::PTR, Flags::NONE, place.0, place.1 + off, MemAttrs::aligned(3));
        #[cfg(test)]
        let free = planted != crate::Planted::MovedBoxNotFreed;
        #[cfg(not(test))]
        let free = {
            let () = planted;
            true
        };
        if free {
            todo.push(Do::BoxFree { g: n.g, place });
        }
        let rep = ps.iter().copied().find(|&q| self.under(q, rep, n.depth, Some(Step::Deref)));
        todo.push(Do::Visit(Node { g: t, off: 0, base: Some(data), depth: n.depth + 1, rep, field_bit: None }));
        Ok(())
    }

    /// What expression `e` (`i` its index) moves when it is used by value, if a part of a
    /// local: the expression the place path starts from, the box dereferences of its own value
    /// on the path, and the moved value's type. A field (`t.f`), an overloaded `*b`, or an
    /// expression whose adjustments start with box dereferences not followed by a borrow (an
    /// autoderef by value). `None` for anything else, and for the operand of an overloaded `*`
    /// (its `*` is the expression that moves).
    fn part_move_of(&mut self, e: ExprId, c: u32, parent: &[(u32, bool)]) -> Result<Option<(ExprId, u32, PTy)>, BuildError> {
        let up = parent.get(c as usize).map_or(NO_EXPR, |&(up, _)| up);
        if up != NO_EXPR
            && let Expr::Unary { op: UnOp::Deref, operand } = self.expr(ExprId(up))?
            && operand.0 == c
            && self.deref_on_operand(operand)
        {
            return Ok(None);
        }
        let rows = self.adjust_rows(e);
        let kinds = self.checked.adjusts.column::<1>().get(rows.start..rows.end).ok_or(malformed("adjustment rows past their column"))?;
        let lead = kinds.iter().take_while(|k| matches!(k, AdjustKind::Deref { .. })).count();
        let borrow_after = matches!(kinds.get(lead), Some(AdjustKind::Borrow { .. }));
        if lead > 0 {
            if borrow_after || !self.box_rows(e, lead)? {
                return Ok(None);
            }
            let t = self.checked.adjusts.column::<2>()[rows.start + lead - 1];
            return Ok(Some((e, lead as u32, t)));
        }
        // No dereference first: `borrow_after` is a borrow as the first adjustment.
        if borrow_after {
            return Ok(None);
        }
        let place = match self.expr(e)? {
            Expr::Field { .. } | Expr::TupleField { .. } => true,
            Expr::Unary { op: UnOp::Deref, operand } => self.deref_on_operand(operand),
            _ => false,
        };
        if !place {
            return Ok(None);
        }
        Ok(Some((e, 0, self.expr_ty(e)?)))
    }

    /// Whether the use whose outermost parenthesis is `c` is in place (borrowed, assigned to,
    /// the base of a further field or index, compared, matched, a `let` condition's
    /// scrutinee): it moves nothing.
    fn used_in_place(&self, c: u32, parent: &[(u32, bool)]) -> Result<bool, BuildError> {
        let up = parent.get(c as usize).map_or(NO_EXPR, |&(up, _)| up);
        if up == NO_EXPR {
            return Ok(false);
        }
        Ok(match self.expr(ExprId(up))? {
            Expr::Ref { operand, .. } | Expr::RawRef { operand, .. } => operand.0 == c,
            Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Index { base, .. } => base.0 == c,
            Expr::Assign { lhs, .. } | Expr::AssignOp { lhs, .. } => lhs.0 == c,
            // [expr.cmp.place]: a comparison's operands are borrowed in place.
            Expr::Binary { op, lhs, rhs } if super::super::operators::comparison(op) => lhs.0 == c || rhs.0 == c,
            Expr::Match { scrutinee, .. } => scrutinee.0 == c,
            Expr::Let { init, .. } => init.0 == c,
            // A struct update's base is a place: its taken fields move by
            // walk/moves/struct_update.rs, not the whole.
            Expr::Struct { base: blaze_typeck::StructBase::Expr(b), .. }
            | Expr::Record { base: blaze_typeck::StructBase::Expr(b), .. } => b.0 == c,
            _ => false,
        })
    }

    /// `scan_flags`, with `parent` per expression: a local some part of which is moved out by
    /// a place expression in a value context (`let a = t.f;`, `let x = *b;`, `let y =
    /// (*b).a;`, `b.consume()` through a box), of a type that needs drop, gets field flags
    /// (`partial`): the word its moves clear and its drops read. Replaces walk/moves.rs
    /// `scan_field_moves`, which saw fields only: one scan for every place path.
    pub(in crate::walk) fn scan_part_moves(&mut self, parent: &[(u32, bool)]) -> Result<(), BuildError> {
        for i in 0..self.body.exprs.len() {
            let e = ExprId(i as u32);
            if let Expr::Paren(_) = self.expr(e)? {
                continue;
            }
            // The context: the parent of the outermost parenthesis around it.
            let mut c = i as u32;
            while let Some(&(up, _)) = parent.get(c as usize)
                && up != NO_EXPR
                && let Expr::Paren(_) = self.expr(ExprId(up))?
            {
                c = up;
            }
            let Some((start, derefs, moved)) = self.part_move_of(e, c, parent)? else { continue };
            if self.used_in_place(c, parent)? {
                continue;
            }
            let mark = self.moves.steps.len();
            let root = self.path_steps(start, derefs)?;
            self.moves.steps.truncate(mark);
            let Some((x, _)) = root else { continue };
            let g = self.ground_ty(moved).map_err(|error| Self::ty_err(e, error))?;
            if self.needs_drop(g).map_err(|error| Self::ty_err(e, error))? {
                *self.partial.get_mut(x.ix()).ok_or(malformed("a local past `pats`"))? = true;
            }
        }
        Ok(())
    }
}
