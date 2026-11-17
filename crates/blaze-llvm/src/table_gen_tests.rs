//! T2's tests (port27/WAVE-BACKEND.md): the table through LLVM. They run templates in this
//! process, so they run on an arm64 host only (the generator refuses another by name).
//!
//! The oracle is LIR's evaluator over the same one-instruction function, and T1's template
//! (blaze-emit's encoders) where both exist. `add x10, x3, x5` is the one arm64 instruction for a
//! 64-bit register add (Arm ARM, ADD (shifted register), shift 0, amount 0), so there the two
//! generators must give equal bytes, not only equal values.

extern crate std;

use std::eprintln;

use blaze_emit::table_gen::{EncodedTable, generate as generate_encoders};
use blaze_lir::Op;
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{Arch, Form, Machine, Os, Pattern, RegClass, Source, Table};

use crate::table_gen::{Plant, Why, generate_some, logical};

const GPR: Form = Form::Reg(RegClass::Gpr);

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// The host's machine and a CPU of it.
fn host() -> (Machine, &'static str) {
    if cfg!(target_os = "macos") {
        (Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE }, "apple-m1")
    } else {
        (Machine { arch: Arch::Arm64, os: Os::Linux, features: Machine::LSE }, "neoverse-n1")
    }
}

unsafe extern "C" {
    /// LLVM's C API (llvm-c/ErrorHandling.h): the function LLVM calls instead of printing
    /// "LLVM ERROR" and aborting. Linked by vendor/llvm-sys.
    fn LLVMInstallFatalErrorHandler(handler: Option<unsafe extern "C" fn(reason: *const core::ffi::c_char)>);
}

/// LLVM's fatal error as a panic line: the panic message is printed before the process aborts
/// (a panic cannot leave an `extern "C"` function), so lanecheck's panic line carries LLVM's text.
unsafe extern "C" fn llvm_fatal(reason: *const core::ffi::c_char) {
    // SAFETY: LLVM passes a NUL-terminated string that lives for the call.
    let text = unsafe { core::ffi::CStr::from_ptr(reason) }.to_string_lossy();
    panic!("LLVM ERROR (fatal handler): {text}");
}

/// `body` on a thread with a 256 MiB stack (an overflow aborts with no panic line) after LLVM's
/// fatal handler is installed; its panic, if any, is this test's.
fn guarded(body: impl FnOnce() + Send + 'static) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    // SAFETY: the handler is an `extern "C"` function of the signature the C API declares.
    ONCE.call_once(|| unsafe { LLVMInstallFatalErrorHandler(Some(llvm_fatal)) });
    let joined = std::thread::Builder::new()
        .name(std::string::String::from("t2-test"))
        .stack_size(256 << 20)
        .spawn(body)
        .expect("the test thread starts")
        .join();
    if let Err(payload) = joined {
        std::panic::resume_unwind(payload);
    }
}

fn reg_reg(op: Op, width: u8) -> Pattern {
    Pattern { op, width, variant: 0, result: GPR, operands: [GPR, GPR, Form::None, Form::None], inner: None }
}

/// The 64-bit register add through LLVM is admitted, its template's bytes are T1's, and its
/// fields are T1's up to exchanging the two inputs (`add` commutes). Passes: "test table_gen_tests::a_64_bit_add_through_llvm_is_the_encoders_template ... ok";
/// stderr "add 64: llvm [..] encoders [..]" with the two byte lists equal.
#[cfg(target_arch = "aarch64")]
#[test]
fn a_64_bit_add_through_llvm_is_the_encoders_template() {
    guarded(add_body);
}

fn add_body() {
    let (machine, cpu) = host();
    let want = reg_reg(Op::Add, 64);
    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    let generated = generate_some(machine, cpu, &|p| *p == want, Plant::None, &mut out, alloc()).unwrap_or_else(|e| panic!("{e}"));
    for refusal in generated.refused.iter() {
        panic!("refused: {refusal}");
    }
    assert_eq!(generated.head.count, 1);
    assert_eq!(generated.head.source, Source::Llvm);
    let llvm = EncodedTable::read(&out, alloc()).unwrap_or_else(|e| panic!("{e}"));
    let llvm = llvm.find(&want).expect("the add's row");

    let mut encoded: AVec<'static, u8> = Vec::new_in(alloc());
    generate_encoders(machine, &mut encoded, alloc()).unwrap_or_else(|e| panic!("{e}"));
    let encoders = EncodedTable::read(&encoded, alloc()).unwrap_or_else(|e| panic!("{e}"));
    let encoders = encoders.find(&want).expect("T1's add row");
    eprintln!("add 64: llvm {:02x?} encoders {:02x?}", llvm.bytes, encoders.bytes);
    assert_eq!(llvm.bytes, encoders.bytes);
    // `add` commutes, and LLVM may put either input in Rn (it put operand 1 there on the first
    // run): the fields are T1's with the two inputs' operand numbers possibly exchanged. Equal
    // values on every input are the validation's to prove (it ran: the row was admitted).
    let swapped = |operand: u8| match operand {
        0 => 1,
        1 => 0,
        other => other,
    };
    let same = llvm.fields == encoders.fields;
    let commuted = llvm.fields.len() == encoders.fields.len()
        && llvm.fields.iter().zip(encoders.fields).all(|(l, e)| {
            l.at == e.at && l.width == e.width && l.from_bit == e.from_bit && l.kind == e.kind && l.operand == swapped(e.operand)
        });
    assert!(same || commuted, "fields differ beyond commuting: llvm {:?} encoders {:?}", llvm.fields, encoders.fields);
}

