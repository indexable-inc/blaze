//! Each intrinsic's meaning (the table's quote) checked on what it builds: run by blaze-lir's
//! reference evaluator where it models the operations, and read back from the function where
//! it does not (memory intrinsics, atomics, flags, data).

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv, DataId, Func, SlotKind};
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
        let mut icx = IntrinsicCx { layouts: &mut self.ls, cx: &mut cx, needs_drop: &needs_drop };
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
    // `simd_add` is intrinsics/simd.rs's row now; `simd_shuffle` is in no table.
    let refused = Which::named("simd_shuffle").unwrap_err();
    assert_eq!(refused.to_string(), "intrinsic `simd_shuffle` is not in blaze's intrinsics table (crates/blaze-build/src/intrinsics.rs)");
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

/// "Gets a reference to a static `Location` indicating where it was called.": a data row of
/// the `Location` by its layout, its filename a pointer to the path's bytes and a NUL.
#[test]
fn caller_location_is_a_location_in_data() {
    let mut env = Env::new();
    // `Location<'a> { filename: NonNull<str>, line: u32, col: u32, .. }`, the pointer modelled
    // as `*const str`.
    let loc = env.w.strukt(1, &[F::RawP(0), F::Leaf(U32), F::Leaf(U32)], ReprAttrs::RUST);
    let str_ = env.g.n(PNode::Str);
    let loc_ty = env.g.adt(loc, &[str_]);
    let r = env.g.r(loc_ty);
    let f = func(&[], &[Ty::PTR], |b, _| {
        let c = Call { which: Which::CallerLocation, generics: &[], args: &[], ret: r, site: Some((Site::Here { file: "src/main.rs", line: 7, col: 5 }, loc_ty)) };
        vec![scalar(env.call(b, &c).unwrap())]
    });
    let v = f.view();
    assert_eq!(v.data_bytes_of(DataId(0)), b"src/main.rs\0");
    let row = v.data_bytes_of(DataId(1));
    assert_eq!(row.len(), 24, "a wide pointer and two u32s");
    assert_eq!(&row[8..16], &11u64.to_le_bytes(), "the length leaves the NUL out");
    assert_eq!((&row[16..20], &row[20..24]), (&7u32.to_le_bytes()[..], &5u32.to_le_bytes()[..]), "line, then col");
    let relocs = v.data_relocs_of(DataId(1));
    assert_eq!((relocs.len(), relocs[0].offset), (1, 0), "the filename's address is a relocation");
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

mod offsets;
