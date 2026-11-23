//! Each intrinsic's meaning (the table's quote) checked on what it builds: run by blaze-lir's
//! reference evaluator where it models the operations, and read back from the function where
//! it does not (memory intrinsics, atomics, flags, data).

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv, Func, SlotKind};
use blaze_lir::{Builder, Flags, Full, Op, Ordering, Ty, Value};
use blaze_own::Alloc;
use blaze_typeck::{AdtKind, IntTy, PNode, PTy, UintTy};

use super::{Arg, Call, GenericArg, IntrinsicCx, IntrinsicError, Out, Site, TABLE, Which, build};
use crate::layout::tests::{F, G, World};
use crate::layout::{IntRepr, LayoutCx, Layouts, ReprAttrs};

struct Env {
    w: World,
    g: G,
    ls: Layouts<Global>,
}

impl Env {
    fn new() -> Env {
        Env { w: World::default(), g: G::new(), ls: Layouts::new_in(Global) }
    }

    fn call(&mut self, b: &mut Builder<Global, Full>, c: &Call<'_>) -> Result<Out, IntrinsicError> {
        let mut cx = LayoutCx { ground: &mut self.g.t, facts: &self.w, scratch: Alloc::global() };
        let needs_drop = |t: PTy| t.0 % 2 == 1;
        let mut icx = IntrinsicCx { layouts: &mut self.ls, cx: &mut cx, needs_drop: &needs_drop, ub_checks: crate::UbChecks::Off, overflow_checks: crate::OverflowChecks::Off };
        build(b, c, &mut icx)
    }
}

/// A C function of `params` returning `rets`, its body `body` over the parameters.
fn func(params: &[Ty], rets: &[Ty], body: impl FnOnce(&mut Builder<Global, Full>, &[Value]) -> Vec<Value>) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let ps: Vec<Param> = params.iter().map(|&t| Param::of(t)).collect();
    let rs: Vec<Param> = rets.iter().map(|&t| Param::of(t)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &ps, &rs);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let args: Vec<Value> = params.iter().map(|&t| b.param(t)).collect();
    let out = body(&mut b, &args);
    if !b.is_ended() {
        b.ret(&out);
    }
    b.finish_checked().expect("the built function verifies")
}

fn eval(f: &Func<Global>, args: &[u128]) -> Vec<u128> {
    let args: Vec<Val> = args.iter().map(|&a| Val::Int(a)).collect();
    match run_in(f.view(), &args, 10_000, Global).expect("the evaluator models it") {
        Outcome::Returned(v) => v
            .iter()
            .map(|v| match v {
                Val::Int(i) => *i,
                other => panic!("a non-integer result {other:?}"),
            })
            .collect(),
        other => panic!("the run did not return: {other:?}"),
    }
}

fn ops(f: &Func<Global>) -> Vec<(Op, Flags)> {
    let v = f.view();
    (0..v.len()).map(|i| (v.op(i), v.flags(i))).collect()
}

fn scalar(o: Out) -> Value {
    match o {
        Out::Scalar(v) => v,
        other => panic!("not a scalar: {other:?}"),
    }
}

const U8: PNode = PNode::Uint(UintTy::U8);
const U16: PNode = PNode::Uint(UintTy::U16);
const U32: PNode = PNode::Uint(UintTy::U32);
const U64: PNode = PNode::Uint(UintTy::U64);
const I8: PNode = PNode::Int(IntTy::I8);
const I32: PNode = PNode::Int(IntTy::I32);
const ISIZE: PNode = PNode::Int(IntTy::Isize);
const USIZE: PNode = PNode::Uint(UintTy::Usize);

fn lir_of(n: PNode) -> Ty {
    match n {
        U8 | I8 | PNode::Bool => Ty::I8,
        U16 => Ty::I16,
        U32 | I32 => Ty::I32,
        _ => Ty::I64,
    }
}

#[test]
fn the_table_is_sorted_whole_and_refuses_what_it_lacks() {
    for w in TABLE.windows(2) {
        assert!(w[0].name < w[1].name, "{} before {}", w[0].name, w[1].name);
    }
    for e in TABLE {
        assert_eq!(Which::named(e.name), Ok(e.which));
        assert_eq!(e.which.entry().name, e.name, "one row per intrinsic");
        assert!(!e.meaning.is_empty() && !e.built.is_empty());
    }
    // `simd_add` and `simd_shuffle` are intrinsics/simd.rs's rows now; `simd_funnel_shl` is in no table.
    let refused = Which::named("simd_funnel_shl").unwrap_err();
    assert_eq!(refused.to_string(), "intrinsic `simd_funnel_shl` is not in blaze's intrinsics table (crates/blaze-build/src/intrinsics.rs)");
}

/// Two-operand integer intrinsics, each case `(intrinsic, T, x, y, results)`, run.
#[test]
fn arithmetic_and_bit_intrinsics_compute_their_quoted_meaning() {
    let cases: &[(Which, PNode, u128, u128, &[u128])] = &[
        // "Wrapping (modular) addition."
        (Which::WrappingAdd, U8, 250, 10, &[4]),
        (Which::WrappingSub, U8, 3, 5, &[254]),
        (Which::WrappingMul, I32, 0x8000_0000, 2, &[0]),
        // "Computes `a + b`, saturating at numeric bounds."
        (Which::SaturatingAdd, U8, 250, 10, &[255]),
        (Which::SaturatingAdd, I8, 100, 100, &[127]),
        (Which::SaturatingSub, I8, 0x80, 1, &[0x80]),
        (Which::SaturatingSub, U8, 3, 5, &[0]),
        // "Performs checked integer addition": the wrapped value and whether it overflowed.
        (Which::AddWithOverflow, U8, 250, 10, &[4, 1]),
        (Which::AddWithOverflow, I8, 100, 27, &[127, 0]),
        (Which::AddWithOverflow, I8, 100, 28, &[0x80, 1]),
        (Which::SubWithOverflow, U32, 1, 2, &[0xffff_ffff, 1]),
        (Which::MulWithOverflow, I32, 0x10000, 0x8000, &[0x8000_0000, 1]),
        (Which::MulWithOverflow, U64, 1 << 32, 1 << 31, &[1 << 63, 0]),
        (Which::UncheckedDiv, I32, 0xffff_fff9, 2, &[0xffff_fffd]),
        (Which::UncheckedRem, U32, 7, 3, &[1]),
        // "Performs rotate left."
        (Which::RotateLeft, U8, 0x81, 1, &[0x03]),
        (Which::RotateRight, U16, 0x0001, 4, &[0x1000]),
    ];
    for &(which, t, x, y, want) in cases {
        let mut env = Env::new();
        let tp = env.g.n(t);
        let ty = lir_of(t);
        let amount_ty = if matches!(which, Which::RotateLeft | Which::RotateRight) { Ty::I32 } else { ty };
        let rets: &[Ty] = if want.len() == 2 { &[ty, Ty::I8] } else { &[ty] };
        let f = func(&[ty, amount_ty], rets, |b, a| {
            let c = Call { which, generics: &[GenericArg::Ty(tp)], args: &[Arg::Scalar(a[0]), Arg::Scalar(a[1])], ret: tp, site: None };
            match env.call(b, &c).expect("builds") {
                Out::Scalar(v) => vec![v],
                Out::Pair(v, o) => vec![v, o],
                other => panic!("{other:?}"),
            }
        });
        assert_eq!(eval(&f, &[x, y]), want, "{which:?}::<{t:?}>({x:#x}, {y:#x})");
    }
}

