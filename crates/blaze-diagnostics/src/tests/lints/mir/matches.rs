//! rustc's UI tests of match checking (`check_match`): a `match` that misses values of its
//! scrutinee (E0004) and a `let` whose pattern is refutable (E0005). The tests are of local
//! enums where they can be: a note "`Option<i32>` defined here" points into core, which in a
//! fixture is minicore's file and not rustc's `$SRC_DIR/core/src/option.rs`.

use crate::tests::{items::ui_core, ui};

#[test]
fn a_missing_variant_is_suggested_after_the_only_arm() {
    // tests/ui/error-codes/E0004.rs
    let source = r#"enum Terminator {
    HastaLaVistaBaby,
    TalkToMyHand,
}

fn main() {
    let x = Terminator::HastaLaVistaBaby;

    match x { //~ ERROR E0004
        Terminator::TalkToMyHand => {}
    }
}
"#;
    let stderr = r#"error[E0004]: non-exhaustive patterns: `Terminator::HastaLaVistaBaby` not covered
  --> $DIR/E0004.rs:9:11
   |
LL |     match x {
   |           ^ pattern `Terminator::HastaLaVistaBaby` not covered
   |
note: `Terminator` defined here
  --> $DIR/E0004.rs:1:6
   |
LL | enum Terminator {
   |      ^^^^^^^^^^
LL |     HastaLaVistaBaby,
   |     ---------------- not covered
   = note: the matched value is of type `Terminator`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern or an explicit pattern as shown
   |
LL ~         Terminator::TalkToMyHand => {},
LL +         Terminator::HastaLaVistaBaby => todo!()
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0004`.
"#;
    assert_eq!(ui("E0004.rs", "2015", source), stderr);
}

#[test]
fn an_empty_match_on_unit_suggests_a_wildcard_in_its_braces() {
    // tests/ui/pattern/usefulness/issue-3096-1.rs
    let source = r#"fn main() {
    match () { } //~ ERROR non-exhaustive
}
"#;
    let stderr = r#"error[E0004]: non-exhaustive patterns: type `()` is non-empty
  --> $DIR/issue-3096-1.rs:2:11
   |
LL |     match () { }
   |           ^^
   |
   = note: the matched value is of type `()`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern as shown
   |
LL ~     match () {
LL +         _ => todo!(),
LL ~     }
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0004`.
"#;
    assert_eq!(ui("issue-3096-1.rs", "2015", source), stderr);
}

#[test]
fn an_empty_match_on_a_raw_pointer_names_no_definition() {
    // tests/ui/pattern/usefulness/issue-3096-2.rs
    let source = r#"enum Bottom { }

fn main() {
    let x = &() as *const () as *const Bottom;
    match x { } //~ ERROR non-exhaustive patterns
}
"#;
    let stderr = r#"error[E0004]: non-exhaustive patterns: type `*const Bottom` is non-empty
  --> $DIR/issue-3096-2.rs:5:11
   |
LL |     match x { }
   |           ^
   |
   = note: the matched value is of type `*const Bottom`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern as shown
   |
LL ~     match x {
LL +         _ => todo!(),
LL ~     }
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0004`.
"#;
    assert_eq!(ui("issue-3096-2.rs", "2015", source), stderr);
}

#[test]
fn a_struct_variant_witness_names_the_fields_it_needs() {
    // tests/ui/pattern/usefulness/struct-like-enum-nonexhaustive.rs
    let source = r#"enum A {
    B { x: Option<isize> },
    C
}

fn main() {
    let x = A::B { x: Some(3) };
    match x {   //~ ERROR non-exhaustive patterns
        A::C => {}
        A::B { x: None } => {}
    }
}
"#;
    let stderr = r#"error[E0004]: non-exhaustive patterns: `A::B { x: Some(_) }` not covered
  --> $DIR/struct-like-enum-nonexhaustive.rs:8:11
   |
LL |     match x {
   |           ^ pattern `A::B { x: Some(_) }` not covered
   |
note: `A` defined here
  --> $DIR/struct-like-enum-nonexhaustive.rs:1:6
   |
LL | enum A {
   |      ^
LL |     B { x: Option<isize> },
   |     - not covered
   = note: the matched value is of type `A`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern or an explicit pattern as shown
   |
LL ~         A::B { x: None } => {},
LL +         A::B { x: Some(_) } => todo!()
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0004`.
"#;
    assert_eq!(ui_core("struct-like-enum-nonexhaustive.rs", "2015", "option", source), stderr);
}

#[test]
fn a_refutable_let_of_an_option_suggests_let_else() {
    // tests/ui/error-codes/E0005.rs
    let source = r#"fn main() {
    let x = Some(1);
    let Some(y) = x; //~ ERROR E0005
}
"#;
    let stderr = r#"error[E0005]: refutable pattern in local binding
  --> $DIR/E0005.rs:3:9
   |
LL |     let Some(y) = x;
   |         ^^^^^^^ pattern `None` not covered
   |
   = note: `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with only one variant
   = note: for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
   = note: the matched value is of type `Option<i32>`
help: you might want to use `let...else` to handle the variant that isn't matched
   |
LL |     let Some(y) = x else { todo!() };
   |                     ++++++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0005`.
"#;
    assert_eq!(ui_core("E0005.rs", "2015", "option", source), stderr);
}

