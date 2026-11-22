//! A C-variadic call, `snprintf(buf, n, fmt, 42i32, 2.5f64)`: the two variable arguments go in
//! 8-byte stack slots at `sp + 0` and `sp + 8` on Apple arm64, and in `x3` and `d0` on Linux
//! arm64 (pass/variadic.rs).
//!
//! The rules, quoted:
//! - The Rust Reference, "External blocks", [items.extern.variadic.syntax]: "Functions within
//!   external blocks may be made variadic by specifying ... as the last parameter." The pinned
//!   library's libc declares the callee so (library/vendor/libc-0.2.191/src/unix/mod.rs):
//!   `pub fn snprintf(s: *mut c_char, n: size_t, format: *const c_char, ...) -> c_int;`
//! - Apple, "Writing ARM64 code for Apple platforms", "Update code that passes arguments to
//!   variadic functions": the caller places every variadic argument on the stack, each in an
//!   8-byte-aligned slot; floats too.
//! - AAPCS64 (Linux on arm64), stage C, for every argument in order: after `buf`, `n`, `fmt` in
//!   `x0..x2` the `int` takes `x3` and the `double` takes `v0`; past `x7`/`v7` an argument of at
//!   most 8 bytes takes the next 8-byte slot of the stacked arguments from `sp + 0` (the NSAA
//!   "rounded up to the larger of 8 or the Natural Alignment of the argument's type").
//! - AAPCS64, "The Stack", "Universal stack constraints": `sp` "mod 16 = 0" at a public
//!   interface: the area is the slots rounded up to 16.
//! - The record's call site (pass/sites.rs, `import_callee`; record_tests.rs, `holds`): `adrp
//!   x16, 0; ldr x16, [x16]; blr x16`, three contiguous instructions the segment's layout
//!   completes by relocation: the area is opened and every slot stored BEFORE the site, and
//!   closed right after its `blr`.
//! - C (ISO C 7.21.6.1, `fprintf`, which `snprintf` formats as): `%d` writes an `int` in
//!   decimal, `%.1f` a `double` with one digit after the point (2.5 is exact: "2.5"), and
//!   `snprintf` returns the number of characters written, NUL excluded: 6 for "42 2.5".
//!
//! A wrong fix fails here: variable arguments left in registers on Apple (`snprintf`'s
//! `va_arg` reads the stack: the run prints garbage), stacked on Linux (no `x3`/`d0` load), an
//! area not 16-aligned or not released (the run crashes or returns to a wrong `sp`), a cell
//! read at its offset before the `sub` moved it (a wrong value in the slot), or a store placed
//! inside the call site (the site's words are broken: the record test fails).
//!
//! Run: `lanecheck.sh blaze-emit test variadic`; prints "test blaze-emit: exit 0; 4 passed,
//! 0 failed" on this Mac (the run test is macOS-only).

use std::alloc::Global;

use blaze_code::record::SiteKind;
use blaze_lir::{
    Builder, Flags, Full, Ty,
    builder::{MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Func, RefKind},
};

use crate::enc::{self, SP, Size, X9, X16};
use crate::pass::variadic::{MOST_STACKED, VarLoc, area_of, place_of};
use crate::pass::{FloatWidth, Loc, Regs};
use crate::tests::emitted_for;
use crate::{Isa, Named, Names, Os, Outside, Via, emit_record};

const APPLE: Isa = Isa { lse: true, os: Os::MacOs };
const LINUX: Isa = Isa { lse: true, os: Os::Linux };

/// `snprintf`'s declared parameters: `char *`, `size_t`, `const char *`.
const DECLARED: [Param; 3] = [Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::PTR)];

/// `f(buf, fmt, &d) = snprintf(buf, 16, fmt, 42i32, *d)`: the `f64` comes from memory (the
/// baseline tier makes no `f64` constant in one word).
fn caller() -> Func<Global> {
    let params = [Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR)];
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let own = b.sig(Conv::C, Variadic::No, &params, &[Param::of(Ty::I32)]);
    b.set_signature(own);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let buf = b.param(Ty::PTR);
    let fmt = b.param(Ty::PTR);
    let at = b.param(Ty::PTR);
    let snprintf_sig = b.sig(Conv::C, Variadic::Yes, &DECLARED, &[Param::of(Ty::I32)]);
    let snprintf = b.reference(RefKind::Func, 0);
    let n = b.iconst(Ty::I64, 16);
    let int = b.iconst(Ty::I32, 42);
    let double = b.load(Ty::F64, Flags::NONE, at, 0, MemAttrs::aligned(3));
    let written = b.call(Flags::NONE, snprintf, snprintf_sig, &[buf, n, fmt, int, double]);
    b.ret(&[written]);
    b.finish_checked().expect("the variadic call verifies: values past the declared params")
}

