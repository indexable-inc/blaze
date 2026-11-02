//! rustc's borrow checker: its errors, from rustc's UI tests (`tests/ui/borrowck/`,
//! `tests/ui/error-codes/`), and `unused_mut`. Most errors are checked by code, where and
//! message; `E0499.rs` by rustc's whole `.stderr`, with the labels hir-ty-borrowck gives.

use crate::tests::{assert_reports, checked, error, items::ui_core, ui};

/// The errors of a crate of `main.rs` and `minicore` (`flags`, if any), each as
/// `E0499 4:17 message`: its code, the line and column of its primary span in `source`, and
/// its message; the lints and the closing lines left out.
fn errors(minicore: Option<&str>, source: &str) -> Vec<String> {
    let header = match minicore {
        Some(flags) => format!("//- minicore: {flags}\n//- /main.rs crate:other:main\n"),
        None => "//- /main.rs crate:other:main\n".to_owned(),
    };
    let checked = checked(&format!("{header}{source}"));
    checked
        .diags
        .iter()
        .filter(|it| it.is_error() && !it.is_lint())
        .filter_map(|diag| {
            let span = diag.span.primary.first()?;
            let before = &source[..span.lo as usize];
            let line = before.matches('\n').count() + 1;
            let column = before[before.rfind('\n').map_or(0, |it| it + 1)..].chars().count() + 1;
            let code = diag.code.as_ref().map_or("", |it| it.as_str());
            Some(format!("{code} {line}:{column} {}", diag.message))
        })
        .collect()
}

#[test]
fn two_mutable_borrows_at_once() {
    // tests/ui/error-codes/E0499.rs
    let source = r#"fn main() {
    let mut i = 0;
    let mut x = &mut i;
    let mut a = &mut i; //~ ERROR E0499
    a.use_mut();
    x.use_mut();
}

trait Fake { fn use_mut(&mut self) { } fn use_ref(&self) { }  }
impl<T> Fake for T { }
"#;
    assert_eq!(errors(None, source), ["E0499 4:17 cannot borrow `i` as mutable more than once at a time"]);
    // tests/ui/error-codes/E0499.stderr
    let stderr = r#"error[E0499]: cannot borrow `i` as mutable more than once at a time
  --> $DIR/E0499.rs:4:17
   |
LL |     let mut x = &mut i;
   |                 ------ first mutable borrow occurs here
LL |     let mut a = &mut i;
   |                 ^^^^^^ second mutable borrow occurs here
