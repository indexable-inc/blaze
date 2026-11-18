//! `asm!` blocks run as machine code (pass/asm.rs, asm/table.rs): what each observable row
//! computes, against a model of the instruction written from the Arm ARM's pseudocode (CRC32's
//! bitwise reflected division, the IEEE square root of the host), never from the emitter.

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;

use blaze_lir::{
    Ty, Value,
    builder::{AsmOperand, AsmPiece, RegKind},
    func::{AsmOperandKind, Func},
};

use crate::tests::{ISA, emitted_for, one, run};

const fn class(kind: AsmOperandKind, class: &'static str) -> AsmOperand<'static> {
    AsmOperand { kind, reg: class, reg_kind: RegKind::Class, aux: 0 }
}

const fn explicit(kind: AsmOperandKind, reg: &'static str) -> AsmOperand<'static> {
    AsmOperand { kind, reg, reg_kind: RegKind::Reg, aux: 0 }
}

const fn op(index: u32, modifier: Option<char>) -> AsmPiece<'static> {
    AsmPiece::Operand { index, modifier }
}

/// `f(params) = asm(template, operands)`, the block's inputs the parameters in order and its
/// one output of type `ret`.
fn block(params: &[Ty], ret: Ty, template: &[AsmPiece<'_>], operands: &[AsmOperand<'_>]) -> Func<Global> {
    one(params, ret, |b, p: &[Value]| {
        let id = b.asm_block(template, operands, 0, "");
        b.asm(id, ret, p)
    })
}

/// `func` emitted and placed, called on each of `cases`' arguments.
fn calls(func: &Func<Global>, cases: &[[u64; 4]]) -> Vec<u64> {
    calls_view(func.view(), cases)
}

fn calls_view(func: blaze_lir::View<'_>, cases: &[[u64; 4]]) -> Vec<u64> {
    let code = emitted_for(func, &[], ISA).unwrap_or_else(|e| panic!("the block emits: {e}"));
    let exec = run::place(&code, |fixup| panic!("no literal word: {fixup:?}"));
    cases.iter().map(|&args| run::call(&exec, args)).collect()
}

#[test]
fn backward_countdown_runs() {
    let f = block(&[Ty::I64], Ty::I64, &[AsmPiece::Text("2: sub x3, x3, #1\ncbnz x3, 2b")], &[explicit(AsmOperandKind::InOut, "x3")]);
    assert_eq!(calls(&f, &[[5, 0, 0, 0]]), [0]);
    println!("asm::countdown: 5 -> 0");
}

#[test]
fn forward_branch_runs() {
    let f = block(
        &[Ty::I64], Ty::I64, &[AsmPiece::Text("tbz x3, #32, 2f\nsub x3, x3, #1\n2:")],
        &[explicit(AsmOperandKind::InOut, "x3")],
    );
    assert_eq!(calls(&f, &[[7, 0, 0, 0], [1 << 32, 0, 0, 0]]), [7, u64::from(u32::MAX)]);
    println!("asm::forward: bit 32 selects skip or subtract");
}

#[test]
fn exclusive_increment_runs() {
    let f = one(&[Ty::PTR], Ty::I64, |b, params| {
        let id = b.asm_block(
            &[AsmPiece::Text("2: ldaxr x2, [x1]\nadd x2, x2, #1\nstlxr w4, x2, [x1]\ncbnz w4, 2b")],
            &[explicit(AsmOperandKind::In, "x1"), explicit(AsmOperandKind::Out, "x2"), explicit(AsmOperandKind::Out, "w4")],
            0,
            "",
        );
        let pair = b.compound(&[Ty::I64, Ty::I32]);
        let result = b.asm(id, pair, params);
        b.project(result, 0)
    });
    let code = emitted_for(f.view(), &[], ISA).expect("the exclusive increment emits");
    let exec = run::place(&code, |fixup| panic!("unexpected relocation {fixup:?}"));
    for initial in [0, 41, u64::MAX] {
        let mut counter = initial;
        let addr = (&raw mut counter).expose_provenance() as u64;
        let result = run::call(&exec, [addr, 0, 0, 0]);
        let expected = initial.wrapping_add(1);
        assert_eq!((result, counter), (expected, expected));
    }
    println!("asm::exclusive: 0 -> 1, 41 -> 42, u64::MAX -> 0");
}

/// CRC32 as the Arm ARM's `CRC32` pseudocode defines it: the accumulator and the data
/// bit-reflected, divided by `poly` (reflected: 0xedb88320 for CRC32, 0x82f63b78 for
/// CRC32C), no inversion on either side.
fn crc_model(acc: u32, data: u64, bytes: u32, poly: u32) -> u32 {
    let mut crc = acc;
    for k in 0..bytes {
        crc ^= (data >> (8 * k)) as u32 & 0xff;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (poly & (crc & 1).wrapping_neg());
        }
    }
    crc
}

