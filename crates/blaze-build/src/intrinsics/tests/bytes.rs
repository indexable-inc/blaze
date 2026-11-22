//! The byte and pointer intrinsics tempo's census lacked, each run by blaze-lir's reference
//! evaluator against its rule in the pinned core (library/core/src/intrinsics/mod.rs) or, for a
//! const intrinsic, against the one rule function the constant evaluator computes it by
//! (intrinsics.rs `byte_order`, `int_cast_bits`).
//!
//! Expected: `test intrinsics::tests::bytes::<name> ... ok` for each test. A `raw_eq` that
//! compares only the first piece fails `raw_eq_compares_every_byte`; a `compare_bytes` that
//! compares signed bytes fails `compare_bytes_orders_as_byte_order`; a `blaze_int_cast` that
//! extends by `D`'s sign fails `blaze_int_cast_is_int_cast_bits`.

use blaze_lir::builder::MemAttrs;
use blaze_lir::func::SlotKind;
use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};
use blaze_typeck::{Mutbl, PNode};
use std::alloc::Global;

use super::{Env, I8, I32, U8, U16, U32, U64, USIZE, eval, func, lir_of, ops, scalar};
use crate::intrinsics::{Arg, Call, GenericArg, Out, Which, byte_order, int_cast_bits};

/// `bytes` stored one by one into a new slot of their length; its address.
fn bytes_at(b: &mut Builder<Global, Full>, bytes: &[Value], align: u8) -> Value {
    let slot = b.slot(bytes.len() as u32, align, SlotKind::Temp);
    let at = b.slot_addr(slot);
    for (i, &v) in bytes.iter().enumerate() {
        b.store(Flags::NONE, v, at, i as i32, MemAttrs::aligned(0));
    }
    at
}

/// "`x as D` between two integer types of any width": every pair of the standard widths, both
/// signs, against `int_cast_bits`.
#[test]
fn blaze_int_cast_is_int_cast_bits() {
    let widths = [(U8, 8, false), (I8, 8, true), (U16, 16, false), (U32, 32, false), (I32, 32, true), (U64, 64, false)];
    for &(from, fb, fs) in &widths {
        for &(to, tb, _) in &widths {
            for x in [0u128, 1, 0x7f, 0x80, 0xff, 0x1234_5678_9abc_def0] {
                let x = int_cast_bits(x, fb, false, fb);
                let mut env = Env::new();
                let (s, d) = (env.g.n(from), env.g.n(to));
                let f = func(&[lir_of(from)], &[lir_of(to)], |b, a| {
                    let c = Call { which: Which::BlazeIntCast, generics: &[GenericArg::Ty(s), GenericArg::Ty(d)], args: &[Arg::Scalar(a[0])], ret: d, site: None };
                    vec![scalar(env.call(b, &c).unwrap())]
                });
                assert_eq!(eval(&f, &[x])[0], int_cast_bits(x, fb, fs, tb), "{from:?} {x:#x} as {to:?}");
            }
        }
    }
    let mut env = Env::new();
    let (b8, s) = (env.g.n(PNode::Bool), env.g.n(U8));
    let _refused = func(&[Ty::I8], &[Ty::I8], |b, a| {
        let c = Call { which: Which::BlazeIntCast, generics: &[GenericArg::Ty(s), GenericArg::Ty(b8)], args: &[Arg::Scalar(a[0])], ret: b8, site: None };
        let error = env.call(b, &c).expect_err("`bool` is not an integer");
        assert!(error.to_string().contains("Both types must be integers"), "{error}");
        vec![a[0]]
    });
}

