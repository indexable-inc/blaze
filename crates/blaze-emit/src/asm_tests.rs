//! `asm!` in the baseline tier: the template reader (asm.rs) and the operands placed around it
//! (pass/asm.rs). The barrier forms are the sites tempo's dependencies hold (compact_str 0.9.1
//! `"/* {value} */"`, zeroize 1.9.1 `"# {}"`, zmij `"/*{:v}*/"` and `"/*{0:w}*/"`,
//! constant_time_eq 0.6.0 `"/* {0} */"`); the unknown-mnemonic case is the refusal's control.

use std::alloc::Global;

use blaze_lir::{
    Ty,
    builder::{AsmOperand, AsmPiece, RegKind},
    func::{AsmOperandKind, Func},
};

use crate::{
    EmitError, Isa,
    asm::{Assembler, Dialect},
    tests::{ISA, emitted_for, one},
};

/// The resolved words of one template, or its source diagnostic.
pub(crate) fn assembled(dialect: Dialect, text: &str) -> Result<Vec<u32>, crate::asm::AsmError> {
    let heap = blaze_own::testing::heap();
    let mut code = Vec::new();
    let mut reader = Assembler::new(dialect, &mut code, heap.alloc());
    reader.feed(text.as_bytes())?;
    reader.finish()?;
    assert_eq!(code.len() % 4, 0, "A64 instructions are whole words");
    Ok(code.chunks_exact(4).map(|w| u32::from_le_bytes(w.try_into().unwrap())).collect())
}

fn words(dialect: Dialect, text: &str) -> Result<usize, crate::asm::AsmError> {
    Ok(assembled(dialect, text)?.len())
}

/// Comments as LLVM's AArch64 assembler reads them make no instruction: `/* */` (across lines
/// too), `//`, `#` at a statement's start, `;` on Apple targets, and on ELF a `;` between two
/// comment-only statements.
///
/// Prints: `asm::comments: 0 words for every comment-only template`.
#[test]
fn comments_make_no_words() {
    let both = ["", "   ", "/* x9 */", "/*x0*/", "/* a\n b */", "// x0", "# x9", "  #x9", "/* v0 */\n// d\n# w3\n"];
    for text in both {
        assert_eq!(words(Dialect::Darwin, text).map_err(|e| e.why), Ok(0), "{text:?} on Apple");
        assert_eq!(words(Dialect::Elf, text).map_err(|e| e.why), Ok(0), "{text:?} on ELF");
    }
    assert_eq!(words(Dialect::Darwin, "; frob x0").map_err(|e| e.why), Ok(0), "`;` is Apple's comment");
    assert_eq!(words(Dialect::Elf, "/* a */ ; // b").map_err(|e| e.why), Ok(0), "`;` is ELF's separator");
    println!("asm::comments: 0 words for every comment-only template");
}

/// Any statement that is not blank is assembled or refused: a mnemonic the table lacks names
/// itself and its line; a `#` after a statement's first character is no comment; on ELF the
/// statement after `;` is read; an unterminated `/*` is refused.
///
/// Prints: `asm::refused: line 2: a mnemonic the baseline tier's assembler has no row for: `frob``.
#[test]
fn unknown_mnemonic_refused_by_name() {
    let e = words(Dialect::Darwin, "/* ok */\n  frob x0, #1").expect_err("`frob` is no AArch64 mnemonic");
    assert_eq!((e.line, e.token.text()), (2, "frob"));
    assert_eq!(e.why, "a mnemonic the baseline tier's assembler has no row for");
    let e = words(Dialect::Elf, "// a\n/* b */; frob").expect_err("ELF reads past `;`");
    assert_eq!((e.line, e.token.text()), (2, "frob"));
    let e = words(Dialect::Darwin, ".arch_extension crc").expect_err("a directive");
    assert_eq!(e.token.text(), ".arch_extension");
    let e = words(Dialect::Darwin, "2: frob").expect_err("a numeric label followed by an unknown instruction");
    assert_eq!((e.line, e.token.text(), e.why), (1, "frob", "a mnemonic the baseline tier's assembler has no row for"));
    let e = words(Dialect::Darwin, "named: frob").expect_err("only decimal local labels are supported");
    assert_eq!((e.line, e.token.text(), e.why), (1, "named:", "a label other than a decimal local label"));
    let e = words(Dialect::Darwin, "/* open").expect_err("an unterminated comment");
    assert_eq!(e.token.text(), "/*");
    let e = words(Dialect::Darwin, "/* a\n\n */ frob").expect_err("a comment across lines leaves its statement open");
    assert_eq!((e.line, e.token.text()), (3, "frob"));
    println!("asm::refused: line {}: {}: `frob`", 2, "a mnemonic the baseline tier's assembler has no row for");
}

