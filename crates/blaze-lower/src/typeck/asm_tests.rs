//! `asm!` operands lowered, converted and checked (INTRINSICS.md P9b). The rules, from the
//! Rust Reference, inline-assembly (the pinned toolchain's share/doc/rust/html/reference/
//! inline-assembly.html):
//! - "Register operands": "Only the following types are allowed as operands for inline
//!   assembly: integers (signed and unsigned), floating-point numbers, pointers (thin only),
//!   function pointers, SIMD vector types"; the AArch64 table: `reg` takes "i8, i16, i32, f32,
//!   i64, f64", `vreg` (`neon`) also "f64x2" and the other 64- and 128-bit vectors; the x86-64
//!   table: `reg` takes "i16, i32, f32, i64, f64" (no `i8`: that is `reg_byte`'s).
//! - So one `asm!` is accepted on one target and refused on another: the architecture is read
//!   by the lowering as `cfg` atoms at the `asm!` (main's ruling (A); drive.md "One check
//!   covers every configuration"), and the body's record holds those reads, so it is filed
//!   under them. A body with no `asm!` reads none and serves every target.
//!
//! Control (rule 8): with `machine/asm_arch.rs`'s `asm_arch` returning
//! `Some(blaze_typeck::AsmArch::Aarch64)` without asking (the architecture from a constant),
//! `the_architecture_is_a_read_of_the_record` must fail: the x86-64 lowering records no read
//! and its `u8` in `reg` is accepted.

use std::vec;
use std::vec::Vec as StdVec;

use blaze_typeck::fixture::Fx;
use blaze_typeck::{DiagCode, InferCtx, ItemKey, Needs, Outcome, PNode, PortableTys, TypeAttrs, UintTy, check};

use crate::columns::View;
use crate::form::{CfgRead, ExprId, TypeId};
use crate::typeck::{Refused, Types, no_const_arg, to_typeck};
use crate::{Cfg, CfgAnswer, ConstIdx, Edition, lower_body};

/// The bodies here write no type (their parameters' types are the world's).
struct NoWrittenTypes;

impl Types for NoWrittenTypes {
    fn ty(&mut self, _: &View<'_>, _: TypeId, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        Err(Refused::Form { what: "a written type (the test world resolves none)", range: Default::default() })
    }

    fn repeat_len(&mut self, _: &View<'_>, _: ExprId) -> Result<u64, Refused> {
        Err(Refused::Form { what: "a repeat length (the test world evaluates none)", range: Default::default() })
    }

    fn const_arg(&mut self, view: &View<'_>, c: ConstIdx, _: &mut PortableTys<'_>) -> Result<blaze_typeck::PTy, Refused> {
        Err(no_const_arg(view, c))
    }
}

