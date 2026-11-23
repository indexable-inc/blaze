//! The casts walk.rs `cast` leaves to this module: those whose source is not a scalar of its
//! own (a wide pointer, an enum, a fn item).
//!
//! The Reference, "Type cast expressions", the table and its notes:
//! - "`*T` where `T: ?Sized` -> `*V` where `V: ?Sized`: Pointer to pointer cast"; when both are
//!   unsized the metadata is kept ("`*const [T]` -> `*const [U]` ... the length is kept"):
//!   the same two words. "`*T` where `T: ?Sized` -> `*V` where `V: Sized`": the address alone
//!   (`slice_ptr as *const T`, `get_unchecked`'s form).
//! - "Function item -> `*V` where `V: Sized`: Function item to pointer cast"; "Function item ->
//!   Integer: Function item to address cast"; "Function item -> Function pointer: Function
//!   item to function pointer cast" ("Function item types": "coerced to a function pointer"):
//!   the address of its instance, as `ReifyFnPointer` makes it (lib.rs, "Fn pointers").
//! - "Enumeration -> Integer type: Enum cast": "Casts an enum to its discriminant, then uses a
//!   numeric cast if needed. Casting is limited to the following kinds of enumerations:
//!   Unit-only enums; Field-less enums without explicit discriminants, or where only unit
//!   variants have explicit discriminants": the discriminant the layout decodes, resized.
//! - `char` -> integer and `u8` -> `char` are integer casts (walk.rs: `char` is `Repr::Int`).
//! - "`e` has type `T` and `T` coerces to `U`: Coercion-cast": the checker records the
//!   coercion as the operand's adjustments (rules/cast.rs, "A trivial cast is a coercion"), so
//!   the adjusted operand has the cast's type and the cast is the identity (a `Box<[T; N]>`
//!   unsized to `Box<[T]>`, `vec!`'s form; a struct cast to itself).
//!
//! Every pointer row, where it is built: thin to thin, a thin or fn pointer to an address, an
//! address to a thin pointer, `&[T; N]` to `*const T`, a fn pointer to a pointer: walk.rs
//! `cast` (one word each). Wide to thin, wide to wide of one metadata kind, fn items: here.
//! Refused by name, as rustc refuses them: wide to an integer (E0606), a length's pointer to a
//! vtable's (E0606), thin or an integer to wide (E0607); the checker does not yet refuse these
//! (rules/cast.rs "Omitted for now: the metadata rules"), so the build does, never building
//! bytes Rust gives no meaning.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Flags, Op, Ty, Value};
use blaze_typeck::{ExprId, PNode, PTy};

