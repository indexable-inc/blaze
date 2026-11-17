//! Arguments past `x7`/`v7` on the stack (pass/stack.rs), both sides, run on this Mac against
//! Rust's own `extern "C"` functions (rustc's Apple AAPCS64 is the witness): an emitted callee
//! called from Rust, an emitted caller calling Rust, the emitted caller calling the emitted
//! callee, and `sret` beside stacked arguments both ways. Each stacked set mixes an `i8`, an
//! `i128`, an `i32`, an `f32`, an `f64`, an `i16` and an `i64`, so Apple's packed slots (an
//! `i8` one byte, an `i128` 16-aligned) are each crossed. Linux's 8-byte slots are checked by
//! their offsets and the words the callee loads them with (no Linux host here).

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;

use blaze_lir::{
    Builder, Flags, Full, Op, Ty, Value,
    builder::{MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Ext, Func, ParamKind, RefKind, SlotKind},
};

use crate::{
    Fixup, Isa, Os, Via,
    pass::stack::{Nsaa, Stacked},
    tests::{emitted, emitted_for, run},
};

/// The 23 parameters: `x0..x7` and `v0..v7` full, then eight stacked.
const PARAMS: [Ty; 23] = [
    Ty::I64,
    Ty::I64,
    Ty::I64,
    Ty::I64,
    Ty::I64,
    Ty::I64,
    Ty::I64,
    Ty::I64,
    Ty::F64,
    Ty::F64,
    Ty::F64,
    Ty::F64,
    Ty::F64,
    Ty::F64,
    Ty::F64,
    Ty::F64,
    // Stacked (Apple offsets): i8 @0, i128 @16, i32 @32, f32 @36, f64 @40, i16 @48, i64 @56,
    // i64 @64 (Linux: 0, 16, 32, 40, 48, 56, 64, 72).
    Ty::I8,
    Ty::I128,
    Ty::I32,
    Ty::F32,
    Ty::F64,
    Ty::I16,
    Ty::I64,
];

type Many = extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64, f64, f64, f64, f64, f64, f64, f64, f64, i8, i128, i32, f32, f64, i16, i64) -> i64;

const fn mix(h: u64, x: u64) -> u64 {
    h.wrapping_mul(31).wrapping_add(x)
}

/// What the emitted callee computes, in Rust: every stacked value and a register value of
/// each class, each bit of each one.
#[allow(clippy::too_many_arguments)]
extern "C" fn rust_many(
    a0: i64,
    _a1: i64,
    _a2: i64,
    _a3: i64,
    _a4: i64,
    _a5: i64,
    _a6: i64,
    a7: i64,
    _f0: f64,
    _f1: f64,
    _f2: f64,
    _f3: f64,
    _f4: f64,
    _f5: f64,
    _f6: f64,
    f7: f64,
    s: i8,
    w: i128,
    t: i32,
    g: f32,
    h: f64,
    u: i16,
    k: i64,
) -> i64 {
    let mut acc = a0 as u64;
    for x in [
        a7 as u64,
        f7.to_bits(),
        s as i64 as u64,
        w as u64,
        (w as u128 >> 64) as u64,
        t as i64 as u64,
        u64::from(g.to_bits()),
        h.to_bits(),
        u64::from(u as u16),
        k as u64,
    ] {
        acc = mix(acc, x);
    }
    acc as i64
}

/// Arguments with every bit of each stacked one set somewhere (and the sign of the small ones).
const S: i8 = -3;
const W: i128 = -0x0123_4567_89ab_cdef_fedc_ba98_7654_3211;
const T: i32 = -0x1234_5679;
const G: f32 = -1.5e-3;
const H: f64 = 6.02e23;
const U: i16 = -2;
const K: i64 = 0x7766_5544_3322_1100;

fn want(a0: i64) -> i64 {
    rust_many(a0, 1, 2, 3, 4, 5, 6, 7, 0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, S, W, T, G, H, U, K)
}

fn sig_params() -> Vec<Param> {
    PARAMS.iter().map(|&ty| Param::of(ty)).collect()
}