/// `f(x) = asm(template, inout(class) x)` of `ty`.
fn barrier(ty: Ty, class: &str, template: &[AsmPiece<'_>]) -> Func<Global> {
    one(&[ty], ty, |b, p| {
        let operand = AsmOperand { kind: AsmOperandKind::InOut, reg: class, reg_kind: RegKind::Class, aux: 0 };
        let id = b.asm_block(template, &[operand], 0, "");
        b.asm(id, ty, &[p[0]])
    })
}

const OPERAND_0: AsmPiece<'static> = AsmPiece::Operand { index: 0, modifier: None };

#[test]
fn numeric_labels_choose_the_nearest_definition() {
    let text = "23: nop; b 23f; b 23b; 023:b 23b; b 23f; nop; 23:";
    let got = assembled(Dialect::Elf, text).expect("repeated numbers at distinct PCs are legal");
    assert_eq!(got, [0xd503201f, 0x14000002, 0x17fffffe, 0x14000000, 0x14000002, 0xd503201f]);
    println!("asm::labels: nearest forward, backward and same-PC targets");
}

#[test]
fn local_label_refusals_name_line_and_token() {
    let far = format!("\ntbz x3, #63, 8f\n{}8:", "nop\n".repeat(8191));
    let cases = [
        ("2:\nb 2f", 2, "2f", "no local label in the requested direction"),
        ("\nb 2b\n2:", 2, "2b", "no local label in the requested direction"),
        ("2:\n/* empty */\n02: nop", 3, "02:", "a local label defined twice at the same instruction address"),
        (far.as_str(), 2, "8f", "local label outside this branch's displacement range"),
        ("nop\nb name", 2, "name", "expected a decimal local label followed by `b` or `f`"),
        ("nop\ntbnz w2, #32, 2f\n2:", 2, "#32", "a tested bit outside the register width"),
    ];
    for (text, expected_line, expected_token, expected_why) in cases {
        let f = barrier(Ty::I64, "reg", &[AsmPiece::Text(text)]);
        let error = emitted_for(f.view(), &[], ISA).map(|_| ()).expect_err(expected_why);
        let EmitError::Asm { line, token, why, .. } = error else { panic!("expected a template diagnostic, got {error}") };
        assert_eq!((line, token.text(), why), (expected_line, expected_token, expected_why));
    }
    println!("asm::labels: all six refusals name their line and token");
}

#[test]
fn test_branch_displacement_limits() {
    let forward = format!("tbz x7, #63, 2f\n{}2:", "nop\n".repeat(8190));
    assert_eq!(assembled(Dialect::Darwin, &forward).unwrap()[0], 0xb6fbffe7);
    let back = format!("2:\n{}tbnz w1, #0, 2b", "nop\n".repeat(8192));
    assert_eq!(assembled(Dialect::Darwin, &back).unwrap()[8192], 0x37040001);
    let outside = format!("2:\n{}tbnz w1, #0, 2b", "nop\n".repeat(8193));
    let err = assembled(Dialect::Darwin, &outside).expect_err("one word below the signed 14-bit minimum");
    assert_eq!((err.line, err.token.text(), err.why), (8195, "2b", "local label outside this branch's displacement range"));
    println!("asm::tb: reaches -8192 and +8191 words");
}

/// The refusal reaches the emitter's caller as `EmitError::Asm`, naming the instruction, the
/// line and the mnemonic, with the placeholder rendered (`frob x9`'s token is `frob`).
///
/// Prints: `asm::emit refused: instruction 1 (`asm`): line 2 of its template is not assembled: ..`.
#[test]
fn emit_refuses_unknown_mnemonic() {
    let f = barrier(Ty::I64, "reg", &[AsmPiece::Text("// first\nfrob "), OPERAND_0]);
    let error = emitted_for(f.view(), &[], ISA).map(|_| ()).expect_err("`frob` is refused");
    let EmitError::Asm { inst: _, line, why, token } = error else { panic!("not the asm refusal: {error:?}") };
    assert_eq!((line, token.text(), why), (2, "frob", "a mnemonic the baseline tier's assembler has no row for"));
    println!("asm::emit refused: {error}");
}

/// An explicit register this tier does not save (or the platform's, the frame's, the stack's)
/// is refused by name.
///
/// Prints nothing.
#[test]
fn unsaved_registers_refused() {
    for reg in ["x19", "x20", "x18", "x29", "fp", "sp", "v8"] {
        let f = one(&[Ty::I64], Ty::I64, |b, p| {
            let operand = AsmOperand { kind: AsmOperandKind::InOut, reg, reg_kind: RegKind::Reg, aux: 0 };
            let id = b.asm_block(&[AsmPiece::Text("/* */")], &[operand], 0, "");
            b.asm(id, Ty::I64, &[p[0]])
        });
        let error = emitted_for(f.view(), &[], ISA).map(|_| ()).expect_err(reg);
        assert!(matches!(error, EmitError::Unsupported { .. }), "{reg}: {error:?}");
    }
}

/// The template's own bytes: the code of `body` less the code of the same block with an
/// empty template (the same operands, so the same loads and stores).
fn template_bytes(ty: Ty, class: &str, body: &[AsmPiece<'_>], isa: Isa) -> usize {
    let with = emitted_for(barrier(ty, class, body).view(), &[], isa).expect("the barrier emits");
    // The empty template is no piece (blaze-lir verify.rs refuses an empty text piece).
    let without = emitted_for(barrier(ty, class, &[]).view(), &[], isa).expect("the empty block emits");
    with.bytes.len() - without.bytes.len()
}

/// The barrier forms of the real sites: the value comes back unchanged and the template makes
/// no byte (the floor: an optimization barrier is no instruction).
///
/// Prints: `asm::barrier: template bytes 0 (floor 0) for each of 5 forms; x returned unchanged`.
#[cfg(target_arch = "aarch64")]
#[test]
fn barrier_returns_input_unchanged() {
    use crate::{Os, tests::run};

    let block = [AsmPiece::Text("/* "), OPERAND_0, AsmPiece::Text(" */")];
    let hash = [AsmPiece::Text("# "), OPERAND_0];
    let tight = [AsmPiece::Text("/*"), AsmPiece::Operand { index: 0, modifier: Some('v') }, AsmPiece::Text("*/")];
    let w = [AsmPiece::Text("/*"), AsmPiece::Operand { index: 0, modifier: Some('w') }, AsmPiece::Text("*/")];
    let forms: [(Ty, &str, &[AsmPiece<'_>]); 5] =
        [(Ty::I64, "reg", &block), (Ty::PTR, "reg", &hash), (Ty::I64, "vreg", &tight), (Ty::I32, "reg", &w), (Ty::I8, "vreg_low16", &block)];
    let xs = [0u64, 1, 0x8000_0000_0000_0000, 0xdead_beef_cafe_f00d, u64::MAX];
    for (ty, class, template) in forms {
        for isa in [Isa { lse: true, os: Os::MacOs }, Isa { lse: true, os: Os::Linux }] {
            assert_eq!(template_bytes(ty, class, template, isa), 0, "{class} {ty:?}: a comment is no instruction");
        }
        let code = emitted_for(barrier(ty, class, template).view(), &[], ISA).expect("the barrier emits");
        let exec = run::place(&code, |fixup| panic!("no literal word: {fixup:?}"));
        let bits = if ty == Ty::I32 {
            32
        } else if ty == Ty::I8 {
            8
        } else {
            64
        };
        for x in xs {
            assert_eq!(run::call(&exec, [x, 0, 0, 0]), x & crate::tests::mask(bits), "{class} {ty:?} on {x:#x}");
        }
    }
    println!("asm::barrier: template bytes 0 (floor 0) for each of 5 forms; x returned unchanged");
}