#[test]
fn a_refutable_let_of_a_local_enum_points_at_the_variants_it_misses() {
    // tests/ui/pattern/usefulness/issue-31561.rs
    let source = r#"//@ dont-require-annotations: NOTE

enum Thing {
    Foo(u8),
    Bar,
    Baz
}

fn main() {
    let Thing::Foo(y) = Thing::Foo(1);
    //~^ ERROR refutable pattern in local binding
    //~| NOTE `Thing::Bar` and `Thing::Baz` not covered
}
"#;
    let stderr = r#"error[E0005]: refutable pattern in local binding
  --> $DIR/issue-31561.rs:10:9
   |
LL |     let Thing::Foo(y) = Thing::Foo(1);
   |         ^^^^^^^^^^^^^ patterns `Thing::Bar` and `Thing::Baz` not covered
   |
   = note: `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with only one variant
   = note: for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
note: `Thing` defined here
  --> $DIR/issue-31561.rs:3:6
   |
LL | enum Thing {
   |      ^^^^^
LL |     Foo(u8),
LL |     Bar,
   |     --- not covered
LL |     Baz
   |     --- not covered
   = note: the matched value is of type `Thing`
help: you might want to use `let...else` to handle the variants that aren't matched
   |
LL |     let Thing::Foo(y) = Thing::Foo(1) else { todo!() };
   |                                       ++++++++++++++++

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0005`.
"#;
    assert_eq!(ui("issue-31561.rs", "2015", source), stderr);
}

