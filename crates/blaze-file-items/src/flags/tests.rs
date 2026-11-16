//! Each form sets its flag, and each look-alike in a string, a comment, a raw string or an
//! identifier sets none.

use blaze_surface::ItemFlags;
use parser::{Edition, LexedStr};

use super::Scan;

const NONE: ItemFlags = ItemFlags::NONE;
const UNSAFE: ItemFlags = ItemFlags::UNSAFE;
const FOREIGN: ItemFlags = ItemFlags::FOREIGN;
const ASM: ItemFlags = ItemFlags::ASM;
const MACRO: ItemFlags = ItemFlags::MACRO_CALL;

/// The flags of every token of `text`, as one machine reads them.
fn flags_of(text: &str) -> ItemFlags {
    let lexed = LexedStr::new(Edition::Edition2024, text);
    assert!(lexed.errors().next().is_none(), "{text:?} lexes");
    let mut scan = Scan::new();
    let mut flags = NONE;
    for at in 0..lexed.len() {
        let kind = lexed.kind(at);
        if !kind.is_trivia() {
            flags |= scan.token(kind, || lexed.text(at));
        }
    }
    flags
}

fn check(cases: &[(&str, ItemFlags)]) {
    for &(text, want) in cases {
        assert_eq!(flags_of(text), want, "the flags of {text:?}");
    }
}

#[test]
fn unsafe_is_the_keyword() {
    check(&[
        ("fn f() { unsafe { g() } }", UNSAFE),
        ("unsafe fn f() {}", UNSAFE),
        ("unsafe impl Send for S {}", UNSAFE),
        ("unsafe trait T {}", UNSAFE),
        ("unsafe extern \"C\" { safe fn f(); }", UNSAFE | FOREIGN),
        ("#[unsafe(no_mangle)] fn f() {}", UNSAFE | FOREIGN),
        ("type F = unsafe fn();", UNSAFE),
        // Look-alikes.
        ("fn f() { let s = \"unsafe { }\"; }", NONE),
        ("fn f() { let s = r#\"unsafe \"# ; }", NONE),
        ("fn f() { let s = b\"unsafe\"; let c = c\"unsafe\"; }", NONE),
        ("// unsafe\nfn f() {}", NONE),
        ("/* unsafe { } */ fn f() {}", NONE),
        ("/// unsafe\n//! unsafe\nfn f() {}", NONE),
        ("#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]", NONE),
        ("fn r#unsafe() { let unsafe_x = 1; let x_unsafe = 2; }", NONE),
    ]);
}

#[test]
fn foreign_is_another_abi_or_a_linker_attribute() {
    check(&[
        ("extern \"C\" { fn f(); }", FOREIGN),
        ("extern { fn f(); }", FOREIGN),
        ("extern \"Rust\" { fn f(); }", FOREIGN),
        ("extern \"C\" fn f() {}", FOREIGN),
        ("extern fn f() {}", FOREIGN),
        ("pub unsafe extern \"system\" fn f() {}", UNSAFE | FOREIGN),
        ("impl S { extern \"C\" fn r#f() {} }", FOREIGN),
        ("#[no_mangle] fn f() {}", FOREIGN),
        ("#[export_name = \"g\"] fn f() {}", FOREIGN),
        ("#[link_section = \".x\"] static X: u8 = 0;", FOREIGN),
        ("#[linkage = \"weak\"] static X: u8 = 0;", FOREIGN),
        ("#[naked] fn f() {}", FOREIGN),
        ("#[link(name = \"c\")] extern {}", FOREIGN),
        ("#[cfg_attr(unix, no_mangle)] fn f() {}", FOREIGN),
        ("#[cfg_attr(any(a, b), inline, cfg_attr(c, export_name = \"g\"))] fn f() {}", FOREIGN),
        // Look-alikes.
        ("extern \"Rust\" fn f() {}", NONE),
        ("extern crate core;", NONE),
        ("type F = extern \"C\" fn(i32) -> i32;", NONE),
        ("fn f(g: extern fn()) {}", NONE),
        ("fn f() { let s = \"extern \\\"C\\\" { }\"; }", NONE),
        ("#[doc = \"no_mangle\"] #[cfg(feature = \"link\")] fn link() { naked(); }", NONE),
        ("#[cfg_attr(no_mangle, inline)] fn f() {}", NONE),
        ("#[doc(alias = \"export_name\")] fn f() {}", NONE),
        ("// #[no_mangle]\n/* extern \"C\" {} */ fn f() {}", NONE),
    ]);
}

#[test]
fn asm_is_the_call() {
    check(&[
        ("fn f() { asm!(\"nop\") }", ASM),
        ("fn f() { core::arch::asm!(\"nop\"); }", ASM),
        ("global_asm!(\"\");", ASM),
        ("#[unsafe(naked)] fn f() { naked_asm!(\"ret\") }", UNSAFE | FOREIGN | ASM),
        // Look-alikes.
        ("fn f() { let s = \"asm!()\"; }", NONE),
        ("// asm!(\"\")\nfn f() {}", NONE),
        ("fn f() { let asm = 1; if asm != 2 { asm(); } }", NONE),
        ("fn f() { let s = r#\"global_asm!(\"\")\"#; }", NONE),
    ]);
}

#[test]
fn a_macro_call_is_one_not_yet_expanded() {
    check(&[
        ("fn f() { println!(\"x\"); }", MACRO),
        ("fn f() { let v = vec![1]; }", MACRO),
        ("m! {}", MACRO),
        ("fn f() { a::b::m!(); }", MACRO),
        ("#[derive(Clone)] struct S;", MACRO),
        ("#[test] fn t() {}", MACRO),
        ("#[tokio::main] fn main() {}", MACRO),
        ("#[global_allocator] static A: X = X;", MACRO),
        ("#[cfg_attr(test, derive(Debug))] struct S;", MACRO),
        ("#[doc = include_str!(\"a.md\")] struct S;", MACRO),
        // Look-alikes and inert attributes.
        ("fn f() { if !a && b != c { return !d; } }", NONE),
        ("macro_rules! m { () => {} }", NONE),
        ("#[inline] #[must_use] #[repr(C)] #[allow(dead_code)] fn f() {}", NONE),
        ("#[rustfmt::skip] #[diagnostic::on_unimplemented(message = \"m!()\")] fn f() {}", NONE),
        ("#[rustc_const_stable(feature = \"x\", since = \"1\")] #[stable(feature = \"y\")] fn f() {}", NONE),
        ("#[cfg(test)] #[cfg_attr(test, inline)] fn f() {}", NONE),
        ("fn f() { let s = \"println!()\"; } // m!()", NONE),
        ("macro_rules! m { ($(#[$a:meta])*) => {} }", NONE),
    ]);
}
