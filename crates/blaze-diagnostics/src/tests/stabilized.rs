//! The language changes of Rust 1.80 to 1.100 that decide whether a program builds (the
//! "Language" and "Compatibility Notes" sections of rustc's RELEASES.md, and
//! `rustc_feature`'s `accepted.rs` at the pinned nightly): each as a small crate blaze checks
//! and says nothing about, or says what rustc's own UI test at the pin says where a release
//! added a diagnostic. `superset-of-rust`: every program the pinned nightly accepts, blaze
//! accepts.
//!
//! What these changes do when the program runs (drop order, layout, const evaluation,
//! coercions) is hir-ty-mir's `tests/mir/stabilized.rs`.

use super::{
    Mode,
    items::{ui_core, ui_fixture},
};
use crate::Options;

/// One program: `name` (the feature, for the failure's report), the fixture's meta after the
/// edition (`2021 cfg:target_arch=aarch64`), minicore's flags, the source.
type Case = (&'static str, &'static str, &'static str, &'static str);

/// Each of `cases` is checked with nothing said: no error and no warning.
#[track_caller]
fn accepted(cases: &[Case]) {
    // alloc: a test's report
    let mut refused = String::new();
    for (name, edition, flags, source) in cases {
        let said = ui_core(&format!("{name}.rs"), edition, flags, source);
        if !said.is_empty() {
            refused.push_str(&format!("=== {name}:\n{said}\n"));
        }
    }
    assert!(refused.is_empty(), "not accepted with nothing said:\n{refused}");
}

/// What rustc's UI test `name` says, as compiletest runs a `check-pass` test in `edition`
/// beside minicore's `flags`.
fn ui_test(name: &str, edition: &str, flags: &str, source: &str) -> String {
    super::ui_core_as(Mode::Other, name, edition, flags, source, &Options::default())
}

// Rust 1.100

/// `!` is a type like any other, with no `#![feature(never_type)]`: an argument, a field's,
/// an associated type, behind a reference, a pointer, an array, a tuple and a function
/// pointer, a generic argument written and inferred, and the `Self` of an impl
/// (`never_type`, accepted in 1.100, rust-lang/rust#155499).
#[test]
fn v1_100_the_never_type_is_a_type_anywhere() {
    accepted(&[
        (
            "never_type_positions",
            "2021",
            "sized, result, option, copy",
            r#"
struct Wrap<T>(T);
struct Field { never: Option<!>, by_ref: &'static !, ptr: *const !, array: [!; 0] }
trait Tr { type Out; fn make() -> Self::Out; }
impl Tr for u8 { type Out = !; fn make() -> ! { loop {} } }
type Never = !;
const NONE: Option<!> = None;
static ALSO: Option<Never> = None;
fn irrefutable(x: Result<u8, !>) -> u8 { let Ok(v) = x; v }
fn positions(_: Wrap<!>, _: &!, _: *const !, _: [!; 0], _: (u8, !), _: fn(!) -> !, _: Field) {}
fn generic<T>() -> Option<T> { None }
fn main() { let _: Option<!> = generic::<!>(); let _ = generic::<Never>(); let _ = (NONE, ALSO); }
"#,
        ),
        (
            "never_type_impls",
            "2021",
            "sized, copy, clone, option",
            r#"
trait Show { fn show(&self) -> u8; }
impl Show for ! { fn show(&self) -> u8 { *self } }
fn takes<T: Show + Copy>(_: Option<T>) {}
fn main() { takes::<!>(None); }
"#,
        ),
        (
            "never_type_patterns",
            "2021",
            "sized, result, copy",
            r#"
fn by_value(x: Result<u8, !>) -> u8 { match x { Ok(v) => v } }
fn by_ref(x: &Result<u8, !>) -> u8 { match x { Ok(v) => *v, Err(e) => *e } }
fn main() {}
"#,
        ),
        // `core::convert::Infallible` is `!` since 1.100: an alias of it is the same type.
        (
            "infallible_is_an_alias_of_never",
            "2021",
            "sized, result",
            r#"
pub type Infallible = !;
fn same(x: Result<u8, Infallible>) -> Result<u8, !> { x }
fn back(x: Result<u8, !>) -> Result<u8, Infallible> { x }
impl From<Infallible> for Error { fn from(x: Infallible) -> Error { x } }
trait From<T> { fn from(x: T) -> Self; }
struct Error;
fn main() {}
"#,
        ),
    ]);
}

/// An unconstrained type variable a coercion of `!` made falls back to `!` in every edition
/// (1.100's compatibility note: "the never type fallback was changed to `!` on all
/// editions"): `x` is a `!`, which only a fallback to `!` lets `let _: ! = x` take.
#[test]
fn v1_100_the_never_fallback_is_never_in_every_edition() {
    let source = r#"
fn unconstrained<T>() -> Option<T> { None }
fn main() {
    let x = match unconstrained() { Some(v) => v, None => return };
    let _: ! = x;
}
"#;
    accepted(&[
        ("never_fallback_2015", "2015", "sized, option", source),
        ("never_fallback_2018", "2018", "sized, option", source),
        ("never_fallback_2021", "2021", "sized, option", source),
        ("never_fallback_2024", "2024", "sized, option", source),
    ]);
}

/// `extern "custom"` (`abi_custom`, accepted in 1.100, rust-lang/rust#158504): a naked
/// definition, a declaration and a function pointer type.
#[test]
fn v1_100_extern_custom() {
    accepted(&[(
        "abi_custom",
        "2021 cfg:target_arch=aarch64",
        "sized, asm",
        r#"
#[unsafe(naked)]
pub unsafe extern "custom" fn f() { core::arch::naked_asm!("ret") }
unsafe extern "custom" { fn g(); }
type P = unsafe extern "custom" fn();
fn main() { let _p: P = f; let _q: P = g; }
"#,
    )]);
}

/// Every lint Rust 1.80 to 1.100 added is one blaze knows by name: allowing them all under
/// `#![deny(warnings)]` says nothing (an unknown one is `unknown_lints`, an error there).
#[test]
fn v1_100_the_lints_of_1_80_to_1_100_are_known_by_name() {
    accepted(&[(
        "lint_names",
        "2021",
        "sized",
        r#"
#![deny(warnings)]
#![allow(
    // 1.100
    aligned_fields_in_packed, repeated_reprs,
    // 1.99
    raw_borrows_via_references, invalid_runtime_symbol_definitions,
    suspicious_runtime_symbol_definitions, unreachable_cfg_select_predicates,
    method_call_on_diverging_infer_var, semicolon_in_expressions_from_non_local_macros,
    // 1.98
    c_void_returns,
    // 1.97
    dead_code_pub_in_binary,
    // 1.94
    unused_visibilities,
    // 1.93
    const_item_interior_mutations, function_casts_as_integer,
    // 1.91
    dangling_pointers_from_locals, integer_to_ptr_transmutes,
    // 1.90
    unknown_diagnostic_attributes, misplaced_diagnostic_attributes,
    malformed_diagnostic_attributes, malformed_diagnostic_format_literals,
    // 1.89
    mismatched_lifetime_syntaxes,
    // 1.88
    dangerous_implicit_autorefs, invalid_null_arguments,
    // 1.86
    double_negations, missing_abi,
    // 1.85
    unpredictable_function_pointer_comparisons, if_let_rescope, tail_expr_drop_order,
    impl_trait_overcaptures, unsafe_attr_outside_unsafe, missing_unsafe_on_extern,
    deprecated_safe_2024, rust_2024_guarded_string_incompatible_syntax,
    rust_2024_prelude_collisions, boxed_slice_into_iter, keyword_idents_2024,
    edition_2024_expr_fragment_specifier, never_type_fallback_flowing_into_unsafe,
    rust_2024_incompatible_pat, closure_returning_async_block, rust_2024_compatibility,
    // 1.84
    dangling_pointers_from_temporaries,
    // 1.83
    non_local_definitions,
    // the pinned nightly's own
    unstable_imports, x86_softfloat_sse,
)]
fn main() {}
"#,
    )]);
}

