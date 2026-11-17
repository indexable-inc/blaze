//! The `simd_*` intrinsics (intrinsics/simd.rs), each checked on what it builds, run by
//! blaze-lir's reference evaluator. The rules are the declarations' doc comments in the pinned
//! `library/core/src/intrinsics/simd/mod.rs`: `simd_eq`/`simd_lt`: "`U` must be a vector of
//! integers with the same number of elements and element size as `T`. Returns `0` for false
//! and `!0` for true."; `simd_splat`: "Creates a vector where every lane has the provided
//! value."; `simd_extract`: "`T` must be a vector with element type `U`, and `idx` must be
//! `const`." with "`idx` must be const and in-bounds of the vector.".
//!
//! Expected: `test intrinsics::simd::tests::<name> ... ok` for each of the four tests. A build
//! that compares `i8` lanes unsigned fails `lt_compares_by_the_lane_signedness`; one that
//! writes 1 for true fails `eq_of_a_splat_sets_all_ones_in_the_equal_lane`; one that reads a
//! non-constant index or one past the lanes fails `extract_reads_its_constant_lane_and_refuses_others`.

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv, Func, SlotKind};
use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};
use blaze_own::Alloc;
use blaze_typeck::{IntTy, ItemKey, PNode, PTy, UintTy};

use super::super::{Arg, Call, GenericArg, IntrinsicCx, IntrinsicError, Out, Which, build};
use super::{SimdOp, TABLE};
use crate::layout::tests::{F, G, World};
use crate::layout::{LayoutCx, Layouts, ReprAttrs};

const SIMD: ReprAttrs = ReprAttrs { simd: true, ..ReprAttrs::RUST };
const U8: PNode = PNode::Uint(UintTy::U8);
const I8: PNode = PNode::Int(IntTy::I8);

struct Env {
    w: World,
    g: G,
    ls: Layouts<Global>,
    /// `#[repr(simd)] struct Simd<A>(A)`, as core_arch's `Simd<T, N>([T; N])`.
    vector: ItemKey,
}

impl Env {
    fn new() -> Env {
        let mut w = World::default();
        let vector = w.strukt(1, &[F::P(0)], SIMD);
        Env { w, g: G::new(), ls: Layouts::new_in(Global), vector }
    }

    /// The 8-byte vector of eight `elem` lanes (`uint8x8_t`, `int8x8_t`).
    fn v8(&mut self, elem: PNode) -> PTy {
        let e = self.g.n(elem);
        let a = self.g.array(e, 8);
        self.g.adt(self.vector, &[a])
    }

    fn call(&mut self, b: &mut Builder<Global, Full>, c: &Call<'_>) -> Result<Out, IntrinsicError> {
        let mut cx = LayoutCx { ground: &mut self.g.t, facts: &self.w, scratch: Alloc::global() };
        let needs_drop = |_: PTy| false;
        let mut icx = IntrinsicCx { layouts: &mut self.ls, cx: &mut cx, needs_drop: &needs_drop, ub_checks: crate::UbChecks::Off, overflow_checks: crate::OverflowChecks::Off };
        build(b, c, &mut icx)
    }
}

fn at(o: Out) -> Value {
    match o {
        Out::At(v) => v,
        other => panic!("not a vector temporary: {other:?}"),
    }
}

/// A fn of no parameters returning a `u64`: the 8-byte vectors `xs` and `ys` written lane by
/// lane into two temporaries, then `body` given their addresses.
fn run(env: &mut Env, xs: [u8; 8], ys: [u8; 8], body: impl FnOnce(&mut Env, &mut Builder<Global, Full>, Value, Value) -> Value) -> u128 {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let mut addrs = [Value(0); 2];
    for (k, lanes) in [xs, ys].iter().enumerate() {
        let slot = b.slot(8, 3, SlotKind::Temp);
        let addr = b.slot_addr(slot);
        for (i, &lane) in lanes.iter().enumerate() {
            let v = b.iconst(Ty::I8, u128::from(lane));
            b.store(Flags::NONE, v, addr, i as i32, MemAttrs::aligned(0));
        }
        addrs[k] = addr;
    }
    let r = body(env, &mut b, addrs[0], addrs[1]);
    b.ret(&[r]);
    let f: Func<Global> = b.finish_checked().expect("the built function verifies");
    match run_in(f.view(), &[], 10_000, Global).expect("the evaluator models it") {
        Outcome::Returned(v) => match v.first() {
            Some(Val::Int(i)) => *i,
            other => panic!("a non-integer result {other:?}"),
        },
        other => panic!("the run did not return: {other:?}"),
    }
}