#[test]
fn matches_and_lets_through_references_name_their_witnesses_with_the_references() {
    // tests/ui/pattern/usefulness/non-exhaustive-defined-here.rs
    let source = r##"#![feature(custom_inner_attributes)]
#![rustfmt::skip]
// Test the "defined here" and "not covered" diagnostic hints.
// We also make sure that references are peeled off from the scrutinee type
// so that the diagnostics work better with default binding modes.

#[derive(Clone)]
enum E {
    //~^ NOTE `E` defined here
    //~| NOTE `E` defined here
    //~| NOTE `E` defined here
    //~| NOTE
    //~| NOTE
    //~| NOTE
    //~| NOTE
    //~| NOTE
    //~| NOTE
    A,
    B,
    //~^ NOTE  not covered
    //~| NOTE  not covered
    //~| NOTE  not covered
    //~| NOTE  not covered
    //~| NOTE  not covered
    //~| NOTE  not covered
    C
    //~^ NOTE not covered
    //~| NOTE not covered
    //~| NOTE not covered
    //~| NOTE not covered
    //~| NOTE not covered
    //~| NOTE not covered
}

fn by_val(e: E) {
    let e1 = e.clone();
    match e1 { //~ ERROR non-exhaustive patterns: `E::B` and `E::C` not covered
        //~^ NOTE patterns `E::B` and `E::C` not covered
        //~| NOTE the matched value is of type `E`
        E::A => {}
    }

    let E::A = e;
    //~^ ERROR refutable pattern in local binding
    //~| NOTE patterns `E::B` and `E::C` not covered
    //~| NOTE `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with
    //~| NOTE for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
    //~| NOTE the matched value is of type `E`
}

fn by_ref_once(e: &E) {
    match e {
    //~^ ERROR non-exhaustive patterns
    //~| NOTE patterns `&E::B` and `&E::C` not covered
    //~| NOTE the matched value is of type `&E`
        E::A => {}
    }

    let E::A = e;
    //~^ ERROR refutable pattern in local binding
    //~| NOTE patterns `&E::B` and `&E::C` not covered
    //~| NOTE `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with
    //~| NOTE for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
    //~| NOTE the matched value is of type `&E`
}

fn by_ref_thrice(e: & &mut &E) {
    match e {
    //~^ ERROR non-exhaustive patterns
    //~| NOTE patterns `&&mut &E::B` and `&&mut &E::C` not covered
    //~| NOTE the matched value is of type `&&mut &E`
        E::A => {}
    }

    let E::A = e;
    //~^ ERROR refutable pattern in local binding
    //~| NOTE patterns `&&mut &E::B` and `&&mut &E::C` not covered
    //~| NOTE `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with
    //~| NOTE for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
    //~| NOTE the matched value is of type `&&mut &E`
}

enum Opt {
    //~^ NOTE `Opt` defined here
    //~| NOTE
    //~| NOTE
    Some(u8),
    None,
    //~^ NOTE not covered
    //~| NOTE not covered
}

fn ref_pat(e: Opt) {
    match e {
        //~^ ERROR non-exhaustive patterns
        //~| NOTE pattern `Opt::None` not covered
        //~| NOTE the matched value is of type `Opt`
        Opt::Some(ref _x) => {}
    }

    let Opt::Some(ref _x) = e;
    //~^ ERROR refutable pattern in local binding
    //~| NOTE the matched value is of type `Opt`
    //~| NOTE pattern `Opt::None` not covered
    //~| NOTE `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with
    //~| NOTE for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
}

fn main() {}
"##;
    let stderr = r#"error[E0004]: non-exhaustive patterns: `E::B` and `E::C` not covered
  --> $DIR/non-exhaustive-defined-here.rs:37:11
   |
LL |     match e1 {
   |           ^^ patterns `E::B` and `E::C` not covered
   |
note: `E` defined here
  --> $DIR/non-exhaustive-defined-here.rs:8:6
   |
LL | enum E {
   |      ^
...
LL |     B,
   |     - not covered
...
LL |     C
   |     - not covered
   = note: the matched value is of type `E`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern, a match arm with multiple or-patterns as shown, or multiple match arms
   |
LL ~         E::A => {},
LL +         E::B | E::C => todo!()
   |

error[E0005]: refutable pattern in local binding
  --> $DIR/non-exhaustive-defined-here.rs:43:9
   |
LL |     let E::A = e;
   |         ^^^^ patterns `E::B` and `E::C` not covered
   |
   = note: `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with only one variant
   = note: for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
note: `E` defined here
  --> $DIR/non-exhaustive-defined-here.rs:8:6
   |
LL | enum E {
   |      ^
...
LL |     B,
   |     - not covered
...
LL |     C
   |     - not covered
   = note: the matched value is of type `E`
help: you might want to use `if let` to ignore the variants that aren't matched
   |
LL |     if let E::A = e { todo!() };
   |     ++              +++++++++++

error[E0004]: non-exhaustive patterns: `&E::B` and `&E::C` not covered
  --> $DIR/non-exhaustive-defined-here.rs:52:11
   |
LL |     match e {
   |           ^ patterns `&E::B` and `&E::C` not covered
   |
note: `E` defined here
  --> $DIR/non-exhaustive-defined-here.rs:8:6
   |
LL | enum E {
   |      ^
...
LL |     B,
   |     - not covered
...
LL |     C
   |     - not covered
   = note: the matched value is of type `&E`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern, a match arm with multiple or-patterns as shown, or multiple match arms
   |
LL ~         E::A => {},
LL +         &E::B | &E::C => todo!()
   |

error[E0005]: refutable pattern in local binding
  --> $DIR/non-exhaustive-defined-here.rs:59:9
   |
LL |     let E::A = e;
   |         ^^^^ patterns `&E::B` and `&E::C` not covered
   |
   = note: `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with only one variant
   = note: for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
note: `E` defined here
  --> $DIR/non-exhaustive-defined-here.rs:8:6
   |
LL | enum E {
   |      ^
...
LL |     B,
   |     - not covered
...
LL |     C
   |     - not covered
   = note: the matched value is of type `&E`
help: you might want to use `if let` to ignore the variants that aren't matched
   |
LL |     if let E::A = e { todo!() };
   |     ++              +++++++++++

error[E0004]: non-exhaustive patterns: `&&mut &E::B` and `&&mut &E::C` not covered
  --> $DIR/non-exhaustive-defined-here.rs:68:11
   |
LL |     match e {
   |           ^ patterns `&&mut &E::B` and `&&mut &E::C` not covered
   |
note: `E` defined here
  --> $DIR/non-exhaustive-defined-here.rs:8:6
   |
LL | enum E {
   |      ^
...
LL |     B,
   |     - not covered
...
LL |     C
   |     - not covered
   = note: the matched value is of type `&&mut &E`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern, a match arm with multiple or-patterns as shown, or multiple match arms
   |
LL ~         E::A => {},
LL +         &&mut &E::B | &&mut &E::C => todo!()
   |

error[E0005]: refutable pattern in local binding
  --> $DIR/non-exhaustive-defined-here.rs:75:9
   |
LL |     let E::A = e;
   |         ^^^^ patterns `&&mut &E::B` and `&&mut &E::C` not covered
   |
   = note: `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with only one variant
   = note: for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
note: `E` defined here
  --> $DIR/non-exhaustive-defined-here.rs:8:6
   |
LL | enum E {
   |      ^
...
LL |     B,
   |     - not covered
...
LL |     C
   |     - not covered
   = note: the matched value is of type `&&mut &E`
help: you might want to use `if let` to ignore the variants that aren't matched
   |
LL |     if let E::A = e { todo!() };
   |     ++              +++++++++++

error[E0004]: non-exhaustive patterns: `Opt::None` not covered
  --> $DIR/non-exhaustive-defined-here.rs:94:11
   |
LL |     match e {
   |           ^ pattern `Opt::None` not covered
   |
note: `Opt` defined here
  --> $DIR/non-exhaustive-defined-here.rs:83:6
   |
LL | enum Opt {
   |      ^^^
...
LL |     None,
   |     ---- not covered
   = note: the matched value is of type `Opt`
help: ensure that all possible cases are being handled by adding a match arm with a wildcard pattern or an explicit pattern as shown
   |
LL ~         Opt::Some(ref _x) => {},
LL +         Opt::None => todo!()
   |

error[E0005]: refutable pattern in local binding
  --> $DIR/non-exhaustive-defined-here.rs:101:9
   |
LL |     let Opt::Some(ref _x) = e;
   |         ^^^^^^^^^^^^^^^^^ pattern `Opt::None` not covered
   |
   = note: `let` bindings require an "irrefutable pattern", like a `struct` or an `enum` with only one variant
   = note: for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html
note: `Opt` defined here
  --> $DIR/non-exhaustive-defined-here.rs:83:6
   |
LL | enum Opt {
   |      ^^^
...
LL |     None,
   |     ---- not covered
   = note: the matched value is of type `Opt`
help: you might want to use `let...else` to handle the variant that isn't matched
   |
LL |     let Opt::Some(ref _x) = e else { todo!() };
   |                               ++++++++++++++++

error: aborting due to 8 previous errors

Some errors have detailed explanations: E0004, E0005.
For more information about an error, try `rustc --explain E0004`.
"#;
    assert_eq!(ui_core("non-exhaustive-defined-here.rs", "2015", "clone, derive", source), stderr);
}
