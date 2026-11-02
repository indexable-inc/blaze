//! rustc's UI tests of the errors of AST validation (`tests/ui/error-codes/`,
//! `tests/ui/parser/`, `tests/ui/generics/`, `tests/ui/auto-traits/`, `tests/ui/c-variadic/`
//! and the rest named at each test), verbatim: each source and the whole `.stderr` rustc
//! writes for it, the errors of other passes in it included.

use crate::tests::{assert_reports, error, items::ui_core, ui};

#[test]
fn an_unsafe_inherent_impl_is_refused() {
    // tests/ui/error-codes/E0197.rs
    let source = r###"struct Foo;

unsafe impl Foo { } //~ ERROR E0197

fn main() {
}
"###;
    let stderr = r###"error[E0197]: inherent impls cannot be unsafe
  --> $DIR/E0197.rs:3:13
   |
LL | unsafe impl Foo { }
   | ------      ^^^ inherent impl for this type
   | |
   | unsafe because of this
   |
   = note: only trait implementations may be annotated with `unsafe`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0197`.
"###;
    assert_eq!(ui("E0197.rs", "2015", source), stderr);
}

#[test]
fn a_visibility_on_an_impl_or_a_trait_impl_item_is_refused() {
    // tests/ui/error-codes/E0449.rs
    let source = r###"//@ run-rustfix

#![allow(warnings)]

struct Bar;

trait Foo {
    fn foo();
}

pub impl Bar {} //~ ERROR E0449

pub impl Foo for Bar { //~ ERROR E0449
    pub fn foo() {} //~ ERROR E0449
}

fn main() {
}
"###;
    let stderr = r###"error[E0449]: visibility qualifiers are not permitted here
  --> $DIR/E0449.rs:11:1
   |
LL | pub impl Bar {}
   | ^^^ help: remove the qualifier
   |
   = note: place qualifiers on individual impl items instead

error[E0449]: visibility qualifiers are not permitted here
  --> $DIR/E0449.rs:13:1
   |
LL | pub impl Foo for Bar {
   | ^^^ help: remove the qualifier
   |
   = note: trait items always share the visibility of their trait

error[E0449]: visibility qualifiers are not permitted here
  --> $DIR/E0449.rs:14:5
   |
LL |     pub fn foo() {}
   |     ^^^ help: remove the qualifier
   |
   = note: trait items always share the visibility of their trait

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0449`.
"###;
    assert_eq!(ui("E0449.rs", "2015", source), stderr);
}

#[test]
fn a_for_bound_under_a_for_predicate_is_nested_quantification() {
    // tests/ui/where-clauses/where-for-self.rs
    let source = r###"// Test that we can quantify lifetimes outside a constraint (i.e., including
// the self type) in a where clause. Specifically, test that we cannot nest
// quantification in constraints (to be clear, there is no reason this should not
// we're testing we don't crash or do something stupid).

trait Bar<'a> {
    fn bar(&self);
}

impl<'a, 'b> Bar<'b> for &'a u32 {
    fn bar(&self) {}
}

fn foo<T>(x: &T)
    where for<'a> &'a T: for<'b> Bar<'b>
    //~^ error: nested quantification of lifetimes
{}

fn main() {}
"###;
    assert_reports(&ui("where-for-self.rs", "2015", source), &[error(15, &[])]);
}

#[test]
fn lifetimes_after_types_are_reordered() {
    // tests/ui/generics/lifetime-before-type-params.rs
    let source = r###"#![allow(unused)]
fn first<T, 'a, 'b>() {}
//~^ ERROR lifetime parameters must be declared prior to type and const parameters
fn second<'a, T, 'b>() {}
//~^ ERROR lifetime parameters must be declared prior to type and const parameters
fn third<T, U, 'a>() {}
//~^ ERROR lifetime parameters must be declared prior to type and const parameters
fn fourth<'a, T, 'b, U, 'c, V>() {}
//~^ ERROR lifetime parameters must be declared prior to type and const parameters

fn main() {}
"###;
    let stderr = r###"error: lifetime parameters must be declared prior to type and const parameters
  --> $DIR/lifetime-before-type-params.rs:2:13
   |
LL | fn first<T, 'a, 'b>() {}
   |         ----^^--^^- help: reorder the parameters: lifetimes, then consts and types: `<'a, 'b, T>`

error: lifetime parameters must be declared prior to type and const parameters
  --> $DIR/lifetime-before-type-params.rs:4:18
   |
LL | fn second<'a, T, 'b>() {}
   |          --------^^- help: reorder the parameters: lifetimes, then consts and types: `<'a, 'b, T>`

error: lifetime parameters must be declared prior to type and const parameters
  --> $DIR/lifetime-before-type-params.rs:6:16
   |
LL | fn third<T, U, 'a>() {}
   |         -------^^- help: reorder the parameters: lifetimes, then consts and types: `<'a, T, U>`

error: lifetime parameters must be declared prior to type and const parameters
  --> $DIR/lifetime-before-type-params.rs:8:18
   |
LL | fn fourth<'a, T, 'b, U, 'c, V>() {}
   |          --------^^-----^^---- help: reorder the parameters: lifetimes, then consts and types: `<'a, 'b, 'c, T, U, V>`

error: aborting due to 4 previous errors

"###;
    assert_eq!(ui("lifetime-before-type-params.rs", "2015", source), stderr);
}

