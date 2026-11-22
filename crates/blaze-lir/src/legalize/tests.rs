//! Legalize's two checks (INTRINSICS.md P3): a vector add made scalar computes what the
//! vector add means, and an `f128` add becomes the call of compiler_builtins' `__addtf3`.

use std::alloc::Global;

use blaze_own::Alloc;

use super::{LegalizeError, QuadRoutines, SoftFloat, legalize};
use crate::{
    BlockKind, Builder, Flags, Full, Func, Op, Ty, Value,
    builder::{Param, Variadic},
    eval::{Outcome, Val, run_in},
    func::{Conv, RefId, RefKind},
    print,
    ty::Scalar,
};

/// Binds no routine.
struct NoRoutines;

impl QuadRoutines for NoRoutines {
    fn routine(&self, _: SoftFloat) -> Option<RefId> {
        None
    }
}

/// Binds `__addtf3` only, to one reference.
struct AddOnly(RefId);

impl QuadRoutines for AddOnly {
    fn routine(&self, r: SoftFloat) -> Option<RefId> {
        (r == SoftFloat::AddTf3).then_some(self.0)
    }
}

const I32: Param = Param::of(Ty::I32);
const F128: Param = Param::of(Ty::F128);

/// `fn(a0..a3, b0..b3: i32) -> (i32, i32, i32, i32)`: the lanes of `<a> + <b>` as
/// `<4 x i32>`, built with `insert`, `add` and `extract`.
fn add_v4i32() -> Func<Alloc<'static>> {
    let mut b = Builder::<Alloc<'static>, Full>::new_in(64, Alloc::global());
    let sig = b.sig(Conv::C, Variadic::No, &[I32; 8], &[I32; 4]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let args: [Value; 8] = core::array::from_fn(|_| b.param(Ty::I32));
    let v4 = Ty::vector(Scalar::I32, 4).expect("`<4 x i32>` is a LIR type");
    let mut a = b.uninit(v4);
    let mut c = b.uninit(v4);
    for k in 0..4 {
        a = b.insert(a, args[k], k as u32);
        c = b.insert(c, args[4 + k], k as u32);
    }
    let sum = b.binary(Op::Add, Flags::NONE, a, c);
    let lanes: [Value; 4] = core::array::from_fn(|k| b.extract(sum, k as u32));
    b.ret(&lanes);
    b.finish_checked().expect("the vector add is valid LIR")
}

/// `fn(x: f128, y: f128) -> f128 { x + y }`, with a reference for `__addtf3` (place 0).
fn add_f128() -> (Func<Alloc<'static>>, RefId) {
    let mut b = Builder::<Alloc<'static>, Full>::new_in(64, Alloc::global());
    let sig = b.sig(Conv::C, Variadic::No, &[F128, F128], &[F128]);
    b.set_signature(sig);
    let addtf3 = b.reference(RefKind::Func, 0);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::F128), b.param(Ty::F128));
    let sum = b.binary(Op::Fadd, Flags::NONE, x, y);
    b.ret(&[sum]);
    (b.finish_checked().expect("the f128 add is valid LIR"), addtf3)
}

/// The next of a splitmix64 sequence: fixed inputs, the same on every run.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// LIR's rule (lib.rs, "Semantics"): an operation without flags is defined, and `add` on
/// integers wraps (Cranelift's `iadd`, LLVM's `add` without `nsw`/`nuw`), lane by lane on a
/// vector (op.rs: "Floats, scalar or lane by lane"; integer arithmetic likewise). So lane `k`
/// of `<a> + <b>` is `a[k].wrapping_add(b[k])`. The evaluator has no vectors (eval.rs: "Anything
/// else (a vector, ...)" is refused), so the input is checked against that rule, not against
/// a run of it; the output must have no vector left and give the rule's lanes on eight fixed
/// pseudo-random inputs (edge values among them by the wrap). A legalize that dropped a lane,
/// swapped two, or lost the wrap fails.
///
/// Expected line: `test legalize::tests::scalarize_add_v4i32 ... ok`.
#[test]
fn scalarize_add_v4i32() {
    let mut func = add_v4i32();
    let stats = legalize(&mut func, &NoRoutines, Alloc::global()).expect("a vector add legalizes");
    assert!(stats.scalarized >= 1, "{stats:?}");
    assert!(func.tys.iter().all(|&ty| !Ty(ty).is_vector()), "a vector is left:\n{}", print(func.view()));
    let mut state = 0x5eed_u64;
    for case in 0..8 {
        let inputs: [u32; 8] = core::array::from_fn(|_| match case {
            0 => u32::MAX,
            _ => next(&mut state) as u32,
        });
        let args: [Val; 8] = core::array::from_fn(|k| Val::Int(u128::from(inputs[k])));
        let outcome = run_in(func.view(), &args, 10_000, Global).expect("the scalar function runs in the evaluator");
        let Outcome::Returned(values) = outcome else {
            panic!("case {case}: {outcome:?}");
        };
        let expected: [Val; 4] = core::array::from_fn(|k| Val::Int(u128::from(inputs[k].wrapping_add(inputs[4 + k]))));
        assert_eq!(&values[..], &expected[..], "case {case}: {inputs:?}");
    }
}

/// compiler_builtins (float/add.rs, nightly-2026-10-27): `pub extern "C" fn __addtf3(a: f128,
/// b: f128) -> f128`, the routine LLVM itself calls for an `fadd fp128` on a target with no
/// quad-precision unit (aarch64). After legalize the add is a `call` of the reference the
/// caller bound for [`SoftFloat::AddTf3`], whose symbol is `__addtf3`, and no `fadd` is left;
/// with no routine bound, legalize refuses by the routine's name.
///
/// Expected line: `test legalize::tests::f128_add_becomes_call ... ok`.
#[test]
fn f128_add_becomes_call() {
    assert_eq!(SoftFloat::AddTf3.symbol(), "__addtf3");
    let (mut func, addtf3) = add_f128();
    let stats = legalize(&mut func, &AddOnly(addtf3), Alloc::global()).expect("an f128 add legalizes");
    assert_eq!((stats.calls, stats.routines), (1, SoftFloat::AddTf3.bit()), "{stats:?}");
    let text = print(func.view());
    let callee = addtf3.to_string();
    assert!(text.lines().any(|line| line.contains("= call") && line.contains(&callee)), "no call of {callee} (__addtf3):\n{text}");
    assert!(!text.contains("fadd"), "an `fadd` is left:\n{text}");

    let (mut unbound, _) = add_f128();
    let refused = legalize(&mut unbound, &NoRoutines, Alloc::global());
    assert_eq!(refused, Err(LegalizeError::NoRoutine(SoftFloat::AddTf3)));
}