/// Rule 8's planted bug: the one-instruction function built with its operands swapped compiles
/// to a template discovery accepts, and validation refuses it against the evaluator (`b - a`
/// where `a - b` is meant). Without the plant the same pattern is admitted. Passes:
/// "test table_gen_tests::a_swapped_sub_is_refused_by_validation ... ok"; stderr
/// "plant SwapOperands: refused: `sub` at width 64 ..: on inputs ..".
#[cfg(target_arch = "aarch64")]
#[test]
fn a_swapped_sub_is_refused_by_validation() {
    guarded(swap_body);
}

fn swap_body() {
    let (machine, cpu) = host();
    let want = reg_reg(Op::Sub, 64);
    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    let clean = generate_some(machine, cpu, &|p| *p == want, Plant::None, &mut out, alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(clean.head.count, 1, "the sub is admitted without the plant");

    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    let planted =
        generate_some(machine, cpu, &|p| *p == want, Plant::SwapOperands, &mut out, alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(planted.head.count, 0, "the swapped sub must not be admitted");
    let refusal = planted.refused.first().expect("the swapped sub's refusal");
    eprintln!("plant SwapOperands: refused: {refusal}");
    assert!(matches!(refusal.why, Why::Evaluator { .. }), "refused for another reason: {refusal}");
}

/// Two generations of a few 32-bit patterns give equal bytes; a division is refused by name,
/// never generated. Passes: "test table_gen_tests::the_table_is_deterministic_and_refusals_are_named ... ok".
#[cfg(target_arch = "aarch64")]
#[test]
fn the_table_is_deterministic_and_refusals_are_named() {
    guarded(determinism_body);
}

fn determinism_body() {
    let (machine, cpu) = host();
    let keep = |p: &Pattern| {
        p.width == 32 && p.inner.is_none() && matches!(p.op, Op::Add | Op::Xor | Op::Icmp | Op::Udiv)
    };
    let mut first: AVec<'static, u8> = Vec::new_in(alloc());
    let a = generate_some(machine, cpu, &keep, Plant::None, &mut first, alloc()).unwrap_or_else(|e| panic!("{e}"));
    let mut second: AVec<'static, u8> = Vec::new_in(alloc());
    let b = generate_some(machine, cpu, &keep, Plant::None, &mut second, alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first[..], second[..], "two generations differ");
    assert_eq!(a.head.digest, b.head.digest);
    assert!(a.head.count > 0, "nothing admitted: {:?}", a.refused.first().map(|r| r.pattern));
    for refusal in a.refused.iter() {
        eprintln!("refused: {refusal}");
        if refusal.pattern.op == Op::Udiv {
            assert!(matches!(refusal.why, Why::Shape { .. }), "a division refused for another reason: {refusal}");
        }
    }
    assert!(a.refused.iter().any(|r| r.pattern.op == Op::Udiv), "the division was not refused");
}

/// `DecodeBitMasks` (Arm ARM, shared pseudocode): N=1 imms=7 is eight ones; N=0 imms=0b111100
/// is the 2-bit element `01` repeated; N=1 imms=0x3f (all ones) names no constant; N=1 is no
/// 32-bit immediate. Passes: "test table_gen_tests::logical_immediates_decode ... ok".
#[test]
fn logical_immediates_decode() {
    assert_eq!(logical(1 << 12 | 7, 64), Some(0xff));
    assert_eq!(logical(0b11_1100, 64), Some(0x5555_5555_5555_5555));
    assert_eq!(logical(0b11_1100, 32), Some(0x5555_5555));
    assert_eq!(logical(1 << 12 | 0x3f, 64), None);
    assert_eq!(logical(1 << 12 | 7, 32), None);
    // N=1, immr=4, imms=3: four ones rotated right by four in a 64-bit element.
    assert_eq!(logical(1 << 12 | 4 << 6 | 3, 64), Some(0xf000_0000_0000_0000));
}