/// `crc32b` and `crc32cx` run: the accumulator in and out of one register (`inout`), the
/// data in another (`in`), against the model.
///
/// Prints: `asm::crc32: crc32b(0xffffffff, 0x61) = 0x....; crc32cx matches on 4 cases`.
#[test]
fn crc32_runs() {
    let b = block(
        &[Ty::I32, Ty::I8],
        Ty::I32,
        &[AsmPiece::Text("crc32b "), op(0, Some('w')), AsmPiece::Text(", "), op(0, Some('w')), AsmPiece::Text(", "), op(1, Some('w'))],
        &[class(AsmOperandKind::InOut, "reg"), class(AsmOperandKind::In, "reg")],
    );
    let cases = [[0xffff_ffff, 0x61, 0, 0], [0, 0, 0, 0], [0x1234_5678, 0xff, 0, 0], [0xdead_beef, 0x80, 0, 0]];
    let got = calls(&b, &cases);
    for (args, got) in cases.iter().zip(&got) {
        let want = crc_model(args[0] as u32, args[1], 1, 0xedb8_8320);
        assert_eq!(*got, u64::from(want), "crc32b {:#x}, {:#x}", args[0], args[1]);
    }
    let cx = block(
        &[Ty::I32, Ty::I64],
        Ty::I32,
        &[AsmPiece::Text("crc32cx "), op(0, Some('w')), AsmPiece::Text(", "), op(0, Some('w')), AsmPiece::Text(", "), op(1, Some('x'))],
        &[class(AsmOperandKind::InOut, "reg"), class(AsmOperandKind::In, "reg")],
    );
    let cases = [[0xffff_ffff, 0x0123_4567_89ab_cdef, 0, 0], [0, 0, 0, 0], [1, u64::MAX, 0, 0], [0xcafe, 0x8000_0000_0000_0001, 0, 0]];
    for (args, got) in cases.iter().zip(calls(&cx, &cases)) {
        let want = crc_model(args[0] as u32, args[1], 8, 0x82f6_3b78);
        assert_eq!(got, u64::from(want), "crc32cx {:#x}, {:#x}", args[0], args[1]);
    }
    println!("asm::crc32: crc32b(0xffffffff, 0x61) = {:#x}; crc32cx matches on {} cases", got[0], cases.len());
}

/// `fsqrt` of a `d` and an `s` register (`inout(vreg)`, the value's bits in an integer): the
/// square root of 4.0 is 2.0, and each case is the host's correctly rounded root.
///
/// Prints: `asm::fsqrt: sqrt(4.0) = 2 (0x4000000000000000); sqrt(9.0f32) = 3`.
#[test]
fn fsqrt_runs() {
    let d = block(
        &[Ty::I64],
        Ty::I64,
        &[AsmPiece::Text("fsqrt "), op(0, Some('d')), AsmPiece::Text(", "), op(0, Some('d'))],
        &[class(AsmOperandKind::InOut, "vreg")],
    );
    let xs = [4.0f64, 2.0, 0.25, 1e300, 0.0];
    let got = calls(&d, &xs.map(|x| [x.to_bits(), 0, 0, 0]));
    for (x, got) in xs.iter().zip(&got) {
        assert_eq!(*got, x.sqrt().to_bits(), "fsqrt d of {x}");
    }
    assert_eq!(got[0], 2.0f64.to_bits(), "the square root of 4.0");
    let s = block(
        &[Ty::I32],
        Ty::I32,
        &[AsmPiece::Text("fsqrt "), op(0, Some('s')), AsmPiece::Text(", "), op(0, Some('s'))],
        &[class(AsmOperandKind::InOut, "vreg")],
    );
    let ys = [9.0f32, 2.0, 1e-30];
    let got_s = calls(&s, &ys.map(|y| [u64::from(y.to_bits()), 0, 0, 0]));
    for (y, got) in ys.iter().zip(&got_s) {
        assert_eq!(*got, u64::from(y.sqrt().to_bits()), "fsqrt s of {y}");
    }
    println!(
        "asm::fsqrt: sqrt(4.0) = {} ({:#x}); sqrt(9.0f32) = {}",
        f64::from_bits(got[0]),
        got[0],
        f32::from_bits(got_s[0] as u32)
    );
}

/// cmov's shape (cmov 0.4 aarch64.rs): `tst` of the condition's low byte, then `csel` on
/// `ne`, in a two-statement template; the result is `a` when the byte is not zero, else `b`.
///
/// Prints: `asm::tst+csel: 4 conditions, each selected as its low byte says`.
#[test]
fn tst_csel_runs() {
    let f = block(
        &[Ty::I64, Ty::I64, Ty::I64],
        Ty::I64,
        &[
            AsmPiece::Text("tst "),
            op(3, Some('w')),
            AsmPiece::Text(", 0xff\ncsel "),
            op(0, Some('x')),
            AsmPiece::Text(", "),
            op(1, Some('x')),
            AsmPiece::Text(", "),
            op(2, Some('x')),
            AsmPiece::Text(", NE"),
        ],
        &[
            class(AsmOperandKind::Out, "reg"),
            class(AsmOperandKind::In, "reg"),
            class(AsmOperandKind::In, "reg"),
            class(AsmOperandKind::In, "reg"),
        ],
    );
    let (a, b) = (0x1111_2222_3333_4444, 0xaaaa_bbbb_cccc_dddd);
    let conditions = [0u64, 1, 0x80, 0x100];
    let got = calls(&f, &conditions.map(|c| [a, b, c, 0]));
    for (c, got) in conditions.iter().zip(got) {
        let want = if c & 0xff != 0 { a } else { b };
        assert_eq!(got, want, "condition {c:#x}");
    }
    println!("asm::tst+csel: {} conditions, each selected as its low byte says", conditions.len());
}

