//! A closure's environment field that holds a captured PLACE below its binding (`x.a`,
//! `(*r).b`: RFC 2229, blaze-typeck env_fields.rs [`EnvField`] with projections), built on both
//! sides: the owner stores the place (its address, or its value moved or copied in), and the
//! closure's body reads every use of the place, and of a place under it, at the field.
//!
//! The rules, from the Reference:
//! - "Closure types", "Capture modes": "Closures can capture variables by immutable borrow,
//!   unique immutable borrow, mutable borrow, or move"; since 2021 "a closure captures the
//!   precise path used ... rather than the entire variable" ("Capture precision"), and a
//!   by-value capture of a field moves that field alone: the variable is partially moved
//!   ("Destructors": "If a variable has been partially initialized, only its initialized
//!   fields are dropped"), and the moved field is dropped with the closure ("Drop order": a
//!   closure's captures are dropped when the closure is, in capture order: the environment's
//!   field order, which the layout's drop glue follows).
//! - [expr.move.movable-place]: "Fields of a place expression which can be moved out of and
//!   don't implement `Drop`": a by-value capture of a field of a type with `Drop` is E0509,
//!   refused by name here as walk/moves.rs `move_field` refuses it.
//! - "Destructors": a by-value capture is the closure's variable; an `FnOnce` body (and an
//!   `async` block's body, which runs once) owns it as a function owns a by-value parameter:
//!   dropped at the body's end, on an early exit and on unwinding unless moved out, and
//!   dropped first when assigned over ([expr.assign.drop-target]).
//!
//! Built: the owner side ([`Walk::store_env_place`]) walks the place's projections from the
//! binding's home (a field's offset by its declaration index in the layout, a thin reference's
//! or raw pointer's target by loading it) and stores the address (by reference) or the bytes
//! (by value); a by-value part whose type needs drop clears its bit in the binding's word
//! (walk/moves.rs `part_bit`, `clear_bit`), which [`Walk::scan_env_moves`] gave the binding in
//! `scan_flags`, so the binding's drops skip it. Inside a closure or `async` block whose own
//! environment holds the place or one above it, the owner side starts from that field instead
//! of a home (the analysis files the outer owner's places as a superset of the inner's: the
//! parent replays each nested place, capture_places.rs `replay_child`); a by-value capture of
//! exactly an owned field moves it out ([`Walk::env_moved`]).
//!
//! The body side ([`Walk::bind_env_place`]) records each projected field's place (the field
//! itself by value, the loaded pointer's target by reference) in [`EnvPlaces`]; [`Walk::env_use`],
//! asked by walk.rs `raw` and `place_derefs`, finds which field holds a used place
//! (env_fields.rs `use_path`, `cover`): exactly that place is the field's; a place under it is
//! walked from it as any field is; the value of a reference whose target is held by shared
//! reference (`Cover::RefOf`) is the field's pointer. A by-value field whose type needs drop,
//! in an `FnOnce` body or an `async` block, is OWNED by the body: a drop-scope entry
//! (walk.rs `Held::EnvPlace`) in the outermost scope, its state static (the walk's own, as a
//! binding's `init`) or, when a move or assignment of it stands under a branch or a loop, a
//! drop flag set at the body's entry ([`Walk::settle_env_places`], the rule of walk.rs
//! `scan_flags` and `used_in_place`). A move out of it ([`Walk::env_moved`]) clears the state;
//! an assignment over it ([`Walk::assign_env_place`]) drops what it holds first and sets it.
//!
//! Every case not built is refused by name, "not lowered: ...". The place analysis
//! (blaze-typeck capture_places.rs `widen`) captures the WHOLE binding instead wherever this
//! build would refuse a precise place: a place through a `Box`'s contents or a wide pointer,
//! below an enum variant's field, a move below a captured place, a pattern that moves a
//! captured place or matches above one. What stays refused: a zero-sized environment holding a
//! place (a place field always has a size), a by-value capture through a dereference that
//! needs drop (the analysis truncates every by-value place at its first dereference), and a
//! move out of a field the body does not own (E0507, the checker's).

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::{SlotId, SlotKind};
use blaze_lir::{Flags, IntCond, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::env_fields::{Cover, PathError, cover, use_path};
use blaze_typeck::{CaptureKind, CaptureProj, EnvField, Expr, ExprId, PNode, PTy};

use super::super::{Held, NO_EXPR, Repr, Shape, V, Walk, malformed, unsupported};
use super::Step;
use crate::{BuildError, FnTrait};

/// A by-value field an `FnOnce` body or an `async` block owns, whose type needs drop.
#[derive(Clone, Copy)]
pub(in crate::walk) struct EnvOwned {
    /// The field's ground type.
    g: PTy,
    /// Its drop flag, when a move or an assignment of it stands under a branch or a loop.
    flag: Option<SlotId>,
    /// Without a flag: whether it holds its value here (the walk's static state).
    init: bool,
}

/// Where a captured place lives: a sized place at `(base, off)`, or an unsized place behind the
/// 16-byte wide pointer `{ addr, meta }` stored at `(base, off)`.
#[derive(Clone, Copy)]
pub(in crate::walk) enum PlaceAt {
    Sized(Value, i32),
    Unsized(Value, i32),
}

/// The fields of the environment of the closure whose body is being built, when one of them
/// holds a place below its binding (empty otherwise: every use then takes the plain path).
pub(in crate::walk) struct EnvPlaces<A: Allocator> {
    /// Every field, in field order (`cover` reads them all).
    env: Vec<EnvField, A>,
    /// Per field: where the held place is (`None`: a whole binding, bound to its own home).
    at: Vec<Option<PlaceAt>, A>,
    /// Per field: its state when the body owns it (`None`: borrowed, a whole binding, or a
    /// type that needs no drop).
    owned: Vec<Option<EnvOwned>, A>,
}

impl<A: Allocator + Copy> EnvPlaces<A> {
    pub(in crate::walk) fn new_in(alloc: A) -> Self {
        EnvPlaces { env: Vec::new_in(alloc), at: Vec::new_in(alloc), owned: Vec::new_in(alloc) }
    }
}

/// [`use_path`]'s refusal as the build's.
fn path_error(e: ExprId, error: PathError) -> BuildError {
    match error {
        PathError::TooDeep => unsupported(e, "not lowered: a use of a captured place deeper than 32 projections (env_fields.rs `use_path`)"),
        PathError::NoFieldRow(_) | PathError::Malformed(_) => malformed("a captured use whose place the record does not name (env_fields.rs `use_path`)"),
    }
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// The offset of field `field` of variant `variant` of a value of ground type `g`, its row,
    /// and whether `g` implements `Drop`: walk.rs `decl_field`, the one lookup by declaration
    /// index (closure.rs `field_offset` is it too). Refused: a union's field and an enum
    /// variant's (the analysis widens those, capture_places.rs `widen`).
    fn env_field_offset(&mut self, e: ExprId, g: PTy, variant: u32, field: u32) -> Result<(i32, u32, bool), BuildError> {
        let l = self.layout(g).map_err(|error| Self::ty_err(e, error))?;
        if self.layouts[l as usize].is_union {
            return Err(unsupported(e, "not lowered: a captured place below a union's field"));
        }
        let (off, row, id) = self.decl_field(l, variant, field)?;
        if id.is_some() {
            return Err(unsupported(e, "not lowered: a captured place below an enum variant's field"));
        }
        let drops = self.layouts[l as usize].drop_fn.is_some();
        Ok((off as i32, row, drops))
    }

    /// The field of the environment of the body being built that holds captured place `f` or
    /// a place above it: its index and how many of `f`'s projections it takes.
    fn outer_field(&self, f: EnvField) -> Option<(usize, u32)> {
        let ck = self.checked;
        self.env_places.env.iter().enumerate().find_map(|(j, o)| {
            let prefix = o.base == f.base && o.len <= f.len && (0..o.len).all(|i| o.proj(ck, i).map(|p| p.0) == f.proj(ck, i).map(|p| p.0));
            prefix.then_some((j, o.len))
        })
    }

    /// Where captured place `f` of closure `e`'s environment is, in the OWNER's body: from its
    /// binding's home, or, inside a closure body holding the place or one above it, from that
    /// field; each field at its offset, each dereference of a thin or wide reference or raw
    /// pointer through the pointer loaded. With it: the owner's field it started from and whether
    /// that field holds exactly `f`.
    fn owner_place(&mut self, e: ExprId, f: EnvField) -> Result<(PlaceAt, Option<(usize, bool)>), BuildError> {
        let (mut at, from, outer) = match self.slots.get(f.base.ix()).copied().flatten() {
            Some((home, _)) => {
                let (base, off) = self.home(home);
                (PlaceAt::Sized(base, off), 0, None)
            }
            None => {
                let Some((j, taken)) = self.outer_field(f) else {
                    return Err(malformed("a binding captured before its binding (no home, no field of the environment above the place)"));
                };
                let at = self.env_places.at.get(j).copied().flatten().ok_or(malformed("a whole-binding field of an environment without its binding's home"))?;
                (at, taken, Some((j, taken == f.len)))
            }
        };
        for i in from..f.len {
            let (proj, _) = f.proj(self.checked, i).ok_or(malformed("a captured place's projection past `capture_projs`"))?;
            let before = f.ty_before(self.checked, i).ok_or(malformed("a captured place's type past the record's types"))?;
            let g = self.ground_ty(before).map_err(|error| Self::ty_err(e, error))?;
            match proj {
                CaptureProj::Field { variant, field } => match at {
                    PlaceAt::Sized(base, off) => {
                        let (foff, _, _) = self.env_field_offset(e, g, variant, field)?;
                        at = PlaceAt::Sized(base, off + foff);
                    }
                    PlaceAt::Unsized(w, woff) => {
                        let l = self.layout(g).map_err(|error| Self::ty_err(e, error))?;
                        let li = usize::try_from(l).map_err(|_| malformed("a layout index past usize"))?;
                        let layout = self.layouts.get(li).ok_or(malformed("a layout index past `layouts`"))?;
                        if layout.is_union {
                            return Err(unsupported(e, "not lowered: a captured place below a union's field"));
                        }
                        if variant != 0 || !self.has_unsized_tail(l) {
                            return Err(unsupported(e, "not lowered: a captured place below an unsized aggregate without an unsized tail"));
                        }
                        let Shape::Fields { first, len } = layout.shape else {
                            return Err(malformed("an unsized aggregate that is not a struct or a tuple"));
                        };
                        if field >= len {
                            return Err(malformed("a field index past its aggregate's fields"));
                        }
                        let ls_id = self.ls_of(g).map_err(|error| Self::ty_err(e, error))?;
                        let dyn_prefix = self.ls.dyn_chain(ls_id);
                        let fi = first.checked_add(field).and_then(|i| usize::try_from(i).ok()).ok_or(malformed("a field index past usize"))?;
                        let &(_, foff, _) = self.layout_fields.get(fi).ok_or(malformed("a field index past `layout_fields`"))?;
                        let foff = i32::try_from(foff).map_err(|_| unsupported(e, "a field at 2 GiB or more"))?;
                        let addr = self.b.load(Ty::PTR, Flags::NONE, w, woff, MemAttrs::aligned(3));
                        if field + 1 == len {
                            let meta = self.b.load(Ty::I64, Flags::NONE, w, woff + 8, MemAttrs::aligned(3));
                            let tail = match dyn_prefix {
                                None => self.address((addr, foff)),
                                Some(prefix) => {
                                    let vtable = self.b.load(Ty::PTR, Flags::NONE, w, woff + 8, MemAttrs::aligned(3));
                                    let off = crate::dyn_tail::tail_offset(&mut self.b, prefix, vtable);
                                    self.b.ptr_add(Flags::INBOUNDS, addr, off)
                                }
                            };
                            let slot = self.b.slot(16, 3, SlotKind::Temp);
                            let to = self.b.slot_addr(slot);
                            self.b.store(Flags::NONE, tail, to, 0, MemAttrs::aligned(3));
                            self.b.store(Flags::NONE, meta, to, 8, MemAttrs::aligned(3));
                            at = PlaceAt::Unsized(to, 0);
                        } else {
                            at = PlaceAt::Sized(addr, foff);
                        }
                    }
                },
                CaptureProj::Deref => {
                    if !matches!(self.ground_node(g).map_err(|error| Self::ty_err(e, error))?, PNode::Ref(..) | PNode::Ptr(..)) {
                        return Err(unsupported(e, "not lowered: a captured place through a `Box`'s contents (`(*b).f`)"));
                    }
                    let PlaceAt::Sized(base, off) = at else {
                        return Err(malformed("a pointer dereferenced from an unsized place"));
                    };
                    if self.repr(g).map_err(|error| Self::ty_err(e, error))? == Repr::Ptr {
                        let V::Scalar(p) = self.load(Repr::Ptr, (base, off)) else {
                            return Err(malformed("a thin pointer loaded as other than a scalar"));
                        };
                        at = PlaceAt::Sized(p, 0);
                    } else {
                        at = PlaceAt::Unsized(base, off);
                    }
                }
                CaptureProj::Index => return Err(malformed("a captured place with an element projection (the place analysis keeps none)")),
            }
        }
        Ok((at, outer))
    }

    /// The by-value capture of place `f` (a part of its binding whose type needs drop) by
    /// closure `e` moves the part out: its bit in the binding's word is cleared, so the
    /// binding's drops skip it (walk/moves.rs `move_field`, with the path from the place's
    /// projections instead of an expression's). Refused: a dereference on the way, a container
    /// with `Drop` (E0509), more than 64 parts.
    fn move_env_part(&mut self, e: ExprId, f: EnvField) -> Result<(), BuildError> {
        let mask = self
            .field_mask(f.base)
            .ok_or(malformed("a binding a closure moves a part of without field flags (`scan_env_moves` did not mark it)"))?;
        let start = self.moves.steps.len() as u32;
        for i in 0..f.len {
            let found = f.proj(self.checked, i).zip(f.ty_before(self.checked, i));
            let Some((CaptureProj::Field { variant, field }, before)) = found.map(|((p, _), t)| (p, t)) else {
                self.moves.steps.truncate(start as usize);
                return Err(unsupported(e, "not lowered: a closure moving a captured place through a dereference"));
            };
            let g = match self.ground_ty(before) {
                Ok(g) => g,
                Err(error) => {
                    self.moves.steps.truncate(start as usize);
                    return Err(Self::ty_err(e, error));
                }
            };
            let (_, row, drops) = match self.env_field_offset(e, g, variant, field) {
                Ok(found) => found,
                Err(error) => {
                    self.moves.steps.truncate(start as usize);
                    return Err(error);
                }
            };
            if drops {
                self.moves.steps.truncate(start as usize);
                return Err(unsupported(e, "a move out of a field of a type that implements `Drop` (E0509)"));
            }
            self.moves.steps.push(Step::Field(row));
        }
        let len = self.moves.steps.len() as u32 - start;
        let Some(bit) = self.part_bit(mask, start, len, 0, Some(e))? else {
            return Err(unsupported(e, "a move out of more than 64 parts of one local (its word)"));
        };
        self.clear_bit(mask, bit);
        Ok(())
    }

    /// Closure `e`'s field `f` (a place below its binding) stored at `dst` of the closure value
    /// being built: the place's address (by reference), or its value (by value: copied, and
    /// moved out of its binding, or out of the owned field of this body's environment holding
    /// exactly it, when its type needs drop).
    pub(in crate::walk) fn store_env_place(&mut self, e: ExprId, f: EnvField, dst: (Value, i32)) -> Result<(), BuildError> {
        let (src, outer) = self.owner_place(e, f)?;
        match f.how {
            CaptureKind::ByRef | CaptureKind::ByMutRef => match src {
                PlaceAt::Sized(base, off) => {
                    let a = self.address((base, off));
                    self.store(V::Scalar(a), Repr::Ptr, dst)
                }
                PlaceAt::Unsized(w, woff) => {
                    let addr = self.b.load(Ty::PTR, Flags::NONE, w, woff, MemAttrs::aligned(3));
                    let meta = self.b.load(Ty::I64, Flags::NONE, w, woff + 8, MemAttrs::aligned(3));
                    self.b.store(Flags::NONE, addr, dst.0, dst.1, MemAttrs::aligned(3));
                    self.b.store(Flags::NONE, meta, dst.0, dst.1 + 8, MemAttrs::aligned(3));
                    Ok(())
                }
            },
            CaptureKind::ByValue => {
                let PlaceAt::Sized(base, off) = src else {
                    return Err(unsupported(e, "not lowered: a by-value capture of an unsized place"));
                };
                let t = f.place_ty(self.checked).ok_or(malformed("a captured place's type past the record's types"))?;
                let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
                let repr = self.repr(g).map_err(|error| Self::ty_err(e, error))?;
                if self.needs_drop(g).map_err(|error| Self::ty_err(e, error))? {
                    match outer {
                        None => self.move_env_part(e, f)?,
                        Some((j, true)) => self.env_moved(e, j)?,
                        // capture_places.rs `widen` (`MoveBelow`) captures the outer binding
                        // whole for this: a move below a captured place.
                        Some((_, false)) => {
                            return Err(unsupported(e, "not lowered: a closure moving a part of a place its enclosing closure's environment holds"));
                        }
                    }
                }
                let v = self.load(repr, (base, off));
                self.store(v, repr, dst)
            }
        }
    }

    /// `scan_flags`: a binding a closure or `async` block of the body moves a part of (a by-value field holding
    /// a place below the binding, whose type needs drop) gets field flags (`partial`), the word
    /// `move_env_part` clears and its drops read, as `scan_part_moves` gives one to a binding a
    /// place expression moves a part of.
    pub(in crate::walk) fn scan_env_moves(&mut self) -> Result<(), BuildError> {
        for i in 0..self.body.exprs.len() {
            let e = ExprId(u32::try_from(i).map_err(|_| malformed("an expression index past u32"))?);
            if !matches!(self.expr(e)?, Expr::Closure { .. } | Expr::AsyncBlock { .. }) {
                continue;
            }
            let checked = self.checked;
            for f in checked.env_fields_by::<crate::layout::BuildEnv>(e) {
                if f.is_whole() || f.how != CaptureKind::ByValue {
                    continue;
                }
                let t = f.place_ty(checked).ok_or(malformed("a captured place's type past the record's types"))?;
                let g = self.ground_ty(t).map_err(|error| Self::ty_err(e, error))?;
                if self.needs_drop(g).map_err(|error| Self::ty_err(e, error))? {
                    *self.partial.get_mut(f.base.ix()).ok_or(malformed("a captured binding past `pats`"))? = true;
                }
            }
        }
        Ok(())
    }

    /// The body of closure `closure` (of kind `kind`) holds field `f`, a place below its
    /// binding, at `at` of its environment (`None`: a zero-sized environment): the place is
    /// the field (by value) or the loaded pointer's target (by reference), recorded for
    /// `env_use`. A by-value field whose type needs drop in an `FnOnce` body (an `async`
    /// block's is one) is the body's: it joins the outermost drop scope (`Held::EnvPlace`),
    /// held. Refused: a zero-sized environment and a place through a `Box`'s contents.
    pub(in crate::walk) fn bind_env_place(&mut self, closure: ExprId, f: EnvField, at: Option<(Value, i32)>, kind: FnTrait) -> Result<(), BuildError> {
        let at = match at {
            Some(at) => at,
            None => {
                if f.how != CaptureKind::ByValue {
                    return Err(malformed("a by-reference capture in a zero-sized closure environment"));
                }
                self.temp(Repr::None)
            }
        };
        for i in 0..f.len {
            if f.derefs_box(self.checked, i).ok_or(malformed("a captured place's projection past `capture_projs`"))? {
                return Err(unsupported(closure, "not lowered: a captured place through a `Box`'s contents (`(*b).f`)"));
            }
        }
        let mut owned = None;
        let place = match f.how {
            CaptureKind::ByRef | CaptureKind::ByMutRef => {
                let stored_repr = match f.stored {
                    Some(t) => {
                        let g = self.ground_ty(t).map_err(|error| Self::ty_err(closure, error))?;
                        self.repr(g).map_err(|error| Self::ty_err(closure, error))?
                    }
                    None => Repr::Ptr,
                };
                if stored_repr == Repr::Ptr {
                    let V::Scalar(p) = self.load(Repr::Ptr, at) else {
                        return Err(malformed("a capture's pointer not a scalar"));
                    };
                    PlaceAt::Sized(p, 0)
                } else {
                    PlaceAt::Unsized(at.0, at.1)
                }
            }
            CaptureKind::ByValue => {
                if kind == FnTrait::FnOnce {
                    let t = f.place_ty(self.checked).ok_or(malformed("a captured place's type past the record's types"))?;
                    let g = self.ground_ty(t).map_err(|error| Self::ty_err(closure, error))?;
                    if self.needs_drop(g).map_err(|error| Self::ty_err(closure, error))? {
                        owned = Some(EnvOwned { g, flag: None, init: true });
                    }
                }
                PlaceAt::Sized(at.0, at.1)
            }
        };
        let j = u32::try_from(self.env_places.env.len()).map_err(|_| malformed("an environment of more than u32 fields"))?;
        self.env_places.env.push(f);
        self.env_places.at.push(Some(place));
        self.env_places.owned.push(owned);
        if owned.is_some() {
            self.scope_pats.push(Held::EnvPlace(j));
        }
        Ok(())
    }

    /// The body of a closure holds field `f`, a whole binding, bound to its own home
    /// (closure.rs `run_closure`): kept beside the place fields for `cover`.
    pub(in crate::walk) fn note_env_whole(&mut self, f: EnvField) {
        self.env_places.env.push(f);
        self.env_places.at.push(None);
        self.env_places.owned.push(None);
    }

    /// After closure or `async` block `owner`'s environment is bound, still at the body's
    /// entry: with no place field, every use takes the plain path; else each owned field whose
    /// state changes under a branch or a loop gets its drop flag, set here.
    pub(in crate::walk) fn settle_env_places(&mut self, owner: ExprId) -> Result<(), BuildError> {
        if self.env_places.at.iter().all(Option::is_none) {
            self.env_places.env.clear();
            self.env_places.at.clear();
            self.env_places.owned.clear();
            return Ok(());
        }
        for j in 0..self.env_places.owned.len() {
            if self.env_places.owned[j].is_none() || !self.env_changes_under_branch(owner, j)? {
                continue;
            }
            let slot = self.b.slot(1, 0, SlotKind::Named);
            self.store_env_flag(slot, true);
            if let Some(o) = self.env_places.owned[j].as_mut() {
                o.flag = Some(slot);
            }
        }
        Ok(())
    }

    /// Whether `x` lies inside `owner` with a conditional edge (a branch, a loop, a guard)
    /// between them (walk.rs `scan_flags`' parents).
    fn under_branch_of(&self, owner: ExprId, x: u32) -> Result<Option<bool>, BuildError> {
        let mut c = x;
        let mut cond = false;
        for _ in 0..=self.body.exprs.len() {
            if c == owner.0 {
                return Ok(Some(cond));
            }
            if c == NO_EXPR {
                return Ok(None);
            }
            let (up, edge) = *self.expr_parent.get(c as usize).ok_or(malformed("an expression without a row of parents (`scan_flags` did not run)"))?;
            // The edge into the owner itself (a closure's body is conditional to its
            // enclosing body) is not the body's own.
            if up != owner.0 {
                cond |= edge;
            }
            c = up;
        }
        Err(malformed("a cycle among a body's expressions"))
    }

    /// Whether owned field `j` of `owner`'s environment is moved out or assigned under a
    /// branch or a loop of the body: a use of exactly its place in a by-value context (walk.rs
    /// `used_in_place`, the rule a local's flag follows; an assignment's left side counts), or
    /// a nested closure or `async` block taking exactly it by value. Errs toward a flag.
    fn env_changes_under_branch(&mut self, owner: ExprId, j: usize) -> Result<bool, BuildError> {
        let body = self.body;
        for i in 0..body.exprs.len() {
            let x = u32::try_from(i).map_err(|_| malformed("an expression index past u32"))?;
            let changes = match body.exprs[i] {
                Expr::Field { .. } | Expr::TupleField { .. } => {
                    let Some(path) = use_path(body, self.checked, ExprId(x), 0).map_err(|error| path_error(ExprId(x), error))? else { continue };
                    if cover(self.checked, &self.env_places.env, &path) != Cover::Exact(j) {
                        continue;
                    }
                    // The context: the parent of the outermost parenthesis around the use.
                    let mut c = x;
                    while let Some(&(up, _)) = self.expr_parent.get(c as usize)
                        && up != NO_EXPR
                        && let Expr::Paren(_) = body.exprs[up as usize]
                    {
                        c = up;
                    }
                    let up = self.expr_parent.get(c as usize).ok_or(malformed("an expression without a row of parents (`scan_flags` did not run)"))?.0;
                    !self.use_keeps_state(c, up)
                }
                Expr::Closure { .. } | Expr::AsyncBlock { .. } if x != owner.0 => {
                    let checked = self.checked;
                    let mut takes = false;
                    for f in checked.env_fields_by::<crate::layout::BuildEnv>(ExprId(x)) {
                        if !f.is_whole() && f.how == CaptureKind::ByValue && self.outer_field(f) == Some((j, f.len)) {
                            takes = true;
                        }
                    }
                    takes
                }
                _ => false,
            };
            if changes && self.under_branch_of(owner, x)? == Some(true) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Stores `on` into drop flag `slot`.
    fn store_env_flag(&mut self, slot: SlotId, on: bool) {
        let at = self.b.slot_addr(slot);
        let v = self.b.iconst(Ty::I8, u128::from(on));
        self.b.store(Flags::NONE, v, at, 0, MemAttrs::aligned(0));
    }

    /// The owned field `j`'s state and place.
    fn env_owned(&self, j: usize) -> Result<(EnvOwned, (Value, i32)), BuildError> {
        let o = self.env_places.owned.get(j).copied().flatten().ok_or(malformed("a drop-scope entry of an environment field the body does not own"))?;
        let Some(PlaceAt::Sized(base, off)) = self.env_places.at.get(j).copied().flatten() else {
            return Err(malformed("an owned environment field without its place"));
        };
        Ok((o, (base, off)))
    }

    /// Owned field `j` of the environment is moved out by `e`: its flag cleared, or its static
    /// state (a second move on a live path is borrowck's E0382). A field the body does not own
    /// is refused (a move out of a borrowed environment, E0507).
    pub(in crate::walk) fn env_moved(&mut self, e: ExprId, j: usize) -> Result<(), BuildError> {
        let Some(o) = self.env_places.owned.get(j).copied().flatten() else {
            return Err(unsupported(e, "a move out of a captured place the closure's body does not own (E0507)"));
        };
        if let Some(slot) = o.flag {
            self.store_env_flag(slot, false);
            return Ok(());
        }
        if !o.init && self.live {
            return Err(unsupported(e, "a use of a captured place already moved (borrowck's E0382)"));
        }
        if let Some(o) = self.env_places.owned[j].as_mut() {
            o.init = false;
        }
        Ok(())
    }

    /// Drops drop-scope entry `Held::EnvPlace(j)` if the field holds its value: statically, or
    /// by its flag (cleared on the way). `kill`: its scope ends (walk.rs `drop_local`).
    pub(in crate::walk) fn drop_env_place(&mut self, j: u32, kill: bool) -> Result<(), BuildError> {
        let (o, at) = self.env_owned(j as usize)?;
        match o.flag {
            None => {
                if o.init {
                    self.drop_at(o.g, at)?;
                }
                if kill && let Some(o) = self.env_places.owned[j as usize].as_mut() {
                    o.init = false;
                }
            }
            Some(slot) => {
                let fa = self.b.slot_addr(slot);
                let f = self.b.load(Ty::I8, Flags::NONE, fa, 0, MemAttrs::aligned(0));
                let zero = self.b.iconst(Ty::I8, 0);
                let set = self.b.icmp(IntCond::Ne, f, zero);
                let yes = self.create();
                let join = self.create();
                self.br(set, (yes, &[]), (join, &[]));
                self.begin(yes);
                self.drop_at(o.g, at)?;
                self.store_env_flag(slot, false);
                self.jump(join, &[]);
                self.begin(join);
            }
        }
        Ok(())
    }

    /// Whether drop-scope entry `Held::EnvPlace(j)` may be dropped here (walk/unwind_ladder.rs
    /// `held_pending`): by its flag, or held by its static state.
    pub(in crate::walk) fn env_place_pending(&self, j: u32) -> bool {
        match self.env_places.owned.get(j as usize).copied().flatten() {
            Some(EnvOwned { flag: Some(_), .. }) => true,
            Some(EnvOwned { flag: None, init, .. }) => init,
            None => false,
        }
    }

    /// `lhs = v` where `lhs` is exactly an owned field of this body's environment, at `at`:
    /// what it holds is dropped first ([expr.assign.drop-target]: unless moved out), then the
    /// store, and it holds its value again. `Ok(false)`: `lhs` is no owned field (walk.rs
    /// `assign_over` takes it).
    pub(in crate::walk) fn assign_env_place(&mut self, lhs: ExprId, at: (Value, i32), v: V, repr: Repr) -> Result<bool, BuildError> {
        if self.env_places.owned.iter().all(Option::is_none) {
            return Ok(false);
        }
        let Some(path) = use_path(self.body, self.checked, lhs, 0).map_err(|error| path_error(lhs, error))? else {
            return Ok(false);
        };
        let Cover::Exact(j) = cover(self.checked, &self.env_places.env, &path) else { return Ok(false) };
        if self.env_places.owned.get(j).copied().flatten().is_none() {
            return Ok(false);
        }
        self.drop_env_place(j as u32, false)?;
        self.store(v, repr, at)?;
        let (o, _) = self.env_owned(j)?;
        match o.flag {
            Some(slot) => self.store_env_flag(slot, true),
            None => {
                if let Some(o) = self.env_places.owned[j].as_mut() {
                    o.init = true;
                }
            }
        }
        Ok(true)
    }

    /// A use of `e` after its first `adjusted` adjustment rows, as a value (`value`: `raw`) or
    /// a place (`place_derefs`), inside a closure body holding place fields: `Ok(true)` when it
    /// pushed the use's value or place (`e`'s place is a field's place, or the reference whose
    /// target a shared-reference field holds); `Ok(false)` for the plain path (no place field,
    /// not a captured place, a place under a field: walked from the field's own use, a whole
    /// binding's field: its home). A use by value of an owned field whose type needs drop moves
    /// it out (`env_moved`). Refused: a move out of a field the body does not own (E0507, the
    /// checker's), and a use of a binding above its captured places (the analysis widens those,
    /// capture_places.rs `widen`).
    pub(in crate::walk) fn env_use(&mut self, e: ExprId, adjusted: u32, value: bool) -> Result<bool, BuildError> {
        if self.env_places.env.is_empty() {
            return Ok(false);
        }
        let Some(path) = use_path(self.body, self.checked, e, adjusted).map_err(|error| path_error(e, error))? else {
            return Ok(false);
        };
        match cover(self.checked, &self.env_places.env, &path) {
            Cover::Exact(j) => {
                let Some(at) = self.env_places.at.get(j).copied().flatten() else { return Ok(false) };
                match at {
                    PlaceAt::Sized(base, off) => {
                        if !value {
                            self.push_val(V::Place(base, off), None);
                            return Ok(true);
                        }
                        let repr = self.expr_repr(e)?;
                        if self.repr_needs_drop(repr) {
                            self.env_moved(e, j)?;
                        }
                        let v = self.load(repr, (base, off));
                        self.push_val(v, None);
                        Ok(true)
                    }
                    PlaceAt::Unsized(w, woff) => {
                        if !value {
                            self.push_val(V::Unsized(w, woff), None);
                            return Ok(true);
                        }
                        Err(unsupported(e, "not lowered: a by-value use of an unsized captured place"))
                    }
                }
            }
            Cover::RefOf(j) => {
                let Some(at) = self.env_places.at.get(j).copied().flatten() else { return Ok(false) };
                match at {
                    PlaceAt::Sized(base, off) => {
                        let p = self.address((base, off));
                        if value {
                            self.push_val(V::Scalar(p), None);
                        } else {
                            let tmp = self.temp(Repr::Ptr);
                            self.store(V::Scalar(p), Repr::Ptr, tmp)?;
                            self.push_val(V::Place(tmp.0, tmp.1), None);
                        }
                    }
                    PlaceAt::Unsized(w, woff) => {
                        if value {
                            self.push_val(V::Place(w, woff), None);
                        } else {
                            let slot = self.b.slot(16, 3, SlotKind::Temp);
                            let to = self.b.slot_addr(slot);
                            let addr = self.b.load(Ty::PTR, Flags::NONE, w, woff, MemAttrs::aligned(3));
                            let meta = self.b.load(Ty::I64, Flags::NONE, w, woff + 8, MemAttrs::aligned(3));
                            self.b.store(Flags::NONE, addr, to, 0, MemAttrs::aligned(3));
                            self.b.store(Flags::NONE, meta, to, 8, MemAttrs::aligned(3));
                            self.push_val(V::Place(to, 0), None);
                        }
                    }
                }
                Ok(true)
            }
            Cover::Under { .. } | Cover::NotCaptured => Ok(false),
            Cover::Above => Err(unsupported(e, "not lowered: a use of a binding above its captured places (the closure holds only parts of it)")),
        }
    }
}
