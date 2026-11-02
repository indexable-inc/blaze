//! `unconditional_recursion`, from rustc's UI tests (`tests/ui/enum/`, `tests/ui/consts/`) with
//! their `.stderr`.

use crate::tests::{assert_reports, error, ui, warning};

#[test]
fn a_function_that_calls_itself_in_a_module_named_as_an_enum() {
    // tests/ui/enum/enum-and-module-in-same-scope.rs
    let source = r##"enum Foo {
    X
}

mod Foo { //~ ERROR the name `Foo` is defined multiple times
    pub static X: isize = 42;
    fn f() { f() } // Check that this does not result in a resolution error
    //~^ WARN cannot return without recursing
}

fn main() {}
"##;
    assert_reports(
        &ui("enum-and-module-in-same-scope.rs", "2015", source),
        &[
            error(5, &["`Foo`"]),
            warning(7, &[]),
        ],
    );
}

#[test]
fn a_const_fn_that_calls_itself() {
    // tests/ui/consts/recursive.rs
    let source = r##"#![allow(unused)]

const fn f<T>(x: T) { //~ WARN function cannot return without recursing
    f(x);
}

const X: () = f(1); //~ ERROR reached the configured maximum number of stack frames

fn main() {}
"##;
    assert_reports(
        &ui("recursive.rs", "2015", source),
        &[warning(3, &[]), error(7, &[])],
    );
}
