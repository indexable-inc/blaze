//! rustc's UI tests of where `await`, `break`, `continue`, labels, `return` and `yield` may
//! be (E0728, E0267, E0268, E0571, E0695, E0696, E0426, E0767, E0572, E0627), and of
//! `unused_must_use`. A test given in part keeps its lines where they are, so the lines and
//! columns of the expectation are the test's.

use crate::tests::{assert_reports, error, items::ui_core, ui, warning};

#[test]
fn an_await_in_a_closure_points_at_the_closure() {
    // tests/ui/async-await/issues/issue-51719.rs
    let source = r#"//@ edition:2018
//
// Tests that the .await syntax can't be used to make a coroutine

async fn foo() {}

fn make_coroutine() {
    let _gen = || foo().await;
    //~^ ERROR `await` is only allowed inside `async` functions and blocks
}

fn main() {}
"#;
    let stderr = r#"error[E0728]: `await` is only allowed inside `async` functions and blocks
  --> $DIR/issue-51719.rs:8:25
   |
LL |     let _gen = || foo().await;
   |                --       ^^^^^ only allowed inside `async` functions and blocks
   |                |
   |                this is not `async`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0728`.
"#;
    assert_eq!(ui_core("issue-51719.rs", "2018", "future", source), stderr);
}

#[test]
fn an_await_in_a_function_points_at_it_and_not_at_a_closure_before() {
    // tests/ui/async-await/issues/non-async-enclosing-span.rs
    let source = r#"//@ edition:2018

async fn do_the_thing() -> u8 {
    8
}
// #63398: point at the enclosing scope and not the previously seen closure
fn main() {  //~ NOTE this is not `async`
    let x = move || {};
    let y = do_the_thing().await; //~ ERROR `await` is only allowed inside `async` functions
    //~^ NOTE only allowed inside `async` functions and blocks
}
"#;
    let stderr = r#"error[E0728]: `await` is only allowed inside `async` functions and blocks
  --> $DIR/non-async-enclosing-span.rs:9:28
   |
LL | fn main() {
   | --------- this is not `async`
LL |     let x = move || {};
LL |     let y = do_the_thing().await;
   |                            ^^^^^ only allowed inside `async` functions and blocks

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0728`.
"#;
    assert_eq!(ui_core("non-async-enclosing-span.rs", "2018", "future", source), stderr);
}

#[test]
fn an_await_of_a_local_future_points_at_the_function() {
    // tests/ui/async-await/issues/issue-51751.rs
    let source = r#"//@ edition:2018

async fn inc(limit: i64) -> i64 {
    limit + 1
}

fn main() {
    let result = inc(10000);
    let finished = result.await;
    //~^ ERROR `await` is only allowed inside `async` functions and blocks
}
"#;
    let stderr = r#"error[E0728]: `await` is only allowed inside `async` functions and blocks
  --> $DIR/issue-51751.rs:9:27
   |
LL | fn main() {
   | --------- this is not `async`
LL |     let result = inc(10000);
LL |     let finished = result.await;
   |                           ^^^^^ only allowed inside `async` functions and blocks

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0728`.
"#;
    assert_eq!(ui_core("issue-51751.rs", "2018", "future", source), stderr);
}

#[test]
fn an_await_of_an_async_closures_call_points_at_the_function() {
    // tests/ui/async-await/issues/issue-62009-2.rs
    let source = r#"//@ edition:2018

async fn print_dur() {}

fn main() {
    (async || 2333)().await;
    //~^ ERROR `await` is only allowed inside `async` functions and blocks
}
"#;
    let stderr = r#"error[E0728]: `await` is only allowed inside `async` functions and blocks
  --> $DIR/issue-62009-2.rs:6:23
   |
LL | fn main() {
   | --------- this is not `async`
LL |     (async || 2333)().await;
   |                       ^^^^^ only allowed inside `async` functions and blocks

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0728`.
"#;
    assert_eq!(ui_core("issue-62009-2.rs", "2018", "async_fn", source), stderr);
}

#[test]
fn an_await_in_an_array_length_points_at_the_length() {
    // tests/ui/async-await/issue-70594.rs
    let source = r#"//@ edition:2018

async fn fun() {
    [1; ().await];
    //~^ error: `await` is only allowed inside `async` functions and blocks
}

fn main() {}
"#;
    assert_reports(&ui_core("issue-70594.rs", "2018", "future", source), &[error(4, &[])]);
}

#[test]
fn control_flow_does_not_cross_an_inline_const() {
    // tests/ui/inline-const/cross-const-control-flow-125846.rs, without its
    // `reference_to_non_constant_in_const_block`: rustc gives its E0435, of resolution, before
    // every error of AST lowering, and the diagnostics have no phase of AST lowering yet to
    // put the `await` error in after it.
    let source = r#"//@edition:2021

fn foo() {
    const { return }
    //~^ ERROR: return statement outside of function body
}

fn labelled_block_break() {
    'a: { const { break 'a } }
    //~^ ERROR: `break` outside of a loop or labeled block
    //~| ERROR: use of unreachable label
}