/// tests/ui/repr/repr-packed-contains-align.rs (rust-lang/rust#162160): a `repr(C, packed)`
/// type holding a `repr(C, align)` one through `repr(C)` types is the deny-by-default lint
/// `aligned_fields_in_packed`, E0588 before 1.100; with other reprs it is nothing.
#[test]
fn v1_100_an_aligned_type_in_a_packed_one_is_a_lint() {
    assert_eq!(
        ui_test(
            "repr-packed-contains-align.rs",
            "2015",
            "sized, copy, clone, derive",
            include_str!("stabilized/repr-packed-contains-align.rs")
        ),
        include_str!("stabilized/repr-packed-contains-align.stderr")
    );
    accepted(&[(
        "aligned_fields_in_packed_allowed",
        "2021",
        "sized",
        r#"
#[repr(C, align(16))] struct Aligned(i32);
#[allow(aligned_fields_in_packed)]
#[repr(C, packed)] struct Packed(u8, Aligned);
#[repr(align(8))] struct RustAligned(u8);
#[repr(packed)] struct RustPacked { a: RustAligned, b: u8 }
fn main() {}
"#,
    )]);
}

/// `dependency_on_unit_never_type_fallback` went with the unit fallback (1.100): naming it is
/// `renamed_and_removed_lints`, as rustc's `register_removed` has it.
#[test]
fn v1_100_the_unit_fallback_lint_is_removed() {
    let said = ui_core(
        "removed.rs",
        "2021",
        "sized",
        "#![allow(dependency_on_unit_never_type_fallback)]\nfn main() {}\n",
    );
    assert!(
        said.starts_with(
            "warning: lint `dependency_on_unit_never_type_fallback` has been removed: the code warned by this lint no longer compiles\n"
        ),
        "{said}"
    );
    assert!(said.contains("`#[warn(renamed_and_removed_lints)]` on by default"), "{said}");
}

/// tests/ui/let-else/detect-invisible-delimiter.rs (1.100's compatibility note,
/// rust-lang/rust#158515): a `let ... else` whose initializer is a macro's `expr`
/// metavariable needs no parentheses around `true && true` or `Thing {}`.
#[test]
fn v1_100_let_else_takes_an_expr_metavariable_as_grouped() {
    let source = r#"// The user shouldn't need to wrap the expression in parentheses(#147899)
//@check-pass
#![allow(irrefutable_let_patterns)]
struct Thing {}
macro_rules! foo {
    ($e:expr) => {
        let _ = $e else {
            return;
        };
    };
}

fn main() {
    foo!(true && true);
    foo!(Thing {});
}
"#;
    assert_eq!(ui_test("detect-invisible-delimiter.rs", "2015", "sized, copy", source), "");
}

// Rust 1.99

/// tests/ui/never_type/basic/clone-never.rs and method-on-never.rs (rust-lang/rust#156047): a
/// method called on a type variable a coercion of `!` made is the method of `!`, with the
/// future-compatibility warning `method_call_on_diverging_infer_var`; E0282 before 1.99.
#[test]
fn v1_99_a_method_on_a_diverging_variable_is_nevers() {
    assert_eq!(
        ui_test(
            "clone-never.rs",
            "2015",
            "sized, clone, panic, builtin_impls",
            include_str!("stabilized/clone-never.rs")
        ),
        include_str!("stabilized/clone-never.stderr")
    );
    assert_eq!(
        ui_test(
            "method-on-never.rs",
            "2015",
            "sized, send, sync, result, try, from, todo, builtin_impls",
            include_str!("stabilized/method-on-never.rs")
        ),
        include_str!("stabilized/method-on-never.stderr")
    );
}

