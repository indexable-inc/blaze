//! rustc's UI tests of the definitions of a scope: names defined twice (E0428, E0252, E0255),
//! generic parameters (E0403, E0262, E0128), bindings (E0415, E0416), fields (E0124),
//! discriminants (E0081, E0084), recursive types (E0072) and unused parameters (E0392, E0091).

use crate::tests::{assert_reports, error, items::ui_core, ui};

#[test]
fn a_type_defined_twice_points_at_the_first() {
    // tests/ui/error-codes/E0428.rs
    let source = r#"//@ dont-require-annotations: NOTE

struct Bar; //~ NOTE previous definition of the type `Bar` here
struct Bar; //~ ERROR E0428

fn main () {
}
"#;
    let stderr = r#"error[E0428]: the name `Bar` is defined multiple times
  --> $DIR/E0428.rs:4:1
   |
LL | struct Bar;
   | ----------- previous definition of the type `Bar` here
LL | struct Bar;
   | ^^^^^^^^^^^ `Bar` redefined here
   |
   = note: `Bar` must be defined only once in the type namespace of this module

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0428`.
"#;
    assert_eq!(ui("E0428.rs", "2015", source), stderr);
}

#[test]
fn two_expansions_of_a_macro_define_one_name_without_conflict() {
    // std's `weak!` on macOS (`sys/pal/unix/weak/dlsym.rs`), called twice in one block by
    // `get_posix_spawn_addchdir`: each expansion's `static DLSYM` is keyed by the `macro`'s
    // context for that call, as rustc keys bindings, so there is no E0428 and each `&DLSYM`
    // resolves (no E0425). rustc builds it and it prints `1 2`.
    let source = r#"#![feature(decl_macro)]
macro weak($name:ident = $v:expr) {
    static DLSYM: u32 = $v;
    let $name = &DLSYM;
}
fn main() {
    weak!(a = 1);
    weak!(b = 2);
    let _ = (a, b);
}
"#;
    assert_eq!(ui("weak.rs", "2024", source), "");
}

#[test]
fn an_import_of_a_name_already_imported_suggests_renaming_it() {
    // tests/ui/error-codes/E0252.rs
    let source = r#"#![allow(non_camel_case_types)]

use foo::baz;
use bar::baz; //~ ERROR E0252

mod foo {
    pub struct baz;
}

mod bar {
    pub mod baz {}
}

fn main() {
}
"#;
    let stderr = r#"error[E0252]: the name `baz` is defined multiple times
  --> $DIR/E0252.rs:4:5
   |
LL | use foo::baz;
   |     -------- previous import of the type `baz` here
LL | use bar::baz;
   |     ^^^^^^^^ `baz` reimported here
   |
   = note: `baz` must be defined only once in the type namespace of this module
help: you can use `as` to change the binding name of the import
   |
LL | use bar::baz as other_baz;
   |              ++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0252`.
"#;
    assert_eq!(ui("E0252.rs", "2015", source), stderr);
}

#[test]
fn an_item_named_as_an_import_suggests_renaming_the_import() {
    // tests/ui/error-codes/E0255.rs
    let source = r#"use bar::foo;

fn foo() {} //~ ERROR E0255

mod bar {
     pub fn foo() {}
}

fn main() {}
"#;
    let stderr = r#"error[E0255]: the name `foo` is defined multiple times
  --> $DIR/E0255.rs:3:1
   |
LL | use bar::foo;
   |     -------- previous import of the value `foo` here
LL |
LL | fn foo() {}
   | ^^^^^^^^ `foo` redefined here
   |
   = note: `foo` must be defined only once in the value namespace of this module
help: you can use `as` to change the binding name of the import
   |
LL | use bar::foo as other_foo;
   |              ++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0255`.
"#;
    assert_eq!(ui("E0255.rs", "2015", source), stderr);
}

#[test]
fn a_type_parameter_declared_twice_points_at_the_first() {
    // tests/ui/error-codes/E0403.rs
    let source = "fn foo<T, T>(s: T, u: T) {} //~ ERROR E0403\n\nfn main() {\n}\n";
    let stderr = r#"error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/E0403.rs:1:11
   |
LL | fn foo<T, T>(s: T, u: T) {}
   |        -  ^ already used
   |        |
   |        first use of `T`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0403`.
"#;
    assert_eq!(ui("E0403.rs", "2015", source), stderr);
}