/// The vector at `v` as the `u64` of its 8 bytes, little endian (as `vreinterpret_u64_u8`).
fn as_u64(b: &mut Builder<Global, Full>, v: Value) -> Value {
    b.load(Ty::I64, Flags::NONE, v, 0, MemAttrs::aligned(3))
}

#[test]
fn the_simd_table_is_sorted_and_every_row_is_named() {
    for w in TABLE.windows(2) {
        assert!(w[0].name < w[1].name, "{} before {}", w[0].name, w[1].name);
    }
    for e in TABLE {
        assert_eq!(Which::named(e.name), Ok(e.which), "{} is found", e.name);
        assert_eq!(e.which.entry().name, e.name, "one row per intrinsic");
    }
    assert_eq!(Which::named("simd_eq"), Ok(Which::Simd(SimdOp::Eq)));
    assert_eq!(Which::named("simd_reduce_or"), Ok(Which::Simd(SimdOp::ReduceOr)), "a vector row (intrinsics/simd_vec.rs)");
    assert!(Which::named("simd_funnel_shl").is_err(), "an intrinsic not in the table stays refused by name");
}

/// `vceq_u8(v, vdup_n_u8(3))` of `[1, 2, ..., 8]`: lane 2 all ones, the rest 0.
#[test]
fn eq_of_a_splat_sets_all_ones_in_the_equal_lane() {
    let mut env = Env::new();
    let u8x8 = env.v8(U8);
    let got = run(&mut env, [1, 2, 3, 4, 5, 6, 7, 8], [0; 8], |env, b, x, _| {
        let three = b.iconst(Ty::I8, 3);
        let generics = [GenericArg::Ty(u8x8), GenericArg::Ty(env.g.n(U8))];
        let splat = Call { which: Which::Simd(SimdOp::Splat), generics: &generics, args: &[Arg::Scalar(three)], ret: u8x8, site: None };
        let y = at(env.call(b, &splat).expect("splat builds"));
        let generics = [GenericArg::Ty(u8x8), GenericArg::Ty(u8x8)];
        let eq = Call { which: Which::Simd(SimdOp::Eq), generics: &generics, args: &[Arg::Place(x), Arg::Place(y)], ret: u8x8, site: None };
        let r = at(env.call(b, &eq).expect("eq builds"));
        as_u64(b, r)
    });
    assert_eq!(got, 0x0000_0000_00FF_0000);
}

/// `vcltz_s8` of `[-1, 0, 1, -128, 5, 6, 7, 8]`: `simd_lt` against zero on `i8` lanes is 0xFF
/// in lanes 0 and 3 only.
#[test]
fn lt_compares_by_the_lane_signedness() {
    let mut env = Env::new();
    let i8x8 = env.v8(I8);
    let u8x8 = env.v8(U8);
    let got = run(&mut env, [0xFF, 0, 1, 0x80, 5, 6, 7, 8], [0; 8], |env, b, x, zero| {
        let generics = [GenericArg::Ty(i8x8), GenericArg::Ty(u8x8)];
        let lt = Call { which: Which::Simd(SimdOp::Lt), generics: &generics, args: &[Arg::Place(x), Arg::Place(zero)], ret: u8x8, site: None };
        let r = at(env.call(b, &lt).expect("lt builds"));
        as_u64(b, r)
    });
    assert_eq!(got, 0x0000_0000_FF00_00FF);
}