/// `raw_eq::<[u16; 3]>` compares in line (three `i16` pieces, no `memcmp`), `raw_eq::<[u8; 40]>`
/// past the in-line limit by `memcmp`; each is 1 exactly when every byte matches, wherever the
/// differing byte is.
#[test]
fn raw_eq_compares_every_byte() {
    for (n, align, elem, by_memcmp) in [(6u64, 1u8, U16, false), (40, 0, U8, true)] {
        for differ in [None, Some(0), Some(n as usize - 1)] {
            let mut env = Env::new();
            let e = env.g.n(elem);
            let count = if elem == U16 { n / 2 } else { n };
            let arr = env.g.array(e, count);
            let bool_ = env.g.n(PNode::Bool);
            let f = func(&[], &[Ty::I8], |b, _| {
                let left: Vec<Value> = (0..n).map(|i| b.iconst(Ty::I8, u128::from(i as u8) * 3)).collect();
                let right: Vec<Value> = (0..n as usize).map(|i| b.iconst(Ty::I8, u128::from(i as u8) * 3 + u128::from(differ == Some(i)))).collect();
                let (pa, pb) = (bytes_at(b, &left, align), bytes_at(b, &right, align));
                let c = Call { which: Which::RawEq, generics: &[GenericArg::Ty(arr)], args: &[Arg::Scalar(pa), Arg::Scalar(pb)], ret: bool_, site: None };
                vec![scalar(env.call(b, &c).unwrap())]
            });
            assert_eq!(ops(&f).iter().any(|&(op, _)| op == Op::Memcmp), by_memcmp, "{n} bytes");
            assert_eq!(eval(&f, &[])[0], u128::from(differ.is_none()), "{n} bytes, differing at {differ:?}");
        }
    }
}

/// `compare_bytes(left, right, n)` is `memcmp`, and its sign is `byte_order`'s: unsigned bytes,
/// the first `n` only.
#[test]
fn compare_bytes_orders_as_byte_order() {
    let cases: &[(&[u8], &[u8], u128)] = &[(&[1, 2, 3], &[1, 0xff, 0], 3), (&[0x80, 0, 0], &[0x7f, 9, 9], 3), (&[5, 6, 7], &[5, 6, 8], 2), (&[5, 6, 7], &[5, 6, 7], 3), (&[9, 0, 0], &[1, 0, 0], 0)];
    for &(l, r, n) in cases {
        let mut env = Env::new();
        let i32_ = env.g.n(I32);
        let f = func(&[Ty::I64], &[Ty::I32], |b, a| {
            let left: Vec<Value> = l.iter().map(|&x| b.iconst(Ty::I8, u128::from(x))).collect();
            let right: Vec<Value> = r.iter().map(|&x| b.iconst(Ty::I8, u128::from(x))).collect();
            let (pa, pb) = (bytes_at(b, &left, 0), bytes_at(b, &right, 0));
            let c = Call { which: Which::CompareBytes, generics: &[], args: &[Arg::Scalar(pa), Arg::Scalar(pb), Arg::Scalar(a[0])], ret: i32_, site: None };
            vec![scalar(env.call(b, &c).unwrap())]
        });
        assert!(ops(&f).iter().any(|&(op, _)| op == Op::Memcmp), "\"will usually lower to `memcmp`\"");
        let got = (eval(&f, &[n])[0] as u32).cast_signed().signum();
        let want = byte_order(&l[..n as usize], &r[..n as usize]);
        assert_eq!(got, want, "{l:?} against {r:?}, {n} bytes");
    }
}