#[test]
fn generic_parameters_have_macros_2_0_hygiene() {
    // tests/ui/hygiene/generic_params.rs (check-pass): an opaque `macro`'s own `T`, `'a` and
    // `C` are parameters apart from the caller's `$T`, `$a` and `$C` (no E0403), and each
    // type, lifetime and const names the one its hygiene says, so the bodies type-check and
    // the lifetimes outlive as written; a semi-opaque or transparent one's are the caller's.
    let source = r#"// Ensure that generic parameters always have modern hygiene.

//@ check-pass

#![feature(decl_macro, rustc_attrs)]

mod type_params {
    macro m($T:ident) {
        fn f<$T: Clone, T: PartialEq>(t1: $T, t2: T) -> ($T, bool) {
            (t1.clone(), t2 == t2)
        }
    }

    #[rustc_macro_transparency = "semiopaque"]
    macro n($T:ident) {
        fn g<$T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
        fn h<T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
    }

    #[rustc_macro_transparency = "transparent"]
    macro p($T:ident) {
        fn j<$T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
        fn k<T: Clone>(t1: $T, t2: T) -> (T, $T) {
            (t1.clone(), t2.clone())
        }
    }

    m!(T);
    n!(T);
    p!(T);
}

mod lifetime_params {
    macro m($a:lifetime) {
        fn f<'b, 'c, $a: 'b, 'a: 'c>(t1: &$a(), t2: &'a ()) -> (&'b (), &'c ()) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "semiopaque"]
    macro n($a:lifetime) {
        fn g<$a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
        fn h<'a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "transparent"]
    macro p($a:lifetime) {
        fn j<$a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
        fn k<'a>(t1: &$a(), t2: &'a ()) -> (&'a (), &$a ()) {
            (t1, t2)
        }
    }

    m!('a);
    n!('a);
    p!('a);
}

mod const_params {
    macro m($C:ident) {
        fn f<const $C: usize, const C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); $C], [(); C]) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "semiopaque"]
    macro n($C:ident) {
        fn g<const $C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
        fn h<const C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
    }

    #[rustc_macro_transparency = "transparent"]
    macro p($C:ident) {
        fn j<const $C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
        fn k<const C: usize>(t1: [(); $C], t2: [(); C]) -> ([(); C], [(); $C]) {
            (t1, t2)
        }
    }

    m!(C);
    n!(C);
    p!(C);
}

fn main() {}
"#;
    assert_eq!(ui_core("generic_params.rs", "2015", "clone, eq", source), "");
}

