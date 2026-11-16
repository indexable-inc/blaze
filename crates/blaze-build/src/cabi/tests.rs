//! Each test names the AAPCS64 rule (section 6.8, stage C; 6.9) or Apple arm64 rule it checks.

use std::alloc::Global;

use blaze_own::Alloc;
use blaze_typeck::{AdtKind, FloatTy, IntTy, PNode, PTy, UintTy};

use super::{ArgLoc, CAbiError, Class, Ext, Place, Platform, RetLoc, assign, class_of, ret};
use crate::layout::tests::{F, G, World};
use crate::layout::{LayoutCx, Layouts, ReprAttrs};

const U8: PNode = PNode::Uint(UintTy::U8);
const U32: PNode = PNode::Uint(UintTy::U32);
const U64: PNode = PNode::Uint(UintTy::U64);
const F32: PNode = PNode::Float(FloatTy::F32);
const F64: PNode = PNode::Float(FloatTy::F64);

fn class(w: &World, g: &mut G, ty: PTy) -> Result<Class, CAbiError> {
    let mut ls = Layouts::new_in(Global);
    let mut cx = LayoutCx { ground: &mut g.t, facts: w, scratch: Alloc::global() };
    class_of(&mut ls, ty, &mut cx, Global)
}

fn locs(args: &[Class], variadic_from: Option<usize>, platform: Platform) -> Vec<ArgLoc> {
    let mut out = Vec::new_in(Global);
    assign(args, variadic_from, platform, &mut out).expect("assigns");
    out
}

const fn gpr(first: u8, count: u8) -> ArgLoc {
    ArgLoc::Direct { at: Place::Gpr { first, count }, ext: Ext::None }
}

const fn fpr(first: u8, count: u8) -> ArgLoc {
    ArgLoc::Direct { at: Place::Fpr { first, count }, ext: Ext::None }
}

const fn stack(offset: u32, size: u32) -> ArgLoc {
    ArgLoc::Direct { at: Place::Stack { offset, size }, ext: Ext::None }
}

const I64: Class = Class::Int { size: 8, signed: true };
const D: Class = Class::Float { size: 8 };

