//! The exact validity judgement (exact.rs). The Rust Reference, "Behavior considered
//! undefined": "A discriminant in an `enum` not included in the type definition"; "A reference
//! or `Box<T>` that is dangling, misaligned, or points to an invalid value"; "A `!`"; "Invalid
//! values for a type with a custom definition of invalid values ... `NonNull<T>` and `NonZero*`".
//! The pinned core, library/core/src/intrinsics/mod.rs:753-782 (the assertions "should only be
//! called if an assertion failure will imply language UB"), library/core/src/mem/mod.rs:801-805
//! (`mem::uninitialized` fills 0x01).

use blaze_typeck::{AdtKind, List, PNode, UintTy};

use super::super::Check;
use super::permits;
use crate::layout::tests::{F, G, World};
use crate::layout::{IntRepr, LayoutCx, Layouts, ReprAttrs};

/// Expected line: "validity exact: 16 decided". Each case is (check, type, permitted):
/// - zero: `Option<&u8>` passes (its niche value 0 is `None`, which has no fields);
///   `Option<NonZero<u8 is 1..=255>>` passes (the same, the niche in the pattern type);
///   `enum E12 { A = 1, B = 2 }` fails (the tag 0 is no discriminant); `enum One<T> { A(T) }` at
///   `&u8` fails (its one variant's field is a reference, never null); `struct S { b: bool }`
///   passes (`false` is 0); `[&u8; 1]` fails by its element;
/// - THE CONTROL: `#[repr(u8)] enum Tagged<T> { A(T), B }` at `&u8` fails: the zero tag is a
///   valid tag (it selects `A`), so the tag-only rule (walkAJ-P8-1) permits it, but `A`'s field
///   would be a null reference; `#[repr(u8)] enum Later<T> { B, A(T) }` at `&u8` passes (the
///   zero tag selects the field-less `B`);
/// - 0x01: `One<&u16>` fails (the tag-only rule, which never looks inside an enum, permits it;
///   the address 0x0101..01 is misaligned for `u16`); `Option<&u8>` fails (0x0101..01 is not the
///   niche value, so the value is `Some` of a dangling reference); `Option<bool>` passes (the
///   byte 0x01 is `Some(true)`);
/// - inhabited: `enum Half { A(!), B(u8) }` is; `enum Void2 { A(!), B(!) }` is not; `(u8, !)`
///   is not; `[!; 0]` is.
#[test]
fn validity_is_exact_for_every_type() {
    let heap = blaze_own::testing::heap();
    let mut w = World::default();
    let option = w.option();
    let nonzero = w.strukt(1, &[F::P(0)], ReprAttrs::RUST);
    let e12 = w.def(AdtKind::Enum, 0, &[&[], &[]], ReprAttrs::RUST);
    w.def_mut(e12).discrs = vec![Some(1), Some(2)];
    let one = w.def(AdtKind::Enum, 1, &[&[F::P(0)]], ReprAttrs::RUST);
    let with_bool = w.strukt(0, &[F::Leaf(PNode::Bool)], ReprAttrs::RUST);
    let repr_u8 = ReprAttrs { int: Some(IntRepr::Unsigned(UintTy::U8)), ..ReprAttrs::RUST };
    let tagged = w.def(AdtKind::Enum, 1, &[&[F::P(0)], &[]], repr_u8);
    let later = w.def(AdtKind::Enum, 1, &[&[], &[F::P(0)]], repr_u8);
    let half = w.def(AdtKind::Enum, 0, &[&[F::Leaf(PNode::Never)], &[F::Leaf(PNode::Uint(UintTy::U8))]], ReprAttrs::RUST);
    let void2 = w.def(AdtKind::Enum, 0, &[&[F::Leaf(PNode::Never)], &[F::Leaf(PNode::Never)]], ReprAttrs::RUST);

    let mut g = G::new();
    let u8_ = g.n(PNode::Uint(UintTy::U8));
    let u16_ = g.n(PNode::Uint(UintTy::U16));
    let bool_ = g.n(PNode::Bool);
    let never = g.n(PNode::Never);
    let ref_u8 = g.r(u8_);
    let ref_u16 = g.r(u16_);
    let opt_ref = g.adt(option, &[ref_u8]);
    let one_bit = g.n(PNode::ConstValue { ty: u8_, bits: 1 });
    let all_bits = g.n(PNode::ConstValue { ty: u8_, bits: 255 });
    let ends = g.t.intern_list(&[one_bit, all_bits]).unwrap();
    let nz_inner = g.n(PNode::Pat(u8_, ends));
    let nz = g.adt(nonzero, &[nz_inner]);
    let opt_nz = g.adt(option, &[nz]);
    let e12_t = g.n(PNode::Adt(e12, List::EMPTY));
    let one_ref = g.adt(one, &[ref_u8]);
    let one_ref16 = g.adt(one, &[ref_u16]);
    let with_bool_t = g.n(PNode::Adt(with_bool, List::EMPTY));
    let refs_1 = g.array(ref_u8, 1);
    let tagged_ref = g.adt(tagged, &[ref_u8]);
    let later_ref = g.adt(later, &[ref_u8]);
    let opt_bool = g.adt(option, &[bool_]);
    let half_t = g.n(PNode::Adt(half, List::EMPTY));
    let void2_t = g.n(PNode::Adt(void2, List::EMPTY));
    let u8_never = g.tuple(&[u8_, never]);
    let nevers_0 = g.array(never, 0);

    let cases = [
        (Check::Zero, opt_ref, true),
        (Check::Zero, opt_nz, true),
        (Check::Zero, e12_t, false),
        (Check::Zero, one_ref, false),
        (Check::Zero, with_bool_t, true),
        (Check::Zero, refs_1, false),
        (Check::Zero, tagged_ref, false),
        (Check::Zero, later_ref, true),
        (Check::Ones, one_ref16, false),
        (Check::Ones, opt_ref, false),
        (Check::Ones, opt_bool, true),
        (Check::Inhabited, half_t, true),
        (Check::Inhabited, void2_t, false),
        (Check::Inhabited, u8_never, false),
        (Check::Inhabited, nevers_0, true),
        (Check::Inhabited, opt_nz, true),
    ];
    let mut ls = Layouts::new_in(heap.alloc());
    let mut n = 0;
    for (check, ty, want) in cases {
        let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: heap.alloc() };
        assert_eq!(permits(check, ty, &mut ls, &mut cx).unwrap(), want, "{check:?} of type {}", ty.0);
        n += 1;
    }
    println!("validity exact: {n} decided");
}

