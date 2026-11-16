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

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Flags, Op, Ty, Value};
use blaze_typeck::{ExprId, PNode, PTy};

use super::{Repr, V, Walk, malformed, unsupported};
use crate::{BuildError, Instance};

impl<A: Allocator + Clone> Walk<'_, '_, '_, A> {
    /// A cast whose source `v` (of `from`) is not a plain scalar, to `to`.
    pub(super) fn cast_other(&mut self, e: ExprId, operand: ExprId, v: V, from: Repr, to: Repr) -> Result<V, BuildError> {
        let t = self.adjusted_ty(operand)?;
        let node = self.node_at(operand, t)?;
        Ok(match (node, from, to) {
            // A fn item: its instance's address, as a pointer or an integer.
            (PNode::FnDef(item, args), Repr::None, Repr::Ptr) => V::Scalar(self.fn_addr(Instance { item, args })),
            (PNode::FnDef(item, args), Repr::None, Repr::Int { ty, .. }) => {
                let p = self.fn_addr(Instance { item, args });
                let addr = self.b.convert(Op::Expose, Flags::NONE, Ty::I64, p);
                V::Scalar(self.resize(addr, Ty::I64, ty, false))
            }
            // A wide pointer: to a thin one its address; to a wide one both words.
            (PNode::Ref(..) | PNode::Ptr(..), Repr::Agg(_), Repr::Ptr) => {
                let at = self.place(v, from)?;
                V::Scalar(self.b.load(Ty::PTR, Flags::NONE, at.0, at.1, MemAttrs::aligned(3)))
            }
            (PNode::Ref(..) | PNode::Ptr(..), Repr::Agg(f), Repr::Agg(t)) => {
                let (fs, ts) = (self.size_align(Repr::Agg(f)), self.size_align(Repr::Agg(t)));
                if fs != ts {
                    return Err(malformed("a cast between wide pointers of different sizes"));
                }
                let at = self.place(v, from)?;
                V::Place(at.0, at.1)
            }
            // An enum with a tag: its discriminant at the target's width. "Casts an enum to its
            // discriminant, then uses a numeric cast": the discriminant is of the enum's integer
            // type (`isize` without a primitive `repr`), so a widening cast extends by THAT type's
            // sign: `repr(usize)`'s `1 << 63` as `u128` is `1 << 63`, not sign-extended.
            (PNode::Adt(k, _), Repr::Agg(_), Repr::Int { ty, .. }) => {
                let at = self.place(v, from)?;
                let d = self.enum_discriminant(operand, t, at)?;
                let signed = !matches!(self.adts.facts().repr(k).int, Some(crate::layout::IntRepr::Unsigned(_)));
                V::Scalar(self.resize(d, Ty::I64, ty, signed))
            }
            // An enum of one variant and no size: that variant's discriminant.
            (PNode::Adt(..), Repr::None, Repr::Int { ty, .. }) => {
                let d = self.lone_discriminant(operand, t)?;
                V::Scalar(self.b.iconst(ty, super::mask(d, ty.bits(super::PTR_BITS))))
            }
            _ => return Err(unsupported(e, "this cast (a wide pointer to an integer, or an aggregate other than an enum)")),
        })
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
