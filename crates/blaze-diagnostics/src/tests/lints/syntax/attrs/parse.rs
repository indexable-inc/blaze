//! What attribute parsing says of builtin attributes (`lints::syntax::attrs::parse`) against
//! rustc's UI tests, sources and `.stderr` verbatim, and the messages of the parsers no whole
//! UI test shows alone.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, checked_with, compiletest, normalized, ui_with},
};

/// What rustc writes compiling the UI test `source`, named `$DIR/<name>`, of edition 2015 (with
/// no flags of its own), with `files` (path, text) beside it.
fn ui(name: &str, source: &str, files: &[(&str, &str)]) -> String {
    if files.is_empty() {
        return ui_with(name, "2015", source, &Options::default());
    }
    let mut fixture = format!("//- /{name} crate:other:main edition:2015\n{source}");
    for (path, text) in files {
        fixture.push_str(&format!("//- /{path}\n{text}"));
    }
    let options = compiletest(Mode::Other, &Options::default());
    normalized(&checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// The messages of `lints` that `source`, of edition 2021, gets, with their lint names.
fn messages(source: &str, lints: &[&str]) -> Vec<String> {
    let fixture = format!("//- /main.rs crate:other:main edition:2021\n{source}");
    checked_with(&fixture, "", &Options::default())
        .messages()
        .into_iter()
        .filter(|it| lints.iter().any(|lint| it.strip_prefix(lint).is_some_and(|rest| rest.starts_with(':'))))
        .collect()
}

/// `tests/ui/lint/lint-misplaced-attr.rs`.
#[test]
fn misplaced_crate_level_attributes() {
    let source = r##"// When denying at the crate level, be sure to not get random warnings from the
// injected intrinsics by the compiler.

#![deny(unused_attributes)]

mod a {
    #![crate_type = "bin"] //~ ERROR the `#![crate_type]` attribute can only be used at the crate root
}

#[crate_type = "bin"] fn main() {} //~ ERROR should be an inner
"##;
    let expected = r##"error: the `#![crate_type]` attribute can only be used at the crate root
  --> $DIR/lint-misplaced-attr.rs:7:5
   |
LL |     #![crate_type = "bin"]
   |     ^^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/lint-misplaced-attr.rs:4:9
   |
LL | #![deny(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

error: crate-level attribute should be an inner attribute: add an exclamation mark: `#![crate_type]`
  --> $DIR/lint-misplaced-attr.rs:10:1
   |
LL | #[crate_type = "bin"] fn main() {}
   | ^^^^^^^^^^^^^^^^^^^^^
   |
note: this attribute does not have an `!`, which means it is applied to this function
  --> $DIR/lint-misplaced-attr.rs:10:23
   |
LL | #[crate_type = "bin"] fn main() {}
   |                       ^^^^^^^^^^^^

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui("lint-misplaced-attr.rs", source, &[]), expected);
}

/// `tests/ui/attributes/crate-only-as-outer.rs`.
#[test]
fn crate_level_attributes_as_outer_and_below_the_root() {
    let source = r##"#![deny(unused)]
#[crate_name = "owo"]
//~^ ERROR: crate-level attribute should be an inner attribute: add an exclamation mark: `#![crate_name]`

fn main() {}

mod inner {
    #![crate_name = "iwi"]
    //~^ ERROR: the `#![crate_name]` attribute can only be used at the crate root
}
"##;
    let expected = r##"error: crate-level attribute should be an inner attribute: add an exclamation mark: `#![crate_name]`
  --> $DIR/crate-only-as-outer.rs:2:1
   |
LL | #[crate_name = "owo"]
   | ^^^^^^^^^^^^^^^^^^^^^
   |
note: this attribute does not have an `!`, which means it is applied to this function
  --> $DIR/crate-only-as-outer.rs:5:1
   |
LL | fn main() {}
   | ^^^^^^^^^^^^
note: the lint level is defined here
  --> $DIR/crate-only-as-outer.rs:1:9
   |
LL | #![deny(unused)]
   |         ^^^^^^
   = note: `#[deny(unused_attributes)]` implied by `#[deny(unused)]`

error: the `#![crate_name]` attribute can only be used at the crate root
  --> $DIR/crate-only-as-outer.rs:8:5
   |
LL |     #![crate_name = "iwi"]
   |     ^^^^^^^^^^^^^^^^^^^^^^

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui("crate-only-as-outer.rs", source, &[]), expected);
}

