//! Unsizing through a pointer nested in structs: `Box<T>` to `Box<dyn Tr>` (core's `Box` is
//! `Box(Unique<T>, A)`, `Unique` is `{ pointer: NonNull<T>, _marker: PhantomData<T> }`,
//! `NonNull` is `{ pointer: *const T }`), `Pin<Box<{async block}>>` to
//! `Pin<Box<dyn Future<Output = u8> + Send>>`, a user `Wrap<T: ?Sized>(u32, Box<T>)` to
//! `Wrap<dyn Tr>`, and the plain `&[T; N]` to `&[T]` and `&T` to `&dyn Tr`.
//!
//! The rule. The Rust Reference, "Type coercions", "Unsized coercions": "`Foo<..., T, ...>` to
//! `Foo<..., U, ...>`, when: `Foo` is a struct. `T` implements `Unsize<U>`. The last field of
//! `Foo` has a type involving `T`. If that field has type `Bar<T>`, then `Bar<T>` implements
//! `Unsize<Bar<U>>`. T is not part of the type of any other fields." And core's
//! `CoerceUnsized` (library/core/src/ops/unsize.rs, the pinned nightly's): "Such an impl can
//! only be written if `Foo<T>` has only a single non-phantomdata field involving `T`. If the
//! type of that field is `Bar<T>`, an implementation of `CoerceUnsized<Bar<U>> for Bar<T>`
//! must exist. The coercion will work by coercing the `Bar<T>` field into `Bar<U>` and filling
//! in the rest of the fields from `Foo<T>` to create a `Foo<U>`. This will effectively drill
//! down to a pointer field and coerce that."
//!
//! So the build follows the coercion by structure, never by a type's name (no case for `Pin`,
//! `Box` or `Rc`), with a work list of steps (no recursion), at most [`UNSIZE_DEPTH`] deep:
//! - at a struct (the same ADT on both sides; its two layouts are the builder's own, computed
//!   for each side, never assumed equal: once the pointer is wide a field after it may move),
//!   every field whose type is the same on both sides is moved unchanged, from its offset in
//!   the source to its offset in the target; exactly one field holding bytes differs, and the
//!   walk steps into it. A field that differs and is zero-sized on both sides (core's
//!   `PhantomData<T>`) holds no bytes, so there is nothing of it to move or coerce: whether it
//!   may differ is the impl's legality, the checker's question, not the build's;
//! - at a pattern type on both sides with the same pattern (core's `NonNull<T>` holds
//!   `pattern_type!(*const T is !null)`; core/src/pat.rs makes it `CoerceUnsized` as its
//!   base), the walk steps into the bases: a pattern type is laid out and held as its base;
//! - at a reference or raw pointer on both sides, the pointee changes from `T` to `U`: the
//!   thin address is kept and the second word made (the length of an array, the table of the
//!   concrete type for a `dyn`, the upcast table of a `dyn`), as for a bare `&T` (walk.rs
//!   `unsize`). A pointee that is itself the same struct on both sides (`RcInner<T>` to
//!   `RcInner<dyn Tr>`: the Reference, "Unsize": the struct's last field) has the metadata of
//!   its last field, followed the same way.
//!
//! Refused by name ([`BuildError::Unsize`], naming the ADT and the rule): a struct where no
//! field or more than one field holding bytes differs, a union, an enum, two different ADTs,
//! anything else on the way to the pointer, and a nesting deeper than [`UNSIZE_DEPTH`].

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Flags, Ty, Value};
use blaze_typeck::{AdtKind, ExprId, ItemKey, PNode, PTy};

use super::{Repr, Shape, V, Walk, malformed};
use crate::{BuildError, ground};

/// The most structs an unsizing passes through before its pointer, and the most struct tails
/// its pointee passes through: `Pin<Box<T>>` is four (`Pin`, `Box`, `Unique`, `NonNull`), so
/// sixteen leaves room for user wrappers around the library's and still bounds the walk.
pub(super) const UNSIZE_DEPTH: u32 = 16;

/// The rule every refusal here names.
const RULE: &str = "the Reference, \"Type coercions\", \"Unsized coercions\"; core's `CoerceUnsized`";

/// Where the walk of one unsizing ended: the pointee before and after (ground types, after
/// struct tails were followed), how many structs it passed through, and, when it was asked to
/// move the other fields, the places of the pointer in the source and in the target.
pub(super) struct UnsizeWalk {
    pub(super) from: PTy,
    pub(super) to: PTy,
    pub(super) depth: u32,
    pub(super) at: Option<((Value, i32), (Value, i32))>,
}