use super::{Repr, V, Walk, malformed, unsupported};
use crate::layout::Tail;
use crate::{BuildError, Instance};

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// A cast whose source `v` (of `from`) is not a plain scalar, to `to`.
    pub(super) fn cast_other(&mut self, e: ExprId, operand: ExprId, v: V, from: Repr, to: Repr) -> Result<V, BuildError> {
        let t = self.adjusted_ty(operand)?;
        // A trivial cast ("`e` has type `T` and `T` coerces to `U`: Coercion-cast"): the
        // checker decided it (blaze-typeck rules/cast.rs `check_casts`, "A trivial cast is a
        // coercion") and recorded the coercion as the operand's adjustments, which the walk
        // applied: the value now has the cast's type, so the cast is its value unchanged
        // (`b.assume_init() as Box<[T]>` in alloc's `box_assume_init_into_vec_unsafe`, which
        // `vec![..]` expands to: an unsizing of a `Box`, then nothing). The checker records no
        // cast kind; an operand whose adjusted type IS the target is the one fact read, and
        // equal ground types are equal bits, whatever the cast's row.
        let g_from = self.ground_ty(t).map_err(|error| Self::ty_err(operand, error))?;
        let to_ty = self.expr_ty(e)?;
        let g_to = self.ground_ty(to_ty).map_err(|error| Self::ty_err(e, error))?;
        if g_from == g_to {
            return Ok(v);
        }
        let node = self.node_at(operand, t)?;
        Ok(match (node, from, to) {
            // A fn item: its instance's address, as a pointer or an integer.
            (PNode::FnDef(item, args), Repr::None, Repr::Ptr) => V::Scalar(self.reify_addr(e, Instance { item, args })?),
            (PNode::FnDef(item, args), Repr::None, Repr::Int { ty, .. }) => {
                let p = self.reify_addr(e, Instance { item, args })?;
                let addr = self.b.convert(Op::Expose, Flags::NONE, Ty::I64, p);
                V::Scalar(self.resize(addr, Ty::I64, ty, false))
            }
            // A wide pointer: to a thin one its address; to a wide one both words.
            (PNode::Ref(..) | PNode::Ptr(..), Repr::Agg(_), Repr::Ptr) => {
                let at = self.place(v, from)?;
                V::Scalar(self.b.load(Ty::PTR, Flags::NONE, at.0, at.1, MemAttrs::aligned(3)))
            }
            // Wide to wide: "`*const [T]` -> `*const [U]` ... the length is kept", and a trait
            // object's pointer to one of the same principal keeps its vtable: both words, when
            // the metadata is of one kind. A length and a vtable are no cast (rustc's E0606,
            // `PtrPtrCast` with differing `PointerKind`s; the checker does not yet refuse it,
            // rules/cast.rs "Omitted for now"): refused by name, never reinterpreted.
            (PNode::Ref(..) | PNode::Ptr(..), Repr::Agg(f), Repr::Agg(tl)) => {
                let (fs, ts) = (self.size_align(Repr::Agg(f)), self.size_align(Repr::Agg(tl)));
                if fs != ts {
                    return Err(malformed("a cast between wide pointers of different sizes"));
                }
                let (mf, mt) = (self.pointee_tail(operand, g_from)?, self.pointee_tail(e, g_to)?);
                let same = matches!((mf, mt), (Some(Tail::Slice { .. }), Some(Tail::Slice { .. })) | (Some(Tail::Dyn), Some(Tail::Dyn)));
                if !same {
                    return Err(unsupported(e, "a cast between wide pointers whose metadata differ in kind (a length and a vtable: rustc's E0606)"));
                }
                let at = self.place(v, from)?;
                V::Place(at.0, at.1)
            }
            // "Pointer to address cast" is for `*T where T: Sized` only: a wide pointer to an
            // integer is rustc's E0606 ("cast through a thin pointer first").
            (PNode::Ref(..) | PNode::Ptr(..), Repr::Agg(_), Repr::Int { .. }) => {
                return Err(unsupported(e, "a wide pointer cast to an integer (the Reference's pointer-to-address cast takes a thin pointer: rustc's E0606)"));
            }
            // A thin pointer or an integer to a wide pointer: no metadata to give it (rustc's
            // E0607 and E0606).
            (_, Repr::Ptr | Repr::Int { .. }, Repr::Agg(_)) if self.is_wide_ptr(e, g_to)? => {
                return Err(unsupported(e, "a thin pointer or an integer cast to a wide pointer (no metadata to give it: rustc's E0607, E0606)"));
            }
            // An enum with a tag: its discriminant at the target's width. "Casts an enum to its
            // discriminant, then uses a numeric cast": the discriminant is of the enum's integer
            // type (`isize` without a primitive `repr`), so a widening cast extends by THAT type's
            // sign: `repr(usize)`'s `1 << 63` as `u128` is `1 << 63`, not sign-extended.
            (PNode::Adt(k, _), Repr::Agg(_), Repr::Int { ty, .. }) => {
                let at = self.place(v, from)?;
                let d = self.enum_discriminant(operand, t, at)?;
                let repr = self.adts.facts().repr(k).map_err(|m| BuildError::NoAdt { adt: m.item })?;
                let signed = !matches!(repr.int, Some(crate::layout::IntRepr::Unsigned(_)));
                V::Scalar(self.resize(d, Ty::I64, ty, signed))
            }
            // An enum of one variant and no size: that variant's discriminant.
            (PNode::Adt(..), Repr::None, Repr::Int { ty, .. }) => {
                let d = self.lone_discriminant(operand, t)?;
                V::Scalar(self.b.iconst(ty, super::mask(d, ty.bits(super::PTR_BITS))))
            }
            _ => return Err(unsupported(e, "this cast (an aggregate other than an enum, past every row of the Reference's cast table built here)")),
        })
    }

    /// The metadata kind of pointer type `p` (ground): its pointee's layout tail, `None` for a
    /// pointer that is not a reference or a raw pointer.
    fn pointee_tail(&mut self, at: ExprId, p: PTy) -> Result<Option<Tail>, BuildError> {
        let pointee = match self.ground_node(p).map_err(|error| Self::ty_err(at, error))? {
            PNode::Ref(_, inner) | PNode::Ptr(_, inner) => inner,
            _ => return Ok(None),
        };
        let id = self.ls_of(pointee).map_err(|error| Self::ty_err(at, error))?;
        Ok(Some(self.ls.get(id).tail))
    }

    /// Whether the cast target of ground type `g`, held as an aggregate, is a (wide) pointer.
    fn is_wide_ptr(&mut self, at: ExprId, g: PTy) -> Result<bool, BuildError> {
        Ok(matches!(self.ground_node(g).map_err(|error| Self::ty_err(at, error))?, PNode::Ref(..) | PNode::Ptr(..)))
    }

    /// The discriminant of the enum of type `t` at `at`, an `i64`.
    fn enum_discriminant(&mut self, operand: ExprId, t: PTy, at: (Value, i32)) -> Result<Value, BuildError> {
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(operand, error))?;
        let id = self.ls_of(g).map_err(|error| Self::ty_err(operand, error))?;
        if !self.is_enum_layout(id) {
            return Err(unsupported(operand, "a cast of a struct or union to an integer"));
        }
        self.read_discriminant(id, at)
    }

    /// The discriminant of the one variant of the zero-sized enum of type `t`.
    fn lone_discriminant(&mut self, operand: ExprId, t: PTy) -> Result<u128, BuildError> {
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(operand, error))?;
        let id = self.ls_of(g).map_err(|error| Self::ty_err(operand, error))?;
        let crate::layout::Variants::Single { index } = self.ls.get(id).variants else {
            return Err(malformed("a zero-sized enum with a tag"));
        };
        if !self.is_enum_layout(id) {
            return Err(unsupported(operand, "a cast of a zero-sized struct to an integer"));
        }
        let d = self.ls.discriminant(id, index).ok_or(malformed("a variant past its enum's discriminants"))?;
        Ok(super::mask(d as u128, 128))
    }

    /// Whether layout `id` is an enum's: only an enum's row lists discriminants.
    fn is_enum_layout(&self, id: crate::layout::LayoutId) -> bool {
        self.ls.discriminant(id, 0).is_some()
    }
}
