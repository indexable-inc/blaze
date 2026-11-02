//! `unreachable_code`, from rustc's UI tests (`tests/ui/reachable/`) with their `.stderr`.

use super::types::ui_std;
use crate::{
    Options,
    emit::Style,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui},
};

/// std's `println!` and what it calls, for the tests that write one: what std's prelude has of
/// it, and its items.
pub(crate) const PRINTLN: (&str, &str) = (
    "pub use crate::println;",
    r#"pub mod io {
    pub fn _print(_args: core::fmt::Arguments<'_>) {}
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::io::_print(core::format_args_nl!($($arg)*));
    }};
}
"#,
);

/// [`ui`] beside a core that declares the library feature `type_ascription`, as
/// `library/core/src/macros/mod.rs` does, for the tests that enable it.
fn ui_ascription(name: &str, source: &str) -> String {
    let fixture = format!("{ASCRIPTION_CORE}//- /{name} crate:other:main edition:2015 deps:core\n{source}");
    let checked = checked_with(&fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

const ASCRIPTION_CORE: &str = r#"//- /core.rs crate:core edition:2024
#![feature(decl_macro, staged_api)]
#![stable(feature = "core", since = "1.6.0")]
#[stable(feature = "core_prelude", since = "1.4.0")]
pub mod prelude {
    #[stable(feature = "prelude_2015", since = "1.55.0")]
    pub mod rust_2015 {}
}
#[unstable(feature = "type_ascription", issue = "23416", reason = "placeholder syntax for type ascription")]
pub macro type_ascribe($expr:expr, $ty:ty) {}
"#;

#[test]
fn a_return_in_a_return() {
    // tests/ui/reachable/expr_return.rs
    let source = r##"#![allow(unused_variables)]
#![allow(unused_assignments)]
#![allow(dead_code)]
#![deny(unreachable_code)]
#![feature(type_ascription)]

fn a() {
    // Here we issue that the "2nd-innermost" return is unreachable,
    // but we stop there.
    let x: () = {return {return {return;}}}; //~ ERROR unreachable
}

fn main() { }
"##;
    let stderr = r##"error: unreachable expression
  --> $DIR/expr_return.rs:10:26
   |
LL |     let x: () = {return {return {return;}}};
   |                          ^^^^^^^^------^^
   |                          |       |
   |                          |       any code following this expression is unreachable
   |                          unreachable expression
   |
note: the lint level is defined here
  --> $DIR/expr_return.rs:4:9
   |
LL | #![deny(unreachable_code)]
   |         ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui_ascription("expr_return.rs", source), stderr);
}

#[test]
fn an_argument_after_a_return_and_a_call_of_one() {
    // tests/ui/reachable/expr_call.rs
    let source = r##"#![allow(unused_variables)]
#![allow(unused_assignments)]
#![allow(dead_code)]
#![deny(unreachable_code)]

fn foo(x: !, y: usize) { }

fn bar(x: !) { }

fn a() {
    // the `22` is unreachable:
    foo(return, 22); //~ ERROR unreachable
}

fn b() {
    // the call is unreachable:
    bar(return); //~ ERROR unreachable
}

fn main() { }
"##;
    assert_reports(&ui("expr_call.rs", "2015", source), &[error(12, &["unreachable expression", "22"]), error(17, &["unreachable call", "bar"])]);
}

#[test]
fn an_argument_after_a_return_and_a_method_call_of_one() {
    // tests/ui/reachable/expr_method.rs
    let source = r##"#![allow(unused_variables)]
#![allow(unused_assignments)]
#![allow(dead_code)]
#![deny(unreachable_code)]

struct Foo;

impl Foo {
    fn foo(&self, x: !, y: usize) { }
    fn bar(&self, x: !) { }
}

fn a() {
    // the `22` is unreachable:
    Foo.foo(return, 22); //~ ERROR unreachable
}

fn b() {
    // the call is unreachable:
    Foo.bar(return); //~ ERROR unreachable
}

fn main() { }
"##;
    assert_reports(&ui("expr_method.rs", "2015", source), &[error(15, &["unreachable expression", "22"]), error(20, &["unreachable call", "bar"])]);
}

#[test]
fn an_element_after_a_return_and_an_array_of_one() {
    // tests/ui/reachable/expr_array.rs
    let source = r##"#![allow(unused_variables)]
#![allow(unused_assignments)]
#![allow(dead_code)]
#![deny(unreachable_code)]
#![feature(type_ascription)]

fn a() {
    // the `22` is unreachable:
    let x: [usize; 2] = [return, 22]; //~ ERROR unreachable
}

fn b() {
    // the array is unreachable:
    let x: [usize; 2] = [22, return]; //~ ERROR unreachable
}

fn main() { }
"##;
    let stderr = r##"error: unreachable expression
  --> $DIR/expr_array.rs:9:34
   |
LL |     let x: [usize; 2] = [return, 22];
   |                          ------  ^^ unreachable expression
   |                          |
   |                          any code following this expression is unreachable
   |
note: the lint level is defined here
  --> $DIR/expr_array.rs:4:9
   |
LL | #![deny(unreachable_code)]
   |         ^^^^^^^^^^^^^^^^

error: unreachable expression
  --> $DIR/expr_array.rs:14:25
   |
LL |     let x: [usize; 2] = [22, return];
   |                         ^^^^^------^
   |                         |    |
   |                         |    any code following this expression is unreachable
   |                         unreachable expression

error: aborting due to 2 previous errors

"##;
    assert_eq!(ui_ascription("expr_array.rs", source), stderr);
}

#[test]
fn a_let_after_a_loop_without_a_break() {
    // tests/ui/reachable/unreachable-code.rs
    let source = r##"#![deny(unreachable_code)]
#![allow(unused_variables)]

fn main() {
  loop{}

  let a = 3; //~ ERROR: unreachable statement
}
"##;
    let stderr = r##"error: unreachable statement
  --> $DIR/unreachable-code.rs:7:3
   |
LL |   loop{}
   |   ------ any code following this expression is unreachable
LL |
LL |   let a = 3;
   |   ^^^^^^^^^^ unreachable statement
   |
note: the lint level is defined here
  --> $DIR/unreachable-code.rs:1:9
   |
LL | #![deny(unreachable_code)]
   |         ^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("unreachable-code.rs", "2015", source), stderr);
}

