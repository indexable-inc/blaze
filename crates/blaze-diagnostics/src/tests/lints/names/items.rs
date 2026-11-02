//! The item lints of `lints::names::items` (naming, lifetimes, `#[no_mangle]`, privacy,
//! `improper_ctypes`, runtime symbols) against rustc's UI tests, and across two crates.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked, checked_with, compiletest, error, items::ui_core, normalized, ui, warning},
};

/// The UI test `items/<name>` against its `.stderr`, as compiletest runs it.
fn ui_test(name: &str, source: &str, expected: &str) {
    assert_eq!(ui(name, "2015", source), expected);
}

/// A `core` whose prelude has only rustc's built-in attribute `define_opaque`.
const DEFINE_OPAQUE_CORE: &str = r#"//- /core.rs crate:core edition:2021 library
#![feature(no_core, rustc_attrs, decl_macro)]
#![no_core]
pub mod prelude {
    pub mod rust_2015 {
        pub use crate::define_opaque;
    }
}
#[rustc_builtin_macro]
pub macro define_opaque($($tt:tt)*) {
    /* compiler built-in */
}
"#;

/// What rustc writes compiling a test that defines an opaque type with `#[define_opaque]`, from
/// std's prelude.
fn ui_opaque(name: &str, source: &str) -> String {
    let fixture = format!("{DEFINE_OPAQUE_CORE}//- /{name} crate:other:main edition:2015 deps:core\n{source}");
    ui_fixture(&fixture)
}

/// What rustc writes compiling `fixture` (its files named `$DIR/<path>`) as compiletest runs a
/// UI test that is not `run-pass`.
fn ui_fixture(fixture: &str) -> String {
    let options = compiletest(Mode::Other, &Options::default());
    let stderr = checked_with(fixture, "$DIR/", &options).stderr_with(Style { ui_testing: true, ..Style::default() });
    normalized(&stderr)
}

/// A `core` and a `std` of what the FFI tests use: `Option`, `ptr::null_mut`, the
/// `unreachable` and `todo` macros (as loops: no test runs them), and
/// `ffi` with `c_void` and the runtime's functions declared `#[rustc_canonical_symbol]` as
/// `library/core/src/ffi/mod.rs` declares them.
const FFI_STD: &str = r#"//- /core.rs crate:core edition:2021 library
#![feature(no_core, lang_items, rustc_attrs, fundamental)]
#![no_core]
pub mod marker {
    #[lang = "pointee_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait PointeeSized {}
    #[lang = "meta_sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait MetaSized: PointeeSized {}
    #[lang = "sized"]
    #[fundamental]
    #[rustc_specialization_trait]
    #[rustc_coinductive]
    pub trait Sized: MetaSized {}
}
pub mod option {
    #[lang = "Option"]
    pub enum Option<T> {
        #[lang = "None"]
        None,
        #[lang = "Some"]
        Some(T),
    }
}
pub mod ptr {
    pub const fn null_mut<T>() -> *mut T {
        0 as *mut T
    }
}
pub mod ffi {
    pub type c_char = i8;
    pub type c_int = i32;
    #[lang = "c_void"]
    #[repr(u8)]
    pub enum c_void {
        __variant1,
        __variant2,
    }
    mod runtime_symbols {
        use super::{c_char, c_int, c_void};
        unsafe extern "C" {
            #[rustc_canonical_symbol]
            fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
            #[rustc_canonical_symbol]
            fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
            #[rustc_canonical_symbol]
            fn memset(s: *mut c_void, c: c_int, n: usize) -> *mut c_void;
            #[rustc_canonical_symbol]
            fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;
            #[rustc_canonical_symbol]
            fn bcmp(s1: *const c_void, s2: *const c_void, n: usize) -> c_int;
            #[rustc_canonical_symbol]
            fn strlen(s: *const c_char) -> usize;
        }
    }
}
//- /std.rs crate:std edition:2021 deps:core library
#![no_std]
pub use core::{ffi, option, ptr};
#[macro_export]
macro_rules! unreachable {
    () => {
        loop {}
    };
}
#[macro_export]
macro_rules! todo {
    () => {
        loop {}
    };
}
pub mod prelude {
    pub mod rust_2015 {
        pub use crate::{todo, unreachable};
        pub use core::marker::Sized;
        pub use core::option::Option::{self, None, Some};
    }
    pub mod rust_2021 {
        pub use crate::{todo, unreachable};
        pub use core::marker::Sized;
        pub use core::option::Option::{self, None, Some};
    }
}
"#;

