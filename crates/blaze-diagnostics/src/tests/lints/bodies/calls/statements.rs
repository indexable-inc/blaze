//! `path_statements`, `let_underscore_drop`, `let_underscore_lock`, `unused_results`,
//! `unit_bindings`, `non_shorthand_field_patterns` and `unused_allocation`, against rustc's UI
//! tests.

use super::{Ui, ui};
use crate::tests::{Mode, assert_reports, error};

#[test]
fn a_path_statement_has_no_effect_or_drops_its_value() {
    let source = r##"//@ compile-flags: -D path-statements
struct Droppy;

impl Drop for Droppy {
    fn drop(&mut self) {}
}

fn main() {
    let x = 10;
    x; //~ ERROR path statement with no effect

    let y = Droppy;
    y; //~ ERROR path statement drops value

    let z = (Droppy,);
    z; //~ ERROR path statement drops value
}
"##;
    let expected = r##"error: path statement with no effect
  --> $DIR/warn-path-statement.rs:10:5
   |
LL |     x;
   |     ^^
   |
   = note: requested on the command line with `-D path-statements`

error: path statement drops value
  --> $DIR/warn-path-statement.rs:13:5
   |
LL |     y;
   |     ^^ help: use `drop` to clarify the intent: `drop(y);`

error: path statement drops value
  --> $DIR/warn-path-statement.rs:16:5
   |
LL |     z;
   |     ^^ help: use `drop` to clarify the intent: `drop(z);`

error: aborting due to 3 previous errors

"##;
    let ui = ui(Ui {
        name: "warn-path-statement.rs",
        core: "drop",
        flags: &["-D path-statements"],
        source,
        ..Ui::default()
    });
    assert_eq!(ui, expected);
}

#[test]
fn a_field_pattern_that_binds_the_fields_name_is_redundant() {
    let source = r##"//@ run-rustfix

#![allow(nonstandard_style, unused_variables, unused_mut)]
#![deny(non_shorthand_field_patterns)]

struct Foo {
    x: isize,
    y: isize,
}

fn main() {
    {
        let Foo {
            x: x, //~ ERROR the `x:` in this pattern is redundant
            y: ref y, //~ ERROR the `y:` in this pattern is redundant
        } = Foo { x: 0, y: 0 };

        let Foo {
            x,
            ref y,
        } = Foo { x: 0, y: 0 };
    }

    {
        const x: isize = 1;

        match (Foo { x: 1, y: 1 }) {
            Foo { x: x, ..} => {},
            _ => {},
        }
    }

    {
        struct Bar {
            x: x,
        }

        struct x;

        match (Bar { x: x }) {
            Bar { x: x } => {},
        }
    }

    {
        struct Bar {
            x: Foo,
        }

        enum Foo { x }

        match (Bar { x: Foo::x }) {
            Bar { x: Foo::x } => {},
        }
    }

    {
        struct Baz {
            x: isize,
            y: isize,
            z: isize,
        }

        let Baz {
            x: mut x, //~ ERROR the `x:` in this pattern is redundant
            y: ref y, //~ ERROR the `y:` in this pattern is redundant
            z: ref mut z, //~ ERROR the `z:` in this pattern is redundant
        } = Baz { x: 0, y: 0, z: 0 };
    }
}
"##;
    let expected = r##"error: the `x:` in this pattern is redundant
  --> $DIR/lint-shorthand-field.rs:14:13
   |
LL |             x: x,
   |             ^^^^ help: use shorthand field pattern: `x`
   |
note: the lint level is defined here
  --> $DIR/lint-shorthand-field.rs:4:9
   |
LL | #![deny(non_shorthand_field_patterns)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: the `y:` in this pattern is redundant
  --> $DIR/lint-shorthand-field.rs:15:13
   |
LL |             y: ref y,
   |             ^^^^^^^^ help: use shorthand field pattern: `ref y`

error: the `x:` in this pattern is redundant
  --> $DIR/lint-shorthand-field.rs:65:13
   |
LL |             x: mut x,
   |             ^^^^^^^^ help: use shorthand field pattern: `mut x`

error: the `y:` in this pattern is redundant
  --> $DIR/lint-shorthand-field.rs:66:13
   |
LL |             y: ref y,
   |             ^^^^^^^^ help: use shorthand field pattern: `ref y`

error: the `z:` in this pattern is redundant
  --> $DIR/lint-shorthand-field.rs:67:13
   |
LL |             z: ref mut z,
   |             ^^^^^^^^^^^^ help: use shorthand field pattern: `ref mut z`

error: aborting due to 5 previous errors

"##;
    assert_eq!(ui(Ui { name: "lint-shorthand-field.rs", source, ..Ui::default() }), expected);
}

