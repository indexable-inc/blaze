//! P11's test (port27/tempo-run/INTRINSICS.md): the inline-asm callee blaze writes for the
//! `asm!` blocks tempo's dependencies hold has the constraint strings the rules give.
//!
//! The rules, from the Reference (inline-assembly.html, the pinned toolchain's
//! share/doc/rust/html/reference):
//! - "lateout(<reg>) <expr>: Identical to out except that the register allocator can reuse a
//!   register allocated to an in." (so a late output is LLVM's `=`, an early one `=&`);
//! - "inlateout(<reg>) <expr>: Identical to inout except that the register allocator can reuse
//!   a register allocated to an in" (an `inout` is one register in and out: a tied input);
//! - "pure: The assembly code has no side effects, must eventually return ..." (no
//!   `sideeffect`; `willreturn`); "nomem: The assembly code does not read from or write to any
//!   memory accessible outside of the assembly code" (no `~{memory}`, `memory(none)`);
//!   "readonly: The assembly code does not write to any memory accessible outside of the
//!   assembly code"; "preserves_flags: The assembly code does not modify the flags register"
//!   (no `~{cc}`); "nostack: The assembly code does not push data to the stack ... If this
//!   option is not used then the stack pointer is guaranteed ... to be suitably aligned"
//!   (`alignstack` only without it);
//! - "Register classes": AArch64 `reg` is `x[0-30]` (LLVM `r`), `vreg` `v[0-31]` (`w`),
//!   `vreg_low16` `v[0-15]` (`x`); "Template modifiers": `reg` takes `w` (`w0`) and `x`
//!   (`x0`), `vreg` takes `v` (`v0`, LLVM: none), `b`, `h`, `s`, `d`, `q`;
//! - "clobber_abi": AArch64 `"C"` clobbers "x[0-17], x18*, x30, v[0-31], p[0-15], ffr", and
//!   "On AArch64 x18 only included in the clobber list if it is not considered as a reserved
//!   register on the target"; x18 "is an OS-reserved register on some AArch64 targets" (macOS
//!   among them: rustc_target asm/aarch64.rs `target_reserves_x18`, `is_like_darwin`);
//! - "x19 ... is used internally by LLVM as a 'base pointer'" and "The frame pointer cannot be
//!   used as an input or output" (refused).
//!
//! The three blocks are tempo's dependencies' own: zeroize 1.9.1 src/barrier.rs
//! (`"# {}", in(reg) ptr, options(readonly, preserves_flags, nostack)`), cmov 0.5.4
//! src/backends/aarch64.rs `csel32` (`"tst {0:w}, 0xff"`, `"csel {1:w}, {2:w}, {3:w}, NE"`,
//! `in(reg) condition, inlateout(reg) *dst, in(reg) *src, in(reg) *dst, options(pure, nomem,
//! nostack)`), constant_time_eq 0.6.0 src/neon.rs (`"cmeq {c:v}.16b, {a:v}.16b, {b:v}.16b"`,
//! `c = lateout(vreg) c, a = in(vreg) a, b = in(vreg) b, options(pure, nomem,
//! preserves_flags, nostack)`).
//!
//! Prints: `test blaze-llvm: exit 0; 1 passed, 0 failed` (through lanecheck.sh).

#![feature(allocator_api)]

use blaze_code::record::Symbol;
use blaze_lir::func::{AsmOperandKind, AsmOperandRec, AsmPieceRec, AsmRec};
use blaze_lir::ty::{Scalar, Ty};
use blaze_llvm::LowerError;
use blaze_llvm::lower_target::{AsmView, asm_attrs, asm_call};
use blaze_tier::{Arch, Machine, Os};

