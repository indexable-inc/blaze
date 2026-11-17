//! D2's tests: LIR becomes LLVM IR text in which a callee known only by facts is a declaration
//! carrying exactly those facts, an offered LIR callee is `available_externally`, LIR's
//! division traps, and the text is the same bytes in any offer order.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_code::record::Symbol;
use blaze_llvm::names::{ir_name, ir_name_str};
use blaze_llvm::{CalleeFacts, LowerError, Offered, lower};
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{Conv, RefKind};
use blaze_lir::ty::Ty;
use blaze_lir::{BlockKind, Builder, Flags, Full, Func, Op};
use blaze_own::Alloc;
use blaze_tier::{Named, Names, Outside};

fn sym(byte: u8) -> Symbol {
    Symbol([byte; 32])
}

fn name(byte: u8) -> std::string::String {
    ir_name_str(&ir_name(sym(byte))).to_owned()
}

/// Function `Outside::Func(k)` is `sym(k)`.
struct ByIndex;

impl Names for ByIndex {
    fn name(&self, of: Outside) -> Option<Named<'_>> {
        match of {
            Outside::Func(k) => Some(Named::Symbol(sym(k as u8))),
            _ => None,
        }
    }
}

/// `f(a) = callee_k(a)` over i64, where callee k is `Outside::Func(k)`.
fn calls(k: u32) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let a = b.param(Ty::I64);
    let callee = b.reference(RefKind::Func, k);
    let r = b.call(Flags::NONE, callee, sig, &[a]);
    b.ret(&[r]);
    b.finish_checked().expect("a hand-built call builds")
}

/// `f(a, b) = a op b` over `ty`.
fn binary(ty: Ty, op: Op, flags: Flags) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(ty), Param::of(ty)], &[Param::of(ty)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(ty), b.param(ty));
    let r = b.binary(op, flags, x, y);
    b.ret(&[r]);
    b.finish_checked().expect("a hand-built binary builds")
}

/// A callee given as facts only is a declaration with exactly those facts: nothing is inferred
/// (D100 point 7 (b)). Passes: "test a_callee_known_by_facts_is_declared_with_them ... ok".
#[test]
fn a_callee_known_by_facts_is_declared_with_them() {
    let alloc = Alloc::global();
    let func = calls(2);
    let facts = [CalleeFacts { symbol: sym(2), nounwind: true, readonly: true, readnone: false, noalias_ret: false }];
    let lowered = lower(func.view(), &ByIndex, sym(1), &[], &facts, alloc).unwrap_or_else(|e| panic!("{e}"));
    let ir = lowered.ir.as_str();
    let want = format!("declare i64 @{}(i64) nounwind readonly", name(2));
    assert!(ir.lines().any(|line| line == want), "wanted `{want}` in:\n{ir}");
    assert!(ir.contains(&format!("define i64 @{}(", name(1))), "{ir}");
    assert!(lowered.linked.is_empty());
}

/// An offered LIR callee is an `available_externally` definition, and no declaration; the
/// offer order does not change a byte. Passes: "test offered_bodies_are_available_externally ... ok".
#[test]
fn offered_bodies_are_available_externally() {
    let alloc = Alloc::global();
    let root = calls(2);
    let (two, three) = (calls(3), calls(4));
    let first = [
        Offered::Lir { view: two.view(), names: &ByIndex, symbol: sym(2) },
        Offered::Lir { view: three.view(), names: &ByIndex, symbol: sym(3) },
    ];
    let second = [
        Offered::Lir { view: three.view(), names: &ByIndex, symbol: sym(3) },
        Offered::Lir { view: two.view(), names: &ByIndex, symbol: sym(2) },
    ];
    let a = lower(root.view(), &ByIndex, sym(1), &first, &[], alloc).unwrap_or_else(|e| panic!("{e}"));
    let b = lower(root.view(), &ByIndex, sym(1), &second, &[], alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(a.ir.as_str(), b.ir.as_str(), "the offer order changes no byte");
    let ir = a.ir.as_str();
    assert!(ir.contains(&format!("define available_externally i64 @{}(", name(2))), "{ir}");
    assert!(!ir.contains(&format!("declare i64 @{}(", name(2))), "a defined callee is not also declared:\n{ir}");
    assert!(ir.contains(&format!("declare i64 @{}(i64)", name(4))), "the leaf is declared:\n{ir}");
}

/// Offering the function itself, or one symbol twice, is refused by name.
/// Passes: "test a_symbol_offered_twice_is_refused ... ok".
#[test]
fn a_symbol_offered_twice_is_refused() {
    let alloc = Alloc::global();
    let root = calls(2);
    let twice = [Offered::Bitcode { bytes: b"", symbol: sym(2) }, Offered::Bitcode { bytes: b"", symbol: sym(2) }];
    let err = lower(root.view(), &ByIndex, sym(1), &twice, &[], alloc).err();
    assert_eq!(err, Some(LowerError::Offered { symbol: sym(2), what: "offered twice" }));
    let itself = [Offered::Bitcode { bytes: b"", symbol: sym(1) }];
    let err = lower(root.view(), &ByIndex, sym(1), &itself, &[], alloc).err();
    assert_eq!(err, Some(LowerError::Offered { symbol: sym(1), what: "the function itself" }));
}

/// LIR division traps on zero unless promised `nonzero`; a shift masks its amount.
/// Passes: "test lirs_semantics_are_kept ... ok".
#[test]
fn lirs_semantics_are_kept() {
    let alloc = Alloc::global();
    let checked = lower(binary(Ty::I32, Op::Udiv, Flags::NONE).view(), &ByIndex, sym(1), &[], &[], alloc).unwrap_or_else(|e| panic!("{e}"));
    assert!(checked.ir.contains("call void @llvm.trap()"), "{}", checked.ir.as_str());
    let promised = lower(binary(Ty::I32, Op::Udiv, Flags::NONZERO).view(), &ByIndex, sym(1), &[], &[], alloc).unwrap_or_else(|e| panic!("{e}"));
    assert!(!promised.ir.contains("llvm.trap"), "{}", promised.ir.as_str());
    let shift = lower(binary(Ty::I32, Op::Shl, Flags::NONE).view(), &ByIndex, sym(1), &[], &[], alloc).unwrap_or_else(|e| panic!("{e}"));
    assert!(shift.ir.contains(", 31\n"), "the amount is masked to 31:\n{}", shift.ir.as_str());
}