#[test]
fn counting_and_reversing_bits() {
    let cases: &[(Which, PNode, u128, u128)] = &[
        // "Returns the number of bits set in an integer type `T`"
        (Which::Ctpop, U64, 0xf0f0, 8),
        // "the number of leading unset bits (zeroes)": counted at `T`'s width, as a `u32`.
        (Which::Ctlz, U16, 1, 15),
        (Which::Ctlz, U8, 0, 8),
        (Which::Cttz, U32, 8, 3),
        (Which::Cttz, U64, 0, 64),
        // "Reverses the bytes in an integer type `T`."
        (Which::Bswap, U32, 0x1122_3344, 0x4433_2211),
        (Which::Bswap, U8, 0xab, 0xab),
        // "Reverses the bits in an integer type `T`."
        (Which::Bitreverse, U8, 1, 0x80),
    ];
    for &(which, t, x, want) in cases {
        let mut env = Env::new();
        let tp = env.g.n(t);
        let ty = lir_of(t);
        let counts = matches!(which, Which::Ctpop | Which::Ctlz | Which::Cttz);
        let f = func(&[ty], &[if counts { Ty::I32 } else { ty }], |b, a| {
            let c = Call { which, generics: &[GenericArg::Ty(tp)], args: &[Arg::Scalar(a[0])], ret: tp, site: None };
            vec![scalar(env.call(b, &c).expect("builds"))]
        });
        assert_eq!(eval(&f, &[x]), [want], "{which:?}::<{t:?}>({x:#x})");
    }
}

/// The promises an `unchecked_*` intrinsic may make are made as flags, the rest computed as
/// the wrapping operation would.
#[test]
fn unchecked_intrinsics_carry_their_promise() {
    for (which, t, op, flags) in [
        (Which::UncheckedAdd, I32, Op::Add, Flags::NSW),
        (Which::UncheckedAdd, U32, Op::Add, Flags::NUW),
        (Which::UncheckedSub, U8, Op::Sub, Flags::NUW),
        (Which::UncheckedMul, I8, Op::Mul, Flags::NSW),
        (Which::ExactDiv, I32, Op::Sdiv, Flags::EXACT),
        (Which::UncheckedShl, U32, Op::Shl, Flags::IN_RANGE),
        (Which::UncheckedShr, I32, Op::Ashr, Flags::IN_RANGE),
        (Which::UncheckedShr, U32, Op::Lshr, Flags::IN_RANGE),
        (Which::CtlzNonzero, U32, Op::Clz, Flags::NONZERO),
        (Which::CttzNonzero, U64, Op::Ctz, Flags::NONZERO),
    ] {
        let mut env = Env::new();
        let tp = env.g.n(t);
        let ty = lir_of(t);
        let one = matches!(which, Which::CtlzNonzero | Which::CttzNonzero);
        let params: &[Ty] = if one { &[ty] } else { &[ty, ty] };
        let f = func(params, &[if one { Ty::I32 } else { ty }], |b, a| {
            let args: Vec<Arg> = a.iter().map(|&v| Arg::Scalar(v)).collect();
            let c = Call { which, generics: &[GenericArg::Ty(tp), GenericArg::Ty(tp)], args: &args, ret: tp, site: None };
            vec![scalar(env.call(b, &c).expect("builds"))]
        });
        assert!(ops(&f).contains(&(op, flags)), "{which:?}: {:?}", ops(&f));
    }
}

#[test]
fn type_queries_are_the_layout() {
    let mut env = Env::new();
    let (u8_, u32_, u16_) = (env.g.n(U8), env.g.n(U32), env.g.n(U16));
    let pair = env.g.tuple(&[u8_, u32_]);
    let slice = env.g.n(PNode::Slice(u16_));
    let usize_ = env.g.n(USIZE);
    // "The size of a type in bytes." / "The minimum alignment of a type." / drop facts.
    let f = func(&[], &[Ty::I64, Ty::I64, Ty::I8, Ty::I8], |b, _| {
        let mut q = |which, t| scalar(env.call(b, &Call { which, generics: &[GenericArg::Ty(t)], args: &[], ret: usize_, site: None }).unwrap());
        vec![q(Which::SizeOf, pair), q(Which::AlignOf, pair), q(Which::NeedsDrop, PTy(1)), q(Which::NeedsDrop, PTy(2))]
    });
    assert_eq!(eval(&f, &[]), [8, 4, 1, 0]);
    // "The size of the referenced value in bytes.": a `&[u16]` of 5 is 10 bytes.
    let f = func(&[Ty::I64], &[Ty::I64, Ty::I64], |b, a| {
        let slot = b.slot(16, 3, SlotKind::Temp);
        let at = b.slot_addr(slot);
        b.store(Flags::NONE, a[0], at, 8, MemAttrs::aligned(3));
        let mut q = |which| scalar(env.call(b, &Call { which, generics: &[GenericArg::Ty(slice)], args: &[Arg::Place(at)], ret: usize_, site: None }).unwrap());
        vec![q(Which::SizeOfVal), q(Which::AlignOfVal)]
    });
    assert_eq!(eval(&f, &[5]), [10, 2]);
    assert_eq!(eval(&f, &[0]), [0, 2]);
}