enum Piece {
    Text(&'static str),
    Operand(u32, Option<char>),
}

/// One `asm!` block's columns, owned by the test.
struct Block {
    text: Vec<u8>,
    pieces: Vec<AsmPieceRec>,
    operands: Vec<AsmOperandRec>,
    tys: Vec<Ty>,
    clobber_abi: Vec<u8>,
    options: u32,
}

/// `operands` are (kind, register or class, whether a class, type).
fn block(template: &[Piece], operands: &[(AsmOperandKind, &str, bool, Ty)], options: u32, clobber_abi: &str) -> Block {
    let mut text = Vec::new();
    let mut pieces = Vec::new();
    for piece in template {
        pieces.push(match *piece {
            Piece::Text(t) => {
                let first = text.len() as u32;
                text.extend_from_slice(t.as_bytes());
                AsmPieceRec { kind: AsmPieceRec::TEXT, a: first, b: t.len() as u32 }
            }
            Piece::Operand(index, modifier) => {
                AsmPieceRec { kind: AsmPieceRec::OPERAND, a: index, b: modifier.map_or(0, u32::from) }
            }
        });
    }
    let mut recs = Vec::new();
    let mut tys = Vec::new();
    for &(kind, reg, class, ty) in operands {
        let reg_first = text.len() as u32;
        text.extend_from_slice(reg.as_bytes());
        recs.push(AsmOperandRec {
            reg_first,
            reg_len: reg.len() as u32,
            aux: 0,
            kind: kind as u8,
            class: u8::from(class),
            pad: [0; 2],
        });
        tys.push(ty);
    }
    Block { text, pieces, operands: recs, tys, clobber_abi: clobber_abi.as_bytes().to_vec(), options }
}

fn view(b: &Block, os: Os) -> AsmView<'_> {
    AsmView {
        body: Symbol([7; 32]),
        inst: Some(3),
        machine: Machine { arch: Arch::Arm64, os, features: 0 },
        options: b.options,
        pieces: &b.pieces,
        operands: &b.operands,
        text: &b.text,
        clobber_abi: &b.clobber_abi,
        tys: &b.tys,
        ptr_bits: 64,
    }
}

/// The callee text and the attribute text, or the refusal.
fn lowered(b: &Block, os: Os) -> Result<(String, String), LowerError> {
    let (mut call, mut attrs) = (String::new(), String::new());
    asm_call(&view(b, os), &mut call)?;
    asm_attrs(&view(b, os), &mut attrs)?;
    Ok((call, attrs))
}

fn refused(b: &Block, os: Os) -> &'static str {
    match lowered(b, os) {
        Err(LowerError::Refused { what, inst: Some(3), .. }) => what,
        other => panic!("expected a refusal naming instruction 3, got {other:?}"),
    }
}

