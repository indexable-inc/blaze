//! Calls past one integer each way, run on this Mac against Rust's own `extern "C"` functions
//! (rustc's AAPCS64 is the witness): `sret` in x8 both ways, `byval` copies both ways (a large
//! one copied by `memcpy`), `call_indirect`, and two-register returns both ways.

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;

use blaze_lir::{
    Builder, Flags, Full, Op, Ty, Value,
    builder::{Facts, MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Func, ParamKind, RefKind, SigId, SlotKind},
};

use crate::{
    EmitError, Fixup, RelocTo, Routine, Via,
    tests::{emitted, run},
};

/// 32 bytes: AAPCS64 returns it through x8 and passes it as a pointer to a copy.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Four([u64; 4]);

/// 256 bytes: past the inline copy bound, so a `byval` copy calls `memcpy`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Wide([u64; 32]);

#[repr(C)]
#[derive(PartialEq, Eq, Debug)]
struct Two(u64, u64);

extern "C" fn make_four(a: u64) -> Four {
    Four([a, a + 1, a + 2, a + 3])
}

/// Writes its copy, so a caller that passed its own memory would see the write.
extern "C" fn sum_four(mut four: Four) -> u64 {
    let sum = four.0.iter().sum();
    four.0[0] = 0xdead;
    std::hint::black_box(&mut four);
    sum
}

extern "C" fn sum_wide(mut wide: Wide) -> u64 {
    let sum = wide.0.iter().enumerate().map(|(i, &it)| it * (i as u64 + 1)).sum();
    wide.0[31] = 0xdead;
    std::hint::black_box(&mut wide);
    sum
}

extern "C" fn sub(a: u64, b: u64) -> u64 {
    a.wrapping_sub(b)
}

extern "C" fn split(a: u64) -> Two {
    Two(a >> 32, a & 0xffff_ffff)
}

const fn by_value(size: u32, align: u8) -> Param {
    Param {
        kind: ParamKind::Byval,
        facts: Facts { deref: 0, size, range: None, align, bits: 0 },
        ..Param::of(Ty::PTR)
    }
}

const SRET: Param = Param { kind: ParamKind::Sret, ..Param::of(Ty::PTR) };

fn begin(params: &[Param], rets: &[Param]) -> (Builder<Global, Full>, Vec<Value>) {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, params, rets);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let values = params.iter().map(|it| b.param(it.ty)).collect();
    (b, values)
}

fn sig(b: &mut Builder<Global, Full>, params: &[Param], rets: &[Param]) -> SigId {
    b.sig(Conv::C, Variadic::No, params, rets)
}

fn placed(func: &Func<Global>, callee: u64) -> blaze_inproc::exec::Executable {
    let code = emitted(func.view(), &[Via::Address]).unwrap_or_else(|why| panic!("{why}"));
    run::place_all(
        &code,
        |_: Fixup| callee,
        |to| match to {
            RelocTo::Routine(Routine::Memcpy) => memcpy_at(),
            other => panic!("{other:?}"),
        },
    )
}

fn memcpy_at() -> u64 {
    unsafe extern "C" {
        fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
    }
    memcpy as usize as u64
}

/// The emitted caller makes room, passes it in x8, and reads the callee's answer from it.
#[test]
fn an_sret_call_passes_x8() {
    let (mut b, v) = begin(&[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let callee_sig = sig(&mut b, &[SRET, Param::of(Ty::I64)], &[]);
    let callee = b.reference(RefKind::Func, 0);
    let slot = b.slot(32, 3, SlotKind::Temp);
    let room = b.slot_addr(slot);
    b.call(Flags::NONE, callee, callee_sig, &[room, v[0]]);
    let third = b.load(Ty::I64, Flags::NONE, room, 16, MemAttrs::aligned(3));
    let fourth = b.load(Ty::I64, Flags::NONE, room, 24, MemAttrs::aligned(3));
    let both = b.binary(Op::Add, Flags::NONE, third, fourth);
    b.ret(&[both]);
    let func = b.finish_checked().expect("builds");
    let exec = placed(&func, make_four as usize as u64);
    assert_eq!(run::call(&exec, [100, 0, 0, 0]), 102 + 103);
}

/// The emitted callee finds the return's room in x8.
#[test]
fn an_sret_parameter_arrives_in_x8() {
    let (mut b, v) = begin(&[SRET, Param::of(Ty::I64)], &[]);
    for k in 0..4 {
        let add = b.iconst(Ty::I64, 10 * k as u128);
        let field = b.binary(Op::Add, Flags::NONE, v[1], add);
        b.store(Flags::NONE, field, v[0], 8 * k, MemAttrs::aligned(3));
    }
    b.ret(&[]);
    let func = b.finish_checked().expect("builds");
    let exec = run::place(&emitted(func.view(), &[]).expect("emits"), |_| unreachable!());
    // SAFETY: an emitted C function of an `sret` pointer and an integer, as Rust calls
    // `extern "C" fn(u64) -> Four` (32 bytes: returned through x8).
    let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u64) -> Four>(exec.code_address() as usize) };
    assert_eq!(f(5), Four([5, 15, 25, 35]));
}

