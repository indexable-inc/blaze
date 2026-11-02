//! rustc's `CheckAlignment`, `CheckNull` and `CheckEnums`: inserted before the uses they guard
//! when the crate checks UB, at every MIR optimization level, and never otherwise.

use hir_ty_mir::mir::{MirOptLevel, MirOptions};

use crate::mir;

/// What core gives the checks: the panic handler their panics need, the constants of a type's
/// size and alignment they read, and `transmute`.
const CORE: &str = r#"
#[lang = "panic_impl"]
fn panic_impl() -> ! { loop {} }

pub trait SizedTypeProperties: Sized {
    #[lang = "mem_size_const"]
    const SIZE: usize = 0;
    #[lang = "mem_align_const"]
    const ALIGN: usize = 1;
}
impl<T> SizedTypeProperties for T {}

#[rustc_intrinsic]
pub unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
"#;

const MISALIGNED: &str =
    "misaligned pointer dereference: address must be a multiple of {} but is {}";
const NULL: &str = "null pointer dereference occurred";
const NULL_REFERENCE: &str = "null reference produced";
const INVALID_ENUM: &str = "trying to construct an enum from an invalid value {}";

fn fixture(program: &str) -> String {
    format!("//- minicore: sized, copy\n#![feature(lang_items, intrinsics)]\n{CORE}\n{program}")
}

fn checked(level: MirOptLevel) -> MirOptions {
    MirOptions { level, ub_checks: true, lifetime_markers: false, inline: false }
}

fn unchecked(level: MirOptLevel) -> MirOptions {
    MirOptions { level, ub_checks: false, lifetime_markers: false, inline: false }
}

fn asserts(mir: &str) -> usize {
    mir.matches("assert(").count()
}

#[track_caller]
fn position(mir: &str, message: &str) -> usize {
    mir.find(message).unwrap_or_else(|| panic!("no `{message}` in\n{mir}"))
}

const READ: &str = "fn read(p: *const u32) -> u32 { unsafe { *p } }";

#[test]
fn a_read_is_checked_for_alignment_then_null() {
    let mir = mir(&fixture(READ), "read", checked(MirOptLevel::Debug));
    assert_eq!(asserts(&mir), 2, "{mir}");
    assert!(position(&mir, MISALIGNED) < position(&mir, NULL), "{mir}");
    assert!(mir.contains("(PtrToPtr)") && mir.contains("as usize (Transmute)"), "{mir}");
}

#[test]
fn nothing_is_inserted_without_ub_checks() {
    for level in [MirOptLevel::Off, MirOptLevel::Debug, MirOptLevel::Release] {
        let mir = mir(&fixture(READ), "read", unchecked(level));
        assert_eq!(asserts(&mir), 0, "{level:?}:\n{mir}");
    }
}

#[test]
fn every_level_checks() {
    for level in [MirOptLevel::Off, MirOptLevel::Debug, MirOptLevel::Release] {
        let mir = mir(&fixture(READ), "read", checked(level));
        assert_eq!(asserts(&mir), 2, "{level:?}:\n{mir}");
    }
}

#[test]
fn a_write_is_checked() {
    let program = "fn write(p: *mut u32) { unsafe { *p = 1 } }";
    let mir = mir(&fixture(program), "write", checked(MirOptLevel::Debug));
    assert!(mir.contains(MISALIGNED) && mir.contains(NULL), "{mir}");
}

#[test]
fn bytes_are_only_checked_for_null() {
    let program = "fn read(p: *const u8) -> u8 { unsafe { *p } }";
    let mir = mir(&fixture(program), "read", checked(MirOptLevel::Debug));
    assert_eq!(asserts(&mir), 1, "{mir}");
    assert!(mir.contains(NULL), "{mir}");
}

#[test]
fn a_reference_to_null_is_checked() {
    let program = "fn borrow(p: *const u32) -> &'static u32 { unsafe { &*p } }";
    let mir = mir(&fixture(program), "borrow", checked(MirOptLevel::Debug));
    assert!(mir.contains(MISALIGNED) && mir.contains(NULL_REFERENCE), "{mir}");
}

#[test]
fn a_raw_borrow_is_not_checked() {
    let program = "fn raw(p: *const u32) -> *const u32 { unsafe { &raw const *p } }";
    let mir = mir(&fixture(program), "raw", checked(MirOptLevel::Debug));
    assert_eq!(asserts(&mir), 0, "{mir}");
}

const DIRECT: &str = r#"
#[repr(u8)]
enum E { A = 1, B = 3 }
fn make(x: u8) -> E { unsafe { transmute::<u8, E>(x) } }
"#;

#[test]
fn a_transmute_to_a_tagged_enum_checks_the_tag() {
    let mir = mir(&fixture(DIRECT), "make", checked(MirOptLevel::Debug));
    assert_eq!(asserts(&mir), 1, "{mir}");
    assert!(mir.contains(INVALID_ENUM) && mir.contains("switchInt"), "{mir}");
    let mir = self::mir(&fixture(DIRECT), "make", unchecked(MirOptLevel::Debug));
    assert!(!mir.contains(INVALID_ENUM), "{mir}");
}

#[test]
fn a_transmute_to_a_niche_enum_checks_the_niche() {
    let program = r#"
enum N { A(bool), B }
fn make(x: u8) -> N { unsafe { transmute::<u8, N>(x) } }
"#;
    let mir = mir(&fixture(program), "make", checked(MirOptLevel::Debug));
    assert_eq!(asserts(&mir), 1, "{mir}");
    assert!(mir.contains(INVALID_ENUM), "{mir}");
}