fn loop_break() {
    loop {
        const { break }
        //~^ ERROR: `break` outside of a loop or labeled block
    }
}

fn continue_to_labelled_block() {
    'a: { const { continue 'a } }
    //~^ ERROR: `continue` outside of a loop
    //~| ERROR: use of unreachable label
}

fn loop_continue() {
    loop {
        const { continue }
        //~^ ERROR: `continue` outside of a loop
    }
}

async fn await_across_const_block() {
    const { async {}.await }
    //~^ ERROR: `await` is only allowed inside `async` functions and blocks
}

fn main() {}
"#;
    assert_reports(
        &ui("cross-const-control-flow-125846.rs", "2021", source),
        &[
            error(9, &["`'a`"]),
            error(22, &["`'a`"]),
            error(35, &["`async`", "`await`"]),
            error(4, &[]),
            error(9, &["`break`"]),
            error(16, &["`break`"]),
            error(22, &["`continue`"]),
            error(29, &["`continue`"]),
        ],
    );
}

#[test]
fn a_break_in_a_closure_points_at_the_closure() {
    // tests/ui/error-codes/E0267.rs
    let source = r#"fn main() {
    let w = || { break; }; //~ ERROR E0267
}
"#;
    let stderr = r#"error[E0267]: `break` inside of a closure
  --> $DIR/E0267.rs:2:18
   |
LL |     let w = || { break; };
   |             --   ^^^^^ cannot `break` inside of a closure
   |             |
   |             enclosing closure

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0267`.
"#;
    assert_eq!(ui("E0267.rs", "2015", source), stderr);
}

#[test]
fn a_break_outside_of_a_loop() {
    // tests/ui/error-codes/E0268.rs
    let source = r#"fn main() {
    break; //~ ERROR E0268
}
"#;
    let stderr = r#"error[E0268]: `break` outside of a loop or labeled block
  --> $DIR/E0268.rs:2:5
   |
LL |     break;
   |     ^^^^^ cannot `break` outside of a loop or labeled block

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0268`.
"#;
    assert_eq!(ui("E0268.rs", "2015", source), stderr);
}