#[test]
fn lint_non_uppercase_statics() {
    ui_test(
        "lint-non-uppercase-statics.rs",
        include_str!("items/lint-non-uppercase-statics.rs"),
        include_str!("items/lint-non-uppercase-statics.stderr"),
    );
}

/// `variant_size_differences` at the variant more than three times the next largest, the tag
/// subtracted; not of an enum with generic parameters, nor under `#[allow]`.
#[test]
fn enum_size_variance() {
    ui_test(
        "enum-size-variance.rs",
        include_str!("items/enum-size-variance.rs"),
        include_str!("items/enum-size-variance.stderr"),
    );
}

#[test]
fn variant_size_differences() {
    ui_test(
        "variant-size-differences.rs",
        include_str!("items/variant-size-differences.rs"),
        include_str!("items/variant-size-differences.stderr"),
    );
}

#[test]
fn zero_uses_in_fn() {
    ui_test(
        "zero-uses-in-fn.rs",
        include_str!("items/zero-uses-in-fn.rs"),
        include_str!("items/zero-uses-in-fn.stderr"),
    );
}

#[test]
fn zero_uses_in_impl() {
    ui_test(
        "zero-uses-in-impl.rs",
        include_str!("items/zero-uses-in-impl.rs"),
        include_str!("items/zero-uses-in-impl.stderr"),
    );
}

#[test]
fn one_use_in_fn_argument() {
    ui_test(
        "one-use-in-fn-argument.rs",
        include_str!("items/one-use-in-fn-argument.rs"),
        include_str!("items/one-use-in-fn-argument.stderr"),
    );
}

#[test]
fn one_use_in_inherent_impl_header() {
    ui_test(
        "one-use-in-inherent-impl-header.rs",
        include_str!("items/one-use-in-inherent-impl-header.rs"),
        include_str!("items/one-use-in-inherent-impl-header.stderr"),
    );
}

#[test]
fn generic_no_mangle() {
    ui_test(
        "generic-no-mangle.rs",
        include_str!("items/generic-no-mangle.rs"),
        include_str!("items/generic-no-mangle.stderr"),
    );
}

#[test]
fn improper_ctypes_73251_1() {
    assert_reports(&ui_opaque("lint-73251-1.rs", include_str!("items/lint-73251-1.rs")), &[error(24, &["`Qux`", "opaque"])]);
}

#[test]
fn improper_ctypes_73249_2() {
    assert_reports(&ui_opaque("lint-73249-2.rs", include_str!("items/lint-73249-2.rs")), &[error(27, &["`Qux`", "opaque"])]);
}

/// A struct of another crate without `repr(C)` is not FFI-safe in an `extern` block or an
/// `extern "C"` fn of this one; rustc points at its definition only when it is local, and the
/// diagnostic reads the struct's representation.
#[test]
fn improper_ctypes_of_a_foreign_struct() {
    let fixture = r#"
//- /dep.rs crate:dep library
#[repr(C)]
pub struct Good { pub x: u8 }
pub struct Bad { pub x: u8 }
//- /main.rs crate:other:main edition:2021 deps:dep
#![allow(dead_code)]
use dep::{Bad, Good};
extern "C" {
    fn good(x: Good);
    fn bad(x: Bad);
}
pub extern "C" fn definition(_x: Bad) {}
fn main() {}
"#;
    let checked = checked(fixture);
    let messages = checked.messages();
    assert_eq!(
        messages,
        [
            "improper_ctypes: `extern` block uses type `Bad`, which is not FFI-safe",
            "improper_ctypes_definitions: `extern` fn uses type `Bad`, which is not FFI-safe",
        ],
        "{messages:?}"
    );
    let stderr = checked.stderr();
    assert!(!stderr.contains("the type is defined here"), "{stderr}");
    for diag in checked.diags.iter().filter(|it| !crate::tests::closing(it)) {
        assert!(diag.facts.iter().any(|it| it.what.contains("representation of") && it.what.contains("Bad")), "{diag:?}");
    }
}

