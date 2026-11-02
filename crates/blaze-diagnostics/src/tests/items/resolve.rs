//! rustc's UI tests of names that do not resolve: imports (E0432), modules with no file (E0583),
//! paths of bodies (E0425 in a module, E0433 of a prefix), imports of a trait's items without
//! `import_trait_associated_functions` (E0658), and the style lints of the names items are given.

use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui},
};

#[test]
fn an_import_from_an_unknown_crate_suggests_the_extern_crate() {
    // tests/ui/error-codes/E0432.rs
    let source = "//@ edition:2015\nuse something::Foo; //~ ERROR E0432\n\nfn main () {\n}\n";
    let stderr = r#"error[E0432]: unresolved import `something`
  --> $DIR/E0432.rs:2:5
   |
LL | use something::Foo;
   |     ^^^^^^^^^ use of unresolved module or unlinked crate `something`
   |
help: you might be missing a crate named `something`, add it to your project and import it in your code
   |
LL + extern crate something;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0432`.
"#;
    assert_eq!(ui("E0432.rs", "2015", source), stderr);
}

#[test]
fn under_cargo_the_missing_crate_help_says_cargo_add() {
    // tests/ui/error-codes/E0432.rs with `rustc-env:CARGO_CRATE_NAME=foo`, as blaze runs it.
    let source = "//@ edition:2015\nuse something::Foo; //~ ERROR E0432\n\nfn main () {\n}\n";
    let stderr = r#"error[E0432]: unresolved import `something`
  --> $DIR/E0432.rs:2:5
   |
LL | use something::Foo;
   |     ^^^^^^^^^ use of unresolved module or unlinked crate `something`
   |
help: if you wanted to use a crate named `something`, use `cargo add something` to add it to your `Cargo.toml` and import it in your code
   |
LL + extern crate something;
   |

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0432`.
"#;
    let mut options = crate::check::Options::default();
    options.lints.check_cfg.cargo = true;
    assert_eq!(crate::tests::ui_with("E0432.rs", "2015", source, &options), stderr);
}

#[test]
fn a_module_with_no_file_says_where_to_create_it() {
    // tests/ui/error-codes/E0583.rs
    let source = "mod module_that_doesnt_exist; //~ ERROR E0583\n\nfn main() {\n}\n";
    let stderr = r#"error[E0583]: file not found for module `module_that_doesnt_exist`
  --> $DIR/E0583.rs:1:1
   |
LL | mod module_that_doesnt_exist;
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: to create the module `module_that_doesnt_exist`, create file "$DIR/module_that_doesnt_exist.rs" or "$DIR/module_that_doesnt_exist/mod.rs"
   = note: if there is a `mod module_that_doesnt_exist` elsewhere in the crate already, import it with `use crate::...` instead

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0583`.
"#;
    assert_eq!(ui("E0583.rs", "2015", source), stderr);
}

#[test]
fn statics_should_have_upper_case_names() {
    // tests/ui/lint/lint-non-uppercase-statics.rs
    let source = r#"#![forbid(non_upper_case_globals)]
#![allow(dead_code)]

static foo: isize = 1; //~ ERROR static variable `foo` should have an upper case name

static mut bar: isize = 1; //~ ERROR static variable `bar` should have an upper case name

#[no_mangle]
pub static extern_foo: isize = 1; // OK, because #[no_mangle] supersedes the warning

fn main() { }
"#;
    let stderr = r#"error: static variable `foo` should have an upper case name
  --> $DIR/lint-non-uppercase-statics.rs:4:8
   |
LL | static foo: isize = 1;
   |        ^^^ help: convert the identifier to upper case (notice the capitalization): `FOO`
   |
note: the lint level is defined here
  --> $DIR/lint-non-uppercase-statics.rs:1:11
   |
LL | #![forbid(non_upper_case_globals)]
   |           ^^^^^^^^^^^^^^^^^^^^^^

error: static variable `bar` should have an upper case name
  --> $DIR/lint-non-uppercase-statics.rs:6:12
   |
LL | static mut bar: isize = 1;
   |            ^^^ help: convert the identifier to upper case: `BAR`

error: aborting due to 2 previous errors

"#;
    assert_eq!(ui("lint-non-uppercase-statics.rs", "2015", source), stderr);
}

#[test]
fn a_function_a_module_lacks_is_e0425_in_that_module() {
    // tests/ui/namespace/namespaced-enum-glob-import-no-impls.rs
    let source = r##"mod m2 {
    pub enum Foo {
        A,
        B(isize),
        C { a: isize },
    }

    impl Foo {
        pub fn foo() {}
        pub fn bar(&self) {}
    }
}

mod m {
    pub use crate::m2::Foo::*;
}

pub fn main() {
    use crate::m2::Foo::*;

    foo(); //~ ERROR cannot find function `foo` in this scope
    m::foo(); //~ ERROR cannot find function `foo` in module `m`
    bar(); //~ ERROR cannot find function `bar` in this scope
    m::bar(); //~ ERROR cannot find function `bar` in module `m`
}
"##;
    assert_reports(
        &ui("namespaced-enum-glob-import-no-impls.rs", "2015", source),
        &[
            error(22, &["`foo`", "`m`"]),
            error(24, &["`bar`", "`m`"]),
            error(21, &["`foo`"]),
            error(23, &["`bar`"]),
        ],
    );
}

#[test]
fn a_prefix_of_a_body_path_that_names_nothing_is_e0433() {
    // tests/ui/resolve/export-fully-qualified-2018.rs
    let source = r##"//@ edition:2018

// In this test baz isn't resolved when called as foo.baz even though
// it's called from inside foo. This is somewhat surprising and may
// want to change eventually.

mod foo {
    pub fn bar() { foo::baz(); } //~ ERROR: cannot find
    //~^ NOTE: use of unresolved module or unlinked crate `foo`

    fn baz() { }
}

fn main() { }
"##;
    let stderr = r##"error[E0433]: cannot find module or crate `foo` in this scope
  --> $DIR/export-fully-qualified-2018.rs:8:20
   |
LL |     pub fn bar() { foo::baz(); }
   |                    ^^^ use of unresolved module or unlinked crate `foo`
   |
   = help: you might be missing a crate named `foo`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0433`.
"##;
    assert_eq!(ui("export-fully-qualified-2018.rs", "2018", source), stderr);
}

