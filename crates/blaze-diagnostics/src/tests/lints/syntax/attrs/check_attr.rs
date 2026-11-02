//! What `check_mod_attrs` lints (`lints::syntax::attrs::check_attr`) against rustc's UI tests,
//! sources and `.stderr` verbatim, and the cases no UI test shows alone.
//!
//! A `.stderr` is the whole pipeline's: where a test also has another's diagnostics, the test
//! says whose they are.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized},
};

/// `crate::tests::ui` of a crate rustc is run on alone, with no `--crate-type` (its
/// `#![crate_type]`s, else a binary), as a UI test is: the crate types decide two of
/// `check_unused_attribute`'s lints, and a fixture's `crate:other:main` is a cargo library, whose
/// `crate-type` the front end does not know.
fn ui(name: &str, edition: &str, source: &str) -> String {
    let fixture = format!("//- /{name} crate:other:main edition:{edition}\n{source}");
    let options = compiletest(Mode::Other, &Options::default());
    normalized(&checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// The messages of `lint` that `source`, a library of edition 2021, gets.
fn messages(source: &str, lint: &str) -> Vec<String> {
    let fixture = format!("//- /lib.rs crate:main edition:2021\n{source}");
    checked_with(&fixture, "", &Options::default())
        .messages()
        .into_iter()
        .filter(|it| it.strip_prefix(lint).is_some_and(|rest| rest.starts_with(':')))
        .collect()
}

/// `tests/ui/repr/conflicting-repr-hints.rs`. The E0566 lints at lines 13 and 19 are
/// `check_attr`'s, their future-incompatible notes and report the levels'; E0587 and E0634 are
/// `diagnostics-items`'.
#[test]
fn conflicting_repr_hints() {
    let source = r##"#![allow(dead_code)]

#[repr(C)]
enum A {
    A,
}

#[repr(u64)]
enum B {
    B,
}

#[repr(C, u64)] //~ ERROR conflicting representation hints
//~^ WARN this was previously accepted
enum C {
    C,
}

#[repr(u32, u64)] //~ ERROR conflicting representation hints
//~^ WARN this was previously accepted
enum D {
    D,
}

#[repr(C, packed)]
struct E(i32);

#[repr(packed, align(8))]
struct F(i32); //~ ERROR type has conflicting packed and align representation hints

#[repr(packed)]
#[repr(align(8))]
struct G(i32); //~ ERROR type has conflicting packed and align representation hints

#[repr(align(8))]
#[repr(packed)]
struct H(i32); //~ ERROR type has conflicting packed and align representation hints

#[repr(packed, packed(2))]
struct I(i32); //~ ERROR type has conflicting packed representation hints

#[repr(packed(2))]
#[repr(packed)]
struct J(i32); //~ ERROR type has conflicting packed representation hints

#[repr(packed, packed(1))]
struct K(i32);

#[repr(packed, align(8))]
union X {
    //~^ ERROR type has conflicting packed and align representation hints
    i: i32,
}

#[repr(packed)]
#[repr(align(8))]
union Y {
    //~^ ERROR type has conflicting packed and align representation hints
    i: i32,
}

#[repr(align(8))]
#[repr(packed)]
union Z {
    //~^ ERROR type has conflicting packed and align representation hints
    i: i32,
}

#[repr(packed, align(0x100))]
pub struct S(u16); //~ ERROR type has conflicting packed and align representation hints

#[repr(packed, align(0x100))]
pub union U { //~ ERROR type has conflicting packed and align representation hints
    u: u16
}

static B: U = U { u: 0 };
static A: S = S(0);

fn main() {}
"##;
    assert_reports(
        &ui("conflicting-repr-hints.rs", "2015", source),
        &[
            error(13, &[]),
            error(19, &[]),
            error(29, &[]),
            error(33, &[]),
            error(37, &[]),
            error(40, &[]),
            error(44, &[]),
            error(50, &[]),
            error(57, &[]),
            error(64, &[]),
            error(70, &[]),
            error(73, &[]),
            error(13, &[]),
            error(19, &[]),
        ],
    );
}

/// `tests/ui/lint/unused/unused-attr-repr-packed-c-order.rs`: `C` with `packed`, in either
/// order, conflicts with nothing.
#[test]
fn c_with_packed_does_not_conflict() {
    let source = r##"//! regression test for <https://github.com/rust-lang/rust/issues/26646>
//@ check-pass
#![deny(unused_attributes)]

#[repr(C)]
#[repr(packed)]
pub struct Foo;

#[repr(packed)]
#[repr(C)]
pub struct Bar;

fn main() {}
"##;
    assert_eq!(ui("unused-attr-repr-packed-c-order.rs", "2015", source), "");
}

/// The rest of `check_repr`'s lint: `C` with an integer only on an enum none of whose
/// variants has fields, `C` with `simd`, the hints of every `#[repr]` together.
#[test]
fn conflicting_repr_hints_by_case() {
    let conflicting = |source: &str| messages(source, "conflicting_repr_hints");
    let lint = "conflicting_repr_hints: conflicting representation hints".to_owned();
    assert_eq!(conflicting("#[repr(C, u8)]\nenum E { A(u8), B }\n"), Vec::<String>::new());
    assert_eq!(conflicting("#[repr(C, u8)]\nenum E { A {}, B }\n"), Vec::<String>::new());
    assert_eq!(conflicting("#[repr(C, u8)]\nenum E { A, B }\n"), [lint.clone()]);
    assert_eq!(conflicting("#[repr(C)]\n#[repr(u8)]\nenum E { A, #[cfg(any())] B(u8) }\n"), [lint.clone()]);
    assert_eq!(conflicting("#[repr(u8)]\n#[repr(i16)]\nenum E { A(u8) }\n"), [lint.clone()]);
    assert_eq!(conflicting("#![feature(repr_simd)]\n#[repr(C, simd)]\nstruct S([f32; 4]);\n"), [lint]);
    assert_eq!(conflicting("#[repr(C, align(8))]\nstruct S(u8);\n"), Vec::<String>::new());
}

/// `tests/ui/lint/inline-exported.rs`.
#[test]
fn inline_on_exported_functions() {
    let source = r##"//! Ensure the unused_attributes lint fires for externally exported functions with `#[inline]`,
//! because `#[inline]` is ignored for such functions.

#![crate_type = "lib"]
#![feature(linkage)]
#![deny(unused_attributes)]

#[inline]
//~^ ERROR: `#[inline]` is ignored on externally exported functions
#[no_mangle]
fn no_mangle() {}

#[inline]
//~^ ERROR: `#[inline]` is ignored on externally exported functions
#[export_name = "export_name"]
fn export_name() {}

#[inline]
//~^ ERROR: `#[inline]` is ignored on externally exported functions
#[linkage = "external"]
fn external_linkage() {}

#[inline]
fn normal() {}

#[inline]
#[linkage = "internal"] // not exported
fn internal_linkage() {}
"##;
    let expected = r##"error: `#[inline]` is ignored on externally exported functions
  --> $DIR/inline-exported.rs:8:1
   |
LL | #[inline]
   | ^^^^^^^^^
   |
   = help: externally exported functions are functions with `#[no_mangle]`, `#[export_name]`, or `#[linkage]`
note: the lint level is defined here
  --> $DIR/inline-exported.rs:6:9
   |
LL | #![deny(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

error: `#[inline]` is ignored on externally exported functions
  --> $DIR/inline-exported.rs:13:1
   |
LL | #[inline]
   | ^^^^^^^^^
   |
   = help: externally exported functions are functions with `#[no_mangle]`, `#[export_name]`, or `#[linkage]`

error: `#[inline]` is ignored on externally exported functions
  --> $DIR/inline-exported.rs:18:1
   |
LL | #[inline]
   | ^^^^^^^^^
   |
   = help: externally exported functions are functions with `#[no_mangle]`, `#[export_name]`, or `#[linkage]`

error: aborting due to 3 previous errors

"##;
    assert_eq!(ui("inline-exported.rs", "2015", source), expected);
}

/// The rest of `check_inline`: inherent and provided trait methods are checked, methods of a
/// trait impl and closures are not, `#[inline(never)]` is not, and a weak lang item
/// (`#[panic_handler]`) is exported.
#[test]
fn inline_on_exported_functions_by_case() {
    let inline = |source: &str| messages(source, "unused_attributes");
    let lint = "unused_attributes: `#[inline]` is ignored on externally exported functions".to_owned();
    assert_eq!(inline("struct S;\nimpl S {\n    #[inline]\n    #[unsafe(no_mangle)]\n    fn f() {}\n}\n"), [lint.clone()]);
    assert_eq!(
        inline("trait T {\n    #[inline]\n    #[unsafe(export_name = \"t_f\")]\n    fn f() {}\n}\n"),
        [lint.clone()]
    );
    assert_eq!(
        inline("trait T { fn f(); }\nstruct S;\nimpl T for S {\n    #[inline]\n    #[unsafe(no_mangle)]\n    fn f() {}\n}\n"),
        Vec::<String>::new()
    );
    assert_eq!(inline("#[inline(never)]\n#[unsafe(no_mangle)]\nfn f() {}\n"), Vec::<String>::new());
    assert_eq!(inline("#[inline(always)]\n#[unsafe(no_mangle)]\nfn f() {}\n"), [lint.clone()]);
    assert_eq!(inline("#[inline]\n#[unsafe(export_name = \"\")]\nfn f() {}\n"), Vec::<String>::new());
    assert_eq!(inline("struct Info;\n#[inline]\n#[panic_handler]\nfn panic(_: &Info) -> ! { loop {} }\n"), [lint]);
}

/// `tests/ui/lint/empty-lint-attributes.rs`: the item's lints before the crate's.
#[test]
fn empty_lint_attributes() {
    let source = r##"//@ check-pass

// Empty (and reason-only) lint attributes are legal—although we may want to
// lint them in the future (Issue #55112).

#![allow()] //~ WARN unused attribute
#![warn(reason = "observationalism")] //~ WARN unused attribute

#[forbid()] //~ WARN unused attribute
fn devoir() {}

#[deny(reason = "ultion")] //~ WARN unused attribute
fn waldgrave() {}

fn main() {}
"##;
    let expected = r##"warning: unused attribute
  --> $DIR/empty-lint-attributes.rs:9:1
   |
LL | #[forbid()]
   | ^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `forbid` with an empty list has no effect
   = note: requested on the command line with `-W unused-attributes`

warning: unused attribute
  --> $DIR/empty-lint-attributes.rs:12:1
   |
LL | #[deny(reason = "ultion")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `deny` without any lints has no effect

warning: unused attribute
  --> $DIR/empty-lint-attributes.rs:6:1
   |
LL | #![allow()]
   | ^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `allow` with an empty list has no effect

warning: unused attribute
  --> $DIR/empty-lint-attributes.rs:7:1
   |
LL | #![warn(reason = "observationalism")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `warn` without any lints has no effect

warning: 4 warnings emitted

"##;
    assert_eq!(ui("empty-lint-attributes.rs", "2015", source), expected);
}

/// `tests/ui/lint/rfc-2383-lint-reason/lint-attribute-only-with-reason.rs`.
#[test]
fn lint_attributes_with_only_a_reason() {
    let source = r##"#![deny(unused_attributes)]

#[allow(reason = "I want to allow something")]//~ ERROR unused attribute
#[expect(reason = "I don't know what I'm waiting for")]//~ ERROR unused attribute
#[warn(reason = "This should be warn by default")]//~ ERROR unused attribute
#[deny(reason = "All listed lints are denied")]//~ ERROR unused attribute
#[forbid(reason = "Just some reason")]//~ ERROR unused attribute

#[allow(clippy::box_collection, reason = "This is still valid")]
#[warn(dead_code, reason = "This is also reasonable")]

fn main() {}
"##;
    let expected = r##"error: unused attribute
  --> $DIR/lint-attribute-only-with-reason.rs:3:1
   |
LL | #[allow(reason = "I want to allow something")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `allow` without any lints has no effect
note: the lint level is defined here
  --> $DIR/lint-attribute-only-with-reason.rs:1:9
   |
LL | #![deny(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

error: unused attribute
  --> $DIR/lint-attribute-only-with-reason.rs:4:1
   |
LL | #[expect(reason = "I don't know what I'm waiting for")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `expect` without any lints has no effect

error: unused attribute
  --> $DIR/lint-attribute-only-with-reason.rs:5:1
   |
LL | #[warn(reason = "This should be warn by default")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `warn` without any lints has no effect

error: unused attribute
  --> $DIR/lint-attribute-only-with-reason.rs:6:1
   |
LL | #[deny(reason = "All listed lints are denied")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `deny` without any lints has no effect

error: unused attribute
  --> $DIR/lint-attribute-only-with-reason.rs:7:1
   |
LL | #[forbid(reason = "Just some reason")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: attribute `forbid` without any lints has no effect

error: aborting due to 5 previous errors

"##;
    assert_eq!(ui("lint-attribute-only-with-reason.rs", "2015", source), expected);
}

/// `tests/ui/lint/unused/empty-attributes.rs`. Lines 2 to 6 are `check_attr`'s; 7, 9 and 12
/// (`EmptyAttributeList`, emitted with the delayed lints after `check_mod_attrs`) are `parse`'s.
#[test]
fn empty_attributes() {
    let source = r##"#![deny(unused_attributes)]
#![allow()] //~ ERROR unused attribute
#![expect()] //~ ERROR unused attribute
#![warn()] //~ ERROR unused attribute
#![deny()] //~ ERROR unused attribute
#![forbid()] //~ ERROR unused attribute
#![feature()] //~ ERROR unused attribute

#[repr()] //~ ERROR unused attribute
pub struct S;

#[target_feature()] //~ ERROR unused attribute
pub unsafe fn foo() {}

fn main() {}
"##;
    assert_reports(
        &ui("empty-attributes.rs", "2015", source),
        &[
            error(2, &[]),
            error(3, &[]),
            error(4, &[]),
            error(5, &[]),
            error(6, &[]),
            error(7, &[]),
            error(9, &[]),
            error(12, &[]),
        ],
    );
}

/// `tests/ui/lint/linker-warning.rs`: the function's lint before the crate's.
#[test]
fn linker_messages_below_the_root_and_in_a_library() {
    let source = r##"//@ check-pass
#![crate_type = "lib"]
#![warn(unused_attributes)]
#![allow(linker_messages)]
//~^ WARNING unused attribute

#[allow(linker_messages)]
//~^ WARNING should be an inner attribute
fn foo() {}
"##;
    let expected = r##"warning: crate-level attribute should be an inner attribute
  --> $DIR/linker-warning.rs:7:1
   |
LL | #[allow(linker_messages)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/linker-warning.rs:3:9
   |
LL | #![warn(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^
help: add a `!`
   |
LL | #![allow(linker_messages)]
   |  +

warning: unused attribute
  --> $DIR/linker-warning.rs:4:1
   |
LL | #![allow(linker_messages)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: the `linker_messages` and `linker_info` lints can only be controlled at the root of a crate that needs to be linked

warning: 2 warnings emitted

"##;
    assert_eq!(ui("linker-warning.rs", "2015", source), expected);
}

/// `tests/ui/lint/linker-warning-bin.rs`: a binary is linked.
#[test]
fn linker_messages_in_a_binary() {
    let source = r##"//@ build-pass
#![crate_type = "bin"]
#![warn(unused_attributes)]
#![allow(linker_messages)]

fn main() {}
"##;
    assert_eq!(ui("linker-warning-bin.rs", "2015", source), "");
}

/// `linker_messages` below the root as an inner attribute, and a cargo library, whose
/// `crate-type` the front end does not know.
#[test]
fn linker_messages_by_case() {
    let unused = |source: &str| messages(source, "unused_attributes");
    assert_eq!(
        unused("mod m {\n    #![allow(linker_info)]\n}\n"),
        ["unused_attributes: crate-level attribute should be in the root module"]
    );
    assert_eq!(unused("#![allow(linker_messages)]\n"), Vec::<String>::new());
    assert_eq!(unused("#![allow(dead_code_pub_in_binary)]\n"), ["unused_attributes: unused attribute"]);
}

/// `tests/ui/lint/dead-code-pub-in-binary/library.rs`. The `dead_code` error is
/// `diagnostics-lints-names`'.
#[test]
fn dead_code_pub_in_binary_in_a_library() {
    let source = r##"#![deny(dead_code)]
#![deny(dead_code_pub_in_binary)]
//~^ WARN unused attribute

#![crate_type = "lib"]

pub fn unused_pub_fn() {} // Should NOT error because it's a library

fn unused_priv_fn() {} //~ ERROR function `unused_priv_fn` is never used
"##;
    let expected = r##"warning: unused attribute
  --> $DIR/library.rs:2:1
   |
LL | #![deny(dead_code_pub_in_binary)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ help: remove this attribute
   |
   = note: the `dead_code_pub_in_binary` lint has no effect in library crates
   = note: requested on the command line with `-W unused-attributes`

error: function `unused_priv_fn` is never used
  --> $DIR/library.rs:9:4
   |
LL | fn unused_priv_fn() {}
   |    ^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/library.rs:1:9
   |
LL | #![deny(dead_code)]
   |         ^^^^^^^^^

error: aborting due to 1 previous error; 1 warning emitted

"##;
    assert_eq!(ui("library.rs", "2015", source), expected);
}

/// `tests/ui/attributes/macro_export_on_decl_macro.rs`.
#[test]
fn macro_export_on_a_decl_macro() {
    let source = r##"// Using #[macro_export] on a decl macro has no effect and should warn

#![feature(decl_macro)]
#![deny(unused)]

#[macro_export] //~ ERROR `#[macro_export]` has no effect on declarative macro definitions
pub macro foo() {}

fn main() {}
"##;
    let expected = r##"error: `#[macro_export]` has no effect on declarative macro definitions
  --> $DIR/macro_export_on_decl_macro.rs:6:1
   |
LL | #[macro_export]
   | ^^^^^^^^^^^^^^^
   |
   = note: declarative macros follow the same exporting rules as regular items
note: the lint level is defined here
  --> $DIR/macro_export_on_decl_macro.rs:4:9
   |
LL | #![deny(unused)]
   |         ^^^^^^
   = note: `#[deny(unused_attributes)]` implied by `#[deny(unused)]`

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("macro_export_on_decl_macro.rs", "2015", source), expected);
}

/// `check_link`: `#[link]` on an `extern "Rust"` block or one of an ABI rustc does not know
/// (lowered as `Rust`), not on one of `C` (written or not) or another ABI; the lint is at the
/// first `#[link]`. The text is that of
/// `tests/ui/feature-gates/issue-43106-gating-of-builtin-attrs.rs`'s line 724.
#[test]
fn link_on_an_extern_block_of_the_rust_abi() {
    let unused = |source: &str| messages(source, "unused_attributes");
    let lint = "unused_attributes: attribute should be applied to an `extern` block with non-Rust ABI".to_owned();
    assert_eq!(unused("#[link(name = \"x\")]\nextern \"Rust\" {}\n"), [lint.clone()]);
    assert_eq!(unused("#[link(name = \"x\")]\n#[link(name = \"y\")]\nextern \"Rust\" {}\n"), [lint]);
    assert_eq!(unused("#[link(name = \"x\")]\nextern \"C\" {}\n"), Vec::<String>::new());
    assert_eq!(unused("#[link(name = \"x\")]\nextern {}\n"), Vec::<String>::new());
    assert_eq!(unused("#[link(name = \"x\")]\nextern \"system\" {}\n"), Vec::<String>::new());
}

/// `tests/ui/attributes/mixed_export_name_and_no_mangle.rs`.
#[test]
fn no_mangle_with_export_name() {
    let source = r##"// issue: rust-lang/rust#47446
//@ run-rustfix
//@ check-pass

#![warn(unused_attributes)]
#[no_mangle]
//~^ WARN `#[no_mangle]` attribute may not be used in combination with `#[export_name]` [unused_attributes]
#[export_name = "foo"]
pub fn bar() {}

#[unsafe(no_mangle)]
//~^ WARN `#[no_mangle]` attribute may not be used in combination with `#[export_name]` [unused_attributes]
#[export_name = "baz"]
pub fn bak() {}

fn main() {}
"##;
    let expected = r##"warning: `#[no_mangle]` attribute may not be used in combination with `#[export_name]`
  --> $DIR/mixed_export_name_and_no_mangle.rs:6:1
   |
LL | #[no_mangle]
   | ^^^^^^^^^^^^ `#[no_mangle]` is ignored
   |
note: `#[export_name]` takes precedence
  --> $DIR/mixed_export_name_and_no_mangle.rs:8:1
   |
LL | #[export_name = "foo"]
   | ^^^^^^^^^^^^^^^^^^^^^^
note: the lint level is defined here
  --> $DIR/mixed_export_name_and_no_mangle.rs:5:9
   |
LL | #![warn(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^
help: remove the `#[no_mangle]` attribute
   |
LL - #[no_mangle]
   |

warning: `#[no_mangle]` attribute may not be used in combination with `#[export_name]`
  --> $DIR/mixed_export_name_and_no_mangle.rs:11:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^ `#[no_mangle]` is ignored
   |
note: `#[export_name]` takes precedence
  --> $DIR/mixed_export_name_and_no_mangle.rs:13:1
   |
LL | #[export_name = "baz"]
   | ^^^^^^^^^^^^^^^^^^^^^^
help: remove the `#[no_mangle]` attribute
   |
LL - #[unsafe(no_mangle)]
   |

warning: 2 warnings emitted

"##;
    assert_eq!(ui("mixed_export_name_and_no_mangle.rs", "2015", source), expected);
}

/// `tests/ui/attributes/mixed_export_name_and_no_mangle_2024.rs`.
#[test]
fn no_mangle_with_export_name_in_2024() {
    let source = r##"// issue: rust-lang/rust#47446
//@ run-rustfix
//@ check-pass
//@ edition:2024

#![warn(unused_attributes)]
#[unsafe(no_mangle)]
//~^ WARN `#[unsafe(no_mangle)]` attribute may not be used in combination with `#[unsafe(export_name)]` [unused_attributes]
#[unsafe(export_name = "foo")]
pub fn bar() {}

#[unsafe(no_mangle)]
//~^ WARN `#[unsafe(no_mangle)]` attribute may not be used in combination with `#[unsafe(export_name)]` [unused_attributes]
#[unsafe(export_name = "baz")]
pub fn bak() {}

fn main() {}
"##;
    let expected = r##"warning: `#[unsafe(no_mangle)]` attribute may not be used in combination with `#[unsafe(export_name)]`
  --> $DIR/mixed_export_name_and_no_mangle_2024.rs:7:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^ `#[unsafe(no_mangle)]` is ignored
   |
note: `#[unsafe(export_name)]` takes precedence
  --> $DIR/mixed_export_name_and_no_mangle_2024.rs:9:1
   |
LL | #[unsafe(export_name = "foo")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
note: the lint level is defined here
  --> $DIR/mixed_export_name_and_no_mangle_2024.rs:6:9
   |
LL | #![warn(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^
help: remove the `#[unsafe(no_mangle)]` attribute
   |
LL - #[unsafe(no_mangle)]
   |

warning: `#[unsafe(no_mangle)]` attribute may not be used in combination with `#[unsafe(export_name)]`
  --> $DIR/mixed_export_name_and_no_mangle_2024.rs:12:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^ `#[unsafe(no_mangle)]` is ignored
   |
note: `#[unsafe(export_name)]` takes precedence
  --> $DIR/mixed_export_name_and_no_mangle_2024.rs:14:1
   |
LL | #[unsafe(export_name = "baz")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
help: remove the `#[unsafe(no_mangle)]` attribute
   |
LL - #[unsafe(no_mangle)]
   |

warning: 2 warnings emitted

"##;
    assert_eq!(ui("mixed_export_name_and_no_mangle_2024.rs", "2024", source), expected);
}