/// `tests/ui/attributes/crate-root-path-in-different-file.rs`.
#[test]
fn crate_root_in_another_file() {
    let source = r##"#![deny(unused_attributes)]

#[path = "auxiliary/submod.rs"]
mod submod;

fn main() {}

//~? ERROR the `#![crate_name]` attribute can only be used at the crate root
"##;
    let aux = r##"#![crate_name = "bar"]
"##;
    let expected = r##"error: the `#![crate_name]` attribute can only be used at the crate root
  --> $DIR/auxiliary/submod.rs:1:1
   |
LL | #![crate_name = "bar"]
   | ^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: the crate root is at `$DIR/crate-root-path-in-different-file.rs`
note: the lint level is defined here
  --> $DIR/crate-root-path-in-different-file.rs:1:9
   |
LL | #![deny(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("crate-root-path-in-different-file.rs", source, &[("auxiliary/submod.rs", aux)]), expected);
}

/// `tests/ui/attributes/invalid-crate-type.rs`.
#[test]
fn invalid_crate_types() {
    let source = r##"// regression test for issue 11256
#![crate_type="foo"]    //~ ERROR invalid `crate_type` value
//~| NOTE `#[deny(unknown_crate_types)]` on by default
//~| HELP did you mean
//~| SUGGESTION lib

// Tests for suggestions (#53958)

#![crate_type="statoclib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION staticlib

#![crate_type="procmacro"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION proc-macro

#![crate_type="static-lib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION staticlib

#![crate_type="drylib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION dylib

#![crate_type="dlib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION lib

#![crate_type="lob"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION lib

#![crate_type="bon"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION bin

#![crate_type="cdalib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION cdylib

// substring matching tests
#![crate_type="binary"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION bin

#![crate_type="library"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION lib

#![crate_type="rustlib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION rlib

#![crate_type="dynamiclib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION dylib

#![crate_type="dylibrary"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION dylib

#![crate_type="cdynamiclib"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION cdylib

#![crate_type="cdylibrary"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION cdylib

#![crate_type="staticlibrary"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION staticlib

#![crate_type="procedural-macro"]
//~^ ERROR invalid `crate_type` value
//~| HELP did you mean
//~| SUGGESTION proc-macro

fn main() {
    return
}
"##;
    let expected = r##"error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:2:15
   |
LL | #![crate_type="foo"]
   |               ^^^^^ help: did you mean: `"lib"`
   |
   = note: `#[deny(unknown_crate_types)]` on by default

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:9:15
   |
LL | #![crate_type="statoclib"]
   |               ^^^^^^^^^^^ help: did you mean: `"staticlib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:14:15
   |
LL | #![crate_type="procmacro"]
   |               ^^^^^^^^^^^ help: did you mean: `"proc-macro"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:19:15
   |
LL | #![crate_type="static-lib"]
   |               ^^^^^^^^^^^^ help: did you mean: `"staticlib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:24:15
   |
LL | #![crate_type="drylib"]
   |               ^^^^^^^^ help: did you mean: `"dylib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:29:15
   |
LL | #![crate_type="dlib"]
   |               ^^^^^^ help: did you mean: `"lib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:34:15
   |
LL | #![crate_type="lob"]
   |               ^^^^^ help: did you mean: `"lib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:39:15
   |
LL | #![crate_type="bon"]
   |               ^^^^^ help: did you mean: `"bin"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:44:15
   |
LL | #![crate_type="cdalib"]
   |               ^^^^^^^^ help: did you mean: `"cdylib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:50:15
   |
LL | #![crate_type="binary"]
   |               ^^^^^^^^ help: did you mean: `"bin"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:55:15
   |
LL | #![crate_type="library"]
   |               ^^^^^^^^^ help: did you mean: `"lib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:60:15
   |
LL | #![crate_type="rustlib"]
   |               ^^^^^^^^^ help: did you mean: `"rlib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:65:15
   |
LL | #![crate_type="dynamiclib"]
   |               ^^^^^^^^^^^^ help: did you mean: `"dylib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:70:15
   |
LL | #![crate_type="dylibrary"]
   |               ^^^^^^^^^^^ help: did you mean: `"dylib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:75:15
   |
LL | #![crate_type="cdynamiclib"]
   |               ^^^^^^^^^^^^^ help: did you mean: `"cdylib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:80:15
   |
LL | #![crate_type="cdylibrary"]
   |               ^^^^^^^^^^^^ help: did you mean: `"cdylib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:85:15
   |
LL | #![crate_type="staticlibrary"]
   |               ^^^^^^^^^^^^^^^ help: did you mean: `"staticlib"`

error: invalid `crate_type` value
  --> $DIR/invalid-crate-type.rs:90:15
   |
LL | #![crate_type="procedural-macro"]
   |               ^^^^^^^^^^^^^^^^^^ help: did you mean: `"proc-macro"`

error: aborting due to 18 previous errors

"##;
    assert_eq!(ui("invalid-crate-type.rs", source, &[]), expected);
}

/// `tests/ui/lint/unused/unused-attr-macro-rules.rs`.
#[test]
fn attributes_on_macro_rules() {
    let source = r##"#![deny(unused_attributes)]
// Unused attributes on macro_rules requires special handling since the
// macro_rules definition does not survive towards HIR.

// A sample of various built-in attributes.
#[macro_export]
#[macro_use] //~ ERROR attribute cannot be used on
//~| WARN previously accepted
#[path="foo"] //~ ERROR attribute cannot be used on
//~| WARN previously accepted
#[recursion_limit="1"] //~ ERROR crate-level attribute should be an inner attribute
macro_rules! foo {
    () => {};
}

// The following should not warn about unused attributes.
#[allow(unused)]
macro_rules! foo2 {
    () => {};
}

#[cfg(false)]
macro_rules! foo {
    () => {};
}

/// Some docs
#[deprecated]
#[doc = "more docs"]
#[macro_export]
macro_rules! bar {
    () => {};
}

fn main() {}
"##;
    let expected = r##"error: the `macro_use` attribute cannot be used on macro defs
  --> $DIR/unused-attr-macro-rules.rs:7:3
   |
LL | #[macro_use]
   |   ^^^^^^^^^
   |
   = help: the `macro_use` attribute can be applied to extern crates and modules
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
note: the lint level is defined here
  --> $DIR/unused-attr-macro-rules.rs:1:9
   |
LL | #![deny(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

error: the `path` attribute cannot be used on macro defs
  --> $DIR/unused-attr-macro-rules.rs:9:3
   |
LL | #[path="foo"]
   |   ^^^^
   |
   = help: the `path` attribute can only be applied to modules
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!

error: crate-level attribute should be an inner attribute: add an exclamation mark: `#![recursion_limit]`
  --> $DIR/unused-attr-macro-rules.rs:11:1
   |
LL | #[recursion_limit="1"]
   | ^^^^^^^^^^^^^^^^^^^^^^
   |
note: this attribute does not have an `!`, which means it is applied to this macro def
  --> $DIR/unused-attr-macro-rules.rs:12:1
   |
LL | / macro_rules! foo {
LL | |     () => {};
LL | | }
   | |_^

error: aborting due to 3 previous errors

"##;
    assert_eq!(ui("unused-attr-macro-rules.rs", source, &[]), expected);
}