/// `size_of_val` / `align_of_val` of `&S<dyn Tr>`, `struct S<T: ?Sized> { a: u64, b: u8, t: T }`
/// (`ArcInner<dyn Tr>`'s shape), through a vtable of size `vs` and alignment `va` (words 1 and
/// 2): rustc's `size_and_align_of_dst`, the Reference's "Type layout": the trait object at the
/// prefix's end (9) rounded up to `va`, the alignment the larger of 8 and `va`, the size its end
/// rounded up to that. A bare `&dyn Tr` answers the vtable's words. A version that took the
/// prefix's size rounded to its own alignment (16) as the tail's offset gives 24, not 16, for
/// `(4, 4)`.
#[test]
fn values_ending_in_a_trait_object_measure_by_their_vtable() {
    let mut env = Env::new();
    let s = env.w.strukt(1, &[F::Leaf(U64), F::Leaf(U8), F::P(0)], ReprAttrs::RUST);
    let principal = env.g.n(PNode::ExTrait(blaze_typeck::ItemKey(0x77), blaze_typeck::List::EMPTY));
    let l = env.g.t.intern_list(&[principal]).unwrap();
    let dyn_tr = env.g.n(PNode::Dynamic(l));
    let s_dyn = env.g.adt(s, &[dyn_tr]);
    let usize_ = env.g.n(USIZE);
    for (t, cases) in [
        (s_dyn, [(4u128, 4u128, [16u128, 8u128]), (1, 1, [16, 8]), (24, 8, [40, 8]), (16, 16, [32, 16])]),
        (dyn_tr, [(4, 4, [4, 4]), (1, 1, [1, 1]), (24, 8, [24, 8]), (16, 16, [16, 16])]),
    ] {
        let f = func(&[Ty::I64, Ty::I64], &[Ty::I64, Ty::I64], |b, a| {
            let vt_slot = b.slot(24, 3, SlotKind::Temp);
            let vt = b.slot_addr(vt_slot);
            b.store(Flags::NONE, a[0], vt, 8, MemAttrs::aligned(3));
            b.store(Flags::NONE, a[1], vt, 16, MemAttrs::aligned(3));
            let wide_slot = b.slot(16, 3, SlotKind::Temp);
            let wide = b.slot_addr(wide_slot);
            b.store(Flags::NONE, vt, wide, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, vt, wide, 8, MemAttrs::aligned(3));
            let mut q = |which| scalar(env.call(b, &Call { which, generics: &[GenericArg::Ty(t)], args: &[Arg::Place(wide)], ret: usize_, site: None }).unwrap());
            vec![q(Which::SizeOfVal), q(Which::AlignOfVal)]
        });
        for (vs, va, want) in cases {
            assert_eq!(eval(&f, &[vs, va]), want, "a vtable of size {vs} and alignment {va}");
        }
    }
}

/// `size_of_val` / `align_of_val` of `&T` for a `T` ending in a slice: rustc's
/// `size_and_align_of_dst`, `round_up(tail_at + len * stride, align)`, rounded once from the
/// tail's own offset. `P { a: u64, b: u8, t: [u8] }` (the tail at 9, aligned to 8) of lengths 0,
/// 3, 7, 8 is 16, 16, 16, 24; `Q { a: u8, t: [u32] }` (the tail at 4, aligned to 4) of length 2
/// is 12; a bare `[u16]` of 5 is 10. A version that took the prefix rounded to its alignment
/// (16) as the tail's start gives 24 for `P` of length 3. Prints nothing; a failure names the
/// case ("P of length 3").
#[test]
fn values_ending_in_a_slice_round_once_from_the_tails_offset() {
    let mut env = Env::new();
    let s3 = env.w.strukt(1, &[F::Leaf(U64), F::Leaf(U8), F::P(0)], ReprAttrs::RUST);
    let s2 = env.w.strukt(1, &[F::Leaf(U8), F::P(0)], ReprAttrs::RUST);
    let (u8_, u16_, u32_) = (env.g.n(U8), env.g.n(U16), env.g.n(U32));
    let (bytes, halves, words) = (env.g.n(PNode::Slice(u8_)), env.g.n(PNode::Slice(u16_)), env.g.n(PNode::Slice(u32_)));
    let p = env.g.adt(s3, &[bytes]);
    let q = env.g.adt(s2, &[words]);
    let usize_ = env.g.n(USIZE);
    let cases: [(&str, PTy, &[(u128, u128)], u128); 3] =
        [("P", p, &[(0, 16), (3, 16), (7, 16), (8, 24)], 8), ("Q", q, &[(2, 12)], 4), ("[u16]", halves, &[(5, 10)], 2)];
    for (name, t, lens, align) in cases {
        let f = func(&[Ty::I64], &[Ty::I64, Ty::I64], |b, a| {
            let slot = b.slot(16, 3, SlotKind::Temp);
            let at = b.slot_addr(slot);
            b.store(Flags::NONE, a[0], at, 8, MemAttrs::aligned(3));
            let mut ask = |which| scalar(env.call(b, &Call { which, generics: &[GenericArg::Ty(t)], args: &[Arg::Place(at)], ret: usize_, site: None }).unwrap());
            vec![ask(Which::SizeOfVal), ask(Which::AlignOfVal)]
        });
        for &(len, want) in lens {
            assert_eq!(eval(&f, &[len]), [want, align], "{name} of length {len}");
        }
    }
}

/// `write_via_move` then `read_via_copy` of a `u32`, `arith_offset` between, and a transmute of
/// the bytes in memory to the `u32` they hold.
#[test]
fn reads_writes_offsets_and_transmutes_go_through_memory() {
    let mut env = Env::new();
    let u32_ = env.g.n(U32);
    let p = env.g.n(PNode::Ptr(blaze_typeck::Mutbl::Not, u32_));
    let u8_ = env.g.n(U8);
    let arr = env.g.array(u8_, 4);
    let unit = env.g.tuple(&[]);
    let f = func(&[Ty::I32, Ty::I64], &[Ty::I32, Ty::I32], |b, a| {
        let slot = b.slot(16, 2, SlotKind::Temp);
        let base = b.slot_addr(slot);
        // "Calculates the offset from a pointer, potentially wrapping.": element 2, byte 8.
        // `arith_offset<T>(dst: *const T, offset: isize)`: its one generic is the element `u32`.
        let c = Call { which: Which::ArithOffset, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(base), Arg::Scalar(a[1])], ret: p, site: None };
        let at = scalar(env.call(b, &c).unwrap());
        let c = Call { which: Which::WriteViaMove, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(at), Arg::Scalar(a[0])], ret: unit, site: None };
        assert_eq!(env.call(b, &c).unwrap(), Out::None);
        let c = Call { which: Which::ReadViaCopy, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(at)], ret: u32_, site: None };
        let read = scalar(env.call(b, &c).unwrap());
        // "Reinterprets the bits of a value of one type as another type.": [u8; 4] to u32.
        let eight = b.iconst(Ty::I64, 8);
        let byte8 = b.ptr_add(Flags::NONE, base, eight);
        let c = Call { which: Which::Transmute, generics: &[GenericArg::Ty(arr), GenericArg::Ty(u32_)], args: &[Arg::Place(byte8)], ret: u32_, site: None };
        let bits = scalar(env.call(b, &c).unwrap());
        vec![read, bits]
    });
    assert_eq!(eval(&f, &[0xdead_beef, 2]), [0xdead_beef, 0xdead_beef]);
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let u16_ = env.g.n(U16);
    let v = b.iconst(Ty::I32, 0);
    let c = Call { which: Which::Transmute, generics: &[GenericArg::Ty(u32_), GenericArg::Ty(u16_)], args: &[Arg::Scalar(v)], ret: u16_, site: None };
    let error = env.call(&mut b, &c).unwrap_err();
    assert_eq!(error.to_string(), "intrinsic `transmute`: a transmute between types of different sizes (rustc's E0512)");
}