/// The emitted twin of [`rust_many`].
fn callee() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &sig_params(), &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let v: Vec<Value> = PARAMS.iter().map(|&ty| b.param(ty)).collect();
    let c31 = b.iconst(Ty::I64, 31);
    let sixty_four = b.iconst(Ty::I128, 64);
    let f7 = b.convert(Op::Bitcast, Flags::NONE, Ty::I64, v[15]);
    let s = b.convert(Op::Sext, Flags::NONE, Ty::I64, v[16]);
    let w_lo = b.convert(Op::Trunc, Flags::NONE, Ty::I64, v[17]);
    let w_hi = b.binary(Op::Lshr, Flags::NONE, v[17], sixty_four);
    let w_hi = b.convert(Op::Trunc, Flags::NONE, Ty::I64, w_hi);
    let t = b.convert(Op::Sext, Flags::NONE, Ty::I64, v[18]);
    let g = b.convert(Op::Bitcast, Flags::NONE, Ty::I32, v[19]);
    let g = b.convert(Op::Zext, Flags::NONE, Ty::I64, g);
    let h = b.convert(Op::Bitcast, Flags::NONE, Ty::I64, v[20]);
    let u = b.convert(Op::Zext, Flags::NONE, Ty::I64, v[21]);
    let mut acc = v[0];
    for x in [v[7], f7, s, w_lo, w_hi, t, g, h, u, v[22]] {
        let times = b.binary(Op::Mul, Flags::NONE, acc, c31);
        acc = b.binary(Op::Add, Flags::NONE, times, x);
    }
    b.ret(&[acc]);
    b.finish_checked().expect("builds")
}

/// `caller(a0) = many(a0, 1..7, 0.5..7.5, S, W, T, G, H, U, K)`, `many` its one literal.
fn caller() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let a0 = b.param(Ty::I64);
    let callee_sig = b.sig(Conv::C, Variadic::No, &sig_params(), &[Param::of(Ty::I64)]);
    let callee = b.reference(RefKind::Func, 0);
    let mut args = vec![a0];
    for k in 1..8u128 {
        args.push(b.iconst(Ty::I64, k));
    }
    for k in 0..8 {
        args.push(b.fconst(Ty::F64, u128::from((k as f64 + 0.5).to_bits())));
    }
    args.push(b.iconst(Ty::I8, u128::from(S as u8)));
    args.push(b.iconst(Ty::I128, W as u128));
    args.push(b.iconst(Ty::I32, u128::from(T as u32)));
    args.push(b.fconst(Ty::F32, u128::from(G.to_bits())));
    args.push(b.fconst(Ty::F64, u128::from(H.to_bits())));
    args.push(b.iconst(Ty::I16, u128::from(U as u16)));
    args.push(b.iconst(Ty::I64, K as u64 as u128));
    let got = b.call(Flags::NONE, callee, callee_sig, &args);
    b.ret(&[got]);
    b.finish_checked().expect("builds")
}

fn placed(func: &Func<Global>, callee: u64) -> blaze_inproc::exec::Executable {
    let code = emitted(func.view(), &[Via::Address]).unwrap_or_else(|why| panic!("{why}\n{}", blaze_lir::print(func.view())));
    run::place(&code, |_: Fixup| callee)
}

const A0: [i64; 3] = [0, -1, 0x0bad_cafe_f00d];

/// Rust calls the emitted callee: each stacked parameter read from `x29 + 16` at Apple's
/// packed offsets, and every register one where it was.
///
/// Expected: `test stack_tests::an_emitted_callee_reads_its_stacked_parameters ... ok`.
#[test]
fn an_emitted_callee_reads_its_stacked_parameters() {
    let func = callee();
    let code = emitted(func.view(), &[]).unwrap_or_else(|why| panic!("{why}\n{}", blaze_lir::print(func.view())));
    let exec = run::place(&code, |_| unreachable!("no literal words"));
    // SAFETY: an emitted C function of `Many`'s signature (`callee`), mapped while `exec` lives;
    // it reads only its own frame and the caller's stacked area.
    let f = unsafe { std::mem::transmute::<usize, Many>(exec.code_address() as usize) };
    for a0 in A0 {
        let got = f(a0, 1, 2, 3, 4, 5, 6, 7, 0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, S, W, T, G, H, U, K);
        assert_eq!(got, want(a0), "a0 {a0}");
    }
    // A control: another stacked `i128` gives another answer (the test sees the stack).
    let other = f(0, 1, 2, 3, 4, 5, 6, 7, 0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, S, W ^ (1 << 100), T, G, H, U, K);
    assert_ne!(other, want(0));
}

/// The emitted caller calls Rust's `rust_many`: Rust reads what the caller stacked.
///
/// Expected: `test stack_tests::an_emitted_caller_stacks_its_arguments ... ok`.
#[test]
fn an_emitted_caller_stacks_its_arguments() {
    let exec = placed(&caller(), rust_many as usize as u64);
    for a0 in A0 {
        assert_eq!(run::call(&exec, [a0 as u64, 0, 0, 0]) as i64, want(a0), "a0 {a0}");
    }
}

