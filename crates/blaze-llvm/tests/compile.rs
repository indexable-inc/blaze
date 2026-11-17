//! D1's tests: the CPU is named, IR errors are named, objects come out, the inliner's remarks
//! are read back (and an unreadable stream is an error), and alone and batched give equal bytes.
//!
//! Each test's doc says the line it prints when it passes.

#![feature(allocator_api)]

use blaze_code::record::Symbol;
use blaze_llvm::names::{ir_name, ir_name_str, symbol_in, symbol_of};
use blaze_llvm::remarks;
use blaze_llvm::{LlvmError, Pipeline, Remark, RemarkError, bitcode, compile, init};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Arch, Machine, Os};

const MAC: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };
const CPU: &str = "apple-m1";

fn sym(byte: u8) -> Symbol {
    Symbol([byte; 32])
}

fn name(byte: u8) -> std::string::String {
    ir_name_str(&ir_name(sym(byte))).to_owned()
}

fn ready() {
    init().expect("LLVM's targets register");
}

fn add_ir(byte: u8) -> std::string::String {
    format!("define i64 @{}(i64 %a, i64 %b) nounwind {{\n  %c = add i64 %a, %b\n  ret i64 %c\n}}\n", name(byte))
}

/// `native` and the empty CPU are refused before LLVM is touched (D100 point 14).
/// Passes: "test native_cpu_is_refused ... ok".
#[test]
fn native_cpu_is_refused() {
    ready();
    let alloc = Alloc::global();
    let mut out: AVec<'_, u8> = Vec::new_in(alloc);
    let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
    let ir = add_ir(1);
    for cpu in ["native", ""] {
        let error = compile(ir.as_bytes(), &MAC, cpu, Pipeline::BlazeO2, &mut out, &mut rows, alloc).unwrap_err();
        assert!(matches!(error, LlvmError::Cpu { .. }), "{error}");
    }
    assert!(out.is_empty());
}

/// IR that does not parse is `LlvmError::Parse` with LLVM's message naming the line.
/// Passes: "test a_parse_error_is_named ... ok".
#[test]
fn a_parse_error_is_named() {
    ready();
    let alloc = Alloc::global();
    let mut out: AVec<'_, u8> = Vec::new_in(alloc);
    let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
    let ir = "define i64 @f(i64 %a) {\n  %c = frobnicate i64 %a\n  ret i64 %c\n}\n";
    let error = compile(ir.as_bytes(), &MAC, CPU, Pipeline::BlazeO2, &mut out, &mut rows, alloc).unwrap_err();
    match error {
        LlvmError::Parse { message } => assert!(message.contains("2:"), "the message names line 2: {}", message.as_str()),
        other => panic!("expected a parse error, got {other}"),
    }
}

