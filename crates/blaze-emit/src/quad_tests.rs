//! `f128` and `f16` in the Reference tier (pass/float_width.rs): the new instruction forms
//! against words written by hand from their encoding diagrams (Arm ARM DDI 0487, C7.2; none
//! from an assembler), and functions emitted and run as machine code in this process.
//!
//! What they rest on: AAPCS64, "Parameter passing", rule C.1 (a half-, single-, double- or
//! quad-precision floating-point argument goes in the least significant bits of `v[NSRN]`;
//! a result as the first argument would go: `v0`); blaze-lir's legalize (legalize/mod.rs:
//! "every `f128` operation becomes a call to the pinned compiler_builtins' `#[no_mangle]`
//! routine for it"; what is left is moves, and `fpext f16 -> f32`, `fptrunc f32 -> f16`).
//!
//! Run with: `lanecheck.sh blaze-emit test quad_tests` (filter) — prints
//! `test blaze-emit: exit 0; 4 passed, 0 failed` on an arm64 Mac.

use std::alloc::Global;

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::Conv;
use blaze_lir::{BlockKind, Builder, Flags, Full, Func, Op, Ty};

use crate::enc;
use crate::tests::ISA;
use crate::{Code, EmitError, emit};

/// `fn(a: f128, b: f128) -> f128 { a + b }`, with no reference of its own.
fn add_f128() -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let q = Param::of(Ty::F128);
    let sig = b.sig(Conv::C, Variadic::No, &[q, q], &[q]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::F128), b.param(Ty::F128));
    let sum = b.binary(Op::Fadd, Flags::NONE, x, y);
    b.ret(&[sum]);
    b.finish_checked().expect("the f128 add is valid LIR")
}

/// `fn(x: from) -> to { op x }` for a float conversion `op`.
#[cfg(target_arch = "aarch64")]
fn convert(op: Op, from: Ty, to: Ty) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(from)], &[Param::of(to)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(from);
    let out = b.convert(op, Flags::NONE, to, x);
    b.ret(&[out]);
    b.finish_checked().expect("the float conversion is valid LIR")
}

/// The code's words, in order (the literal words after the code read as two each).
#[cfg(target_arch = "aarch64")]
fn words(code: &Code<Global>) -> std::vec::Vec<u32> {
    code.bytes.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
}

/// The new forms, each spelled as binary, one `_`-separated group per field of its diagram.
///
/// Prints: `test quad_tests::quad_and_half_encodings ... ok`
#[test]
fn quad_and_half_encodings() {
    let cases: &[(u32, u32, &str)] = &[
        // C7.2 LDR (immediate, SIMD&FP), unsigned offset: size=00 111 1 01 opc=11 imm12 Rn Rt
        // (opc 11 with size 00 is the 128-bit `q` form; imm12 is scaled by 16).
        (enc::ldr_q(0, 31, 1), 0b00_111_1_01_11_000000000001_11111_00000, "ldr q0, [sp, #16]"),
        // C7.2 STR (immediate, SIMD&FP), unsigned offset: opc=10.
        (enc::str_q(1, 31, 2), 0b00_111_1_01_10_000000000010_11111_00001, "str q1, [sp, #32]"),
        (enc::str_q(3, 12, 0), 0b00_111_1_01_10_000000000000_01100_00011, "str q3, [x12]"),
        // C7.2 UMOV: 0 Q=0 op=0 01110000 imm5=00010 (the `h` lane 0) 0 imm4=0111 1 Rn Rd.
        (enc::umov_h0(9, 2), 0b0_0_0_01110000_00010_0_0111_1_00010_01001, "umov w9, v2.h[0]"),
        // C7.2 FCVT: M=0 0 S=0 11110 ftype 1 0001 opc 10000 Rn Rd; ftype 11 half, 00 single,
        // 01 double (the source); opc 00 single, 01 double, 11 half (the destination).
        (enc::fcvt_from_h(false, 0, 0), 0b0_0_0_11110_11_1_0001_00_10000_00000_00000, "fcvt s0, h0"),
        (enc::fcvt_from_h(true, 0, 1), 0b0_0_0_11110_11_1_0001_01_10000_00001_00000, "fcvt d0, h1"),
        (enc::fcvt_to_h(false, 0, 0), 0b0_0_0_11110_00_1_0001_11_10000_00000_00000, "fcvt h0, s0"),
        (enc::fcvt_to_h(true, 2, 0), 0b0_0_0_11110_01_1_0001_11_10000_00000_00010, "fcvt h2, d0"),
    ];
    for &(got, want, text) in cases {
        assert_eq!(got, want, "{text}: {got:#010x} != {want:#010x}");
    }
}