#[test]
fn a_default_before_a_parameter_without_one_is_refused() {
    // tests/ui/generics/generic-non-trailing-defaults.rs
    let source = r###"struct Heap;

struct Vec<A = Heap, T>(A, T);
//~^ ERROR generic parameters with a default must be trailing

struct Foo<A, B = Vec<C>, C>(A, B, C);
//~^ ERROR generic parameters with a default must be trailing
//~| ERROR generic parameter defaults cannot reference parameters before they are declared

fn main() {}
"###;
    let stderr = r###"error: generic parameters with a default must be trailing
  --> $DIR/generic-non-trailing-defaults.rs:3:12
   |
LL | struct Vec<A = Heap, T>(A, T);
   |            ^

error: generic parameters with a default must be trailing
  --> $DIR/generic-non-trailing-defaults.rs:6:15
   |
LL | struct Foo<A, B = Vec<C>, C>(A, B, C);
   |               ^

error[E0128]: generic parameter defaults cannot reference parameters before they are declared
  --> $DIR/generic-non-trailing-defaults.rs:6:23
   |
LL | struct Foo<A, B = Vec<C>, C>(A, B, C);
   |                       ^ cannot reference `C` before it is declared

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0128`.
"###;
    assert_eq!(ui("generic-non-trailing-defaults.rs", "2015", source), stderr);
}

#[test]
fn which_functions_need_a_body() {
    // tests/ui/parser/fn-body-optional-semantic-fail.rs
    let source = r###"// Tests the different rules for `fn` forms requiring the presence or lack of a body.
// Also ensures that functions without a body don't show other odd errors.

trait Trait {}

fn main() {
    fn f1(); //~ ERROR free function without a body
    fn f1_rpit() -> impl Trait; //~ ERROR free function without a body
    fn f2() {} // OK.

    trait X {
        fn f1(); // OK.
        fn f2() {} // OK.
    }

    struct Y;
    impl X for Y {
        fn f1(); //~ ERROR associated function in `impl` without body
        fn f2() {} // OK.
    }

    impl Y {
        fn f3(); //~ ERROR associated function in `impl` without body
        fn f4() {} // OK.
    }

    extern "C" {
        fn f5(); // OK.
        fn f6() {} //~ ERROR incorrect function inside `extern` block
    }
}
"###;
    assert_reports(&ui("fn-body-optional-semantic-fail.rs", "2015", source), &[error(7, &[]), error(8, &[]), error(18, &["`impl`"]), error(23, &["`impl`"]), error(29, &["`extern`"])]);
}

#[test]
fn an_inherent_impl_function_needs_a_body() {
    // tests/ui/parser/impl-item-fn-no-body-semantic-fail.rs
    let source = r###"fn main() {}

struct X;

impl X {
    fn f(); //~ ERROR associated function in `impl` without body
}
"###;
    let stderr = r###"error: associated function in `impl` without body
  --> $DIR/impl-item-fn-no-body-semantic-fail.rs:6:5
   |
LL |     fn f();
   |     ^^^^^^-
   |           |
   |           help: provide a definition for the function: `{ <body> }`

error: aborting due to 1 previous error

"###;
    assert_eq!(ui("impl-item-fn-no-body-semantic-fail.rs", "2015", source), stderr);
}

#[test]
fn a_free_extern_function_without_a_body_suggests_an_extern_block() {
    // tests/ui/extern/not-in-block.rs
    let source = r###"#![crate_type = "lib"]
#![allow(missing_abi)]

extern fn none_fn(x: bool) -> i32;
//~^ ERROR free function without a body
extern "C" fn c_fn(x: bool) -> i32;
//~^ ERROR free function without a body
"###;
    let stderr = r###"error: free function without a body
  --> $DIR/not-in-block.rs:4:1
   |
LL | extern fn none_fn(x: bool) -> i32;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: provide a definition for the function
   |
LL - extern fn none_fn(x: bool) -> i32;
LL + extern fn none_fn(x: bool) -> i32 { <body> }
   |
help: if you meant to declare an externally defined function, use an `extern` block
   |
LL | extern { fn none_fn(x: bool) -> i32; }
   |        +                             +

error: free function without a body
  --> $DIR/not-in-block.rs:6:1
   |
LL | extern "C" fn c_fn(x: bool) -> i32;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
help: provide a definition for the function
   |
LL - extern "C" fn c_fn(x: bool) -> i32;
LL + extern "C" fn c_fn(x: bool) -> i32 { <body> }
   |
help: if you meant to declare an externally defined function, use an `extern` block
   |
LL | extern "C" { fn c_fn(x: bool) -> i32; }
   |            +                          +

error: aborting due to 2 previous errors

"###;
    assert_eq!(ui("not-in-block.rs", "2015", source), stderr);
}

#[test]
fn track_caller_needs_the_rust_abi() {
    // tests/ui/rfcs/rfc-2091-track-caller/error-with-invalid-abi.rs
    let source = r###"#[track_caller]
//~^ ERROR `#[track_caller]` can only be used with the Rust ABI
extern "C" fn c_fn() {}

#[track_caller]
extern "Rust" fn rust_fn() {}

extern "C" {
    #[track_caller]
    //~^ ERROR `#[track_caller]` can only be used with the Rust ABI
    fn c_extern();
}

extern "Rust" {
    #[track_caller]
    fn rust_extern();
}

struct S;

impl S {
    #[track_caller]
    //~^ ERROR `#[track_caller]` can only be used with the Rust ABI
    extern "C" fn c_method() {}

    #[track_caller]
    extern "Rust" fn rust_method() {}
}

#[rustfmt::skip] // rustfmt will insert the implicit "C"
#[track_caller]
//~^ ERROR `#[track_caller]` can only be used with the Rust ABI
pub extern fn extern_missing_abi() {}
//~^ WARN `extern` declarations without an explicit ABI are deprecated

fn main() {}
"###;
    let stderr = r###"error[E0737]: `#[track_caller]` can only be used with the Rust ABI
  --> $DIR/error-with-invalid-abi.rs:1:1
   |
LL | #[track_caller]
   | ^^^^^^^^^^^^^^^ using `#[track_caller]` here
LL |
LL | extern "C" fn c_fn() {}
   | ---------- not using the Rust ABI because of this

error[E0737]: `#[track_caller]` can only be used with the Rust ABI
  --> $DIR/error-with-invalid-abi.rs:9:5
   |
LL | extern "C" {
   | ---------- not using the Rust ABI because of this
LL |     #[track_caller]
   |     ^^^^^^^^^^^^^^^ using `#[track_caller]` here

error[E0737]: `#[track_caller]` can only be used with the Rust ABI
  --> $DIR/error-with-invalid-abi.rs:22:5
   |
LL |     #[track_caller]
   |     ^^^^^^^^^^^^^^^ using `#[track_caller]` here
LL |
LL |     extern "C" fn c_method() {}
   |     ---------- not using the Rust ABI because of this

error[E0737]: `#[track_caller]` can only be used with the Rust ABI
  --> $DIR/error-with-invalid-abi.rs:31:1
   |
LL | #[track_caller]
   | ^^^^^^^^^^^^^^^ using `#[track_caller]` here
LL |
LL | pub extern fn extern_missing_abi() {}
   |     ------ not using the Rust ABI because of this

warning: `extern` declarations without an explicit ABI are deprecated
  --> $DIR/error-with-invalid-abi.rs:33:5
   |
LL | pub extern fn extern_missing_abi() {}
   |     ^^^^^^ help: explicitly specify the "C" ABI: `extern "C"`
   |
   = note: `#[warn(missing_abi)]` on by default

error: aborting due to 4 previous errors; 1 warning emitted

For more information about this error, try `rustc --explain E0737`.
"###;
    assert_eq!(ui("error-with-invalid-abi.rs", "2015", source), stderr);
}

#[test]
fn no_mangle_needs_an_ascii_name() {
    // tests/ui/rfcs/rfc-2457-non-ascii-idents/no_mangle_nonascii_forbidden.rs
    let source = r###"#[no_mangle]
pub fn řųśť() {}  //~ ERROR `#[no_mangle]` requires ASCII identifier

pub struct Foo;

impl Foo {
    #[no_mangle]
    pub fn řųśť() {}  //~ ERROR `#[no_mangle]` requires ASCII identifier
}

trait Bar {
    fn řųśť();
}

impl Bar for Foo {
    #[no_mangle]
    fn řųśť() {}  //~ ERROR `#[no_mangle]` requires ASCII identifier
}

fn main() {}
"###;
    let stderr = r###"error[E0754]: `#[no_mangle]` requires ASCII identifier
  --> $DIR/no_mangle_nonascii_forbidden.rs:2:1
   |
LL | pub fn řųśť() {}
   | ^^^^^^^^^^^^^

error[E0754]: `#[no_mangle]` requires ASCII identifier
  --> $DIR/no_mangle_nonascii_forbidden.rs:8:5
   |
LL |     pub fn řųśť() {}
   |     ^^^^^^^^^^^^^

error[E0754]: `#[no_mangle]` requires ASCII identifier
  --> $DIR/no_mangle_nonascii_forbidden.rs:17:5
   |
LL |     fn řųśť() {}
   |     ^^^^^^^^^

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0754`.
"###;
    assert_eq!(ui("no_mangle_nonascii_forbidden.rs", "2015", source), stderr);
}

#[test]
fn an_auto_trait_has_no_generics_supertraits_or_items() {
    // tests/ui/auto-traits/auto-trait-validation.rs
    let source = r###"#![feature(auto_traits)]
#![allow(dead_code)]

//@ run-rustfix

auto trait Generic<T> {}
//~^ ERROR auto traits cannot have generic parameters [E0567]
auto trait Bound : Copy {}
//~^ ERROR auto traits cannot have super traits or lifetime bounds [E0568]
auto trait LifetimeBound : 'static {}
//~^ ERROR auto traits cannot have super traits or lifetime bounds [E0568]
auto trait MyTrait { fn foo() {} }
//~^ ERROR auto traits cannot have associated items [E0380]
auto trait AssocTy { type Bar; }
//~^ ERROR auto traits cannot have associated items [E0380]
auto trait All<'a, T> {
    //~^ ERROR auto traits cannot have generic parameters [E0567]
    type Bar;
    //~^ ERROR auto traits cannot have associated items [E0380]
    fn foo() {}
}
// We can't test both generic params and super-traits because the suggestion span overlaps.
auto trait All2: Copy + 'static {
    //~^ ERROR auto traits cannot have super traits or lifetime bounds [E0568]
    type Bar;
    //~^ ERROR auto traits cannot have associated items [E0380]
    fn foo() {}
}
fn main() {}
"###;
    let stderr = r###"error[E0567]: auto traits cannot have generic parameters
  --> $DIR/auto-trait-validation.rs:6:19
   |
LL | auto trait Generic<T> {}
   |            -------^^^
   |            |
   |            auto trait cannot have generic parameters

error[E0568]: auto traits cannot have super traits or lifetime bounds
  --> $DIR/auto-trait-validation.rs:8:20
   |
LL | auto trait Bound : Copy {}
   |            -----   ^^^^
   |            |
   |            auto traits cannot have super traits or lifetime bounds

error[E0568]: auto traits cannot have super traits or lifetime bounds
  --> $DIR/auto-trait-validation.rs:10:28
   |
LL | auto trait LifetimeBound : 'static {}
   |            -------------   ^^^^^^^
   |            |
   |            auto traits cannot have super traits or lifetime bounds

error[E0380]: auto traits cannot have associated items
  --> $DIR/auto-trait-validation.rs:12:25
   |
LL | auto trait MyTrait { fn foo() {} }
   |            -------      ^^^
   |            |
   |            auto traits cannot have associated items

error[E0380]: auto traits cannot have associated items
  --> $DIR/auto-trait-validation.rs:14:27
   |
LL | auto trait AssocTy { type Bar; }
   |            -------        ^^^
   |            |
   |            auto traits cannot have associated items

error[E0567]: auto traits cannot have generic parameters
  --> $DIR/auto-trait-validation.rs:16:15
   |
LL | auto trait All<'a, T> {
   |            ---^^^^^^^
   |            |
   |            auto trait cannot have generic parameters

error[E0380]: auto traits cannot have associated items
  --> $DIR/auto-trait-validation.rs:18:10
   |
LL | auto trait All<'a, T> {
   |            --- auto traits cannot have associated items
LL |
LL |     type Bar;
   |          ^^^
LL |
LL |     fn foo() {}
   |        ^^^

error[E0568]: auto traits cannot have super traits or lifetime bounds
  --> $DIR/auto-trait-validation.rs:23:18
   |
LL | auto trait All2: Copy + 'static {
   |            ----  ^^^^   ^^^^^^^
   |            |
   |            auto traits cannot have super traits or lifetime bounds

error[E0380]: auto traits cannot have associated items
  --> $DIR/auto-trait-validation.rs:25:10
   |
LL | auto trait All2: Copy + 'static {
   |            ---- auto traits cannot have associated items
LL |
LL |     type Bar;
   |          ^^^
LL |
LL |     fn foo() {}
   |        ^^^

error: aborting due to 9 previous errors

Some errors have detailed explanations: E0380, E0567, E0568.
For more information about an error, try `rustc --explain E0380`.
"###;
    assert_eq!(ui_core("auto-trait-validation.rs", "2015", "copy", source), stderr);
}

#[test]
fn a_trait_or_trait_impl_function_cannot_be_const() {
    // tests/ui/mismatched_types/const-fn-in-trait.rs
    let source = r###"trait Foo {
    fn f() -> u32;
    const fn g(); //~ ERROR cannot be declared const
}

impl Foo for u32 {
    const fn f() -> u32 { 22 } //~ ERROR cannot be declared const
    fn g() {}
}

fn main() { }
"###;
    let stderr = r###"error[E0379]: functions in traits cannot be declared const
  --> $DIR/const-fn-in-trait.rs:3:5
   |
LL |     const fn g();
   |     ^^^^^-
   |     |
   |     functions in traits cannot be const
   |     help: remove the `const`

error[E0379]: functions in trait impls cannot be declared const
  --> $DIR/const-fn-in-trait.rs:7:5
   |
LL |     const fn f() -> u32 { 22 }
   |     ^^^^^-
   |     |
   |     functions in trait impls cannot be const
   |     help: remove the `const`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0379`.
"###;
    assert_eq!(ui("const-fn-in-trait.rs", "2015", source), stderr);
}

#[test]
fn a_trait_object_has_one_lifetime_bound() {
    // tests/ui/regions/region-bounds-on-objects-and-type-parameters.rs
    let source = r###"// Test related to when a region bound is required to be specified.

trait IsStatic : 'static { }
trait IsSend : Send { }
trait Is<'a> : 'a { }
trait Is2<'a> : 'a { }
trait SomeTrait { }

// Bounds on object types:

struct Foo<'a,'b,'c> { //~ ERROR parameter `'c` is never used
    // All of these are ok, because we can derive exactly one bound:
    a: Box<dyn IsStatic>,
    b: Box<dyn Is<'static>>,
    c: Box<dyn Is<'a>>,
    d: Box<dyn IsSend>,
    e: Box<dyn Is<'a>+Send>, // we can derive two bounds, but one is 'static, so ok
    f: Box<dyn SomeTrait>,   // OK, defaults to 'static due to RFC 599.
    g: Box<dyn SomeTrait+'a>,

    z: Box<dyn Is<'a>+'b+'c>,
    //~^ ERROR only a single explicit lifetime bound is permitted
    //~| ERROR lifetime bound not satisfied
}

fn test<
    'a,
    'b,
    A:IsStatic,
    B:Is<'a>+Is2<'b>, // OK in a parameter, but not an object type.
    C:'b+Is<'a>+Is2<'b>,
    D:Is<'a>+Is2<'static>,
    E:'a+'b           // OK in a parameter, but not an object type.
>() { }

fn main() { }
"###;
    assert_reports(
        &ui("region-bounds-on-objects-and-type-parameters.rs", "2015", source),
        &[
            error(21, &[]),
            error(11, &["`'c`"]),
            error(21, &[]),
        ],
    );
}