#[test]
fn a_lifetime_named_twice_through_a_semi_opaque_or_transparent_macro_is_e0403() {
    // tests/ui/hygiene/duplicate_lifetimes.rs: the names are compared normalized to macros
    // 2.0, so a semi-opaque or transparent `macro`'s `'a` is the caller's `$a`.
    let source = r#"// Ensure that lifetime parameter names are modernized before we check for
// duplicates.

#![feature(decl_macro, rustc_attrs)]

#[rustc_macro_transparency = "semiopaque"]
macro m($a:lifetime) {
    fn g<$a, 'a>() {} //~ ERROR the name `'a` is already used for a generic parameter
}

#[rustc_macro_transparency = "transparent"]
macro n($a:lifetime) {
    fn h<$a, 'a>() {} //~ ERROR the name `'a` is already used for a generic parameter
}

m!('a);
n!('a);

fn main() {}
"#;
    let stderr = ui("duplicate_lifetimes.rs", "2015", source);
    assert_reports(&stderr, &[error(8, &["E0403", "`'a`"]), error(13, &["E0403", "`'a`"])]);
}

#[test]
fn a_type_or_const_parameter_named_twice_through_a_semi_opaque_or_transparent_macro_is_e0403() {
    // duplicate_lifetimes.rs's rule for type and const parameters, as rustc reports it: none
    // for the opaque `m`, whose own `T` and `C` are keyed apart from the caller's.
    let source = r#"#![feature(decl_macro, rustc_attrs)]

macro m($T:ident, $C:ident) {
    fn f<$T, T, const $C: usize, const C: usize>() {}
}

#[rustc_macro_transparency = "semiopaque"]
macro n($T:ident, $C:ident) {
    fn g<$T, T>() {}
    fn h<const $C: usize, const C: usize>() {}
}

#[rustc_macro_transparency = "transparent"]
macro p($T:ident, $C:ident) {
    fn j<$T, T>() {}
    fn k<const $C: usize, const C: usize>() {}
}

m!(T, C);
n!(T, C);
p!(T, C);

fn main() {}
"#;
    let stderr = ui("duplicate_params.rs", "2015", source);
    let expected = [
        error(9, &["E0403", "`T`"]),
        error(10, &["E0403", "`C`"]),
        error(15, &["E0403", "`T`"]),
        error(16, &["E0403", "`C`"]),
    ];
    assert_reports(&stderr, &expected);
}

#[test]
fn an_opaque_macros_own_parameter_is_another_than_the_callers() {
    // generic_params.rs's rule seen from the other side, as rustc reports it: in an opaque
    // `macro` the caller's `$T`, `$a` and `$C` and the macro's own `T`, `'a` and `C` are two
    // parameters each, so returning one as the other is E0308, or for lifetimes "lifetime may
    // not live long enough".
    let source = r#"#![feature(decl_macro)]

macro m($T:ident, $a:lifetime, $C:ident) {
    fn f<$T, T>(t: $T) -> T { t }
    fn g<$a, 'a>(t: &$a ()) -> &'a () { t }
    fn h<const $C: usize, const C: usize>(t: [(); $C]) -> [(); C] { t }
}

m!(T, 'a, C);

fn main() {}
"#;
    let stderr = ui("two_params.rs", "2015", source);
    let expected = [
        error(4, &["E0308", "type parameter"]),
        error(5, &["lifetime may not live long enough"]),
        error(6, &["E0308"]),
    ];
    assert_reports(&stderr, &expected);
}

#[test]
fn a_lifetime_declared_twice_is_e0403() {
    // tests/ui/error-codes/E0263.rs
    let source =
        "fn foo<'a, 'b, 'a>(x: &'a str, y: &'b str) {\n    //~^ ERROR E0403\n}\n\nfn main() {}\n";
    let stderr = r#"error[E0403]: the name `'a` is already used for a generic parameter in this item's generic parameters
  --> $DIR/E0263.rs:1:16
   |
LL | fn foo<'a, 'b, 'a>(x: &'a str, y: &'b str) {
   |        --      ^^ already used
   |        |
   |        first use of `'a`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0403`.
"#;
    assert_eq!(ui("E0263.rs", "2015", source), stderr);
}

#[test]
fn every_item_kind_reports_its_duplicated_type_parameter() {
    // tests/ui/duplicate/duplicate-type-parameter.rs
    let source = r#"type Foo<T,T> = Option<T>;
//~^ ERROR the name `T` is already used

struct Bar<T,T>(T);
//~^ ERROR the name `T` is already used

struct Baz<T,T> {
//~^ ERROR the name `T` is already used
    x: T,
}

enum Boo<T,T> {
//~^ ERROR the name `T` is already used
    A(T),
    B,
}

fn quux<T,T>(x: T) {}
//~^ ERROR the name `T` is already used

trait Qux<T,T> {}
//~^ ERROR the name `T` is already used

impl<T,T> Qux<T,T> for Option<T> {}
//~^ ERROR the name `T` is already used

fn main() {
}
"#;
    let stderr = r#"error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/duplicate-type-parameter.rs:1:12
   |
LL | type Foo<T,T> = Option<T>;
   |          - ^ already used
   |          |
   |          first use of `T`

error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/duplicate-type-parameter.rs:4:14
   |
LL | struct Bar<T,T>(T);
   |            - ^ already used
   |            |
   |            first use of `T`

error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/duplicate-type-parameter.rs:7:14
   |
LL | struct Baz<T,T> {
   |            - ^ already used
   |            |
   |            first use of `T`

error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/duplicate-type-parameter.rs:12:12
   |
LL | enum Boo<T,T> {
   |          - ^ already used
   |          |
   |          first use of `T`

error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/duplicate-type-parameter.rs:18:11
   |
LL | fn quux<T,T>(x: T) {}
   |         - ^ already used
   |         |
   |         first use of `T`

error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/duplicate-type-parameter.rs:21:13
   |
LL | trait Qux<T,T> {}
   |           - ^ already used
   |           |
   |           first use of `T`

error[E0403]: the name `T` is already used for a generic parameter in this item's generic parameters
  --> $DIR/duplicate-type-parameter.rs:24:8
   |
LL | impl<T,T> Qux<T,T> for Option<T> {}
   |      - ^ already used
   |      |
   |      first use of `T`

error: aborting due to 7 previous errors

For more information about this error, try `rustc --explain E0403`.
"#;
    assert_eq!(ui_core("duplicate-type-parameter.rs", "2015", "option", source), stderr);
}

#[test]
fn static_is_not_a_lifetime_parameter_name() {
    // tests/ui/error-codes/E0262.rs
    let source = "fn foo<'static>(x: &'static str) { } //~ ERROR E0262\n                                     //~| NOTE 'static is a reserved lifetime name\n\nfn main() {}\n";
    let stderr = r#"error[E0262]: invalid lifetime parameter name: `'static`
  --> $DIR/E0262.rs:1:8
   |
LL | fn foo<'static>(x: &'static str) { }
   |        ^^^^^^^ 'static is a reserved lifetime name

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0262`.
"#;
    assert_eq!(ui("E0262.rs", "2015", source), stderr);
}

#[test]
fn a_default_cannot_name_a_later_parameter() {
    // tests/ui/error-codes/E0128.rs
    let source = "struct Foo<T=U, U=()> { //~ ERROR E0128\n    field1: T,\n    field2: U,\n}\n\nfn main() {\n}\n";
    let stderr = r#"error[E0128]: generic parameter defaults cannot reference parameters before they are declared
  --> $DIR/E0128.rs:1:14
   |
LL | struct Foo<T=U, U=()> {
   |              ^ cannot reference `U` before it is declared

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0128`.
"#;
    assert_eq!(ui("E0128.rs", "2015", source), stderr);
}

#[test]
fn a_default_cannot_name_a_later_parameter_in_its_arguments() {
    // tests/ui/generics/generic-type-params-forward-mention.rs
    let source = r#"// Ensure that we get an error and not an ICE for this problematic case.
struct Foo<T = Option<U>, U = bool>(T, U);
//~^ ERROR generic parameter defaults cannot reference parameters before they are declared
fn main() {
    let x: Foo;
}
"#;
    let stderr = r#"error[E0128]: generic parameter defaults cannot reference parameters before they are declared
  --> $DIR/generic-type-params-forward-mention.rs:2:23
   |
LL | struct Foo<T = Option<U>, U = bool>(T, U);
   |                       ^ cannot reference `U` before it is declared

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0128`.
"#;
    assert_eq!(ui_core("generic-type-params-forward-mention.rs", "2015", "option", source), stderr);
}

#[test]
fn a_parameter_bound_twice_in_a_list() {
    // tests/ui/error-codes/E0415.rs
    let source = "fn foo(f: i32, f: i32) {} //~ ERROR E0415\n\nfn main() {\n}\n";
    let stderr = r#"error[E0415]: identifier `f` is bound more than once in this parameter list
  --> $DIR/E0415.rs:1:16
   |
LL | fn foo(f: i32, f: i32) {}
   |                ^ used as parameter more than once

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0415`.
"#;
    assert_eq!(ui("E0415.rs", "2015", source), stderr);
}

#[test]
fn a_name_bound_twice_in_a_pattern() {
    // tests/ui/error-codes/E0416.rs
    let source =
        "fn main() {\n    match (1, 2) {\n        (x, x) => {} //~ ERROR E0416\n    }\n}\n";
    let stderr = r#"error[E0416]: identifier `x` is bound more than once in the same pattern
  --> $DIR/E0416.rs:3:13
   |
LL |         (x, x) => {}
   |             ^ used in a pattern more than once

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0416`.
"#;
    assert_eq!(ui("E0416.rs", "2015", source), stderr);
}

#[test]
fn a_field_declared_twice_points_at_the_first() {
    // tests/ui/error-codes/E0124.rs
    let source = r#"struct Foo {
    field1: i32,
    field1: i32,
    //~^ ERROR field `field1` is already declared [E0124]
}

fn main() {
}
"#;
    let stderr = r#"error[E0124]: field `field1` is already declared
  --> $DIR/E0124.rs:3:5
   |
LL |     field1: i32,
   |     ----------- `field1` first declared here
LL |     field1: i32,
   |     ^^^^^^^^^^^ field already declared

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0124`.
"#;
    assert_eq!(ui("E0124.rs", "2015", source), stderr);
}

#[test]
fn fields_a_macro_writes_are_keyed_by_its_context() {
    // tests/ui/hygiene/fields-definition.rs: a struct a `macro` writes may have fields `a` and
    // `$a` (the caller's `a`), two names; a `macro_rules!` writes one name twice.
    let source = r#"#![feature(decl_macro)]

macro modern($a: ident) {
    struct Modern {
        a: u8,
        $a: u8, // OK
    }
}

macro_rules! legacy {
    ($a: ident) => {
        struct Legacy {
            a: u8,
            $a: u8, //~ ERROR field `a` is already declared
        }
    }
}

modern!(a);
legacy!(a);

fn main() {}
"#;
    // rustc's one error, `Legacy`'s. Its primary span is checked by count and macro alone:
    // rustc points at `$a: u8` in the `macro_rules!`, blaze at `legacy!(a)`, where the field's
    // name was written (a span mapping of a node made of two files' tokens, not hygiene).
    let stderr = ui("fields-definition.rs", "2015", source);
    assert_eq!(
        stderr.matches("error[E0124]: field `a` is already declared").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("`a` first declared here"), "{stderr}");
    assert!(stderr.contains("this error originates in the macro `legacy`"), "{stderr}");
}

#[test]
fn discriminants_assigned_twice_point_at_each_assignment() {
    // tests/ui/error-codes/E0081.rs
    let source = r#"enum Enum {
    //~^ ERROR discriminant value `3` assigned more than once
    P = 3,
    //~^ NOTE `3` assigned here
    X = 3,
    //~^ NOTE `3` assigned here
    Y = 5
}

#[repr(u8)]
enum EnumOverflowRepr {
    //~^ ERROR discriminant value `1` assigned more than once
    P = 257,
    //~^ NOTE `1` (overflowed from `257`) assigned here
    X = 513,
    //~^ NOTE `1` (overflowed from `513`) assigned here
}

#[repr(i8)]
enum NegDisEnum {
    //~^ ERROR discriminant value `-1` assigned more than once
    First = -1,
    //~^ NOTE `-1` assigned here
    Second = -2,
    //~^ NOTE discriminant for `Last` incremented from this startpoint (`Second` + 1 variant later => `Last` = -1)
    Last,
    //~^ NOTE `-1` assigned here
}

enum MultipleDuplicates {
    //~^ ERROR discriminant value `0` assigned more than once
    //~^^ ERROR discriminant value `-2` assigned more than once
    V0,
    //~^ NOTE `0` assigned here
    V1 = 0,
    //~^ NOTE `0` assigned here
    V2,
    V3,
    V4 = 0,
    //~^ NOTE `0` assigned here
    V5 = -2,
    //~^ NOTE discriminant for `V7` incremented from this startpoint (`V5` + 2 variants later => `V7` = 0)
    //~^^ NOTE `-2` assigned here
    V6,
    V7,
    //~^ NOTE `0` assigned here
    V8 = -3,
    //~^ NOTE discriminant for `V9` incremented from this startpoint (`V8` + 1 variant later => `V9` = -2)
    V9,
    //~^ NOTE `-2` assigned here
}

// Test for #131902
// Ensure that casting an enum with too many variants for its repr
// does not ICE
#[repr(u8)]
enum TooManyVariants {
    //~^ ERROR discriminant value `0` assigned more than once
    X000, X001, X002, X003, X004, X005, X006, X007, X008, X009,
    //~^ NOTE `0` assigned here
    //~| NOTE discriminant for `X256` incremented from this startpoint
    X010, X011, X012, X013, X014, X015, X016, X017, X018, X019,
    X020, X021, X022, X023, X024, X025, X026, X027, X028, X029,
    X030, X031, X032, X033, X034, X035, X036, X037, X038, X039,
    X040, X041, X042, X043, X044, X045, X046, X047, X048, X049,
    X050, X051, X052, X053, X054, X055, X056, X057, X058, X059,
    X060, X061, X062, X063, X064, X065, X066, X067, X068, X069,
    X070, X071, X072, X073, X074, X075, X076, X077, X078, X079,
    X080, X081, X082, X083, X084, X085, X086, X087, X088, X089,
    X090, X091, X092, X093, X094, X095, X096, X097, X098, X099,
    X100, X101, X102, X103, X104, X105, X106, X107, X108, X109,
    X110, X111, X112, X113, X114, X115, X116, X117, X118, X119,
    X120, X121, X122, X123, X124, X125, X126, X127, X128, X129,
    X130, X131, X132, X133, X134, X135, X136, X137, X138, X139,
    X140, X141, X142, X143, X144, X145, X146, X147, X148, X149,
    X150, X151, X152, X153, X154, X155, X156, X157, X158, X159,
    X160, X161, X162, X163, X164, X165, X166, X167, X168, X169,
    X170, X171, X172, X173, X174, X175, X176, X177, X178, X179,
    X180, X181, X182, X183, X184, X185, X186, X187, X188, X189,
    X190, X191, X192, X193, X194, X195, X196, X197, X198, X199,
    X200, X201, X202, X203, X204, X205, X206, X207, X208, X209,
    X210, X211, X212, X213, X214, X215, X216, X217, X218, X219,
    X220, X221, X222, X223, X224, X225, X226, X227, X228, X229,
    X230, X231, X232, X233, X234, X235, X236, X237, X238, X239,
    X240, X241, X242, X243, X244, X245, X246, X247, X248, X249,
    X250, X251, X252, X253, X254, X255,
    X256,
    //~^ ERROR enum discriminant overflowed
    //~| NOTE overflowed on value after 255
    //~| NOTE explicitly set `X256 = 0`
    //~| NOTE `0` assigned here
}

fn main() {
    TooManyVariants::X256 as u8;
}
"#;
    assert_reports(
        &ui_core("E0081.rs", "2015", "unary_ops, builtin_impls", source),
        &[
            error(1, &["`3`"]),
            error(11, &["`1`"]),
            error(20, &["`-1`"]),
            error(30, &["`0`"]),
            error(30, &["`-2`"]),
            error(87, &["`X256`"]),
            error(57, &["`0`"]),
        ],
    );
}

#[test]
fn a_zero_variant_enum_cannot_have_a_repr_of_an_integer() {
    // tests/ui/error-codes/E0084.rs
    let source = "#[repr(i32)] //~ ERROR: E0084\nenum Foo {}\n\nfn main() {\n}\n";
    let stderr = r#"error[E0084]: unsupported representation for zero-variant enum
  --> $DIR/E0084.rs:1:8
   |
LL | #[repr(i32)]
   |        ^^^
LL | enum Foo {}
   | -------- zero-variant enum

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0084`.
"#;
    assert_eq!(ui("E0084.rs", "2015", source), stderr);
}

#[test]
fn a_struct_holding_itself_through_a_generic_has_infinite_size() {
    // tests/ui/span/E0072.rs
    let source = "struct ListNode { //~ ERROR has infinite size\n    head: u8,\n    tail: Option<ListNode>,\n}\n\nfn main() {\n}\n";
    let stderr = r#"error[E0072]: recursive type `ListNode` has infinite size
  --> $DIR/E0072.rs:1:1
   |
LL | struct ListNode {
   | ^^^^^^^^^^^^^^^
LL |     head: u8,
LL |     tail: Option<ListNode>,
   |                  -------- recursive without indirection
   |
help: insert some indirection (e.g., a `Box`, `Rc`, or `&`) to break the cycle
   |
LL |     tail: Option<Box<ListNode>>,
   |                  ++++        +

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0072`.
"#;
    assert_eq!(ui_core("E0072.rs", "2015", "option", source), stderr);
}

#[test]
fn structs_holding_themselves_have_infinite_size() {
    // tests/ui/infinite/infinite-struct.rs
    let source = r#"struct Take(Take);
//~^ ERROR has infinite size

// check that we don't hang trying to find the tail of a recursive struct (#79437)
fn foo() -> Take {
    Take(loop {})
}

// mutually infinite structs
struct Foo { //~ ERROR has infinite size
    x: Bar<Foo>,
}

struct Bar<T>([T; 1]);

fn main() {}
"#;
    let stderr = r#"error[E0072]: recursive type `Take` has infinite size
  --> $DIR/infinite-struct.rs:1:1
   |
LL | struct Take(Take);
   | ^^^^^^^^^^^ ---- recursive without indirection
   |
help: insert some indirection (e.g., a `Box`, `Rc`, or `&`) to break the cycle
   |
LL | struct Take(Box<Take>);
   |             ++++    +

error[E0072]: recursive type `Foo` has infinite size
  --> $DIR/infinite-struct.rs:10:1
   |
LL | struct Foo {
   | ^^^^^^^^^^
LL |     x: Bar<Foo>,
   |            --- recursive without indirection
   |
help: insert some indirection (e.g., a `Box`, `Rc`, or `&`) to break the cycle
   |
LL |     x: Bar<Box<Foo>>,
   |            ++++   +

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0072`.
"#;
    assert_eq!(ui("infinite-struct.rs", "2015", source), stderr);
}

#[test]
fn an_unused_type_parameter_of_an_enum() {
    // tests/ui/error-codes/E0392.rs
    let source = "enum Foo<T> { Bar } //~ ERROR E0392\n\nfn main() {\n}\n";
    assert_reports(&ui("E0392.rs", "2015", source), &[error(1, &["`T`"])]);
}

/// addr2line's `MappedLookup` (src/lookup.rs, 0.26 and 0.27, which std uses): `T` is named
/// only by the `Fn` sugar's return type, the projection `<F as FnOnce<(L::Output,)>>::Output
/// == T`, which constrains it because the fields constrain `F` and `L`
/// (`identify_constrained_generic_params`).
#[test]
fn a_parameter_only_an_fn_bounds_return_names_is_constrained() {
    let source = "pub trait LookupContinuation { type Output; }
pub struct MappedLookup<T, L, F>
where
    L: LookupContinuation,
    F: FnOnce(L::Output) -> T,
{
    original: L,
    mutator: F,
}
";
    assert_eq!(ui_core("mapped-lookup.rs", "2021", "sized, fn", source), "");
}

/// The projection constrains `T` only once everything it reads is: with no field naming `L`,
/// neither `L` nor `T` is constrained, and both are E0392.
#[test]
fn an_fn_bounds_return_constrains_nothing_while_its_inputs_are_unused() {
    let source = "pub trait LookupContinuation { type Output; }
pub struct MappedLookup<T, L, F>
where
    L: LookupContinuation,
    F: FnOnce(L::Output) -> T,
{
    mutator: F,
}
";
    assert_reports(
        &ui_core("mapped-lookup.rs", "2021", "sized, fn", source),
        &[error(2, &["E0392", "`T`"]), error(2, &["E0392", "`L`"])],
    );
}

#[test]
fn a_bounded_unused_type_parameter_of_an_alias_is_not_a_const() {
    // tests/ui/type-alias/unused_type_parameter.rs
    let source = "//! regression test for <https://github.com/rust-lang/rust/issues/17994>\ntrait Tr {}\ntype Huh<T> where T: Tr = isize; //~ ERROR type parameter `T` is never used\nfn main() {}\n";
    let stderr = r#"error[E0091]: type parameter `T` is never used
  --> $DIR/unused_type_parameter.rs:3:10
   |
LL | type Huh<T> where T: Tr = isize;
   |          ^ unused type parameter
   |
   = help: consider removing `T` or referring to it in the body of the type alias

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0091`.
"#;
    assert_eq!(ui("unused_type_parameter.rs", "2015", source), stderr);
}

#[test]
fn an_unused_type_parameter_of_an_alias_points_at_its_name() {
    // tests/ui/typeck/unused-type-parameter-span-30236.rs
    let source = "// https://github.com/rust-lang/rust/issues/30236\ntype Foo<\n    Unused //~ ERROR type parameter `Unused` is never used\n    > = u8;\n\nfn main() {\n\n}\n";
    let stderr = r#"error[E0091]: type parameter `Unused` is never used
  --> $DIR/unused-type-parameter-span-30236.rs:3:5
   |
LL |     Unused
   |     ^^^^^^ unused type parameter
   |
   = help: consider removing `Unused` or referring to it in the body of the type alias
   = help: if you intended `Unused` to be a const parameter, use `const Unused: /* Type */` instead

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0091`.
"#;
    assert_eq!(ui("unused-type-parameter-span-30236.rs", "2015", source), stderr);
}

#[test]
fn explicit_discriminants_with_fields_need_a_repr() {
    // tests/ui/enum-discriminant/arbitrary_enum_discriminant-no-repr.rs
    let source = r#"#![crate_type = "lib"]

// Test that if any variant is non-unit,
// we need a repr.
enum Enum {
    //~^ ERROR `#[repr(inttype)]` must be specified
    Unit = 1,
    Tuple(),
    Struct {},
}

// Test that if any non-unit variant has an explicit
// discriminant we need a repr.
enum Enum2 {
    //~^ ERROR `#[repr(inttype)]` must be specified
    Tuple() = 2,
}
"#;
    let stderr = r#"error[E0732]: `#[repr(inttype)]` must be specified for enums with explicit discriminants and non-unit variants
  --> $DIR/arbitrary_enum_discriminant-no-repr.rs:5:1
   |
LL | enum Enum {
   | ^^^^^^^^^
LL |
LL |     Unit = 1,
   |            - explicit discriminant specified here
LL |     Tuple(),
   |     ----- non-unit discriminant declared here

error[E0732]: `#[repr(inttype)]` must be specified for enums with explicit discriminants and non-unit variants
  --> $DIR/arbitrary_enum_discriminant-no-repr.rs:14:1
   |
LL | enum Enum2 {
   | ^^^^^^^^^^
LL |
LL |     Tuple() = 2,
   |               - explicit discriminant on non-unit variant specified here

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0732`.
"#;
    assert_eq!(ui("arbitrary_enum_discriminant-no-repr.rs", "2015", source), stderr);
}

#[test]
fn a_bits_field_of_no_fixed_width_names_the_struct_the_field_and_the_type() {
    // blaze's own: `usize` is as wide as the target's pointers.
    let checked = crate::tests::checked(
        "//- /main.rs crate:other:main\n#[blaze::bits]\nstruct Wire { tag: bool, len: usize }\nfn main() {}\n",
    );
    let errors: Vec<String> = checked
        .diags
        .iter()
        .filter(|it| it.is_error() && !it.is_lint())
        .map(|it| it.message.to_string())
        .collect();
    assert_eq!(
        errors,
        ["field `len` of `#[blaze::bits]` struct `Wire`: type `usize` is as wide as the \
             target's pointers, and a bit-packed layout may not change with the target: use a \
             fixed-width integer"]
    );
}

#[test]
fn variants_a_macro_writes_are_keyed_by_its_context() {
    // blaze's own, the variants' version of tests/ui/hygiene/fields-definition.rs (checked
    // against the pinned rustc): an enum a `macro` writes may have variants `V` and `$v` (the
    // caller's `V`), two names, as tests/ui/hygiene/cross-crate-variants.rs's `MyEnum` has; a
    // `macro_rules!` writes one name twice.
    let source = r#"#![feature(decl_macro)]

macro modern($v: ident) {
    enum Modern {
        V = 0,
        $v = 1, // OK
    }
}

macro_rules! legacy {
    ($v: ident) => {
        enum Legacy {
            V = 0,
            $v = 1, //~ ERROR the name `V` is defined multiple times
        }
    }
}

modern!(V);
legacy!(V);

fn main() {}
"#;
    // rustc's one error, `Legacy`'s, checked by count and macro as the fields' version is.
    let stderr = ui("variants-definition.rs", "2015", source);
    assert_eq!(
        stderr.matches("error[E0428]: the name `V` is defined multiple times").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("this error originates in the macro `legacy`"), "{stderr}");
}