/// The emitted caller copies its value (inline for 32 bytes, by `memcpy` for 256) and passes
/// the copy: the callee's write to its copy leaves the caller's memory as it was.
#[test]
fn a_byval_call_passes_a_copy() {
    for (size, callee) in [(32u32, sum_four as usize as u64), (256, sum_wide as usize as u64)] {
        let (mut b, v) = begin(&[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        let callee_sig = sig(&mut b, &[by_value(size, 3)], &[Param::of(Ty::I64)]);
        let callee_ref = b.reference(RefKind::Func, 0);
        let sum = b.call(Flags::NONE, callee_ref, callee_sig, &[v[0]]);
        b.ret(&[sum]);
        let func = b.finish_checked().expect("builds");
        let code = emitted(func.view(), &[Via::Address]).expect("emits");
        let called = code.relocs.iter().any(|r| r.to == RelocTo::Routine(Routine::Memcpy));
        assert_eq!(called, u64::from(size) > crate::BULK_INLINE_MOST, "{size} bytes: copied by memcpy");
        let exec = placed(&func, callee);
        let mut value: Vec<u64> = (1..=u64::from(size) / 8).collect();
        let before = value.clone();
        let got = run::call(&exec, [value.as_mut_ptr() as u64, 0, 0, 0]);
        let want: u64 = match size {
            32 => value.iter().sum(),
            _ => value.iter().enumerate().map(|(i, &it)| it * (i as u64 + 1)).sum(),
        };
        assert_eq!(got, want, "{size} bytes");
        assert_eq!(value, before, "{size} bytes: the callee wrote only its copy");
    }
}

/// The emitted callee receives the copy's address in its argument register.
#[test]
fn a_byval_parameter_arrives_as_a_pointer() {
    let (mut b, v) = begin(&[Param::of(Ty::I64), by_value(32, 3)], &[Param::of(Ty::I64)]);
    let mut acc = v[0];
    for k in 0..4 {
        let field = b.load(Ty::I64, Flags::NONE, v[1], 8 * k, MemAttrs::aligned(3));
        acc = b.binary(Op::Add, Flags::NONE, acc, field);
    }
    b.ret(&[acc]);
    let func = b.finish_checked().expect("builds");
    let exec = run::place(&emitted(func.view(), &[]).expect("emits"), |_| unreachable!());
    // SAFETY: an emitted C function of an integer and a 32-byte `byval` struct, as Rust passes
    // `Four` (a pointer to a copy in x1).
    let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u64, Four) -> u64>(exec.code_address() as usize) };
    assert_eq!(f(1000, Four([1, 2, 3, 4])), 1010);
}