/// `volatile_store::<[u32; 4]>` and `volatile_load::<[u32; 4]>`: four `i32` pieces each, every
/// access `volatile`, at each piece's offset.
#[test]
fn volatile_aggregates_move_by_volatile_pieces() {
    let mut env = Env::new();
    let u32_ = env.g.n(U32);
    let arr = env.g.array(u32_, 4);
    let unit = env.g.tuple(&[]);
    let f = func(&[Ty::I32], &[Ty::I32], |b, a| {
        let src = b.slot(16, 2, SlotKind::Temp);
        let src = b.slot_addr(src);
        for k in 0..4 {
            let v = b.iconst(Ty::I32, 10 + k);
            b.store(Flags::NONE, v, src, (4 * k) as i32, MemAttrs::aligned(2));
        }
        b.store(Flags::NONE, a[0], src, 12, MemAttrs::aligned(2));
        let dst = b.slot(16, 2, SlotKind::Temp);
        let dst = b.slot_addr(dst);
        let st = Call { which: Which::VolatileStore, generics: &[GenericArg::Ty(arr)], args: &[Arg::Scalar(dst), Arg::Place(src)], ret: unit, site: None };
        assert!(matches!(env.call(b, &st), Ok(Out::None)));
        let ld = Call { which: Which::VolatileLoad, generics: &[GenericArg::Ty(arr)], args: &[Arg::Scalar(dst)], ret: arr, site: None };
        let Ok(Out::At(copy)) = env.call(b, &ld) else { panic!("an aggregate is loaded into a temporary") };
        let first = b.load(Ty::I32, Flags::NONE, copy, 0, MemAttrs::aligned(2));
        let last = b.load(Ty::I32, Flags::NONE, copy, 12, MemAttrs::aligned(2));
        vec![b.binary(Op::Add, Flags::NONE, first, last)]
    });
    let volatile = ops(&f).iter().filter(|&&(op, fl)| matches!(op, Op::Load | Op::Store) && fl.contains(Flags::VOLATILE)).count();
    // The evaluator refuses a volatile access ("an instruction with flags"), so the build is read
    // back: four volatile `i32` pieces each way, at offsets 0, 4, 8 and 12.
    assert_eq!(volatile, 8, "four volatile stores, four volatile loads");
    let v = f.view();
    let offsets: Vec<i32> = (0..v.len()).filter(|&i| v.op(i) == Op::Load && v.flags(i).contains(Flags::VOLATILE)).map(|i| {
            let mut o = v.operands(i);
            o.value();
            o.offset()
        })
        .collect();
    assert_eq!(offsets, [0, 4, 8, 12]);
}

/// `write_box_via_move::<u32>(b, x)` writes `x` into the box's memory and returns the box.
#[test]
fn write_box_via_move_writes_into_the_box() {
    let mut env = Env::new();
    let u32_ = env.g.n(U32);
    let p = env.g.n(PNode::Ptr(Mutbl::Mut, u32_));
    let f = func(&[Ty::I32], &[Ty::I32], |b, a| {
        let slot = b.slot(4, 2, SlotKind::Temp);
        let at = b.slot_addr(slot);
        let c = Call { which: Which::WriteBoxViaMove, generics: &[GenericArg::Ty(u32_)], args: &[Arg::Scalar(at), Arg::Scalar(a[0])], ret: p, site: None };
        let back = scalar(env.call(b, &c).unwrap());
        vec![b.load(Ty::I32, Flags::NONE, back, 0, MemAttrs::aligned(2))]
    });
    assert_eq!(eval(&f, &[0xdead_beef])[0], 0xdead_beef);
}

/// `ptr_offset_from_unsigned::<()>`: "This function panics if `T` is a Zero-Sized Type": the call
/// is built as a trap that diverges.
#[test]
fn offset_from_of_a_zero_sized_type_traps() {
    let mut env = Env::new();
    let unit = env.g.tuple(&[]);
    let usize_ = env.g.n(USIZE);
    let f = func(&[], &[], |b, _| {
        let slot = b.slot(1, 0, SlotKind::Temp);
        let at = b.slot_addr(slot);
        let c = Call { which: Which::PtrOffsetFromUnsigned, generics: &[GenericArg::Ty(unit)], args: &[Arg::Scalar(at), Arg::Scalar(at)], ret: usize_, site: None };
        assert!(matches!(env.call(b, &c), Ok(Out::Diverges)));
        vec![]
    });
    assert!(ops(&f).iter().any(|&(op, _)| op == Op::Trap));
}

/// `ptr_mask(p, mask)`: the address `and` the mask, the provenance kept.
#[test]
fn ptr_mask_ands_the_address() {
    let mut env = Env::new();
    let u8_ = env.g.n(U8);
    let p = env.g.n(PNode::Ptr(Mutbl::Not, u8_));
    let f = func(&[Ty::I64, Ty::I64], &[Ty::I64], |b, a| {
        let ptr = b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, a[0]);
        let c = Call { which: Which::PtrMask, generics: &[GenericArg::Ty(u8_)], args: &[Arg::Scalar(ptr), Arg::Scalar(a[1])], ret: p, site: None };
        let masked = scalar(env.call(b, &c).unwrap());
        vec![b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, masked)]
    });
    assert_eq!(eval(&f, &[0x1234_5677, 0xffff_ffff_ffff_fff8])[0], 0x1234_5670);
}