/// An explicit register beside a class operand: the class never takes the named register,
/// so `eor x9, x9, {1}` with `inout("x9") x` gives `x ^ y` (a class operand put in x9 too
/// would make it 0).
///
/// Prints: `asm::explicit: x9 kept apart from the class operand`.
#[test]
fn explicit_register_kept_apart() {
    let f = block(
        &[Ty::I64, Ty::I64],
        Ty::I64,
        &[AsmPiece::Text("eor x9, x9, "), op(1, None)],
        &[explicit(AsmOperandKind::InOut, "x9"), class(AsmOperandKind::In, "reg")],
    );
    let cases = [[0xf0f0, 0x0ff0, 0, 0], [u64::MAX, 1, 0, 0], [0, 0, 0, 0]];
    for (args, got) in cases.iter().zip(calls(&f, &cases)) {
        assert_eq!(got, args[0] ^ args[1], "{:#x} ^ {:#x}", args[0], args[1]);
    }
    println!("asm::explicit: x9 kept apart from the class operand");
}

/// constant_time_eq's NEON compare (0.4 `neon.rs`: `cmeq {c:v}.16b, {a:v}.16b, {b:v}.16b`) on
/// `u8x16` operands: legalize passes each vector as its `i128` (blaze-lir legalize/vector.rs
/// `Out::asm`), the emitter loads it into a `q` register, and the result's lanes come back.
/// Each of the low 8 bytes is 0xff where the inputs' bytes are equal, else 0.
///
/// Prints: `asm::cmeq: 0x....; vector operands through legalize`.
#[test]
fn cmeq_on_vectors_runs() {
    let u8x16 = Ty::vector(blaze_lir::Scalar::I8, 16).expect("u8x16 is a LIR vector");
    let f = one(&[Ty::I64, Ty::I64], Ty::I64, |b, p: &[Value]| {
        let flags = blaze_lir::Flags::NONE;
        let mut vectors = [Value(0); 2];
        for (k, &x) in p.iter().enumerate() {
            let wide = b.convert(blaze_lir::Op::Zext, flags, Ty::I128, x);
            vectors[k] = b.convert(blaze_lir::Op::Bitcast, flags, u8x16, wide);
        }
        let id = b.asm_block(
            &[AsmPiece::Text("cmeq "), op(0, Some('v')), AsmPiece::Text(".16b, "), op(1, Some('v')), AsmPiece::Text(".16b, "), op(2, Some('v')), AsmPiece::Text(".16b")],
            &[class(AsmOperandKind::LateOut, "vreg"), class(AsmOperandKind::In, "vreg"), class(AsmOperandKind::In, "vreg")],
            0,
            "",
        );
        let c = b.asm(id, u8x16, &vectors);
        let wide = b.convert(blaze_lir::Op::Bitcast, flags, Ty::I128, c);
        b.convert(blaze_lir::Op::Trunc, flags, Ty::I64, wide)
    });
    let cases = [[0x0102_0304_0506_0708u64, 0x0102_0304_0506_0708, 0, 0], [0x00ff_00ff_00ff_00ff, 0x00ff_0000_00ff_0000, 0, 0], [0, u64::MAX, 0, 0]];
    let model = |a: u64, b: u64| (0..8).fold(0u64, |m, k| m | (u64::from((a >> (8 * k)) as u8 == (b >> (8 * k)) as u8) * 0xff) << (8 * k));
    let heap = blaze_own::testing::heap();
    let (legal, _) = crate::routines::legalized(f.view(), &crate::routines::RoutinePlaces::NONE, heap.alloc())
        .unwrap_or_else(|why| panic!("an `asm!` block on `u8x16` legalizes: {why}"));
    let legal = legal.expect("a function holding vectors is rewritten");
    let got = calls_view(legal.view(), &cases);
    for (args, got) in cases.iter().zip(&got) {
        assert_eq!(*got, model(args[0], args[1]), "cmeq of {:#x} and {:#x}", args[0], args[1]);
    }
    println!("asm::cmeq: {:#x}; vector operands through legalize", got[1]);
}

/// quanta's `mrs {}, cntvct_el0` (an output only): two reads, the second no earlier.
///
/// Prints: `asm::cntvct: two reads, nondecreasing`.
#[test]
fn cntvct_reads() {
    let f = block(&[], Ty::I64, &[AsmPiece::Text("mrs "), op(0, None), AsmPiece::Text(", cntvct_el0")], &[class(AsmOperandKind::Out, "reg")]);
    let got = calls(&f, &[[0; 4], [0; 4]]);
    assert!(got[0] != 0 && got[1] >= got[0], "cntvct_el0 read {got:x?}");
    println!("asm::cntvct: two reads, nondecreasing");
}