/// The field pattern is the macro's: only `unused_variables` of the binding handed to it.
#[test]
fn a_field_pattern_written_by_a_macro_is_not_linted() {
    let source = r##"//@ run-pass
#![warn(unused)]
#![deny(non_shorthand_field_patterns)]

pub struct Value<A> { pub value: A }

#[macro_export]
macro_rules! pat {
    ($a:pat) => {
        Value { value: $a }
    };
}

fn main() {
    let pat!(value) = Value { value: () };
    //~^ WARN unused variable: `value`
}
"##;
    let expected = r##"warning: unused variable: `value`
  --> $DIR/issue-49588-non-shorthand-field-patterns-in-pattern-macro.rs:15:14
   |
LL |     let pat!(value) = Value { value: () };
   |              ^^^^^ help: if this is intentional, prefix it with an underscore: `_value`
   |
note: the lint level is defined here
  --> $DIR/issue-49588-non-shorthand-field-patterns-in-pattern-macro.rs:2:9
   |
LL | #![warn(unused)]
   |         ^^^^^^
   = note: `#[warn(unused_variables)]` implied by `#[warn(unused)]`

warning: 1 warning emitted

"##;
    let ui = ui(Ui {
        name: "issue-49588-non-shorthand-field-patterns-in-pattern-macro.rs",
        mode: Mode::RunPass,
        source,
        ..Ui::default()
    });
    assert_eq!(ui, expected);
}

/// The `deny_level` revision. `sort` is std's slice method, given here by a trait of std's
/// prelude.
#[test]
fn a_binding_of_unit_written_as_neither_side_is_linted() {
    let source = r##"//! Basic checks for `unit_bindings` lint.
//!
//! The `unit_bindings` lint tries to detect cases like `let list = list.sort()`. The lint will
//! trigger on bindings that have the unit `()` type **except** if:
//!
//! - The user wrote `()` on either side, i.e.
//!     - `let () = <expr>;` or `let <expr> = ();`
//!     - `let _ = ();`
//! - The binding occurs within macro expansions, e.g. `foo!();`.
//! - The user explicitly provided type annotations, e.g. `let x: () = <expr>`.
//!
//! Examples where the lint *should* fire on include:
//!
//! - `let _ = <expr>;`
//! - `let pat = <expr>;`
//! - `let _pat = <expr>;`

//@ revisions: default_level deny_level
//@[default_level] check-pass (`unit_bindings` is currently allow-by-default)

#![allow(unused)]
#![cfg_attr(deny_level, deny(unit_bindings))]

// The `list` binding below should trigger the lint if it's not contained in a macro expansion.
macro_rules! expands_to_sus {
    () => {
        let mut v = [1, 2, 3];
        let list = v.sort();
    }
}

// No warning for `y` and `z` because it is provided as type parameter.
fn ty_param_check<T: Copy>(x: T) {
    let y = x;
    let z: T = x;
}

fn main() {
    // No warning if user explicitly wrote `()` on either side.
    let expr = ();
    let () = expr;
    let _ = ();
    // No warning if user explicitly annotates the unit type on the binding.
    let pat: () = expr;
    // No warning for let bindings with unit type in macro expansions.
    expands_to_sus!();
    // No warning for unit bindings in generic fns.
    ty_param_check(());

    let _ = expr; //[deny_level]~ ERROR binding has unit type
    let pat = expr; //[deny_level]~ ERROR binding has unit type
    let _pat = expr; //[deny_level]~ ERROR binding has unit type

    let mut v = [1, 2, 3];
    let list = v.sort(); //[deny_level]~ ERROR binding has unit type

    // Limitation: the lint currently does not fire on nested unit LHS bindings, i.e.
    // this will not currently trigger the lint.
    let (nested, _) = (expr, 0i32);
}
"##;
    let expected = r##"error: binding has unit type `()`
  --> $DIR/unit_bindings.rs:50:5
   |
LL |     let _ = expr;
   |     ^^^^-^^^^^^^^
   |         |
   |         this pattern is inferred to be the unit type `()`
   |
note: the lint level is defined here
  --> $DIR/unit_bindings.rs:22:30
   |
LL | #![cfg_attr(deny_level, deny(unit_bindings))]
   |                              ^^^^^^^^^^^^^

error: binding has unit type `()`
  --> $DIR/unit_bindings.rs:51:5
   |
LL |     let pat = expr;
   |     ^^^^---^^^^^^^^
   |         |
   |         this pattern is inferred to be the unit type `()`

error: binding has unit type `()`
  --> $DIR/unit_bindings.rs:52:5
   |
LL |     let _pat = expr;
   |     ^^^^----^^^^^^^^
   |         |
   |         this pattern is inferred to be the unit type `()`

error: binding has unit type `()`
  --> $DIR/unit_bindings.rs:55:5
   |
LL |     let list = v.sort();
   |     ^^^^----^^^^^^^^^^^^
   |         |
   |         this pattern is inferred to be the unit type `()`

error: aborting due to 4 previous errors

"##;
    let ui = ui(Ui {
        name: "unit_bindings.rs",
        core: "copy, slice, unsize",
        std: Some((SORT, "")),
        meta: "cfg:deny_level",
        source,
        ..Ui::default()
    });
    assert_eq!(ui, expected);
}

