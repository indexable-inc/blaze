//! The vector `simd_*` rows (intrinsics/simd_vec.rs): built as one LIR vector operation each,
//! legalized as the Reference and Fast tiers take them (blaze-lir `legalize`), then run by
//! blaze-lir's reference evaluator.
//!
//! The rules, from the pinned `library/core/src/intrinsics/simd/mod.rs`: `simd_saturating_sub`
//! "Subtracts two simd vectors elementwise, with saturation." (`vqsubq_u8`); `simd_reduce_max`
//! "Returns the maximum element of a vector." (`vmaxvq_u8`, which `is_ascii_neon` calls in
//! `library/core/src/slice/ascii.rs`), compared by the lanes' type: `u8` here.
//!
//! Expected: `test intrinsics::simd_vec::tests::reduce_and_saturate ... ok`. A build that
//! subtracts wrapping gives 252 (lane 0: 1 - 5); one that takes the signed maximum gives 10
//! (195 is -61 as an `i8`); one that is not one vector operation each fails the op counts.

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv, Func, RefId, SlotKind};
use blaze_lir::legalize::{QuadRoutines, SoftFloat, legalize};
use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};
use blaze_typeck::{PNode, PTy, UintTy};

use super::super::simd::SimdOp;
use super::super::{Arg, Call, GenericArg, IntrinsicCx, Out, Which, build};
use crate::layout::tests::{F, G, World};
use crate::layout::{LayoutCx, Layouts, ReprAttrs};

const SIMD: ReprAttrs = ReprAttrs { simd: true, ..ReprAttrs::RUST };
const U8: PNode = PNode::Uint(UintTy::U8);

/// No `f128` routine is bound: these functions have no `f128` operation.
struct NoRoutines;

impl QuadRoutines for NoRoutines {
    fn routine(&self, _: SoftFloat) -> Option<RefId> {
        None
    }
}

#[test]
fn reduce_and_saturate() {
    let heap = blaze_own::testing::heap();
    let mut w = World::default();
    let vector = w.strukt(1, &[F::P(0)], SIMD);
    let mut g = G::new();
    let mut ls: Layouts<Global> = Layouts::new_in(Global);
    let u8_ = g.n(U8);
    let lanes16 = g.array(u8_, 16);
    // `uint8x16_t`: `#[repr(simd)] struct Simd<[u8; 16]>`.
    let u8x16: PTy = g.adt(vector, &[lanes16]);

    let xs: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 200];
    let ys: [u8; 16] = [5; 16];

    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let mut addrs = [Value(0); 2];
    for (k, lanes) in [xs, ys].iter().enumerate() {
        let slot = b.slot(16, 4, SlotKind::Temp);
        let addr = b.slot_addr(slot);
        for (i, &lane) in lanes.iter().enumerate() {
            let v = b.iconst(Ty::I8, u128::from(lane));
            b.store(Flags::NONE, v, addr, i as i32, MemAttrs::aligned(0));
        }
        addrs[k] = addr;
    }
    let max = {
        let mut cx = LayoutCx { ground: &mut g.t, facts: &w, scratch: heap.alloc() };
        let needs_drop = |_: PTy| false;
        let mut icx = IntrinsicCx { layouts: &mut ls, cx: &mut cx, needs_drop: &needs_drop, ub_checks: crate::UbChecks::Off, overflow_checks: crate::OverflowChecks::Off };
        // `vqsubq_u8(x, y)`
        let generics = [GenericArg::Ty(u8x16)];
        let args = [Arg::Place(addrs[0]), Arg::Place(addrs[1])];
        let sub = Call { which: Which::Simd(SimdOp::SaturatingSub), generics: &generics, args: &args, ret: u8x16, site: None };
        let diff = match build(&mut b, &sub, &mut icx).expect("simd_saturating_sub builds") {
            Out::At(v) => v,
            other => panic!("not a vector temporary: {other:?}"),
        };
        // `vmaxvq_u8(diff)`
        let generics = [GenericArg::Ty(u8x16), GenericArg::Ty(u8_)];
        let args = [Arg::Place(diff)];
        let reduce = Call { which: Which::Simd(SimdOp::ReduceMax), generics: &generics, args: &args, ret: u8_, site: None };
        match build(&mut b, &reduce, &mut icx).expect("simd_reduce_max builds") {
            Out::Scalar(v) => v,
            other => panic!("not a scalar: {other:?}"),
        }
    };
    let wide = b.convert(Op::Zext, Flags::NONE, Ty::I64, max);
    b.ret(&[wide]);
    let f: Func<Global> = b.finish_checked().expect("the built function verifies");

    // One vector operation each.
    let view = f.view();
    let count = |op: Op| (0..view.len()).filter(|&i| view.op(i) == op).count();
    assert_eq!(count(Op::UsubSat), 1, "simd_saturating_sub is one `usub_sat`");
    assert_eq!(count(Op::ReduceUmax), 1, "simd_reduce_max of `u8` lanes is one `reduce_umax`");
    assert_eq!(count(Op::ReduceSmax), 0, "`u8` lanes are compared unsigned");

    // As the Reference and Fast tiers take it: scalar lanes.
    let alloc = heap.alloc();
    let mut legal = f.view().to_func_in(alloc);
    let made = legalize(&mut legal, &NoRoutines, alloc).expect("the vector operations legalize");
    assert!(made.scalarized >= 2, "the vector operations were made scalar: {made:?}");
    let legal_view = legal.view();
    assert!((0..legal_view.len()).all(|i| !legal_view.ty(Value(i as u32)).is_vector()), "no vector value is left");

    let got = match run_in(legal.view(), &[], 100_000, Global).expect("the evaluator models it") {
        Outcome::Returned(v) => match v.first() {
            Some(Val::Int(i)) => *i,
            other => panic!("a non-integer result {other:?}"),
        },
        other => panic!("the run did not return: {other:?}"),
    };
    // max(saturating x - 5) = 200 - 5.
    assert_eq!(got, 195);
}
