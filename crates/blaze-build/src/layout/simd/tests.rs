//! `repr(simd)` layout (layout/simd.rs): a vector is its array `[E; N]` with the target's
//! short-vector alignment, its size (AAPCS64 section 5.9: 8 for 64 bits, 16 for 128 bits);
//! the pinned `stdarch/crates/core_arch/src/macros.rs` `as_array`: "this type is just an
//! overaligned `[T; N]` with potential padding at the end". A C call of one is refused by
//! name (cabi.rs): AAPCS64 passes a short vector in a SIMD register, not built yet.
//!
//! Expected: `test layout::simd::tests::vectors_are_their_array_at_the_vector_alignment ... ok`
//! and `test layout::simd::tests::other_vectors_and_c_calls_of_vectors_are_refused_by_name ... ok`.
//! A fix that keeps the array's own alignment (1 for `u8` lanes) fails the first on `U8x8`; a
//! fix that caps a 32-byte vector's alignment at 16, pads a packed `f32x3`, refuses a 4-byte
//! vector, or classifies a vector as a C composite (or a `f32x4` as an HFA of four
//! registers), fails the second. The size rule is rustc_abi's (layout/simd_rule.rs's doc).

use std::alloc::Global;

use blaze_own::Alloc;
use blaze_typeck::{FloatTy, PNode, UintTy};

use crate::cabi::{CAbiError, class_of};
use crate::layout::tests::{F, G, World, lay};
use crate::layout::{LayoutCx, Layouts, Refusal, ReprAttrs};

const SIMD: ReprAttrs = ReprAttrs { simd: true, ..ReprAttrs::RUST };

#[test]
fn vectors_are_their_array_at_the_vector_alignment() {
    let mut w = World::default();
    // `struct Simd<A>(A)` under `repr(simd)`, applied to an array, as core_arch's
    // `Simd<T, N>([T; N])` is; and `#[repr(C)] struct S<A> { a: u8, v: Simd<A> }`.
    let vector = w.strukt(1, &[F::P(0)], SIMD);
    let holder = w.strukt(1, &[F::Leaf(PNode::Uint(UintTy::U8)), F::AdtP(vector, 0)], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let u8_ = g.n(PNode::Uint(UintTy::U8));
    let u32_ = g.n(PNode::Uint(UintTy::U32));
    let f32_ = g.n(PNode::Float(FloatTy::F32));
    let u64_ = g.n(PNode::Uint(UintTy::U64));
    for (elem, n, size) in [(u8_, 8, 8), (u32_, 4, 16), (f32_, 2, 8), (u64_, 1, 8), (u8_, 16, 16)] {
        let arr = g.array(elem, n);
        let t = g.adt(vector, &[arr]);
        let id = lay(&w, &mut g, &mut ls, t).unwrap();
        let l = *ls.get(id);
        assert_eq!((l.size, l.align_bytes()), (size, size), "a vector of {n} lanes: its size, aligned to it");
        assert_eq!(ls.field_offset(id, 0), Some(0), "the array at offset 0");
        assert!(l.scalar.is_none() && l.pair.is_none() && l.niche.is_none(), "an aggregate with no niche: {l:?}");
    }
    // `uint32x4_t` after a `u8` in a `repr(C)` struct: at 16, the struct 32 bytes.
    let arr = g.array(u32_, 4);
    let t = g.adt(holder, &[arr]);
    let id = lay(&w, &mut g, &mut ls, t).unwrap();
    assert_eq!((ls.field_offset(id, 1), ls.get(id).size, ls.get(id).align_bytes()), (Some(16), 32, 16), "a 16-byte vector field is 16-aligned");
}

#[test]
fn other_vectors_and_c_calls_of_vectors_are_refused_by_name() {
    let mut w = World::default();
    let vector = w.strukt(1, &[F::P(0)], SIMD);
    let packed_vector = w.strukt(1, &[F::P(0)], ReprAttrs { packed: Some(0), ..SIMD });
    let two_fields = w.strukt(0, &[F::Leaf(PNode::Uint(UintTy::U32)), F::Leaf(PNode::Uint(UintTy::U32))], SIMD);
    let mut g = G::new();
    let mut ls = Layouts::new_in(Global);
    let u8_ = g.n(PNode::Uint(UintTy::U8));
    let bool_ = g.n(PNode::Bool);
    let refused = |w: &World, g: &mut G, ls: &mut Layouts<Global>, t, part: &str| {
        let error = lay(w, g, ls, t).expect_err("refused");
        assert!(matches!(error.why, Refusal::Unsupported(what) if what.contains(part)), "{error}");
    };
    // rustc_abi `vector_type_layout` (layout.rs:1500-1517 at the pinned commit): size rounded
    // up to `rust_vector_align(size)` = `next_power_of_two` on aarch64-apple-darwin (no `v`
    // entry in its data layout), uncapped; packed with a non-power-of-two count: the array's
    // size at `max_aligned_factor(size)`.
    let f32_ = g.n(PNode::Float(FloatTy::F32));
    let a4 = g.array(u8_, 4);
    let a32 = g.array(u8_, 32);
    let f3 = g.array(f32_, 3);
    for (adt, arr, shape) in [(vector, a4, (4, 4)), (vector, a32, (32, 32)), (vector, f3, (16, 16)), (packed_vector, f3, (12, 4)), (packed_vector, a32, (32, 32))] {
        let t = g.adt(adt, &[arr]);
        let id = lay(&w, &mut g, &mut ls, t).unwrap();
        let l = *ls.get(id);
        assert_eq!((l.size, l.align_bytes()), shape, "a vector's size and alignment");
    }
    let b8 = g.array(bool_, 8);
    let t = g.adt(vector, &[b8]);
    refused(&w, &mut g, &mut ls, t, "element is not");
    let t = g.adt(two_fields, &[]);
    refused(&w, &mut g, &mut ls, t, "several scalar fields");
    // A C call: `float32x4_t` would be an HFA of four `s` registers and `uint8x8_t` a composite
    // in `x0`; AAPCS64 passes either in ONE SIMD register. Refused, not guessed.
    let f32_ = g.n(PNode::Float(FloatTy::F32));
    for (elem, n) in [(u8_, 8), (f32_, 4)] {
        let arr = g.array(elem, n);
        let t = g.adt(vector, &[arr]);
        let mut cls = Layouts::new_in(Global);
        let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: Alloc::global() };
        let class = class_of(&mut cls, t, &mut cx, Global);
        assert!(matches!(class, Err(CAbiError::Refused { why, .. }) if why.contains("repr(simd)")), "{class:?}");
    }
}
