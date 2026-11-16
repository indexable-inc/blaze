//! `repr(simd)` vectors: their layout ([`Layouts::simd`]).
//!
//! The Reference (1.88) has no section on `repr(simd)`; the pinned library is the evidence.
//! `library/core/src/intrinsics/simd/mod.rs:3`: "In this module, a "vector" is any `repr(simd)`
//! type." Every NEON vector type is made by `stdarch/crates/core_arch/src/macros.rs`'s
//! `types!`: `#[repr(simd)] pub struct $name($v [$elem_type; $len]);`, and the same macro's
//! `as_array` states the layout: "this type is just an overaligned `[T; N]` with potential
//! padding at the end, so pointer casting to a `&[T; N]` is safe."
//!
//! So a vector is laid out as a one-field struct whose field is the array `[E; N]` at offset
//! 0, with the alignment raised as `repr(align(n))` raises it. The alignment is the TARGET's:
//! AAPCS64 (section 5.9, "Short vectors") gives a 64-bit short vector 8-byte alignment and a
//! 128-bit one 16-byte alignment, the vector's size; this is what makes `vld1_u8`'s
//! `read_unaligned` and `as_array` agree. Only those two sizes are laid out: a vector of any
//! other size (rustc pads a non-power-of-two one, and gives a 32-byte one 32-byte alignment)
//! is refused by name, as is an element that is not an integer, a float (`f16`, `f32`, `f64`)
//! or a thin pointer, the older form of N scalar fields (the pinned library declares none),
//! and `repr(simd, packed)`.
//!
//! A vector has no scalar or pair form and no niche: the walk holds it as an aggregate
//! (`Repr::Agg`) and the Rust ABI passes it by the address of the caller's copy (lib.rs,
//! "Rust ABI"). A C call taking or returning a vector is refused by name in `cabi::class_of`:
//! AAPCS64 passes a short vector in one SIMD register (`v0` to `v7`), which blaze's C calls
//! do not build yet; nothing about a vector is ABI-compatible with C until they do.

use core::alloc::Allocator;

use blaze_own::Alloc;
use blaze_typeck::{Adt, AdtKind, FloatTy, PNode, PTy};

use super::{Fields, LayoutCx, LayoutError, LayoutId, Layouts, Order, Prim, Refusal, ReprAttrs};
use crate::ground;

#[cfg(test)]
mod tests;

/// The vector sizes laid out: AAPCS64's short vectors, 64 and 128 bits.
const SHORT_VECTOR_BYTES: [u64; 2] = [8, 16];

/// A vector's lanes, as the `simd_*` intrinsics read them (intrinsics/simd.rs): lane `i` is
/// the element at byte `i * stride` of the vector's memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lanes {
    pub count: u64,
    /// The element's primitive (an integer, `f16`/`f32`/`f64` or a thin pointer).
    pub elem: Prim,
    /// The element's size: the bytes from one lane to the next.
    pub stride: u64,
    /// The vector's size (8 or 16) and log2 of its alignment.
    pub size: u64,
    pub align: u8,
}