#[test]
fn union_validity_is_exact() {
    let heap = blaze_own::testing::heap();
    let w = World::default();
    let mut g = G::new();
    let u8_ = g.n(PNode::Uint(UintTy::U8));
    let u64_ = g.n(PNode::Uint(UintTy::U64));
    let bool_ = g.n(PNode::Bool);
    let unit = g.tuple(&[]);
    let never = g.n(PNode::Never);
    let ref_u8 = g.r(u8_);
    let ref_u64 = g.r(u64_);
    let pair_never = g.tuple(&[u8_, never]);

    let l_ref_unit = g.t.intern_list(&[ref_u64, unit]).unwrap();
    let u_ref_unit = g.n(PNode::Union(l_ref_unit));
    let l_refs = g.t.intern_list(&[ref_u64, ref_u8]).unwrap();
    let u_refs = g.n(PNode::Union(l_refs));
    let l_bool_unit = g.t.intern_list(&[bool_, unit]).unwrap();
    let u_bool_unit = g.n(PNode::Union(l_bool_unit));
    let l_u64_never = g.t.intern_list(&[u64_, never]).unwrap();
    let u_u64_never = g.n(PNode::Union(l_u64_never));
    let l_never_u64 = g.t.intern_list(&[never, u64_]).unwrap();
    let u_never_u64 = g.n(PNode::Union(l_never_u64));
    let l_never_ref = g.t.intern_list(&[never, ref_u64]).unwrap();
    let u_never_ref = g.n(PNode::Union(l_never_ref));
    let l_uninhab = g.t.intern_list(&[pair_never, never]).unwrap();
    let u_uninhab = g.n(PNode::Union(l_uninhab));
    let u_empty = g.n(PNode::Union(List::EMPTY));

    let cases = [
        (Check::Zero, u_ref_unit, true),
        (Check::Ones, u_ref_unit, false),
        (Check::Zero, u_refs, false),
        (Check::Zero, u_bool_unit, true),
        (Check::Ones, u_bool_unit, true),
        (Check::Zero, u_u64_never, true),
        (Check::Zero, u_never_u64, true),
        (Check::Zero, u_never_ref, false),
        (Check::Zero, u_uninhab, false),
        (Check::Zero, u_empty, false),
        (Check::Inhabited, u_ref_unit, true),
        (Check::Inhabited, u_u64_never, true),
        (Check::Inhabited, u_never_u64, true),
        (Check::Inhabited, u_uninhab, false),
        (Check::Inhabited, u_empty, false),
    ];
    let mut ls = Layouts::new_in(heap.alloc());
    for (check, ty, want) in cases {
        let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: heap.alloc() };
        assert_eq!(permits(check, ty, &mut ls, &mut cx).unwrap(), want, "{check:?} of union {}", ty.0);
    }
}
