//! A struct ending in a trait object: `ArcInner<dyn Tr>`, `RcInner<dyn Tr>`,
//! `struct S { a: u8, t: dyn Tr }`.
//!
//! The Reference, "Type layout": a field is placed at the next offset that is a multiple of its
//! alignment; a trait object's alignment is its value's, word 2 of the vtable its pointer
//! carries (codegen.md "Trait objects": drop_in_place at word 0, the size at 1, the alignment at
//! 2). So the sized prefix's fields are at static offsets, and the trait object at
//!
//! ```text
//! offset = round_up(prefix_end, vtable.align)
//! ```
//!
//! where `prefix_end` is where the last sized field ends ([`DynPrefix::tail_at`]: `ls` places
//! the `dyn` field there, `dyn`'s own static alignment being 1). The value's alignment is the
//! larger of the prefix's ([`DynPrefix::align`], `repr(align)` included) and the vtable's, and
//! its size is `round_up(offset + vtable.size, align)`: `size_of_val` / `align_of_val`, which
//! `Layout::for_value` (`Arc`'s and `Rc`'s deallocation) reads. These are rustc's
//! `size_and_align_of_dst` for one level of tail; a struct whose last field is itself a struct
//! ending in `dyn` is refused by name in `ls` (layout.rs `univariant`), as is a packed one.

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Builder, Flags, Op, Sink, Ty, Value};

use crate::layout::DynPrefix;

/// The vtable's word holding the value's size.
const SIZE_WORD: i32 = 8;
/// The vtable's word holding the value's alignment (a power of two).
const ALIGN_WORD: i32 = 16;

/// `x` rounded up to a multiple of the power of two `align` (both `i64`): `(x + align - 1) &
/// -align`.
fn round_up<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, x: Value, align: Value) -> Value {
    let one = b.iconst(Ty::I64, 1);
    let mask = b.binary(Op::Sub, Flags::NONE, align, one);
    let up = b.binary(Op::Add, Flags::NONE, x, mask);
    let zero = b.iconst(Ty::I64, 0);
    let keep = b.binary(Op::Sub, Flags::NONE, zero, align);
    b.binary(Op::And, Flags::NONE, up, keep)
}

/// The trait object's offset in the value whose vtable is `vtable` (an `i64`).
pub(crate) fn tail_offset<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, prefix: DynPrefix, vtable: Value) -> Value {
    let va = b.load(Ty::I64, Flags::NONE, vtable, ALIGN_WORD, MemAttrs::aligned(3));
    let at = b.iconst(Ty::I64, u128::from(prefix.tail_at));
    round_up(b, at, va)
}

/// The value's size and alignment (both `i64`), by the vtable `vtable`.
pub(crate) fn size_and_align<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, prefix: DynPrefix, vtable: Value) -> (Value, Value) {
    let vs = b.load(Ty::I64, Flags::NONE, vtable, SIZE_WORD, MemAttrs::aligned(3));
    let va = b.load(Ty::I64, Flags::NONE, vtable, ALIGN_WORD, MemAttrs::aligned(3));
    // A bare `dyn Tr`: the empty prefix; the vtable's size is already a multiple of its
    // alignment (the Reference, "Type layout": "the size of a value is always a multiple of
    // its alignment").
    if prefix.tail_at == 0 && prefix.align == 0 {
        return (vs, va);
    }
    let at = b.iconst(Ty::I64, u128::from(prefix.tail_at));
    let offset = round_up(b, at, va);
    let end = b.binary(Op::Add, Flags::NONE, offset, vs);
    let sized = b.iconst(Ty::I64, 1u128 << prefix.align);
    let align = b.binary(Op::Umax, Flags::NONE, sized, va);
    (round_up(b, end, align), align)
}

/// The size (an `i64`) of a value ending in a slice or `str` of `len` (an `i64`) elements
/// `stride` bytes apart, its tail at `tail_at` (the row's unrounded `size`, layout.rs
/// `univariant`), the whole aligned to `1 << align` (the element's alignment included): rustc's
/// `size_and_align_of_dst`, `round_up(tail_at + len * stride, align)`, the run-time form of
/// `Layouts::size_of_val`. Here only `len` is a run-time value, so the rounding is the same one
/// the `dyn` tail takes with the vtable's alignment.
pub(crate) fn slice_size<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, tail_at: u64, stride: u64, align: u8, len: Value) -> Value {
    let stride = b.iconst(Ty::I64, u128::from(stride));
    let tail = b.binary(Op::Mul, Flags::NONE, len, stride);
    let at = b.iconst(Ty::I64, u128::from(tail_at));
    let end = b.binary(Op::Add, Flags::NONE, at, tail);
    let align = b.iconst(Ty::I64, 1u128 << align);
    round_up(b, end, align)
}