#[test]
fn classes_follow_the_fundamental_types_and_homogeneous_aggregates() {
    let mut w = World::default();
    let three_f32 = w.strukt(0, &[F::Leaf(F32), F::Leaf(F32), F::Leaf(F32)], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let mixed = w.strukt(0, &[F::Leaf(F32), F::Leaf(F64)], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let twelve = w.strukt(0, &[F::Leaf(U32), F::Leaf(U32), F::Leaf(U32)], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let newtype = w.strukt(0, &[F::Leaf(U8)], ReprAttrs { transparent: true, ..ReprAttrs::RUST });
    let unit = w.strukt(0, &[], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let union = w.def(AdtKind::Union, 0, &[&[F::Leaf(U32), F::Leaf(F32)]], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let data_enum = w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32)], &[]], ReprAttrs { c: true, ..ReprAttrs::RUST });
    let mut g = G::new();
    let f32_ = g.n(F32);
    let f64_ = g.n(F64);
    let i16_ = g.n(PNode::Int(IntTy::I16));
    let u8_ = g.n(U8);
    let arr4 = g.array(f64_, 4);
    let arr5 = g.array(f64_, 5);
    let slice = g.n(PNode::Slice(u8_));
    let wide = g.r(slice);
    let cases: Vec<(PTy, Class)> = vec![
        (i16_, Class::Int { size: 2, signed: true }),
        (f32_, Class::Float { size: 4 }),
        (g.adt(three_f32, &[]), Class::Hfa { count: 3, size: 4 }),
        // 5.9.5.1: the members must be one float type.
        (g.adt(mixed, &[]), Class::Composite { size: 16, align: 3 }),
        (arr4, Class::Hfa { count: 4, size: 8 }),
        // More than four members is no HFA: a 40-byte composite.
        (arr5, Class::Composite { size: 40, align: 3 }),
        (g.adt(twelve, &[]), Class::Composite { size: 12, align: 2 }),
        // A `repr(transparent)` newtype is its field.
        (g.adt(newtype, &[]), Class::Int { size: 1, signed: false }),
        (g.adt(unit, &[]), Class::Ignore),
        (wide, Class::Composite { size: 16, align: 3 }),
    ];
    for (t, want) in cases {
        assert_eq!(class(&w, &mut g, t).unwrap(), want, "type {t:?}");
    }
    for (t, why) in [(g.adt(union, &[]), "a union"), (g.adt(data_enum, &[]), "an enum with fields"), (slice, "an unsized value")] {
        match class(&w, &mut g, t) {
            Err(CAbiError::Refused { why: w, .. }) => assert_eq!(w, why),
            other => panic!("{t:?}: {other:?}"),
        }
    }
}

/// C.1 and C.9: floats and integers take their own register files, independently.
#[test]
fn integers_and_floats_count_separately() {
    let args = [Class::Int { size: 4, signed: true }, D, I64, Class::Float { size: 4 }];
    assert_eq!(locs(&args, None, Platform::Aapcs), [gpr(0, 1), fpr(0, 1), gpr(1, 1), fpr(1, 1)]);
}

/// C.11: the ninth integer goes on the stack. AAPCS64 rounds each stacked argument to 8 bytes;
/// Apple packs them at their natural size and alignment.
#[test]
fn the_ninth_integer_is_stacked_by_each_platform_rule() {
    let mut args = vec![I64; 8];
    args.extend([Class::Int { size: 1, signed: false }, Class::Int { size: 4, signed: false }]);
    let generic = locs(&args, None, Platform::Aapcs);
    assert_eq!(&generic[..8], &(0..8).map(|i| gpr(i, 1)).collect::<Vec<_>>()[..]);
    assert_eq!(&generic[8..], &[stack(0, 8), stack(8, 8)]);
    let apple = locs(&args, None, Platform::Apple);
    assert_eq!(&apple[8..], &[
        ArgLoc::Direct { at: Place::Stack { offset: 0, size: 1 }, ext: Ext::Zext32 },
        ArgLoc::Direct { at: Place::Stack { offset: 4, size: 4 }, ext: Ext::None },
    ]);
}

/// C.2/C.3: an HFA takes consecutive SIMD registers if all fit; otherwise NSRN becomes 8 and
/// the HFA and every float after it are stacked.
#[test]
fn an_hfa_that_does_not_fit_closes_the_simd_registers() {
    let hfa3 = Class::Hfa { count: 3, size: 4 };
    assert_eq!(locs(&[hfa3, D], None, Platform::Aapcs), [fpr(0, 3), fpr(3, 1)]);
    let mut args = vec![D; 6];
    args.extend([hfa3, D]);
    let l = locs(&args, None, Platform::Aapcs);
    assert_eq!(&l[6..], &[stack(0, 16), stack(16, 8)], "12 bytes rounded to 16, then the double");
}

/// C.8/C.9: a 16-byte-aligned argument starts at an even register; an 8-aligned 16-byte
/// composite does not. C.10: a composite of up to 16 bytes takes its words of registers.
#[test]
fn sixteen_byte_alignment_rounds_to_an_even_register() {
    let i128_ = Class::Int { size: 16, signed: true };
    let pair = Class::Composite { size: 16, align: 3 };
    let twelve = Class::Composite { size: 12, align: 2 };
    assert_eq!(locs(&[I64, i128_], None, Platform::Aapcs), [gpr(0, 1), gpr(2, 2)]);
    assert_eq!(locs(&[I64, pair, twelve], None, Platform::Aapcs), [gpr(0, 1), gpr(1, 2), gpr(3, 2)]);
    // C.11: seven registers taken, a two-word composite does not fit and closes them.
    let mut args = vec![I64; 7];
    args.extend([pair, I64]);
    assert_eq!(&locs(&args, None, Platform::Aapcs)[7..], &[stack(0, 16), stack(16, 8)]);
}

/// B.4: a composite over 16 bytes is passed as a pointer to a caller's copy; 6.9: returned
/// through `x8`.
#[test]
fn large_composites_go_through_memory() {
    let big = Class::Composite { size: 24, align: 3 };
    assert_eq!(locs(&[I64, big], None, Platform::Aapcs), [gpr(0, 1), ArgLoc::Indirect { at: Place::Gpr { first: 1, count: 1 } }]);
    assert_eq!(ret(big), RetLoc::Indirect);
    assert_eq!(ret(Class::Composite { size: 12, align: 2 }), RetLoc::Direct(Place::Gpr { first: 0, count: 2 }));
    assert_eq!(ret(Class::Hfa { count: 4, size: 8 }), RetLoc::Direct(Place::Fpr { first: 0, count: 4 }));
    assert_eq!(ret(Class::Ignore), RetLoc::Ignore);
}

/// Apple: an integer under 32 bits is extended by the caller; every variadic argument goes on
/// the stack in an 8-byte slot (so `printf("%d", 1)`'s 1 is at the stack pointer).
#[test]
fn apple_extends_small_integers_and_stacks_variadic_arguments() {
    let args = [Class::Int { size: 8, signed: false }, Class::Int { size: 2, signed: true }, Class::Int { size: 4, signed: true }, D];
    assert_eq!(locs(&args, None, Platform::Apple), [
        gpr(0, 1),
        ArgLoc::Direct { at: Place::Gpr { first: 1, count: 1 }, ext: Ext::Sext32 },
        gpr(2, 1),
        fpr(0, 1),
    ]);
    assert_eq!(locs(&args, Some(1), Platform::Apple), [
        gpr(0, 1),
        ArgLoc::Direct { at: Place::Stack { offset: 0, size: 8 }, ext: Ext::Sext32 },
        stack(8, 8),
        stack(16, 8),
    ]);
    // AAPCS64 passes variadic arguments as any other.
    assert_eq!(locs(&args, Some(1), Platform::Aapcs), [gpr(0, 1), gpr(1, 1), gpr(2, 1), fpr(0, 1)]);
}