LL |     a.use_mut();
LL |     x.use_mut();
   |     - first borrow later used here

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0499`.
"#;
    assert_eq!(ui("E0499.rs", "2015", source), stderr);
}

#[test]
fn a_mutable_borrow_while_borrowed_as_immutable() {
    // tests/ui/error-codes/E0502.rs
    let source = r#"fn bar(x: &mut i32) {}
fn foo(a: &mut i32) {
    let ref y = a;
    bar(a); //~ ERROR E0502
    y.use_ref();
}

fn main() {
}

trait Fake { fn use_mut(&mut self) { } fn use_ref(&self) { }  }
impl<T> Fake for T { }
"#;
    assert_eq!(
        errors(None, source),
        ["E0502 4:5 cannot borrow `*a` as mutable because it is also borrowed as immutable"]
    );
}

#[test]
fn a_move_out_of_a_borrowed_value() {
    // tests/ui/error-codes/E0505.rs
    let source = r#"struct Value {}

fn eat(val: Value) {}

fn main() {
    let x = Value{};
    {
        let _ref_to_val: &Value = &x;
        eat(x); //~ ERROR E0505
        _ref_to_val.use_ref();
    }
}

trait Fake { fn use_mut(&mut self) { } fn use_ref(&self) { }  }
impl<T> Fake for T { }
"#;
    assert_eq!(errors(None, source), ["E0505 9:13 cannot move out of `x` because it is borrowed"]);
}

#[test]
fn a_mutable_borrow_of_an_immutable_binding() {
    // tests/ui/error-codes/E0596.rs
    let source = r#"fn main() {
    let x = 1;
    let y = &mut x; //~ ERROR [E0596]
}
"#;
    assert_eq!(
        errors(None, source),
        ["E0596 3:13 cannot borrow `x` as mutable, as it is not declared as mutable"]
    );
}

#[test]
fn a_borrow_that_outlives_its_value() {
    // tests/ui/error-codes/E0597.rs
    let source = r#"struct Foo<'a> {
    x: Option<&'a u32>,
}

fn main() {
    let mut x = Foo { x: None };
    let y = 0;
    x.x = Some(&y);
    //~^ ERROR `y` does not live long enough [E0597]
}

impl<'a> Drop for Foo<'a> { fn drop(&mut self) { } }
"#;
    assert_eq!(errors(Some("option, drop"), source), ["E0597 8:16 `y` does not live long enough"]);
}

#[test]
fn an_immutable_pattern_binding_assigned_again() {
    // tests/ui/borrowck/borrowck-match-binding-is-assignment.rs
    let source = r#"// Test that immutable pattern bindings cannot be reassigned.

enum E {
    Foo(isize)
}

struct S {
    bar: isize,
}

pub fn main() {
    match 1 {
        x => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match E::Foo(1) {
        E::Foo(x) => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match (S { bar: 1 }) {
        S { bar: x } => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match (1,) {
        (x,) => {
            x += 1; //~ ERROR [E0384]
        }
    }

    match [1,2,3] {
        [x,_,_] => {
            x += 1; //~ ERROR [E0384]
        }
    }
}
"#;
    let message = "cannot assign twice to immutable variable `x`";
    assert_eq!(
        errors(None, source),
        [14, 20, 26, 32, 38].map(|line| format!("E0384 {line}:13 {message}"))
    );
}

#[test]
fn a_use_of_a_mutably_borrowed_value() {
    // tests/ui/borrowck/borrowck-match-already-borrowed.rs
    let source = r#"enum Foo {
    A(i32),
    B
}

fn match_enum() {
    let mut foo = Foo::B;
    let p = &mut foo;
    let _ = match foo { //~ ERROR [E0503]
        Foo::B => 1,
        _ => 2,
        Foo::A(x) => x //~ ERROR [E0503]
    };
    drop(p);
}


fn main() {
    let mut x = 1;
    let r = &mut x;
    let _ = match x {
        x => x + 1, //~ ERROR [E0503]
        y => y + 2, //~ ERROR [E0503]
    };
    drop(r);
}
"#;
    let errors = errors(Some("drop"), source);
    let expected = [("9:19", "`foo`"), ("12:16", "`foo.0`"), ("22:9", "`x`"), ("23:9", "`x`")];
    assert_eq!(errors.len(), expected.len(), "{errors:?}");
    for (error, (at, place)) in errors.iter().zip(expected) {
        // The bindings copy an `i32`: a use, not a move.
        assert!(error.contains(&format!(" {at} ")) && error.contains(place) && error.contains("cannot use"), "{at} {place}: {errors:?}");
    }
}

#[test]
fn a_mut_binding_never_mutated() {
    // tests/ui/nll/issue-61424.rs, with `x = 1u32; let _ = x;` for its `String::new()` and
    // `dbg!(x)`, which need std: the same lines and columns.
    let source = r#"//@ run-rustfix

#![deny(unused_mut)]

fn main() {
    let mut x; //~ ERROR: variable does not need to be mutable
    x = 1u32;
    let _ = x;
}
"#;
    let stderr = r#"error: variable does not need to be mutable
  --> $DIR/issue-61424.rs:6:9
   |
LL |     let mut x;
   |         ----^
   |         |
   |         help: remove this `mut`
   |
note: the lint level is defined here
  --> $DIR/issue-61424.rs:3:9
   |
LL | #![deny(unused_mut)]
   |         ^^^^^^^^^^

error: aborting due to 1 previous error

"#;
    assert_eq!(ui("issue-61424.rs", "2015", source), stderr);
}

#[test]
fn a_mut_closure_argument_never_mutated() {
    // tests/ui/nll/unused-mut-issue-50343.rs, its closure called directly rather than through
    // `vec![..].iter().map(..)`, which needs std; minicore's `Fn` traits for the call.
    let source = r#"//@ run-rustfix

#![deny(unused_mut)]
#![allow(unused_variables)] // for rustfix

fn main() {
    let f = |(mut x, _y): (i32, i32)| ();
    f((42, 22));
}
"#;
    assert_reports(
        &ui_core("unused-mut-issue-50343.rs", "2015", "fn", source),
        &[error(7, &["`mut`"])],
    );
}

#[test]
fn a_mut_binding_mutated_is_not_linted() {
    let source = r#"#![deny(unused_mut)]

fn main() {
    let mut x = 1u32;
    x += 1;
    let _ = x;
}
"#;
    assert_eq!(ui("mutated.rs", "2015", source), "");
}
