//! Parentheses around a pattern: of a `let`, a `for`, an argument, a match arm and the
//! subpatterns of one.

use crate::{
    Options,
    tests::{assert_reports, error, ui, ui_run_pass, warning},
};

#[test]
fn a_space_takes_the_place_of_parentheses_after_a_keyword() {
    let source = r#"//@ run-rustfix
#![deny(unused_parens)]

fn main() {
    if let(Some(_))= Some(1) {}
    //~^ ERROR unnecessary parentheses around pattern

    for(_x)in 1..10 {}
    //~^ ERROR unnecessary parentheses around pattern

    if(2 == 1) {}
    //~^ ERROR unnecessary parentheses around `if` condition

    // reported by parser
    for(_x in 1..10) {}
    //~^ ERROR unexpected parentheses surrounding
}
"#;
    assert_reports(
        &ui("issue-103435-extra-parentheses.rs", "2015", source),
        &[
            error(15, &[]),
            error(5, &[]),
            error(8, &[]),
            error(11, &["`if`"]),
        ],
    );
}

/// What a macro wrote is linted without a suggestion, once for each expansion.
#[test]
fn parentheses_a_macro_wrote_are_linted_in_each_expansion() {
    let source = r#"//@ run-pass

#![warn(unused_parens)]
#![allow(dead_code)]

trait Foo {
    fn bar();
    fn tar();
}

macro_rules! unused_parens {
    ($ty:ident) => {
        impl<$ty: Foo> Foo for ($ty,) {
            fn bar() { <$ty>::bar() }
            fn tar() {}
        }
    };

    ($ty:ident $(, $rest:ident)*) => {
        impl<$ty: Foo, $($rest: Foo),*> Foo for ($ty, $($rest),*) {
            fn bar() {
                <$ty>::bar();
                <($($rest),*)>::bar() //~WARN unnecessary parentheses around type
            }
            fn tar() {
              let (_t) = 1; //~WARN unnecessary parentheses around pattern
                            //~| WARN unnecessary parentheses around pattern
              let (_t1,) = (1,);
              let (_t2, _t3) = (1, 2);
            }
        }

        unused_parens!($($rest),*);
    }
}

unused_parens!(T1, T2, T3);

fn main() {}
"#;
    assert_reports(
        &ui_run_pass("unused-parens-in-macro-issue-120642.rs", "2015", source, &Options::default()),
        &[
            warning(26, &[]),
            warning(23, &[]),
            warning(26, &[]),
        ],
    );
}