/// `copy`, `copy_nonoverlapping` and `write_bytes` are bulk operations of `count * size`
/// bytes; `offset` promises in-bounds; `ptr_offset_from` divides exactly.
#[test]
fn memory_and_pointer_intrinsics_build_their_operations() {
    let mut env = Env::new();
    let u32_ = env.g.n(U32);
    let p = env.g.n(PNode::Ptr(blaze_typeck::Mutbl::Not, u32_));
    let isize_ = env.g.n(ISIZE);
    let unit = env.g.tuple(&[]);
    for (which, op, flags) in [
        (Which::Copy, Op::Memmove, Flags::NONE),
        (Which::CopyNonoverlapping, Op::Memcpy, Flags::NONE),
        (Which::WriteBytes, Op::Memset, Flags::NONE),
        (Which::VolatileCopyMemory, Op::Memmove, Flags::VOLATILE),
        (Which::VolatileCopyNonoverlappingMemory, Op::Memcpy, Flags::VOLATILE),
        (Which::VolatileSetMemory, Op::Memset, Flags::VOLATILE),
    ] {
        let set = matches!(which, Which::WriteBytes | Which::VolatileSetMemory);
        let f = func(&[Ty::PTR, if set { Ty::I8 } else { Ty::PTR }, Ty::I64], &[], |b, a| {
            let c = Call { which, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(a[0]), Arg::Scalar(a[1]), Arg::Scalar(a[2])], ret: unit, site: None };
            assert_eq!(env.call(b, &c).unwrap(), Out::None);
            vec![]
        });
        let ops = ops(&f);
        assert!(ops.contains(&(Op::Mul, Flags::NONE)) && ops.contains(&(op, flags)), "{which:?}: {ops:?}");
    }
    let f = func(&[Ty::PTR, Ty::I64], &[Ty::PTR], |b, a| {
        let c = Call { which: Which::Offset, generics: &[GenericArg::Ty(p), GenericArg::Ty(isize_)], args: &[Arg::Scalar(a[0]), Arg::Scalar(a[1])], ret: p, site: None };
        vec![scalar(env.call(b, &c).unwrap())]
    });
    assert!(ops(&f).contains(&(Op::PtrAdd, Flags::INBOUNDS)));
    let f = func(&[Ty::PTR, Ty::PTR], &[Ty::I64], |b, a| {
        let c = Call { which: Which::PtrOffsetFrom, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(a[0]), Arg::Scalar(a[1])], ret: isize_, site: None };
        vec![scalar(env.call(b, &c).unwrap())]
    });
    assert!(ops(&f).contains(&(Op::Sdiv, Flags::EXACT)));
}

/// "Returns the value of the discriminant for the variant in 'v'; if `T` has no discriminant,
/// returns `0`.": a direct tag (`repr(i8)` with -1), a niche (`Option<bool>`, `None` = 2), and
/// a struct.
#[test]
fn discriminant_value_decodes_each_encoding() {
    let mut env = Env::new();
    let option = env.w.option();
    let ordering = env.w.def(AdtKind::Enum, 0, &[&[], &[], &[]], ReprAttrs { int: Some(IntRepr::Signed(IntTy::I8)), ..ReprAttrs::RUST });
    env.w.def_mut(ordering).discrs = vec![Some(-1), Some(0), Some(1)];
    let strukt = env.w.strukt(0, &[F::Leaf(U32)], ReprAttrs::RUST);
    let bool_ = env.g.n(PNode::Bool);
    let opt_bool = env.g.adt(option, &[bool_]);
    let ord = env.g.adt(ordering, &[]);
    let s = env.g.adt(strukt, &[]);
    let isize_ = env.g.n(ISIZE);
    let i8_ = env.g.n(I8);
    for (t, ret, byte, want) in [
        (opt_bool, isize_, 2u128, 0u128),
        (opt_bool, isize_, 0, 1),
        (opt_bool, isize_, 1, 1),
        (ord, i8_, 0xff, 0xff),
        (ord, i8_, 1, 1),
        (s, i8_, 7, 0),
    ] {
        let rty = if ret == isize_ { Ty::I64 } else { Ty::I8 };
        let f = func(&[Ty::I8], &[rty], |b, a| {
            let slot = b.slot(8, 3, SlotKind::Temp);
            let at = b.slot_addr(slot);
            b.store(Flags::NONE, a[0], at, 0, MemAttrs::aligned(0));
            let c = Call { which: Which::DiscriminantValue, generics: &[GenericArg::Ty(t)], args: &[Arg::Scalar(at)], ret, site: None };
            vec![scalar(env.call(b, &c).unwrap())]
        });
        assert_eq!(eval(&f, &[byte]), [want], "{t:?} holding {byte}");
    }
}