/// `simd_extract::<u8x8, u8>(v, 7)` reads lane 7; an index of 8 (past the lanes) and an index
/// that is not a constant are refused by name.
#[test]
fn extract_reads_its_constant_lane_and_refuses_others() {
    let mut env = Env::new();
    let u8x8 = env.v8(U8);
    let u8_ = env.g.n(U8);
    let generics = [GenericArg::Ty(u8x8), GenericArg::Ty(u8_)];
    let got = run(&mut env, [1, 2, 3, 4, 5, 6, 7, 8], [0; 8], |env, b, x, _| {
        let seven = b.iconst(Ty::I32, 7);
        let ex = Call { which: Which::Simd(SimdOp::Extract), generics: &generics, args: &[Arg::Place(x), Arg::Scalar(seven)], ret: u8_, site: None };
        let Out::Scalar(lane) = env.call(b, &ex).expect("extract builds") else { panic!("a lane is a scalar") };
        b.convert(Op::Zext, Flags::NONE, Ty::I64, lane)
    });
    assert_eq!(got, 8);
    run(&mut env, [0; 8], [0; 8], |env, b, x, y| {
        let eight = b.iconst(Ty::I32, 8);
        let past = Call { which: Which::Simd(SimdOp::Extract), generics: &generics, args: &[Arg::Place(x), Arg::Scalar(eight)], ret: u8_, site: None };
        let error = env.call(b, &past).expect_err("an index past the lanes");
        assert!(error.to_string().contains("in-bounds"), "{error}");
        // A loaded index is not a constant.
        let loaded = b.load(Ty::I32, Flags::NONE, y, 0, MemAttrs::aligned(2));
        let dynamic = Call { which: Which::Simd(SimdOp::Extract), generics: &generics, args: &[Arg::Place(x), Arg::Scalar(loaded)], ret: u8_, site: None };
        let error = env.call(b, &dynamic).expect_err("a non-constant index");
        assert!(matches!(error, IntrinsicError::Unsupported { why, .. } if why.contains("not a constant")), "{error}");
        b.iconst(Ty::I64, 0)
    });
}

/// `simd_select_bitmask::<M, u8x8>(m, yes, no)`: "if the bit in `mask` is `1`, select the
/// element from `if_true`", lane 0 the lowest bit; the same with `M` a `u8` and a `[u8; 1]`. A
/// mask of fewer bits than lanes, and a signed `M`, are refused by name.
/// Prints `test intrinsics::simd::tests::select_bitmask_takes_lane_i_by_bit_i ... ok`.
#[test]
fn select_bitmask_takes_lane_i_by_bit_i() {
    let mut env = Env::new();
    let u8x8 = env.v8(U8);
    let u8_ = env.g.n(U8);
    let bytes1 = env.g.array(u8_, 1);
    let yes = [1, 2, 3, 4, 5, 6, 7, 8];
    let no = [0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8];
    // Bits 0, 2, 5 and 7 set.
    let want = u64::from_le_bytes([1, 0xA2, 3, 0xA4, 0xA5, 6, 0xA7, 8]);
    for array in [false, true] {
        let got = run(&mut env, yes, no, |env, b, y, n| {
            let m = b.iconst(Ty::I8, 0b1010_0101);
            let (mt, arg) = if array {
                let slot = b.slot(1, 0, SlotKind::Temp);
                let at = b.slot_addr(slot);
                b.store(Flags::NONE, m, at, 0, MemAttrs::aligned(0));
                (bytes1, Arg::Place(at))
            } else {
                (u8_, Arg::Scalar(m))
            };
            let generics = [GenericArg::Ty(mt), GenericArg::Ty(u8x8)];
            let c = Call { which: Which::Simd(SimdOp::SelectBitmask), generics: &generics, args: &[arg, Arg::Place(y), Arg::Place(n)], ret: u8x8, site: None };
            let r = at(env.call(b, &c).expect("select_bitmask builds"));
            as_u64(b, r)
        });
        assert_eq!(got, u128::from(want), "a mask as {}", if array { "`[u8; 1]`" } else { "`u8`" });
    }
    let u16_ = env.g.n(PNode::Uint(UintTy::U16));
    let lanes16 = env.g.array(u16_, 16);
    let u16x16 = env.g.adt(env.vector, &[lanes16]);
    let i8_ = env.g.n(I8);
    run(&mut env, [0; 8], [0; 8], |env, b, y, n| {
        let m = b.iconst(Ty::I8, 0);
        for (mt, vt, why) in [(u8_, u16x16, "fewer bits"), (i8_, u8x8, "unsigned integer")] {
            let generics = [GenericArg::Ty(mt), GenericArg::Ty(vt)];
            let c = Call { which: Which::Simd(SimdOp::SelectBitmask), generics: &generics, args: &[Arg::Scalar(m), Arg::Place(y), Arg::Place(n)], ret: vt, site: None };
            let error = env.call(b, &c).expect_err(why);
            assert!(error.to_string().contains(why), "{error}");
        }
        b.iconst(Ty::I64, 0)
    });
}