/// C-variadic function definitions (`c_variadic`, accepted in 1.99, rust-lang/rust#155697):
/// the list is a `core::ffi::VaList`, named or not; and a naked one with any ABI a
/// C-variadic declaration may have (`c_variadic_naked_functions`, rust-lang/rust#159746).
#[test]
fn v1_99_c_variadic_definitions() {
    accepted(&[
        (
            "c_variadic_definitions",
            "2021 cfg:target_arch=aarch64",
            "sized, copy, add, builtin_impls",
            r#"
#![feature(lang_items)]
#[lang = "va_list"]
pub struct VaList<'f>(&'f ());
impl<'f> VaList<'f> { pub unsafe fn arg<T>(&mut self) -> T { loop {} } }
pub unsafe extern "C" fn named(n: i32, mut ap: ...) -> i32 { unsafe { ap.arg::<i32>() + n } }
pub unsafe extern "C" fn unnamed(n: i32, _: ...) -> i32 { n }
unsafe extern "C" fn only_the_list(_: ...) {}
unsafe extern "C-unwind" fn unwinding(mut ap: ...) -> i32 { unsafe { ap.arg::<i32>() } }
fn main() { unsafe { named(1, 2i32); unnamed(1); only_the_list(1u64, 2.0f64); unwinding(3i32); } }
"#,
        ),
        (
            "c_variadic_naked",
            "2021 cfg:target_arch=x86_64",
            "sized, asm",
            r#"
#![feature(lang_items)]
#[lang = "va_list"]
pub struct VaList<'f>(&'f ());
#[unsafe(naked)]
pub unsafe extern "sysv64" fn sysv(n: i32, _: ...) -> i32 { core::arch::naked_asm!("ret") }
#[unsafe(naked)]
pub unsafe extern "win64" fn win(n: i32, _: ...) -> i32 { core::arch::naked_asm!("ret") }
#[unsafe(naked)]
pub unsafe extern "efiapi" fn efi(n: i32, _: ...) -> i32 { core::arch::naked_asm!("ret") }
#[unsafe(naked)]
pub unsafe extern "C" fn c(n: i32, _: ...) -> i32 { core::arch::naked_asm!("ret") }
fn main() {}
"#,
        ),
    ]);
}

/// tests/ui/sized/infer_var_subtyping.rs (rust-lang/rust#157820): a type variable related by
/// subtyping to one known `Sized` coerces to a trait object. `Box` is the test's own here.
#[test]
fn v1_99_a_variable_sized_through_subtyping_coerces_to_a_trait_object() {
    accepted(&[(
        "infer_var_subtyping",
        "2021",
        "sized, send, coerce_unsized, dispatch_from_dyn",
        r#"
struct Box<T: ?Sized>(*const T);
impl<T> Box<T> { fn new(x: T) -> Box<T> { loop {} } }
impl<T: ?Sized + core::marker::Unsize<U>, U: ?Sized> core::ops::CoerceUnsized<Box<U>> for Box<T> {}
fn blah(e: !) {
    let source = Box::new(e);
    let _: Box<dyn Send> = source;
}
fn main() {}
"#,
    )]);
}

/// `#[my_macro] mod foo;` (rust-lang/rust#157857): an attribute macro on a module that is
/// in another file.
#[test]
fn v1_99_an_attribute_macro_on_an_outlined_module() {
    let fixture = r#"//- proc_macros: identity
//- /main.rs crate:other:main edition:2021
#[proc_macros::identity]
mod foo;
fn main() { foo::x(); }
//- /foo.rs
pub fn x() {}
"#;
    // The fixture's proc-macro crate is checked too and has no `proc_macro` to name.
    let said = ui_fixture(fixture);
    assert!(!said.contains("$DIR/main.rs") && !said.contains("$DIR/foo.rs"), "{said}");
}

/// rust-lang/rust#156508: an anonymous lifetime in the type of an associated constant is
/// `'static`, with no lint; rust-lang/rust#159525: a 128-bit integer in an `xmm` register of
/// `asm!`.
#[test]
fn v1_99_anonymous_lifetimes_of_associated_consts_and_u128_in_xmm() {
    accepted(&[
        (
            "anonymous_lifetimes_of_associated_consts",
            "2021",
            "sized",
            r#"
#![deny(warnings)]
trait Tr { const C: &'static str; }
struct S<'a>(&'a u8);
impl<'a> S<'a> { const X: &str = "a"; const Y: &'a u8 = &1; }
impl Tr for () { const C: &str = "x"; }
fn main() { let _ = (S::X, <() as Tr>::C); }
"#,
        ),
        (
            "u128_in_xmm",
            "2021 cfg:target_arch=x86_64",
            "sized, asm, copy",
            r#"
fn main() {
    let x: u128 = 1;
    let y: u128;
    unsafe { core::arch::asm!("movaps {1}, {0}", in(xmm_reg) x, out(xmm_reg) y); }
    let _ = y;
}
"#,
        ),
    ]);
}

// Rust 1.98

/// rust-lang/rust#149219: an unsizing coercion of a `&mut` may shorten its lifetime, also
/// where the reference is in an invariant position.
#[test]
fn v1_98_unsizing_a_mut_reference_may_shorten_its_lifetime() {
    accepted(&[(
        "unsize_mut_shorten",
        "2021",
        "sized, send, coerce_unsized, dispatch_from_dyn",
        r#"
use core::ops::CoerceUnsized;
struct Invariant<T>(*mut T);
impl<T: CoerceUnsized<U>, U> CoerceUnsized<Invariant<U>> for Invariant<T> {}
fn plain<'long: 'short, 'short>(r: &'long mut i32) -> &'short mut dyn Send { r }
fn invariant<'long: 'short, 'short>(c: Invariant<&'long mut i32>) -> Invariant<&'short mut dyn Send> { c }
fn main() {}
"#,
    )]);
}