#[test]
fn a_macro_statement_after_a_continue() {
    // tests/ui/reachable/expr_again.rs
    let source = r##"#![allow(unused_variables)]

#![deny(unreachable_code)]

fn main() {
    let x = loop {
        continue;
        println!("hi");
        //~^ ERROR unreachable statement
    };
}
"##;
    assert_reports(&ui_std("expr_again.rs", "2015", "sized, fmt", PRINTLN, source), &[error(8, &[])]);
}

#[test]
fn what_follows_a_match_whose_arms_all_diverge() {
    // tests/ui/reachable/expr_match.rs
    let source = r##"#![allow(unused_variables)]
#![allow(unused_assignments)]
#![allow(dead_code)]
#![deny(unreachable_code)]

fn b() {
    match () { () => return }
    println!("I am dead");
    //~^ ERROR unreachable statement
}

fn c() {
    match () { () if false => return, () => () }
    println!("I am not dead");
}

fn d() {
    match () { () if false => return, () => return }
    println!("I am dead");
    //~^ ERROR unreachable statement
}

fn e() {
    match () {
        () if return => (),
        //~^ ERROR unreachable expression
        () => return,
    }
    println!("I am dead");
    //~^ ERROR unreachable statement
}

fn f() {
    match Some(()) { None => (), Some(()) => return }
    println!("I am not dead");
}

fn g() {
    match Some(()) { None => return, Some(()) => () }
    println!("I am not dead");
}

fn main() { }
"##;
    assert_reports(
        &ui_std("expr_match.rs", "2015", "sized, fmt, option", PRINTLN, source),
        &[
            error(8, &[]),
            error(19, &[]),
            error(25, &[]),
            error(29, &[]),
        ],
    );
}