/// A build for one architecture: `target_arch` holds for `arch` only; every other atom is
/// false.
struct Arch(&'static str);

impl Cfg for Arch {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        if name == "target_arch" && value == Some(self.0) { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// `text` lowered for `arch`, converted and checked as `owner` of `fx`: each finding as its
/// code and the text at its range, and the record's `cfg` reads.
fn check_for(fx: &Fx, owner: ItemKey, arch: &'static str, text: &'static str) -> (StdVec<(DiagCode, &'static str)>, StdVec<CfgRead>) {
    let alloc = blaze_own::testing::heap().alloc();
    let lowered = match lower_body(text, Edition::E2024, &mut Arch(arch), &mut crate::NoModule, alloc) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    let view = lowered.view();
    let reads: StdVec<CfgRead> =view.cfg_reads().collect();
    let mut tys = PortableTys::new_in(alloc);
    let body = match to_typeck(&view, owner, &mut tys, &mut NoWrittenTypes, alloc) {
        Ok(b) => b,
        Err(e) => panic!("to_typeck refused {e:?} in\n{text}"),
    };
    let bv = body.view(tys.view());
    let mut cx = InferCtx::new_in(alloc);
    let c = match check(&bv, fx, &mut cx, &Needs::new_in(alloc), alloc) {
        Ok(Outcome::Done(c)) => c,
        Ok(Outcome::Need(n)) => panic!("{text}: the world missed {:?}", n.questions),
        Err(e) => panic!("check refused {e:?} in\n{text}"),
    };
    let found = c.diags.iter().map(|d| (d.code, &text[d.at.start as usize..d.at.end as usize])).collect();
    (found, reads)
}

fn read(value: &str, answer: bool) -> CfgRead {
    CfgRead { name: blaze_typeck::Name::of("target_arch").0, value: blaze_typeck::Name::of(value).0, answer }
}

/// The AArch64 table: a `u64` in `reg` and an `f64x2` (a `repr(simd)` struct of one
/// `[f64; 2]`, the pinned library's `float64x2_t` shape) in `vreg` are taken; a struct in
/// `reg` is no operand type, refused at the operand naming the class.
/// Line: `test typeck::asm_tests::class_type_rules ... ok`.
#[test]
fn class_type_rules() {
    let mut fx = Fx::new();
    let u64_ = fx.ty(PNode::Uint(UintTy::U64));
    let f64_ = fx.ty(PNode::Float(blaze_typeck::FloatTy::F64));
    let s = fx.strukt("S", 0, &[("x", u64_)]);
    let s_ty = fx.adt(s, &[]);
    let lanes = fx.array(f64_, 2);
    let v = fx.tuple_strukt("F64x2", 0, &[lanes]);
    fx.set_attrs(v, TypeAttrs(TypeAttrs::SIMD));
    let v_ty = fx.adt(v, &[]);
    let unit = fx.unit();
    let f = fx.func("f", 0, &[u64_, s_ty, v_ty], unit, StdVec::new());
    fx.finish();
    let text = r#"fn f(a: u64, s: S, v: F64x2) { unsafe { builtin # asm("{} {} {}", in(reg) a, in(reg) s, in(vreg) v) } }"#;
    let (found, _) = check_for(&fx, f, "aarch64", text);
    assert_eq!(found, vec![(DiagCode::AsmOperandType, "s")], "only the struct in `reg` is refused");
}

/// One `asm!` body checked for aarch64 and for x86-64: `u8` in `reg` is AArch64's (`i8`) and
/// not x86-64's, so the two checks differ, and each record holds the `target_arch` reads that
/// chose its table (two records, each filed under its target). A body with no `asm!` reads
/// nothing under either: one record serves both.
/// Line: `test typeck::asm_tests::the_architecture_is_a_read_of_the_record ... ok`.
#[test]
fn the_architecture_is_a_read_of_the_record() {
    let mut fx = Fx::new();
    let u8_ = fx.u8();
    let unit = fx.unit();
    let g = fx.func("g", 0, &[u8_], unit, StdVec::new());
    let h = fx.func("h", 0, &[u8_], u8_, StdVec::new());
    fx.finish();
    let with_asm = r#"fn g(b: u8) { unsafe { builtin # asm("{}", in(reg) b) } }"#;
    let (a64, a64_reads) = check_for(&fx, g, "aarch64", with_asm);
    let (x86, x86_reads) = check_for(&fx, g, "x86_64", with_asm);
    assert_eq!(a64, vec![], "aarch64 `reg` takes i8");
    assert_eq!(x86, vec![(DiagCode::AsmOperandType, "b")], "x86-64 `reg` takes no i8");
    assert_eq!(a64_reads, vec![read("aarch64", true)]);
    assert_eq!(x86_reads, vec![read("aarch64", false), read("x86_64", true)]);

    let without = "fn h(b: u8) -> u8 { b }";
    let (a64, a64_reads) = check_for(&fx, h, "aarch64", without);
    let (x86, x86_reads) = check_for(&fx, h, "x86_64", without);
    assert_eq!((a64, x86), (vec![], vec![]));
    assert_eq!((a64_reads, x86_reads), (vec![], vec![]), "no `asm!`: no read, one record for both");
}