/// Each atomic is the LIR operation with the ordering asked; orderings an operation cannot
/// have are refused by name.
#[test]
fn atomics_carry_their_orderings() {
    let mut env = Env::new();
    let u64_ = env.g.n(U64);
    let unit = env.g.tuple(&[]);
    let seq = GenericArg::Const(4);
    let f = func(&[Ty::PTR, Ty::I64, Ty::I64], &[Ty::I64, Ty::I64, Ty::I8, Ty::I64], |b, a| {
        let (p, x, y) = (Arg::Scalar(a[0]), Arg::Scalar(a[1]), Arg::Scalar(a[2]));
        let c = Call { which: Which::AtomicXadd, generics: &[GenericArg::Ty(u64_), GenericArg::Ty(u64_), seq], args: &[p, x], ret: u64_, site: None };
        let old = scalar(env.call(b, &c).unwrap());
        let c = Call { which: Which::AtomicUmax, generics: &[GenericArg::Ty(u64_), GenericArg::Const(0)], args: &[p, x], ret: u64_, site: None };
        let max = scalar(env.call(b, &c).unwrap());
        // AcqRel on success, Acquire on failure.
        let c = Call { which: Which::AtomicCxchgWeak, generics: &[GenericArg::Ty(u64_), GenericArg::Const(3), GenericArg::Const(2)], args: &[p, x, y], ret: u64_, site: None };
        let Out::Pair(seen, done) = env.call(b, &c).unwrap() else { panic!("a pair") };
        let c = Call { which: Which::AtomicLoad, generics: &[GenericArg::Ty(u64_), GenericArg::Const(2), GenericArg::Const(1)], args: &[p], ret: u64_, site: None };
        let loaded = scalar(env.call(b, &c).unwrap());
        let c = Call { which: Which::AtomicStore, generics: &[GenericArg::Ty(u64_), GenericArg::Const(1), GenericArg::Const(0)], args: &[p, y], ret: unit, site: None };
        env.call(b, &c).unwrap();
        let c = Call { which: Which::AtomicFence, generics: &[GenericArg::Const(2)], args: &[], ret: unit, site: None };
        env.call(b, &c).unwrap();
        let c = Call { which: Which::AtomicSingleThreadFence, generics: &[seq], args: &[], ret: unit, site: None };
        env.call(b, &c).unwrap();
        vec![old, max, done, if seen == loaded { loaded } else { seen }]
    });
    let v = f.view();
    let mut seen = Vec::new();
    for i in 0..v.len() {
        let (op, flags) = (v.op(i), v.flags(i));
        match op {
            Op::AtomicRmw | Op::AtomicLoad | Op::AtomicStore | Op::Fence | Op::Cmpxchg => {
                seen.push((op, flags.ordering(), (op == Op::Cmpxchg).then(|| flags.failure()).flatten(), flags.without(Flags::NONE)))
            }
            _ => {}
        }
    }
    let ord = |o| Some(o);
    assert_eq!(seen[0].1, ord(Ordering::SeqCst));
    assert_eq!(seen[1].1, ord(Ordering::Relaxed));
    assert_eq!((seen[2].0, seen[2].1, seen[2].2), (Op::Cmpxchg, ord(Ordering::AcqRel), ord(Ordering::Acquire)));
    assert!(seen[2].3.contains(Flags::WEAK));
    assert_eq!((seen[3].0, seen[3].1), (Op::AtomicLoad, ord(Ordering::Acquire)));
    assert!(seen[3].3.contains(Flags::VOLATILE), "VOLATILE = true");
    assert_eq!((seen[4].0, seen[4].1), (Op::AtomicStore, ord(Ordering::Release)));
    assert_eq!((seen[5].0, seen[5].1), (Op::Fence, ord(Ordering::Acquire)));
    assert!(seen[6].3.contains(Flags::SINGLE_THREAD));
    // Refused by name.
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let p = b.null();
    let c = Call { which: Which::AtomicLoad, generics: &[GenericArg::Ty(u64_), GenericArg::Const(1), GenericArg::Const(0)], args: &[Arg::Scalar(p)], ret: u64_, site: None };
    assert_eq!(env.call(&mut b, &c).unwrap_err().to_string(), "intrinsic `atomic_load`: an atomic load with a release ordering");
    let c = Call { which: Which::AtomicFence, generics: &[GenericArg::Const(0)], args: &[], ret: unit, site: None };
    assert_eq!(env.call(&mut b, &c).unwrap_err().to_string(), "intrinsic `atomic_fence`: a relaxed fence (core panics on one)");
}

/// "Gets a reference to a static `Location` indicating where it was called.": the site is
/// item-relative (`CallerSite`: file, item, offset), and the row's line and column are source
/// positions placed with the program (rule 71). The function holds two rows: the path and a
/// NUL, and the `Location`, whose filename is an address of the path's row and its length, and
/// whose `line` and `col` are one source position each of (item 9, byte 30), their bytes zero.
/// A builder that wrote an absolute line fails the zero-bytes assert; one that dropped a
/// position, the positions assert. A malformed `Location` is named as such. Passes silently.
#[test]
fn caller_location_is_a_row_of_source_positions() {
    let mut env = Env::new();
    // `Location<'a> { filename: NonNull<str>, line: u32, col: u32, .. }`, the pointer modelled
    // as `*const str`.
    let loc = env.w.strukt(1, &[F::RawP(0), F::Leaf(U32), F::Leaf(U32)], ReprAttrs::RUST);
    let str_ = env.g.n(PNode::Str);
    let loc_ty = env.g.adt(loc, &[str_]);
    let r = env.g.r(loc_ty);
    let site = crate::CallerSite { file: "src/main.rs", item: blaze_typeck::ItemKey(9), offset: 30 };
    let f = func(&[], &[Ty::PTR], |b, _| {
        let c = Call { which: Which::CallerLocation, generics: &[], args: &[], ret: r, site: Some((Site::Here(site), loc_ty)) };
        match env.call(b, &c) {
            Ok(Out::Scalar(at)) => vec![at],
            Ok(_) => panic!("a `Location`'s address is one pointer"),
            Err(e) => panic!("{e}"),
        }
    });
    let view = f.view();
    let text = blaze_lir::print(view);
    use blaze_lir::func::DataId;
    assert_eq!(view.datas.len(), 2, "the path's row and the `Location`'s: {text}");
    assert_eq!(view.data_bytes_of(DataId(0)), b"src/main.rs\0", "{text}");
    assert!(view.data_positions_of(DataId(0)).is_empty(), "{text}");
    let relocs = view.data_relocs_of(DataId(1));
    assert_eq!(relocs.len(), 1, "the filename's address: {text}");
    let row = view.data_bytes_of(DataId(1));
    let f_at = relocs[0].offset as usize;
    assert_eq!(&row[f_at + 8..f_at + 16], &11u64.to_le_bytes(), "the path's length after its address: {text}");
    let positions = view.data_positions_of(DataId(1));
    let fields: std::vec::Vec<_> = positions.iter().map(|p| (p.item_key(), p.site, blaze_lir::PositionField::from_word(p.field))).collect();
    assert_eq!(fields.len(), 2, "{text}");
    assert!(fields.contains(&(9, 30, Some(blaze_lir::PositionField::Line))) && fields.contains(&(9, 30, Some(blaze_lir::PositionField::Col))), "{text}");
    for p in positions {
        assert_eq!(&row[p.offset as usize..p.offset as usize + 4], &[0; 4], "no absolute line or column in the row: {text}");
    }
    let bad = env.w.strukt(0, &[F::Leaf(U32)], ReprAttrs::RUST);
    let bad_ty = env.g.adt(bad, &[]);
    let bad_r = env.g.r(bad_ty);
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let c = Call { which: Which::CallerLocation, generics: &[], args: &[], ret: bad_r, site: Some((Site::Here(site), bad_ty)) };
    assert_eq!(env.call(&mut b, &c).unwrap_err().to_string(), "intrinsic `caller_location`: `Location` without its fields (filename, line, col)");
}