/// std's `[T]::sort`, as a method of a trait of the fixture std's prelude.
const SORT: &str = "\
pub trait SliceSort { fn sort(&mut self); }
impl<T> SliceSort for [T] { fn sort(&mut self) {} }
";

/// std's `println!`, which expands to nothing these lints look at.
const PRINTLN: &str = "\
#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{}};
}
";

#[test]
fn let_underscore_drops_a_value_with_a_destructor_at_once() {
    let source = r##"//@ check-pass
#![warn(let_underscore_drop)]

struct NontrivialDrop;

impl Drop for NontrivialDrop {
    fn drop(&mut self) {
        println!("Dropping!");
    }
}

fn main() {
    let _ = NontrivialDrop; //~WARNING non-binding let on a type that has a destructor

    let (_, _) = (NontrivialDrop, NontrivialDrop); // This should be ignored.
}
"##;
    let expected = r##"warning: non-binding let on a type that has a destructor
  --> $DIR/let_underscore_drop.rs:13:5
   |
LL |     let _ = NontrivialDrop;
   |     ^^^^^^^^^^^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/let_underscore_drop.rs:2:9
   |
LL | #![warn(let_underscore_drop)]
   |         ^^^^^^^^^^^^^^^^^^^
help: consider binding to an unused variable to avoid immediately dropping the value
   |
LL |     let _unused = NontrivialDrop;
   |          ++++++
help: consider immediately dropping the value
   |
LL -     let _ = NontrivialDrop;
LL +     drop(NontrivialDrop);
   |

warning: 1 warning emitted

"##;
    let ui = ui(Ui {
        name: "let_underscore_drop.rs",
        core: "drop",
        std: Some(("pub use crate::println;", PRINTLN)),
        source,
        ..Ui::default()
    });
    assert_eq!(ui, expected);
}

/// std's `Arc`, `Mutex` and `MutexGuard` as far as the test uses them; `Result::unwrap` as a
/// method of a trait of the fixture std's prelude.
const SYNC: (&str, &str) = (
    "\
pub trait ResultUnwrap<T> { fn unwrap(self) -> T; }
impl<T, E> ResultUnwrap<T> for Result<T, E> { fn unwrap(self) -> T { loop {} } }
",
    "\
pub mod sync {
    use core::ops::Deref;
    pub struct Arc<T: ?Sized> { ptr: *const T }
    impl<T> Arc<T> {
        pub fn new(data: T) -> Arc<T> { loop {} }
    }
    impl<T: ?Sized> Deref for Arc<T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
    impl<T: ?Sized> Drop for Arc<T> {
        fn drop(&mut self) {}
    }
    pub struct Mutex<T: ?Sized> { data: T }
    impl<T> Mutex<T> {
        pub fn new(t: T) -> Mutex<T> { loop {} }
    }
    impl<T: ?Sized> Mutex<T> {
        pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> { loop {} }
    }
    #[rustc_diagnostic_item = \"MutexGuard\"]
    pub struct MutexGuard<'a, T: ?Sized + 'a> { lock: &'a Mutex<T> }
    impl<T: ?Sized> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {}
    }
    pub struct PoisonError<T> { data: T }
    pub type LockResult<T> = Result<T, PoisonError<T>>;
}
",
);