/// An add compiles to a Mach-O arm64 object (magic `cffaedfe`, CPU type arm64 `0100000c`).
/// Passes (once the pipeline text is in): "test an_add_compiles_to_an_object ... ok".
#[test]
fn an_add_compiles_to_an_object() {
    ready();
    let alloc = Alloc::global();
    let mut out: AVec<'_, u8> = Vec::new_in(alloc);
    let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
    compile(add_ir(1).as_bytes(), &MAC, CPU, Pipeline::BlazeO2, &mut out, &mut rows, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(&out[..4], &[0xcf, 0xfa, 0xed, 0xfe], "Mach-O 64-bit magic");
    assert_eq!(&out[4..8], &[0x0c, 0x00, 0x00, 0x01], "CPU_TYPE_ARM64");
    assert!(rows.is_empty(), "no call, no decision: {rows:?}");
}

/// The inliner's decisions come back as rows: an internal one-line callee is inlined (a
/// `Passed` remark), a `noinline` callee is not (LLVM's `NeverInline` `Missed` remark; a
/// declaration is never a candidate, so it has no remark).
/// Passes (once the pipeline text is in): "test the_inliners_decisions_are_read_back ... ok".
#[test]
fn the_inliners_decisions_are_read_back() {
    ready();
    let alloc = Alloc::global();
    let (caller, small, outside) = (name(1), name(2), name(3));
    let ir = format!(
        "define i64 @{outside}(i64 %x) noinline {{\n  %y = mul i64 %x, 7\n  ret i64 %y\n}}\n\
         define internal i64 @{small}(i64 %x) {{\n  %y = add i64 %x, 1\n  ret i64 %y\n}}\n\
         define i64 @{caller}(i64 %a) {{\n  %b = call i64 @{small}(i64 %a)\n  %c = call i64 @{outside}(i64 %b)\n  ret i64 %c\n}}\n"
    );
    let mut out: AVec<'_, u8> = Vec::new_in(alloc);
    let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
    compile(ir.as_bytes(), &MAC, CPU, Pipeline::BlazeO2, &mut out, &mut rows, alloc).unwrap_or_else(|e| panic!("{e}"));
    let want = [
        Remark { caller: sym(1), callee: sym(2), inlined: true },
        Remark { caller: sym(1), callee: sym(3), inlined: false },
    ];
    assert_eq!(rows.as_slice(), &want, "sorted, unique: the inlined body and the callee still called");
}

/// A stream that does not parse, and a remark of a pass that was not asked for, are errors by
/// name: never rows guessed from them. Passes: "test an_unreadable_stream_is_an_error ... ok".
#[test]
fn an_unreadable_stream_is_an_error() {
    let alloc = Alloc::global();
    let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
    let broken = b"--- !Passed\nPass: inline\nName: Inlined\nFunction: f\nArgs:\n  - Callee: [\n";
    assert_eq!(remarks::read(broken, &mut rows), Err(RemarkError::Parse));
    let other = b"--- !Passed\nPass: licm\nName: Hoisted\nFunction: f\nArgs:\n  - String: hoisted\n...\n";
    assert_eq!(remarks::read(other, &mut rows), Err(RemarkError::Pass));
    let callee = name(2);
    let no_caller = format!("--- !Passed\nPass: inline\nName: Inlined\nFunction: f\nArgs:\n  - Callee: {callee}\n...\n");
    assert_eq!(remarks::read(no_caller.as_bytes(), &mut rows), Err(RemarkError::NoCaller));
    assert!(rows.is_empty());
    let whole = format!(
        "--- !Passed\nPass: inline\nName: Inlined\nFunction: {c}\nArgs:\n  - Callee: {callee}\n  - String: ' inlined into '\n  - Caller: {c}\n...\n",
        c = name(1)
    );
    assert_eq!(remarks::read(whole.as_bytes(), &mut rows), Ok(()));
    assert_eq!(rows.as_slice(), &[Remark { caller: sym(1), callee: sym(2), inlined: true }]);
}

/// Two functions compiled one after the other in this process, then each again in its own
/// call, give equal object and bitcode bytes: nothing of one compile reaches the next.
/// Passes (once the pipeline text is in): "test alone_and_batched_give_equal_bytes ... ok".
#[test]
fn alone_and_batched_give_equal_bytes() {
    ready();
    let alloc = Alloc::global();
    let mul = format!("define i64 @{}(i64 %a, i64 %b) nounwind {{\n  %c = mul i64 %a, %b\n  ret i64 %c\n}}\n", name(4));
    let inputs = [add_ir(1), mul];
    let once = |ir: &str| {
        let mut object: AVec<'_, u8> = Vec::new_in(alloc);
        let mut code: AVec<'_, u8> = Vec::new_in(alloc);
        let mut rows: AVec<'_, Remark> = Vec::new_in(alloc);
        bitcode(ir.as_bytes(), &[], &MAC, CPU, Pipeline::BlazeO2, &mut object, &mut code, &mut rows, alloc)
            .unwrap_or_else(|e| panic!("{e}"));
        (object.as_slice().to_vec(), code.as_slice().to_vec())
    };
    let batched: std::vec::Vec<_> = inputs.iter().map(|ir| once(ir)).collect();
    let second = once(&inputs[1]);
    let first = once(&inputs[0]);
    assert_eq!(first, batched[0], "the first function alone equals it batched");
    assert_eq!(second, batched[1], "the second function alone equals it batched");
    assert_ne!(batched[0].0, batched[1].0, "two functions, two objects");
}

/// A symbol's IR name reads back to it; a C name reads back as blaze-code's symbol of it.
/// Passes: "test names_round_trip ... ok".
#[test]
fn names_round_trip() {
    let symbol = Symbol(core::array::from_fn(|i| i as u8 * 7));
    assert_eq!(symbol_in(&ir_name(symbol)), Some(symbol));
    assert_eq!(symbol_of(b"memcpy"), Some(Symbol::of_name("memcpy")));
    assert_eq!(symbol_in(b"bz00"), None);
    assert_eq!(symbol_of(b""), None);
}