/// The emitted caller calls the emitted callee: both sides of blaze's own layout agree.
///
/// Expected: `test stack_tests::emitted_caller_and_callee_agree ... ok`.
#[test]
fn emitted_caller_and_callee_agree() {
    let func = callee();
    let code = emitted(func.view(), &[]).unwrap_or_else(|why| panic!("{why}"));
    let callee = run::place(&code, |_| unreachable!("no literal words"));
    let exec = placed(&caller(), callee.code_address());
    for a0 in A0 {
        assert_eq!(run::call(&exec, [a0 as u64, 0, 0, 0]) as i64, want(a0), "a0 {a0}");
    }
}

/// 32 bytes: returned through `x8`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Four([u64; 4]);

#[allow(clippy::too_many_arguments)]
extern "C" fn rust_nine(a0: u64, a1: u64, _a2: u64, _a3: u64, _a4: u64, _a5: u64, _a6: u64, a7: u64, a8: u64) -> Four {
    Four([a8, a0, a7.wrapping_mul(3), a1 ^ a8])
}

const SRET: Param = Param { kind: ParamKind::Sret, ..Param::of(Ty::PTR) };

/// `sret` in `x8` beside a ninth integer on the stack, both ways: Rust calls an emitted
/// `nine` that writes `rust_nine`'s answer, and an emitted caller calls Rust's `rust_nine`.
///
/// Expected: `test stack_tests::sret_beside_a_stacked_argument_both_ways ... ok`.
#[test]
fn sret_beside_a_stacked_argument_both_ways() {
    let mut nine = vec![SRET];
    nine.extend([Param::of(Ty::I64); 9]);
    // The callee: `*sret = [a8, a0, a7 * 3, a1 ^ a8]`.
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &nine, &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let out = b.param(Ty::PTR);
    let a: Vec<Value> = (0..9).map(|_| b.param(Ty::I64)).collect();
    let three = b.iconst(Ty::I64, 3);
    let a7 = b.binary(Op::Mul, Flags::NONE, a[7], three);
    let a1 = b.binary(Op::Xor, Flags::NONE, a[1], a[8]);
    for (k, value) in [a[8], a[0], a7, a1].into_iter().enumerate() {
        b.store(Flags::NONE, value, out, 8 * k as i32, MemAttrs::aligned(3));
    }
    b.ret(&[]);
    let func = b.finish_checked().expect("builds");
    let code = emitted(func.view(), &[]).unwrap_or_else(|why| panic!("{why}"));
    let exec = run::place(&code, |_| unreachable!("no literal words"));
    type Nine = extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64) -> Four;
    // SAFETY: an emitted C function of `Nine`'s signature, mapped while `exec` lives; it writes
    // only the 32 bytes `x8` points at, which the Rust caller provides.
    let f = unsafe { std::mem::transmute::<usize, Nine>(exec.code_address() as usize) };
    assert_eq!(f(10, 11, 12, 13, 14, 15, 16, 17, 1000), rust_nine(10, 11, 12, 13, 14, 15, 16, 17, 1000));

    // The caller: `caller(p) = { r = rust_nine(p, 1..7, 1000); r[0] + r[3] }`.
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let p = b.param(Ty::I64);
    let callee_sig = b.sig(Conv::C, Variadic::No, &nine, &[]);
    let callee = b.reference(RefKind::Func, 0);
    let slot = b.slot(32, 3, SlotKind::Temp);
    let room = b.slot_addr(slot);
    let mut args = vec![room, p];
    for k in 1..8u128 {
        args.push(b.iconst(Ty::I64, k));
    }
    args.push(b.iconst(Ty::I64, 1000));
    b.call(Flags::NONE, callee, callee_sig, &args);
    let first = b.load(Ty::I64, Flags::NONE, room, 0, MemAttrs::aligned(3));
    let last = b.load(Ty::I64, Flags::NONE, room, 24, MemAttrs::aligned(3));
    let both = b.binary(Op::Add, Flags::NONE, first, last);
    b.ret(&[both]);
    let func = b.finish_checked().expect("builds");
    let exec = placed(&func, rust_nine as usize as u64);
    let r = rust_nine(77, 1, 2, 3, 4, 5, 6, 7, 1000);
    assert_eq!(run::call(&exec, [77, 0, 0, 0]), r.0[0].wrapping_add(r.0[3]));
}

/// The stacked offsets of [`PARAMS`]'s stacked tail: Apple packs each at its natural size and
/// alignment, Linux gives each at least 8 (AAPCS64 C.14..C.17), both 16-align an `i128`.
///
/// Expected: `test stack_tests::stacked_offsets_per_os ... ok`.
#[test]
fn stacked_offsets_per_os() {
    let tail = &PARAMS[16..];
    let walk = |os: Os| -> Vec<Stacked> {
        let mut n = Nsaa::new(os);
        tail.iter().map(|&ty| n.place(ty).expect("a scalar")).collect()
    };
    let at = |v: Vec<Stacked>| v.iter().map(|it| (it.at, it.bytes)).collect::<Vec<_>>();
    assert_eq!(at(walk(Os::MacOs)), [(0, 1), (16, 16), (32, 4), (36, 4), (40, 8), (48, 2), (56, 8)]);
    assert_eq!(at(walk(Os::Linux)), [(0, 8), (16, 16), (32, 8), (40, 8), (48, 8), (56, 8), (64, 8)]);
}

