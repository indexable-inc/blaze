//! Match checking's lints: `unreachable_patterns`, `bindings_with_variant_name` and
//! `irrefutable_let_patterns`, from rustc's UI tests (`tests/ui/pattern/`,
//! `tests/ui/let-else/`) with their `.stderr`.

use super::types::ui_std;
use crate::tests::{assert_reports, error, ui};

#[test]
fn bindings_named_as_the_variants_they_shadow() {
    // tests/ui/pattern/issue-14221.rs
    let source = r##"#![deny(unreachable_patterns)]
#![allow(unused_variables)]
#![allow(non_snake_case)]

pub enum E {
    A,
    B,
}

pub mod b {
    pub fn key(e: crate::E) -> &'static str {
        match e {
            A => "A",
//~^ ERROR pattern binding `A` is named the same as one of the variants of the type `E`
            B => "B", //~ ERROR: unreachable pattern
//~^ ERROR pattern binding `B` is named the same as one of the variants of the type `E`
        }
    }
}

fn main() {}
"##;
    assert_reports(
        &ui("issue-14221.rs", "2015", source),
        &[
            error(13, &["`A`", "`E`"]),
            error(15, &["`B`", "`E`"]),
            error(15, &[]),
        ],
    );
}

#[test]
fn a_string_literal_matched_twice() {
    // tests/ui/pattern/usefulness/issue-30240-b.rs
    let source = r##"#![deny(unreachable_patterns)]

fn main() {
    match "world" {
        "hello" => {}
        _ => {},
    }

    match "world" {
        ref _x if false => {}
        "hello" => {}
        "hello" => {} //~ ERROR unreachable pattern
        _ => {},
    }
}
"##;
    let stderr = r##"error: unreachable pattern
  --> $DIR/issue-30240-b.rs:12:9
   |
LL |         "hello" => {}
   |         ------- matches all the relevant values
LL |         "hello" => {}
   |         ^^^^^^^ no value can reach this
   |
note: the lint level is defined here
  --> $DIR/issue-30240-b.rs:1:9
   |
LL | #![deny(unreachable_patterns)]
   |         ^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("issue-30240-b.rs", "2015", source), stderr);
}

#[test]
fn a_struct_pattern_after_one_that_matches_any_value() {
    // tests/ui/pattern/usefulness/struct-pattern-match-useless.rs
    let source = r##"#![deny(unreachable_patterns)]

struct Foo {
    x: isize,
    y: isize,
}

pub fn main() {
    let a = Foo { x: 1, y: 2 };
    match a {
        Foo { x: _x, y: _y } => (),
        Foo { .. } => () //~ ERROR unreachable pattern
    }

}
"##;
    let stderr = r##"error: unreachable pattern
  --> $DIR/struct-pattern-match-useless.rs:12:9
   |
LL |         Foo { x: _x, y: _y } => (),
   |         -------------------- matches any value
LL |         Foo { .. } => ()
   |         ^^^^^^^^^^ no value can reach this
   |
note: the lint level is defined here
  --> $DIR/struct-pattern-match-useless.rs:1:9
   |
LL | #![deny(unreachable_patterns)]
   |         ^^^^^^^^^^^^^^^^^^^^

error: aborting due to 1 previous error

"##;
    assert_eq!(ui("struct-pattern-match-useless.rs", "2015", source), stderr);
}

#[test]
fn the_else_of_an_irrefutable_let_else() {
    // tests/ui/let-else/let-else-irrefutable.rs
    let std_items = r#"pub mod io {
    pub fn _eprint(_args: core::fmt::Arguments<'_>) {}
}

#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        $crate::io::_eprint(core::format_args_nl!($($arg)*));
    }};
}
"#;
    let source = r##"//@ check-pass

fn main() {
    let x = 1 else { return }; //~ WARN unreachable `else` clause

    // Multiline else blocks should not get printed
    let x = 1 else { //~ WARN unreachable `else` clause
        eprintln!("problem case encountered");
        return
    };

    let case = Some("a");
    let name = Some(case) else {
        //~^ WARN unreachable `else` clause
        //~| HELP consider using `let Some(name) = case` to match on a specific variant
        eprintln!("problem case encountered");
        return
    };
}
"##;
    let stderr = r##"warning: unreachable `else` clause
  --> $DIR/let-else-irrefutable.rs:4:15
   |
LL |     let x = 1 else { return };
   |     --------- ^^^^
   |     |
   |     assigning to binding pattern will always succeed
   |
   = note: this pattern always matches, so the else clause is unreachable
   = note: `#[warn(irrefutable_let_patterns)]` on by default

warning: unreachable `else` clause
  --> $DIR/let-else-irrefutable.rs:7:15
   |
LL |     let x = 1 else {
   |     --------- ^^^^
   |     |
   |     assigning to binding pattern will always succeed
   |
   = note: this pattern always matches, so the else clause is unreachable

warning: unreachable `else` clause
  --> $DIR/let-else-irrefutable.rs:13:27
   |
LL |     let name = Some(case) else {
   |     --------------------- ^^^^
   |     |
   |     assigning to binding pattern will always succeed
   |
   = note: this pattern always matches, so the else clause is unreachable
help: consider using `let Some(name) = case` to match on a specific variant
   |
LL -     let name = Some(case) else {
LL +     let Some(name) = case else {
   |

warning: 3 warnings emitted

"##;
    assert_eq!(ui_std("let-else-irrefutable.rs", "2015", "sized, fmt, option", ("pub use crate::eprintln;", std_items), source), stderr);
}