/// rustc's `clashing_extern_declarations` compares the declarations of one crate only: the same
/// symbol declared differently by a dependency is no clash.
#[test]
fn clashing_extern_declarations_are_per_crate() {
    let fixture = r#"
//- /dep.rs crate:dep library edition:2021
extern "C" {
    pub fn clash(x: u8);
}
//- /main.rs crate:other:main edition:2021 deps:dep
#![allow(dead_code)]
extern "C" {
    fn clash(x: u16);
}
mod inner {
    extern "C" {
        fn clash(x: u32);
    }
}
fn main() {}
"#;
    let messages = checked(fixture).messages();
    assert_eq!(messages, ["clashing_extern_declarations: `clash` redeclared with a different signature"], "{messages:?}");
}

/// `tests/ui/lint/runtime-symbols.rs`, its `core` the symbols `core` declares canonical, with the
/// test's `normalize-stderr` of `*const i8`/`*const u8` applied.
#[test]
fn runtime_symbols() {
    let source = include_str!("items/runtime-symbols.rs");
    // A host target, as compiletest runs it: `bcmp`'s C-variadic definition is stable there.
    let fixture = format!(
        "{FFI_STD}//- /runtime-symbols.rs crate:other:main edition:2021 deps:core,std cfg:target_arch=aarch64\n{source}"
    );
    let stderr = ui_fixture(&fixture).replace("*const i8", "*const U8").replace("*const u8", "*const U8");
    assert_reports(
        &stderr,
        &[
            error(13, &["`memmove`"]),
            error(17, &["`memset`"]),
            error(20, &["`memcmp`"]),
            error(25, &["`strlen`"]),
            error(31, &["`strlen`"]),
            error(37, &["`memcpy`"]),
            error(44, &[]),
            error(51, &["`bcmp`"]),
            warning(57, &["`memcpy`"]),
            warning(63, &["`memmove`"]),
            warning(69, &["`memset`"]),
            warning(74, &["`bcmp`"]),
            warning(80, &["`strlen`"]),
        ],
    );
}

/// A definition of a runtime symbol with the runtime's signature, a `#[no_mangle]` function
/// whose symbol is no runtime's, and a mangled `strlen` give nothing; the lint reads the
/// signatures of both functions.
#[test]
fn runtime_symbols_matching_and_mangled() {
    let source = r#"
#![allow(dead_code, unused_variables)]
use core::ffi::{c_char, c_void};
#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> usize { 0 }
#[no_mangle]
pub extern "C" fn not_a_runtime_symbol() {}
mod inner {
    pub fn memcpy() {}
}
#[no_mangle]
pub extern "C" fn memset(s: *mut c_void, c: i64, n: usize) -> *mut c_void { s }
fn main() {}
"#;
    let fixture = format!("{FFI_STD}//- /main.rs crate:other:main edition:2021 deps:core,std\n{source}");
    let checked = checked(&fixture);
    let messages = checked.messages();
    assert_eq!(
        messages,
        ["suspicious_runtime_symbol_definitions: suspicious definition of the runtime `memset` symbol used by the standard library"],
        "{messages:?}"
    );
    let facts: Vec<&str> = checked.diags[0].facts.iter().map(|it| it.what.as_str()).collect();
    assert!(facts.contains(&"the signature of the runtime `memset`"), "{facts:?}");
    assert!(facts.contains(&"the signature of `memset`"), "{facts:?}");
}

/// `tests/ui/lint/c-void-returns.rs`: a free function, a foreign function and a `fn` pointer
/// type returning `c_void`.
#[test]
fn c_void_returns() {
    let source = include_str!("items/c-void-returns.rs");
    let fixture = format!("{FFI_STD}//- /c-void-returns.rs crate:other:main edition:2015 deps:core,std\n{source}");
    assert_eq!(ui_fixture(&fixture), include_str!("items/c-void-returns.stderr"));
}

