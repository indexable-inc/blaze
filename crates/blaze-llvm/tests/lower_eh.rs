//! P8a's tests (port27/tempo-run/UNWIND.md 3.8): LIR's `invoke`, landing blocks and `resume`
//! become LLVM's `invoke`, `landingpad` and `resume` under `rust_eh_personality`, and LLVM's
//! object then carries the function's LSDA (`__TEXT,__gcc_except_tab` on Mach-O), which
//! crate::read reads back into a Cleanup row.
//!
//! Each test's doc says the line it prints when it passes. Each fails without lower_eh.rs and
//! its wiring: `lower` refused every landing block ("a landing block (unwinding) is not
//! lowered") and every invoke and resume ("unwinding is not lowered").

#![feature(allocator_api)]

use blaze_code::record::{Space, Symbol, Target};
use blaze_code::unwind::Action;
use blaze_llvm::names::{ir_name, ir_name_str};
use blaze_llvm::read::read_object;
use blaze_llvm::{Pipeline, Remark, compile, init, lower};
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{Conv, RefKind};
use blaze_lir::ty::Ty;
use blaze_lir::{BlockKind, Builder, Flags, Full, Func};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Arch, Machine, Names, Named, Os, Outside};

const MAC: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };
const CPU: &str = "apple-m1";

/// The test's root allocator capability (D102).
fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

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

/// `f(a) = invoke may_panic(a) to normal unwind pad; normal: ret r; pad(e): dropper(e); then`
/// by `kind`: Cleanup `resume e`, Catch `ret 0`, Terminate `unreachable`. `may_panic` is
/// `Outside::Func(2)`, `dropper` `Outside::Func(3)`.
fn invokes(kind: BlockKind) -> Func<Alloc<'static>> {
    let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc());
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let one_ptr = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
    let may_panic = b.reference(RefKind::Func, 2);
    let dropper = b.reference(RefKind::Func, 3);
    let entry = b.create_block(BlockKind::Normal);
    let normal = b.create_block(BlockKind::Normal);
    let pad = b.create_block(kind);
    b.begin(entry);
    let a = b.param(Ty::I64);
    let r = b.invoke(Flags::NONE, may_panic, sig, &[a], normal, pad);
    b.begin(normal);
    b.ret(&[r]);
    b.begin(pad);
    let exception = b.param(Ty::PTR);
    b.call(Flags::NONE, dropper, one_ptr, &[exception]);
    match kind {
        BlockKind::Cleanup => b.resume(exception),
        BlockKind::Catch => {
            let zero = b.iconst(Ty::I64, 0);
            b.ret(&[zero]);
        }
        _ => b.unreachable(),
    }
    b.finish_checked().expect("a hand-built invoke builds")
}

/// `f(a) = may_panic(a)` with a plain call: no landing block.
fn calls() -> Func<Alloc<'static>> {
    let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc());
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let may_panic = b.reference(RefKind::Func, 2);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let a = b.param(Ty::I64);
    let r = b.call(Flags::NONE, may_panic, sig, &[a]);
    b.ret(&[r]);
    b.finish_checked().expect("a hand-built call builds")
}

fn ir_of(func: &Func<Alloc<'static>>) -> std::string::String {
    let lowered = lower(func.view(), &ByIndex, sym(1), &[], &[], alloc()).unwrap_or_else(|e| panic!("{e}"));
    lowered.ir.as_str().to_owned()
}

fn object(ir: &str) -> AVec<'static, u8> {
    init().expect("LLVM's targets register");
    let alloc = alloc();
    let mut out: AVec<'static, u8> = Vec::new_in(alloc);
    let mut rows: AVec<'static, Remark> = Vec::new_in(alloc);
    compile(ir.as_bytes(), &MAC, CPU, Pipeline::BlazeO2, &mut out, &mut rows, alloc).unwrap_or_else(|e| panic!("{e}\n{ir}"));
    out
}

/// The IR text of a function with an invoke and a cleanup holds `invoke`, the landing pad with
/// `cleanup` and the exception taken from it, `resume` of the rebuilt pair, and the personality
/// (named on the definition, declared with the library's type). The catch and terminate kinds
/// print their own clause. A function without a landing block names no personality.
/// Passes: "test invoke_and_cleanup_lower_to_llvm_eh ... ok".
#[test]
fn invoke_and_cleanup_lower_to_llvm_eh() {
    let ir = ir_of(&invokes(BlockKind::Cleanup));
    let f = name(1);
    let header = ir.lines().find(|line| line.starts_with("define ") && line.contains(&format!("@{f}("))).expect("f is defined");
    assert!(header.contains(" uwtable \"frame-pointer\"=\"all\""), "{header}");
    assert!(header.ends_with(" personality ptr @rust_eh_personality {"), "the personality names the definition: {header}");
    assert!(ir.lines().any(|line| line == "declare i32 @rust_eh_personality(i32, i32, i64, ptr, ptr)"), "{ir}");
    let invoke = ir.lines().find(|line| line.contains(" invoke ")).unwrap_or_else(|| panic!("an invoke in:\n{ir}"));
    assert!(invoke.contains(&format!("@{}(", name(2))) && invoke.contains(" to label %b1 unwind label %b2"), "{invoke}");
    assert!(ir.lines().any(|line| line.ends_with(" = landingpad { ptr, i32 } cleanup")), "{ir}");
    assert!(ir.lines().any(|line| line.contains(" = extractvalue { ptr, i32 } ") && line.ends_with(".lp, 0")), "{ir}");
    assert!(ir.lines().any(|line| line.starts_with("  resume { ptr, i32 } ")), "{ir}");

    let catch = ir_of(&invokes(BlockKind::Catch));
    assert!(catch.lines().any(|line| line.ends_with(" = landingpad { ptr, i32 } catch ptr null")), "{catch}");
    let terminate = ir_of(&invokes(BlockKind::Terminate));
    assert!(
        terminate.lines().any(|line| line.ends_with(" = landingpad { ptr, i32 } filter [0 x ptr] zeroinitializer")),
        "{terminate}"
    );

    let plain = ir_of(&calls());
    assert!(!plain.contains("personality"), "no landing block, no personality:\n{plain}");
}