/// The callee's first stacked loads, against the words `clang -c -arch arm64` assembles:
/// Apple `ldurb w9, [x29, #16]` (the `i8`), Linux `ldur x9, [x29, #16]` (its 8-byte slot);
/// then the `i128`'s two words from `[x29, #32]` and `[x29, #40]` on both.
///
/// Expected: `test stack_tests::the_callee_loads_its_slots_by_the_os ... ok`.
#[test]
fn the_callee_loads_its_slots_by_the_os() {
    const LDURB_W9_FP_16: u32 = 0x3841_03a9;
    const LDUR_X9_FP_16: u32 = 0xf841_03a9;
    const LDUR_X9_FP_32: u32 = 0xf842_03a9;
    const LDUR_X9_FP_40: u32 = 0xf842_83a9;
    let func = callee();
    let words = |os: Os| -> Vec<u32> {
        let code = emitted_for(func.view(), &[], Isa { lse: true, os }).unwrap_or_else(|why| panic!("{why}"));
        code.bytes.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
    };
    let (mac, linux) = (words(Os::MacOs), words(Os::Linux));
    assert!(mac.contains(&LDURB_W9_FP_16) && !mac.contains(&LDUR_X9_FP_16), "Apple: the `i8` is one byte");
    assert!(linux.contains(&LDUR_X9_FP_16) && !linux.contains(&LDURB_W9_FP_16), "Linux: its slot is 8 bytes");
    for words in [&mac, &linux] {
        assert!(words.contains(&LDUR_X9_FP_32) && words.contains(&LDUR_X9_FP_40), "the `i128` at 16..32 of the area");
    }
}

/// A sign-extended stacked `i8` on Linux fills its 8-byte slot with the sign: `sxtb x9, w9`
/// (`sbfm x9, x9, #0, #7`, 0x93401d29) before `str x9, [sp]` (0xf90003e9). On Apple the slot
/// is the byte: `strb w9, [sp]` (0x390003e9).
///
/// Expected: `test stack_tests::a_signed_stacked_byte_fills_its_linux_slot ... ok`.
#[test]
fn a_signed_stacked_byte_fills_its_linux_slot() {
    let mut params = vec![Param::of(Ty::I64); 8];
    params.push(Param { ext: Ext::Sext, ..Param::of(Ty::I8) });
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I8)], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I8);
    let callee_sig = b.sig(Conv::C, Variadic::No, &params, &[]);
    let callee = b.reference(RefKind::Func, 0);
    let mut args: Vec<Value> = (0..8u128).map(|k| b.iconst(Ty::I64, k)).collect();
    args.push(x);
    b.call(Flags::NONE, callee, callee_sig, &args);
    b.ret(&[]);
    let func = b.finish_checked().expect("builds");
    let words = |os: Os| -> Vec<u32> {
        let code = emitted_for(func.view(), &[Via::Address], Isa { lse: true, os }).unwrap_or_else(|why| panic!("{why}"));
        code.bytes.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
    };
    let (mac, linux) = (words(Os::MacOs), words(Os::Linux));
    const SUB_SP_16: u32 = 0xd100_43ff;
    const ADD_SP_16: u32 = 0x9100_43ff;
    const SXTB: u32 = 0x9340_1d29;
    const STR_X9_SP: u32 = 0xf900_03e9;
    const STRB_W9_SP: u32 = 0x3900_03e9;
    // (The entry's `str x9, [sp]` is the parameter's own cell; the slot's store follows the open.)
    let at = |words: &[u32], w: u32, from: usize| words[from..].iter().position(|&it| it == w).map(|it| it + from);
    let open = at(&linux, SUB_SP_16, 0).expect("Linux opens a 16-byte area");
    let sxtb = at(&linux, SXTB, open).expect("then sign-extends");
    let store = at(&linux, STR_X9_SP, sxtb).expect("then stores 8 bytes");
    assert_eq!((sxtb, store), (open + 2, open + 3), "Linux: open, load, extend, store");
    assert!(at(&linux, ADD_SP_16, store).is_some(), "closed after the call");
    let open = at(&mac, SUB_SP_16, 0).expect("Apple opens a 16-byte area");
    assert!(at(&mac, STRB_W9_SP, open).is_some(), "Apple: the byte alone");
    assert!(at(&mac, STR_X9_SP, open).is_none(), "and no 8-byte store into the area");
}
