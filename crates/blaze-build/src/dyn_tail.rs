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
//! `Layout::for_value` (`Arc`'s and `Rc`'s deallocation) reads. A struct whose last field is
//! itself a struct ending in `dyn` (`ArcInner<Mutex<dyn Tr>>`) is a chain of such levels
//! ([`DynChain`]): each level's tail is at its prefix's end rounded up to the tail's own
//! run-time alignment, and the size and alignment fold from the innermost level out, as
//! rustc's `size_and_align_of_dst` recurses. A packed one is refused by name in `ls` (layout.rs
//! `univariant`).

use core::alloc::Allocator;

use blaze_lir::builder::MemAttrs;
use blaze_lir::{Builder, Flags, Op, Sink, Ty, Value};

use crate::layout::{DynChain, DynPrefix};

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

/// `max(1 << level.align, a)`: the alignment of a value of `level` whose tail's is `a`.
fn level_align<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, level: DynPrefix, a: Value) -> Value {
    let sized = b.iconst(Ty::I64, 1u128 << level.align);
    b.binary(Op::Umax, Flags::NONE, sized, a)
}

/// The offset of the outermost level's tail (the next level, or the trait object itself) in
/// the value whose vtable is `vtable` (an `i64`): `round_up(tail_at, align_of_val(tail))`,
/// the tail's alignment the larger of its own level's static one and the vtable's.
pub(crate) fn tail_offset<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, chain: DynChain, vtable: Value) -> Value {
    let levels = chain.levels();
    let mut a = b.load(Ty::I64, Flags::NONE, vtable, ALIGN_WORD, MemAttrs::aligned(3));
    if let Some(&tail) = levels.get(1) {
        a = level_align(b, tail, a);
    }
    let at = b.iconst(Ty::I64, u128::from(levels[0].tail_at));
    round_up(b, at, a)
}

/// The value's size and alignment (both `i64`), by the vtable `vtable`: rustc's
/// `size_and_align_of_dst` level by level from the innermost, each `size = round_up(tail_at +
/// tail_size, align)` (the tail's size a multiple of its alignment, so rounding its offset
/// first gives the same) and `align = max(static, tail_align)`.
pub(crate) fn size_and_align<LA: Allocator + Clone, S: Sink>(b: &mut Builder<LA, S>, chain: DynChain, vtable: Value) -> (Value, Value) {
    let mut size = b.load(Ty::I64, Flags::NONE, vtable, SIZE_WORD, MemAttrs::aligned(3));
    let mut align = b.load(Ty::I64, Flags::NONE, vtable, ALIGN_WORD, MemAttrs::aligned(3));
    for &level in chain.levels().iter().rev() {
        // A bare `dyn Tr`: the empty prefix; the vtable's size is already a multiple of its
        // alignment (the Reference, "Type layout": "the size of a value is always a multiple
        // of its alignment").
        if level.tail_at == 0 && level.align == 0 {
            continue;
        }
        let at = b.iconst(Ty::I64, u128::from(level.tail_at));
        let offset = round_up(b, at, align);
        let end = b.binary(Op::Add, Flags::NONE, offset, size);
        align = level_align(b, level, align);
        size = round_up(b, end, align);
    }
    (size, align)
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