#[test]
fn patterns_in_foreign_functions_and_function_pointers() {
    // tests/ui/pattern/no-patterns-in-args.rs
    let source = r###"extern "C" {
    fn f1(mut arg: u8); //~ ERROR patterns aren't allowed in foreign function declarations
    fn f2(&arg: u8); //~ ERROR patterns aren't allowed in foreign function declarations
    fn f3(arg @ _: u8); //~ ERROR patterns aren't allowed in foreign function declarations
    fn g1(arg: u8); // OK
    fn g2(_: u8); // OK
// fn g3(u8); // Not yet
}

type A1 = fn(mut arg: u8); //~ ERROR patterns aren't allowed in function pointer types
type A2 = fn(&arg: u8); //~ ERROR patterns aren't allowed in function pointer types
type B1 = fn(arg: u8); // OK
type B2 = fn(_: u8); // OK
type B3 = fn(u8); // OK

fn main() {}
"###;
    let stderr = r###"error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/no-patterns-in-args.rs:2:11
   |
LL |     fn f1(mut arg: u8);
   |           ^^^^^^^ pattern not allowed in foreign function

error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/no-patterns-in-args.rs:3:11
   |
LL |     fn f2(&arg: u8);
   |           ^^^^ pattern not allowed in foreign function

error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/no-patterns-in-args.rs:4:11
   |