/// `three_way_compare`: "Does a three-way comparison between the two arguments, which must be
/// of character or integer (signed or unsigned) type." (core/src/intrinsics/mod.rs), its
/// result `cmp::Ordering` (`#[repr(i8)]`: Less = -1, Equal = 0, Greater = 1) as the `i8`
/// scalar. A builder that compares a signed `T` unsigned fails the `-1` vs `1` case (it says
/// Greater); one that compares `u32` signed fails `u32::MAX` vs `1` (it says Less). Passes
/// silently; on a wrong answer it prints e.g. "ThreeWayCompare::<Int(I32)>(0xffffffff, 0x1)".
#[test]
fn three_way_compare_orders_by_the_types_signedness() {
    const LESS: u128 = 0xff;
    let cases: &[(PNode, u128, u128, u128)] = &[
        (I32, 0xffff_ffff, 1, LESS),
        (U32, 0xffff_ffff, 1, 1),
        (PNode::Char, 'a' as u128, 'a' as u128, 0),
        (I8, 0x80, 0x7f, LESS),
        (U8, 3, 3, 0),
        (U64, 2, 7, LESS),
        (ISIZE, 5, 0xffff_ffff_ffff_fffe, 1),
    ];
    for &(t, x, y, want) in cases {
        let mut env = Env::new();
        let tp = env.g.n(t);
        let i8_ = env.g.n(I8);
        let ty = if t == PNode::Char { Ty::I32 } else { lir_of(t) };
        let f = func(&[ty, ty], &[Ty::I8], |b, a| {
            let c = Call { which: Which::ThreeWayCompare, generics: &[GenericArg::Ty(tp)], args: &[Arg::Scalar(a[0]), Arg::Scalar(a[1])], ret: i8_, site: None };
            vec![scalar(env.call(b, &c).expect("builds"))]
        });
        assert_eq!(eval(&f, &[x, y]), [want], "ThreeWayCompare::<{t:?}>({x:#x}, {y:#x})");
    }
    // Not an integer or `char`: refused by name, never guessed.
    let mut env = Env::new();
    let bool_ = env.g.n(PNode::Bool);
    let i8_ = env.g.n(I8);
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let x = b.iconst(Ty::I8, 1);
    let c = Call { which: Which::ThreeWayCompare, generics: &[GenericArg::Ty(bool_)], args: &[Arg::Scalar(x), Arg::Scalar(x)], ret: i8_, site: None };
    assert_eq!(
        env.call(&mut b, &c).unwrap_err().to_string(),
        "intrinsic `three_way_compare`: `three_way_compare` of a type that is not an integer or `char`"
    );
}

/// `slice_get_unchecked::<*const u16, *const [u16], u16>(p, i)`: "Projects to the `index`-th
/// element of `slice_ptr` ... without a bounds check" (core/src/intrinsics/mod.rs): the wide
/// pointer's data word (at 0 of its two words), then `ptr_add inbounds` of `i * 2`; no
/// compare of `i` with the length (the caller's promise, `index < PtrMetadata(slice_ptr)`).
/// A zero-sized element: the data word itself. A `SlicePtr` to no slice is refused by name.
/// The run's memory holds no pointers, so this reads the built operations back; the value is
/// the rung x0-ref-13-slice-get-unchecked's. Passes silently.
#[test]
fn slice_get_unchecked_projects_without_a_check() {
    let mut env = Env::new();
    let u16_ = env.g.n(U16);
    let slice = env.g.n(PNode::Slice(u16_));
    let sp = env.g.n(PNode::Ptr(blaze_typeck::Mutbl::Not, slice));
    let ip = env.g.n(PNode::Ptr(blaze_typeck::Mutbl::Not, u16_));
    let unit = env.g.tuple(&[]);
    let units = env.g.n(PNode::Slice(unit));
    let usp = env.g.n(PNode::Ptr(blaze_typeck::Mutbl::Not, units));
    let uip = env.g.n(PNode::Ptr(blaze_typeck::Mutbl::Not, unit));
    let mut get = |b: &mut Builder<Global, Full>, item: PTy, slice_ptr: PTy, elem: PTy, index: Value| {
        let slot = b.slot(16, 3, SlotKind::Temp);
        let at = b.slot_addr(slot);
        let c = Call {
            which: Which::SliceGetUnchecked,
            generics: &[GenericArg::Ty(item), GenericArg::Ty(slice_ptr), GenericArg::Ty(elem)],
            args: &[Arg::Place(at), Arg::Scalar(index)],
            ret: item,
            site: None,
        };
        env.call(b, &c)
    };
    let f = func(&[Ty::I64], &[Ty::PTR], |b, a| vec![scalar(get(b, ip, sp, u16_, a[0]).expect("builds"))]);
    let built = ops(&f);
    assert!(built.contains(&(Op::Mul, Flags::NUW)), "index times the stride: {built:?}");
    assert!(built.contains(&(Op::PtrAdd, Flags::INBOUNDS)), "an in-bounds projection: {built:?}");
    assert!(!built.iter().any(|&(op, _)| op == Op::Icmp), "no bounds check: {built:?}");
    let f = func(&[Ty::I64], &[Ty::PTR], |b, a| vec![scalar(get(b, uip, usp, unit, a[0]).expect("builds"))]);
    assert!(!ops(&f).iter().any(|&(op, _)| op == Op::PtrAdd), "a zero-sized element is at the data word: {:?}", ops(&f));
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let i = b.iconst(Ty::I64, 0);
    assert_eq!(
        get(&mut b, ip, ip, u16_, i).unwrap_err().to_string(),
        "intrinsic `slice_get_unchecked`: `slice_get_unchecked` of a `SlicePtr` that is not a pointer to a slice"
    );
}

/// `type_id::<T>()`'s hash (instance.rs `type_id_hash`): one type gives one value however its
/// table was filled (a second table, other types interned first, the type's index another:
/// what an alias resolved to the same type, or another crate's build, sees), two types two
/// values, and lifetimes are not in it (a ground `&T` holds no region).
#[test]
fn type_id_is_one_value_per_type() {
    use crate::instance::type_id_hash;
    let mut a = G::new();
    let (u8a, u32a) = (a.n(U8), a.n(U32));
    let pair_a = a.tuple(&[u8a, u32a]);
    let mut b = G::new();
    let filler = b.n(PNode::Bool);
    let _ = b.r(filler);
    let (u32b, u8b) = (b.n(U32), b.n(U8));
    let pair_b = b.tuple(&[u8b, u32b]);
    assert_ne!(pair_a, pair_b, "the control: the type's index differs between the tables");
    let h = |g: &G, t: PTy| type_id_hash(t, g.t.view(), Alloc::global()).expect("a ground type");
    assert_eq!(h(&a, pair_a), h(&a, pair_a), "one type asked twice");
    assert_eq!(h(&a, pair_a), h(&b, pair_b), "one type in two tables (an alias of it, another build)");
    assert_eq!(h(&a, u8a), h(&b, u8b));
    let swapped = b.tuple(&[u32b, u8b]);
    let i8b = b.n(I8);
    let rb = b.r(u8b);
    let pb = b.n(PNode::Ptr(blaze_typeck::Mutbl::Not, u8b));
    let (arr2, arr3) = (b.array(u8b, 2), b.array(u8b, 3));
    let all = [pair_b, swapped, u8b, i8b, u32b, rb, pb, arr2, arr3];
    for (i, &x) in all.iter().enumerate() {
        for &y in &all[i + 1..] {
            assert_ne!(h(&b, x), h(&b, y), "{:?} and {:?} are two types", b.t.view().get(x), b.t.view().get(y));
        }
    }
    println!("type_id::<(u8, u32)>() = {:#034x} in both tables; 9 types, 9 values", h(&a, pair_a));
}