/// An `f128` add given to the literal-word form is legalized there as in the record form, and
/// with no routine bound it is refused naming `__addtf3` (rule 3: never computed here, never a
/// fallback). Judge-changed 2026-11-17 by lane TE: it expected the pass's own `fadd` refusal,
/// from when `emit` skipped legalize.
///
/// Prints: `test quad_tests::unlegalized_f128_arithmetic_is_refused_by_name ... ok`
#[test]
fn unlegalized_f128_arithmetic_is_refused_by_name() {
    let mut code = Code::new_in(Global);
    let func = add_f128();
    let refused = emit(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &[], ISA, &mut code);
    assert!(
        matches!(refused, Err(EmitError::Legalize(blaze_lir::legalize::LegalizeError::NoRoutine(r))) if r.symbol() == "__addtf3"),
        "an unlegalized f128 add: {refused:?}"
    );
}

/// The legalized `f128` add (legalize: one call of `__addtf3`, routines.rs) emitted for arm64:
/// its parameters arrive in `q0`, `q1` and are stored whole to their cells; the call loads
/// them back into `q0`, `q1` in order and calls through the one literal word, `__addtf3`'s
/// place; the call's result is stored whole from `q0`, and the return loads it into `q0`.
/// Then run, with a stand-in for `__addtf3` that subtracts lane by lane (`sub v0.2d, v0.2d,
/// v1.2d`: an argument order swapped, a lane lost or a result read from another register
/// gives another answer).
///
/// Prints: `test quad_tests::an_f128_add_calls_addtf3_in_q_registers ... ok`
#[cfg(target_arch = "aarch64")]
#[test]
fn an_f128_add_calls_addtf3_in_q_registers() {
    use blaze_lir::legalize::SoftFloat;
    use blaze_own::Alloc;

    use crate::Via;
    use crate::routines::{RoutinePlaces, legalized};

    let mut places = RoutinePlaces::NONE;
    places.bind(SoftFloat::AddTf3, 0);
    let heap = blaze_own::testing::heap();
    let alloc: Alloc<'_> = heap.alloc();
    let func = add_f128();
    let (legal, called) =
        legalized(func.view(), &places, alloc).unwrap_or_else(|why| panic!("an f128 add with `__addtf3` bound legalizes: {why}"));
    assert!(called.contains(SoftFloat::AddTf3), "{called:?}");
    let legal = legal.expect("an f128 add is rewritten");
    let mut code = Code::new_in(Global);
    emit(blaze_lir::verify(legal.view()).expect("the legalized function is LIR"), &[Via::Address], ISA, &mut code)
        .unwrap_or_else(|why| panic!("the legalized f128 add emits on the Reference tier: {why:?}"));
    assert_eq!(code.fixups.len(), 1, "one literal word: __addtf3's");
    assert_eq!(code.fixups[0].place, 0);

    // The words: the scaled immediate masked off, `ldr`/`str q<n>, [sp, #_]` are these.
    let w = words(&code);
    let imm_mask = 0xfff << 10;
    let is = |word: u32, form: u32| word & !imm_mask == form;
    let str_q = |v: u32| enc::str_q(v, 31, 0);
    let ldr_q = |v: u32| enc::ldr_q(v, 31, 0);
    let blr = w.iter().position(|&it| it == enc::blr(16)).expect("a `blr x16`: the call");
    let ret = w.iter().position(|&it| it == enc::ret()).expect("a `ret`");
    // Parameters 0 and 1 (cells at 0 and 16) stored from q0 and q1.
    assert!(w[..blr].contains(&enc::str_q(0, 31, 0)), "str q0, [sp] in the prologue");
    assert!(w[..blr].contains(&enc::str_q(1, 31, 1)), "str q1, [sp, #16] in the prologue");
    // The arguments loaded back, a into q0 and b into q1.
    assert!(w[..blr].contains(&enc::ldr_q(0, 31, 0)), "ldr q0, [sp] before the call");
    assert!(w[..blr].contains(&enc::ldr_q(1, 31, 1)), "ldr q1, [sp, #16] before the call");
    // The result stored from q0 right after the call, and loaded into q0 for the return.
    let after = w[blr + 1];
    assert!(is(after, str_q(0)), "the call's result stored from q0: {after:#010x}");
    let cell = (after & imm_mask) >> 10;
    assert!(w[blr..ret].contains(&enc::ldr_q(0, 31, cell)), "the return loads q0 from the call's cell {cell}");
    // No `f128` moved through the `x` registers' `fmov`s (a q register is never split).
    assert!(!w[..ret].iter().any(|&it| is(it, ldr_q(2)) || is(it, str_q(2))), "only q0 and q1 move");

    use core::arch::{asm, naked_asm};

    /// The stand-in: `q0 = q0 - q1`, each 64-bit lane apart.
    #[unsafe(naked)]
    extern "C" fn stand_in() {
        naked_asm!("sub v0.2d, v0.2d, v1.2d", "ret")
    }

    let exec = crate::tests::run::place(&code, |fixup| {
        assert_eq!(fixup.place, 0);
        stand_in as usize as u64
    });
    let at = exec.code_address();
    let cases: [([u64; 2], [u64; 2]); 3] = [
        ([10, 100], [3, 7]),
        ([0, 0x8000_0000_0000_0000], [1, 1]),
        ([0x3fff_8000_0000_0000, 0x4000_0000_dead_beef], [0x1234_5678, 0xffff_ffff_ffff_ffff]),
    ];
    for (a, b) in cases {
        let (lo, hi): (u64, u64);
        // SAFETY: `at` is an emitted C function `(f128, f128) -> f128`, mapped executable while
        // `exec` lives, its literal word holding `stand_in`'s address; AAPCS64 C.1 passes the two
        // in q0 and q1 and returns in q0, which the asm fills and reads; `clobber_abi("C")`
        // names every register the call may change (x0-x17, x30, v0-v31's caller-saved parts),
        // and the stack is the caller's, 16-byte aligned.
        unsafe {
            asm!(
                "fmov d0, {a0}",
                "mov v0.d[1], {a1}",
                "fmov d1, {b0}",
                "mov v1.d[1], {b1}",
                "blr {f}",
                "fmov x9, d0",
                "mov x10, v0.d[1]",
                a0 = in(reg) a[0],
                a1 = in(reg) a[1],
                b0 = in(reg) b[0],
                b1 = in(reg) b[1],
                f = in(reg) at,
                // Explicit registers: `clobber_abi` refuses generic outputs.
                out("x9") lo,
                out("x10") hi,
                clobber_abi("C"),
            );
        }
        assert_eq!([lo, hi], [a[0].wrapping_sub(b[0]), a[1].wrapping_sub(b[1])], "{a:x?} - {b:x?}");
    }
}