LL |     fn f3(arg @ _: u8);
   |           ^^^^^^^ pattern not allowed in foreign function

error[E0561]: patterns aren't allowed in function pointer types
  --> $DIR/no-patterns-in-args.rs:10:14
   |
LL | type A1 = fn(mut arg: u8);
   |              ^^^^^^^

error[E0561]: patterns aren't allowed in function pointer types
  --> $DIR/no-patterns-in-args.rs:11:14
   |
LL | type A2 = fn(&arg: u8);
   |              ^^^^

error: aborting due to 5 previous errors

Some errors have detailed explanations: E0130, E0561.
For more information about an error, try `rustc --explain E0130`.
"###;
    assert_eq!(ui("no-patterns-in-args.rs", "2015", source), stderr);
}

#[test]
fn self_is_only_a_parameter_of_associated_functions() {
    // tests/ui/parser/self-param-semantic-fail.rs
    let source = r###"// This test ensures that `self` is semantically rejected
// in contexts with `FnDecl` but outside of associated `fn`s.
// FIXME(Centril): For now closures are an exception.

fn main() {}

fn free() {
    fn f1(self) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f2(mut self) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f3(&self) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f4(&mut self) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f5<'a>(&'a self) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f6<'a>(&'a mut self) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f7(self: u8) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f8(mut self: u8) {}
    //~^ ERROR `self` parameter is only allowed in associated functions
}