/// `call_indirect` calls the pointer it is given.
#[test]
fn call_indirect_calls_its_pointer() {
    let (mut b, v) = begin(&[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let callee_sig = sig(&mut b, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let out = b.call_indirect(Flags::NONE, v[0], callee_sig, &[v[1], v[2]]);
    b.ret(&[out]);
    let func = b.finish_checked().expect("builds");
    let code = emitted(func.view(), &[]).expect("emits");
    assert!(code.fixups.is_empty() && code.relocs.is_empty(), "no literal word: the callee is a value");
    let exec = run::place(&code, |_| unreachable!());
    assert_eq!(run::call(&exec, [sub as usize as u64, 50, 8, 0]), 42);
    assert_eq!(run::call(&exec, [sub as usize as u64, 0, 1, 0]), u64::MAX);
}

/// A call's two results are a pair; an emitted function returns two values in x0 and x1, each
/// narrowed to its type.
#[test]
fn two_returns_go_in_x0_and_x1_both_ways() {
    let (mut b, v) = begin(&[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let callee_sig = sig(&mut b, &[Param::of(Ty::I64)], &[Param::of(Ty::I64), Param::of(Ty::I64)]);
    let callee = b.reference(RefKind::Func, 0);
    let pair = b.call(Flags::NONE, callee, callee_sig, &[v[0]]);
    let (hi, lo) = (b.project(pair, 0), b.project(pair, 1));
    let k = b.iconst(Ty::I64, 1000);
    let hi = b.binary(Op::Mul, Flags::NONE, hi, k);
    let out = b.binary(Op::Add, Flags::NONE, hi, lo);
    b.ret(&[out]);
    let func = b.finish_checked().expect("builds");
    let exec = placed(&func, split as usize as u64);
    assert_eq!(run::call(&exec, [(7 << 32) | 9, 0, 0, 0]), 7009);

    let (mut b, v) = begin(&[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I32), Param::of(Ty::I8)]);
    let sum = b.binary(Op::Add, Flags::NONE, v[0], v[1]);
    let (wide, narrow) = (b.convert(Op::Trunc, Flags::NONE, Ty::I32, sum), b.convert(Op::Trunc, Flags::NONE, Ty::I8, v[1]));
    b.ret(&[wide, narrow]);
    let func = b.finish_checked().expect("builds");
    let exec = run::place(&emitted(func.view(), &[]).expect("emits"), |_| unreachable!());
    // SAFETY: an emitted C function of two integers returning two, in x0 and x1 as AAPCS64
    // returns this 16-byte struct.
    let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u64, u64) -> Two>(exec.code_address() as usize) };
    assert_eq!(f(0xffff_ffff, 0x1_0000_0102), Two(0x101, 0x02), "each narrowed to its type");
}

/// More than eight named integer arguments are built (pass/stack.rs; runs in stack_tests.rs:
/// judge-changed 2026-11-17 by lane TE, it said "refused" while the stack was not built); a
/// C-variadic DEFINITION stays refused by name; a
/// C-variadic CALL is built (pass/variadic.rs; its words and a run against `snprintf` are in
/// variadic_tests.rs). Judge-changed from "variadic calls are refused": that encoded the
/// tier's old gap, not the convention: AAPCS64 places a variadic call's arguments by stage C
/// as named ones, and Apple's arm64 ABI ("Writing ARM64 code for Apple platforms", "Update code
/// that passes arguments to variadic functions") places the variable ones on the stack, so the
/// call is defined on both. A definition (`unsafe extern "C" fn f(x: u64, mut args: ...)`, the
/// unstable `c_variadic`) needs its `va_list` built per platform, which no rung does yet.
#[test]
fn stack_arguments_and_variadic_definitions_are_refused() {
    let nine = [Param::of(Ty::I64); 9];
    let (mut b, v) = begin(&[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let callee_sig = sig(&mut b, &nine, &[Param::of(Ty::I64)]);
    let callee = b.reference(RefKind::Func, 0);
    let out = b.call(Flags::NONE, callee, callee_sig, &[v[0]; 9]);
    b.ret(&[out]);
    let func = b.finish_checked().expect("builds");
    let built = emitted(func.view(), &[Via::Address]);
    assert!(built.is_ok(), "nine integer arguments are built: {:?}", built.err());

    // A variadic callee given only its declared argument: the call is a plain one on both
    // platforms (only the `...` values move), and it is built.
    let (mut b, v) = begin(&[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let callee_sig = b.sig(Conv::C, Variadic::Yes, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let callee = b.reference(RefKind::Func, 0);
    let out = b.call(Flags::NONE, callee, callee_sig, &[v[0]]);
    b.ret(&[out]);
    let func = b.finish_checked().expect("builds");
    let built = emitted(func.view(), &[Via::Address]);
    assert!(built.is_ok(), "a variadic call is built: {:?}", built.err());

    // A function whose own signature is variadic.
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let own = b.sig(Conv::C, Variadic::Yes, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(own);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I64);
    b.ret(&[x]);
    let func = b.finish_checked().expect("builds");
    let refused = emitted(func.view(), &[Via::Address]).err();
    assert_eq!(
        refused,
        Some(EmitError::Signature { why: crate::pass::variadic::VARIADIC_DEFINITION }),
        "a variadic definition is refused by name"
    );
}