/// A method and a closure returning `c_void` get the message of a foreign declaration, as
/// rustc's `FnKind` other than a Rust-ABI item function does; `#[allow]` on the closure's
/// statement allows its lint.
#[test]
fn c_void_returns_of_methods_and_closures() {
    let source = r#"
#![allow(dead_code)]
use std::ffi::c_void;
struct S;
impl S {
    fn m() -> c_void { unreachable!() }
}
fn f() {
    let _a = || -> c_void { unreachable!() };
    #[allow(c_void_returns)]
    let _b = || -> c_void { unreachable!() };
}
fn main() {}
"#;
    let fixture = format!("{FFI_STD}//- /main.rs crate:other:main edition:2021 deps:core,std\n{source}");
    let messages = checked(&fixture).messages();
    let extern_ = "c_void_returns: declarations returning `c_void` are not compatible with C functions returning `void`";
    assert_eq!(messages, [extern_, extern_], "{messages:?}");
}

#[test]
fn mismatched_lifetime_syntaxes_missing_lifetime_kind() {
    ui_test(
        "missing-lifetime-kind.rs",
        include_str!("items/missing-lifetime-kind.rs"),
        include_str!("items/missing-lifetime-kind.stderr"),
    );
}

/// `&'static self` gives the return type's elided lifetime, and the suggestion names it.
#[test]
fn mismatched_lifetime_syntaxes_example_from_issue48686() {
    ui_test(
        "example-from-issue48686.rs",
        include_str!("items/example-from-issue48686.rs"),
        include_str!("items/example-from-issue48686.stderr"),
    );
}

/// No lint where the lifetimes agree, where the return type's lifetimes are a `fn` pointer's
/// own, or where `dyn Trait + '_` is the only other syntax; `impl Trait + '_` and a provided
/// trait method are (twice, as rustc's `check_trait_item` and `check_fn` both lint it, which
/// only `-Z deduplicate-diagnostics=no` shows).
#[test]
fn mismatched_lifetime_syntaxes_scopes() {
    let source = r#"
#![allow(dead_code)]
trait Tr {}
struct S<'a>(&'a u8);
fn agree(x: &u8) -> &u8 { x }
fn named<'a>(x: &'a u8) -> S<'a> { S(x) }
fn fn_ptr(_: &u8) -> fn(S) -> S { loop {} }
fn object<'a>(_: &'a u8) -> Box<dyn Tr + '_> { loop {} }
fn outlives<'a>(x: &'a u8) -> impl Tr + '_ { x }
impl Tr for &u8 {}
struct Box<T: ?Sized>(*const T);
trait Provided {
    fn get<'a>(&self, x: &'a u8) -> S<'a> { S(x) }
    fn mixed<'a>(&'a self) -> S { loop {} }
}
fn main() {}
"#;
    let fixture = format!("{FFI_STD}//- /main.rs crate:other:main edition:2021 deps:core,std\n{source}");
    let messages = checked(&fixture).messages();
    let named = "mismatched_lifetime_syntaxes: hiding a lifetime that's named elsewhere is confusing";
    let eliding = "mismatched_lifetime_syntaxes: eliding a lifetime that's named elsewhere is confusing";
    assert_eq!(messages, [eliding, named], "{messages:?}");
    let options = compiletest(Mode::Other, &Options::default());
    let kept = checked_with(&fixture, "$DIR/", &options).messages();
    assert_eq!(kept, [eliding, named, named], "{kept:?}");
}