fn refused(expr: ExprId, adt: Option<ItemKey>, why: &'static str) -> BuildError {
    BuildError::Unsize { expr, adt, why, rule: RULE }
}

/// `base` moved by a field's offset, an offset `ls` keeps under 2^31 (walk.rs `layout`).
fn offset_by((p, off): (Value, i32), by: u32) -> Result<(Value, i32), BuildError> {
    let by = i32::try_from(by).map_err(|_| malformed("a field offset past 2^31 bytes"))?;
    let off = off.checked_add(by).ok_or(malformed("a field offset past 2^31 bytes"))?;
    Ok((p, off))
}

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Adjustment `n` of `e`, an unsizing of the value on top (a thin or wide pointer, or a
    /// struct holding one, any depth): the target value is built in a fresh temporary: the
    /// other fields moved to their offsets in the target, then the pointer's two words.
    pub(super) fn unsize_nested(&mut self, e: ExprId, n: usize) -> Result<(), BuildError> {
        let before = self.ty_before(e, n)?;
        let after = self.checked.adjusts.column::<2>()[self.adjust_rows(e).start + n];
        let gb = self.ground_ty(before).map_err(|error| Self::ty_err(e, error))?;
        let ga = self.ground_ty(after).map_err(|error| Self::ty_err(e, error))?;
        let v = self.pop()?;
        let (rb, ra) = (self.repr_at(e, before)?, self.repr_at(e, after)?);
        let dst = self.temp(ra);
        // The source pointer's place (none: a bare thin pointer, the scalar `v`) and the
        // target pointer's place.
        let (walk, src, d) = match rb {
            Repr::Ptr => {
                let walk = self.unsize_walk(e, gb, ga, None)?;
                if walk.depth != 0 {
                    return Err(malformed("a struct held as a bare pointer"));
                }
                (walk, None, dst)
            }
            Repr::Agg(_) => {
                let src = self.place(v, rb)?;
                let walk = self.unsize_walk(e, gb, ga, Some((src, dst)))?;
                let (s, d) = walk.at.ok_or(malformed("an unsizing walk that lost its places"))?;
                (walk, Some(s), d)
            }
            _ => return Err(malformed("an unsized pointer held as other than a pointer or an aggregate")),
        };
        let nf = self.ground_node(walk.from).map_err(|error| Self::ty_err(e, error))?;
        // The data address; and an upcast's source is already wide: its second word is the
        // table to start from.
        let (p, old) = match src {
            None => (self.scalar(v, Ty::PTR)?, None),
            Some(s) => {
                let p = self.load(Repr::Ptr, s);
                let p = self.scalar(p, Ty::PTR)?;
                let old = match nf {
                    PNode::Dynamic(_) => {
                        let w = self.load(Repr::Ptr, offset_by(s, 8)?);
                        Some(self.scalar(w, Ty::PTR)?)
                    }
                    _ => None,
                };
                (p, old)
            }
        };
        let second = self.unsize_metadata(e, walk.from, walk.to, old)?;
        self.b.store(Flags::NONE, p, d.0, d.1, MemAttrs::aligned(3));
        let d8 = offset_by(d, 8)?;
        self.b.store(Flags::NONE, second, d8.0, d8.1, MemAttrs::aligned(3));
        self.push_val(V::Place(dst.0, dst.1), None);
        Ok(())
    }

    /// The walk from ground source `s` to ground target `t` of an unsizing, by a work list: at
    /// each struct, the fields that are the same moved (when `at` gives the source and target
    /// places) and the one that differs stepped into; it ends at a reference or raw pointer on
    /// both sides, whose pointees are returned after their struct tails are followed.
    pub(super) fn unsize_walk(
        &mut self,
        e: ExprId,
        s: PTy,
        t: PTy,
        mut at: Option<((Value, i32), (Value, i32))>,
    ) -> Result<UnsizeWalk, BuildError> {
        let (mut s, mut t) = (s, t);
        let mut depth = 0u32;
        loop {
            let ns = self.ground_node(s).map_err(|error| Self::ty_err(e, error))?;
            let nt = self.ground_node(t).map_err(|error| Self::ty_err(e, error))?;
            let key = match (ns, nt) {
                (PNode::Ref(_, f) | PNode::Ptr(_, f), PNode::Ref(_, g) | PNode::Ptr(_, g)) => {
                    let (from, to) = self.unsize_tails(e, f, g)?;
                    return Ok(UnsizeWalk { from, to, depth, at });
                }
                // A pattern type on both sides (core's `NonNull<T>` is `{ pointer:
                // pattern_type!(*const T is !null) }`; core/src/pat.rs: `impl<T, U>
                // CoerceUnsized<pattern_type!(*const U is !null)> for pattern_type!(*const T is
                // !null) where T: Unsize<U>`): held as its base (walk.rs `repr`), at offset 0
                // and with no other bytes, so the walk steps into the bases. The pattern itself
                // must be the same: a coercion keeps the value, so it cannot widen or narrow it.
                (PNode::Pat(bs, ps), PNode::Pat(bt, pt)) => {
                    let full = || malformed("a pattern type's ranges past the ground table");
                    let view = self.ground.view();
                    if view.list(ps).map_err(|_| full())? != view.list(pt).map_err(|_| full())? {
                        return Err(refused(e, None, "an unsizing between two pattern types of different patterns"));
                    }
                    s = bs;
                    t = bt;
                    continue;
                }
                (PNode::Adt(k, _), PNode::Adt(k2, _)) if k == k2 => k,
                (PNode::Adt(k, _), PNode::Adt(..)) => {
                    return Err(refused(e, Some(k), "an unsizing between two different ADTs"));
                }
                (PNode::Adt(k, _), _) | (_, PNode::Adt(k, _)) => {
                    return Err(refused(e, Some(k), "an unsizing between a struct and a type that is not the same struct"));
                }
                _ => return Err(refused(e, None, "an unsizing of other than a pointer or a struct holding one")),
            };
            if depth >= UNSIZE_DEPTH {
                return Err(refused(e, Some(key), "an unsizing through more nested structs than the build's bound (`UNSIZE_DEPTH`, 16)"));
            }
            let ls = self.layout(s).map_err(|error| Self::ty_err(e, error))? as usize;
            let lt = self.layout(t).map_err(|error| Self::ty_err(e, error))? as usize;
            if self.layouts[ls].is_union || self.layouts[lt].is_union {
                return Err(refused(e, Some(key), "an unsizing through a union (its fields overlap: no one field is coerced)"));
            }
            let (Shape::Fields { first: fs, len: ls_len }, Shape::Fields { first: ft, len: lt_len }) =
                (self.layouts[ls].shape, self.layouts[lt].shape)
            else {
                return Err(refused(e, Some(key), "an unsizing through an enum (only a struct's one field is coerced)"));
            };
            if ls_len != lt_len {
                return Err(malformed("one struct laid out with two field counts"));
            }
            let mut step: Option<(PTy, PTy, u32, u32)> = None;
            for j in 0..ls_len {
                let (is, it) = ((fs + j) as usize, (ft + j) as usize);
                let (fty_s, fty_t) = (self.layout_field_tys[is], self.layout_field_tys[it]);
                let (_, os, rs) = self.layout_fields[is];
                let (_, ot, rt) = self.layout_fields[it];
                if fty_s == fty_t {
                    // The same type on both sides: moved unchanged, offset to offset.
                    if let Some((src, dst)) = at {
                        let v = self.load(rs, offset_by(src, os)?);
                        self.store(v, rt, offset_by(dst, ot)?)?;
                    }
                    continue;
                }
                // Differs, and holds no bytes on either side (core's `PhantomData<T>`).
                if self.size_align(rs).0 == 0 && self.size_align(rt).0 == 0 {
                    continue;
                }
                if step.is_some() {
                    return Err(refused(
                        e,
                        Some(key),
                        "an unsizing through a struct where more than one field's type differs (exactly one field may involve `T`)",
                    ));
                }
                step = Some((fty_s, fty_t, os, ot));
            }
            let Some((fty_s, fty_t, os, ot)) = step else {
                return Err(refused(e, Some(key), "an unsizing through a struct where no field holding bytes differs"));
            };
            at = match at {
                Some((src, dst)) => Some((offset_by(src, os)?, offset_by(dst, ot)?)),
                None => None,
            };
            s = fty_s;
            t = fty_t;
            depth += 1;
        }
    }

    /// The pointees `f` and `g` of an unsizing, each through its struct tails while both are
    /// the same struct and differ (the Reference, "Unsize": a struct whose last field
    /// involves `T`), by a work list: the pair whose metadata the wide pointer carries.
    fn unsize_tails(&mut self, e: ExprId, f: PTy, g: PTy) -> Result<(PTy, PTy), BuildError> {
        let (mut f, mut g) = (f, g);
        for _ in 0..UNSIZE_DEPTH {
            if f == g {
                return Ok((f, g));
            }
            let nf = self.ground_node(f).map_err(|error| Self::ty_err(e, error))?;
            let ng = self.ground_node(g).map_err(|error| Self::ty_err(e, error))?;
            let (k, lf, lg) = match (nf, ng) {
                (PNode::Adt(k, lf), PNode::Adt(k2, lg)) if k == k2 => (k, lf, lg),
                _ => return Ok((f, g)),
            };
            let adts = self.adts;
            let adt = adts.adt(k).ok_or(BuildError::NoAdt { adt: k })?;
            if !matches!(adt.kind, AdtKind::Struct) || adt.variants.len() != 1 {
                return Err(refused(e, Some(k), "an unsizing of a pointee that is an enum or a union (only a struct's last field is unsized)"));
            }
            let fields = adt.fields.get(adt.variants[0].fields.range()).ok_or(malformed("an ADT's fields past their column"))?;
            let Some(last) = fields.last() else {
                return Err(refused(e, Some(k), "an unsizing of a pointee struct without fields"));
            };
            let (types, last_ty) = (adt.types, last.ty);
            let full = || malformed("a struct's arguments past the ground table");
            let mut args: blaze_own::Vec<PTy, A> = blaze_own::Vec::new_in(self.alloc);
            args.extend_from_slice(self.ground.view().list(lf).map_err(|_| full())?);
            let tf = ground(types, last_ty, &args, self.ground, self.scratch, self.adts.facts())?;
            args.clear();
            args.extend_from_slice(self.ground.view().list(lg).map_err(|_| full())?);
            let tg = ground(types, last_ty, &args, self.ground, self.scratch, self.adts.facts())?;
            f = tf;
            g = tg;
        }
        Err(refused(e, None, "an unsizing whose pointee passes through more struct tails than the build's bound (`UNSIZE_DEPTH`, 16)"))
    }

    /// The second word of the wide pointer an unsizing makes from pointee `from` to `to`
    /// (ground; `old`: the source's second word when it is already wide, an upcast):
    /// - `[T; N]` to `[T]`: `N`;
    /// - a concrete `T` to `dyn Tr`: `(T, Tr)`'s table;
    /// - `dyn Sub` to `dyn Tr`: the same table on the first supertrait chain, else the word
    ///   `Shape::upcast` names; an auto trait dropped: the same table.
    pub(super) fn unsize_metadata(&mut self, e: ExprId, from: PTy, to: PTy, old: Option<Value>) -> Result<Value, BuildError> {
        let node = |w: &Self, t: PTy| w.ground_node(t).map_err(|error| Self::ty_err(e, error));
        let (nf, nt) = (node(self, from)?, node(self, to)?);
        Ok(match (nf, nt, old) {
            (PNode::Array(_, len), PNode::Slice(_), None) => {
                let len = self.ground_len(len).map_err(|error| Self::ty_err(e, error))?;
                self.b.iconst(Ty::I64, u128::from(len))
            }
            (PNode::Dynamic(_), PNode::Dynamic(_), Some(vt)) => {
                let (pf, pt) = (self.principal(e, from)?, self.principal(e, to)?);
                match (pf, pt) {
                    (_, None) => return Err(refused(e, None, "an unsizing to a trait object of auto traits alone (no principal trait)")),
                    (Some((a, _)), Some((b, _))) if a == b => vt,
                    (Some((a, _)), Some((b, _))) => {
                        let shape = crate::vtable::Shape::of(a, |t| self.adts.object_trait(t), self.scratch)
                            .map_err(|error| BuildError::Vtable { at: Some(e), error })?;
                        match shape.upcast(b) {
                            Some(crate::vtable::Upcast::Prefix) => vt,
                            Some(crate::vtable::Upcast::Load(slot)) => {
                                let off = 8 * (crate::vtable::HEADER_WORDS + slot);
                                let off = i32::try_from(off).map_err(|_| malformed("a vtable slot past 2^31 bytes"))?;
                                self.b.load(Ty::PTR, Flags::NONE, vt, off, MemAttrs::aligned(3))
                            }
                            None => return Err(malformed("an upcast to a trait that is not a supertrait")),
                        }
                    }
                    (None, Some(_)) => return Err(malformed("an upcast from a trait object without a principal")),
                }
            }
            (PNode::Dynamic(_), _, _) | (_, _, Some(_)) => return Err(malformed("an unsizing of a trait object to other than a trait object")),
            (_, PNode::Dynamic(_), None) => {
                let Some((principal, args)) = self.principal(e, to)? else {
                    return Err(refused(e, None, "an unsizing to a trait object of auto traits alone (no principal trait)"));
                };
                self.vtable_addr(crate::vtable::VtableRef { concrete: from, principal, args })?
            }
            _ => return Err(refused(e, None, "an unsizing whose pointer's pointee changes to other than a slice or a trait object")),
        })
    }
}