fn words(isa: Isa) -> Vec<u32, Global> {
    let func = caller();
    let code = emitted_for(func.view(), &[Via::Address], isa).unwrap_or_else(|why| panic!("{isa:?}: {why}"));
    code.bytes.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
}

/// `ldr`/`str` of a 64-bit register at `sp + imm*8`, any immediate.
const IMM12: u32 = 0xfff << 10;
/// A literal load's 19-bit word offset.
const IMM19: u32 = 0x7_ffff << 5;

/// The register walk after `snprintf`'s three declared parameters: at `x3`, `v0`.
fn named(os: Os) -> Regs {
    let mut r = Regs::new(os);
    for p in DECLARED {
        r.next(p.ty);
    }
    r
}

#[test]
fn variadic_places_follow_the_platform() {
    let mut apple = named(Os::MacOs);
    let mut slots = 0;
    assert_eq!(place_of(Os::MacOs, &mut apple, &mut slots, Ty::I32), Ok(VarLoc::Slot(0)), "Apple: the int in slot 0");
    assert_eq!(place_of(Os::MacOs, &mut apple, &mut slots, Ty::F64), Ok(VarLoc::Slot(8)), "Apple: the double in slot 8, not v0");
    assert_eq!(area_of(slots), 16, "Apple: two slots, 16 bytes (sp stays 16-aligned)");
    assert_eq!(area_of(3), 32, "three slots round up to 32");
    let mut linux = named(Os::Linux);
    let mut slots = 0;
    assert_eq!(place_of(Os::Linux, &mut linux, &mut slots, Ty::I32), Ok(VarLoc::Reg(Loc::X(3))), "Linux: the int in x3");
    assert_eq!(place_of(Os::Linux, &mut linux, &mut slots, Ty::F64), Ok(VarLoc::Reg(Loc::V(0, FloatWidth::Double))), "Linux: the double in v0");
    assert_eq!(area_of(slots), 0, "Linux: nothing stacked");
    // Linux past x7: x4..x7 take four more, the fifth is the first stacked slot; a float still
    // takes v1 (the vector registers are counted apart).
    for x in 4..8 {
        assert_eq!(place_of(Os::Linux, &mut linux, &mut slots, Ty::I64), Ok(VarLoc::Reg(Loc::X(x))), "Linux: x{x}");
    }
    assert_eq!(place_of(Os::Linux, &mut linux, &mut slots, Ty::PTR), Ok(VarLoc::Slot(0)), "Linux: past x7, slot 0");
    assert_eq!(place_of(Os::Linux, &mut linux, &mut slots, Ty::F64), Ok(VarLoc::Reg(Loc::V(1, FloatWidth::Double))), "Linux: the next double in v1");
    assert_eq!(place_of(Os::Linux, &mut linux, &mut slots, Ty::I32), Ok(VarLoc::Slot(8)), "Linux: an int past x7 takes 8 bytes");
    assert_eq!(area_of(slots), 16);
    // C's promotions are the checker's: an unpromoted `f32` or `i8` is refused by name here.
    assert!(place_of(Os::MacOs, &mut named(Os::MacOs), &mut 0, Ty::F32).is_err(), "an f32 to `...`");
    assert!(place_of(Os::Linux, &mut named(Os::Linux), &mut 0, Ty::I8).is_err(), "an i8 to `...`");
    assert!(place_of(Os::MacOs, &mut named(Os::MacOs), &mut { MOST_STACKED }, Ty::I64).is_err(), "past one `sub`'s area");
}

#[test]
fn variadic_call_words_stack_on_apple_and_use_x3_d0_on_linux() {
    let apple = words(APPLE);
    let open = apple.iter().position(|&w| w == enc::sub_imm(SP, SP, 16, false)).expect("Apple: `sub sp, sp, #16` opens the area");
    let slot0 = apple.iter().position(|&w| w == enc::str(Size::X, X9, SP, 0)).expect("Apple: the int stored at sp + 0");
    let slot8 = apple.iter().position(|&w| w == enc::str(Size::X, X9, SP, 1)).expect("Apple: the double stored at sp + 8");
    assert!(open < slot0 && slot0 < slot8, "Apple: open, int, double in order");
    // The literal-word form: the callee's literal into x16, `blr`, then the area closed.
    assert_eq!(apple[slot8 + 1] & !IMM19, enc::ldr_lit(X16, 0), "Apple: the callee loaded after the last slot");
    assert_eq!(apple[slot8 + 2], enc::blr(X16), "Apple: `blr x16` next");
    assert_eq!(apple[slot8 + 3], enc::add_imm(SP, SP, 16, false), "Apple: `add sp, sp, #16` right after the `blr`");
    assert!(!apple.contains(&enc::fmov_to_d(0, X9)), "Apple: no variadic double in d0");

    let linux = words(LINUX);
    let x3 = linux.iter().any(|&w| w & !IMM12 == enc::ldr(Size::X, 3, SP, 0));
    assert!(x3, "Linux: the int loaded into x3");
    assert!(linux.contains(&enc::fmov_to_d(0, X9)), "Linux: the double moved into d0");
    let blr = linux.iter().position(|&w| w == enc::blr(X16)).expect("Linux: the call");
    assert_eq!(linux[blr - 1] & !IMM19, enc::ldr_lit(X16, 0), "Linux: the callee loaded right before the `blr`");
    assert_ne!(linux[blr + 1], enc::add_imm(SP, SP, 16, false), "Linux: no outgoing area to close");
}