/// `tests/ui/async-await/track-caller/panic-track-caller.rs`'s `nofeat` revision, of a free
/// `async fn`: the attribute, the function, and the feature's notes.
#[test]
fn ungated_async_fn_track_caller() {
    let source = r#"#[track_caller]
async fn bar_track_caller() {}

fn main() {}
"#;
    let expected = r#"warning: `#[track_caller]` on async functions is a no-op
  --> $DIR/track-caller.rs:1:1
   |
LL | #[track_caller]
   | ^^^^^^^^^^^^^^^
LL | async fn bar_track_caller() {}
   | ------------------------------ this function will not propagate the caller location
   |
   = note: see issue #110011 <https://github.com/rust-lang/rust/issues/110011> for more information
   = help: add `#![feature(async_fn_track_caller)]` to the crate attributes to enable
   = note: this compiler was built on YYYY-MM-DD; consider upgrading it if it is out of date
   = note: `#[warn(ungated_async_fn_track_caller)]` on by default

warning: 1 warning emitted

"#;
    assert_eq!(ui_core("track-caller.rs", "2021", "future", source), expected);
}

/// Methods are linted too; a function that is not `async`, a required trait method and a
/// crate with the feature are not.
#[test]
fn ungated_async_fn_track_caller_of_methods_and_with_the_feature() {
    let source = r#"
#![allow(dead_code)]
struct Foo;
impl Foo {
    #[track_caller]
    async fn bar_assoc() {}
}
trait Tr {
    #[track_caller]
    async fn required();
}
#[track_caller]
fn not_async() {}
fn main() {}
"#;
    let fixture = format!("//- minicore: future\n//- /main.rs crate:other:main edition:2021\n{source}");
    let messages = checked(&fixture).messages();
    assert_eq!(messages, ["`#[track_caller]` on async functions is a no-op"], "{messages:?}");
    let gated = format!("//- minicore: future\n//- /main.rs crate:other:main edition:2021\n#![feature(async_fn_track_caller)]\n{source}");
    let messages = checked(&gated).messages();
    assert!(!messages.iter().any(|it| it.contains("no-op")), "{messages:?}");
}

#[test]
fn non_local_definitions_consts() {
    assert_eq!(ui("consts.rs", "2021", include_str!("items/consts.rs")), include_str!("items/consts.stderr"));
}

#[test]
fn non_local_definitions_inside_macro_rules() {
    let source = include_str!("items/inside-macro_rules.rs");
    assert_eq!(ui("inside-macro_rules.rs", "2021", source), include_str!("items/inside-macro_rules.stderr"));
}

/// The `check-pass` tests without a `.stderr`: modules and anonymous constants are transparent.
#[test]
fn non_local_definitions_transparent_modules_and_const_anons() {
    let tests = [
        ("module_as_local.rs", include_str!("items/module_as_local.rs")),
        ("convoluted-locals-131474.rs", include_str!("items/convoluted-locals-131474.rs")),
        ("convoluted-locals-131474-with-mods.rs", include_str!("items/convoluted-locals-131474-with-mods.rs")),
        ("convoluted-locals-132427.rs", include_str!("items/convoluted-locals-132427.rs")),
    ];
    for (name, source) in tests {
        assert_eq!(ui(name, "2015", source), "", "{name}");
    }
}

/// A `#[macro_export]` `macro_rules!` in a function, in a method (up 2 bodies) and in a
/// `const _`, which is ignored at the top level.
#[test]
fn non_local_definitions_macro_rules() {
    let source = r#"
#![allow(dead_code)]
const _: () = {
    #[macro_export]
    macro_rules! ignored { () => {} }
};
fn main() {
    #[macro_export]
    macro_rules! m { () => {} }
    struct InsideMain;
    impl InsideMain {
        fn bar() {
            #[macro_export]
            macro_rules! m2 { () => {} }
        }
    }
}
"#;
    let fixture = format!("//- /main.rs crate:other:main edition:2021\n{source}");
    let checked = checked(&fixture);
    let messages = checked.messages();
    let message = "non_local_definitions: non-local `macro_rules!` definition, `#[macro_export]` macro should be written at top level module";
    assert_eq!(messages, [message, message], "{messages:?}");
    let helps: Vec<&str> = checked.diags.iter().flat_map(|it| &it.children).map(|it| it.message.as_str()).filter(|it| it.starts_with("remove")).collect();
    assert_eq!(
        helps,
        [
            "remove the `#[macro_export]` or move this `macro_rules!` outside the of the current function `main`",
            "remove the `#[macro_export]` or move this `macro_rules!` outside the of the current associated function `bar` and up 2 bodies",
        ]
    );
}