extern "C" {
    fn f1(self);
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f2(mut self);
    //~^ ERROR `self` parameter is only allowed in associated functions
    //~| ERROR patterns aren't allowed in
    fn f3(&self);
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f4(&mut self);
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f5<'a>(&'a self);
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f6<'a>(&'a mut self);
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f7(self: u8);
    //~^ ERROR `self` parameter is only allowed in associated functions
    fn f8(mut self: u8);
//~^ ERROR `self` parameter is only allowed in associated functions
//~| ERROR patterns aren't allowed in
}

type X1 = fn(self);
//~^ ERROR `self` parameter is only allowed in associated functions
type X2 = fn(mut self);
//~^ ERROR `self` parameter is only allowed in associated functions
//~| ERROR patterns aren't allowed in
type X3 = fn(&self);
//~^ ERROR `self` parameter is only allowed in associated functions
type X4 = fn(&mut self);
//~^ ERROR `self` parameter is only allowed in associated functions
type X5 = for<'a> fn(&'a self);
//~^ ERROR `self` parameter is only allowed in associated functions
type X6 = for<'a> fn(&'a mut self);
//~^ ERROR `self` parameter is only allowed in associated functions
type X7 = fn(self: u8);
//~^ ERROR `self` parameter is only allowed in associated functions
type X8 = fn(mut self: u8);
//~^ ERROR `self` parameter is only allowed in associated functions
//~| ERROR patterns aren't allowed in
"###;
    let stderr = r###"error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:8:11
   |
LL |     fn f1(self) {}
   |           ^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:10:11
   |
LL |     fn f2(mut self) {}
   |           ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:12:11
   |
LL |     fn f3(&self) {}
   |           ^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:14:11
   |
LL |     fn f4(&mut self) {}
   |           ^^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:16:15
   |
LL |     fn f5<'a>(&'a self) {}
   |               ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:18:15
   |
LL |     fn f6<'a>(&'a mut self) {}
   |               ^^^^^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:20:11
   |
LL |     fn f7(self: u8) {}
   |           ^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:22:11
   |
LL |     fn f8(mut self: u8) {}
   |           ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:27:11
   |