/// The built `type_id::<T>()` (a run-time call: core makes it in `TypeId::of`'s `const` block,
/// blaze-consteval tests/type_id.rs) is `type_id_hash`'s 128 bits as two little-endian words,
/// so it equals the constant evaluator's value, which is made from the same function.
#[test]
fn type_id_is_the_one_hash_at_run_time() {
    let mut env = Env::new();
    let (u8_, u32_, u64_) = (env.g.n(U8), env.g.n(U32), env.g.n(U64));
    let pair = env.g.tuple(&[u8_, u32_]);
    let tid = env.g.array(u64_, 2);
    for t in [u8_, pair] {
        let want = crate::instance::type_id_hash(t, env.g.t.view(), Alloc::global()).expect("a ground type");
        let f = func(&[], &[Ty::I64, Ty::I64], |b, _| {
            let c = Call { which: Which::TypeId, generics: &[GenericArg::Ty(t)], args: &[], ret: tid, site: None };
            let Out::At(at) = env.call(b, &c).expect("builds") else { panic!("`TypeId` is returned in memory") };
            let lo = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::aligned(3));
            let hi = b.load(Ty::I64, Flags::NONE, at, 8, MemAttrs::aligned(3));
            vec![lo, hi]
        });
        let got = eval(&f, &[]);
        assert_eq!(got[0] | (got[1] << 64), want, "{t:?}");
    }
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let c = Call { which: Which::TypeId, generics: &[GenericArg::Ty(u8_)], args: &[], ret: u64_, site: None };
    assert_eq!(env.call(&mut b, &c).unwrap_err().to_string(), "intrinsic `type_id`: a result that is not `TypeId`'s 16 bytes");
}

/// `float_to_int_unchecked::<F, I>(x)`: truncation toward zero, `fptosi` or `fptoui` by `I`'s
/// signedness, at each integer width. Its undefined inputs (NaN, infinities, out of range) are
/// left out: blaze's operations saturate there, a defined value where Rust's is undefined.
#[test]
fn float_to_int_unchecked_truncates_toward_zero() {
    use blaze_typeck::FloatTy;
    let (f32_, f64_) = (PNode::Float(FloatTy::F32), PNode::Float(FloatTy::F64));
    let i16_ = PNode::Int(IntTy::I16);
    let (i64_, i128_, u128_) = (PNode::Int(IntTy::I64), PNode::Int(IntTy::I128), PNode::Uint(UintTy::U128));
    let f = |x: f32| u128::from(x.to_bits());
    let d = |x: f64| u128::from(x.to_bits());
    let cases: &[(PNode, Ty, u128, PNode, Ty, u128)] = &[
        (f32_, Ty::F32, f(3.9), I32, Ty::I32, 3),
        (f32_, Ty::F32, f(-3.9), I32, Ty::I32, 0xffff_fffd),
        (f32_, Ty::F32, f(-0.5), I32, Ty::I32, 0),
        (f32_, Ty::F32, f(255.9), U8, Ty::I8, 255),
        (f32_, Ty::F32, f(-128.0), I8, Ty::I8, 0x80),
        (f32_, Ty::F32, f(300.25), i16_, Ty::I16, 300),
        (f64_, Ty::F64, d(-1.5), i64_, Ty::I64, u128::from(u64::MAX)),
        (f64_, Ty::F64, d(4_294_967_295.75), U32, Ty::I32, 0xffff_ffff),
        (f64_, Ty::F64, d(1e20), u128_, Ty::I128, 100_000_000_000_000_000_000),
        (f64_, Ty::F64, d(-(2f64.powi(100))), i128_, Ty::I128, (-(1i128 << 100)) as u128),
    ];
    for &(fl, fty, x, int, ity, want) in cases {
        let mut env = Env::new();
        let (fp, ip) = (env.g.n(fl), env.g.n(int));
        let func_ = func(&[fty], &[ity], |b, a| {
            let c = Call { which: Which::FloatToIntUnchecked, generics: &[GenericArg::Ty(fp), GenericArg::Ty(ip)], args: &[Arg::Scalar(a[0])], ret: ip, site: None };
            vec![scalar(env.call(b, &c).expect("builds"))]
        });
        let signed = matches!(int, PNode::Int(_));
        assert!(ops(&func_).contains(&(if signed { Op::FptoSi } else { Op::FptoUi }, Flags::NONE)), "{int:?}");
        assert_eq!(eval(&func_, &[x]), [want], "float_to_int_unchecked::<{fl:?}, {int:?}>({x:#x})");
    }
    // f16 and f128 build the same operation (blaze-lir's legalize makes them, not the evaluator).
    for (fl, fty) in [(PNode::Float(FloatTy::F16), Ty::F16), (PNode::Float(FloatTy::F128), Ty::scalar(blaze_lir::Scalar::F128))] {
        let mut env = Env::new();
        let (fp, ip) = (env.g.n(fl), env.g.n(U64));
        let func_ = func(&[fty], &[Ty::I64], |b, a| {
            let c = Call { which: Which::FloatToIntUnchecked, generics: &[GenericArg::Ty(fp), GenericArg::Ty(ip)], args: &[Arg::Scalar(a[0])], ret: ip, site: None };
            vec![scalar(env.call(b, &c).expect("builds"))]
        });
        assert!(ops(&func_).contains(&(Op::FptoUi, Flags::NONE)), "{fl:?}");
    }
    // An integer as the float, or a float as the integer, is not the intrinsic's signature.
    let mut env = Env::new();
    let (fp, ip) = (env.g.n(f32_), env.g.n(U8));
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let x = b.fconst(Ty::F32, 0);
    let c = Call { which: Which::FloatToIntUnchecked, generics: &[GenericArg::Ty(ip), GenericArg::Ty(fp)], args: &[Arg::Scalar(x)], ret: fp, site: None };
    assert_eq!(env.call(&mut b, &c).unwrap_err().to_string(), "intrinsic `float_to_int_unchecked`: a first type argument that is not a float");
}