#[test]
fn let_underscore_of_a_lock_releases_it_at_once() {
    let source = r##"//@ check-fail
use std::sync::{Arc, Mutex};

struct Struct<T> {
    a: T,
}

fn main() {
    let data = Arc::new(Mutex::new(0));
    let _ = data.lock().unwrap(); //~ERROR non-binding let on a synchronization lock

    let _ = data.lock(); //~ERROR non-binding let on a synchronization lock

    let (_, _) = (data.lock(), 1); //~ERROR non-binding let on a synchronization lock

    let (_a, Struct { a: _ }) = (0, Struct { a: data.lock() }); //~ERROR non-binding let on a synchronization lock

    (_ , _) = (data.lock(), 1); //~ERROR non-binding let on a synchronization lock

    let _b;
    (_b, Struct { a: _ }) = (0, Struct { a: data.lock() }); //~ERROR non-binding let on a synchronization lock
}
"##;
    let expected = r##"error: non-binding let on a synchronization lock
  --> $DIR/let_underscore_lock.rs:10:9
   |
LL |     let _ = data.lock().unwrap();
   |         ^ this lock is not assigned to a binding and is immediately dropped
   |
   = note: `#[deny(let_underscore_lock)]` (part of `#[deny(let_underscore)]`) on by default
help: consider binding to an unused variable to avoid immediately dropping the value
   |
LL |     let _unused = data.lock().unwrap();
   |          ++++++
help: consider immediately dropping the value
   |
LL -     let _ = data.lock().unwrap();
LL +     drop(data.lock().unwrap());
   |

error: non-binding let on a synchronization lock
  --> $DIR/let_underscore_lock.rs:12:9
   |
LL |     let _ = data.lock();
   |         ^ this lock is not assigned to a binding and is immediately dropped
   |
help: consider binding to an unused variable to avoid immediately dropping the value
   |
LL |     let _unused = data.lock();
   |          ++++++
help: consider immediately dropping the value
   |
LL -     let _ = data.lock();
LL +     drop(data.lock());
   |

error: non-binding let on a synchronization lock
  --> $DIR/let_underscore_lock.rs:14:10
   |
LL |     let (_, _) = (data.lock(), 1);
   |          ^ this lock is not assigned to a binding and is immediately dropped
   |
   = help: consider immediately dropping the value using `drop(..)` after the `let` statement
help: consider binding to an unused variable to avoid immediately dropping the value
   |
LL |     let (_unused, _) = (data.lock(), 1);
   |           ++++++

error: non-binding let on a synchronization lock
  --> $DIR/let_underscore_lock.rs:16:26
   |
LL |     let (_a, Struct { a: _ }) = (0, Struct { a: data.lock() });
   |                          ^ this lock is not assigned to a binding and is immediately dropped
   |
   = help: consider immediately dropping the value using `drop(..)` after the `let` statement
help: consider binding to an unused variable to avoid immediately dropping the value
   |
LL |     let (_a, Struct { a: _unused }) = (0, Struct { a: data.lock() });
   |                           ++++++

error: non-binding let on a synchronization lock
  --> $DIR/let_underscore_lock.rs:18:6
   |
LL |     (_ , _) = (data.lock(), 1);
   |      ^ this lock is not assigned to a binding and is immediately dropped
   |
help: consider binding to an unused variable to avoid immediately dropping the value
  --> $DIR/let_underscore_lock.rs:18:6
   |
LL |     (_ , _) = (data.lock(), 1);
   |      ^
   = help: consider immediately dropping the value using `drop(..)` after the `let` statement

error: non-binding let on a synchronization lock
  --> $DIR/let_underscore_lock.rs:21:22
   |
LL |     (_b, Struct { a: _ }) = (0, Struct { a: data.lock() });
   |                      ^ this lock is not assigned to a binding and is immediately dropped
   |
help: consider binding to an unused variable to avoid immediately dropping the value
  --> $DIR/let_underscore_lock.rs:21:22
   |
LL |     (_b, Struct { a: _ }) = (0, Struct { a: data.lock() });
   |                      ^
   = help: consider immediately dropping the value using `drop(..)` after the `let` statement

error: aborting due to 6 previous errors

"##;
    let ui = ui(Ui {
        name: "let_underscore_lock.rs",
        core: "drop, deref, result, sized",
        std: Some(SYNC),
        source,
        ..Ui::default()
    });
    assert_eq!(ui, expected);
}