/// Every reference is a C function's name: `snprintf` is one.
struct ByC;

impl Names for ByC {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        Some(Named::C("snprintf"))
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

/// In the record form the call site's three instructions are contiguous, with the area opened
/// and both slots stored before its `adrp` and closed right after its `blr`.
/// Prints nothing; fails naming the word that broke the shape.
#[test]
fn variadic_stores_precede_the_record_call_site() {
    let func = caller();
    let heap = blaze_own::testing::heap();
    for isa in [APPLE, LINUX] {
        let (record, _) = emit_record(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &ByC, &crate::RoutinePlaces::NONE, isa, heap.alloc()).unwrap_or_else(|why| panic!("{isa:?}: {why}"));
        let code: Vec<u32, Global> =
            record.code.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let calls: Vec<u32, Global> = record.sites.iter().filter(|s| s.kind == SiteKind::Call).map(|s| s.at / 4).collect();
        assert_eq!(calls.len(), 1, "{isa:?}: one call site");
        let at = calls[0] as usize;
        assert_eq!(code[at], enc::adrp(X16, 0), "{isa:?}: the site starts with `adrp x16, 0`");
        assert_eq!(code[at + 1], enc::ldr(Size::X, X16, X16, 0), "{isa:?}: then `ldr x16, [x16]`");
        assert_eq!(code[at + 2], enc::blr(X16), "{isa:?}: then `blr x16`, nothing between");
        if isa.os == Os::Linux {
            assert_ne!(code[at + 3], enc::add_imm(SP, SP, 16, false), "Linux: no area");
            continue;
        }
        let open = code[..at].iter().rposition(|&w| w == enc::sub_imm(SP, SP, 16, false)).expect("Apple: the area opened before the site");
        let slot0 = code[..at].iter().rposition(|&w| w == enc::str(Size::X, X9, SP, 0)).expect("Apple: slot 0 stored before the site");
        let slot8 = code[..at].iter().rposition(|&w| w == enc::str(Size::X, X9, SP, 1)).expect("Apple: slot 8 stored before the site");
        assert!(open < slot0 && slot0 < slot8, "Apple: open, int, double, then the site");
        // Between the area's opening and the site only x9 is written (this caller has no cell
        // past the scaled reach, so not pass.rs `FAR` either): x16 and x17, the site's and the
        // linker's scratch, are left alone (bits 0..4 of a load, a store or an `add` name its
        // register).
        for (k, &w) in code[open..at].iter().enumerate() {
            assert!(!matches!(w & 31, 16 | 17), "Apple: word {} before the site names x16 or x17: {w:#010x}", open + k);
        }
        assert_eq!(code[at + 3], enc::add_imm(SP, SP, 16, false), "Apple: `add sp, sp, #16` right after the site's `blr`");
    }
}

/// The emitted caller runs on this Mac against the C library's own `snprintf`.
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn variadic_call_runs_snprintf_on_this_mac() {
    use crate::tests::run;
    use crate::{Fixup, RelocTo};

    unsafe extern "C" {
        fn snprintf(s: *mut u8, n: usize, format: *const u8, ...) -> i32;
    }
    let func = caller();
    let code = emitted_for(func.view(), &[Via::Address], APPLE).unwrap_or_else(|why| panic!("{why}"));
    let exec = run::place_all(&code, |_: Fixup| snprintf as usize as u64, |to: RelocTo| panic!("no routine is called: {to:?}"));
    let mut buf = [0xffu8; 16];
    let fmt = c"%d %.1f";
    let double = 2.5f64;
    let got = run::call(&exec, [buf.as_mut_ptr() as u64, fmt.as_ptr() as u64, &double as *const f64 as u64, 0]);
    assert_eq!(got as u32 as i32, 6, "snprintf returns the characters written, NUL excluded");
    assert_eq!(&buf[..7], b"42 2.5\0", "C's `%d %.1f` of 42 and 2.5");
}