/// Compiled through D1's `compile`, each of the three kinds gives a Mach-O object (on this Mac)
/// carrying the function's LSDA in `__TEXT,__gcc_except_tab`; the plain call gives none.
/// Passes: "test invoke_and_cleanup_reach_llvms_object ... ok".
#[test]
fn invoke_and_cleanup_reach_llvms_object() {
    for kind in [BlockKind::Cleanup, BlockKind::Catch, BlockKind::Terminate] {
        let obj = object(&ir_of(&invokes(kind)));
        assert_eq!(&obj[..4], &[0xcf, 0xfa, 0xed, 0xfe], "Mach-O 64-bit magic");
        assert!(obj.windows(16).any(|w| w == b"__gcc_except_tab"), "{} landing: the object has an LSDA section", kind.name());
    }
    let obj = object(&ir_of(&calls()));
    assert!(!obj.windows(16).any(|w| w == b"__gcc_except_tab"), "a plain call: no LSDA section");
}

/// The lowered cleanup, compiled and read back by crate::read (D3 with P8b's LSDA reader), is a
/// record whose rows hold a Cleanup row and nothing but Cleanup and Continue rows, and whose
/// frame saves the pair x19:x20. The exception is live across `dropper(e)` until `resume e`, so
/// LLVM keeps it in x19 and saves the pair in the platform's order (x19 at CFA - 24), which is
/// blaze-code unwind.rs's one frame form (unwind-P8a-4). Before that ruling the reader refused
/// this frame: "a saved pair is not its lower register at CFA - 32 - 16k ...".
/// Passes: "test a_lowered_cleanup_reads_back_as_a_cleanup_row ... ok".
#[test]
fn a_lowered_cleanup_reads_back_as_a_cleanup_row() {
    let obj = object(&ir_of(&invokes(BlockKind::Cleanup)));
    // LLVM lowers `resume` to a call of the unwinder's `_Unwind_Resume` (Mach-O `__Unwind_Resume`,
    // read without the leading underscore): an outside C name the caller resolves, as it
    // resolves every outside name (read.rs, D100 point 11).
    let outside = [name(2), name(3), "_Unwind_Resume".to_owned()];
    let names = |text: &str| outside.iter().any(|n| n == text).then(|| Target { space: Space::Outside, symbol: Symbol::of_name(text) });
    let e = read_object(&obj, &name(1), &names, alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert!(e.unwind.iter().any(|row| row.action == Action::Cleanup), "a Cleanup row: {:?}", &e.unwind[..]);
    assert!(
        e.unwind.iter().all(|row| matches!(row.action, Action::Cleanup | Action::Continue)),
        "only Cleanup and Continue rows: {:?}",
        &e.unwind[..]
    );
    let frame = e.unwind_frame.expect("the cleanup's frame is described");
    assert!(frame.x_pairs & 1 != 0, "the pair x19:x20 is saved (the exception is kept across the drop): {frame:?}");
}

/// The lowered catch, read back the same way, is a record whose rows hold a Catch row and
/// nothing but Catch and Continue rows: its frame is the plain record (nothing is kept across a
/// call).
/// Passes: "test a_lowered_catch_reads_back_as_a_catch_row ... ok".
#[test]
fn a_lowered_catch_reads_back_as_a_catch_row() {
    let obj = object(&ir_of(&invokes(BlockKind::Catch)));
    let outside = [name(2), name(3)];
    let names = |text: &str| outside.iter().any(|n| n == text).then(|| Target { space: Space::Outside, symbol: Symbol::of_name(text) });
    let e = read_object(&obj, &name(1), &names, alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert!(e.unwind.iter().any(|row| row.action == Action::Catch), "a Catch row: {:?}", &e.unwind[..]);
    assert!(
        e.unwind.iter().all(|row| matches!(row.action, Action::Catch | Action::Continue)),
        "only Catch and Continue rows: {:?}",
        &e.unwind[..]
    );
}
