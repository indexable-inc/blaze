//! rustc's UI tests of a call with the wrong arguments: too few, too many, swapped and
//! permuted, of a function and of a closure.

use super::ui_core;
use crate::tests::{assert_reports, error, ui};

#[test]
fn a_missing_argument_is_provided() {
    // tests/ui/error-codes/E0061.rs
    let source = "fn f(a: u16, b: &str) {}

fn f2(a: u16) {}

fn main() {
    f(0);
    //~^ ERROR E0061

    f2();
    //~^ ERROR E0061
}
";
    let stderr = "error[E0061]: this function takes 2 arguments but 1 argument was supplied
  --> $DIR/E0061.rs:6:5
   |
LL |     f(0);
   |     ^--- argument #2 of type `&str` is missing
   |
note: function defined here
  --> $DIR/E0061.rs:1:4
   |
LL | fn f(a: u16, b: &str) {}
   |    ^         -------
help: provide the argument
   |
LL |     f(0, /* &str */);
   |        ++++++++++++

error[E0061]: this function takes 1 argument but 0 arguments were supplied
  --> $DIR/E0061.rs:9:5
   |
LL |     f2();
   |     ^^-- argument #1 of type `u16` is missing
   |
note: function defined here
  --> $DIR/E0061.rs:3:4
   |
LL | fn f2(a: u16) {}
   |    ^^ ------
help: provide the argument
   |
LL |     f2(/* u16 */);
   |        +++++++++

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0061`.
";
    assert_eq!(ui("E0061.rs", "2015", source), stderr);
}

#[test]
fn a_closure_called_with_too_few_and_too_many_arguments() {
    // tests/ui/error-codes/E0057.rs
    let source = "fn main() {
    let f = |x| x * 3;
    let a = f(); //~ ERROR E0057
    let b = f(4);
    let c = f(2, 3); //~ ERROR E0057
}
";
    assert_reports(&ui_core("E0057.rs", "fn", source), &[error(3, &[]), error(5, &[])]);
}

#[test]
fn each_form_of_wrong_arguments() {
    // tests/ui/argument-suggestions/basic.rs
    let source = "// Some basic \"obvious\" cases for the heuristic error messages added for #65853
// One for each of the detected cases

enum E { X, Y }
enum F { X2, Y2 }
struct G {}
struct H {}
struct X {}
struct Y {}
struct Z {}


fn invalid(_i: u32) {}
fn extra() {}
fn missing(_i: u32) {}
fn swapped(_i: u32, _s: &str) {}
fn permuted(_x: X, _y: Y, _z: Z) {}

fn main() {
    invalid(1.0); //~ ERROR mismatched types
    extra(\"\"); //~ ERROR function takes
    missing(); //~ ERROR function takes
    swapped(\"\", 1); //~ ERROR arguments to this function are incorrect
    permuted(Y {}, Z {}, X {}); //~ ERROR arguments to this function are incorrect

    let closure = |x| x;
    closure(); //~ ERROR function takes
}
";
    assert_reports(
        &ui_core("basic.rs", "fn", source),
        &[
            error(20, &[]),
            error(21, &[]),
            error(22, &[]),
            error(23, &[]),
            error(24, &[]),
            error(27, &[]),
        ],
    );
}