/// `f16` passed and returned in `h` registers, and converted by `fcvt`: `fpext f16 -> f32`
/// of 1.0 (0x3c00), 65504 (0x7bff, the largest finite) and -0.5 (0xb800) is exact; `fptrunc
/// f32 -> f16` of 1/3 rounds to 0x3555 (to nearest: 0.333251953125), of 1e5 to infinity
/// (0x7c00, past 65520), of 65519 to 0x7bff (below the halfway 65520). IEEE 754 binary16:
/// 1 sign, 5 exponent (bias 15), 10 fraction bits. An `f16` in `s0`'s low 16 bits is `h0`
/// (AAPCS64 C.1: "the least significant bits").
///
/// Prints: `test quad_tests::f16_moves_and_converts_in_h_registers ... ok`
#[cfg(target_arch = "aarch64")]
#[test]
fn f16_moves_and_converts_in_h_registers() {
    let call = |func: &Func<Global>, x: f32| -> f32 {
        let code = crate::tests::emitted(func.view(), &[]).unwrap_or_else(|why| panic!("{why:?}"));
        let exec = crate::tests::run::place(&code, |_| unreachable!("no literal word"));
        // SAFETY: the code is an emitted C function of one float parameter and one float
        // result, both in v0 (AAPCS64 C.1), mapped executable while `exec` lives.
        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(f32) -> f32>(exec.code_address() as usize) };
        f(x)
    };
    let ext = convert(Op::Fpext, Ty::F16, Ty::F32);
    for (half, want) in [(0x3c00u32, 1.0f32), (0x7bff, 65504.0), (0xb800, -0.5)] {
        // Garbage above the 16 bits: AAPCS64 leaves them unspecified.
        let got = call(&ext, f32::from_bits(half | 0x5a5a_0000));
        assert_eq!(got.to_bits(), want.to_bits(), "fpext {half:#06x}");
    }
    let trunc = convert(Op::Fptrunc, Ty::F32, Ty::F16);
    for (x, want) in [(1.0f32 / 3.0, 0x3555u32), (1e5, 0x7c00), (65519.0, 0x7bff), (-0.5, 0xb800)] {
        let got = call(&trunc, x).to_bits() & 0xffff;
        assert_eq!(got, want, "fptrunc {x}");
    }
}