impl<A: Allocator + Clone> Layouts<A> {
    /// The lanes of ground type `t` when it is a `repr(simd)` vector; `None` for any other
    /// type. Lays `t` out first, so a vector [`Layouts::simd`] refuses is refused here too.
    pub fn lanes(&mut self, t: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<Option<Lanes>, LayoutError> {
        let id = self.of(t, cx)?;
        let refuse = |why| LayoutError { ty: t, why };
        let facts = cx.facts;
        let view = cx.ground.view();
        let PNode::Adt(key, l) = view.node(t).map_err(|_| refuse(Refusal::Malformed("a type past the ground table")))? else {
            return Ok(None);
        };
        if !facts.repr(key).simd {
            return Ok(None);
        }
        let adt = facts.adt(key).ok_or(refuse(Refusal::NoAdt(key)))?;
        let field = adt.fields.first().ok_or(refuse(Refusal::Malformed("a `repr(simd)` type without a field")))?;
        let list = view.list(l).map_err(|_| refuse(Refusal::Malformed("a list past the ground table")))?;
        let mut args: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::with_capacity_in(list.len(), cx.scratch);
        args.extend_from_slice(list);
        let array = ground(adt.types, field.ty, args.as_slice(), cx.ground, cx.scratch, facts).map_err(|e| refuse(Refusal::Ground(e)))?;
        let array = self.of(array, cx)?;
        let Fields::Array { elem, stride, count } = self.get(array).fields else {
            return Err(refuse(Refusal::Malformed("a laid-out vector whose field is not an array")));
        };
        let elem = self.get(elem).scalar.ok_or(refuse(Refusal::Malformed("a laid-out vector whose element is not a scalar")))?.prim;
        let v = *self.get(id);
        Ok(Some(Lanes { count, elem, stride, size: v.size, align: v.align }))
    }

    /// The layout of `t`, a `repr(simd)` ADT `adt` with `attrs`, its fields' ground types
    /// `parts` laid out already.
    pub(super) fn simd(&mut self, t: PTy, adt: &Adt<'_>, attrs: ReprAttrs, cx: &mut LayoutCx<'_, '_>, parts: &[PTy]) -> Result<LayoutId, Refusal> {
        if adt.kind != AdtKind::Struct {
            return Err(Refusal::Malformed("a `repr(simd)` enum or union (only a struct is a vector)"));
        }
        if attrs.packed.is_some() {
            return Err(Refusal::Unsupported("a `repr(simd, packed)` type (lanes at the element's alignment)"));
        }
        if attrs.c || attrs.transparent || attrs.int.is_some() {
            return Err(Refusal::Unsupported("a `repr(simd)` type with another `repr` hint"));
        }
        let [variant] = adt.variants else { return Err(Refusal::Malformed("a struct without exactly one variant")) };
        let fields = parts.get(variant.fields.range()).ok_or(Refusal::Malformed("a struct's fields past their column"))?;
        let field = match fields {
            [one] => *one,
            [] => return Err(Refusal::Malformed("a `repr(simd)` type without a field")),
            _ => return Err(Refusal::Unsupported("a `repr(simd)` type of several scalar fields (the older form; the pinned library declares every vector as one array field)")),
        };
        let view = cx.ground.view();
        let PNode::Array(elem, _) = view.node(field).map_err(|_| Refusal::Malformed("a type past the ground table"))? else {
            return Err(Refusal::Unsupported("a `repr(simd)` type whose field is not an array"));
        };
        let scalar_kind = match view.node(elem).map_err(|_| Refusal::Malformed("a type past the ground table"))? {
            PNode::Int(_) | PNode::Uint(_) | PNode::Ptr(..) => true,
            PNode::Float(f) => f != FloatTy::F128,
            _ => false,
        };
        let array = *self.get(self.done(field).ok_or(Refusal::Malformed("a field not laid out"))?);
        let lane = *self.get(self.done(elem).ok_or(Refusal::Malformed("an element not laid out"))?);
        // A thin raw pointer is one scalar; a wide one (to a slice or `dyn`) is a pair.
        let lane_is_scalar = matches!(lane.scalar, Some(s) if s.prim != Prim::F128);
        if !scalar_kind || !lane_is_scalar {
            return Err(Refusal::Unsupported("a `repr(simd)` type whose element is not an integer, float or thin pointer"));
        }
        if !SHORT_VECTOR_BYTES.contains(&array.size) {
            return Err(Refusal::Unsupported("a `repr(simd)` type whose size is not 8 or 16 bytes (only AAPCS64's short vectors are laid out)"));
        }
        // The target's alignment of a short vector, its size (8 or 16: log2 3 or 4), raised as
        // `repr(align(n))` raises a struct's.
        let vector_align = array.size.trailing_zeros() as u8;
        let raised = ReprAttrs { align: Some(attrs.align.map_or(vector_align, |a| a.max(vector_align))), ..attrs };
        let mut u = self.univariant(fields, Order::Declaration, raised, 0, 0)?;
        // An aggregate: no scalar or pair form (an array has none; said here, not inherited),
        // and no niche (every bit pattern of an integer or float lane is valid; a vector is
        // never an enum's niche carrier).
        u.scalar = None;
        u.pair = None;
        u.niche = None;
        let row = self.struct_row(t, u);
        self.push(row)
    }
}