LL |     fn f1(self);
   |           ^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:29:11
   |
LL |     fn f2(mut self);
   |           ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/self-param-semantic-fail.rs:29:11
   |
LL |     fn f2(mut self);
   |           ^^^^^^^^ pattern not allowed in foreign function

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:32:11
   |
LL |     fn f3(&self);
   |           ^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:34:11
   |
LL |     fn f4(&mut self);
   |           ^^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:36:15
   |
LL |     fn f5<'a>(&'a self);
   |               ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:38:15
   |
LL |     fn f6<'a>(&'a mut self);
   |               ^^^^^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:40:11
   |
LL |     fn f7(self: u8);
   |           ^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:42:11
   |
LL |     fn f8(mut self: u8);
   |           ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/self-param-semantic-fail.rs:42:11
   |
LL |     fn f8(mut self: u8);
   |           ^^^^^^^^ pattern not allowed in foreign function

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:47:14
   |
LL | type X1 = fn(self);
   |              ^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:49:14
   |
LL | type X2 = fn(mut self);
   |              ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error[E0561]: patterns aren't allowed in function pointer types
  --> $DIR/self-param-semantic-fail.rs:49:14
   |
LL | type X2 = fn(mut self);
   |              ^^^^^^^^

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:52:14
   |
LL | type X3 = fn(&self);
   |              ^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:54:14
   |
LL | type X4 = fn(&mut self);
   |              ^^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:56:22
   |
LL | type X5 = for<'a> fn(&'a self);
   |                      ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:58:22
   |
LL | type X6 = for<'a> fn(&'a mut self);
   |                      ^^^^^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:60:14
   |
LL | type X7 = fn(self: u8);
   |              ^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error: `self` parameter is only allowed in associated functions
  --> $DIR/self-param-semantic-fail.rs:62:14
   |
LL | type X8 = fn(mut self: u8);
   |              ^^^^^^^^ not semantically valid as function parameter
   |
   = note: associated functions are those in `impl` or `trait` definitions

error[E0561]: patterns aren't allowed in function pointer types
  --> $DIR/self-param-semantic-fail.rs:62:14
   |
LL | type X8 = fn(mut self: u8);
   |              ^^^^^^^^

error: aborting due to 28 previous errors

Some errors have detailed explanations: E0130, E0561.
For more information about an error, try `rustc --explain E0130`.
"###;
    assert_eq!(ui("self-param-semantic-fail.rs", "2015", source), stderr);
}