#[test]
fn asm_constraints_match_rustc_forms() {
    use AsmOperandKind::{In, InLateOut, LateOut};
    let i8x16 = Ty::vector(Scalar::I8, 16).expect("i8x16 is a vector type");

    // zeroize: an input pointer; not pure (side effect), reads memory; flags kept; no stack.
    let zeroize = block(
        &[Piece::Text("# "), Piece::Operand(0, None)],
        &[(In, "reg", true, Ty::PTR)],
        AsmRec::READONLY | AsmRec::PRESERVES_FLAGS | AsmRec::NOSTACK,
        "",
    );
    let (call, attrs) = lowered(&zeroize, Os::MacOs).expect("zeroize's barrier lowers");
    assert_eq!(call, r##"asm sideeffect "# ${0}", "r,~{memory}""##);
    assert_eq!(attrs, " memory(read, inaccessiblemem: readwrite)");

    // cmov csel32: the late in-out is output 0, tied by its input "0"; inputs follow it in
    // operand order (operand 0 is constraint 1, operand 2 is 3, operand 3 is 4); `w` kept.
    let csel = block(
        &[
            Piece::Text("tst "),
            Piece::Operand(0, Some('w')),
            Piece::Text(", 0xff\ncsel "),
            Piece::Operand(1, Some('w')),
            Piece::Text(", "),
            Piece::Operand(2, Some('w')),
            Piece::Text(", "),
            Piece::Operand(3, Some('w')),
            Piece::Text(", NE"),
        ],
        &[(In, "reg", true, Ty::I8), (InLateOut, "reg", true, Ty::I32), (In, "reg", true, Ty::I32), (In, "reg", true, Ty::I32)],
        AsmRec::PURE | AsmRec::NOMEM | AsmRec::NOSTACK,
        "",
    );
    let (call, attrs) = lowered(&csel, Os::MacOs).expect("cmov's csel lowers");
    assert_eq!(call, r#"asm "tst ${1:w}, 0xff\0Acsel ${0:w}, ${3:w}, ${4:w}, NE", "=r,r,0,r,r,~{cc}""#);
    assert_eq!(attrs, " memory(none) willreturn");

    // constant_time_eq cmeq: vreg is `w`; `v` is LLVM's default and is not written.
    let cmeq = block(
        &[
            Piece::Text("cmeq "),
            Piece::Operand(0, Some('v')),
            Piece::Text(".16b, "),
            Piece::Operand(1, Some('v')),
            Piece::Text(".16b, "),
            Piece::Operand(2, Some('v')),
            Piece::Text(".16b"),
        ],
        &[(LateOut, "vreg", true, i8x16), (In, "vreg", true, i8x16), (In, "vreg", true, i8x16)],
        AsmRec::PURE | AsmRec::NOMEM | AsmRec::PRESERVES_FLAGS | AsmRec::NOSTACK,
        "",
    );
    let (call, attrs) = lowered(&cmeq, Os::MacOs).expect("constant_time_eq's cmeq lowers");
    assert_eq!(call, r#"asm "cmeq ${0}.16b, ${1}.16b, ${2}.16b", "=w,w,w""#);
    assert_eq!(attrs, " memory(none) willreturn");

    // Explicit registers and clobber_abi("C"), no options: a late in-out in x0 repeats `{x0}`
    // (not tied); an f64 in v0 is `{d0}`; the clobber set leaves out x0 and v0 (explicit
    // outputs) and, on macOS, x18; `$` in text is doubled.
    let call_c = block(
        &[Piece::Text("blr "), Piece::Operand(0, None), Piece::Text(" // $")],
        &[(In, "reg", true, Ty::PTR), (InLateOut, "x0", false, Ty::I64), (LateOut, "v0", false, Ty::F64)],
        0,
        "C",
    );
    let clobbers = |x18: bool| {
        let mut list = Vec::new();
        list.extend((1..=17).map(|n| format!("~{{x{n}}}")));
        if x18 {
            list.push("~{x18}".to_owned());
        }
        list.push("~{x30}".to_owned());
        list.extend((1..=31).map(|n| format!("~{{q{n}}}")));
        list.extend((0..=15).map(|n| format!("~{{p{n}}}")));
        list.push("~{ffr}".to_owned());
        list.join(",")
    };
    let (call, attrs) = lowered(&call_c, Os::MacOs).expect("a call with clobber_abi lowers");
    assert_eq!(
        call,
        format!(r#"asm sideeffect alignstack "blr ${{2}} // $$", "={{x0}},={{d0}},r,{{x0}},{},~{{cc}},~{{memory}}""#, clobbers(false))
    );
    assert_eq!(attrs, "");
    let (call, _) = lowered(&call_c, Os::Linux).expect("on Linux x18 is clobbered too");
    assert!(call.contains(&clobbers(true)), "{call}");

    // A 32-bit value in an explicit `w5` is `{w5}`; a 64-bit one in `x5` is `{x5}`.
    let w5 = block(&[Piece::Text("nop")], &[(In, "w5", false, Ty::I32), (In, "x6", false, Ty::I64)], AsmRec::NOMEM | AsmRec::NOSTACK, "");
    let (call, attrs) = lowered(&w5, Os::MacOs).expect("explicit registers lower");
    assert_eq!(call, r#"asm sideeffect "nop", "{w5},{x6},~{cc}""#);
    assert_eq!(attrs, " memory(inaccessiblemem: readwrite)");

    // The Reference's refusals, by name.
    let reg = |name: &str| block(&[Piece::Text("nop")], &[(In, name, false, Ty::I64)], AsmRec::NOMEM, "");
    assert_eq!(refused(&reg("x19"), Os::MacOs), "x19, LLVM's base pointer, as an `asm!` operand");
    assert_eq!(refused(&reg("fp"), Os::MacOs), "the frame pointer x29 as an `asm!` operand");
    assert_eq!(refused(&reg("x18"), Os::MacOs), "x18, reserved by the OS on this target, as an `asm!` operand");
    assert!(lowered(&reg("x18"), Os::Linux).is_ok(), "x18 is an ordinary register on Linux");
    let pure_alone = block(&[Piece::Text("nop")], &[], AsmRec::PURE, "");
    assert_eq!(refused(&pure_alone, Os::MacOs), "`pure` without `nomem` or `readonly`");
    let input_clobbered = block(&[Piece::Text("svc 0")], &[(In, "x0", false, Ty::I64)], 0, "C");
    assert_eq!(refused(&input_clobbered, Os::MacOs), "an explicit input register that `clobber_abi` also clobbers");
}