#[test]
fn structural_record_reflection_intrinsics_lower_to_constants() {
    use blaze_typeck::{ItemKey, List, Name, RecordSchema};

    let mut env = Env::new();
    let u8_ = env.g.n(U8);
    let u32_ = env.g.n(U32);
    let u64_ = env.g.n(U64);
    let i32_ = env.g.n(I32);
    let bool_ = env.g.n(PNode::Bool);
    let usize_ = env.g.n(USIZE);
    let unit = env.g.tuple(&[]);
    let pair = env.g.tuple(&[u8_, u32_]);

    let unit_struct = env.w.strukt(0, &[], ReprAttrs::RUST);
    let pair_struct = env.w.strukt(0, &[F::Leaf(U32), F::Leaf(I32)], ReprAttrs::RUST);
    let an_enum = env.w.def(AdtKind::Enum, 0, &[&[F::Leaf(U32)], &[F::Leaf(U8)]], ReprAttrs::RUST);
    let a_union = env.w.def(AdtKind::Union, 0, &[&[F::Leaf(U32), F::Leaf(U64)]], ReprAttrs::RUST);

    let t_unit_struct = env.g.adt(unit_struct, &[]);
    let t_pair_struct = env.g.adt(pair_struct, &[]);
    let t_enum = env.g.adt(an_enum, &[]);
    let t_union = env.g.adt(a_union, &[]);
    let t_missing_adt = env.g.adt(ItemKey(0xdead_beef), &[]);

    env.g.t.register_record_schema(RecordSchema::EMPTY, &[]).expect("empty schema");
    let rec_empty = env.g.n(PNode::Record(RecordSchema::EMPTY, List::EMPTY));

    let pos_names = [Name::of("x"), Name::of("y")];
    let pos_schema = RecordSchema::of_sorted_names(&pos_names);
    env.g.t.register_record_schema(pos_schema, &pos_names).expect("pos schema");
    let pos_fields = env.g.t.intern_list(&[i32_, i32_]).expect("pos fields");
    let rec_pos = env.g.n(PNode::Record(pos_schema, pos_fields));

    let vel_names = [Name::of("vx"), Name::of("vy")];
    let vel_schema = RecordSchema::of_sorted_names(&vel_names);
    env.g.t.register_record_schema(vel_schema, &vel_names).expect("vel schema");
    let vel_fields = env.g.t.intern_list(&[i32_, i32_]).expect("vel fields");
    let rec_vel = env.g.n(PNode::Record(vel_schema, vel_fields));

    let overlap_names = [Name::of("hp"), Name::of("x")];
    let overlap_schema = RecordSchema::of_sorted_names(&overlap_names);
    env.g.t.register_record_schema(overlap_schema, &overlap_names).expect("overlap schema");
    let overlap_fields = env.g.t.intern_list(&[u32_, i32_]).expect("overlap fields");
    let rec_overlap = env.g.n(PNode::Record(overlap_schema, overlap_fields));

    let eval_one = |env: &mut Env, call: &Call<'_>, rty: Ty| -> u128 {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let rs = [Param::of(rty)];
        let sig = b.sig(Conv::C, Variadic::No, &[], &rs);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let out = {
            let mut cx = LayoutCx { ground: &mut env.g.t, facts: &env.w, scratch: alloc };
            let needs_drop = |_: PTy| false;
            let mut icx = IntrinsicCx {
                layouts: &mut env.ls,
                cx: &mut cx,
                needs_drop: &needs_drop,
                ub_checks: crate::UbChecks::Off,
                overflow_checks: crate::OverflowChecks::Off,
            };
            scalar(build(&mut b, call, &mut icx).expect("intrinsic builds"))
        };
        b.ret(&[out]);
        let f = b.finish_checked().expect("built function verifies");
        match run_in(f.view(), &[], 10_000, alloc).expect("evaluator runs") {
            Outcome::Returned(vs) => match vs.as_slice() {
                [Val::Int(i)] => *i,
                other => panic!("expected one int result, got {other:?}"),
            },
            other => panic!("did not return: {other:?}"),
        }
    };

    // 1. `blaze_field_count`
    for (ty, want) in [
        (rec_empty, 0u128),
        (rec_pos, 2),
        (unit, 0),
        (pair, 2),
        (t_unit_struct, 0),
        (t_pair_struct, 2),
        (t_enum, 0),
        (t_union, 0),
        (u32_, 0),
    ] {
        let c = Call { which: Which::BlazeFieldCount, generics: &[GenericArg::Ty(ty)], args: &[], ret: usize_, site: None };
        assert_eq!(eval_one(&mut env, &c, Ty::I64), want, "blaze_field_count({ty:?})");
    }
    // Missing ADT propagates `LayoutError` with `Refusal::NoAdt` so lazy front ends load and retry.
    {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let mut cx = LayoutCx { ground: &mut env.g.t, facts: &env.w, scratch: alloc };
        let needs_drop = |_: PTy| false;
        let mut icx = IntrinsicCx {
            layouts: &mut env.ls,
            cx: &mut cx,
            needs_drop: &needs_drop,
            ub_checks: crate::UbChecks::Off,
            overflow_checks: crate::OverflowChecks::Off,
        };
        let c = Call { which: Which::BlazeFieldCount, generics: &[GenericArg::Ty(t_missing_adt)], args: &[], ret: usize_, site: None };
        assert!(matches!(
            build(&mut b, &c, &mut icx),
            Err(IntrinsicError::Layout(crate::layout::LayoutError { why: crate::layout::Refusal::NoAdt(k), .. })) if k == ItemKey(0xdead_beef)
        ));
    }

    // 2. `blaze_records_disjoint`
    for (a, b, want) in [
        (rec_pos, rec_vel, 1u128),
        (rec_pos, rec_overlap, 0),
        (rec_pos, rec_pos, 0),
        (rec_empty, rec_pos, 1),
        (rec_pos, unit, 1),
        (unit, rec_pos, 1),
        (unit, unit, 1),
        (u32_, u32_, 0),
        (u32_, i32_, 1),
        (rec_pos, u32_, 1),
    ] {
        let c = Call {
            which: Which::BlazeRecordsDisjoint,
            generics: &[GenericArg::Ty(a), GenericArg::Ty(b)],
            args: &[],
            ret: bool_,
            site: None,
        };
        assert_eq!(eval_one(&mut env, &c, Ty::I8), want, "blaze_records_disjoint({a:?}, {b:?})");
    }

    // 3. `blaze_record_schema_id`
    for (ty, want) in [
        (rec_pos, u128::from(pos_schema.0)),
        (rec_vel, u128::from(vel_schema.0)),
        (rec_empty, u128::from(RecordSchema::EMPTY.0)),
        (u32_, 0),
        (pair, 0),
        (t_pair_struct, 0),
        (t_enum, 0),
    ] {
        let c = Call { which: Which::BlazeRecordSchemaId, generics: &[GenericArg::Ty(ty)], args: &[], ret: u64_, site: None };
        assert_eq!(eval_one(&mut env, &c, Ty::I64), want, "blaze_record_schema_id({ty:?})");
    }
}

mod bytes;
mod offsets;
