//! Not UI tests of rustc's: what hir once recovered from without a word (an error type, a
//! fresh variable, `pub`, `extern "C"`, no prelude) is each rustc's error, by its code and
//! message.

use crate::tests::{assert_reports, checked, error, items::ui_core};

/// The crate `source`, of edition `edition` over minicore's `sized`, gives an error whose
/// `code: message` starts with `wanted`.
#[track_caller]
fn refused(edition: &str, source: &str, wanted: &str) {
    let messages = checked(&format!("//- minicore: sized\n//- /main.rs crate:other:main edition:{edition}\n{source}"))
        .messages();
    assert!(messages.iter().any(|it| it.starts_with(wanted)), "{wanted:?} in {messages:?}");
}

#[test]
fn an_abi_rustc_does_not_know_is_e0703() {
    refused("2021", "extern \"Cc\" fn f() {}\nfn main() {}\n", "E0703: invalid ABI: found `Cc`");
    refused("2021", "type F = extern \"nope\" fn();\nfn main() {}\n", "E0703: invalid ABI: found `nope`");
    refused("2021", "extern \"nope\" {}\nfn main() {}\n", "E0703: invalid ABI: found `nope`");
}

#[test]
fn a_visibility_that_names_no_ancestor_module() {
    let not_ancestor = "mod a { pub mod b {} }\nmod c { pub(in crate::a) fn f() {} }\nfn main() {}\n";
    refused("2021", not_ancestor, "E0742: visibilities can only be restricted to ancestor modules");
    let not_module = "struct S;\nmod m { pub(in crate::S) fn f() {} }\nfn main() {}\n";
    refused("2021", not_module, "E0577: expected module, found struct `crate::S`");
    let nothing = "mod m { pub(in crate::nothing) fn f() {} }\nfn main() {}\n";
    refused("2021", nothing, "E0433: cannot find `nothing` in `crate`");
    let relative = "mod m { pub(in m) fn f() {} }\nfn main() {}\n";
    refused("2018", relative, "relative paths are not supported in visibilities in 2018 edition or later");
}

#[test]
fn a_crate_without_its_prelude_crate_is_e0463() {
    let messages = checked("//- /main.rs crate:other:main edition:2021\n#![no_std]\nfn main() {}\n").messages();
    assert!(messages.iter().any(|it| it == "E0463: can't find crate for `core`"), "{messages:?}");
}

#[test]
fn a_local_as_a_const_argument_is_e0435() {
    let source = "fn f<const N: usize>() {}\nfn main() { let x = 3; f::<x>(); }\n";
    refused("2021", source, "E0435: attempt to use a non-constant value in a constant");
}

#[test]
fn type_lowering_errors_are_rustcs() {
    refused(
        "2021",
        "trait T {}\nfn f<X: T>(_: X::Missing) {}\nfn main() {}\n",
        "E0220: associated type `Missing` not found for `X`",
    );
    refused("2021", "struct S;\nfn f(_: S::Assoc) {}\nfn main() {}\n", "E0223: ambiguous associated type");
    refused(
        "2021",
        "trait A {}\ntrait B {}\nfn f(_: &(dyn A + B)) {}\nfn main() {}\n",
        "E0225: only auto traits can be used as additional traits in a trait object",
    );
    refused(
        "2021",
        "fn f(_: &dyn ?Sized) {}\nfn main() {}\n",
        "E0224: at least one trait is required for an object type",
    );
    refused(
        "2021",
        "struct S { f: impl Sized }\nfn main() {}\n",
        "E0562: `impl Trait` is not allowed in field types",
    );
    refused(
        "2021",
        "trait T { type A; }\nfn f(_: &dyn T<A = u8, A = u16>) {}\nfn main() {}\n",
        "E0719: the value of the associated type `A` in trait `T` is already specified",
    );
    refused(
        "2021",
        "trait T<Rhs = Self> {}\nfn f(_: &dyn T) {}\nfn main() {}\n",
        "E0393: the type parameter `Rhs` must be explicitly specified",
    );
}

#[test]
fn a_struct_path_to_something_else() {
    let to_a_function = ui_core("main.rs", "2021", "sized", "fn g() {}\nfn main() { let _ = g {}; }\n");
    assert_reports(&to_a_function, &[error(2, &["`g`", "struct"])]);
    let to_no_variant = ui_core("main.rs", "2021", "sized", "enum E { A }\nfn main() { let _ = E::C {}; }\n");
    assert_reports(&to_no_variant, &[error(2, &["`C`", "`E`"])]);
}

#[test]
fn a_type_relative_path_in_a_pattern() {
    let s = "struct S;\nimpl S { fn new() -> S { S } }\n";
    refused(
        "2021",
        &format!("{s}fn main() {{ match S {{ S::new(..) => {{}} }} }}\n"),
        "E0164: expected tuple struct or tuple variant, found associated function `S::new`",
    );
    refused(
        "2021",
        &format!("{s}fn main() {{ match S {{ S::new => {{}} }} }}\n"),
        "E0533: expected unit struct, unit variant or constant, found associated function `S::new`",
    );
}

#[test]
fn self_of_a_struct_with_fields_is_no_constructor() {
    let source = "struct S { a: u8 }\nimpl S { fn f() { let _ = Self; } }\nfn main() {}\n";
    refused("2021", source, "the `Self` constructor can only be used with tuple or unit structs");
}

#[test]
fn generic_arguments_on_a_variant_through_an_alias_are_e0109() {
    let source = "enum E<T> { V(T) }\ntype A = E<u8>;\nfn main() { let _ = A::V::<u8>(0); }\n";
    refused("2021", source, "E0109: type arguments are not allowed on this type");
}

#[test]
fn a_method_on_a_raw_pointer_to_an_unknown_pointee() {
    let source = "fn main() { let p = 0usize as *const _; let _ = p.is_null(); }\n";
    refused("2018", source, "E0282: type annotations needed");
    refused("2015", source, "tyvar_behind_raw_pointer: type annotations needed");
}