#[test]
fn impl_trait_in_impl_trait_behind_a_constraint() {
    // tests/ui/impl-trait/issues/issue-57979-nested-impl-trait-in-assoc-proj.rs
    let source = r###"// rust-lang/rust#57979 : the initial support for `impl Trait` didn't
// properly check syntax hidden behind an associated type projection.
// Here we test behavior of occurrences of `impl Trait` within an
// `impl Trait` in that context.

pub trait Foo<T> { }
pub trait Bar { }
pub trait Quux { type Assoc; }
pub fn demo(_: impl Quux<Assoc=impl Foo<impl Bar>>) { }
//~^ ERROR nested `impl Trait` is not allowed

fn main() { }
"###;
    let stderr = r###"error[E0666]: nested `impl Trait` is not allowed
  --> $DIR/issue-57979-nested-impl-trait-in-assoc-proj.rs:9:41
   |
LL | pub fn demo(_: impl Quux<Assoc=impl Foo<impl Bar>>) { }
   |                                ---------^^^^^^^^-
   |                                |        |
   |                                |        nested `impl Trait` here
   |                                outer `impl Trait`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0666`.
"###;
    assert_eq!(ui("issue-57979-nested-impl-trait-in-assoc-proj.rs", "2015", source), stderr);
}

#[test]
fn a_const_bound_on_a_function_that_is_not_const() {
    // tests/ui/traits/const-traits/const-bound-on-not-const-associated-fn.rs
    let source = r###"//@ compile-flags: -Znext-solver

#![feature(const_trait_impl)]

const trait MyTrait {
    fn do_something(&self);
}

trait OtherTrait {
    fn do_something_else() where Self: [const] MyTrait;
    //~^ ERROR `[const]` is not allowed here
}

struct MyStruct<T>(T);

const impl MyTrait for u32 {
    fn do_something(&self) {}
}

impl<T> MyStruct<T> {
    pub fn foo(&self) where T: [const] MyTrait {
        //~^ ERROR `[const]` is not allowed here
        self.0.do_something();
    }
}

fn main() {
    MyStruct(0u32).foo();
}
"###;
    let stderr = r###"error: `[const]` is not allowed here
  --> $DIR/const-bound-on-not-const-associated-fn.rs:10:40
   |
LL |     fn do_something_else() where Self: [const] MyTrait;
   |                                        ^^^^^^^
   |
note: this function is not `const`, so it cannot have `[const]` trait bounds
  --> $DIR/const-bound-on-not-const-associated-fn.rs:10:8
   |
LL |     fn do_something_else() where Self: [const] MyTrait;
   |        ^^^^^^^^^^^^^^^^^

error: `[const]` is not allowed here
  --> $DIR/const-bound-on-not-const-associated-fn.rs:21:32
   |
LL |     pub fn foo(&self) where T: [const] MyTrait {
   |                                ^^^^^^^
   |
note: this function is not `const`, so it cannot have `[const]` trait bounds
  --> $DIR/const-bound-on-not-const-associated-fn.rs:21:12
   |
LL |     pub fn foo(&self) where T: [const] MyTrait {
   |            ^^^

error: aborting due to 2 previous errors

"###;
    assert_eq!(ui("const-bound-on-not-const-associated-fn.rs", "2015", source), stderr);
}

#[test]
fn a_generic_argument_after_a_constraint_is_moved() {
    // tests/ui/parser/issues/issue-32214.rs
    let source = r###"trait Trait<T> { type Item; }

pub fn test<W, I: Trait<Item=(), W> >() {}
//~^ ERROR generic arguments must come before the first constraint

fn main() { }
"###;
    let stderr = r###"error: generic arguments must come before the first constraint
  --> $DIR/issue-32214.rs:3:34
   |
LL | pub fn test<W, I: Trait<Item=(), W> >() {}
   |                         -------  ^ generic argument
   |                         |
   |                         constraint
   |
help: move the constraint after the generic argument
   |
LL - pub fn test<W, I: Trait<Item=(), W> >() {}
LL + pub fn test<W, I: Trait<W, Item = ()> >() {}
   |

error: aborting due to 1 previous error

"###;
    assert_eq!(ui("issue-32214.rs", "2015", source), stderr);
}

#[test]
fn a_union_needs_a_field() {
    // tests/ui/union/union-empty.rs
    let source = r###"union U {} //~ ERROR unions cannot have zero fields

fn main() {}
"###;
    let stderr = r###"error: unions cannot have zero fields
  --> $DIR/union-empty.rs:1:1
   |
LL | union U {}
   | ^^^^^^^^^^

error: aborting due to 1 previous error

"###;
    assert_eq!(ui("union-empty.rs", "2015", source), stderr);
}

#[test]
fn a_function_cannot_be_const_and_async() {
    // tests/ui/async-await/const-async-fn-in-main.rs
    let source = r###"//@ edition:2021
// Check what happens when a const async fn is in the main function (#102796)

fn main() {
    const async fn a() {}
//~^ ERROR functions cannot be both `const` and `async`
}
"###;
    let stderr = r###"error: functions cannot be both `const` and `async`
  --> $DIR/const-async-fn-in-main.rs:5:5
   |
LL |     const async fn a() {}
   |     ^^^^^-^^^^^----------
   |     |     |
   |     |     `async` because of this
   |     `const` because of this

error: aborting due to 1 previous error

"###;
    assert_eq!(ui_core("const-async-fn-in-main.rs", "2021", "future", source), stderr);
}

#[test]
fn c_variadic_dots_must_be_last_and_extern() {
    // tests/ui/c-variadic/issue-86053-1.rs
    let source = r###"// Regression test for the ICE described in issue #86053.

#![crate_type="lib"]

fn ordering4 < 'a , 'b     > ( a :            ,   self , self ,   self ,
    //~^ ERROR expected type, found `,`
    //~| ERROR unexpected `self` parameter in function
    //~| ERROR unexpected `self` parameter in function
    //~| ERROR unexpected `self` parameter in function
    self , _: ... ,   self ,   self , _: ... ) where F : FnOnce ( & 'a & 'b usize ) {
    //~^ ERROR unexpected `self` parameter in function
    //~| ERROR unexpected `self` parameter in function
    //~| ERROR unexpected `self` parameter in function
    //~| ERROR `...` must be the last argument of a C-variadic function
    //~| ERROR `...` is not supported for non-extern functions
    //~| ERROR cannot find type `F` in this scope
}
"###;
    assert_reports(
        &ui("issue-86053-1.rs", "2015", source),
        &[
            error(5, &["`,`"]),
            error(5, &["`self`"]),
            error(5, &["`self`"]),
            error(5, &["`self`"]),
            error(10, &["`self`"]),
            error(10, &["`self`"]),
            error(10, &["`self`"]),
            error(10, &["`...`"]),
            error(10, &["`...`"]),
            error(10, &["`F`"]),
        ],
    );
}

#[test]
fn a_pattern_in_a_foreign_function() {
    // tests/ui/error-codes/E0130.rs
    let source = r#"extern "C" {
    fn foo((a, b): (u32, u32));
//~^ ERROR E0130
}

fn main() {}
"#;
    let stderr = r#"error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/E0130.rs:2:12
   |
LL |     fn foo((a, b): (u32, u32));
   |            ^^^^^^ pattern not allowed in foreign function

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0130`.
"#;
    assert_eq!(ui("E0130.rs", "2015", source), stderr);
}