/// A `std` with the one item the test reads beside minicore's: `HashMap`, with `Default`.
const STD_HASH_MAP: &str = "//- /std.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub use core::prelude::rust_2018::*;
    }
}
pub mod collections {
    pub struct HashMap<K, V>(K, V);
    impl<K, V> core::default::Default for HashMap<K, V> {
        fn default() -> Self {
            loop {}
        }
    }
}
";

#[test]
fn an_import_of_a_traits_items_is_unstable_once_per_name_and_glob() {
    // tests/ui/feature-gates/feature-gate-import-trait-associated-functions.rs
    let source = r#"//@ edition:2018
use std::collections::HashMap;

use A::{DEFAULT, new};
//~^ ERROR `use` associated items of traits is unstable [E0658]
//~| ERROR `use` associated items of traits is unstable [E0658]
use Default::default;
//~^ ERROR `use` associated items of traits is unstable [E0658]

struct S {
    a: HashMap<i32, i32>,
}

impl S {
    fn new() -> S {
        S { a: default() }
    }
}

trait A: Sized {
    const DEFAULT: Option<Self> = None;
    fn new() -> Self;
    fn do_something(&self);
}

mod b {
    use super::A::{self, DEFAULT, new};
    //~^ ERROR `use` associated items of traits is unstable [E0658]
    //~| ERROR `use` associated items of traits is unstable [E0658]

    struct B();

    impl A for B {
        const DEFAULT: Option<Self> = Some(B());
        fn new() -> Self {
            B()
        }

        fn do_something(&self) {}
    }

    fn f() {
        let b: B = new();
        b.do_something();
        let c: B = DEFAULT.unwrap();
    }
}

impl A for S {
    fn new() -> Self {
        S::new()
    }

    fn do_something(&self) {}
}

fn f() {
    let s: S = new();
    s.do_something();
    let t: Option<S> = DEFAULT;
}

trait Glob {}
use Glob::*; //~ ERROR `use` associated items of traits is unstable

fn main() {}
"#;
    let name = "feature-gate-import-trait-associated-functions.rs";
    let fixture = format!("//- minicore: sized, default, option\n{STD_HASH_MAP}//- /{name} crate:other:main edition:2018 deps:std\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    let unstable = |line| error(line, &["`use` associated items of traits"]);
    assert_reports(
        &normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() })),
        &[unstable(4), unstable(4), unstable(7), unstable(27), unstable(27), unstable(64)],
    );
}