/// `tests/ui/lint/inert-attr-macro.rs`.
#[test]
fn inert_attributes_on_macro_calls() {
    let source = r##"//@ check-pass

#![warn(unused)]

macro_rules! foo {
    () => {}
}

fn main() {
    #[inline] foo!(); //~ WARN the `inline` attribute cannot be used on macro calls
    //~^ WARN previously accepted

    // This does nothing, since `#[allow(warnings)]` is itself
    // an inert attribute on a macro call
    #[allow(warnings)] #[inline] foo!(); //~ WARN unused attribute `allow`
    //~^ WARN the `inline` attribute cannot be used on macro calls
    //~| WARN previously accepted

    // This does work, since the attribute is on a parent
    // of the macro invocation.
    #[allow(warnings)] { #[inline] foo!(); }

    // Ok, `cfg` and `cfg_attr` are expanded eagerly and do not warn.
    #[cfg(true)] foo!();
    #[cfg(false)] foo!();
    #[cfg_attr(true, cfg(true))] foo!();
    #[cfg_attr(false, nonexistent)] foo!();
}
"##;
    let expected = r##"warning: the `inline` attribute cannot be used on macro calls
  --> $DIR/inert-attr-macro.rs:10:7
   |
LL |     #[inline] foo!();
   |       ^^^^^^
   |
   = help: the `inline` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute
note: the lint level is defined here
  --> $DIR/inert-attr-macro.rs:3:9
   |
LL | #![warn(unused)]
   |         ^^^^^^
   = note: `#[warn(unused_attributes)]` implied by `#[warn(unused)]`

warning: unused attribute `allow`
  --> $DIR/inert-attr-macro.rs:15:5
   |
LL |     #[allow(warnings)] #[inline] foo!();
   |     ^^^^^^^^^^^^^^^^^^
   |
note: the built-in attribute `allow` will be ignored, since it's applied to the macro invocation `foo`
  --> $DIR/inert-attr-macro.rs:15:34
   |
LL |     #[allow(warnings)] #[inline] foo!();
   |                                  ^^^

warning: the `inline` attribute cannot be used on macro calls
  --> $DIR/inert-attr-macro.rs:15:26
   |
LL |     #[allow(warnings)] #[inline] foo!();
   |                          ^^^^^^
   |
   = help: the `inline` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: 3 warnings emitted

"##;
    assert_eq!(ui("inert-attr-macro.rs", source, &[]), expected);
}