// Rust 1.97

#[test]
fn v1_97_trailing_self_in_imports_and_the_atomic_alignment_cfg() {
    accepted(&[
        // rust-lang/rust#155137: `use a::b::self;` is `use a::b::{self};`.
        (
            "trailing_self_imports",
            "2021",
            "sized",
            r#"
mod a { pub mod b { pub fn f() {} } }
use a::b::self;
use a::b::self as c;
use a::{b::self as d};
fn main() { b::f(); c::f(); d::f(); }
"#,
        ),
        // `cfg(target_has_atomic_equal_alignment)` (rust-lang/rust#155006).
        (
            "cfg_target_has_atomic_equal_alignment",
            "2021",
            "sized",
            r#"
#[cfg(target_has_atomic_equal_alignment = "32")] fn f() {}
#[cfg(not(target_has_atomic_equal_alignment = "32"))] fn f() {}
#[cfg(target_has_atomic_equal_alignment = "ptr")] fn g() {}
#[cfg(not(target_has_atomic_equal_alignment = "ptr"))] fn g() {}
fn main() { f(); g(); }
"#,
        ),
    ]);
}

// Rust 1.96

#[test]
fn v1_96_expr_metavariables_in_cfg_never_in_tuples_and_structural_constants() {
    accepted(&[
        // rust-lang/rust#146961: an `expr` metavariable as a `cfg` predicate.
        (
            "cfg_expr_metavariable",
            "2021",
            "sized",
            r#"
macro_rules! m { ($e:expr) => { #[cfg($e)] fn yes() {} #[cfg(not($e))] fn yes() {} }; }
m!(true);
macro_rules! n { ($e:expr) => { #[cfg(any($e, false))] fn no() {} }; }
n!(true);
fn main() { yes(); no(); }
"#,
        ),
        // rust-lang/rust#147834: each element of a tuple expression is a coercion site, with
        // or without an expected type.
        (
            "never_coerces_in_tuples",
            "2021",
            "sized",
            r#"
#![allow(unreachable_code)]
fn first() -> (u8, u8) { let x: ! = loop {}; (x, 1) }
fn second(x: !) -> (u8, &'static str) { (1, x) }
fn main() { ((),) = (loop {},); }
"#,
        ),
        // rust-lang/rust#154891, the rule the library fix rests on (`ManuallyDrop` derives
        // `PartialEq` over a `MaybeDangling` whose `StructuralPartialEq` impl is written): a
        // constant of such a type is a pattern.
        (
            "written_structural_partial_eq_in_a_constant_pattern",
            "2021",
            "sized, eq, ord, derive, copy, builtin_impls",
            r#"
#![feature(structural_match)]
use core::marker::StructuralPartialEq;
struct MaybeDangling<P>(P);
impl<P: PartialEq> PartialEq for MaybeDangling<P> { fn eq(&self, o: &Self) -> bool { self.0 == o.0 } }
impl<P> StructuralPartialEq for MaybeDangling<P> {}
#[derive(PartialEq)]
struct ManuallyDrop<T> { value: MaybeDangling<T> }
const C: ManuallyDrop<u32> = ManuallyDrop { value: MaybeDangling(1) };
fn f(x: ManuallyDrop<u32>) -> bool { match x { C => true, _ => false } }
fn main() {}
"#,
        ),
    ]);
}

// Rust 1.95

#[test]
fn v1_95_if_let_guards_and_keyword_imports() {
    accepted(&[
        // `if_let_guard` (rust-lang/rust#141295), with a chain.
        (
            "if_let_guard",
            "2021",
            "sized, option, copy, ord",
            r#"
fn f(x: Option<Option<u8>>) -> u8 {
    match x {
        Some(y) if let Some(z) = y && z > 1 => z,
        Some(_) if let None = None::<u8> => 0,
        _ => 9,
    }
}
fn main() {}
"#,
        ),
        // rust-lang/rust#146972: `crate`, `super`, `self` and `$crate` imported under a name.
        (
            "keyword_imports_renamed",
            "2021",
            "sized",
            r#"
mod m { pub fn f() {} pub mod n { use super as sup; use self as this; pub fn g() { sup::f(); this::h(); } pub fn h() {} } }
use crate as krate;
use {crate as k2, self as me};
macro_rules! mm { () => { use $crate as dc; fn via() { dc::m::f(); } } }
mm!();
fn main() { krate::m::f(); k2::m::f(); me::m::f(); via(); m::n::g(); }
"#,
        ),
        // rust-lang/rust#146832: an irrefutable `let` in a chain is not linted.
        (
            "irrefutable_let_in_a_chain",
            "2024",
            "sized, option, copy",
            r#"
#![deny(irrefutable_let_patterns)]
fn f(x: Option<u8>) { if let y = x && let Some(_) = y {} }
fn main() {}
"#,
        ),
    ]);
}

// Rust 1.94

/// rust-lang/rust#148321: identifiers are Unicode 17's. U+323B0 is of CJK Extension J, a
/// block Unicode 15.1 does not have; the lexer's tables are `unicode-ident` 1.0.24's
/// (`UNICODE_VERSION` 17.0.0), the version rustc's Cargo.lock has at the pin.
#[test]
fn v1_94_identifiers_are_unicode_17s() {
    accepted(&[(
        "unicode_17_identifier",
        "2021",
        "sized",
        "#![allow(uncommon_codepoints)]\nfn main() { let \u{323B0} = 1; let _ = \u{323B0}; }\n",
    )]);
}

// Rust 1.93

#[test]
fn v1_93() {
    accepted(&[
        // `extern_system_varargs` (rust-lang/rust#145954).
        (
            "system_variadic_declaration",
            "2021",
            "sized",
            "unsafe extern \"system\" { fn f(x: i32, ...); }\nfn main() {}\n",
        ),
        // `asm_cfg` (rust-lang/rust#147736): `#[cfg]` on a template and an operand of `asm!`.
        (
            "asm_cfg",
            "2021 cfg:target_arch=aarch64",
            "sized, asm",
            r#"
fn main() { unsafe { core::arch::asm!("nop", #[cfg(false)] "bad", #[cfg(false)] in("x9") 1u64, ); } }
"#,
        ),
        // rust-lang/rust#148602: the LUB of function items of differing safety is an unsafe
        // function pointer.
        (
            "lub_of_fn_items_of_differing_safety",
            "2021",
            "sized, copy",
            r#"
fn a() {} unsafe fn b() {}
fn main() {
    let _x: unsafe fn() = if true { a } else { b };
    let _y: [unsafe fn(); 2] = [a, b];
    let _z = match 1 { 0 => a, _ => b };
}
"#,
        ),
        // rust-lang/rust#148746: a `const` holding a mutable reference to a `static`.
        (
            "const_with_a_mutable_reference_to_a_static",
            "2021",
            "sized",
            r#"
static mut S: i32 = 0;
const C: &mut i32 = unsafe { &mut *(&raw mut S) };
fn main() {}
"#,
        ),
    ]);
}

// Rust 1.92

#[test]
fn v1_92() {
    accepted(&[
        // rust-lang/rust#141469: `&raw` of a union's field needs no `unsafe`.
        (
            "raw_borrow_of_a_union_field",
            "2021",
            "sized",
            r#"
union U { a: u8, b: u32 }
fn main() { let mut u = U { a: 1 }; let _p = &raw const u.a; let _q = &raw mut u.b; }
"#,
        ),
        // rust-lang/rust#146593: several bounds on one associated item.
        (
            "several_bounds_on_one_associated_item",
            "2021",
            "sized, iterator, copy, send",
            r#"
fn f<T: Iterator<Item: Copy, Item: Send>>(_: T) {}
fn g(_: impl Iterator<Item: Copy, Item: Send>) {}
fn main() {}
"#,
        ),
        // rust-lang/rust#145724: `#[track_caller]` with `#[no_mangle]`.
        (
            "track_caller_with_no_mangle",
            "2021",
            "sized",
            "#[track_caller] #[unsafe(no_mangle)] pub extern \"Rust\" fn f() {}\nfn main() {}\n",
        ),
        // rust-lang/rust#144064: an associated type's item bound is taken before a
        // where-bound for an auto trait.
        (
            "item_bounds_before_where_bounds",
            "2021",
            "sized, send",
            r#"
trait Tr { type A: Send; }
fn needs<T: Send>() {}
fn f<T: Tr>() where T::A: Send { needs::<T::A>() }
fn main() {}
"#,
        ),
    ]);
}

// Rust 1.89 to 1.91

#[test]
fn v1_89_to_1_91() {
    accepted(&[
        // 1.91 `extended_varargs_abi_support` (rust-lang/rust#144066).
        (
            "variadic_declarations_of_other_abis",
            "2021 cfg:target_arch=x86_64",
            "sized",
            r#"
unsafe extern "sysv64" { fn a(x: i32, ...); }
unsafe extern "win64" { fn b(x: i32, ...); }
unsafe extern "efiapi" { fn c(x: i32, ...); }
fn main() {}
"#,
        ),
        // 1.91 rust-lang/rust#139451: `target_env = "macabi"` and `"sim"` are known values.
        (
            "target_env_macabi_and_sim",
            "2021",
            "sized",
            r#"
#![deny(unexpected_cfgs)]
#[cfg(not(target_env = "macabi"))] fn f() {}
#[cfg(not(target_env = "sim"))] fn g() {}
fn main() { f(); g(); }
"#,
        ),
        // 1.90 rust-lang/rust#140942: a constant whose value refers to mutable or external
        // memory.
        (
            "constants_referring_to_mutable_memory",
            "2021",
            "sized, cell, sync",
            r#"
use core::cell::UnsafeCell;
struct Sy(UnsafeCell<i32>); unsafe impl Sync for Sy {}
static S: Sy = Sy(UnsafeCell::new(0));
static mut M: i32 = 1;
const C: &Sy = &S;
const D: *const i32 = &raw const M;
const E: &i32 = unsafe { &*(&raw const M) };
fn main() {}
"#,
        ),
        // 1.89 `generic_arg_infer` (rust-lang/rust#141610): `_` as a const argument and an
        // array length.
        (
            "inferred_const_arguments",
            "2021",
            "sized, copy",
            r#"
struct A<const N: usize>([u8; N]);
fn mk<const N: usize>() -> A<N> { A([0; N]) }
fn main() {
    let _a: [u8; _] = [0; 3];
    let _b: A<_> = A([0; 2]);
    let c = [1u8; _];
    let _: [u8; 4] = c;
    let d = mk::<_>();
    let _: A<5> = d;
}
"#,
        ),
        // 1.89 `repr128` (rust-lang/rust#138285).
        (
            "repr128",
            "2021",
            "sized",
            r#"
#[repr(u128)] enum E { A = 1 << 100, B }
#[repr(i128)] enum F { A = -1, B }
fn main() { let _ = E::B as u128; let _ = F::A as i128; }
"#,
        ),
        // 1.89 rust-lang/rust#140560: `#![doc(test(attr(..)))]` anywhere.
        (
            "doc_test_attr_anywhere",
            "2021",
            "sized",
            "#![doc(test(attr(deny(warnings))))]\n#[doc(test(attr(allow(unused))))]\npub mod m {}\nfn main() {}\n",
        ),
        // 1.89 rust-lang/rust#140593: a temporary's lifetime is extended through a tuple
        // struct's and a variant's constructor.
        (
            "temporaries_extended_through_constructors",
            "2021",
            "sized, option, copy",
            r#"
struct W<'a>(&'a u8);
fn mk() -> u8 { 1 }
fn main() { let w = W(&mk()); let s = Some(&mk()); let _ = (w.0, s); }
"#,
        ),
        // 1.89 the x86 target features `avx512*`, `kl`, `widekl`, `sha512`, `sm3`, `sm4`
        // (rust-lang/rust#138940, #140766, #140767) and 1.91's `sse4a` and `tbm` (#144542).
        (
            "x86_target_features",
            "2021 cfg:target_arch=x86_64",
            "sized",
            r#"
#[target_feature(enable = "avx512f,avx512bw,avx512vl,kl,widekl,sha512,sm3,sm4,sse4a,tbm")]
fn h() {}
fn main() { unsafe { h(); } }
"#,
        ),
    ]);
}

// Rust 1.88

#[test]
fn v1_88() {
    accepted(&[
        // `let_chains` in the 2024 edition (rust-lang/rust#132833).
        (
            "let_chains",
            "2024",
            "sized, option, copy, add, ord, builtin_impls",
            r#"
fn f(a: Option<u8>, b: Option<u8>) -> u8 {
    if let Some(x) = a && x > 1 && let Some(y) = b { x + y } else { 0 }
}
fn g(mut it: Option<u8>) { while let Some(x) = it && x > 0 { it = None; } }
fn main() {}
"#,
        ),
        // `naked_functions` (rust-lang/rust#134213).
        (
            "naked_functions",
            "2021 cfg:target_arch=aarch64",
            "sized, asm",
            r#"
#[unsafe(naked)]
pub extern "C" fn add(a: u64, b: u64) -> u64 { core::arch::naked_asm!("add x0, x0, x1", "ret") }
fn main() {}
"#,
        ),
        // `cfg_boolean_literals` (rust-lang/rust#138632).
        (
            "cfg_boolean_literals",
            "2021",
            "sized, cfg",
            r#"
#[cfg(true)] fn f() {}
#[cfg(false)] fn f() -> u8 { 1 }
#[cfg_attr(true, allow(dead_code))] fn g() {}
fn main() { f(); let _ = cfg!(true); let _ = cfg!(any(false, true)); }
"#,
        ),
    ]);
}

// Rust 1.86 and 1.87

#[test]
fn v1_86_and_1_87() {
    accepted(&[
        // 1.87 `asm_goto` (rust-lang/rust#133870).
        (
            "asm_goto",
            "2021 cfg:target_arch=aarch64",
            "sized, asm",
            "fn main() { unsafe { core::arch::asm!(\"b {}\", label { let _ = 1; }); } }\n",
        ),
        // 1.87 rust-lang/rust#134900: `..EXPR` after a unary operator parses.
        (
            "open_range_after_a_unary_operator",
            "2021",
            "sized",
            "macro_rules! m { ($e:expr) => {}; }\nm!(!..true); m!(-..5); m!(*..x);\nfn main() {}\n",
        ),
        // 1.87 rust-lang/rust#135480: an impl for an unsized type leaves out the methods
        // with `Self: Sized`.
        (
            "sized_methods_left_out_of_unsized_impls",
            "2021",
            "sized",
            r#"
trait Tr { fn a(&self); fn b(self) where Self: Sized; }
trait Other {}
impl Tr for [u8] { fn a(&self) {} }
impl Tr for dyn Other { fn a(&self) {} }
impl Tr for str { fn a(&self) {} }
fn main() {}
"#,
        ),
        // 1.87 `precise_capturing_in_traits` (rust-lang/rust#138128).
        (
            "use_bounds_in_trait_methods",
            "2021",
            "sized, iterator",
            r#"
trait Tr {
    fn it<'a, T>(&'a self, t: T) -> impl Iterator<Item = u8> + use<'a, Self, T>;
    fn only<'a>(&'a self) -> impl Sized + use<Self>;
}
fn main() {}
"#,
        ),
        // 1.86 `trait_upcasting` (rust-lang/rust#134367), also dropping auto traits and
        // through a raw pointer.
        (
            "trait_upcasting",
            "2021",
            "sized, coerce_unsized, dispatch_from_dyn, send, sync",
            r#"
trait A { fn a(&self) -> u8; } trait B: A + Send {}
fn up(x: &dyn B) -> &dyn A { x }
fn up_auto<'a>(x: &'a (dyn B + Sync)) -> &'a (dyn A + Send) { x }
fn up_raw(x: *const dyn B) -> *const dyn A { x }
fn main() {}
"#,
        ),
        // 1.86 `target_feature_11` (rust-lang/rust#134090): safe `#[target_feature]`
        // functions, called without `unsafe` from one with the features.
        (
            "safe_target_feature_functions",
            "2021 cfg:target_arch=x86_64",
            "sized",
            r#"
#[target_feature(enable = "sse2")] fn f() {}
#[target_feature(enable = "sse2")] fn g() { f(); }
fn main() { unsafe { g(); } }
"#,
        ),
    ]);
}

// Rust 1.85: the 2024 edition, async closures

#[test]
fn v1_85() {
    accepted(&[
        // `async_closure` (rust-lang/rust#132706).
        (
            "async_closures",
            "2021",
            "sized, async_fn, future, add, builtin_impls",
            r#"
async fn call<F: AsyncFn(u8) -> u8>(f: F) -> u8 { f(1).await }
async fn call_mut(mut f: impl AsyncFnMut() -> u8) -> u8 { f().await }
async fn call_once(f: impl AsyncFnOnce() -> u8) -> u8 { f().await }
fn main() {
    let s = 1u8;
    let c = async |x: u8| x + s;
    let _ = call(c);
    let _ = call_mut(async || 1);
    let _ = call_once(async move || s);
}
"#,
        ),
        // `#[diagnostic::do_not_recommend]` (rust-lang/rust#132056).
        (
            "do_not_recommend",
            "2021",
            "sized, copy",
            "trait Tr {}\n#[diagnostic::do_not_recommend]\nimpl<T> Tr for T where T: Copy {}\nfn main() {}\n",
        ),
        // The 2024 edition: `unsafe extern` with `safe` items, unsafe attributes,
        // `unsafe_op_in_unsafe_fn`, an RPIT that captures every parameter in scope and
        // `use<>` to capture none, `gen` as a raw identifier.
        (
            "edition_2024_items",
            "2024",
            "sized, option, copy",
            r#"
#![deny(unsafe_op_in_unsafe_fn)]
unsafe extern "C" { pub safe fn sf(x: i32) -> i32; pub unsafe fn uf(); pub static ST: i32; pub safe static SS: i32; }
#[unsafe(no_mangle)] pub extern "C" fn nm() {}
#[unsafe(export_name = "en")] pub fn en() {}
unsafe fn u() {}
unsafe fn caller() { unsafe { u() } }
fn captures<'a, T>(x: &'a T) -> impl Sized { x }
fn captures_none<'a>(x: &'a u8) -> impl Sized + use<> { let _ = x; 1u8 }
fn gen_kw() { let r#gen = 1; let _ = r#gen; }
fn main() {
    let _ = sf(1);
    let _ = SS;
    let v;
    { let y = 1u8; v = captures_none(&y); }
    let _ = v;
}
"#,
        ),
        // The 2024 edition's `expr` fragment takes `const { .. }` and `_`; `expr_2021` is
        // the old one (1.83, `expr_fragment_specifier_2024`).
        (
            "edition_2024_expr_fragment",
            "2024",
            "sized",
            r#"
macro_rules! m { ($e:expr) => { 1 }; ($($t:tt)*) => { 2 }; }
macro_rules! old { ($e:expr_2021) => { 1 }; ($($t:tt)*) => { 2 }; }
fn main() { let _ = m!(const { 1 }); let _ = m!(_); let _ = old!(const { 1 }); }
"#,
        ),
        // In the 2024 edition `&raw` of a `static mut` is how it is reached.
        (
            "edition_2024_static_mut",
            "2024",
            "sized",
            "static mut S: u8 = 0;\nfn main() { unsafe { let p = &raw mut S; *p = 1; } }\n",
        ),
    ]);
}

// Rust 1.83 and 1.84

#[test]
fn v1_83_and_1_84() {
    accepted(&[
        // 1.84 rust-lang/rust#121560: `#[deny]` inside `#[forbid]` of the same lint.
        (
            "deny_inside_forbid",
            "2021",
            "sized",
            "#![forbid(unsafe_code)]\n#[deny(unsafe_code)] fn f() {}\nfn main() {}\n",
        ),
        // 1.84 rust-lang/rust#131857: a coercion drops the principal of a trait object.
        (
            "dropping_the_principal",
            "2021",
            "sized, coerce_unsized, dispatch_from_dyn, send",
            "trait Tr {}\nfn f(x: &(dyn Tr + Send)) -> &dyn Send { x }\nfn main() {}\n",
        ),
        // 1.84 rust-lang/rust#129248: `&raw` of a dereferenced pointer needs no `unsafe`.
        (
            "raw_borrow_of_a_dereferenced_pointer",
            "2021",
            "sized",
            "fn f(p: *const u8) -> *const u8 { &raw const *p }\nfn g(p: *mut u8) -> *mut u8 { &raw mut *p }\nfn main() {}\n",
        ),
        // 1.83 `const_mut_refs`, `const_refs_to_cell`, `const_refs_to_static`
        // (rust-lang/rust#129195, #129759).
        (
            "mutable_references_and_statics_in_const",
            "2021",
            "sized, cell, copy, add, builtin_impls",
            r#"
use core::cell::Cell;
const fn inc(x: &mut i32) { *x += 1; }
const A: i32 = { let mut a = 1; inc(&mut a); let p = &raw mut a; unsafe { *p += 1 }; a };
const fn cell(c: &Cell<i32>) -> *const Cell<i32> { c }
static S: i32 = 5;
const R: &i32 = &S;
const V: i32 = S;
fn main() {}
"#,
        ),
        // 1.83 raw lifetimes and labels (rust-lang/rust#126452).
        (
            "raw_lifetimes",
            "2021",
            "sized",
            "fn f<'r#fn>(x: &'r#fn u8) -> &'r#fn u8 { x }\nfn main() { 'r#loop: loop { break 'r#loop; } }\n",
        ),
        // 1.83 rust-lang/rust#128934: an empty `#[non_exhaustive]` struct.
        (
            "empty_non_exhaustive_structs",
            "2021",
            "sized",
            "#[non_exhaustive] pub struct E {}\n#[non_exhaustive] pub struct U;\nfn main() {}\n",
        ),
        // 1.83 `const_extern_fn` (rust-lang/rust#129753).
        (
            "const_extern_fn",
            "2021 cfg:target_arch=x86_64",
            "sized",
            r#"
const extern "C" fn a() -> u8 { 1 }
const unsafe extern "system" fn b() -> u8 { 2 }
const extern "sysv64" fn c() -> u8 { 3 }
const X: u8 = a();
fn main() {}
"#,
        ),
        // 1.83 `expr_2021` in every edition (rust-lang/rust#129972).
        (
            "expr_2021_fragment",
            "2021",
            "sized, add, builtin_impls",
            "macro_rules! m { ($e:expr_2021) => { $e }; }\nfn main() { let _ = m!(1 + 1); }\n",
        ),
    ]);
}

/// 1.83 rust-lang/rust#129392: a place of type `!` that is not read does not coerce. rustc
/// refuses this (E0308), and so does blaze.
#[test]
fn v1_83_a_place_of_type_never_does_not_coerce() {
    let said = ui_core(
        "never_place.rs",
        "2021",
        "sized",
        "fn main() { unsafe { let x: *const ! = 0 as _; let _: u8 = *x; } }\n",
    );
    assert!(said.starts_with("error[E0308]: mismatched types\n"), "{said}");
}

// Rust 1.80 to 1.82

#[test]
fn v1_80_to_1_82() {
    accepted(&[
        // 1.82 `min_exhaustive_patterns` (rust-lang/rust#122792).
        (
            "empty_patterns_left_out",
            "2021",
            "sized, result, option",
            r#"
enum Void {}
fn f(x: Result<u8, Void>) -> u8 { let Ok(v) = x; v }
fn g(x: Result<u8, Void>) -> u8 { match x { Ok(v) => v } }
fn h(x: Option<Void>) { match x { None => {} } }
fn main() {}
"#,
        ),
        // 1.82 rust-lang/rust#125834: `&raw` of a `static mut` and of an extern static is safe.
        (
            "raw_borrow_of_statics",
            "2021",
            "sized",
            r#"
static mut M: i32 = 0;
unsafe extern "C" { static EX: i32; static mut EXM: i32; }
fn main() { let _a = &raw const M; let _b = &raw mut M; let _c = &raw const EX; let _d = &raw mut EXM; }
"#,
        ),
        // 1.82 `precise_capturing` (rust-lang/rust#127672).
        (
            "precise_capturing",
            "2021",
            "sized",
            "fn f<'a, 'b, T>(x: &'a u8, _y: &'b u8, _t: T) -> impl Sized + use<'a, T> { x }\nfn main() {}\n",
        ),
        // 1.82 `unsafe_extern_blocks` (rust-lang/rust#127921).
        (
            "unsafe_extern_blocks",
            "2021",
            "sized",
            r#"
unsafe extern "C" { pub safe fn sqrt(x: f64) -> f64; pub unsafe fn strlen(p: *const u8) -> usize; pub fn implicit(); pub safe static S: i32; }
fn main() { let _ = sqrt(1.0); let _ = S; unsafe { implicit(); } }
"#,
        ),
        // 1.82 `offset_of_nested` (rust-lang/rust#128284).
        (
            "nested_offset_of",
            "2021",
            "sized, offset_of",
            "struct A { b: B } struct B { c: (u8, u32) }\nconst O: usize = core::mem::offset_of!(A, b.c.1);\nfn main() {}\n",
        ),
        // 1.82 `asm_const` (rust-lang/rust#128570).
        (
            "asm_const_operands",
            "2021 cfg:target_arch=aarch64",
            "sized, asm",
            "const N: usize = 4;\nfn main() { unsafe { core::arch::asm!(\"mov x0, {}\", const N, out(\"x0\") _); } }\n",
        ),
        // 1.82 `const_fn_floating_point_arithmetic` (rust-lang/rust#128596).
        (
            "floats_in_const_fn",
            "2021",
            "sized, add, mul, ord, builtin_impls",
            r#"
const fn f(x: f64, y: f32) -> f64 { x * 2.0 + (y as f64) }
const X: f64 = f(1.5, 2.0);
const B: bool = 1.0f32 < 2.0f32;
fn main() {}
"#,
        ),
        // 1.82 `unsafe_attributes` (rust-lang/rust#128771).
        (
            "unsafe_attributes",
            "2021",
            "sized",
            r#"
#[unsafe(no_mangle)] pub fn a() {}
#[unsafe(export_name = "bb")] pub fn b() {}
#[cfg_attr(true, unsafe(no_mangle))] pub fn d() {}
fn main() {}
"#,
        ),
        // 1.82 rust-lang/rust#127054: `for<'a>` before the bound, `?Sized` beside it.
        (
            "binders_and_modifiers_of_bounds",
            "2021",
            "sized",
            r#"
trait Tr<'a> {}
fn f<T: for<'a> Tr<'a> + ?Sized>(_: &T) {}
fn g<T: ?Sized>(_: &T) where for<'a> T: Tr<'a> {}
fn main() {}
"#,
        ),
        // 1.82 rust-lang/rust#120221: a `let` statement is no pattern to a macro.
        (
            "statements_are_not_patterns_to_a_macro",
            "2021",
            "sized",
            "macro_rules! m { ($p:pat) => { 1 }; ($s:stmt) => { 2 }; }\nfn main() { let _ = m!(let x = 1); }\n",
        ),
        // 1.81 `lint_reasons` (rust-lang/rust#120924): `#[expect]` with a reason.
        (
            "expect_with_a_reason",
            "2021",
            "sized",
            "#![warn(unused)]\n#[expect(unused_variables, reason = \"testing\")]\nfn main() { let x = 1; }\n",
        ),
        // 1.81 `c_unwind` (rust-lang/rust#116088's ABIs).
        (
            "unwinding_abis",
            "2021",
            "sized",
            "extern \"C-unwind\" fn a() {}\nunsafe extern \"system-unwind\" { fn b(); }\nfn main() {}\n",
        ),
        // 1.80 `exclusive_range_pattern` (rust-lang/rust#124459).
        (
            "exclusive_range_patterns",
            "2021",
            "sized",
            "fn f(x: u8) -> u8 { match x { 0..10 => 0, 10..=20 => 1, 21.. => 2 } }\nfn main() {}\n",
        ),
        // 1.80 rust-lang/rust#124048: C23's variadics with no named parameter.
        (
            "variadics_with_no_named_parameter",
            "2021",
            "sized",
            "unsafe extern \"C\" { fn f(...); }\nfn main() {}\n",
        ),
    ]);
}