#[test]
fn literal_unit_struct_and_tuple_patterns_in_foreign_functions() {
    // tests/ui/extern/foreign-fn-pattern-error-10877.rs
    let source = r#"//! Regression test for https://github.com/rust-lang/rust/issues/10877

struct Foo {
    x: isize,
}
extern "C" {
    fn foo(1: ());
    //~^ ERROR: patterns aren't allowed in foreign function declarations
    fn bar((): isize);
    //~^ ERROR: patterns aren't allowed in foreign function declarations
    fn baz(Foo { x }: isize);
    //~^ ERROR: patterns aren't allowed in foreign function declarations
    fn qux((x, y): ());
    //~^ ERROR: patterns aren't allowed in foreign function declarations
    fn this_is_actually_ok(a: usize);
    fn and_so_is_this(_: usize);
}
fn main() {}
"#;
    let stderr = r#"error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/foreign-fn-pattern-error-10877.rs:7:12
   |
LL |     fn foo(1: ());
   |            ^ pattern not allowed in foreign function

error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/foreign-fn-pattern-error-10877.rs:9:12
   |
LL |     fn bar((): isize);
   |            ^^ pattern not allowed in foreign function

error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/foreign-fn-pattern-error-10877.rs:11:12
   |
LL |     fn baz(Foo { x }: isize);
   |            ^^^^^^^^^ pattern not allowed in foreign function

error[E0130]: patterns aren't allowed in foreign function declarations
  --> $DIR/foreign-fn-pattern-error-10877.rs:13:12
   |
LL |     fn qux((x, y): ());
   |            ^^^^^^ pattern not allowed in foreign function

error: aborting due to 4 previous errors

For more information about this error, try `rustc --explain E0130`.
"#;
    assert_eq!(ui("foreign-fn-pattern-error-10877.rs", "2015", source), stderr);
}

#[test]
fn free_statics_without_bodies() {
    // tests/ui/parser/item-free-static-no-body-semantic-fail.rs
    let source = r#"// Semantically, a free `static` item cannot omit its body.

fn main() {}

static A: u8; //~ ERROR free static item without body
static B; //~ ERROR free static item without body
//~^ ERROR missing type for `static` item

static mut C: u8; //~ ERROR free static item without body
static mut D; //~ ERROR free static item without body
//~^ ERROR missing type for `static mut` item
"#;
    assert_reports(&ui("item-free-static-no-body-semantic-fail.rs", "2015", source), &[error(5, &[]), error(6, &[]), error(9, &[]), error(10, &[]), error(6, &["`static`"]), error(10, &["`static mut`"])]);
}

#[test]
fn free_consts_without_bodies() {
    // tests/ui/parser/item-free-const-no-body-semantic-fail.rs
    let source = r#"// Semantically, a free `const` item cannot omit its body.

fn main() {}

const A: u8; //~ ERROR free constant item without body
const B; //~ ERROR free constant item without body
//~^ ERROR missing type for `const` item
"#;
    assert_reports(&ui("item-free-const-no-body-semantic-fail.rs", "2015", source), &[error(5, &[]), error(6, &[]), error(6, &["`const`"])]);
}

#[test]
fn an_inherent_associated_const_without_a_body() {
    // tests/ui/parser/impl-item-const-semantic-fail.rs
    let source = r#"fn main() {}

struct X;

impl X {
    const Y: u8; //~ ERROR associated constant in `impl` without body
}
"#;
    let stderr = r#"error: associated constant in `impl` without body
  --> $DIR/impl-item-const-semantic-fail.rs:6:5
   |
LL |     const Y: u8;
   |     ^^^^^^^^^^^-
   |                |
   |                help: provide a definition for the constant: `= <expr>;`

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("impl-item-const-semantic-fail.rs", "2015", source), stderr);
}

#[test]
fn inherent_associated_types_without_bodies_before_their_bounds() {
    // tests/ui/parser/impl-item-type-no-body-semantic-fail.rs
    let source = r#"fn main() {}

struct X;

impl X {
    type Y;
    //~^ ERROR associated type in `impl` without body
    //~| ERROR inherent associated types are unstable
    type Z: Ord;
    //~^ ERROR associated type in `impl` without body
    //~| ERROR bounds on `type`s in `impl`s have no effect
    //~| ERROR inherent associated types are unstable
    type W: Ord where Self: Eq;
    //~^ ERROR associated type in `impl` without body
    //~| ERROR bounds on `type`s in `impl`s have no effect
    //~| ERROR inherent associated types are unstable
    //~| ERROR `X: Eq` is not satisfied
    type W where Self: Eq;
    //~^ ERROR associated type in `impl` without body
    //~| ERROR inherent associated types are unstable
    //~| ERROR duplicate definitions
    //~| ERROR `X: Eq` is not satisfied
}
"#;
    assert_reports(
        &ui_core("impl-item-type-no-body-semantic-fail.rs", "2015", "ord, derive", source),
        &[
            error(6, &["`impl`"]),
            error(9, &["`impl`"]),
            error(9, &["`impl`", "`type`"]),
            error(13, &["`impl`"]),
            error(13, &["`impl`", "`type`"]),
            error(18, &["`impl`"]),
            error(6, &[]),
            error(9, &[]),
            error(13, &[]),
            error(18, &[]),
            error(13, &["`X: Eq`"]),
            error(18, &["`X: Eq`"]),
            error(18, &["`W`"]),
        ],
    );
}

#[test]
fn an_unsafe_negative_impl() {
    // tests/ui/error-codes/E0198.rs
    let source = r#"#![feature(negative_impls)]

struct Foo;

unsafe impl !Send for Foo { } //~ ERROR E0198

fn main() {
}
"#;
    let stderr = r#"error[E0198]: negative impls cannot be unsafe
  --> $DIR/E0198.rs:5:13
   |
LL | unsafe impl !Send for Foo { }
   | ------      -^^^^
   | |           |
   | |           negative because of this
   | unsafe because of this

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0198`.
"#;
    assert_eq!(ui_core("E0198.rs", "2015", "send", source), stderr);
}