/// `tests/ui/attributes/attr-on-mac-call.rs`.
#[test]
fn attributes_on_macro_calls() {
    let source = r##"//@ check-fail
// Regression test for https://github.com/rust-lang/rust/issues/145779
#![warn(unused_attributes)]
#![feature(register_tool)]
#![feature(sanitize)]

fn main() {
    #[export_name = "x"]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[unsafe(naked)]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[track_caller]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[used]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[target_feature(enable = "x")]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[deprecated]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[inline]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[link_name = "x"]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[link_section = "__TEXT,__text"]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[link_ordinal(42)]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[non_exhaustive]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[proc_macro]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[cold]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[no_mangle]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[deprecated]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[automatically_derived]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[macro_use]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[must_use]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[no_implicit_prelude]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[path = ""]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[ignore]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[should_panic]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[link_name = "x"]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    #[sanitize(address = "off")]
    //~^ ERROR attribute cannot be used on macro calls
    unreachable!();

    #[repr()]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    //~| WARN unused attribute
    unreachable!();
    #[repr(u8)]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    unreachable!();
    #[repr(align(8))]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    unreachable!();
    #[repr(packed)]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    unreachable!();
    #[repr(C)]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    unreachable!();
    #[repr(Rust)]
    //~^ WARN attribute cannot be used on macro calls
    //~| WARN previously accepted
    unreachable!();
    #[repr(simd)]
    //~^ ERROR attribute cannot be used on macro calls
    //~| ERROR SIMD types are experimental and possibly buggy
    unreachable!();
    #[register_tool(xyz)]
    //~^ ERROR crate-level attribute should be an inner attribute
    unreachable!();
}
"##;
    let expected = r##"error: the `sanitize` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:77:7
   |
LL |     #[sanitize(address = "off")]
   |       ^^^^^^^^
   |
   = help: the `sanitize` attribute can be applied to crates, functions, impl blocks, modules, and statics
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

error[E0658]: SIMD types are experimental and possibly buggy
  --> $DIR/attr-on-mac-call.rs:106:12
   |
LL |     #[repr(simd)]
   |            ^^^^
   |
   = note: see issue #27731 <https://github.com/rust-lang/rust/issues/27731> for more information
   = help: add `#![feature(repr_simd)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date

error: the `repr(simd)` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:106:7
   |
LL |     #[repr(simd)]
   |       ^^^^^^^^^^
   |
   = help: the `repr(simd)` attribute can only be applied to structs
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

error: crate-level attribute should be an inner attribute: add an exclamation mark: `#![register_tool]`
  --> $DIR/attr-on-mac-call.rs:110:5
   |
LL |     #[register_tool(xyz)]
   |     ^^^^^^^^^^^^^^^^^^^^^
   |
note: this attribute does not have an `!`, which means it is applied to this macro call
  --> $DIR/attr-on-mac-call.rs:112:5
   |
LL |     unreachable!();
   |     ^^^^^^^^^^^^^^

warning: the `export_name` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:8:7
   |
LL |     #[export_name = "x"]
   |       ^^^^^^^^^^^
   |
   = help: the `export_name` attribute can be applied to functions and statics
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute
note: the lint level is defined here
  --> $DIR/attr-on-mac-call.rs:3:9
   |
LL | #![warn(unused_attributes)]
   |         ^^^^^^^^^^^^^^^^^

warning: the `naked` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:11:14
   |
LL |     #[unsafe(naked)]
   |              ^^^^^
   |
   = help: the `naked` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `track_caller` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:14:7
   |
LL |     #[track_caller]
   |       ^^^^^^^^^^^^
   |
   = help: the `track_caller` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `used` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:17:7
   |
LL |     #[used]
   |       ^^^^
   |
   = help: the `used` attribute can only be applied to statics
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `target_feature` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:20:7
   |
LL |     #[target_feature(enable = "x")]
   |       ^^^^^^^^^^^^^^
   |
   = help: the `target_feature` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `deprecated` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:23:7
   |
LL |     #[deprecated]
   |       ^^^^^^^^^^
   |
   = help: the `deprecated` attribute can be applied to associated consts, associated types, constants, crates, data types, enum variants, foreign statics, functions, inherent impl blocks, macro defs, modules, statics, struct fields, traits, type aliases, and use statements
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `inline` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:26:7
   |
LL |     #[inline]
   |       ^^^^^^
   |
   = help: the `inline` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `link_name` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:29:7
   |
LL |     #[link_name = "x"]
   |       ^^^^^^^^^
   |
   = help: the `link_name` attribute can be applied to foreign functions and foreign statics
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `link_section` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:32:7
   |
LL |     #[link_section = "__TEXT,__text"]
   |       ^^^^^^^^^^^^
   |
   = help: the `link_section` attribute can be applied to functions and statics
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `link_ordinal` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:35:7
   |
LL |     #[link_ordinal(42)]
   |       ^^^^^^^^^^^^
   |
   = help: the `link_ordinal` attribute can be applied to foreign functions and foreign statics
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `non_exhaustive` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:38:7
   |
LL |     #[non_exhaustive]
   |       ^^^^^^^^^^^^^^
   |
   = help: the `non_exhaustive` attribute can be applied to data types and enum variants
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `proc_macro` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:41:7
   |
LL |     #[proc_macro]
   |       ^^^^^^^^^^
   |
   = help: the `proc_macro` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `cold` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:44:7
   |
LL |     #[cold]
   |       ^^^^
   |
   = help: the `cold` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `no_mangle` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:47:7
   |
LL |     #[no_mangle]
   |       ^^^^^^^^^
   |
   = help: the `no_mangle` attribute can be applied to functions and statics
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `deprecated` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:50:7
   |
LL |     #[deprecated]
   |       ^^^^^^^^^^
   |
   = help: the `deprecated` attribute can be applied to associated consts, associated types, constants, crates, data types, enum variants, foreign statics, functions, inherent impl blocks, macro defs, modules, statics, struct fields, traits, type aliases, and use statements
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `automatically_derived` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:53:7
   |
LL |     #[automatically_derived]
   |       ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: the `automatically_derived` attribute can only be applied to trait impl blocks
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `macro_use` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:56:7
   |
LL |     #[macro_use]
   |       ^^^^^^^^^
   |
   = help: the `macro_use` attribute can be applied to extern crates and modules
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `must_use` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:59:7
   |
LL |     #[must_use]
   |       ^^^^^^^^
   |
   = help: the `must_use` attribute can be applied to data types, functions, and traits
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `no_implicit_prelude` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:62:7
   |
LL |     #[no_implicit_prelude]
   |       ^^^^^^^^^^^^^^^^^^^
   |
   = help: the `no_implicit_prelude` attribute can be applied to crates and modules
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `path` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:65:7
   |
LL |     #[path = ""]
   |       ^^^^
   |
   = help: the `path` attribute can only be applied to modules
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `ignore` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:68:7
   |
LL |     #[ignore]
   |       ^^^^^^
   |
   = help: the `ignore` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `should_panic` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:71:7
   |
LL |     #[should_panic]
   |       ^^^^^^^^^^^^
   |
   = help: the `should_panic` attribute can only be applied to functions
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `link_name` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:74:7
   |
LL |     #[link_name = "x"]
   |       ^^^^^^^^^
   |
   = help: the `link_name` attribute can be applied to foreign functions and foreign statics
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `repr()` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:81:7
   |
LL |     #[repr()]
   |       ^^^^^^
   |
   = help: the `repr()` attribute can only be applied to data types
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: unused attribute
  --> $DIR/attr-on-mac-call.rs:81:5
   |
LL |     #[repr()]
   |     ^^^^^^^^^ help: remove this attribute
   |
   = note: using `repr` with an empty list has no effect

warning: the `repr(u8)` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:86:7
   |
LL |     #[repr(u8)]
   |       ^^^^^^^^
   |
   = help: the `repr(u8)` attribute can only be applied to enums
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `repr(align(...))` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:90:7
   |
LL |     #[repr(align(8))]
   |       ^^^^^^^^^^^^^^
   |
   = help: the `repr(align(...))` attribute can only be applied to data types
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `repr(packed)` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:94:7
   |
LL |     #[repr(packed)]
   |       ^^^^^^^^^^^^
   |
   = help: the `repr(packed)` attribute can only be applied to data types
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `repr(C)` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:98:7
   |
LL |     #[repr(C)]
   |       ^^^^^^^
   |
   = help: the `repr(C)` attribute can only be applied to data types
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

warning: the `repr(Rust)` attribute cannot be used on macro calls
  --> $DIR/attr-on-mac-call.rs:102:7
   |
LL |     #[repr(Rust)]
   |       ^^^^^^^^^^
   |
   = help: the `repr(Rust)` attribute can only be applied to data types
   = warning: this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!
   = note: placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute

error: aborting due to 4 previous errors; 30 warnings emitted

For more information about this error, try `rustc --explain E0658`.
"##;
    let fixture = format!("{CORE_UNREACHABLE}//- /attr-on-mac-call.rs crate:other:main edition:2015 deps:core\n{source}");
    let options = compiletest(Mode::Other, &Options::default());
    let stderr = checked_with(&fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    assert_eq!(normalized(&stderr), expected);
}

/// The core `attr-on-mac-call.rs` calls `unreachable!` of, through its prelude as core's
/// prelude gives it; the expansion is not core's, which these lints do not look into.
const CORE_UNREACHABLE: &str = "//- /core.rs crate:core edition:2015
pub mod prelude {
    pub mod rust_2015 {
        pub use crate::unreachable;
    }
}
#[macro_export]
macro_rules! unreachable {
    () => {
        loop {}
    };
}
";

/// `tests/ui/rust-2024/unsafe-attributes/in_2024_compatibility.rs`.
#[test]
fn unsafe_attribute_without_unsafe_before_2024() {
    let source = r##"//@ edition:2015
#![deny(rust_2024_compatibility)]

#[no_mangle]
//~^ ERROR: unsafe attribute used without unsafe
//~| WARN this is accepted in the current edition
extern "C" fn foo() {}

fn main() {}
"##;
    let expected = r##"error: unsafe attribute used without unsafe
  --> $DIR/in_2024_compatibility.rs:4:3
   |
LL | #[no_mangle]
   |   ^^^^^^^^^ usage of unsafe attribute
   |
   = warning: this is accepted in the current edition (Rust 2015) but is a hard error in Rust 2024!
   = note: for more information, see <https://doc.rust-lang.org/edition-guide/rust-2024/unsafe-attributes.html>
note: the lint level is defined here
  --> $DIR/in_2024_compatibility.rs:2:9
   |
LL | #![deny(rust_2024_compatibility)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^
   = note: `#[deny(unsafe_attr_outside_unsafe)]` implied by `#[deny(rust_2024_compatibility)]`
help: wrap the attribute in `unsafe(...)`
   |
LL | #[unsafe(no_mangle)]
   |   +++++++         +

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("in_2024_compatibility.rs", source, &[]), expected);
}