#[test]
fn a_break_with_a_value_from_a_while_loop_suggests_breaking_without_it() {
    // tests/ui/typeck/issue-114529-illegal-break-with-value.rs: rustc checks `main` before
    // the anonymous consts of its array lengths.
    let source = r#"// Regression test for issue #114529
// Tests that we do not ICE during const eval for a
// break-with-value in contexts where it is illegal

#[allow(while_true)]
fn main() {
    [(); {
        while true {
            break 9; //~ ERROR `break` with value from a `while` loop
        };
        51
    }];

    [(); {
        while let Some(v) = Some(9) {
            break v; //~ ERROR `break` with value from a `while` loop
        };
        51
    }];

    while true {
        break (|| { //~ ERROR `break` with value from a `while` loop
            let local = 9;
        });
    }
}
"#;
    let stderr = r#"error[E0571]: `break` with value from a `while` loop
  --> $DIR/issue-114529-illegal-break-with-value.rs:22:9
   |
LL |       while true {
   |       ---------- you can't `break` with a value in a `while` loop
LL | /         break (|| {
LL | |             let local = 9;
LL | |         });
   | |__________^ can only break with a value inside `loop` or breakable block
   |
help: use `break` on its own without a value inside this `while` loop
   |
LL -         break (|| {
LL -             let local = 9;
LL -         });
LL +         break;
   |

error[E0571]: `break` with value from a `while` loop
  --> $DIR/issue-114529-illegal-break-with-value.rs:9:13
   |
LL |         while true {
   |         ---------- you can't `break` with a value in a `while` loop
LL |             break 9;
   |             ^^^^^^^ can only break with a value inside `loop` or breakable block
   |
help: use `break` on its own without a value inside this `while` loop
   |
LL -             break 9;
LL +             break;
   |

error[E0571]: `break` with value from a `while` loop
  --> $DIR/issue-114529-illegal-break-with-value.rs:16:13
   |
LL |         while let Some(v) = Some(9) {
   |         --------------------------- you can't `break` with a value in a `while` loop
LL |             break v;
   |             ^^^^^^^ can only break with a value inside `loop` or breakable block
   |
help: use `break` on its own without a value inside this `while` loop
   |
LL -             break v;
LL +             break;
   |

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0571`.
"#;
    assert_eq!(ui_core("issue-114529-illegal-break-with-value.rs", "2015", "option", source), stderr);
}

#[test]
fn an_unlabeled_continue_in_a_labeled_block_and_a_continue_to_one() {
    // tests/ui/label/label_break_value_continue.rs, without `continue_crossing`: in a loop,
    // the front end takes its `continue` to be the loop's.
    let source = r#"#![allow(unused_labels)]

// Simple continue pointing to an unlabeled break should yield in an error
fn continue_simple() {
    'b: {
        continue; //~ ERROR unlabeled `continue` inside of a labeled block
    }
}

// Labeled continue pointing to an unlabeled break should yield in an error
fn continue_labeled() {
    'b: {
        continue 'b; //~ ERROR `continue` pointing to a labeled block
    }
}

pub fn main() {}
"#;
    let stderr = r#"error[E0695]: unlabeled `continue` inside of a labeled block
  --> $DIR/label_break_value_continue.rs:6:9
   |
LL |         continue;
   |         ^^^^^^^^ `continue` statements that would diverge to or through a labeled block need to bear a label

error[E0696]: `continue` pointing to a labeled block
  --> $DIR/label_break_value_continue.rs:13:9
   |
LL | /     'b: {
LL | |         continue 'b;
   | |         ^^^^^^^^^^^ labeled blocks cannot be `continue`'d
LL | |     }
   | |_____- labeled block the `continue` points to

error: aborting due to 2 previous errors

Some errors have detailed explanations: E0695, E0696.
For more information about an error, try `rustc --explain E0695`.
"#;
    assert_eq!(ui("label_break_value_continue.rs", "2015", source), stderr);
}

#[test]
fn an_unlabeled_break_in_a_labeled_block() {
    // tests/ui/label/label_break_value_unlabeled_break.rs, without
    // `unlabeled_break_crossing`: in a loop, the front end takes its `break` to be the
    // loop's.
    let source = r#"#![allow(unused_labels)]

// Simple unlabeled break should yield in an error
fn unlabeled_break_simple() {
    'b: {
        break; //~ ERROR unlabeled `break` inside of a labeled block
    }
}

pub fn main() {}
"#;
    let stderr = r#"error[E0695]: unlabeled `break` inside of a labeled block
  --> $DIR/label_break_value_unlabeled_break.rs:6:9
   |
LL |         break;
   |         ^^^^^ `break` statements that would diverge to or through a labeled block need to bear a label

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0695`.
"#;
    assert_eq!(ui("label_break_value_unlabeled_break.rs", "2015", source), stderr);
}

#[test]
fn a_continue_to_a_labeled_block_in_a_match_arm() {
    // tests/ui/label/continue-pointing-to-block-ice-121623.rs
    let source = r#"//! Regression test for ICE #121623. Liveness linting assumes that `continue`s all point to loops.
//! This tests that if a `continue` points to a block, we don't run liveness lints.

fn main() {
    match () {
        _ => 'b: {
            continue 'b;
            //~^ ERROR `continue` pointing to a labeled block
        }
    }
}
"#;
    let stderr = r#"error[E0696]: `continue` pointing to a labeled block
  --> $DIR/continue-pointing-to-block-ice-121623.rs:7:13
   |
LL |           _ => 'b: {
   |  ______________-
LL | |             continue 'b;
   | |             ^^^^^^^^^^^ labeled blocks cannot be `continue`'d
LL | |
LL | |         }
   | |_________- labeled block the `continue` points to

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0696`.
"#;
    assert_eq!(ui("continue-pointing-to-block-ice-121623.rs", "2015", source), stderr);
}

#[test]
fn a_return_in_a_constant() {
    // tests/ui/error-codes/E0572.rs
    let source = r#"const FOO: u32 = return 0; //~ ERROR E0572

fn main() {}
"#;
    let stderr = r#"error[E0572]: return statement outside of function body
  --> $DIR/E0572.rs:1:18
   |
LL | const FOO: u32 = return 0;
   |                  ^^^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0572`.
"#;
    assert_eq!(ui("E0572.rs", "2015", source), stderr);
}

#[test]
fn a_return_in_a_constants_block() {
    // tests/ui/consts/const-return-outside-fn.rs
    let source = r#"//! regression test for issue <https://github.com/rust-lang/rust/issues/38458>
const x: () = {
    return; //~ ERROR return statement outside of function body
};

fn main() {}
"#;
    let stderr = r#"error[E0572]: return statement outside of function body
  --> $DIR/const-return-outside-fn.rs:3:5
   |
LL |     return;
   |     ^^^^^^

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0572`.
"#;
    assert_eq!(ui("const-return-outside-fn.rs", "2015", source), stderr);
}

#[test]
fn a_return_in_an_array_length_points_at_the_function_it_is_not_of() {
    // tests/ui/return/issue-86188-return-not-in-fn-body.rs
    let source = r#"// Due to a compiler bug, if a return occurs outside of a function body
// (e.g. in an AnonConst body), the return value expression would not be
// type-checked, leading to an ICE. This test checks that the ICE no
// longer happens, and that an appropriate error message is issued that
// also explains why the return is considered "outside of a function body"
// if it seems to be inside one, as in the main function below.

const C: [(); 42] = {
    [(); return || {
        //~^ ERROR: return statement outside of function body [E0572]
        let tx;
    }]
};

struct S {}
trait Tr {
    fn foo();
    fn bar() {
        //~^ NOTE: ...not the enclosing function body
        [(); return];
        //~^ ERROR: return statement outside of function body [E0572]
        //~| NOTE: the return is part of this body...
    }
}
impl Tr for S {
    fn foo() {
        //~^ NOTE: ...not the enclosing function body
        [(); return];
        //~^ ERROR: return statement outside of function body [E0572]
        //~| NOTE: the return is part of this body...
    }
}

fn main() {
    //~^ NOTE: ...not the enclosing function body
    [(); return || {
        //~^ ERROR: return statement outside of function body [E0572]
        //~| NOTE: the return is part of this body...
        let tx;
    }];
}
"#;
    assert_reports(
        &ui("issue-86188-return-not-in-fn-body.rs", "2015", source),
        &[
            error(9, &[]),
            error(20, &[]),
            error(28, &[]),
            error(36, &[]),
        ],
    );
}

#[test]
fn a_break_to_an_undeclared_label() {
    // tests/ui/error-codes/E0426.rs
    let source = r#"fn main () {
    loop {
        break 'a;
        //~^ ERROR E0426
    }
}
"#;
    let stderr = r#"error[E0426]: use of undeclared label `'a`
  --> $DIR/E0426.rs:3:15
   |
LL |         break 'a;
   |               ^^ undeclared label `'a`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0426`.
"#;
    assert_eq!(ui("E0426.rs", "2015", source), stderr);
}

#[test]
fn undeclared_labels_are_pointed_at_on_their_own_lines() {
    // tests/ui/label/undeclared-label-span.rs
    let source = r#"//! regression test for <https://github.com/rust-lang/rust/issues/28109>
//! Make sure that label for continue and break is spanned correctly.

fn main() {
    loop {
        continue
        'b //~ ERROR use of undeclared label
        ;
        break
        'c //~ ERROR use of undeclared label
        ;
    }
}
"#;
    let stderr = r#"error[E0426]: use of undeclared label `'b`
  --> $DIR/undeclared-label-span.rs:7:9
   |
LL |         'b
   |         ^^ undeclared label `'b`

error[E0426]: use of undeclared label `'c`
  --> $DIR/undeclared-label-span.rs:10:9
   |
LL |         'c
   |         ^^ undeclared label `'c`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0426`.
"#;
    assert_eq!(ui("undeclared-label-span.rs", "2015", source), stderr);
}

#[test]
fn an_undeclared_label_suggests_the_innermost_similar_one() {
    // tests/ui/suggestions/suggest-labels.rs
    let source = r#"#[allow(unreachable_code, unused_labels)]
fn main() {
    'foo: loop {
        break 'fo; //~ ERROR use of undeclared label
    }

    'bar: loop {
        continue 'bor; //~ ERROR use of undeclared label
    }

    'longlabel: loop {
        'longlabel1: loop {
            break 'longlable; //~ ERROR use of undeclared label
        }
    }
}
"#;
    let stderr = r#"error[E0426]: use of undeclared label `'fo`
  --> $DIR/suggest-labels.rs:4:15
   |
LL |     'foo: loop {
   |     ---- a label with a similar name is reachable
LL |         break 'fo;
   |               ^^^
   |               |
   |               undeclared label `'fo`
   |               help: try using similarly named label: `'foo`

error[E0426]: use of undeclared label `'bor`
  --> $DIR/suggest-labels.rs:8:18
   |
LL |     'bar: loop {
   |     ---- a label with a similar name is reachable
LL |         continue 'bor;
   |                  ^^^^
   |                  |
   |                  undeclared label `'bor`
   |                  help: try using similarly named label: `'bar`

error[E0426]: use of undeclared label `'longlable`
  --> $DIR/suggest-labels.rs:13:19
   |
LL |         'longlabel1: loop {
   |         ----------- a label with a similar name is reachable
LL |             break 'longlable;
   |                   ^^^^^^^^^^
   |                   |
   |                   undeclared label `'longlable`
   |                   help: try using similarly named label: `'longlabel1`

error: aborting due to 3 previous errors

For more information about this error, try `rustc --explain E0426`.
"#;
    assert_eq!(ui("suggest-labels.rs", "2015", source), stderr);
}

#[test]
fn a_label_from_a_macro_is_not_the_label_around_its_call() {
    // tests/ui/hygiene/hygienic-label-1.rs
    let source = r#"macro_rules! foo {
    () => { break 'x; } //~ ERROR use of undeclared label `'x`
}

pub fn main() {
    'x: loop { foo!(); }
}
"#;
    let stderr = r#"error[E0426]: use of undeclared label `'x`
  --> $DIR/hygienic-label-1.rs:2:19
   |
LL |     () => { break 'x; }
   |                   ^^ undeclared label `'x`
...
LL |     'x: loop { foo!(); }
   |                ------ in this macro invocation
   |
   = note: this error originates in the macro `foo` (in Nightly builds, run with -Z macro-backtrace for more info)

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0426`.
"#;
    assert_eq!(ui("hygienic-label-1.rs", "2015", source), stderr);
}

#[test]
fn a_label_around_a_macros_argument_is_not_the_labels_in_its_body() {
    // tests/ui/hygiene/hygienic-label-2.rs
    let source = r#"macro_rules! foo {
    ($e: expr) => { 'x: loop { $e } }
}

pub fn main() {
    foo!(break 'x); //~ ERROR use of undeclared label `'x`
}
"#;
    let stderr = r#"error[E0426]: use of undeclared label `'x`
  --> $DIR/hygienic-label-2.rs:6:16
   |
LL |     foo!(break 'x);
   |                ^^ undeclared label `'x`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0426`.
"#;
    assert_eq!(ui("hygienic-label-2.rs", "2015", source), stderr);
}

#[test]
fn a_label_of_a_loop_outside_a_closure_is_unreachable() {
    // tests/ui/error-codes/E0767.rs
    let source = r#"fn main() {
    'a: loop {
        || {
            //~^ ERROR mismatched types
            loop { break 'a; } //~ ERROR E0767
        }
    }
}
"#;
    let stderr = r#"error[E0767]: use of unreachable label `'a`
  --> $DIR/E0767.rs:5:26
   |
LL |     'a: loop {
   |     -- unreachable label defined here
...
LL |             loop { break 'a; }
   |                          ^^ unreachable label `'a`
   |
   = note: labels are unreachable through functions, closures, async blocks and modules

error[E0308]: mismatched types
  --> $DIR/E0767.rs:3:9
   |
LL | /         || {
LL | |
LL | |             loop { break 'a; }
LL | |         }
   | |_________^ expected `()`, found closure
   |
   = note: expected unit type `()`
                found closure `{closure@$DIR/E0767.rs:3:9: 3:11}`

error: aborting due to 2 previous errors

Some errors have detailed explanations: E0308, E0767.
For more information about an error, try `rustc --explain E0308`.
"#;
    assert_eq!(ui("E0767.rs", "2015", source), stderr);
}

#[test]
fn a_yield_in_a_constant() {
    // tests/ui/coroutine/yield-in-const.rs: a constant is no item `#[coroutine]` could be
    // put on.
    let source = r#"#![feature(coroutines)]

const A: u8 = { yield 3u8; 3u8};
//~^ ERROR yield expression outside
//~| ERROR `yield` can only be used in

fn main() {}
"#;
    let stderr = r#"error: `yield` can only be used in `#[coroutine]` closures, or `gen` blocks
  --> $DIR/yield-in-const.rs:3:17
   |
LL | const A: u8 = { yield 3u8; 3u8};
   |                 ^^^^^^^^^

error[E0627]: yield expression outside of coroutine literal
  --> $DIR/yield-in-const.rs:3:17
   |
LL | const A: u8 = { yield 3u8; 3u8};
   |                 ^^^^^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0627`.
"#;
    assert_eq!(ui("yield-in-const.rs", "2015", source), stderr);
}

#[test]
fn a_yield_in_a_static() {
    // tests/ui/coroutine/yield-in-static.rs
    let source = r#"#![feature(coroutines)]

static B: u8 = { yield 3u8; 3u8};
//~^ ERROR yield expression outside
//~| ERROR `yield` can only be used in

fn main() {}
"#;
    let stderr = r#"error: `yield` can only be used in `#[coroutine]` closures, or `gen` blocks
  --> $DIR/yield-in-static.rs:3:18
   |
LL | static B: u8 = { yield 3u8; 3u8};
   |                  ^^^^^^^^^

error[E0627]: yield expression outside of coroutine literal
  --> $DIR/yield-in-static.rs:3:18
   |
LL | static B: u8 = { yield 3u8; 3u8};
   |                  ^^^^^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0627`.
"#;
    assert_eq!(ui("yield-in-static.rs", "2015", source), stderr);
}

#[test]
fn a_yield_in_a_function_suggests_making_it_a_coroutine() {
    // tests/ui/coroutine/yield-in-function.rs
    let source = r#"#![feature(coroutines)]

fn main() { yield; }
//~^ ERROR yield expression outside
//~| ERROR `yield` can only be used in
"#;
    let stderr = r#"error: `yield` can only be used in `#[coroutine]` closures, or `gen` blocks
  --> $DIR/yield-in-function.rs:3:13
   |
LL | fn main() { yield; }
   |             ^^^^^
   |
help: use `#[coroutine]` to make this closure a coroutine
   |
LL | #[coroutine] fn main() { yield; }
   | ++++++++++++

error[E0627]: yield expression outside of coroutine literal
  --> $DIR/yield-in-function.rs:3:13
   |
LL | fn main() { yield; }
   |             ^^^^^

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0627`.
"#;
    assert_eq!(ui("yield-in-function.rs", "2015", source), stderr);
}

#[test]
fn an_unused_must_use_result_as_a_blocks_tail_suggests_ending_it() {
    // tests/ui/lint/unused/must-use-block-expr.rs, without the operators of its `main`: the
    // front end reports a statement only for a `#[must_use]` function or type.
    let source = r#"//@ run-rustfix
//@ check-pass

#![warn(unused_must_use)]

#[must_use]
fn foo() -> i32 {
    42
}

fn bar() {
    {
        foo();
        //~^ WARN unused return value
    }
}

fn baz() {
    {
        foo()
        //~^ WARN unused return value
    };
}

fn main() {
    bar();
    baz();
}
"#;
    assert_reports(
        &ui("must-use-block-expr.rs", "2015", source),
        &[warning(13, &["`foo`"]), warning(20, &["`foo`"])],
    );
}