/// std's `panic!` and `ops`, as far as the test uses them.
const PANIC_OPS: &str = "\
pub use core::ops;
#[macro_export]
macro_rules! panic {
    () => { loop {} };
}
";

/// `unused_must_use` of these statements is `lower/unused_must_use.rs`'s; `unused_results`
/// is given only where no `unused_must_use` is and the type is not trivial.
#[test]
fn a_result_of_an_ordinary_type_is_unused() {
    let source = r##"#![allow(dead_code)]
#![deny(unused_results, unused_must_use)]
//~^ NOTE: the lint level is defined here
//~| NOTE: the lint level is defined here

use std::ops::ControlFlow;

#[must_use]
enum MustUse { Test }

#[must_use = "some message"]
enum MustUseMsg { Test2 }

enum Nothing {}

fn foo<T>() -> T { panic!() }

fn bar() -> isize { return foo::<isize>(); }
fn baz() -> MustUse { return foo::<MustUse>(); }
fn qux() -> MustUseMsg { return foo::<MustUseMsg>(); }

#[allow(unused_results)]
fn test() {
    foo::<isize>();
    foo::<MustUse>(); //~ ERROR: unused `MustUse` that must be used
    foo::<MustUseMsg>(); //~ ERROR: unused `MustUseMsg` that must be used
    //~^ NOTE: some message
}

#[allow(unused_results, unused_must_use)]
fn test2() {
    foo::<isize>();
    foo::<MustUse>();
    foo::<MustUseMsg>();
}

fn main() {
    foo::<isize>(); //~ ERROR: unused result of type `isize`
    foo::<MustUse>(); //~ ERROR: unused `MustUse` that must be used
    foo::<MustUseMsg>(); //~ ERROR: unused `MustUseMsg` that must be used
    //~^ NOTE: some message

    let _ = foo::<isize>();
    let _ = foo::<MustUse>();
    let _ = foo::<MustUseMsg>();

    // "trivial" types
    ();
    ((), ());
    Ok::<(), Nothing>(());
    ControlFlow::<Nothing>::Continue(());
    ((), Ok::<(), Nothing>(()), ((((), ())), ((),)));
    foo::<Nothing>();

    ((), 1); //~ ERROR: unused result of type `((), i32)`
    (1, ()); //~ ERROR: unused result of type `(i32, ())`
}
"##;
    let ui = ui(Ui {
        name: "unused-result.rs",
        core: "result, try",
        std: Some(("pub use crate::panic;", PANIC_OPS)),
        source,
        ..Ui::default()
    });
    assert_reports(
        &ui,
        &[
            error(25, &["`MustUse`"]),
            error(26, &["`MustUseMsg`"]),
            error(38, &["`isize`"]),
            error(39, &["`MustUse`"]),
            error(40, &["`MustUseMsg`"]),
            error(55, &["`((), i32)`"]),
            error(56, &["`(i32, ())`"]),
        ],
    );
}

/// std's `Box` as far as the test uses it.
const BOX: (&str, &str) = (
    "pub use crate::boxed::Box;",
    "\
pub mod boxed {
    #[lang = \"owned_box\"]
    pub struct Box<T: ?Sized>(*mut T);
    impl<T> Box<T> {
        #[rustc_diagnostic_item = \"box_new\"]
        pub fn new(x: T) -> Box<T> { loop {} }
    }
    impl<T: ?Sized> core::ops::Deref for Box<T> {
        type Target = T;
        fn deref(&self) -> &T { loop {} }
    }
}
",
);

#[test]
fn a_box_borrowed_at_once_is_an_unnecessary_allocation() {
    let source = r##"#![deny(unused_allocation)]

fn main() {
    _ = Box::new([1]).len(); //~ error: unnecessary allocation, use `&` instead
}
"##;
    let expected = r##"error: unnecessary allocation, use `&` instead
  --> $DIR/unused-allocation.rs:4:9
   |
LL |     _ = Box::new([1]).len();
   |         ^^^^^^^^^^^^^
   |
note: the lint level is defined here
  --> $DIR/unused-allocation.rs:1:9
   |
LL | #![deny(unused_allocation)]
   |         ^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    let ui = ui(Ui {
        name: "unused-allocation.rs",
        core: "deref, sized, slice, unsize",
        std: Some(BOX),
        source,
        ..Ui::default()
    });
    assert_eq!(ui, expected);
}