#[test]
fn ill_formed_inputs_name_the_valid_forms() {
    let source = r#"
#[inline = 5]
fn a() {}
#[ignore()]
fn b() {}
#[ignore = 1]
fn c() {}
fn main() {}
"#;
    assert_eq!(
        messages(source, &["ill_formed_attribute_input"]),
        [
            "ill_formed_attribute_input: valid forms for the attribute are `inline`, `inline(always)`, and `inline(never)`",
            "ill_formed_attribute_input: valid forms for the attribute are `ignore` and `ignore = \"reason\"`",
            "ill_formed_attribute_input: valid forms for the attribute are `ignore` and `ignore = \"reason\"`",
        ]
    );
}

#[test]
fn a_second_attribute_of_a_single_parser_is_unused() {
    let source = r#"
#[inline]
#[inline(always)]
fn a() {}
#[must_use]
fn b() -> u8 { 0 }
#[macro_use]
#[macro_use]
mod m {}
fn main() {}
"#;
    assert_eq!(messages(source, &["unused_attributes"]), ["unused_attributes: unused attribute"; 2]);
}

#[test]
fn an_attribute_on_a_macro_call_is_parsed_alone() {
    let source = r#"
macro_rules! m { () => {} }
fn main() {
    #[inline]
    #[inline]
    m!();
}
"#;
    assert_eq!(
        messages(source, &["unused_attributes"]),
        ["unused_attributes: the `inline` attribute cannot be used on macro calls"; 2]
    );
}

#[test]
fn empty_lists_are_unused() {
    let source = r#"
#![feature()]
#[macro_use()]
mod m {}
fn main() {}
"#;
    assert_eq!(messages(source, &["unused_attributes"]), ["unused_attributes: unused attribute"; 2]);
}

#[test]
fn unsafe_attributes_are_unsafe_code() {
    let source = r#"
#![deny(unsafe_code)]
#[unsafe(no_mangle)]
fn a() {}
#[unsafe(export_name = "bee")]
fn b() {}
#[inline]
fn c() {}
fn main() {}
"#;
    assert_eq!(
        messages(source, &["unsafe_code"]),
        [
            "unsafe_code: usage of the unsafe `no_mangle` attribute",
            "unsafe_code: usage of the unsafe `export_name` attribute",
        ]
    );
}

#[test]
fn derive_helpers_named_as_builtin_attributes_are_ambiguous() {
    let source = r#"
#[proc_macro_derive(Foo, attributes(inline, foo, must_use))]
pub fn foo(input: u8) -> u8 { input }
fn main() {}
"#;
    assert_eq!(
        messages(source, &["ambiguous_derive_helpers"]),
        ["ambiguous_derive_helpers: there exists a built-in attribute with the same name"; 2]
    );
}
